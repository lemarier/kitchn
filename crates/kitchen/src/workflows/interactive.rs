//! Interactive entrypoints: `work`, `pr`, `issue new`, and `issue refine`.
//!
//! A person opens a worktree in an adopted repository and hands Kitchen an
//! issue, a pull request, or a rough idea. [`resolve_house`] finds the house
//! from the repository through the house registry and fails closed when it
//! is missing or ambiguous. [`execution_mode`] decides from Orca evidence
//! whether work may fan out to workers; without positive evidence every
//! entrypoint runs as a single agent and says why.
//!
//! The entrypoints use the same workflow code and durable claims as
//! scheduled runs; only the [`Claimant`]'s trigger differs. They refuse any
//! claimant that is not [`Trigger::Interactive`]: authority comes from the
//! person present, one effect at a time, through [`crate::contracts::Consent`].
//! Nothing here stores consent as a grant. [`work`] claims the issue under
//! the task id scheduled pickup derives
//! ([`crate::workflows::pickup::issue_task_id`]); [`pull_request`]
//! claims a writer round under the task id scheduled repair derives
//! ([`repair_task_id`]). An item one trigger holds is skipped by the other,
//! and ownership moves only through [`hand_back`] (a recorded relinquish that
//! the next claimant adopts), an explicit takeover after the lease expired,
//! or settlement.
//!
//! [`draft_preview`] and [`apply_draft`] cover `issue new` and `issue
//! refine`: every issue, comment, label, and dependency change is previewed,
//! bound to a digest, and written only after the person approves that exact
//! digest. Sub-issue creation goes through [`Decomposer`], which the
//! decomposition workflow (#47) implements.

use std::{collections::BTreeSet, fmt, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    ErrorClass, HolderId, HouseId, TaskId,
    adapters::orca::{OrcaVersion, REQUIRED_FEATURES, capabilities as orca_capabilities},
    adoption::{HouseRegistry, RepositoryMatch, ResolvedInstructions},
    contracts::{
        Capability, CapabilitySet, Claimant, CommitId, IssueNumber, LeaseTtl, Repository,
        Settlement, TaskSpec, Timestamp, Trigger,
    },
    house::{HouseConfig, RepositoryConfig},
    state::{HouseStore, Lease, OwnershipEvent, StateError, TaskState},
    workflows::{
        coordination::REQUIRED_WORKER_CAPABILITIES,
        pickup::{FollowUpBudget, IssueRef, TaskTemplate},
        repair::{Mergeability, PullRequestState, PullRequestView, repair_task_id},
    },
};

mod draft;

pub use draft::{
    AcknowledgeOutcome, AcknowledgeReason, AcknowledgeReport, Acknowledgement, ApprovedDraft,
    DRAFT_TASK_PREFIX, DraftApproval, DraftDigest, DraftOptions, DraftOutcome, DraftReport,
    DraftTarget, DraftWriter, ForgeWriter, IssueDraft, MAX_ACKNOWLEDGE_REASON_BYTES,
    MAX_DRAFT_BLOCKERS, MAX_DRAFT_LABELS, MAX_DRAFT_QUESTIONS, PlannedWrite, Preview, ReadBack,
    WriteReadBack, Written, acknowledge_draft, apply_draft, draft_preview, draft_task_id,
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Largest Orca command output read as evidence, in bytes.
pub const MAX_ORCA_OUTPUT_BYTES: usize = 1024 * 1024;

/// An interactive entrypoint refused its input. Private issue text is never
/// included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InteractiveError {
    /// An entrypoint was called by a scheduled or event claimant. Interactive
    /// entrypoints act only for a person present.
    #[error("interactive entrypoints need a person present")]
    NeedsPerson,
    /// The resolved instructions belong to another house than the binding.
    #[error("resolved instructions belong to another house")]
    HouseMismatch,
    /// A draft is malformed or over its bounds.
    #[error("invalid issue draft: {0}")]
    InvalidDraft(&'static str),
    /// A forge receipt did not name the created issue.
    #[error("forge receipt does not name the created issue")]
    UnreadableReceipt,
    /// A draft could not be encoded for its digest.
    #[error("issue draft could not be encoded")]
    Encoding,
    /// An acknowledgement named a task that is not an issue draft.
    #[error("only issue draft tasks can be acknowledged")]
    NotADraft,
    /// An acknowledgement reason is empty, multi-line, or too long.
    #[error("invalid acknowledgement reason")]
    InvalidReason,
    /// Sub-issue creation is not connected in this build.
    #[error("sub-issue creation is not available; it needs the decomposition workflow (#47)")]
    DecompositionUnavailable,
    /// The pull request's writer round counter is exhausted.
    #[error("pull request round counter is exhausted")]
    RoundOverflow,
    /// A session asked for more review-fix rounds than the house allows. A
    /// session may only lower the house budget.
    #[error("{requested} fix rounds exceed the house budget of {house}")]
    BudgetAboveHouse {
        /// Rounds the session asked for.
        requested: u8,
        /// The house budget.
        house: u8,
    },
}

impl InteractiveError {
    /// Broad handling class.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::NeedsPerson
            | Self::HouseMismatch
            | Self::DecompositionUnavailable
            | Self::BudgetAboveHouse { .. } => ErrorClass::Refused,
            Self::InvalidDraft(_) | Self::RoundOverflow | Self::NotADraft | Self::InvalidReason => {
                ErrorClass::InvalidInput
            }
            Self::UnreadableReceipt | Self::Encoding => ErrorClass::Execution,
        }
    }
}

/// Which entrypoint a person invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Entrypoint {
    /// Coordinate or implement one issue.
    Work {
        /// The issue.
        issue: IssueNumber,
    },
    /// Review, follow up, repair, or judge one pull request at its head.
    PullRequest {
        /// The pull request.
        number: IssueNumber,
    },
    /// Draft a new issue with the person.
    IssueNew,
    /// Refine an existing rough issue.
    IssueRefine {
        /// The issue.
        issue: IssueNumber,
    },
}

impl fmt::Display for Entrypoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Work { issue } => write!(formatter, "work #{}", issue.get()),
            Self::PullRequest { number } => write!(formatter, "pr #{}", number.get()),
            Self::IssueNew => formatter.write_str("issue new"),
            Self::IssueRefine { issue } => write!(formatter, "issue refine #{}", issue.get()),
        }
    }
}

/// Refuse a claimant that is not a person present.
fn require_person(claimant: &Claimant) -> Result<()> {
    match claimant.trigger {
        Trigger::Interactive => Ok(()),
        Trigger::Scheduled | Trigger::Event(_) => Err(InteractiveError::NeedsPerson.into()),
    }
}

/// A bound repository with its house policy and the instructions pinned for
/// new tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHouse {
    /// The registry binding for the repository.
    pub binding: RepositoryConfig,
    /// The house policy.
    pub config: HouseConfig,
    /// The verified instruction snapshot and pinned revisions. Agents read
    /// house rules from its entry point, never from the repository alone.
    pub instructions: ResolvedInstructions,
}

/// How the registry resolved the session's checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HouseResolution {
    /// The repository is bound; entrypoints may run.
    Ready(Box<ResolvedHouse>),
    /// Exactly one house claims the repository, which is not set up yet.
    /// Entrypoints refuse to run until the person binds it
    /// (`kitchn house setup`).
    NeedsSetup {
        /// The repository as the house allowlist names it.
        repository: Repository,
        /// The only claiming house.
        house: HouseId,
    },
}

/// Resolve the house for the checkout containing `start`, pinning the
/// repository instructions at `revision`.
///
/// # Errors
/// Fails closed with the registry's errors:
/// [`crate::house::HouseError::HouseSelection`] when no house claims the
/// checkout, [`crate::house::HouseError::AmbiguousHouse`] when several do
/// without a stored choice, [`crate::house::HouseError::RemotesDisagree`] when
/// remotes name different houses, and snapshot verification failures.
/// [`InteractiveError::HouseMismatch`] if the pinned instructions belong to
/// another house than the binding.
pub fn resolve_house(
    registry: &HouseRegistry,
    start: &Path,
    revision: CommitId,
) -> Result<HouseResolution> {
    let binding = match registry.resolve_repository(start)? {
        RepositoryMatch::Bound(binding) => binding,
        RepositoryMatch::Unbound { repository, house } => {
            return Ok(HouseResolution::NeedsSetup { repository, house });
        }
    };
    let instructions = registry.resolve(start, revision)?;
    if instructions.house != binding.house {
        return Err(InteractiveError::HouseMismatch.into());
    }
    let config = registry.load(&binding.house)?;
    Ok(HouseResolution::Ready(Box::new(ResolvedHouse {
        binding,
        config,
        instructions,
    })))
}

/// Why no orchestrator can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unavailable {
    /// No orchestrator evidence was supplied. Terminal variables such as
    /// `TERM_PROGRAM=Orca` are hints, not evidence.
    NotObserved,
    /// The evidence could not be read or parsed, or exceeded its bound.
    Unreadable,
    /// The runtime is not reachable and ready.
    RuntimeNotReady,
    /// The runtime version is outside the adapter's supported range.
    UnsupportedVersion,
    /// The runtime lacks a feature the adapter needs.
    MissingRuntimeFeature,
}

/// What the session knows about its orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Orchestrator {
    /// A ready, supported Orca runtime.
    Orca {
        /// The repository of the worktree this session runs in, from
        /// `projectId`, or `None` when Orca names no GitHub project.
        project: Option<Repository>,
        /// Capabilities the adapter declares for this runtime.
        capabilities: CapabilitySet,
    },
    /// No usable orchestrator.
    Unavailable(Unavailable),
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct StatusResult {
    runtime: RuntimeStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    state: String,
    reachable: bool,
    app_version: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
struct WorktreeResult {
    worktree: WorktreeStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeStatus {
    project_id: Option<String>,
}

fn envelope<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    if bytes.len() > MAX_ORCA_OUTPUT_BYTES {
        return None;
    }
    let envelope: Envelope<T> = serde_json::from_slice(bytes).ok()?;
    envelope.ok.then_some(envelope.result).flatten()
}

impl Orchestrator {
    /// Interpret the output of `orca status --json` and `orca worktree
    /// current --json`, captured in this session's worktree.
    ///
    /// Anything unreadable, oversized, not ready, unsupported, or missing a
    /// required runtime feature is [`Orchestrator::Unavailable`], never a
    /// guess. The worktree's `projectId` must be `github:<owner>/<name>`;
    /// any other value names no project.
    #[must_use]
    pub fn from_orca(status: &[u8], worktree: &[u8]) -> Self {
        let Some(status) = envelope::<StatusResult>(status) else {
            return Self::Unavailable(Unavailable::Unreadable);
        };
        let Some(worktree) = envelope::<WorktreeResult>(worktree) else {
            return Self::Unavailable(Unavailable::Unreadable);
        };
        let runtime = status.runtime;
        if !runtime.reachable || runtime.state != "ready" {
            return Self::Unavailable(Unavailable::RuntimeNotReady);
        }
        if !OrcaVersion::parse(&runtime.app_version).is_some_and(OrcaVersion::is_supported) {
            return Self::Unavailable(Unavailable::UnsupportedVersion);
        }
        if !REQUIRED_FEATURES
            .iter()
            .all(|feature| runtime.capabilities.iter().any(|have| have == feature))
        {
            return Self::Unavailable(Unavailable::MissingRuntimeFeature);
        }
        let project = worktree
            .worktree
            .project_id
            .as_deref()
            .and_then(|id| id.strip_prefix("github:"))
            .and_then(|name| Repository::new(name).ok());
        Self::Orca {
            project,
            capabilities: orca_capabilities(),
        }
    }
}

/// Why an entrypoint runs as a single agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SoloReason {
    /// No usable orchestrator.
    Unavailable {
        /// Why.
        reason: Unavailable,
    },
    /// The orchestrator's worktree names no project.
    ProjectUnknown,
    /// The orchestrator's worktree belongs to another repository.
    ProjectMismatch,
    /// The orchestrator lacks capabilities that fan-out needs.
    MissingCapabilities {
        /// The missing capabilities.
        missing: Vec<Capability>,
    },
}

impl fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotObserved => "no orchestrator evidence",
            Self::Unreadable => "orchestrator evidence unreadable",
            Self::RuntimeNotReady => "Orca runtime not ready",
            Self::UnsupportedVersion => "unsupported Orca version",
            Self::MissingRuntimeFeature => "Orca runtime lacks a required feature",
        })
    }
}

impl fmt::Display for SoloReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason } => reason.fmt(formatter),
            Self::ProjectUnknown => formatter.write_str("the Orca worktree names no project"),
            Self::ProjectMismatch => {
                formatter.write_str("the Orca worktree belongs to another repository")
            }
            Self::MissingCapabilities { missing } => {
                formatter.write_str("Orca lacks ")?;
                for (position, capability) in missing.iter().enumerate() {
                    if position > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{capability}")?;
                }
                Ok(())
            }
        }
    }
}

/// Whether work may fan out to workers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ExecutionMode {
    /// Positive evidence: the orchestrator runs this repository's worktree
    /// and supports every capability supervised workers need.
    FanOut,
    /// Work alone in this session; fan-out is reported unavailable.
    Solo {
        /// Why.
        reason: SoloReason,
    },
}

impl ExecutionMode {
    /// Whether workers may be launched.
    #[must_use]
    pub const fn fans_out(&self) -> bool {
        match self {
            Self::FanOut => true,
            Self::Solo { .. } => false,
        }
    }
}

/// Decide whether work in `repository` may fan out through `orchestrator`.
/// Only fan-out needs orchestrator capabilities; every entrypoint works
/// without them.
#[must_use]
pub fn execution_mode(repository: &Repository, orchestrator: &Orchestrator) -> ExecutionMode {
    let solo = |reason| ExecutionMode::Solo { reason };
    let (project, capabilities) = match orchestrator {
        Orchestrator::Unavailable(reason) => {
            return solo(SoloReason::Unavailable { reason: *reason });
        }
        Orchestrator::Orca {
            project,
            capabilities,
        } => (project, capabilities),
    };
    let Some(project) = project else {
        return solo(SoloReason::ProjectUnknown);
    };
    if !project.as_str().eq_ignore_ascii_case(repository.as_str()) {
        return solo(SoloReason::ProjectMismatch);
    }
    let missing: Vec<Capability> = REQUIRED_WORKER_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| !capabilities.supports(*capability))
        .collect();
    if missing.is_empty() {
        ExecutionMode::FanOut
    } else {
        solo(SoloReason::MissingCapabilities { missing })
    }
}

/// Why a claim was not obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClaimRefusal {
    /// Another claimant holds a live claim, under the named trigger. Ask its
    /// owner for a hand-back; do not work on it.
    Held {
        /// The holder's trigger.
        trigger: Trigger,
    },
    /// The previous claim expired without a hand-back. Only an explicit
    /// takeover by the person may proceed.
    OwnerUncertain,
    /// The task already settled.
    Settled {
        /// How.
        settlement: Settlement,
    },
}

/// A claim for the session, or why there is none.
type Claimed = std::result::Result<Lease, ClaimRefusal>;

/// How [`claim_task`] treats claims that already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClaimPolicy {
    /// Take over an expired claim, recorded as a takeover. A live claim is
    /// never taken over.
    pub(crate) take_over: bool,
    /// Renew the claimant's own live claim instead of refusing it, so a
    /// person who reruns an entrypoint continues their work.
    pub(crate) resume: bool,
}

/// Claim `spec`'s task for `claimant`, creating it on first use. An existing
/// task keeps its original specification.
fn claim_task(
    store: &HouseStore,
    spec: TaskSpec,
    claimant: &Claimant,
    ttl: LeaseTtl,
    now: Timestamp,
    policy: ClaimPolicy,
) -> Result<Claimed> {
    let id = spec.id.clone();
    match store.task(&id) {
        Ok(_) => {}
        Err(crate::Error::State(StateError::TaskNotFound(_))) => {
            match store.create_task(spec, claimant, now) {
                Ok(_) | Err(crate::Error::State(StateError::TaskConflict(_))) => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    let held = |store: &HouseStore| -> Result<Claimed> {
        let trigger = match store.task(&id)?.state() {
            TaskState::Claimed { lease } => lease.trigger().clone(),
            TaskState::Open | TaskState::Settled { .. } => claimant.trigger.clone(),
        };
        Ok(Err(ClaimRefusal::Held { trigger }))
    };
    match store.claim(&id, claimant, ttl, now) {
        Ok(lease) => Ok(Ok(lease)),
        Err(crate::Error::State(StateError::ClaimHeld { .. })) if policy.resume => {
            match store.task(&id)?.state() {
                TaskState::Claimed { lease }
                    if lease.holder() == &claimant.holder
                        && lease.trigger() == &claimant.trigger =>
                {
                    Ok(Ok(store.renew(&id, lease.fence(), ttl, now)?))
                }
                TaskState::Claimed { .. } | TaskState::Open | TaskState::Settled { .. } => {
                    held(store)
                }
            }
        }
        Err(crate::Error::State(StateError::ClaimHeld { .. })) => held(store),
        Err(crate::Error::State(StateError::LeaseExpired { .. })) if policy.take_over => {
            match store.take_over(&id, claimant, ttl, now) {
                Ok(lease) => Ok(Ok(lease)),
                Err(crate::Error::State(StateError::LeaseLive { .. })) => held(store),
                Err(error) => Err(error),
            }
        }
        Err(crate::Error::State(StateError::LeaseExpired { .. })) => {
            Ok(Err(ClaimRefusal::OwnerUncertain))
        }
        Err(crate::Error::State(StateError::TaskSettled { settlement, .. })) => {
            Ok(Err(ClaimRefusal::Settled { settlement }))
        }
        Err(error) => Err(error),
    }
}

/// The result of [`hand_back`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum HandBack {
    /// The claim was relinquished; the next claimant adopts the task.
    Released,
    /// `holder` holds no interactive claim on the task; nothing changed.
    NotHeld,
}

/// Give `holder`'s interactive claim on `task` back so the next claimant,
/// scheduled or interactive, adopts the task. The relinquish is recorded in
/// the task's ownership history. A claim held by anyone else, or under
/// another trigger, is left alone.
///
/// # Errors
/// Store errors, including a claim that changed concurrently.
pub fn hand_back(
    store: &HouseStore,
    task: &TaskId,
    holder: &HolderId,
    now: Timestamp,
) -> Result<HandBack> {
    let fence = match store.task(task)?.state() {
        TaskState::Claimed { lease }
            if lease.holder() == holder && *lease.trigger() == Trigger::Interactive =>
        {
            lease.fence()
        }
        TaskState::Claimed { .. } | TaskState::Open | TaskState::Settled { .. } => {
            return Ok(HandBack::NotHeld);
        }
    };
    store.relinquish(task, fence, now)?;
    Ok(HandBack::Released)
}

/// Whether the task's current claim adopted relinquished work.
fn adopted(store: &HouseStore, task: &TaskId) -> Result<bool> {
    Ok(matches!(
        store.task(task)?.ownership().last(),
        Some(OwnershipEvent::Adopted { .. })
    ))
}

/// One issue as the forge reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueStatus {
    /// Open.
    Open,
    /// Closed.
    Closed,
}

/// One sub-issue of the issue being worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubIssue {
    /// Its number.
    pub number: IssueNumber,
    /// Its status.
    pub status: IssueStatus,
    /// Whether an open blocked-by dependency holds it back.
    #[serde(default)]
    pub blocked: bool,
}

/// What `work` needs to know about the issue, read from the forge and from
/// the agent's reading of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssueFacts {
    /// The issue's status.
    pub status: IssueStatus,
    /// Existing sub-issues.
    #[serde(default)]
    pub sub_issues: Vec<SubIssue>,
    /// Whether the issue has independent parts that could be split into
    /// sub-issues. Only consulted when it has none.
    #[serde(default)]
    pub independent_parts: bool,
}

/// Why an entrypoint has nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Idle {
    /// The issue is closed.
    IssueClosed,
    /// Every sub-issue is closed; closing the parent is the person's call.
    SubIssuesDone,
    /// The pull request merged.
    Merged,
    /// The pull request is closed without merging.
    Closed,
    /// Repair was requested but the pull request merges cleanly.
    NothingToRepair,
}

/// What `work` does next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WorkPlan {
    /// Nothing to do; no claim was taken.
    Idle {
        /// Why.
        reason: Idle,
    },
    /// Someone else owns the issue; nothing was claimed.
    Skipped {
        /// Why.
        refusal: ClaimRefusal,
    },
    /// Coordinate existing sub-issues: fan out to workers when the mode
    /// allows, otherwise take them one at a time in this session.
    Coordinate {
        /// The parent issue's task.
        task: TaskId,
        /// Whether the claim adopted work a previous claimant handed back.
        adopted: bool,
        /// Open sub-issues with no open blocker.
        ready: Vec<IssueNumber>,
        /// Open sub-issues that wait for a blocker.
        waiting: Vec<IssueNumber>,
        /// Whether workers may be launched.
        fan_out: bool,
    },
    /// The issue has independent parts and no sub-issues: propose a split.
    /// Sub-issues and blocked-by links are created only after the person
    /// approves the exact preview, through [`Decomposer`].
    ProposeSplit {
        /// The issue's task.
        task: TaskId,
        /// Whether the claim adopted work a previous claimant handed back.
        adopted: bool,
        /// Whether a split may fan out once approved.
        fan_out: bool,
    },
    /// Implement the issue in this session.
    Implement {
        /// The issue's task.
        task: TaskId,
        /// Whether the claim adopted work a previous claimant handed back.
        adopted: bool,
    },
}

/// Everything `work` acts on.
pub struct WorkRequest<'a> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The house's task template, identical to scheduled pickup's.
    pub template: &'a TaskTemplate,
    /// The issue.
    pub issue: &'a IssueRef,
    /// The forge's facts about it.
    pub facts: &'a IssueFacts,
    /// The person's session; must be interactive.
    pub claimant: &'a Claimant,
    /// Claim lease.
    pub ttl: LeaseTtl,
    /// Current time.
    pub now: Timestamp,
    /// The session's execution mode.
    pub mode: &'a ExecutionMode,
    /// Take over an expired claim. Only the person asks for this.
    pub take_over: bool,
}

/// `work <issue>`: claim the issue on the task scheduled pickup shares, then
/// coordinate its sub-issues, propose a split, or implement it.
///
/// # Errors
/// [`InteractiveError::NeedsPerson`] for a non-interactive claimant, and
/// store errors.
pub fn work(request: &WorkRequest<'_>) -> Result<(WorkPlan, Option<Lease>)> {
    require_person(request.claimant)?;
    let facts = request.facts;
    if facts.status == IssueStatus::Closed {
        return Ok((
            WorkPlan::Idle {
                reason: Idle::IssueClosed,
            },
            None,
        ));
    }
    let open: Vec<&SubIssue> = facts
        .sub_issues
        .iter()
        .filter(|sub| sub.status == IssueStatus::Open)
        .collect();
    if !facts.sub_issues.is_empty() && open.is_empty() {
        return Ok((
            WorkPlan::Idle {
                reason: Idle::SubIssuesDone,
            },
            None,
        ));
    }
    let spec = request.template.spec_for(request.issue)?;
    let task = spec.id.clone();
    let lease = match claim_task(
        request.store,
        spec,
        request.claimant,
        request.ttl,
        request.now,
        ClaimPolicy {
            take_over: request.take_over,
            resume: true,
        },
    )? {
        Ok(lease) => lease,
        Err(refusal) => return Ok((WorkPlan::Skipped { refusal }, None)),
    };
    let adopted = adopted(request.store, &task)?;
    let fan_out = request.mode.fans_out();
    let plan = if !open.is_empty() {
        let (waiting, ready): (Vec<&SubIssue>, Vec<&SubIssue>) =
            open.into_iter().partition(|sub| sub.blocked);
        WorkPlan::Coordinate {
            task,
            adopted,
            ready: ready.iter().map(|sub| sub.number).collect(),
            waiting: waiting.iter().map(|sub| sub.number).collect(),
            fan_out,
        }
    } else if facts.independent_parts {
        WorkPlan::ProposeSplit {
            task,
            adopted,
            fan_out,
        }
    } else {
        WorkPlan::Implement { task, adopted }
    };
    Ok((plan, Some(lease)))
}

/// Sub-issue creation for `work` and `issue refine`.
///
/// The decomposition workflow (#47) implements this: `preview` validates a
/// proposal and returns its rendered preview and digest, and `apply` writes
/// the sub-issues and blocked-by links only for an approval of that digest,
/// under an interactive claimant, idempotently across reruns. Proposals are
/// passed as the workflow's own JSON so this seam does not fix its schema.
pub trait Decomposer {
    /// Preview the split `proposal` of `parent`.
    ///
    /// # Errors
    /// The workflow's validation errors.
    fn preview(&self, parent: &IssueRef, proposal: &[u8]) -> Result<SplitPreview>;

    /// Write the split the person approved.
    ///
    /// # Errors
    /// The workflow's errors; nothing is written for a stale approval.
    fn apply(
        &self,
        parent: &IssueRef,
        proposal: &[u8],
        approval: &DraftApproval,
        claimant: &Claimant,
    ) -> Result<SplitOutcome>;
}

/// A previewed split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPreview {
    /// The preview to show the person, verbatim.
    pub rendered: String,
    /// The digest an approval must name.
    pub digest: String,
    /// Whether the preview can be applied.
    pub ready: bool,
}

/// How applying a split ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitOutcome {
    /// Every sub-issue and link exists.
    Completed {
        /// The sub-issues, in creation order.
        sub_issues: Vec<IssueNumber>,
    },
    /// Nothing or only part was written; the report says why, and a rerun
    /// completes it without duplicates.
    Incomplete {
        /// The workflow's report.
        report: String,
    },
}

/// The decomposer of a build without the decomposition workflow: it refuses,
/// so a split is reported unavailable instead of faked.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDecomposer;

impl Decomposer for NoDecomposer {
    fn preview(&self, _: &IssueRef, _: &[u8]) -> Result<SplitPreview> {
        Err(InteractiveError::DecompositionUnavailable.into())
    }

    fn apply(
        &self,
        _: &IssueRef,
        _: &[u8],
        _: &DraftApproval,
        _: &Claimant,
    ) -> Result<SplitOutcome> {
        Err(InteractiveError::DecompositionUnavailable.into())
    }
}

/// What the person asked `pr` to do, if they named it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrIntent {
    /// Review the head.
    Review,
    /// Address review feedback.
    FollowUp,
    /// Resolve conflicts with the base.
    Repair,
    /// Give a gate verdict for the head.
    Gate,
}

/// Review state of the pull request's current head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewState {
    /// No review of the current head yet.
    Unreviewed,
    /// Review feedback on the current head is not addressed.
    ChangesRequested,
    /// Reviewed with no open feedback.
    Reviewed,
}

/// What `pr` needs to know about the pull request, read from the forge.
/// Rounds already spent are not a fact the session supplies: [`pull_request`]
/// reads them from the house store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFacts {
    /// The forge's view of the pull request.
    pub view: PullRequestView,
    /// Review state at `view.head`.
    pub review: ReviewState,
}

/// What `pr` does next. Every plan names the exact head it applies to; a
/// moved head voids it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum PrPlan {
    /// Nothing to do; no claim was taken.
    Idle {
        /// Why.
        reason: Idle,
    },
    /// The forge has not computed mergeability; read it again before acting.
    Recheck {
        /// The head read.
        head: CommitId,
    },
    /// Someone else writes to this pull request; nothing was claimed.
    Skipped {
        /// Why.
        refusal: ClaimRefusal,
    },
    /// The house's fix-round budget, or the lower one the person asked for,
    /// is spent; the person decides.
    BudgetExhausted {
        /// Rounds spent, from the house store.
        rounds_used: u8,
    },
    /// Review the head. Read-only: findings are posted only with consent.
    Review {
        /// The exact head.
        head: CommitId,
    },
    /// Evaluate the gate at the head. Read-only: the verdict is void when
    /// the head or base moves, and merging stays the person's decision.
    Gate {
        /// The exact head.
        head: CommitId,
    },
    /// Address review feedback as this round's writer.
    FollowUp {
        /// The exact head the feedback is about.
        head: CommitId,
        /// The writer round's task, shared with scheduled repair.
        task: TaskId,
        /// The round.
        round: u8,
    },
    /// Resolve conflicts as this round's writer.
    Repair {
        /// The exact head that conflicts.
        head: CommitId,
        /// The writer round's task, shared with scheduled repair.
        task: TaskId,
        /// The round.
        round: u8,
    },
}

/// Everything `pr` acts on.
pub struct PrRequest<'a> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The house's task template.
    pub template: &'a TaskTemplate,
    /// The repository.
    pub repository: &'a Repository,
    /// The forge's facts.
    pub facts: &'a PrFacts,
    /// What the person asked for, or `None` to route from the facts.
    pub intent: Option<PrIntent>,
    /// The house's follow-up budget, from
    /// [`crate::house::HouseConfig::follow_up_budget`].
    pub follow_up: FollowUpBudget,
    /// A lower review-fix round budget the person asked for, or `None` for
    /// the house budget. It can never raise it.
    pub fix_rounds: Option<u8>,
    /// The person's session; must be interactive.
    pub claimant: &'a Claimant,
    /// Claim lease for writer rounds.
    pub ttl: LeaseTtl,
    /// Current time.
    pub now: Timestamp,
    /// Take over an expired writer claim. Only the person asks for this.
    pub take_over: bool,
}

/// Route from the facts when the person named no intent.
fn route(facts: &PrFacts) -> Option<PrIntent> {
    match facts.view.mergeability {
        Mergeability::Conflicting => return Some(PrIntent::Repair),
        Mergeability::Unknown => return None,
        Mergeability::Clean | Mergeability::Behind => {}
    }
    Some(match facts.review {
        ReviewState::ChangesRequested => PrIntent::FollowUp,
        ReviewState::Unreviewed => PrIntent::Review,
        ReviewState::Reviewed => PrIntent::Gate,
    })
}

/// The pull request's current writer round, from the durable repair tasks
/// both triggers claim: the first round whose task has not settled, which
/// is either held or relinquished by its writer, or not yet created. Every
/// earlier round is spent. Rounds are created in order, so the scan stops at
/// the first round without a task.
fn current_round(store: &HouseStore, repository: &Repository, number: IssueNumber) -> Result<u8> {
    let mut round: u8 = 1;
    loop {
        match store.task(&repair_task_id(repository, number, round)?) {
            Ok(record) => match record.state() {
                TaskState::Settled { .. } => {
                    round = round
                        .checked_add(1)
                        .ok_or(InteractiveError::RoundOverflow)?;
                }
                TaskState::Open | TaskState::Claimed { .. } => return Ok(round),
            },
            Err(crate::Error::State(StateError::TaskNotFound(_))) => return Ok(round),
            Err(error) => return Err(error),
        }
    }
}

/// `pr <number>`: review, follow up, repair, or judge one pull request at
/// its exact head. A writer round claims the current round's task, the one
/// scheduled repair derives and claims, so a round scheduled repair holds is
/// skipped and the two never write to the same pull request at once. Rounds
/// spent come from the house store and the budget from the house's
/// follow-up policy, never from the session.
///
/// # Errors
/// [`InteractiveError::NeedsPerson`] for a non-interactive claimant,
/// [`InteractiveError::BudgetAboveHouse`] when `fix_rounds` exceeds the
/// house budget, [`InteractiveError::RoundOverflow`] at the round counter's
/// limit, and store errors.
pub fn pull_request(request: &PrRequest<'_>) -> Result<(PrPlan, Option<Lease>)> {
    require_person(request.claimant)?;
    let budget = match request.fix_rounds {
        None => request.follow_up.fix_rounds,
        Some(requested) if requested <= request.follow_up.fix_rounds => requested,
        Some(requested) => {
            return Err(InteractiveError::BudgetAboveHouse {
                requested,
                house: request.follow_up.fix_rounds,
            }
            .into());
        }
    };
    let facts = request.facts;
    let head = facts.view.head.clone();
    match facts.view.state {
        PullRequestState::Open => {}
        PullRequestState::Merged => {
            return Ok((
                PrPlan::Idle {
                    reason: Idle::Merged,
                },
                None,
            ));
        }
        PullRequestState::Closed => {
            return Ok((
                PrPlan::Idle {
                    reason: Idle::Closed,
                },
                None,
            ));
        }
    }
    let intent = match request.intent.or_else(|| route(facts)) {
        Some(intent) => intent,
        None => return Ok((PrPlan::Recheck { head }, None)),
    };
    let repair = match intent {
        PrIntent::Review => return Ok((PrPlan::Review { head }, None)),
        PrIntent::Gate => return Ok((PrPlan::Gate { head }, None)),
        PrIntent::Repair => match facts.view.mergeability {
            Mergeability::Conflicting => true,
            Mergeability::Unknown => return Ok((PrPlan::Recheck { head }, None)),
            Mergeability::Clean | Mergeability::Behind => {
                return Ok((
                    PrPlan::Idle {
                        reason: Idle::NothingToRepair,
                    },
                    None,
                ));
            }
        },
        PrIntent::FollowUp => false,
    };
    let round = current_round(request.store, request.repository, facts.view.number)?;
    let rounds_used = round.saturating_sub(1);
    if rounds_used >= budget {
        return Ok((PrPlan::BudgetExhausted { rounds_used }, None));
    }
    let task = repair_task_id(request.repository, facts.view.number, round)?;
    let template = request.template;
    let spec = TaskSpec {
        id: task.clone(),
        role: crate::contracts::Role::StationCook,
        repository: Some(request.repository.clone()),
        authority: template.authority.clone(),
        retry: template.retry,
        provenance: template.provenance.clone(),
        resources: BTreeSet::new(),
        requires: crate::contracts::CapabilityRequirements::new(),
        agent: crate::workflows::pickup::resolve_agent(
            template.agents.as_ref(),
            crate::contracts::Role::StationCook,
            request.repository,
        ),
    };
    let lease = match claim_task(
        request.store,
        spec,
        request.claimant,
        request.ttl,
        request.now,
        ClaimPolicy {
            take_over: request.take_over,
            resume: true,
        },
    )? {
        Ok(lease) => lease,
        Err(refusal) => return Ok((PrPlan::Skipped { refusal }, None)),
    };
    let plan = if repair {
        PrPlan::Repair { head, task, round }
    } else {
        PrPlan::FollowUp { head, task, round }
    };
    Ok((plan, Some(lease)))
}
