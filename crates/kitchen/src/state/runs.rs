//! The run ledger: one entry per tick pass run, kept in the house store.
//!
//! The ledger is the source of truth for when a pass last ran and how it
//! ended; [`crate::workflows::tick`] owns the rules. It is bounded: retention
//! drops settled runs older than [`RUN_RETENTION`] and keeps at most
//! [`MAX_RUNS_PER_PASS`] settled runs per pass. A running or uncertain entry
//! is never dropped. The pass lease allows one running entry per pass, and a
//! pass does not start while it has an uncertain one, so each pass has at
//! most one of each.

use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    HolderId, TaskId,
    contracts::{ExternalRef, Fence, Text, Timestamp},
    state::MAX_ACKNOWLEDGEMENT_REASON_BYTES,
    workflows::tick::{
        MAX_PASS_RUNTIME, MAX_RUN_EVIDENCE, MAX_RUN_TASKS, Pass, PassOutcome, PassReport, RunUsage,
        TickError,
    },
};

/// Settled (ended, or uncertain and settled by a person) runs the ledger
/// keeps per pass.
pub const MAX_RUNS_PER_PASS: usize = 64;
/// How long the ledger keeps a settled run.
pub const RUN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Entries the ledger holds at most: per pass, its kept runs, one running
/// entry, and one uncertain entry awaiting a person.
const MAX_RUNS: usize = Pass::ALL.len() * (MAX_RUNS_PER_PASS + 2);

/// A run's ledger id, unique within the house store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(u64);

impl RunId {
    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "run {}", self.0)
    }
}

impl FromStr for RunId {
    type Err = TickError;

    /// The number `kitchn tick runs` prints after `run`.
    fn from_str(text: &str) -> Result<Self, TickError> {
        text.parse().map(Self).map_err(|_| TickError::UnknownRun)
    }
}

/// Where a run stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RunState {
    /// The pass is running under the run's lease.
    Running,
    /// The pass recorded its end.
    Ended {
        /// When.
        ended_at: Timestamp,
        /// How it ended.
        outcome: PassOutcome,
        /// Usage where known.
        usage: RunUsage,
        /// The backend's own run references, linked as evidence only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        backend_runs: Vec<ExternalRef>,
    },
    /// The lease expired before the run recorded an end. Whether the pass
    /// finished its work, and which effects it had, is unknown. The pass
    /// does not run again until a person settles this run.
    Uncertain {
        /// When a later tick recorded it.
        recorded_at: Timestamp,
    },
    /// A person settled an uncertain run after checking what it did. How
    /// the run ended stays unknown; its own late end is refused.
    Settled {
        /// When a later tick recorded the run as uncertain.
        uncertain_at: Timestamp,
        /// The person's session that settled it.
        by: HolderId,
        /// When.
        settled_at: Timestamp,
        /// Why the person is content for the pass to run again.
        reason: Text,
        /// Unresolved effects of the tasks the run recorded, when settled.
        unresolved_effects: usize,
    },
}

/// One pass run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunRecord {
    /// The ledger id.
    pub id: RunId,
    /// The pass.
    pub pass: Pass,
    /// The tick that started it.
    pub holder: HolderId,
    /// The pass lease fence it ran under.
    pub fence: Fence,
    /// When it started.
    pub started_at: Timestamp,
    /// Tasks the run said it would touch, recorded before it touched them.
    /// A blocked run reports their unresolved effects; the list may be
    /// incomplete, so it never clears a run by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskId>,
    /// Where it stands.
    pub state: RunState,
}

impl RunRecord {
    /// When the run's outcome was settled, if it was.
    const fn settled_at(&self) -> Option<Timestamp> {
        match &self.state {
            RunState::Running | RunState::Uncertain { .. } => None,
            RunState::Ended { ended_at, .. } => Some(*ended_at),
            RunState::Settled { settled_at, .. } => Some(*settled_at),
        }
    }

    /// When the run's pass lease ends at the latest; renewal never extends
    /// it further.
    #[must_use]
    pub fn deadline(&self) -> Timestamp {
        self.started_at.saturating_add(MAX_PASS_RUNTIME)
    }

    /// A running entry under `fence` within its runtime at `now`, or why not.
    fn live(&self, fence: Fence, now: Timestamp) -> Result<(), TickError> {
        if self.fence != fence {
            return Err(TickError::NotRunOwner);
        }
        match self.state {
            RunState::Running if now < self.deadline() => Ok(()),
            RunState::Running | RunState::Uncertain { .. } | RunState::Settled { .. } => {
                Err(TickError::Superseded)
            }
            RunState::Ended { .. } => Err(TickError::AlreadyFinished),
        }
    }
}

/// What [`crate::state::HouseStore::start_run`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStart {
    /// The pass is due and now runs under `fence`.
    Started {
        /// The new ledger entry.
        run: RunId,
        /// The pass lease fence.
        fence: Fence,
    },
    /// An uncertain run blocks the pass until a person settles it. No
    /// lease is held.
    Blocked {
        /// The uncertain run.
        run: RunId,
        /// Unresolved effects of the tasks it recorded, for the report only.
        unresolved_effects: usize,
        /// Whether this call recorded it as uncertain.
        newly_uncertain: bool,
    },
    /// The pass is not due.
    NotDue {
        /// When it is due.
        next_due: Timestamp,
    },
    /// Another tick holds the pass lease.
    Busy {
        /// The holder.
        holder: HolderId,
        /// When the lease expires.
        expires_at: Timestamp,
    },
}

/// What [`crate::state::HouseStore::settle_run`] recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum RunSettle {
    /// The run is settled now; the pass may run again when due.
    Settled(RunRecord),
    /// A person settled it earlier; that record stands and nothing changed.
    AlreadySettled(RunRecord),
}

/// The ledger table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunLedger {
    /// The next run id; ids are never reused, even after retention.
    next: u64,
    runs: Vec<RunRecord>,
}

impl RunLedger {
    pub(crate) const fn new() -> Self {
        Self {
            next: 0,
            runs: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.next == 0 && self.runs.is_empty()
    }

    pub(crate) fn runs(&self) -> &[RunRecord] {
        &self.runs
    }

    /// When `pass` is next due, or `None` when it never ran.
    pub(crate) fn next_due(&self, pass: Pass, every: Duration) -> Option<Timestamp> {
        self.runs
            .iter()
            .filter(|run| run.pass == pass)
            .map(|run| run.started_at)
            .max()
            .map(|last| last.saturating_add(every))
    }

    /// Record `pass`'s running entry, if any, as uncertain.
    pub(crate) fn mark_uncertain(&mut self, pass: Pass, now: Timestamp) -> Option<RunId> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.pass == pass && run.state == RunState::Running)?;
        run.state = RunState::Uncertain { recorded_at: now };
        Some(run.id)
    }

    /// `pass`'s uncertain run.
    pub(crate) fn uncertain(&self, pass: Pass) -> Option<&RunRecord> {
        self.runs
            .iter()
            .find(|run| run.pass == pass && matches!(run.state, RunState::Uncertain { .. }))
    }

    pub(crate) fn get(&self, run: RunId) -> Result<&RunRecord, TickError> {
        self.runs
            .iter()
            .find(|record| record.id == run)
            .ok_or(TickError::UnknownRun)
    }

    fn get_mut(&mut self, run: RunId) -> Result<&mut RunRecord, TickError> {
        self.runs
            .iter_mut()
            .find(|record| record.id == run)
            .ok_or(TickError::UnknownRun)
    }

    /// Running `run` under `fence` within its runtime at `now`, or why not.
    pub(crate) fn live(
        &self,
        run: RunId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<&RunRecord, TickError> {
        let record = self.get(run)?;
        record.live(fence, now)?;
        Ok(record)
    }

    /// Record that live `run` is about to touch `task`. Repeating is a no-op.
    pub(crate) fn touch(
        &mut self,
        run: RunId,
        fence: Fence,
        task: &TaskId,
        now: Timestamp,
    ) -> Result<(), TickError> {
        let record = self.get_mut(run)?;
        record.live(fence, now)?;
        if record.tasks.contains(task) {
            return Ok(());
        }
        if record.tasks.len() >= MAX_RUN_TASKS {
            return Err(TickError::TooManyTasks { max: MAX_RUN_TASKS });
        }
        record.tasks.push(task.clone());
        Ok(())
    }

    /// Record that a person settled uncertain `run` of `pass`. Settling it
    /// again returns the first record unchanged.
    pub(crate) fn settle(
        &mut self,
        pass: Pass,
        run: RunId,
        by: &HolderId,
        reason: &Text,
        unresolved_effects: usize,
        now: Timestamp,
    ) -> Result<RunSettle, TickError> {
        let record = self.get_mut(run)?;
        if record.pass != pass {
            return Err(TickError::UnknownRun);
        }
        match record.state {
            RunState::Uncertain { recorded_at } => {
                record.state = RunState::Settled {
                    uncertain_at: recorded_at,
                    by: by.clone(),
                    settled_at: now,
                    reason: reason.clone(),
                    unresolved_effects,
                };
                Ok(RunSettle::Settled(record.clone()))
            }
            RunState::Settled { .. } => Ok(RunSettle::AlreadySettled(record.clone())),
            RunState::Running | RunState::Ended { .. } => Err(TickError::NotUncertain),
        }
    }

    pub(crate) fn start(
        &mut self,
        pass: Pass,
        holder: &HolderId,
        fence: Fence,
        now: Timestamp,
    ) -> RunId {
        self.retain(pass, now);
        let id = RunId(self.next);
        self.next = self.next.saturating_add(1);
        self.runs.push(RunRecord {
            id,
            pass,
            holder: holder.clone(),
            fence,
            started_at: now,
            tasks: Vec::new(),
            state: RunState::Running,
        });
        id
    }

    /// Record the end of `run`, which the caller checked is live under its
    /// pass lease. A repeated identical end is a no-op. A run past its
    /// deadline or recorded as uncertain was superseded: its late end is
    /// refused.
    pub(crate) fn finish(
        &mut self,
        run: RunId,
        fence: Fence,
        report: PassReport,
        now: Timestamp,
    ) -> Result<Pass, TickError> {
        if report.backend_runs.len() > MAX_RUN_EVIDENCE {
            return Err(TickError::TooMuchEvidence {
                max: MAX_RUN_EVIDENCE,
            });
        }
        let record = self.get_mut(run)?;
        if record.fence != fence {
            return Err(TickError::NotRunOwner);
        }
        let deadline = record.deadline();
        match &record.state {
            RunState::Running if now < deadline => {
                record.state = RunState::Ended {
                    ended_at: now,
                    outcome: report.outcome,
                    usage: report.usage,
                    backend_runs: report.backend_runs,
                };
                Ok(record.pass)
            }
            RunState::Ended {
                outcome,
                usage,
                backend_runs,
                ..
            } if *outcome == report.outcome
                && *usage == report.usage
                && *backend_runs == report.backend_runs =>
            {
                Ok(record.pass)
            }
            RunState::Ended { .. } => Err(TickError::AlreadyFinished),
            RunState::Running | RunState::Uncertain { .. } | RunState::Settled { .. } => {
                Err(TickError::Superseded)
            }
        }
    }

    /// Drop settled runs past [`RUN_RETENTION`], then the oldest settled
    /// runs beyond [`MAX_RUNS_PER_PASS`] per pass, leaving room for the run
    /// of `starting`. Running and uncertain entries stay.
    fn retain(&mut self, starting: Pass, now: Timestamp) {
        let droppable_at = RunRecord::settled_at;
        self.runs.retain(|run| {
            droppable_at(run).is_none_or(|at| now.saturating_since(at) < RUN_RETENTION)
        });
        for pass in Pass::ALL {
            let settled = self
                .runs
                .iter()
                .filter(|run| run.pass == pass && droppable_at(run).is_some())
                .count();
            let keep = if pass == starting {
                MAX_RUNS_PER_PASS.saturating_sub(1)
            } else {
                MAX_RUNS_PER_PASS
            };
            let mut excess = settled.saturating_sub(keep);
            // Runs are appended in start order, so the first ones are oldest.
            self.runs.retain(|run| {
                if excess > 0 && run.pass == pass && droppable_at(run).is_some() {
                    excess -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Bounds, unique ids below `next`, and at most one running and one
    /// uncertain entry per pass.
    pub(crate) fn validate(&self) -> bool {
        let bounded = self.runs.len() <= MAX_RUNS
            && self.runs.iter().all(|run| {
                run.id.0 < self.next
                    && run.tasks.len() <= MAX_RUN_TASKS
                    && match &run.state {
                        RunState::Ended { backend_runs, .. } => {
                            backend_runs.len() <= MAX_RUN_EVIDENCE
                        }
                        RunState::Settled { reason, .. } => {
                            reason.as_str().len() <= MAX_ACKNOWLEDGEMENT_REASON_BYTES
                        }
                        RunState::Running | RunState::Uncertain { .. } => true,
                    }
            });
        let ordered = self.runs.windows(2).all(|pair| match pair {
            [earlier, later] => earlier.id < later.id,
            _ => true,
        });
        let single_running = Pass::ALL.iter().all(|pass| {
            let of_pass = |open: fn(&RunState) -> bool| {
                self.runs
                    .iter()
                    .filter(|run| run.pass == *pass && open(&run.state))
                    .count()
            };
            of_pass(|state| *state == RunState::Running) <= 1
                && of_pass(|state| matches!(state, RunState::Uncertain { .. })) <= 1
        });
        bounded && ordered && single_running
    }
}
