//! Attested gate evidence: the independent review, acceptance, hardware,
//! and risk facts the merge gate needs besides the forge's own
//! ([`crate::workflows::gate::GateSupplement`]), recorded in the house store
//! as a `gate.attestation/1` workflow marker keyed by the pull request and
//! its exact head and base.
//!
//! An attestation rests on a forge review ([`ForgeReview`]): its id and the
//! login the reviewer claims. Only [`record_gate_attestation`] writes one. It
//! refuses a claimant who wrote the branch, and a claimed reviewer who is
//! the pull request's author. A recorded attestation is never rewritten: a
//! different one for the same subject is refused, and a moved head or base
//! needs a new attestation.
//!
//! The record authenticates nothing by itself: anyone who can open the
//! store can claim any login. The scheduled gate reads it back with
//! [`gate_attestation`] and merges only after the house's forge shows that
//! review approved, on exactly the head, by the claimed login, and that
//! login is neither the author the forge reports nor a forge login a branch
//! writer pushed as.
//!
//! The house records name a writer by its holder or worker handle, which is
//! not a forge login and is never compared with one. A scheduled writer
//! pushes as the house's forge login. A person's forge login is recorded
//! nowhere, so a branch a person wrote, in their own session or at a
//! worker's terminal, is never merged here ([`BranchWriters::forge_logins`]).
//!
//! Nothing in Kitchen records an attestation yet, so until a reviewer
//! workflow does (#230), the scheduled gate reports every pull request as
//! unattested and merges nothing.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use super::{RunError, repair_of};
use crate::{
    HolderId, HouseId, WorkflowId,
    contracts::{
        BranchName, Claimant, CommitId, ContractError, EvidenceSubject, IssueNumber, Repository,
        Timestamp, Trigger, ValueKind,
    },
    integrations::github::{Review, ReviewState},
    state::{
        HouseStore, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, OwnershipEvent, StateError,
        TaskRecord, WorkItem,
    },
    workflows::{
        coordination::{launched_workers, person_took_over, task_branch},
        gate::{RiskClass, SemanticReview},
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
    /// The reviewer's forge login. The gate merges only when the forge
    /// shows this login as the review's author.
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

/// Record `attestation` for its exact subject. `author` is the pull
/// request's author and `branch` its head branch, as the forge reports
/// them. `recorded_by` must not be a branch writer, and the claimed
/// reviewer must not be `author`. Recording the same attestation again is a
/// no-op.
///
/// # Errors
/// [`ContractError::CrossHouse`] for another house,
/// [`RunError::AttestationByWriter`] when `recorded_by` wrote the branch,
/// [`RunError::AttestationNotIndependent`] for a reviewer who is `author`
/// or an empty login, [`RunError::AttestationRecorded`] when a
/// different attestation exists for the subject, and store errors.
pub fn record_gate_attestation(
    store: &HouseStore,
    attestation: &GateAttestation,
    author: &str,
    branch: &BranchName,
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
    let writers = BranchWriters::of(
        &store.tasks()?,
        &attestation.repository,
        attestation.pull_request,
        branch,
    );
    if writers.includes(recorded_by.holder.as_str()) {
        return Err(RunError::AttestationByWriter.into());
    }
    if !independent(&attestation.forge_review.reviewer, Some(author), &[]) {
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
        Ok(_) => Ok(()),
        Err(crate::Error::State(StateError::MarkerConflict)) => {
            Err(RunError::AttestationRecorded.into())
        }
        Err(error) => Err(error),
    }
}

/// The attestation recorded for exactly this pull request, head, and base,
/// if any.
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
) -> Result<Option<GateAttestation>> {
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
    Ok(Some(attestation))
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

/// Whether `login` did not write the branch: it is not empty, differs from
/// the pull request's `author`, which must be known, and is none of the
/// `writer_logins`, the forge logins the branch's writers pushed as.
pub(super) fn independent(login: &str, author: Option<&str>, writer_logins: &[String]) -> bool {
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

    /// The forge logins the branch's writers pushed as, when every one is
    /// verified: `house`, the logins Kitchen's unattended writers push
    /// through. `None` when a person wrote the branch, since no record ties
    /// a session to a forge login, or when `house` names no login.
    pub(super) fn forge_logins<'a>(&self, house: &'a [String]) -> Option<&'a [String]> {
        (!self.person && !house.is_empty()).then_some(house)
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
    use super::{BranchWriters, independent};

    #[test]
    fn a_reviewer_must_differ_from_a_known_author() {
        assert!(independent("Reviewer", Some("kitchen-bot"), &[]));
        assert!(!independent("Reviewer", Some("reviewer"), &[]));
        assert!(!independent("Reviewer", None, &[]));
        assert!(!independent("", Some("kitchen-bot"), &[]));
    }

    #[test]
    fn a_reviewer_must_not_be_a_forge_login_a_writer_pushed_as() {
        let house = ["kitchen-bot".to_owned()];
        assert!(independent("safety-reviewer", Some("someone"), &house));
        assert!(!independent("Kitchen-Bot", Some("someone"), &house));
    }

    #[test]
    fn writers_have_forge_logins_only_when_none_is_a_person() {
        let house = ["kitchen-bot".to_owned()];
        let unattended = BranchWriters {
            names: vec!["kitchn-run".to_owned(), "worker-1".to_owned()],
            person: false,
        };
        assert_eq!(unattended.forge_logins(&house), Some(house.as_slice()));
        // Without a house login, an unattended writer's login is unknown.
        assert_eq!(unattended.forge_logins(&[]), None);
        // A session name is not a login: it stays unknown whatever it is.
        let person = BranchWriters {
            names: vec!["session-dana".to_owned()],
            person: true,
        };
        assert_eq!(person.forge_logins(&house), None);
        assert!(person.includes("Session-Dana"));
        assert!(!person.includes("dana"));
    }
}
