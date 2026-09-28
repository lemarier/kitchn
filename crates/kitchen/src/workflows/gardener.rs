//! Independent daily issue hygiene policy. The schedule owner supplies an
//! inventory; this workflow only previews changes under separate house grants.

use super::{ClaimState, Precheck, WorkflowError, known, valid_label};
use crate::{
    BackendId, HouseId,
    contracts::{
        Capability, CloseReason, ContractError, GitHubAction, Grant, GrantScope, HouseGrants,
        IssueNumber, Permission, Repository, Timestamp,
    },
    integrations::github::{GitHubClient, GitHubReadTransport, IssueState},
};

/// Portable declaration until #6's schedule payload accepts a workflow owner
/// and typed precheck binding. This declaration cannot activate a live job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Stable workflow owner.
    pub owner: &'static str,
    /// Cadence selected by the house scheduler.
    pub cadence: &'static str,
    /// Stable typed precheck name.
    pub precheck: &'static str,
}

/// Gardener's independent daily schedule.
pub const SCHEDULE: Schedule = Schedule {
    owner: "gardener",
    cadence: "daily",
    precheck: "gardener-hygiene",
};

/// Required backend support before scheduling is permitted.
pub const REQUIRED_CAPABILITIES: [Capability; 4] = [
    Capability::ScheduleManage,
    Capability::SchedulePrecheck,
    Capability::ScheduleSingleConsumer,
    Capability::ScheduleRunTimeout,
];

/// One issue with complete relationship and label evidence.
#[derive(Debug, Clone)]
pub struct Issue {
    /// Number.
    pub number: IssueNumber,
    /// Provider lifecycle; unknown is never treated as open or closed.
    pub state: IssueState,
    /// Human only.
    pub human_only: bool,
    /// Durable claim observation.
    pub claim: ClaimState,
    /// Labels.
    pub labels: Vec<String>,
    /// Prose blocker.
    pub prose_blocker: Option<IssueNumber>,
    /// Linked blocker.
    pub linked_blocker: bool,
    /// Blocker open.
    pub blocker_open: bool,
    /// Parent completed.
    pub parent_completed: bool,
    /// Merged work.
    pub merged_work: bool,
    /// Duplicate of.
    pub duplicate_of: Option<IssueNumber>,
    /// Stale.
    pub stale: bool,
}

/// House-selected labels that Kitchen may inspect for residue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLabels {
    /// Label for an unclaimed ready issue.
    pub ready: String,
    /// Label mirrored from a durable claim.
    pub working: String,
}

/// Why the independent gardener should inspect a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    /// Daily changes.
    pub daily_changes: bool,
    /// Stale issue.
    pub stale_issue: bool,
    /// Closed agent label.
    pub closed_agent_label: bool,
}

/// The inventory window of one precheck. `since` should be the start of the
/// last completed pass, so a failed day is re-read rather than skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    since: Timestamp,
    stale_before: Timestamp,
}

impl Window {
    /// Changes at or after `since`; open issues untouched before
    /// `stale_before` are stale.
    ///
    /// # Errors
    /// Refuses a stale cutoff after the change window starts.
    pub fn new(since: Timestamp, stale_before: Timestamp) -> Result<Self, WorkflowError> {
        if stale_before > since {
            return Err(WorkflowError::IncompleteEvidence);
        }
        Ok(Self {
            since,
            stale_before,
        })
    }
}

/// Read the changed and open issue inventory for the precheck. A partial or
/// unrecognized read is an error, never an idle day.
pub fn signal<T: GitHubReadTransport>(
    client: &GitHubClient<T>,
    house: &HouseId,
    repo: &Repository,
    labels: &AgentLabels,
    window: Window,
) -> Result<Signal, WorkflowError> {
    let changed = known(client.issues_filtered(house, repo, None, Some(window.since)))?;
    let open = known(client.issues_filtered(house, repo, Some(IssueState::Open), None))?;
    let mut closed_agent_label = false;
    for issue in &changed {
        match issue.state {
            IssueState::Open => {}
            IssueState::Closed => {
                closed_agent_label |= issue
                    .labels
                    .iter()
                    .any(|label| label.name == labels.ready || label.name == labels.working);
            }
            IssueState::Unknown => return Err(WorkflowError::IncompleteEvidence),
        }
    }
    if open.iter().any(|issue| issue.state != IssueState::Open) {
        return Err(WorkflowError::IncompleteEvidence);
    }
    Ok(Signal {
        daily_changes: !changed.is_empty(),
        stale_issue: open
            .iter()
            .any(|issue| issue.updated_at < window.stale_before),
        closed_agent_label,
    })
}

/// Evidence that the house holds a standing [`Permission::CloseIssue`] grant
/// for one repository. No other permission implies it, and consent-only
/// policy limits do not count for unattended runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseAuthority {
    repository: Repository,
}

impl CloseAuthority {
    /// The authority for `repository` on `destination`, or `None` without a
    /// standing grant.
    ///
    /// # Errors
    /// Refuses policy that names two credentials for the same grant.
    pub fn from_grants(
        grants: &HouseGrants,
        repository: &Repository,
        destination: &BackendId,
    ) -> Result<Option<Self>, WorkflowError> {
        let scope = GrantScope::Repository(repository.clone());
        let credential = match grants.permitted(Permission::CloseIssue, &scope, destination) {
            Ok(credential) => credential,
            Err(ContractError::AuthorityExpansion { .. }) => return Ok(None),
            Err(_) => return Err(WorkflowError::DecisionMismatch),
        };
        let grant = Grant::repository(
            Permission::CloseIssue,
            repository.clone(),
            destination.clone(),
            credential,
        );
        Ok(grants.covers(&grant).then(|| Self {
            repository: repository.clone(),
        }))
    }
}

/// Distinct gardener precheck. Errors remain errors, not idle ticks.
pub fn precheck(signal: Result<Signal, WorkflowError>) -> Result<Precheck, WorkflowError> {
    let signal = signal?;
    if signal.daily_changes || signal.stale_issue || signal.closed_agent_label {
        Ok(Precheck::Actionable)
    } else {
        Ok(Precheck::Idle)
    }
}

/// A proposed action and its separate permission boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A mutation suitable for a fresh-read and authority check.
    Mutation(GitHubAction),
    /// Human assessment is needed; no automatic close or duplicate mark.
    Review {
        /// Candidate issue.
        issue: IssueNumber,
        /// Reason to ask for review.
        reason: ReviewReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Reason for review without an automatic close.
pub enum ReviewReason {
    /// Completed parent.
    CompletedParent,
    /// Merged work.
    MergedWork,
    /// Duplicate.
    Duplicate,
    /// Stale.
    Stale,
}

/// Preview only actionable findings. A closed issue's agent label is removed
/// only when it has no live claim. Human-only and claimed work is untouched.
/// Completed, merged, and duplicate issues are proposed for closure only with
/// `close` authority for `repository`; stale issues always need review.
pub fn plan(
    repository: &Repository,
    issues: &[Issue],
    labels: &AgentLabels,
    close: Option<&CloseAuthority>,
) -> Result<Vec<Finding>, WorkflowError> {
    if !valid_label(&labels.ready)
        || !valid_label(&labels.working)
        || labels.ready == labels.working
    {
        return Err(WorkflowError::IncompleteEvidence);
    }
    if close.is_some_and(|authority| &authority.repository != repository) {
        return Err(WorkflowError::DecisionMismatch);
    }
    let mut findings = Vec::new();
    for issue in issues {
        if issue.state == IssueState::Unknown {
            return Err(WorkflowError::IncompleteEvidence);
        }
        if issue.claim == ClaimState::Unknown {
            return Err(WorkflowError::IncompleteEvidence);
        }
        if issue.human_only || issue.claim == ClaimState::ClaimedByOther {
            continue;
        }
        if issue.state == IssueState::Closed {
            for label in &issue.labels {
                if label == &labels.ready || label == &labels.working {
                    findings.push(Finding::Mutation(GitHubAction::SetLabel {
                        issue: issue.number,
                        label: label.clone(),
                        present: false,
                    }));
                }
            }
            continue;
        }
        if let Some(blocker) = issue.prose_blocker
            && issue.blocker_open
            && !issue.linked_blocker
            && blocker != issue.number
        {
            findings.push(Finding::Mutation(GitHubAction::LinkDependency {
                issue: issue.number,
                blocker,
            }));
        }
        let closure = match issue.duplicate_of {
            Some(original) if original != issue.number => Some(CloseReason::Duplicate(original)),
            Some(_) => return Err(WorkflowError::IncompleteEvidence),
            None if issue.parent_completed || issue.merged_work => Some(CloseReason::Completed),
            None => None,
        };
        if let (Some(reason), Some(_)) = (closure, close) {
            findings.push(Finding::Mutation(GitHubAction::CloseIssue {
                repository: repository.clone(),
                number: issue.number,
                reason,
            }));
            continue;
        }
        for (active, reason) in [
            (issue.parent_completed, ReviewReason::CompletedParent),
            (issue.merged_work, ReviewReason::MergedWork),
            (issue.duplicate_of.is_some(), ReviewReason::Duplicate),
            (issue.stale, ReviewReason::Stale),
        ] {
            if active {
                findings.push(Finding::Review {
                    issue: issue.number,
                    reason,
                });
            }
        }
    }
    Ok(findings)
}
