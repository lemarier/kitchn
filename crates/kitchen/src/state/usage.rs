//! Token usage, cost, and human time per task attempt.
//!
//! Every attempt carries an [`AttemptUsage`]: what the worker backend
//! reported for it, or an explicit [`AttemptUsage::NotReported`]. A report
//! keeps each quantity the backend did not give as `None`, never zero, and
//! only a backend that declares [`Capability::UsageAttribution`] may report.
//!
//! Human time is never reported by anyone. [`HumanTime`] is derived from
//! events the store recorded when they happened: a person's reply to a
//! worker question ([`crate::state::HouseStore::record_human_reply`]) and
//! claims held under [`Trigger::Interactive`], the sessions in which a
//! person reviewed or reworked the task.
//!
//! Records live on their task: they are house-scoped with it, bounded by the
//! task's attempt budget and [`MAX_HUMAN_REPLIES_PER_ATTEMPT`], and retire
//! with it under the house retention policy
//! ([`crate::state::RetentionPolicy`]).
//!
//! [`Capability::UsageAttribution`]: crate::contracts::Capability::UsageAttribution

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, ErrorClass, TaskId,
    contracts::{AttemptNumber, ExternalRef, IssueNumber, Repository, Role, Timestamp, Trigger},
    scheduling::AgentFamily,
    selection::{AgentModel, WorkType},
    state::{AttemptRecord, AttemptState, OwnershipEvent, TaskRecord, TaskState},
};

/// Person replies to worker questions kept per attempt.
pub const MAX_HUMAN_REPLIES_PER_ATTEMPT: usize = 64;

/// Tokens by kind, as the backend reported them. `None` is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenCounts {
    /// Input tokens, excluding cache reads and writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// Input tokens read from the provider's prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    /// Input tokens written to the provider's prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
}

impl TokenCounts {
    /// The sum of every kind, when every kind is known.
    #[must_use]
    pub fn total(&self) -> Option<u64> {
        [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .try_fold(0_u64, |sum, count| sum.checked_add(count?))
    }

    const fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
    }
}

/// An amount in millionths of a US dollar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UsdMicros(pub u64);

/// Where a cost came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CostBasis {
    /// The backend or provider reported the charge.
    Reported,
    /// Computed from reported tokens and the provider's price list.
    Computed,
}

/// The cost of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cost {
    /// The amount.
    pub amount: UsdMicros,
    /// Reported or computed.
    pub basis: CostBasis,
}

/// What a worker backend reported for one attempt. Each field the backend
/// did not report stays `None`; a report must state at least one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageReport {
    /// The backend's reference for the report, such as its run id.
    pub source: ExternalRef,
    /// The agent family that ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentFamily>,
    /// The model that ran, as the backend reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AgentModel>,
    /// Tokens by kind.
    #[serde(default)]
    pub tokens: TokenCounts,
    /// The cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

impl UsageReport {
    /// Whether the report states nothing beyond its source.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.agent.is_none()
            && self.model.is_none()
            && self.tokens.is_empty()
            && self.cost.is_none()
    }
}

/// An attempt's usage record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AttemptUsage {
    /// No backend reported usage for the attempt: its backend lacks
    /// usage attribution, reported nothing, or the attempt was stored
    /// before usage was recorded. Its cost is unknown, not zero.
    #[default]
    NotReported,
    /// The worker backend's report.
    Reported {
        /// The reporting backend.
        backend: BackendId,
        /// The report.
        report: UsageReport,
        /// When the store recorded it.
        at: Timestamp,
    },
}

/// A person's reply to a worker question, recorded when it was delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HumanReply {
    /// The backend's reference for the question.
    pub question: ExternalRef,
    /// When the worker asked, as the backend reported it.
    pub asked_at: Timestamp,
    /// When the store recorded the reply.
    pub answered_at: Timestamp,
}

impl HumanReply {
    /// How long the worker waited for the person.
    #[must_use]
    pub const fn latency(&self) -> Duration {
        self.answered_at.saturating_since(self.asked_at)
    }
}

/// Human time spent on one attempt, derived from recorded events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HumanTime {
    /// Reply latency to worker questions answered outside an interactive
    /// session. Latency is how long the worker waited, an upper bound on the
    /// person's attention.
    pub replies: Duration,
    /// Interactive sessions: time a person-present claim was held within
    /// the attempt's window.
    pub sessions: Duration,
    /// `false` when a session in the window has no recorded end (still
    /// held, or taken over after its lease expired). The durations are then
    /// lower bounds.
    pub complete: bool,
}

impl HumanTime {
    /// Replies and sessions together. A reply answered during a session
    /// counts only in the session.
    #[must_use]
    pub const fn total(&self) -> Duration {
        self.replies.saturating_add(self.sessions)
    }
}

/// One attempt's usage, linked to its task, station, work type, and pull
/// request, as the brigade audit reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptUsageEntry {
    /// The task.
    pub task: TaskId,
    /// The attempt.
    pub attempt: AttemptNumber,
    /// The station: the task's role.
    pub station: Role,
    /// The work type the task was created for, when recorded.
    pub work_type: Option<WorkType>,
    /// The task's repository.
    pub repository: Option<Repository>,
    /// The pull request linked to the task.
    pub pull_request: Option<IssueNumber>,
    /// Whether the attempt ended: finished, interrupted, or cancelled.
    pub ended: bool,
    /// Tokens, model, and cost, or an explicit not-reported.
    pub usage: AttemptUsage,
    /// Human time derived from recorded events.
    pub human: HumanTime,
}

impl AttemptUsageEntry {
    pub(super) fn of_task(task: &TaskRecord) -> impl Iterator<Item = Self> + '_ {
        let spec = task.spec();
        task.attempts()
            .iter()
            .enumerate()
            .map(move |(index, attempt)| Self {
                task: spec.id.clone(),
                attempt: attempt.number(),
                station: spec.role,
                work_type: spec.work_type.clone(),
                repository: spec.repository.clone(),
                pull_request: task.pull_request(),
                ended: attempt.state() != AttemptState::Running,
                usage: attempt.usage().clone(),
                human: human_time(task, index),
            })
    }
}

/// Why usage could not be recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UsageError {
    /// The report states nothing beyond its source; record nothing and the
    /// attempt stays not reported.
    #[error("usage report states no usage")]
    EmptyReport,
    /// The attempt already carries a different report.
    #[error("attempt {0} already has a different usage report")]
    AlreadyReported(AttemptNumber),
    /// The reply is dated before its question.
    #[error("reply is recorded before its question was asked")]
    ReplyBeforeQuestion,
    /// The attempt holds [`MAX_HUMAN_REPLIES_PER_ATTEMPT`] replies.
    #[error("attempt already holds the maximum number of human replies")]
    TooManyReplies,
    /// A pull request needs the task's repository.
    #[error("task has no repository for a pull request")]
    NoRepository,
    /// The task is already linked to another pull request.
    #[error("task is linked to pull request #{}", .0.get())]
    PullRequestConflict(IssueNumber),
}

impl UsageError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::EmptyReport | Self::ReplyBeforeQuestion | Self::NoRepository => {
                ErrorClass::InvalidInput
            }
            Self::AlreadyReported(_) | Self::PullRequestConflict(_) => ErrorClass::Conflict,
            Self::TooManyReplies => ErrorClass::Refused,
        }
    }
}

/// A claim's recorded span.
struct Session {
    interactive: bool,
    start: Timestamp,
    /// `None` when no end was recorded.
    end: Option<Timestamp>,
}

/// Replay the ownership history into claim spans.
fn sessions(history: &[OwnershipEvent]) -> Vec<Session> {
    let mut spans = Vec::new();
    let mut open: Option<(crate::contracts::Fence, Session)> = None;
    for event in history {
        match event {
            OwnershipEvent::Claimed {
                trigger, fence, at, ..
            }
            | OwnershipEvent::Adopted {
                trigger, fence, at, ..
            }
            | OwnershipEvent::TakenOver {
                trigger, fence, at, ..
            } => {
                // A takeover follows an expired lease whose end was not
                // recorded; so does any claim that replaces an open one.
                spans.extend(open.take().map(|(_, session)| session));
                open = Some((
                    *fence,
                    Session {
                        interactive: *trigger == Trigger::Interactive,
                        start: *at,
                        end: None,
                    },
                ));
            }
            OwnershipEvent::Relinquished { fence, at } | OwnershipEvent::Released { fence, at } => {
                if let Some((held, mut session)) = open.take() {
                    if held == *fence {
                        session.end = Some(*at);
                        spans.push(session);
                    } else {
                        open = Some((held, session));
                    }
                }
            }
        }
    }
    spans.extend(open.map(|(_, session)| session));
    spans
}

/// Human time for the attempt at `index`. Its window runs from its start to
/// the next attempt's start, or to settlement for the last attempt, so
/// review and rework after a worker finished count toward that attempt.
fn human_time(task: &TaskRecord, index: usize) -> HumanTime {
    let attempts = task.attempts();
    let Some(attempt) = attempts.get(index) else {
        return HumanTime::default();
    };
    let start = attempt.started_at();
    let end = attempts
        .get(index.saturating_add(1))
        .map(AttemptRecord::started_at)
        .or(match task.state() {
            TaskState::Settled { at, .. } => Some(*at),
            TaskState::Open | TaskState::Claimed { .. } => None,
        });
    let mut time = HumanTime {
        complete: true,
        ..HumanTime::default()
    };
    let interactive: Vec<Session> = sessions(task.ownership())
        .into_iter()
        .filter(|session| session.interactive)
        .collect();
    for session in &interactive {
        let from = session.start.max(start);
        let Some(until) = session.end else {
            if end.is_none_or(|end| session.start < end) {
                time.complete = false;
            }
            continue;
        };
        let until = end.map_or(until, |end| until.min(end));
        time.sessions = time.sessions.saturating_add(until.saturating_since(from));
    }
    let in_session = |at: Timestamp| {
        interactive
            .iter()
            .any(|session| session.start <= at && session.end.is_none_or(|end| at <= end))
    };
    time.replies = attempt
        .replies()
        .iter()
        .filter(|reply| !in_session(reply.answered_at))
        .fold(Duration::ZERO, |sum, reply| {
            sum.saturating_add(reply.latency())
        });
    time
}
