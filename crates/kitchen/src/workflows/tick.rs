//! The house tick: one Kitchen entry point that any trigger invokes.
//!
//! Kitchen owns the schedule definitions ([`TickPolicy`], in the house
//! configuration) and the run ledger ([`crate::state::RunRecord`], in the
//! house store). launchd, cron, GitHub Actions, or a backend schedule only
//! start `kitchn tick`; they decide nothing and keep no state Kitchen reads.
//!
//! Rules:
//!
//! - Each pass has its own consumer lease in the house store. The due check,
//!   the lease, and the ledger entry change in one store transaction, so a
//!   second trigger that fires while a pass runs finds the lease live and
//!   leaves the pass alone ([`TickDecision::Busy`]).
//! - A pass is due when it never ran or its last run started at least its
//!   interval ago. Failed and uncertain runs count as runs.
//! - A run whose lease expired before it recorded an end is uncertain, not
//!   failed: the next tick takes the lease over and records the run as
//!   [`crate::state::RunState::Uncertain`]. The run is superseded: its late
//!   end, renewal, or task record is refused ([`TickError::Superseded`]).
//! - An uncertain run blocks its pass until it is reconciled, unless the
//!   runner declares the pass [`Repeat::Idempotent`]. The tick asks the
//!   runner to reconcile it ([`PassRunner::reconcile`]) under the pass
//!   lease; the store records it as recovered only when the runner
//!   establishes the outcome and no task the run recorded
//!   ([`HouseStore::record_run_task`]) has an unresolved effect. Otherwise
//!   the pass waits and the tick reports [`TickDecision::NeedsAttention`].
//! - A pass that may outlast [`PASS_LEASE`] renews its run
//!   ([`HouseStore::renew_run`]), for at most [`MAX_PASS_RUNTIME`].
//! - A backend's own run history is evidence linked from a run
//!   ([`PassReport::backend_runs`]), never a second ledger.
//! - [`trigger_plist`] and [`trigger_cron`] only render text for a person
//!   to install. Nothing here installs or changes a live schedule.

use std::{collections::BTreeMap, fmt, path::Path, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, ErrorClass, HolderId, HouseId, IdentifierError,
    contracts::{Clock, ExternalRef, Fence, LeaseTtl, Timestamp},
    house::HouseConfig,
    scheduling::{IntervalMinutes, SchedulePolicy},
    state::{HouseStore, RunId, RunRecord, RunSettle, RunStart, TokenCounts},
};

/// How long a pass holds its tick lease without renewing it. A pass that
/// runs longer without [`HouseStore::renew_run`] loses the lease to the next
/// tick, which records the run as uncertain.
pub const PASS_LEASE: Duration = Duration::from_secs(60 * 60);

/// How long after its start a run may still renew its lease or record tasks.
pub const MAX_PASS_RUNTIME: Duration = Duration::from_secs(6 * 60 * 60);

/// Backend run references one run links at most.
pub const MAX_RUN_EVIDENCE: usize = 8;

/// Tasks one run records at most.
pub const MAX_RUN_TASKS: usize = 64;

/// A tick was refused. Input text is never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TickError {
    /// The house configuration and the store belong to different houses.
    #[error("the house configuration and store belong to different houses")]
    CrossHouse,
    /// The house schedules no tick passes.
    #[error("the house schedules no tick passes")]
    NoPasses,
    /// No run with that id is in the ledger.
    #[error("no such run in the ledger")]
    UnknownRun,
    /// The run was started under another lease.
    #[error("the run belongs to another lease")]
    NotRunOwner,
    /// The run already recorded a different end.
    #[error("the run already recorded a different end")]
    AlreadyFinished,
    /// A later tick recorded the run as uncertain, or its pass lease lapsed;
    /// it may no longer act or record an end.
    #[error("a later tick superseded the run")]
    Superseded,
    /// Only an uncertain run can be reconciled.
    #[error("the run is not uncertain")]
    NotUncertain,
    /// The run started more than [`MAX_PASS_RUNTIME`] ago.
    #[error("the run exceeded its maximum runtime")]
    RunTooLong,
    /// A run records at most this many tasks.
    #[error("a run records at most {max} tasks")]
    TooManyTasks {
        /// Most accepted tasks.
        max: usize,
    },
    /// A run links more backend runs than the ledger keeps.
    #[error("a run links at most {max} backend runs")]
    TooMuchEvidence {
        /// Most accepted references.
        max: usize,
    },
    /// Trigger paths must be absolute UTF-8 without control characters.
    #[error("trigger paths must be absolute UTF-8 without control characters")]
    TriggerPath,
    /// A trigger fires every 1 to 59 minutes.
    #[error("a trigger fires every 1 to 59 minutes")]
    TriggerInterval,
}

impl TickError {
    /// Broad handling class.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::TooMuchEvidence { .. }
            | Self::TooManyTasks { .. }
            | Self::TriggerPath
            | Self::TriggerInterval => ErrorClass::InvalidInput,
            Self::CrossHouse
            | Self::NoPasses
            | Self::UnknownRun
            | Self::NotRunOwner
            | Self::RunTooLong => ErrorClass::Refused,
            Self::AlreadyFinished | Self::Superseded | Self::NotUncertain => ErrorClass::Conflict,
        }
    }
}

/// A workflow pass the tick can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pass {
    /// Issue pickup.
    Pickup,
    /// Supervised coordination of running workers.
    Coordinate,
    /// Pull request repair.
    Repair,
    /// The exact-head merge gate.
    Gate,
}

impl Pass {
    /// Every pass.
    pub const ALL: [Self; 4] = [Self::Pickup, Self::Coordinate, Self::Repair, Self::Gate];

    /// The stable lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pickup => "pickup",
            Self::Coordinate => "coordinate",
            Self::Repair => "repair",
            Self::Gate => "gate",
        }
    }

    /// The consumer scope of this pass's tick lease.
    pub(crate) fn consumer(self) -> Result<ConsumerId, IdentifierError> {
        ConsumerId::new(match self {
            Self::Pickup => "tick-pickup",
            Self::Coordinate => "tick-coordinate",
            Self::Repair => "tick-repair",
            Self::Gate => "tick-gate",
        })
    }
}

impl fmt::Display for Pass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// When one pass is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PassSchedule {
    /// Minimum time between the starts of two runs.
    pub every_minutes: IntervalMinutes,
}

/// The passes a house's tick runs. Stored in house configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TickPolicy {
    /// Scheduled passes. A pass not listed never runs from the tick.
    pub passes: BTreeMap<Pass, PassSchedule>,
}

impl TickPolicy {
    /// Whether every interval keeps the house's schedule minimum.
    #[must_use]
    pub fn keeps(&self, schedules: Option<&SchedulePolicy>) -> bool {
        schedules.is_none_or(|policy| {
            self.passes
                .values()
                .all(|pass| pass.every_minutes >= policy.min_interval_minutes)
        })
    }
}

/// How a pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PassOutcome {
    /// The pass did its work.
    Done,
    /// There was nothing to do.
    Idle,
    /// The pass did not complete.
    Failed {
        /// Why.
        reason: PassFailure,
    },
}

/// Why a pass did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PassFailure {
    /// This build of Kitchen cannot run the pass.
    NotAvailable,
    /// The pass refused: missing authority, capability, or configuration.
    Refused,
    /// The pass failed while running.
    Execution,
}

/// A run's usage, as far as the pass knows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RunUsage {
    /// The pass reported no usage. Its cost is unknown, not zero.
    #[default]
    NotReported,
    /// Tokens the pass's workers reported.
    Reported {
        /// Tokens by kind.
        tokens: TokenCounts,
    },
}

/// What a pass returns to the tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// How it ended.
    pub outcome: PassOutcome,
    /// Usage where known.
    pub usage: RunUsage,
    /// The backend's own run references, linked as evidence, at most
    /// [`MAX_RUN_EVIDENCE`].
    pub backend_runs: Vec<ExternalRef>,
}

impl PassReport {
    /// A report with no usage and no backend evidence.
    #[must_use]
    pub const fn new(outcome: PassOutcome) -> Self {
        Self {
            outcome,
            usage: RunUsage::NotReported,
            backend_runs: Vec::new(),
        }
    }
}

/// The run a pass executes under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassRun {
    /// The house.
    pub house: HouseId,
    /// The ledger entry.
    pub run: RunId,
    /// The tick lease fence the run holds.
    pub fence: Fence,
}

/// Whether a pass may run again while an earlier run of it is uncertain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Repeat {
    /// An uncertain run blocks the pass until it is reconciled.
    #[default]
    AfterReconcile,
    /// Repeating the pass cannot repeat an external effect, so an uncertain
    /// run blocks nothing. Declare it only with that proof.
    Idempotent,
}

/// What a runner established about an uncertain run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// How the run ended. The store still refuses it while a task the run
    /// recorded has an unresolved effect.
    Ended(PassReport),
    /// The outcome is not established; the pass keeps waiting.
    Unknown,
}

/// An uncertain run to reconcile, under the pass lease the tick holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassRecovery {
    /// The house.
    pub house: HouseId,
    /// The uncertain run, including the tasks it recorded.
    pub record: RunRecord,
    /// The tick's current pass lease fence.
    pub fence: Fence,
}

/// Runs one bounded workflow pass. The tick calls it only while it holds the
/// pass's lease; the pass takes its own workflow lease for its work.
///
/// Before touching a task, a pass records it with
/// [`HouseStore::record_run_task`]; after a crash, reconciliation checks
/// those tasks' effects. A pass that may outlast [`PASS_LEASE`] renews its
/// run with [`HouseStore::renew_run`] and stops when that is refused.
pub trait PassRunner {
    /// Run `pass` once and report how it ended. Failures are outcomes, not
    /// errors, so the ledger always records them.
    fn run(&mut self, pass: Pass, run: &PassRun) -> PassReport;

    /// Reconcile an uncertain run of `pass`: resolve the effects of the
    /// tasks it recorded through the store's durable intent (take over each
    /// task's claim and call [`crate::state::reconcile`]) and establish how
    /// the run ended. Never repeat the run's work here.
    fn reconcile(&mut self, pass: Pass, uncertain: &PassRecovery) -> Recovery;

    /// Whether `pass` may run again while an earlier run is uncertain.
    fn repeat(&self, _pass: Pass) -> Repeat {
        Repeat::AfterReconcile
    }
}

/// What the tick decided for one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickDecision {
    /// The pass ran.
    Ran {
        /// Its ledger entry.
        run: RunId,
        /// How it ended.
        outcome: PassOutcome,
    },
    /// The pass is not due.
    NotDue {
        /// When it is due next.
        next_due: Timestamp,
    },
    /// An uncertain run is not reconciled, so the pass did not run. A
    /// person or a later tick has to settle the run's effects first.
    NeedsAttention {
        /// The uncertain run.
        run: RunId,
        /// Unresolved effects of the tasks it recorded.
        unresolved_effects: usize,
    },
    /// The pass ran past its lease and a later tick recorded the run as
    /// uncertain, so its end was refused. The run waits for reconciliation.
    Superseded {
        /// The superseded run.
        run: RunId,
    },
    /// Another tick holds the pass's lease.
    Busy {
        /// The holder.
        holder: HolderId,
        /// When its lease expires.
        expires_at: Timestamp,
    },
}

/// The tick's result for one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassTick {
    /// The pass.
    pub pass: Pass,
    /// What happened.
    pub decision: TickDecision,
    /// An earlier run this tick recorded as uncertain.
    pub uncertain: Option<RunId>,
    /// Uncertain runs this tick reconciled, oldest first.
    pub recovered: Vec<RunId>,
}

/// The tick's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// One entry per scheduled pass, in pass order.
    pub passes: Vec<PassTick>,
}

impl TickReport {
    /// Whether no pass that ran failed and none waits on reconciliation.
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self.passes.iter().any(|pass| {
            matches!(
                pass.decision,
                TickDecision::Ran {
                    outcome: PassOutcome::Failed { .. },
                    ..
                } | TickDecision::NeedsAttention { .. }
                    | TickDecision::Superseded { .. }
            )
        })
    }
}

/// Run every due pass of `config`'s tick once, in pass order.
///
/// A store error while recording a run's end leaves the run open; the next
/// tick after its lease expires records it as uncertain.
///
/// # Errors
/// [`TickError::CrossHouse`] when `config` and `store` belong to different
/// houses, [`TickError::NoPasses`] when the house schedules none, and store
/// errors.
pub fn tick(
    store: &HouseStore,
    config: &HouseConfig,
    holder: &HolderId,
    runner: &mut dyn PassRunner,
    clock: &dyn Clock,
) -> crate::Result<TickReport> {
    if store.house() != &config.house {
        return Err(TickError::CrossHouse.into());
    }
    let passes = config
        .tick
        .as_ref()
        .map(|policy| &policy.passes)
        .filter(|passes| !passes.is_empty())
        .ok_or(TickError::NoPasses)?;
    let ttl = LeaseTtl::new(PASS_LEASE)?;
    let mut report = TickReport {
        passes: Vec::with_capacity(passes.len()),
    };
    for (&pass, schedule) in passes {
        report.passes.push(tick_pass(
            store, config, pass, schedule, holder, ttl, runner, clock,
        )?);
    }
    Ok(report)
}

/// One pass: reconcile its uncertain runs, then run it when due.
#[expect(
    clippy::too_many_arguments,
    reason = "the tick's inputs, passed through once"
)]
fn tick_pass(
    store: &HouseStore,
    config: &HouseConfig,
    pass: Pass,
    schedule: &PassSchedule,
    holder: &HolderId,
    ttl: LeaseTtl,
    runner: &mut dyn PassRunner,
    clock: &dyn Clock,
) -> crate::Result<PassTick> {
    let repeat = runner.repeat(pass);
    let mut first_uncertain = None;
    let mut recovered = Vec::new();
    // Each round recovers one uncertain run or stops, and the ledger holds
    // a bounded number of them, so this ends.
    let decision = loop {
        let start = store.start_run(
            pass,
            schedule.every_minutes,
            repeat,
            holder,
            ttl,
            clock.now(),
        )?;
        match start {
            RunStart::Busy { holder, expires_at } => {
                break TickDecision::Busy { holder, expires_at };
            }
            RunStart::NotDue {
                next_due,
                uncertain,
            } => {
                first_uncertain = first_uncertain.or(uncertain);
                break TickDecision::NotDue { next_due };
            }
            RunStart::Reconcile {
                record,
                fence,
                uncertain,
            } => {
                first_uncertain = first_uncertain.or(uncertain);
                let run = record.id;
                let context = PassRecovery {
                    house: config.house.clone(),
                    record,
                    fence,
                };
                let recovery = runner.reconcile(pass, &context);
                match store.settle_run(run, fence, recovery, clock.now())? {
                    RunSettle::Recovered => recovered.push(run),
                    RunSettle::Blocked { unresolved_effects } => {
                        break TickDecision::NeedsAttention {
                            run,
                            unresolved_effects,
                        };
                    }
                }
            }
            RunStart::Started {
                run,
                fence,
                uncertain,
            } => {
                first_uncertain = first_uncertain.or(uncertain);
                let context = PassRun {
                    house: config.house.clone(),
                    run,
                    fence,
                };
                let ended = runner.run(pass, &context);
                let outcome = ended.outcome;
                match store.finish_run(run, fence, ended, clock.now()) {
                    Ok(()) => break TickDecision::Ran { run, outcome },
                    Err(crate::Error::Tick(TickError::Superseded)) => {
                        break TickDecision::Superseded { run };
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    };
    Ok(PassTick {
        pass,
        decision,
        uncertain: first_uncertain,
        recovered,
    })
}

/// Minutes between trigger firings, 1 to 59.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerMinutes(u8);

impl TriggerMinutes {
    /// Validate an interval.
    ///
    /// # Errors
    /// [`TickError::TriggerInterval`] outside 1 to 59.
    pub const fn new(minutes: u8) -> Result<Self, TickError> {
        if minutes == 0 || minutes > 59 {
            return Err(TickError::TriggerInterval);
        }
        Ok(Self(minutes))
    }
}

/// What a printed trigger runs: `kitchn tick --registry <dir> --house <id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerTarget {
    kitchn: String,
    registry: String,
    house: HouseId,
    every: TriggerMinutes,
}

impl TriggerTarget {
    /// Validate the paths a trigger names.
    ///
    /// # Errors
    /// [`TickError::TriggerPath`] for a relative, non-UTF-8, or
    /// control-character path.
    pub fn new(
        kitchn: &Path,
        registry: &Path,
        house: HouseId,
        every: TriggerMinutes,
    ) -> Result<Self, TickError> {
        Ok(Self {
            kitchn: trigger_path(kitchn)?,
            registry: trigger_path(registry)?,
            house,
            every,
        })
    }

    fn argv(&self) -> [&str; 6] {
        [
            &self.kitchn,
            "tick",
            "--registry",
            &self.registry,
            "--house",
            self.house.as_str(),
        ]
    }
}

fn trigger_path(path: &Path) -> Result<String, TickError> {
    let text = path.to_str().ok_or(TickError::TriggerPath)?;
    if !path.is_absolute() || text.chars().any(char::is_control) {
        return Err(TickError::TriggerPath);
    }
    Ok(text.to_owned())
}

/// A launchd agent plist that runs the tick every interval. Printed for a
/// person to install; Kitchen never loads it.
#[must_use]
pub fn trigger_plist(target: &TriggerTarget) -> String {
    let arguments: String = target
        .argv()
        .iter()
        .map(|arg| format!("    <string>{}</string>\n", xml_escape(arg)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.getkitchn.tick.{house}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>StartInterval</key>
  <integer>{seconds}</integer>
  <key>RunAtLoad</key>
  <false/>
</dict>
</plist>
"#,
        house = target.house.as_str(),
        seconds = u32::from(target.every.0) * 60,
    )
}

/// A crontab line that runs the tick every interval. Printed for a person
/// to install; Kitchen never edits a crontab.
#[must_use]
pub fn trigger_cron(target: &TriggerTarget) -> String {
    let command: Vec<String> = target.argv().iter().map(|arg| shell_quote(arg)).collect();
    format!("*/{} * * * * {}", target.every.0, command.join(" "))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Single-quote for `sh`, and escape `%`, which cron turns into a newline.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''").replace('%', r"\%"))
}
