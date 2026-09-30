//! Attested gate evidence: the independent review, acceptance, hardware,
//! and risk facts the merge gate needs besides the forge's own
//! ([`crate::workflows::gate::GateSupplement`]), recorded in the house store
//! as a `gate.attestation/1` workflow marker keyed by the pull request and
//! its exact head and base.
//!
//! An attestation rests on a forge review ([`ForgeReview`]): its id and the
//! review author's login. [`attest_gate_review`] reads the review and its
//! structured claims from the house's forge, then records them. It refuses
//! a reviewer who wrote the branch or authored the pull request. A
//! recorded attestation is never rewritten: a different one for the same
//! subject is refused, and a moved head or base needs a new attestation.
//!
//! The record authenticates nothing by itself. The scheduled gate reads it
//! back with [`gate_attestation`] and merges only when the recorded principal
//! is the forge review's author, the review remains approved at the exact
//! head with the same claims, and the reviewer is neither a branch writer,
//! the pull request author, nor any commit author or committer
//! ([`commit_logins`]).
//!
//! Who wrote the branch is read from the forge, not from the house records:
//! those name a writer by its holder or worker handle, which is not a forge
//! login, and a worker can push with credentials of its own. A commit the
//! forge links to no account leaves a writer unknown, and the pull request
//! is only reported. The forge links a commit to an account by the email in
//! the commit, which whoever pushes chooses, so this rules out a reviewer
//! the commits name and does not prove who pushed.
//!
//! The records add one refusal: a branch a person wrote, in their own
//! session or at a worker's terminal, is never merged here
//! ([`BranchWriters::person`]).
//!
//! The `kitchn gate attest` command verifies and records a reviewer's
//! attestation. The scheduled gate checks the evidence again before merging.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use super::{RunError, repair_of};
use crate::{
    HolderId, HouseId, WorkflowId,
    contracts::{
        BranchName, Claimant, CommitId, ContractError, EvidenceSubject, IssueNumber, Repository,
        Timestamp, Trigger, ValueKind,
    },
    integrations::github::{
        GitHubClient, GitHubReadTransport, IssueState, PullRequestCommit, Review, ReviewState,
    },
    state::{
        HouseStore, MarkerFact, MarkerKey, MarkerRecording, MarkerSchema, MarkerSubject,
        OwnershipEvent, StateError, TaskRecord, WorkItem,
    },
    workflows::{
        coordination::{launched_workers, person_took_over, task_branch},
        gate::{RiskClass, SemanticReview},
        known,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Workflow id under which gate attestations are recorded.
pub const GATE_ATTESTATION_WORKFLOW: &str = "merge-gate-attestation";

const SCHEMA: &str = "gate.attestation";

/// The forge review an attestation rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgeReview {
    /// The forge's id for the pull request review.
    pub id: NonZeroU64,
    /// The review author's forge login.
    pub reviewer: String,
}

/// What an independent reviewer attests about one pull request at one exact
/// head and base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateAttestation {
    /// The house the attestation belongs to.
    pub house: HouseId,
    /// The repository.
    pub repository: Repository,
    /// The pull request.
    pub pull_request: IssueNumber,
    /// The exact head reviewed.
    pub head: CommitId,
    /// The exact base the diff was compared against.
    pub base: CommitId,
    /// The forge review the attestation rests on.
    pub forge_review: ForgeReview,
    /// The review's result.
    pub review: SemanticReview,
    /// The review read committed content without running the pull
    /// request's code with credentials.
    pub read_only: bool,
    /// The linked issue's acceptance evidence is complete.
    pub acceptance_met: bool,
    /// Required hardware work is complete, or none is required.
    pub hardware_complete: bool,
    /// The complete risk classification of the diff. A class needs a human
    /// approval this record does not carry, so the gate will not merge it.
    pub risk_classes: Vec<RiskClass>,
}

/// An attestation read back from the house store, with who recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAttestation {
    /// What was attested.
    pub attestation: GateAttestation,
    /// The forge review author's login, recorded as a holder. The gate
    /// verifies it again against the review and branch writers.
    pub recorded_by: HolderId,
}

/// Record `attestation` for its exact subject. The pull request's head
/// branch and author are read from `forge`, the house's forge: a caller
/// cannot name another branch to leave its own writer tasks out.
/// `recorded_by` must not be a writer of that branch, and the claimed
/// reviewer must not be the author. An attestation for the same subject is
/// refused, including an identical repeat.
///
/// # Errors
/// [`ContractError::CrossHouse`] for another house, a failed or incomplete
/// read of the pull request, [`RunError::AttestationByWriter`] when
/// `recorded_by` wrote the branch, [`RunError::AttestationNotIndependent`]
/// for a reviewer who is the author, an empty login, or a pull request whose
/// author the forge does not name, [`RunError::AttestationRecorded`] when an
/// attestation exists for the subject, and store errors.
pub fn record_gate_attestation<T: GitHubReadTransport>(
    store: &HouseStore,
    forge: &GitHubClient<T>,
    attestation: &GateAttestation,
    recorded_by: &Claimant,
    now: Timestamp,
) -> Result<()> {
    if &attestation.house != store.house() {
        return Err(ContractError::CrossHouse {
            expected: store.house().clone(),
            found: attestation.house.clone(),
        }
        .into());
    }
    let pull_request = known(forge.pull_request(
        store.house(),
        &attestation.repository,
        attestation.pull_request,
    ))?;
    let writers = BranchWriters::of(
        &store.tasks()?,
        &attestation.repository,
        attestation.pull_request,
        &BranchName::new(&pull_request.head.name)?,
    );
    if writers.includes(recorded_by.holder.as_str()) {
        return Err(RunError::AttestationByWriter.into());
    }
    let author = pull_request.user.as_ref().map(|user| user.login.as_str());
    if !independent(&attestation.forge_review.reviewer, author, &[]) {
        return Err(RunError::AttestationNotIndependent.into());
    }
    let key = key(
        &attestation.repository,
        attestation.pull_request,
        &attestation.head,
        &attestation.base,
    )?;
    let fact = MarkerFact::workflow(schema()?, attestation)?;
    match store.record_marker(key, fact, recorded_by, now) {
        Ok(MarkerRecording::Recorded(_)) => Ok(()),
        Ok(MarkerRecording::AlreadyRecorded(_) | MarkerRecording::Superseded(_)) => {
            Err(RunError::AttestationRecorded.into())
        }
        Err(crate::Error::State(StateError::MarkerConflict)) => {
            Err(RunError::AttestationRecorded.into())
        }
        Err(error) => Err(error),
    }
}

/// Verify a review against the forge's current pull request and commit
/// identities, then record it for the exact head and base. This is the
/// reviewer command's entrypoint; the scheduled gate independently checks
/// the same evidence before an effect.
///
/// # Errors
/// Refuses a closed, moved, self-reviewed, unverified, or already attested
/// pull request. Incomplete forge evidence and store failures are errors.
pub fn attest_gate_review<T: GitHubReadTransport>(
    store: &HouseStore,
    forge: &GitHubClient<T>,
    repository: &Repository,
    pull_number: IssueNumber,
    review_id: NonZeroU64,
    now: Timestamp,
) -> Result<GateAttestation> {
    let pull_request = known(forge.pull_request(store.house(), repository, pull_number))?;
    if pull_request.state != IssueState::Open || pull_request.merged {
        return Err(RunError::AttestationClosed.into());
    }
    let head = pull_request.head.sha.clone();
    let base_branch = BranchName::new(&pull_request.base.name)?;
    let base = known(forge.branch_tip(store.house(), repository, &base_branch))?;
    let reviews = known(forge.reviews(store.house(), repository, pull_number))?;
    let review = reviews
        .iter()
        .find(|review| review.id == review_id.get())
        .ok_or(RunError::AttestationReviewUnverified)?;
    if review.state != ReviewState::Approved || review.commit_id != head {
        return Err(RunError::AttestationReviewUnverified.into());
    }
    let claims = parse_review_block(review.body.as_deref())?;
    if claims.head != head {
        return Err(RunError::AttestationStaleHead.into());
    }
    if claims.base != base {
        return Err(RunError::AttestationStaleBase.into());
    }
    let reviewer = review.user.login.as_str();
    check_review_independence(
        store,
        forge,
        repository,
        pull_number,
        &pull_request,
        reviewer,
    )?;
    let attestation = GateAttestation {
        house: store.house().clone(),
        repository: repository.clone(),
        pull_request: pull_number,
        head,
        base,
        forge_review: ForgeReview {
            id: review_id,
            reviewer: reviewer.to_owned(),
        },
        review: claims.semantic,
        read_only: claims.read_only,
        acceptance_met: claims.acceptance,
        hardware_complete: claims.hardware,
        risk_classes: claims.risk,
    };
    let recorded_by = Claimant::interactive(HolderId::new(reviewer)?);
    record_gate_attestation(store, forge, &attestation, &recorded_by, now)?;
    Ok(attestation)
}

/// Refuse an approval by a PR author or branch writer before it is posted or
/// recorded. Both callers use the same forge and house evidence.
pub(super) fn check_review_independence<T: GitHubReadTransport>(
    store: &HouseStore,
    forge: &GitHubClient<T>,
    repository: &Repository,
    pull_number: IssueNumber,
    pull_request: &crate::integrations::github::PullRequest,
    reviewer: &str,
) -> Result<()> {
    let writers = BranchWriters::of(
        &store.tasks()?,
        repository,
        pull_number,
        &BranchName::new(&pull_request.head.name)?,
    );
    if writers.includes(reviewer) {
        return Err(RunError::AttestationByWriter.into());
    }
    if writers.person() {
        return Err(RunError::AttestationWritersUnknown.into());
    }
    let commits = known(forge.pull_request_commits(
        store.house(),
        repository,
        pull_number,
        &pull_request.head.sha,
    ))?;
    let commit_writers = commit_logins(&commits).ok_or(RunError::AttestationWritersUnknown)?;
    if !independent(
        reviewer,
        pull_request.user.as_ref().map(|user| user.login.as_str()),
        &commit_writers,
    ) {
        return Err(RunError::AttestationNotIndependent.into());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ReviewClaims {
    head: CommitId,
    base: CommitId,
    semantic: SemanticReview,
    read_only: bool,
    acceptance: bool,
    hardware: bool,
    risk: Vec<RiskClass>,
}

/// Parse the single, complete `kitchen-attestation` fenced block in a forge review.
pub(super) fn parse_review_block(body: Option<&str>) -> Result<ReviewClaims> {
    let body = body.ok_or(RunError::AttestationBlockInvalid)?;
    let normalized = body.replace("\r\n", "\n");
    let body = normalized.as_str();
    let mut blocks = body.split("```kitchen-attestation\n");
    let _prefix = blocks.next();
    let block = blocks.next().ok_or(RunError::AttestationBlockInvalid)?;
    if blocks.next().is_some() {
        return Err(RunError::AttestationBlockInvalid.into());
    }
    let (content, suffix) = block
        .split_once("\n```")
        .ok_or(RunError::AttestationBlockInvalid)?;
    if suffix.starts_with('`') {
        return Err(RunError::AttestationBlockInvalid.into());
    }
    let mut fields = std::collections::BTreeMap::new();
    for line in content.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or(RunError::AttestationBlockInvalid)?;
        if value.is_empty() || fields.insert(key, value).is_some() {
            return Err(RunError::AttestationBlockInvalid.into());
        }
    }
    if fields.len() != 7 {
        return Err(RunError::AttestationBlockInvalid.into());
    }
    let field = |key| {
        fields
            .get(key)
            .copied()
            .ok_or(RunError::AttestationBlockInvalid)
    };
    let head = CommitId::new(field("head")?).map_err(|_| RunError::AttestationBlockInvalid)?;
    let base = CommitId::new(field("base")?).map_err(|_| RunError::AttestationBlockInvalid)?;
    let semantic = serde_json::from_value::<SemanticReview>(serde_json::Value::String(
        field("semantic")?.to_owned(),
    ))
    .map_err(|_| RunError::AttestationBlockInvalid)?;
    let read_only = match field("read_only")? {
        "true" => true,
        "false" => false,
        _ => return Err(RunError::AttestationBlockInvalid.into()),
    };
    let complete = |key| match field(key)? {
        "complete" => Ok(true),
        "incomplete" => Ok(false),
        _ => Err(RunError::AttestationBlockInvalid),
    };
    let acceptance = complete("acceptance")?;
    let hardware = complete("hardware")?;
    let risk = if field("risk")? == "none" {
        Vec::new()
    } else {
        let mut classes = Vec::new();
        for value in field("risk")?.split(',') {
            let class =
                serde_json::from_value::<RiskClass>(serde_json::Value::String(value.to_owned()))
                    .map_err(|_| RunError::AttestationBlockInvalid)?;
            if classes.contains(&class) {
                return Err(RunError::AttestationBlockInvalid.into());
            }
            classes.push(class);
        }
        classes
    };
    Ok(ReviewClaims {
        head,
        base,
        semantic,
        read_only,
        acceptance,
        hardware,
        risk,
    })
}

pub(super) fn claims_match(review: &Review, attestation: &GateAttestation) -> bool {
    parse_review_block(review.body.as_deref()).is_ok_and(|claims| {
        claims.head == attestation.head
            && claims.base == attestation.base
            && claims.semantic == attestation.review
            && claims.read_only == attestation.read_only
            && claims.acceptance == attestation.acceptance_met
            && claims.hardware == attestation.hardware_complete
            && claims.risk == attestation.risk_classes
    })
}

/// The attestation recorded for exactly this pull request, head, and base,
/// if any, and the holder that recorded it.
///
/// # Errors
/// [`StateError::MarkerPayloadInvalid`] for a payload that describes
/// another subject or house, and store errors.
pub fn gate_attestation(
    store: &HouseStore,
    repository: &Repository,
    pull_request: IssueNumber,
    head: &CommitId,
    base: &CommitId,
) -> Result<Option<RecordedAttestation>> {
    let Some(marker) = store.marker(&key(repository, pull_request, head, base)?)? else {
        return Ok(None);
    };
    let attestation: GateAttestation = marker.fact().decode(&schema()?)?;
    if &attestation.house != store.house()
        || &attestation.repository != repository
        || attestation.pull_request != pull_request
        || &attestation.head != head
        || &attestation.base != base
    {
        return Err(StateError::MarkerPayloadInvalid.into());
    }
    Ok(Some(RecordedAttestation {
        attestation,
        recorded_by: marker.recorded_by().holder.clone(),
    }))
}

/// Whether the forge's `reviews` hold the review `claimed` names, by the
/// claimed login, approved on exactly `head`.
pub(super) fn review_verified(reviews: &[Review], claimed: &ForgeReview, head: &CommitId) -> bool {
    reviews.iter().any(|review| {
        review.id == claimed.id.get()
            && !claimed.reviewer.is_empty()
            && review.user.login.eq_ignore_ascii_case(&claimed.reviewer)
            && &review.commit_id == head
            && review.state == ReviewState::Approved
    })
}

/// The forge logins of every author and committer of `commits`, or `None`
/// when the forge links one of them to no account.
pub(super) fn commit_logins(commits: &[PullRequestCommit]) -> Option<Vec<&str>> {
    commits
        .iter()
        .flat_map(|commit| [commit.author.as_deref(), commit.committer.as_deref()])
        .collect()
}

/// Whether `login` did not write the branch: it is not empty, differs from
/// the pull request's `author`, which must be known, and is none of the
/// `writer_logins`, the forge logins of the branch's commits.
pub(super) fn independent(login: &str, author: Option<&str>, writer_logins: &[&str]) -> bool {
    !login.is_empty()
        && author.is_some_and(|author| !author.eq_ignore_ascii_case(login))
        && !writer_logins
            .iter()
            .any(|writer| writer.eq_ignore_ascii_case(login))
}

/// Who wrote a pull request's branch in the house's records: every holder
/// that created or held one of its writer tasks (the pickup task on the
/// branch, and every repair or follow-up round of the pull request, a
/// person's or scheduled), and every worker a launch on the branch created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BranchWriters {
    /// Holders and worker handles, as the house records name them. These
    /// are not forge logins.
    names: Vec<String>,
    /// A person wrote the branch: an interactive holder created or held one
    /// of its writer tasks, or a person took one of its workers' terminals
    /// over.
    person: bool,
}

impl BranchWriters {
    pub(super) fn of(
        tasks: &[TaskRecord],
        repository: &Repository,
        pull_request: IssueNumber,
        branch: &BranchName,
    ) -> Self {
        let mut names = Vec::new();
        let mut person = false;
        let mut holder = |holder: &HolderId, trigger: &Trigger| {
            names.push(holder.as_str().to_owned());
            match trigger {
                Trigger::Interactive => person = true,
                Trigger::Scheduled | Trigger::Event(_) => {}
            }
        };
        let mut workers = Vec::new();
        for record in tasks {
            let writes = task_branch(record).as_ref() == Some(branch)
                || repair_of(record, repository).is_some_and(|(number, _)| number == pull_request);
            if writes {
                let created = record.created_by();
                holder(&created.holder, &created.trigger);
                for event in record.ownership() {
                    match event {
                        OwnershipEvent::Claimed {
                            holder: by,
                            trigger,
                            ..
                        }
                        | OwnershipEvent::Adopted {
                            holder: by,
                            trigger,
                            ..
                        }
                        | OwnershipEvent::TakenOver {
                            holder: by,
                            trigger,
                            ..
                        } => holder(by, trigger),
                        OwnershipEvent::Relinquished { .. } | OwnershipEvent::Released { .. } => {}
                    }
                }
            }
            workers.extend(
                launched_workers(record)
                    .filter(|view| view.branch.as_ref() == Some(branch))
                    .map(|view| {
                        (
                            view.worker.handle.as_str().to_owned(),
                            person_took_over(record, &view.worker),
                        )
                    }),
            );
        }
        for (handle, taken_over) in workers {
            names.push(handle);
            person |= taken_over;
        }
        names.sort_unstable();
        names.dedup();
        Self { names, person }
    }

    /// Whether `name`, a holder or worker handle, is one of the writers,
    /// ignoring ASCII case.
    pub(super) fn includes(&self, name: &str) -> bool {
        self.names
            .iter()
            .any(|writer| writer.eq_ignore_ascii_case(name))
    }

    /// Whether a person wrote the branch. No record ties a session to a
    /// forge login, so the gate merges no such branch whatever the forge
    /// shows of its commits.
    pub(super) const fn person(&self) -> bool {
        self.person
    }
}

fn schema() -> std::result::Result<MarkerSchema, StateError> {
    MarkerSchema::new(SCHEMA, NonZeroU32::MIN)
}

fn key(
    repository: &Repository,
    pull_request: IssueNumber,
    head: &CommitId,
    base: &CommitId,
) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(GATE_ATTESTATION_WORKFLOW)?,
        item: WorkItem::PullRequest {
            repository: repository.clone(),
            number: NonZeroU64::new(pull_request.get()).ok_or(ContractError::InvalidValue {
                kind: ValueKind::Text,
            })?,
        },
        subject: MarkerSubject::Git(EvidenceSubject {
            head: head.clone(),
            base: Some(base.clone()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::{BranchWriters, commit_logins, independent, parse_review_block};
    use crate::{
        contracts::{CommitId, ContractError},
        integrations::github::PullRequestCommit,
        workflows::gate::SemanticReview,
    };

    fn commit(
        author: Option<&str>,
        committer: Option<&str>,
    ) -> Result<PullRequestCommit, ContractError> {
        Ok(PullRequestCommit {
            sha: CommitId::new(&"d".repeat(40))?,
            author: author.map(str::to_owned),
            committer: committer.map(str::to_owned),
        })
    }

    #[test]
    fn a_reviewer_must_differ_from_a_known_author() {
        assert!(independent("Reviewer", Some("kitchen-bot"), &[]));
        assert!(!independent("Reviewer", Some("reviewer"), &[]));
        assert!(!independent("Reviewer", None, &[]));
        assert!(!independent("", Some("kitchen-bot"), &[]));
    }

    #[test]
    fn review_block_requires_one_complete_typed_subject() -> Result<(), Box<dyn std::error::Error>>
    {
        let head = "a".repeat(40);
        let base = "b".repeat(40);
        let valid = format!(
            "Review text\n```kitchen-attestation\nhead={head}\nbase={base}\nsemantic=clean\nread_only=true\nacceptance=complete\nhardware=incomplete\nrisk=workflow-rules,dependencies\n```\n"
        );
        let claims = parse_review_block(Some(&valid))?;
        assert_eq!(claims.head, CommitId::new(&head)?);
        assert_eq!(claims.base, CommitId::new(&base)?);
        assert_eq!(claims.risk.len(), 2);
        assert!(!claims.hardware);
        assert!(parse_review_block(None).is_err());
        assert!(parse_review_block(Some("ordinary review text")).is_err());
        assert!(
            parse_review_block(Some(&valid.replace("semantic=clean", "semantic=great"))).is_err()
        );
        assert!(
            parse_review_block(Some(
                &valid.replace("risk=workflow-rules,dependencies", "risk=none,dependencies")
            ))
            .is_err()
        );
        assert!(parse_review_block(Some(&valid.replace("base=", "head="))).is_err());
        assert_eq!(
            parse_review_block(Some(&valid.replace('\n', "\r\n")))?,
            claims
        );
        Ok(())
    }

    #[test]
    fn readme_example_block_parses() -> Result<(), Box<dyn std::error::Error>> {
        // The README holds exactly one block, so parsing the whole file
        // checks the example a reviewer copies.
        let readme = include_str!("../../../../../README.md");
        let claims = parse_review_block(Some(readme))?;
        assert_eq!(
            claims.head,
            CommitId::new("9523e3b1c4f07a2d8e6b5f3a1c0d9e8f7a6b5c4d")?
        );
        assert_eq!(
            claims.base,
            CommitId::new("e635128a7f3c2b1d0e9f8a7b6c5d4e3f2a1b0c9d")?
        );
        assert_eq!(claims.semantic, SemanticReview::Clean);
        assert!(claims.read_only && claims.acceptance && claims.hardware);
        assert!(claims.risk.is_empty());
        Ok(())
    }

    #[test]
    fn a_reviewer_must_not_be_a_forge_login_of_a_commit() {
        let writers = ["kitchen-bot"];
        assert!(independent("safety-reviewer", Some("someone"), &writers));
        assert!(!independent("Kitchen-Bot", Some("someone"), &writers));
    }

    #[test]
    fn commit_logins_are_every_author_and_committer_or_unknown() -> Result<(), ContractError> {
        let linked = [
            commit(Some("kitchen-bot"), Some("web-flow"))?,
            commit(Some("dana"), Some("kitchen-bot"))?,
        ];
        assert_eq!(
            commit_logins(&linked),
            Some(vec!["kitchen-bot", "web-flow", "dana", "kitchen-bot"])
        );
        assert_eq!(commit_logins(&[]), Some(Vec::new()));
        // One unlinked author or committer leaves the writers unknown.
        for unlinked in [
            commit(None, Some("kitchen-bot"))?,
            commit(Some("dana"), None)?,
        ] {
            assert_eq!(commit_logins(&[linked[0].clone(), unlinked]), None);
        }
        Ok(())
    }

    #[test]
    fn a_session_name_is_a_writer_name_and_never_a_login() {
        let person = BranchWriters {
            names: vec!["session-dana".to_owned()],
            person: true,
        };
        assert!(person.person());
        assert!(person.includes("Session-Dana"));
        assert!(!person.includes("dana"));
        let unattended = BranchWriters {
            names: vec!["kitchn-run".to_owned(), "worker-1".to_owned()],
            person: false,
        };
        assert!(!unattended.person());
    }
}
