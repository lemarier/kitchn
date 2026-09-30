//! The run ledger: one entry per tick pass run, kept in the house store.
//!
//! The ledger is the source of truth for when a pass last ran and how it
//! ended; [`crate::workflows::tick`] owns the rules. It is bounded: retention
//! drops ended runs older than [`RUN_RETENTION`] and keeps at most
//! [`MAX_RUNS_PER_PASS`] ended runs per pass. A running entry is never
//! dropped, and the pass lease allows at most one per pass.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    HolderId,
    contracts::{ExternalRef, Fence, Timestamp},
    workflows::tick::{MAX_RUN_EVIDENCE, Pass, PassOutcome, PassReport, RunUsage, TickError},
};

/// Ended runs the ledger keeps per pass.
pub const MAX_RUNS_PER_PASS: usize = 64;
/// How long the ledger keeps an ended run.
pub const RUN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Entries the ledger holds at most: every pass's ended runs and one
/// running entry each.
const MAX_RUNS: usize = Pass::ALL.len() * (MAX_RUNS_PER_PASS + 1);

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
    /// finished its work is unknown.
    Uncertain {
        /// When a later tick recorded it.
        reconciled_at: Timestamp,
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
    /// Where it stands.
    pub state: RunState,
}

impl RunRecord {
    /// When the run left the running state, if it did.
    const fn settled_at(&self) -> Option<Timestamp> {
        match &self.state {
            RunState::Running => None,
            RunState::Ended { ended_at, .. } => Some(*ended_at),
            RunState::Uncertain { reconciled_at } => Some(*reconciled_at),
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
        reconciled: Option<RunId>,
    },
    /// The pass is not due.
    NotDue {
        /// When it is due.
        next_due: Timestamp,
        /// An earlier run recorded as uncertain first.
        reconciled: Option<RunId>,
    },
    /// Another tick holds the pass lease.
    Busy {
        /// The holder.
        holder: HolderId,
        /// When the lease expires.
        expires_at: Timestamp,
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
    pub(crate) fn reconcile(&mut self, pass: Pass, now: Timestamp) -> Option<RunId> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.pass == pass && run.state == RunState::Running)?;
        run.state = RunState::Uncertain { reconciled_at: now };
        Some(run.id)
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
            state: RunState::Running,
        });
        id
    }

    /// Record the end of `run`. A repeated identical end is a no-op; a late
    /// end replaces an uncertain state with the run's outcome.
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
        let record = self
            .runs
            .iter_mut()
            .find(|record| record.id == run)
            .ok_or(TickError::UnknownRun)?;
        if record.fence != fence {
            return Err(TickError::NotRunOwner);
        }
        match &record.state {
            RunState::Running | RunState::Uncertain { .. } => {
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
        }
    }

    /// Drop ended runs past [`RUN_RETENTION`], then the oldest ended runs
    /// beyond [`MAX_RUNS_PER_PASS`] per pass, leaving room for the run of
    /// `starting`. Running entries stay.
    fn retain(&mut self, starting: Pass, now: Timestamp) {
        self.runs.retain(|run| {
            run.settled_at()
                .is_none_or(|at| now.saturating_since(at) < RUN_RETENTION)
        });
        for pass in Pass::ALL {
            let ended = self
                .runs
                .iter()
                .filter(|run| run.pass == pass && run.settled_at().is_some())
                .count();
            let keep = if pass == starting {
                MAX_RUNS_PER_PASS.saturating_sub(1)
            } else {
                MAX_RUNS_PER_PASS
            };
            let mut excess = ended.saturating_sub(keep);
            // Runs are appended in start order, so the first ones are oldest.
            self.runs.retain(|run| {
                if excess > 0 && run.pass == pass && run.settled_at().is_some() {
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
                    && match &run.state {
                        RunState::Ended { backend_runs, .. } => {
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
