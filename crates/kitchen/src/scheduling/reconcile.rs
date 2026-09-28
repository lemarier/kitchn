//! Schedule observations and install reconciliation.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId,
    contracts::{ResourceRef, Timestamp},
    state::{ConsumerEvent, ConsumerRecord},
    trust::Measurement,
};

/// Most recent runs one observation reports. Budgets count runs over a
/// window, so this covers a day of runs every 15 minutes.
pub const MAX_SCHEDULE_RUNS: usize = 100;

/// A schedule's state as a backend reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservedScheduleState {
    /// Installed and firing.
    Active,
    /// Installed and paused.
    Paused,
    /// The backend has no such schedule.
    Missing,
    /// The backend cannot tell.
    Unknown,
}

/// What one scheduled run did, as the backend recorded it.
///
/// No variant says the agent started: a backend's "completed launch" is not
/// readiness evidence. [`run_verdict`] combines this with Kitchen's own
/// readiness evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunOutcome {
    /// Queued or still starting.
    Pending,
    /// The backend reports the launch step finished. The agent may still
    /// never have started.
    LaunchReported,
    /// The precheck found nothing to do.
    PrecheckIdle,
    /// The precheck failed, timed out, or exited with an unexpected code.
    /// This is an error to report, never idle.
    PrecheckFailed,
    /// Skipped for another reason, such as a missed window or an unavailable host.
    Skipped,
    /// The backend failed to launch the agent.
    LaunchFailed,
    /// The backend reported a status Kitchen does not recognize.
    Unknown,
}

/// One scheduled run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleRun {
    /// What the run did.
    pub outcome: RunOutcome,
    /// When it was due, when the backend reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<Timestamp>,
    /// When the backend recorded the run. Places a run with no due time,
    /// such as a trial, in a usage window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,
    /// Tokens the run used as the backend reports them. A backend that
    /// reports no usage leaves it missing or unavailable, never zero.
    pub usage: Measurement<u64>,
}

/// How a scheduled run ended, once readiness evidence is taken into account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunVerdict {
    /// Still within its readiness deadline.
    Pending,
    /// Kitchen observed positive readiness evidence from the run's agent.
    Started,
    /// The agent never became ready: a launch failure, never a completed or
    /// idle run.
    LaunchFailed,
    /// The precheck found nothing to do.
    Idle,
    /// The precheck failed and must be reported.
    PrecheckFailed,
    /// Skipped before any launch for another reason.
    Skipped,
    /// The backend's record cannot be interpreted.
    Unknown,
}

/// Decide how a run ended.
///
/// `ready` is Kitchen's own positive evidence that the run's agent started,
/// such as the run acquiring its workflow consumer lease. Without it, a run
/// the backend reports as launched is pending until `deadline` after it was
/// due, then a launch failure. A run with no due time cannot age out, so it
/// stays pending until evidence or an explicit decision.
#[must_use]
pub fn run_verdict(
    run: &ScheduleRun,
    ready: bool,
    now: Timestamp,
    deadline: std::time::Duration,
) -> RunVerdict {
    let expired = run
        .scheduled_for
        .is_some_and(|due| now.saturating_since(due) > deadline);
    match run.outcome {
        RunOutcome::Pending | RunOutcome::LaunchReported if ready => RunVerdict::Started,
        RunOutcome::Pending | RunOutcome::LaunchReported if expired => RunVerdict::LaunchFailed,
        RunOutcome::Pending | RunOutcome::LaunchReported => RunVerdict::Pending,
        RunOutcome::LaunchFailed => RunVerdict::LaunchFailed,
        RunOutcome::PrecheckIdle => RunVerdict::Idle,
        RunOutcome::PrecheckFailed => RunVerdict::PrecheckFailed,
        RunOutcome::Skipped => RunVerdict::Skipped,
        RunOutcome::Unknown => RunVerdict::Unknown,
    }
}

/// Kitchen's own record that a scheduled run's agent started, such as the
/// workflow acquiring its consumer lease. A backend's report that a launch
/// finished is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReadinessSignal {
    at: Timestamp,
}

impl ReadinessSignal {
    /// A signal recorded at `at`.
    #[must_use]
    pub const fn new(at: Timestamp) -> Self {
        Self { at }
    }

    /// When Kitchen recorded it.
    #[must_use]
    pub const fn at(self) -> Timestamp {
        self.at
    }

    /// The signals in a consumer scope's recorded history: each time a
    /// consumer acquired, adopted, or took over the scope. Handing the scope
    /// over (relinquish, release) is not readiness.
    ///
    /// Scheduled and interactive consumers share a scope, so a session that
    /// acquired it inside a run's deadline counts for that run; pass only
    /// the signals that belong to scheduled runs when they can be told apart.
    #[must_use]
    pub fn from_consumer(record: &ConsumerRecord) -> Vec<Self> {
        record
            .history()
            .filter_map(|event| match event {
                ConsumerEvent::Acquired { at, .. }
                | ConsumerEvent::Adopted { at, .. }
                | ConsumerEvent::TakenOver { at, .. } => Some(Self::new(*at)),
                ConsumerEvent::Relinquished { .. } | ConsumerEvent::Released { .. } => None,
            })
            .collect()
    }
}

/// The evidence and clock a schedule's runs are judged against.
#[derive(Debug, Clone, Copy)]
pub struct Readiness<'a> {
    signals: &'a [ReadinessSignal],
    now: Timestamp,
    deadline: Duration,
}

impl<'a> Readiness<'a> {
    /// Judge runs at `now`, allowing each run `deadline` after it was due to
    /// show a signal in `signals`.
    #[must_use]
    pub const fn new(signals: &'a [ReadinessSignal], now: Timestamp, deadline: Duration) -> Self {
        Self {
            signals,
            now,
            deadline,
        }
    }

    /// Judge each run with [`run_verdict`].
    ///
    /// A signal counts for the newest run due at or before it, and only when
    /// it came within the deadline: a session that started long after a
    /// swallowed launch does not vindicate that launch. A run with no due
    /// time has nothing to join a signal to and stays pending.
    #[must_use]
    pub fn judge(&self, runs: &[ScheduleRun]) -> Vec<JudgedRun> {
        runs.iter()
            .map(|run| {
                let ready = run.scheduled_for.is_some_and(|due| {
                    let next_due = runs
                        .iter()
                        .filter_map(|other| other.scheduled_for)
                        .filter(|other| *other > due)
                        .min();
                    let end = due.saturating_add(self.deadline);
                    self.signals.iter().any(|signal| {
                        signal.at >= due
                            && signal.at <= end
                            && next_due.is_none_or(|next| signal.at < next)
                    })
                });
                JudgedRun {
                    run: run.clone(),
                    verdict: run_verdict(run, ready, self.now, self.deadline),
                }
            })
            .collect()
    }
}

/// A run and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JudgedRun {
    /// What the backend recorded.
    pub run: ScheduleRun,
    /// How it ended once readiness evidence is taken into account.
    pub verdict: RunVerdict,
}

/// A read-only view of one schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleObservation {
    /// Its state.
    pub state: ObservedScheduleState,
    /// Up to [`MAX_SCHEDULE_RUNS`] runs with their verdicts, newest first. A
    /// run with no due time, such as a trial or one still dispatching, counts
    /// as the newest and comes first.
    pub recent_runs: Vec<JudgedRun>,
}

/// A part of a schedule definition an installed schedule can differ in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScheduleField {
    /// The prompt the agent receives.
    Prompt,
    /// The agent family.
    Agent,
    /// When it fires.
    Recurrence,
    /// The time zone it fires in.
    Timezone,
    /// The precheck command or its timeout.
    Precheck,
    /// Where runs happen, including the repository and base branch.
    Workspace,
    /// The missed-run grace window.
    MissedRunGrace,
    /// Whether runs reuse the previous session.
    SessionReuse,
}

/// A schedule a backend reports as installed for this house.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSchedule {
    /// The backend resource.
    pub resource: ResourceRef,
    /// The workflow consumer scope decoded from the backend's native name.
    pub consumer: ConsumerId,
    /// Its state.
    pub state: ObservedScheduleState,
}

/// What an install must do after reconciling against the inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlan {
    /// No schedule serves this consumer; creating one is safe.
    Create,
    /// Exactly one schedule serves this consumer; reuse it instead of creating another.
    Installed(ResourceRef),
    /// Several schedules serve this consumer. Creating would add another one;
    /// the duplicates need an explicit decision.
    Duplicates(Vec<ResourceRef>),
}

/// Decide whether installing a schedule for `consumer` may create one.
///
/// A schedule whose state is unknown still counts: when unsure, reuse or
/// refuse rather than add a consumer.
///
/// `installed` must be a complete inventory of this house's schedules. An
/// incomplete or failed listing must not reach this function, because a
/// missing entry would allow a duplicate create.
#[must_use]
pub fn plan_install(consumer: &ConsumerId, installed: &[InstalledSchedule]) -> InstallPlan {
    let mut matches: Vec<ResourceRef> = installed
        .iter()
        .filter(|schedule| {
            &schedule.consumer == consumer && schedule.state != ObservedScheduleState::Missing
        })
        .map(|schedule| schedule.resource.clone())
        .collect();
    match matches.as_slice() {
        [] => InstallPlan::Create,
        [only] => InstallPlan::Installed(only.clone()),
        [_, _, ..] => {
            matches.sort();
            InstallPlan::Duplicates(matches)
        }
    }
}
