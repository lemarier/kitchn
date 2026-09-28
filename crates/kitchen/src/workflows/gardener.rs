//! Independent daily issue hygiene policy. The schedule owner supplies an
//! inventory; this workflow only previews changes under separate house grants.

use super::{Precheck, WorkflowError};
use crate::contracts::{Capability, GitHubAction, IssueNumber};

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
    /// Closed.
    pub closed: bool,
    /// Human only.
    pub human_only: bool,
    /// Claimed by other.
    pub claimed_by_other: bool,
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
pub fn plan(issues: &[Issue], labels: &AgentLabels) -> Result<Vec<Finding>, WorkflowError> {
    if labels.ready.is_empty() || labels.working.is_empty() || labels.ready == labels.working {
        return Err(WorkflowError::IncompleteEvidence);
    }
    let mut findings = Vec::new();
    for issue in issues {
        if issue.human_only || issue.claimed_by_other {
            continue;
        }
        if issue.closed {
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
