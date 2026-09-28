//! PR repair: which settled, owned branches may get a repair writer, in
//! which order, within which budgets, and the push check every writer runs
//! immediately before pushing.
//!
//! Repair never touches a branch with a live writer, a person's terminal,
//! dirty or unpushed work, or a merged or closed pull request. Stacks repair
//! bottom-up with one writer per layer. Semantic work (resolving a conflict,
//! addressing review feedback) belongs to the scoped repair worker; the
//! lifecycle and authority checks live here.

use std::collections::BTreeSet;

use crate::{
    HouseId, TaskId,
    contracts::{
        CommitId, IssueNumber, Provenance, Repository, ResourceRef, RetryPolicy, Role, Settlement,
        TaskAuthority, TaskSpec,
    },
    integrations::github::{
        GitHubClient, GitHubReadTransport, IssueState, MergeState, Observation, PullRequest,
    },
    workflows::pickup::{BranchName, FollowUpBudget, derived_task_id},
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Largest number of repairs in flight at once, outside issue capacity.
pub const MAX_CONCURRENT_REPAIRS: usize = 2;

/// A fact that could not be read is unknown, never a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed<T> {
    /// Known value.
    Known(T),
    /// Could not be established.
    Unknown,
}

/// Pull request lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    /// Open.
    Open,
    /// Closed without merging.
    Closed,
    /// Merged.
    Merged,
}

/// Whether the pull request merges cleanly into its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeability {
    /// Merges cleanly.
    Clean,
    /// Has conflicts.
    Conflicting,
    /// Behind its base without conflicts.
    Behind,
    /// The forge has not computed it yet.
    Unknown,
}

/// The facts repair and the push check need about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestView {
    /// Number.
    pub number: IssueNumber,
    /// Lifecycle.
    pub state: PullRequestState,
    /// Current head commit.
    pub head: CommitId,
    /// Head branch name, verbatim.
    pub head_branch: String,
    /// Mergeability.
    pub mergeability: Mergeability,
}

impl PullRequestView {
    /// Map #7's pull-request read to the repair view.
    #[must_use]
    pub fn from_github(pull_request: &PullRequest) -> Self {
        let state = match (pull_request.merged, pull_request.state) {
            (true, _) => PullRequestState::Merged,
            (false, IssueState::Open) => PullRequestState::Open,
            // An unrecognized state is treated as not open: never push to it.
            (false, IssueState::Closed | IssueState::Unknown) => PullRequestState::Closed,
        };
        let mergeability = match (pull_request.mergeable, pull_request.mergeable_state) {
            (Some(false), _) | (_, Some(MergeState::Dirty)) => Mergeability::Conflicting,
            (_, Some(MergeState::Behind)) => Mergeability::Behind,
            (Some(true), _) => Mergeability::Clean,
            (None, _) => Mergeability::Unknown,
        };
        Self {
            number: pull_request.number,
            state,
            head: pull_request.head.sha.clone(),
            head_branch: pull_request.head.name.clone(),
            mergeability,
        }
    }
}

/// Read one pull request through #7's house-scoped client.
pub fn observe_pull_request<T: GitHubReadTransport>(
    client: &GitHubClient<T>,
    house: &HouseId,
    repository: &Repository,
    number: IssueNumber,
) -> Observed<PullRequestView> {
    match client.pull_request(house, repository, number) {
        Observation::Known(pull_request) => {
            Observed::Known(PullRequestView::from_github(&pull_request))
        }
        Observation::Unavailable(_) | Observation::Unknown => Observed::Unknown,
    }
}

/// Who writes to the branch now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Writer {
    /// No writer.
    None,
    /// A Kitchen task has a live or uncertain claim on the branch.
    Task(TaskId),
    /// A person took over the branch's terminal.
    Person,
    /// Could not be established.
    Unknown,
}

/// The local checkout that holds the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorktreeView {
    /// Uncommitted or untracked changes exist.
    pub dirty: Observed<bool>,
    /// Commits exist that the remote branch lacks.
    pub unpushed: Observed<bool>,
}

/// A stacked pull request's relation to its lower layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackLayer {
    /// Identifies the stack.
    pub stack: TaskId,
    /// One-based depth; 1 is based on the default branch.
    pub depth: u8,
    /// The lower layer merged.
    pub lower_merged: bool,
    /// The head already contains the lower layer's merge.
    pub contains_lower_merge: Observed<bool>,
}

/// Who owns the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// A Kitchen task created it and settled.
    Settled {
        /// The owning task.
        task: TaskId,
        /// Its settlement.
        settlement: Settlement,
    },
    /// A Kitchen task created it and has not settled.
    Unsettled(TaskId),
    /// Not created by Kitchen in this house.
    Foreign,
}

/// Everything repair decides on for one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairCandidate {
    /// The repository.
    pub repository: Repository,
    /// The pull request.
    pub pull_request: PullRequestView,
    /// The branch.
    pub branch: BranchName,
    /// Who owns the branch.
    pub ownership: Ownership,
    /// Current writer.
    pub writer: Writer,
    /// The checkout holding the branch.
    pub worktree: WorktreeView,
    /// Stack position, if stacked.
    pub stack: Option<StackLayer>,
    /// Earlier consecutive ticks that saw unknown mergeability.
    pub unknown_rechecks: u8,
    /// Repair and review-fix rounds already spent on this pull request.
    pub rounds_used: u8,
}

/// The repair a writer performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairKind {
    /// Resolve merge conflicts with the base.
    Conflict,
    /// Rebuild a stacked layer after its lower layer merged.
    Restack,
}

/// Why a person or the owning coordinator must decide instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandOver {
    /// A person controls the branch's terminal.
    PersonOwnsTerminal,
    /// Dirty or unpushed work would be at risk.
    PreserveWork,
    /// Local state could not be read.
    WorktreeUnknown,
    /// Mergeability stayed unknown across rechecks.
    MergeabilityUnknown,
    /// The repair and review-fix budget is spent.
    BudgetExhausted,
    /// Stack state could not be established.
    StackUnknown,
}

/// Why repair does nothing for this pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Merged or closed: never write again.
    Finished,
    /// Not created by Kitchen in this house.
    NotOwned,
    /// The owning task settled without success; a person decides.
    Unsuccessful,
    /// Someone is writing now.
    WriterActive,
    /// Nothing to repair.
    Healthy,
    /// A lower layer of the same stack is repaired first.
    LowerLayerFirst,
    /// No repair slot this tick.
    NoSlot,
}

/// The repair decision for one pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairDecision {
    /// Launch a repair writer.
    Repair(RepairKind),
    /// Read mergeability again later; push nothing.
    Recheck,
    /// Hand over to a person.
    HandOver(HandOver),
    /// Do nothing.
    Skip(Skip),
}

/// Repair policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairPolicy {
    /// Ticks of unknown mergeability before hand-over.
    pub max_unknown_rechecks: u8,
    /// Follow-up budgets; `fix_rounds` bounds repair plus review-fix rounds.
    pub budget: FollowUpBudget,
}

/// Decide what to do about one pull request, ignoring slots and stacks.
#[must_use]
pub fn assess(policy: &RepairPolicy, candidate: &RepairCandidate) -> RepairDecision {
    match candidate.pull_request.state {
        PullRequestState::Open => {}
        PullRequestState::Merged | PullRequestState::Closed => {
            return RepairDecision::Skip(Skip::Finished);
        }
    }
    match &candidate.ownership {
        Ownership::Settled {
            settlement: Settlement::Succeeded,
            ..
        } => {}
        Ownership::Settled { .. } => return RepairDecision::Skip(Skip::Unsuccessful),
        Ownership::Foreign => return RepairDecision::Skip(Skip::NotOwned),
        Ownership::Unsettled(_) => return RepairDecision::Skip(Skip::WriterActive),
    }
    match candidate.writer {
        Writer::None => {}
        Writer::Task(_) | Writer::Unknown => return RepairDecision::Skip(Skip::WriterActive),
        Writer::Person => return RepairDecision::HandOver(HandOver::PersonOwnsTerminal),
    }
    let kind = match (&candidate.stack, candidate.pull_request.mergeability) {
        (Some(layer), _) if layer.lower_merged => match layer.contains_lower_merge {
            Observed::Known(true) => None,
            Observed::Known(false) => Some(RepairKind::Restack),
            Observed::Unknown => return RepairDecision::HandOver(HandOver::StackUnknown),
        },
        (_, Mergeability::Conflicting) => Some(RepairKind::Conflict),
        (_, Mergeability::Clean | Mergeability::Behind) => None,
        (_, Mergeability::Unknown) => {
            return if candidate.unknown_rechecks >= policy.max_unknown_rechecks {
                RepairDecision::HandOver(HandOver::MergeabilityUnknown)
            } else {
                RepairDecision::Recheck
            };
        }
    };
    let Some(kind) = kind else {
        return RepairDecision::Skip(Skip::Healthy);
    };
    match (candidate.worktree.dirty, candidate.worktree.unpushed) {
        (Observed::Known(false), Observed::Known(false)) => {}
        (Observed::Known(true), _) | (_, Observed::Known(true)) => {
            return RepairDecision::HandOver(HandOver::PreserveWork);
        }
        (Observed::Unknown, _) | (_, Observed::Unknown) => {
            return RepairDecision::HandOver(HandOver::WorktreeUnknown);
        }
    }
    if candidate.rounds_used >= policy.budget.fix_rounds {
        return RepairDecision::HandOver(HandOver::BudgetExhausted);
    }
    RepairDecision::Repair(kind)
}

/// Decide every candidate: at most [`MAX_CONCURRENT_REPAIRS`] in flight, one
/// writer per stack at a time, lowest layer first.
#[must_use]
pub fn plan(
    policy: &RepairPolicy,
    candidates: &[RepairCandidate],
    in_flight: usize,
) -> Vec<(IssueNumber, RepairDecision)> {
    let mut ordered: Vec<&RepairCandidate> = candidates.iter().collect();
    ordered.sort_by_key(|candidate| {
        (
            candidate.stack.as_ref().map_or(0, |layer| layer.depth),
            candidate.pull_request.number.get(),
        )
    });
    let mut slots = MAX_CONCURRENT_REPAIRS.saturating_sub(in_flight);
    let mut busy_stacks: BTreeSet<&TaskId> = BTreeSet::new();
    let mut decisions = Vec::with_capacity(ordered.len());
    for candidate in ordered {
        let stack = candidate.stack.as_ref().map(|layer| &layer.stack);
        let mut decision = assess(policy, candidate);
        if let Some(stack) = stack {
            if busy_stacks.contains(stack) {
                decision = RepairDecision::Skip(Skip::LowerLayerFirst);
            } else if matches!(
                decision,
                RepairDecision::Repair(_)
                    | RepairDecision::Skip(Skip::WriterActive)
                    | RepairDecision::HandOver(_)
            ) {
                // A lower layer that is being written or waits for a person
                // blocks every layer above it.
                busy_stacks.insert(stack);
            }
        }
        if let RepairDecision::Repair(_) = decision {
            if slots == 0 {
                decision = RepairDecision::Skip(Skip::NoSlot);
            } else {
                slots = slots.saturating_sub(1);
            }
        }
        decisions.push((candidate.pull_request.number, decision));
    }
    decisions
}

/// The durable task id of a repair of pull request `number`.
///
/// # Errors
/// Never fails for valid inputs; the id syntax error is propagated defensively.
pub fn repair_task_id(
    repository: &Repository,
    number: IssueNumber,
    round: u8,
) -> Result<crate::TaskId> {
    derived_task_id(&format!("repair{round}"), repository, number)
}

/// The task spec for a repair writer. The existing worktree is handed to the
/// task, so the writer works in place and cannot target other resources.
#[must_use]
pub fn repair_spec(
    id: TaskId,
    repository: Repository,
    worktree: ResourceRef,
    authority: TaskAuthority,
    retry: RetryPolicy,
    provenance: Provenance,
) -> TaskSpec {
    TaskSpec {
        id,
        role: Role::StationCook,
        repository: Some(repository),
        authority,
        retry,
        provenance,
        resources: BTreeSet::from([worktree]),
        requires: crate::contracts::CapabilityRequirements::new(),
    }
}

/// What a writer is about to push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushIntent {
    /// The exact branch.
    pub branch: BranchName,
    /// The pull request the branch belongs to, once opened.
    pub pull_request: Option<IssueNumber>,
    /// The remote head the writer last saw; `None` before the first push.
    pub expected_remote: Option<CommitId>,
}

/// State read immediately before the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushObservation {
    /// The pull request, when the intent names one.
    pub pull_request: Observed<Option<PullRequestView>>,
    /// The remote branch head; `None` when the branch does not exist.
    pub remote_head: Observed<Option<CommitId>>,
}

/// Why a push must not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRefusal {
    /// The pull request merged; pushing would recreate a deleted branch.
    Merged,
    /// The pull request closed.
    Closed,
    /// The branch was deleted.
    BranchDeleted,
    /// Someone else moved the branch.
    RemoteMoved {
        /// The head found.
        found: CommitId,
    },
    /// The branch already exists though the writer expected to create it.
    BranchExists,
    /// The pull request's head branch is not this branch.
    WrongBranch,
    /// The named pull request does not exist.
    PullRequestMissing,
    /// State could not be read.
    Unknown,
}

/// Permission to push, valid only for the state it was checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct PushPermit {
    /// The remote head the push may replace, if any.
    pub replaces: Option<CommitId>,
}

/// Check PR and branch state immediately before a push. Every push needs a
/// fresh check; a stale permit proves nothing.
///
/// # Errors
/// Returns the [`PushRefusal`] that forbids pushing.
pub fn check_push(
    intent: &PushIntent,
    observed: &PushObservation,
) -> std::result::Result<PushPermit, PushRefusal> {
    if let Some(number) = intent.pull_request {
        let Observed::Known(pull_request) = &observed.pull_request else {
            return Err(PushRefusal::Unknown);
        };
        let Some(pull_request) = pull_request else {
            return Err(PushRefusal::PullRequestMissing);
        };
        if pull_request.number != number {
            return Err(PushRefusal::Unknown);
        }
        match pull_request.state {
            PullRequestState::Open => {}
            PullRequestState::Merged => return Err(PushRefusal::Merged),
            PullRequestState::Closed => return Err(PushRefusal::Closed),
        }
        if intent
            .branch
            .verify_observed(&pull_request.head_branch)
            .is_err()
        {
            return Err(PushRefusal::WrongBranch);
        }
    }
    let Observed::Known(remote) = &observed.remote_head else {
        return Err(PushRefusal::Unknown);
    };
    match (&intent.expected_remote, remote) {
        (None, None) => Ok(PushPermit { replaces: None }),
        (None, Some(_)) => Err(PushRefusal::BranchExists),
        (Some(_), None) => Err(PushRefusal::BranchDeleted),
        (Some(expected), Some(found)) if expected == found => Ok(PushPermit {
            replaces: Some(found.clone()),
        }),
        (Some(_), Some(found)) => Err(PushRefusal::RemoteMoved {
            found: found.clone(),
        }),
    }
}
