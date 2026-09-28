//! `issue new` and `issue refine`: preview every write, apply only what the
//! person approved.
//!
//! An [`IssueDraft`] is what the session wrote with the person: a new issue
//! or a refinement comment on an existing one, label changes, and blocked-by
//! links to existing issues. [`draft_preview`] validates it, lists every
//! forge write in order, and binds them to a [`DraftDigest`]. Open product
//! questions make the preview not ready: they are asked in the session, never
//! posted.
//!
//! [`apply_draft`] writes nothing without a [`DraftApproval`] naming the
//! digest of the preview it recomputes. Each write carries a
//! [`Consent`] derived from that approval for exactly that write, under an
//! interactive claim. All writes of one approved preview run as one durable
//! task whose id comes from the draft's subject and digest, so a rerun after
//! partial posting reconciles uncertain writes, reuses applied ones, and
//! submits only the rest: no duplicate issue, comment, label, or link. A
//! write whose outcome stays unknown stops the run.
//!
//! One subject (an existing issue, or new issues in a repository) has at
//! most one unfinished draft at a time. A per-subject slot task, claimed for
//! the duration of the call, serializes the check for an earlier unfinished
//! draft with the creation of this one, so two sessions cannot both pass.

use std::{
    fmt::{self, Write as _},
    str::FromStr,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ClaimPolicy, ClaimRefusal, InteractiveError, claim_task, require_person};
use crate::{
    EffectName, HolderId, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, CapabilityRequirements, Claimant, Clock,
        Consent, Effect, EffectExecutor, ExternalRef, FailureClass, GitHubAction, GitHubEffect,
        GitHubMutation, HouseGrants, IssueNumber, LeaseTtl, NotAppliedReason, Provenance,
        Repository, RetryPolicy, Role, Settlement, TaskAuthority, TaskSpec, Text,
    },
    integrations::github::{GitHubExecutor, GitHubMutationTransport},
    state::{
        EffectPlan, EffectRecord, EffectState, HouseStore, Lease, TaskRecord, TaskState, reconcile,
        run_effect,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// The workflow id of interactive drafts.
pub const DRAFT_WORKFLOW: &str = "interactive-draft";
/// Prefix of the tasks that write approved drafts.
pub const DRAFT_TASK_PREFIX: &str = "draft-";
/// Prefix of the per-subject slot tasks.
const SLOT_TASK_PREFIX: &str = "draftslot-";
/// Most label changes, added and removed together, in one draft.
pub const MAX_DRAFT_LABELS: usize = 20;
/// Most blocked-by links in one draft.
pub const MAX_DRAFT_BLOCKERS: usize = 10;
/// Most open questions in one draft.
pub const MAX_DRAFT_QUESTIONS: usize = 10;
const MAX_TITLE_BYTES: usize = 256;
const MAX_BODY_BYTES: usize = 60 * 1024;
const MAX_LABEL_BYTES: usize = 50;
const MAX_QUESTION_BYTES: usize = 500;
/// Drafts take over an expired slot or task claim: the call that held it
/// stopped, and its fence is superseded so it cannot write again. A live
/// claim, even the same person's, is never shared, so two concurrent calls
/// cannot both write.
const TAKE_OVER_EXPIRED: ClaimPolicy = ClaimPolicy {
    take_over: true,
    resume: false,
};
/// Attempts one draft task may use, including recovery.
const ATTEMPTS: u32 = 4;
/// Longest a draft task may keep retrying after its first attempt.
const BUDGET: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What a draft writes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DraftTarget {
    /// Create one new issue.
    New {
        /// Title.
        title: String,
        /// Body: outcome, ownership, acceptance criteria, dependencies.
        body: String,
    },
    /// Post a refinement of an existing issue as one comment.
    Refine {
        /// The issue.
        issue: IssueNumber,
        /// The refinement: sharpened acceptance criteria and findings.
        comment: String,
    },
}

/// An issue draft written with the person. Untrusted until previewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssueDraft {
    /// The repository written to.
    pub repository: Repository,
    /// The issue created or refined.
    pub target: DraftTarget,
    /// Labels to add to the target issue.
    #[serde(default)]
    pub add_labels: Vec<String>,
    /// Labels to remove from the target issue; only when refining.
    #[serde(default)]
    pub remove_labels: Vec<String>,
    /// Existing issues that block the target issue.
    #[serde(default)]
    pub blocked_by: Vec<IssueNumber>,
    /// Product decisions only the person can make. The preview is not ready
    /// while any remain; they are asked in the session, never posted.
    #[serde(default)]
    pub questions: Vec<String>,
}

/// One forge write of a preview, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PlannedWrite {
    /// Create the new issue.
    CreateIssue {
        /// Title.
        title: String,
        /// Body.
        body: String,
    },
    /// Comment on the refined issue.
    Comment {
        /// The issue.
        issue: IssueNumber,
        /// Comment text.
        body: String,
    },
    /// Add or remove one label on the target issue.
    Label {
        /// The label.
        label: String,
        /// Whether it is added.
        present: bool,
    },
    /// Record an existing issue as blocking the target issue.
    BlockedBy {
        /// The blocking issue.
        blocker: IssueNumber,
    },
}

/// A draft's preview digest, `sha256:` and 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DraftDigest(String);

impl DraftDigest {
    /// The digest text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or_default()
    }
}

impl FromStr for DraftDigest {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self> {
        let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InteractiveError::InvalidDraft("digest").into())
        }
    }
}

impl TryFrom<String> for DraftDigest {
    type Error = crate::Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<DraftDigest> for String {
    fn from(value: DraftDigest) -> Self {
        value.0
    }
}

impl fmt::Display for DraftDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Every write a draft would make, bound to one digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// The repository written to.
    pub repository: Repository,
    /// The writes, in the order they are made.
    pub writes: Vec<PlannedWrite>,
    /// Open questions for the person; none may remain to apply.
    pub questions: Vec<String>,
    /// The digest an approval must name.
    pub digest: DraftDigest,
}

impl Preview {
    /// Whether the preview can be applied: no open questions.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.questions.is_empty()
    }

    /// The preview as the person should see it, with every write in full.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("Draft for {}\n", self.repository);
        for (position, write) in self.writes.iter().enumerate() {
            let step = position.saturating_add(1);
            let _ = match write {
                PlannedWrite::CreateIssue { title, body } => {
                    write!(out, "\n{step}. Create issue \"{title}\":\n{body}\n")
                }
                PlannedWrite::Comment { issue, body } => {
                    write!(out, "\n{step}. Comment on #{}:\n{body}\n", issue.get())
                }
                PlannedWrite::Label {
                    label,
                    present: true,
                } => writeln!(out, "{step}. Add label \"{label}\""),
                PlannedWrite::Label {
                    label,
                    present: false,
                } => writeln!(out, "{step}. Remove label \"{label}\""),
                PlannedWrite::BlockedBy { blocker } => {
                    writeln!(out, "{step}. Mark blocked by #{}", blocker.get())
                }
            };
        }
        if self.questions.is_empty() {
            let _ = write!(
                out,
                "\n{} writes. Approve digest {} to post exactly this.",
                self.writes.len(),
                self.digest
            );
        } else {
            out.push_str("\nNot ready. Decide first:\n");
            for question in &self.questions {
                let _ = writeln!(out, "- {question}");
            }
        }
        out
    }

    /// The issue the writes target, when it already exists.
    #[must_use]
    pub fn existing_issue(&self) -> Option<IssueNumber> {
        self.writes.iter().find_map(|write| match write {
            PlannedWrite::Comment { issue, .. } => Some(*issue),
            PlannedWrite::CreateIssue { .. }
            | PlannedWrite::Label { .. }
            | PlannedWrite::BlockedBy { .. } => None,
        })
    }
}

fn single_line(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn body_text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_BODY_BYTES && !value.contains('\0')
}

fn invalid(field: &'static str) -> crate::Error {
    InteractiveError::InvalidDraft(field).into()
}

/// Validate `draft` and list its writes: the issue creation or comment,
/// then label changes, then blocked-by links.
///
/// # Errors
/// [`InteractiveError::InvalidDraft`] naming the offending field for empty
/// or oversized text, repeated or conflicting labels, label removal on a new
/// issue, repeated or self blockers, and bounds; nothing private is echoed.
pub fn draft_preview(draft: &IssueDraft) -> Result<Preview> {
    let mut writes = Vec::new();
    let target = match &draft.target {
        DraftTarget::New { title, body } => {
            if !single_line(title, MAX_TITLE_BYTES) {
                return Err(invalid("title"));
            }
            if !body_text(body) {
                return Err(invalid("body"));
            }
            if !draft.remove_labels.is_empty() {
                return Err(invalid("removeLabels"));
            }
            writes.push(PlannedWrite::CreateIssue {
                title: title.clone(),
                body: body.clone(),
            });
            None
        }
        DraftTarget::Refine { issue, comment } => {
            if !body_text(comment) {
                return Err(invalid("comment"));
            }
            writes.push(PlannedWrite::Comment {
                issue: *issue,
                body: comment.clone(),
            });
            Some(*issue)
        }
    };
    let labels = draft
        .add_labels
        .iter()
        .map(|label| (label, true))
        .chain(draft.remove_labels.iter().map(|label| (label, false)));
    let mut seen: Vec<&str> = Vec::new();
    for (label, present) in labels {
        if !single_line(label, MAX_LABEL_BYTES) || seen.contains(&label.as_str()) {
            return Err(invalid("labels"));
        }
        seen.push(label);
        writes.push(PlannedWrite::Label {
            label: label.clone(),
            present,
        });
    }
    if seen.len() > MAX_DRAFT_LABELS {
        return Err(invalid("labels"));
    }
    if draft.blocked_by.len() > MAX_DRAFT_BLOCKERS {
        return Err(invalid("blockedBy"));
    }
    for (position, blocker) in draft.blocked_by.iter().enumerate() {
        let repeated = draft
            .blocked_by
            .get(..position)
            .is_some_and(|earlier| earlier.contains(blocker));
        if Some(*blocker) == target || repeated {
            return Err(invalid("blockedBy"));
        }
        writes.push(PlannedWrite::BlockedBy { blocker: *blocker });
    }
    if draft.questions.len() > MAX_DRAFT_QUESTIONS
        || !draft
            .questions
            .iter()
            .all(|question| single_line(question, MAX_QUESTION_BYTES))
    {
        return Err(invalid("questions"));
    }
    let digest = digest(&draft.repository, &writes)?;
    Ok(Preview {
        repository: draft.repository.clone(),
        writes,
        questions: draft.questions.clone(),
        digest,
    })
}

fn sha256_hex(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn digest(repository: &Repository, writes: &[PlannedWrite]) -> Result<DraftDigest> {
    let encoded = serde_json::to_vec(writes).map_err(|_| InteractiveError::Encoding)?;
    let hex = sha256_hex(
        b"kitchen-interactive-draft-v1\0",
        &[repository.as_str().as_bytes(), &encoded],
    );
    format!("sha256:{hex}").parse()
}

/// The subject a draft writes to: one existing issue, or new issues in one
/// repository. At most one draft per subject is unfinished at a time.
fn subject_hash(preview: &Preview) -> String {
    let target = preview
        .existing_issue()
        .map_or_else(|| "new".to_owned(), |issue| issue.get().to_string());
    sha256_hex(
        b"kitchen-interactive-draft-subject-v1\0",
        &[preview.repository.as_str().as_bytes(), target.as_bytes()],
    )
    .get(..16)
    .unwrap_or_default()
    .to_owned()
}

/// The task that writes `preview` once approved.
///
/// # Errors
/// Never for a valid preview; the id syntax is checked defensively.
pub fn draft_task_id(preview: &Preview) -> Result<TaskId> {
    let digest = preview
        .digest
        .hex()
        .get(..24)
        .ok_or(InteractiveError::Encoding)?;
    Ok(TaskId::new(&format!(
        "{DRAFT_TASK_PREFIX}{}-{digest}",
        subject_hash(preview)
    ))?)
}

fn slot_task_id(preview: &Preview) -> Result<TaskId> {
    Ok(TaskId::new(&format!(
        "{SLOT_TASK_PREFIX}{}",
        subject_hash(preview)
    ))?)
}

/// A person's approval of one exact preview. Build it only from what a
/// person present agreed to. It is not persisted as a grant: each
/// [`apply_draft`] call needs it again, and it covers only that preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftApproval {
    /// A reference for this approval, chosen by the session.
    pub id: ExternalRef,
    /// The person who approved.
    pub given_by: HolderId,
    /// The digest of the preview they approved.
    pub digest: DraftDigest,
}

/// A forge executor that also builds its own effects, such as the
/// house-scoped [`GitHubExecutor`].
pub trait ForgeWriter: EffectExecutor {
    /// Build the persisted effect for `mutation`, with the house's requester
    /// and posting budget.
    ///
    /// # Errors
    /// Refuses a mutation the house does not permit.
    fn github_effect(&self, mutation: GitHubMutation) -> Result<GitHubEffect>;
}

impl<T: GitHubMutationTransport> ForgeWriter for GitHubExecutor<T> {
    fn github_effect(&self, mutation: GitHubMutation) -> Result<GitHubEffect> {
        Ok(self.effect(mutation)?)
    }
}

/// The durable store, forge, and house grants [`apply_draft`] writes
/// through.
pub struct DraftWriter<'a> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The house-scoped forge executor.
    pub forge: &'a dyn ForgeWriter,
    /// The house's current grants; its policy limits bound what consent can
    /// authorize.
    pub grants: &'a HouseGrants,
    /// Time source.
    pub clock: &'a dyn Clock,
}

/// Bounds for the draft task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftOptions {
    /// Instruction revisions pinned on the task.
    pub provenance: Provenance,
    /// Lease on the task while this call writes.
    pub lease: LeaseTtl,
}

/// One write that is applied, in this call or an earlier one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Written {
    /// The write's logical name.
    pub effect: EffectName,
    /// The forge reference from the receipt.
    pub reference: ExternalRef,
    /// Whether an earlier call applied it.
    pub reused: bool,
}

/// How an [`apply_draft`] call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DraftOutcome {
    /// Every write is applied and the task settled.
    Completed,
    /// The person did not approve. Nothing was written.
    Declined,
    /// The approval names another digest. Nothing was written.
    StaleApproval,
    /// Open questions remain. Nothing was written.
    NotReady,
    /// The preview needs more writes than the house allows one task.
    /// Nothing was written.
    OverBudget {
        /// Writes needed.
        needed: u32,
        /// The house's per-task limit.
        limit: u32,
    },
    /// An earlier draft for the same subject is unfinished and may have
    /// written. Finish it first. Nothing was written.
    EarlierUnfinished {
        /// The unfinished task.
        task: TaskId,
    },
    /// Another session is writing this subject now.
    HeldElsewhere,
    /// A write's outcome is unknown; nothing after it was submitted.
    Uncertain {
        /// The write.
        effect: EffectName,
    },
    /// The forge refused a write; a rerun retries within the attempt budget.
    NotApplied {
        /// The write.
        effect: EffectName,
        /// Why.
        reason: NotAppliedReason,
    },
    /// The task had settled before this call.
    Settled {
        /// How.
        settlement: Settlement,
    },
}

/// The result of [`apply_draft`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftReport {
    /// The preview this call acted on.
    pub preview: Preview,
    /// The draft task, once one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// The target issue, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueNumber>,
    /// Applied writes, in write order.
    pub written: Vec<Written>,
    /// How the call ended.
    pub outcome: DraftOutcome,
}

/// One write step of a preview.
struct DraftWrite {
    /// Position in the preview.
    position: usize,
    /// Logical effect name.
    name: EffectName,
}

fn steps(preview: &Preview) -> Result<Vec<DraftWrite>> {
    preview
        .writes
        .iter()
        .enumerate()
        .map(|(position, write)| {
            let name = match write {
                PlannedWrite::CreateIssue { .. } => "create".to_owned(),
                PlannedWrite::Comment { .. } => "comment".to_owned(),
                PlannedWrite::Label { .. } => format!("label-{position}"),
                PlannedWrite::BlockedBy { blocker } => format!("blocked-by-{}", blocker.get()),
            };
            Ok(DraftWrite {
                position,
                name: EffectName::new(&name)?,
            })
        })
        .collect()
}

fn action(
    preview: &Preview,
    write: &PlannedWrite,
    target: Option<IssueNumber>,
) -> Result<GitHubAction> {
    let target = || target.ok_or_else(|| crate::Error::from(InteractiveError::UnreadableReceipt));
    Ok(match write {
        PlannedWrite::CreateIssue { title, body } => GitHubAction::CreateIssue {
            title: Text::new(title)?,
            body: Text::new(body)?,
        },
        PlannedWrite::Comment { issue, body } => GitHubAction::PostComment {
            issue: *issue,
            body: Text::new(body)?,
        },
        PlannedWrite::Label { label, present } => GitHubAction::SetLabel {
            issue: target()?,
            label: label.clone(),
            present: *present,
        },
        PlannedWrite::BlockedBy { blocker } => GitHubAction::LinkDependency {
            issue: target()?,
            blocker: *blocker,
        },
    })
    .and_then(|action| {
        GitHubMutation {
            repository: preview.repository.clone(),
            action: action.clone(),
        }
        .validate()?;
        Ok(action)
    })
}

/// The issue number in a receipt of the form
/// `https://github.com/<repository>/issues/<number>`.
fn issue_number(repository: &Repository, reference: &ExternalRef) -> Result<IssueNumber> {
    let prefix = format!("https://github.com/{}/issues/", repository.as_str());
    reference
        .as_str()
        .strip_prefix(&prefix)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse::<u64>().ok())
        .and_then(|number| IssueNumber::new(number).ok())
        .ok_or_else(|| InteractiveError::UnreadableReceipt.into())
}

/// A consent reference unique to the approval, attempt, and write: the
/// store refuses one consent for two effects.
fn consent_id(
    approval: &DraftApproval,
    attempt: AttemptNumber,
    name: &EffectName,
) -> Result<ExternalRef> {
    let hex = sha256_hex(
        b"kitchen-interactive-draft-consent-v1\0",
        &[
            approval.id.as_str().as_bytes(),
            approval.digest.as_str().as_bytes(),
            &attempt.get().to_be_bytes(),
            name.as_str().as_bytes(),
        ],
    );
    Ok(ExternalRef::new(&format!(
        "draft-consent-{}",
        hex.get(..32).unwrap_or_default()
    ))?)
}

/// Every write already applied in any attempt, and the target issue.
fn collect_applied(
    record: &TaskRecord,
    steps: &[DraftWrite],
    report: &mut DraftReport,
) -> Result<()> {
    for step in steps {
        let receipt = record.effects().iter().rev().find_map(|effect| {
            match (effect.name() == &step.name, effect.state()) {
                (true, EffectState::Applied { receipt, .. }) => Some(receipt),
                _ => None,
            }
        });
        let Some(receipt) = receipt else {
            continue;
        };
        if matches!(
            report.preview.writes.get(step.position),
            Some(PlannedWrite::CreateIssue { .. })
        ) {
            report.issue = Some(issue_number(
                &report.preview.repository,
                receipt.reference(),
            )?);
        }
        report.written.push(Written {
            effect: step.name.clone(),
            reference: receipt.reference().clone(),
            reused: true,
        });
    }
    Ok(())
}

/// An earlier draft of the same subject that is not settled and is being
/// run or has a write that may have reached the forge.
fn earlier_unfinished(
    tasks: &[TaskRecord],
    own: &TaskId,
    subject: &str,
    now: crate::contracts::Timestamp,
) -> Option<TaskId> {
    let prefix = format!("{DRAFT_TASK_PREFIX}{subject}-");
    tasks
        .iter()
        .find(|task| {
            task.spec().id != *own
                && task.spec().id.as_str().starts_with(&prefix)
                && match task.state() {
                    TaskState::Settled { .. } => false,
                    TaskState::Claimed { lease } if lease.is_live(now) => true,
                    TaskState::Open | TaskState::Claimed { .. } => task
                        .effects()
                        .iter()
                        .any(|effect| !matches!(effect.state(), EffectState::NotApplied { .. })),
                }
        })
        .map(|task| task.spec().id.clone())
}

/// Give a claim back unless the task settled.
fn release(store: &HouseStore, id: &TaskId, lease: &Lease, clock: &dyn Clock) -> Result<()> {
    if matches!(store.task(id)?.state(), TaskState::Claimed { lease: held } if held.fence() == lease.fence())
    {
        store.relinquish(id, lease.fence(), clock.now())?;
    }
    Ok(())
}

fn spec(
    id: TaskId,
    preview: &Preview,
    grants: &HouseGrants,
    options: &DraftOptions,
) -> Result<TaskSpec> {
    Ok(TaskSpec {
        id,
        role: Role::SousChef,
        repository: Some(preview.repository.clone()),
        authority: TaskAuthority::delegate(grants, [])?,
        retry: RetryPolicy::new(ATTEMPTS, BUDGET)?,
        provenance: options.provenance.clone(),
        resources: std::collections::BTreeSet::new(),
        requires: CapabilityRequirements::new(),
        agent: None,
    })
}

/// Write the approved draft, or complete an earlier partial write of it.
///
/// `approval` is `None` when the person declined; nothing is written then.
/// Otherwise the preview is recomputed from `draft` and nothing is written
/// unless the approval names its digest, it is ready, and its writes fit the
/// house's posting budget. See the module documentation for the rerun
/// guarantees. The task carries no standing authority: every write is
/// authorized by the person's consent, within the house's policy limits.
///
/// # Errors
/// [`InteractiveError::NeedsPerson`] for a non-interactive claimant;
/// [`draft_preview`]'s errors; [`InteractiveError::UnreadableReceipt`] when
/// the created issue's receipt names no issue; executor, contract, and store
/// errors, which leave interrupted work for the next run.
pub fn apply_draft(
    writer: &DraftWriter<'_>,
    draft: &IssueDraft,
    approval: Option<&DraftApproval>,
    claimant: &Claimant,
    options: &DraftOptions,
) -> Result<DraftReport> {
    require_person(claimant)?;
    let preview = draft_preview(draft)?;
    let issue = preview.existing_issue();
    let report = |preview: Preview, task: Option<TaskId>, outcome| DraftReport {
        preview,
        task,
        issue,
        written: Vec::new(),
        outcome,
    };
    let Some(approval) = approval else {
        return Ok(report(preview, None, DraftOutcome::Declined));
    };
    if approval.digest != preview.digest {
        return Ok(report(preview, None, DraftOutcome::StaleApproval));
    }
    if !preview.ready() {
        return Ok(report(preview, None, DraftOutcome::NotReady));
    }
    let steps = steps(&preview)?;
    let first = preview
        .writes
        .first()
        .ok_or(InteractiveError::InvalidDraft("target"))?;
    // Building an effect only validates it: this reads the house's per-task
    // posting limit before any task exists.
    let probe = writer.forge.github_effect(GitHubMutation {
        repository: preview.repository.clone(),
        action: action(&preview, first, issue)?,
    })?;
    let needed = u32::try_from(steps.len()).unwrap_or(u32::MAX);
    let limit = probe.posting_budget.limit();
    if needed > limit {
        return Ok(report(
            preview,
            None,
            DraftOutcome::OverBudget { needed, limit },
        ));
    }
    let store = writer.store;
    let clock = writer.clock;
    let slot_id = slot_task_id(&preview)?;
    let slot = match claim_task(
        store,
        spec(slot_id.clone(), &preview, writer.grants, options)?,
        claimant,
        options.lease,
        clock.now(),
        TAKE_OVER_EXPIRED,
    )? {
        Ok(lease) => lease,
        // Slot tasks never start attempts, so they do not settle; treat one
        // that did like a slot someone else holds rather than write.
        Err(
            ClaimRefusal::Held { .. } | ClaimRefusal::OwnerUncertain | ClaimRefusal::Settled { .. },
        ) => {
            return Ok(report(preview, None, DraftOutcome::HeldElsewhere));
        }
    };
    let result = apply_in_slot(writer, &preview, &steps, approval, claimant, options);
    let released = release(store, &slot_id, &slot, clock);
    let applied = result?;
    released?;
    Ok(match applied {
        Applied::Early(outcome) => report(preview, None, outcome),
        Applied::Report(run) => run,
    })
}

enum Applied {
    Early(DraftOutcome),
    Report(DraftReport),
}

fn apply_in_slot(
    writer: &DraftWriter<'_>,
    preview: &Preview,
    steps: &[DraftWrite],
    approval: &DraftApproval,
    claimant: &Claimant,
    options: &DraftOptions,
) -> Result<Applied> {
    let store = writer.store;
    let clock = writer.clock;
    let id = draft_task_id(preview)?;
    let now = clock.now();
    if let Some(task) = earlier_unfinished(&store.tasks()?, &id, &subject_hash(preview), now) {
        return Ok(Applied::Early(DraftOutcome::EarlierUnfinished { task }));
    }
    let mut run = DraftReport {
        preview: preview.clone(),
        task: Some(id.clone()),
        issue: preview.existing_issue(),
        written: Vec::new(),
        outcome: DraftOutcome::Completed,
    };
    let lease = match claim_task(
        store,
        spec(id.clone(), preview, writer.grants, options)?,
        claimant,
        options.lease,
        now,
        TAKE_OVER_EXPIRED,
    )? {
        Ok(lease) => lease,
        Err(ClaimRefusal::Held { .. } | ClaimRefusal::OwnerUncertain) => {
            run.outcome = DraftOutcome::HeldElsewhere;
            return Ok(Applied::Report(run));
        }
        Err(ClaimRefusal::Settled { settlement }) => {
            collect_applied(&store.task(&id)?, steps, &mut run)?;
            run.outcome = match settlement {
                Settlement::Succeeded => DraftOutcome::Completed,
                Settlement::Failed | Settlement::Cancelled | Settlement::Exhausted => {
                    DraftOutcome::Settled { settlement }
                }
            };
            return Ok(Applied::Report(run));
        }
    };
    let fence = lease.fence();
    let reconciled = reconcile(store, writer.forge, &id, fence, clock)?;
    if let Some(stuck) = reconciled
        .unresolved
        .first()
        .or_else(|| reconciled.foreign.first())
    {
        run.outcome = DraftOutcome::Uncertain {
            effect: stuck.name().clone(),
        };
        release(store, &id, &lease, clock)?;
        return Ok(Applied::Report(run));
    }
    let attempt = match store.start_attempt(&id, fence, clock.now())? {
        AttemptStart::Started(attempt) | AttemptStart::AlreadyRunning(attempt) => attempt,
        AttemptStart::Exhausted => {
            collect_applied(&store.task(&id)?, steps, &mut run)?;
            run.outcome = DraftOutcome::Settled {
                settlement: Settlement::Exhausted,
            };
            return Ok(Applied::Report(run));
        }
    };
    collect_applied(&store.task(&id)?, steps, &mut run)?;
    for step in steps {
        if run
            .written
            .iter()
            .any(|written| written.effect == step.name)
        {
            continue;
        }
        // A write whose earlier outcome is still unknown, even a waived one,
        // is never submitted again: it may have posted.
        let unknown = store
            .task(&id)?
            .effects()
            .iter()
            .any(|effect| effect.name() == &step.name && !effect.state().is_resolved());
        if unknown {
            run.outcome = DraftOutcome::Uncertain {
                effect: step.name.clone(),
            };
            release(store, &id, &lease, clock)?;
            return Ok(Applied::Report(run));
        }
        let write = preview
            .writes
            .get(step.position)
            .ok_or(InteractiveError::Encoding)?;
        let effect: Effect = writer
            .forge
            .github_effect(GitHubMutation {
                repository: preview.repository.clone(),
                action: action(preview, write, run.issue)?,
            })?
            .into();
        let record = run_write(writer, approval, &id, fence, attempt, step, effect)?;
        match record.state() {
            EffectState::Applied { receipt, .. } => {
                if matches!(write, PlannedWrite::CreateIssue { .. }) {
                    run.issue = Some(issue_number(&preview.repository, receipt.reference())?);
                }
                run.written.push(Written {
                    effect: step.name.clone(),
                    reference: receipt.reference().clone(),
                    reused: false,
                });
            }
            EffectState::NotApplied { reason, .. } => {
                let reason = *reason;
                store.finish_attempt(
                    &id,
                    fence,
                    attempt,
                    AttemptOutcome::Failed(FailureClass::Retryable),
                    clock.now(),
                )?;
                release(store, &id, &lease, clock)?;
                run.outcome = DraftOutcome::NotApplied {
                    effect: step.name.clone(),
                    reason,
                };
                return Ok(Applied::Report(run));
            }
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => {
                release(store, &id, &lease, clock)?;
                run.outcome = DraftOutcome::Uncertain {
                    effect: step.name.clone(),
                };
                return Ok(Applied::Report(run));
            }
        }
    }
    store.finish_attempt(&id, fence, attempt, AttemptOutcome::Succeeded, clock.now())?;
    Ok(Applied::Report(run))
}

fn run_write(
    writer: &DraftWriter<'_>,
    approval: &DraftApproval,
    id: &TaskId,
    fence: crate::contracts::Fence,
    attempt: AttemptNumber,
    step: &DraftWrite,
    effect: Effect,
) -> Result<EffectRecord> {
    let store = writer.store;
    let revision = store.task(id)?.evidence().revision();
    let consent = Consent {
        id: consent_id(approval, attempt, &step.name)?,
        given_by: approval.given_by.clone(),
        house: store.house().clone(),
        task: id.clone(),
        effect: effect.clone(),
        revision,
    };
    let plan = EffectPlan {
        task: id.clone(),
        fence,
        name: step.name.clone(),
        decided_at: revision,
        effect,
        consent: Some(consent),
    };
    run_effect(store, writer.forge, writer.grants, plan, writer.clock)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn receipts_must_name_an_issue_in_the_repository() -> TestResult {
        let repo = Repository::new("sample/project")?;
        let good = ExternalRef::new("https://github.com/sample/project/issues/42")?;
        assert_eq!(issue_number(&repo, &good)?.get(), 42);
        for bad in [
            "https://github.com/other/project/issues/42",
            "https://github.com/sample/project/pull/42",
            "https://github.com/sample/project/issues/0",
            "https://github.com/sample/project/issues/",
            "kitchen-key",
        ] {
            assert!(
                issue_number(&repo, &ExternalRef::new(bad)?).is_err(),
                "{bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn digests_parse_only_in_canonical_form() {
        let hex = "0".repeat(64);
        assert!(format!("sha256:{hex}").parse::<DraftDigest>().is_ok());
        assert!(hex.parse::<DraftDigest>().is_err());
        assert!(
            format!("sha256:{}", "A".repeat(64))
                .parse::<DraftDigest>()
                .is_err()
        );
        assert!(
            format!("sha256:{}", "0".repeat(63))
                .parse::<DraftDigest>()
                .is_err()
        );
    }
}
