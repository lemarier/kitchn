//! The run ledger: one entry per tick pass run, kept in the house store.
//!
//! The ledger is the source of truth for when a pass last ran and how it
//! ended; [`crate::workflows::tick`] owns the rules. It is bounded: retention
//! drops settled runs older than [`RUN_RETENTION`] and keeps at most
//! [`MAX_RUNS_PER_PASS`] settled runs per pass. A running entry is never
//! dropped, nor an uncertain one unless its pass is idempotent. The pass
//! lease allows one running entry per pass, and a pass that is not
//! idempotent does not start while it has an uncertain one.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    HolderId, TaskId,
    contracts::{ExternalRef, Fence, Timestamp},
    workflows::tick::{
        MAX_PASS_RUNTIME, MAX_RUN_EVIDENCE, MAX_RUN_TASKS, Pass, PassOutcome, PassReport, Repeat,
        RunUsage, TickError,
    },
};

/// Settled (ended or recovered) runs the ledger keeps per pass.
pub const MAX_RUNS_PER_PASS: usize = 64;
/// How long the ledger keeps a settled run.
pub const RUN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Entries the ledger holds at most: per pass, its kept runs, one running
/// entry, and one uncertain entry awaiting reconciliation.
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
    /// finished its work is unknown. Unless the pass is idempotent, it does
    /// not run again until this run is reconciled.
    Uncertain {
        /// When a later tick recorded it.
        reconciled_at: Timestamp,
    },
    /// A later tick reconciled an uncertain run: the tasks it touched have
    /// no unresolved effects, and the runner established this outcome. The
    /// run's own late end is refused.
    Recovered {
        /// When the reconciliation was recorded.
        recovered_at: Timestamp,
        /// How the run ended, as reconciled.
        outcome: PassOutcome,
        /// Usage where known.
        usage: RunUsage,
        /// The backend's own run references, linked as evidence only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        backend_runs: Vec<ExternalRef>,
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
    /// Reconciliation checks their effects.
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
            RunState::Recovered { recovered_at, .. } => Some(*recovered_at),
        }
    }

    /// A live run under `fence`, or why not.
    fn live(&self, fence: Fence) -> Result<(), TickError> {
        if self.fence != fence {
            return Err(TickError::NotRunOwner);
        }
        match self.state {
            RunState::Running => Ok(()),
            RunState::Uncertain { .. } | RunState::Recovered { .. } => Err(TickError::Superseded),
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
        /// An earlier run recorded as uncertain first.
        uncertain: Option<RunId>,
    },
    /// An uncertain run blocks the pass. The caller holds the pass lease
    /// under `fence` to reconcile it and must settle or release it.
    Reconcile {
        /// The oldest uncertain run.
        record: RunRecord,
        /// The pass lease fence.
        fence: Fence,
        /// An earlier run recorded as uncertain first.
        uncertain: Option<RunId>,
    },
    /// The pass is not due.
    NotDue {
        /// When it is due.
        next_due: Timestamp,
        /// An earlier run recorded as uncertain first.
        uncertain: Option<RunId>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum RunSettle {
    /// The run is recovered; the pass may run again.
    Recovered,
    /// The run stays uncertain and keeps blocking the pass.
    Blocked {
        /// Unresolved effects of the tasks the run touched.
        unresolved_effects: usize,
    },
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
        run.state = RunState::Uncertain { reconciled_at: now };
        Some(run.id)
    }

    /// `pass`'s oldest uncertain run.
    pub(crate) fn oldest_uncertain(&self, pass: Pass) -> Option<&RunRecord> {
        self.runs
            .iter()
            .find(|run| run.pass == pass && matches!(run.state, RunState::Uncertain { .. }))
    }

    /// Uncertain `run`, or why it cannot be reconciled.
    pub(crate) fn uncertain(&self, run: RunId) -> Result<&RunRecord, TickError> {
        let record = self
            .runs
            .iter()
            .find(|record| record.id == run)
            .ok_or(TickError::UnknownRun)?;
        match record.state {
            RunState::Uncertain { .. } => Ok(record),
            RunState::Running => Err(TickError::NotUncertain),
            RunState::Ended { .. } | RunState::Recovered { .. } => Err(TickError::AlreadyFinished),
        }
    }

    fn get_mut(&mut self, run: RunId) -> Result<&mut RunRecord, TickError> {
        self.runs
            .iter_mut()
            .find(|record| record.id == run)
            .ok_or(TickError::UnknownRun)
    }

    /// Check that `run` is live under `fence` and may still act at `now`.
    pub(crate) fn check_live(
        &self,
        run: RunId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<Pass, TickError> {
        let record = self
            .runs
            .iter()
            .find(|record| record.id == run)
            .ok_or(TickError::UnknownRun)?;
        record.live(fence)?;
        if now.saturating_since(record.started_at) >= MAX_PASS_RUNTIME {
            return Err(TickError::RunTooLong);
        }
        Ok(record.pass)
    }

    /// Record that live `run` is about to touch `task`. Repeating is a no-op.
    pub(crate) fn touch(
        &mut self,
        run: RunId,
        fence: Fence,
        task: &TaskId,
    ) -> Result<(), TickError> {
        let record = self.get_mut(run)?;
        record.live(fence)?;
        if record.tasks.contains(task) {
            return Ok(());
        }
        if record.tasks.len() >= MAX_RUN_TASKS {
            return Err(TickError::TooManyTasks { max: MAX_RUN_TASKS });
        }
        record.tasks.push(task.clone());
        Ok(())
    }

    /// Settle uncertain `run` with its reconciled outcome.
    pub(crate) fn recover(
        &mut self,
        run: RunId,
        report: PassReport,
        now: Timestamp,
    ) -> Result<(), TickError> {
        if report.backend_runs.len() > MAX_RUN_EVIDENCE {
            return Err(TickError::TooMuchEvidence {
                max: MAX_RUN_EVIDENCE,
            });
        }
        let record = self.get_mut(run)?;
        match record.state {
            RunState::Uncertain { .. } => {
                record.state = RunState::Recovered {
                    recovered_at: now,
                    outcome: report.outcome,
                    usage: report.usage,
                    backend_runs: report.backend_runs,
                };
                Ok(())
            }
            RunState::Running => Err(TickError::NotUncertain),
            RunState::Ended { .. } | RunState::Recovered { .. } => Err(TickError::AlreadyFinished),
        }
    }

    pub(crate) fn start(
        &mut self,
        pass: Pass,
        repeat: Repeat,
        holder: &HolderId,
        fence: Fence,
        now: Timestamp,
    ) -> RunId {
        self.retain(pass, repeat, now);
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

    /// Record the end of `run`. A repeated identical end is a no-op. A run
    /// that a later tick recorded as uncertain was superseded: its late end
    /// is refused, and a reconciled outcome stays.
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
        match &record.state {
            RunState::Running => {
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
            RunState::Uncertain { .. } | RunState::Recovered { .. } => Err(TickError::Superseded),
        }
    }

    /// Drop settled runs past [`RUN_RETENTION`], then the oldest settled
    /// runs beyond [`MAX_RUNS_PER_PASS`] per pass, leaving room for the run
    /// of `starting`. Running entries stay, and so do uncertain ones, except
    /// those of `starting` when it is idempotent: they oblige nothing.
    fn retain(&mut self, starting: Pass, repeat: Repeat, now: Timestamp) {
        let droppable_at = |run: &RunRecord| match run.state {
            RunState::Uncertain { reconciled_at }
                if run.pass == starting && repeat == Repeat::Idempotent =>
            {
                Some(reconciled_at)
            }
            RunState::Running
            | RunState::Uncertain { .. }
            | RunState::Ended { .. }
            | RunState::Recovered { .. } => run.settled_at(),
        };
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

    /// Bounds, unique ids below `next`, and at most one running entry per pass.
    pub(crate) fn validate(&self) -> bool {
        let bounded = self.runs.len() <= MAX_RUNS
            && self.runs.iter().all(|run| {
                run.id.0 < self.next
                    && run.tasks.len() <= MAX_RUN_TASKS
                    && match &run.state {
                        RunState::Ended { backend_runs, .. }
                        | RunState::Recovered { backend_runs, .. } => {
                            backend_runs.len() <= MAX_RUN_EVIDENCE
                        }
                        RunState::Running | RunState::Uncertain { .. } => true,
                    }
            });
        let ordered = self.runs.windows(2).all(|pair| match pair {
            [earlier, later] => earlier.id < later.id,
            _ => true,
        });
        let single_running = Pass::ALL.iter().all(|pass| {
            self.runs
                .iter()
                .filter(|run| run.pass == *pass && run.state == RunState::Running)
                .count()
                <= 1
        });
        bounded && ordered && single_running
    }
}
