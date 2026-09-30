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
//!   [`crate::state::RunState::Uncertain`]. A run is superseded once its
//!   lease lapses or [`MAX_PASS_RUNTIME`] passes: its late end, renewal, or
//!   task record is refused ([`TickError::Superseded`]) and changes nothing.
//! - An uncertain run blocks its pass ([`TickDecision::Blocked`]) until a
//!   person settles it with [`HouseStore::settle_run`], which records who,
//!   when, and why. No tick or trigger settles a run: a pass may act without
//!   recording a task first, so the unresolved effects of the tasks it
//!   recorded ([`HouseStore::record_run_task`]) are reported, never taken as
//!   proof that it did nothing.
//! - A pass that may outlast [`PASS_LEASE`] renews its run
//!   ([`HouseStore::renew_run`]); the lease never extends past
//!   [`MAX_PASS_RUNTIME`] after the run started.
//! - A backend's own run history is evidence linked from a run
//!   ([`PassReport::backend_runs`]), never a second ledger.
//! - [`trigger_plist`] and [`trigger_cron`] only render text for a person
//!   to install. Nothing here installs or changes a live schedule.

use std::{collections::BTreeMap, fmt, path::Path, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, ErrorClass, HolderId, HouseId, IdentifierError, TaskId,
    contracts::{Clock, ExternalRef, Fence, LeaseTtl, Timestamp},
    house::HouseConfig,
    scheduling::{IntervalMinutes, SchedulePolicy},
    state::{HouseStore, RunId, RunStart, TokenCounts},
};

/// How long a pass holds its tick lease without renewing it. A pass that
/// runs longer without [`HouseStore::renew_run`] loses the lease to the next
/// tick, which records the run as uncertain.
pub const PASS_LEASE: Duration = Duration::from_secs(60 * 60);

/// How long after its start a run may hold its pass lease, renew it, record
/// tasks, or record its end.
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
    /// No pass has that name.
    #[error("no such pass")]
    UnknownPass,
    /// No run with that id is in the ledger.
    #[error("no such run in the ledger")]
    UnknownRun,
    /// The run was started under another lease.
    #[error("the run belongs to another lease")]
    NotRunOwner,
    /// The run already recorded a different end.
    #[error("the run already recorded a different end")]
    AlreadyFinished,
    /// The run's pass lease lapsed, its runtime ran out, or a later tick
    /// recorded it as uncertain; it may no longer act or record an end.
    #[error("the run was superseded")]
    Superseded,
    /// Only an uncertain run can be settled.
    #[error("the run is not uncertain")]
    NotUncertain,
    /// Only a person present can settle an uncertain run.
    #[error("only a person can settle an uncertain run")]
    SettleNeedsPerson,
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
            | Self::UnknownPass
            | Self::TriggerPath
            | Self::TriggerInterval => ErrorClass::InvalidInput,
            Self::CrossHouse
            | Self::NoPasses
            | Self::UnknownRun
            | Self::NotRunOwner
            | Self::SettleNeedsPerson => ErrorClass::Refused,
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

impl FromStr for Pass {
    type Err = TickError;

    fn from_str(text: &str) -> Result<Self, TickError> {
        Self::ALL
            .into_iter()
            .find(|pass| pass.as_str() == text)
            .ok_or(TickError::UnknownPass)
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
    /// The build of Kitchen that recorded the run could not run the pass.
    /// Kept for runs recorded before the tick ran the scheduled passes.
    NotAvailable,
    /// The pass refused: missing authority, capability, or configuration.
    Refused,
    /// The pass failed while running.
    Execution,
    /// Another holder of the pass's workflow lease, such as `kitchn run`,
    /// was running it; this run did nothing.
    Busy,
    /// The pass's workflow lease expired without a release: an earlier
    /// pass may have died mid-way. The tick never takes over; a person
    /// checks, then runs the pass with `--take-over`.
    OwnerUncertain,
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
    /// The backend's own run references, linked as evidence. The tick keeps
    /// only the first [`MAX_RUN_EVIDENCE`].
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

impl PassRun {
    /// Record that the run is about to touch `task`, before the pass creates
    /// intent or effects for it.
    ///
    /// # Errors
    /// The errors of [`HouseStore::record_run_task`]; the pass stops.
    pub fn record_task(
        &self,
        store: &HouseStore,
        task: &TaskId,
        clock: &dyn Clock,
    ) -> crate::Result<()> {
        store.record_run_task(self.run, self.fence, task, clock.now())
    }

    /// Extend the run's lease by [`PASS_LEASE`], never past its runtime.
    ///
    /// # Errors
    /// The errors of [`HouseStore::renew_run`]; the pass stops.
    pub fn renew(&self, store: &HouseStore, clock: &dyn Clock) -> crate::Result<()> {
        store.renew_run(
            self.run,
            self.fence,
            LeaseTtl::new(PASS_LEASE)?,
            clock.now(),
        )?;
        Ok(())
    }
}

/// Runs one bounded workflow pass. The tick calls it only while it holds the
/// pass's lease; the pass takes its own workflow lease for its work.
///
/// Before touching a task, a pass records it with
/// [`HouseStore::record_run_task`], so a blocked run can report those
/// tasks' effects. A pass that may outlast [`PASS_LEASE`] renews its run
/// with [`HouseStore::renew_run`] and stops when that is refused.
pub trait PassRunner {
    /// Run `pass` once and report how it ended. Failures are outcomes, not
    /// errors, so the ledger always records them.
    fn run(&mut self, pass: Pass, run: &PassRun) -> PassReport;
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
    /// An uncertain run blocks the pass until a person settles it.
    Blocked {
        /// The uncertain run.
        run: RunId,
        /// Unresolved effects of the tasks it recorded. Zero does not mean
        /// the run did nothing.
        unresolved_effects: usize,
    },
    /// The pass outlasted its lease or runtime, so its end was refused. The
    /// next tick records the run as uncertain.
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
    /// Whether this tick recorded the blocking run as uncertain.
    pub newly_uncertain: bool,
}

/// The tick's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// One entry per scheduled pass, in pass order.
    pub passes: Vec<PassTick>,
}

impl TickReport {
    /// Whether no pass that ran failed, was superseded, or is blocked.
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self.passes.iter().any(|pass| {
            matches!(
                pass.decision,
                TickDecision::Ran {
                    outcome: PassOutcome::Failed { .. },
                    ..
                } | TickDecision::Blocked { .. }
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

/// One pass: run it when due and not blocked.
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
    let start = store.start_run(pass, schedule.every_minutes, holder, ttl, clock.now())?;
    let (decision, newly_uncertain) = match start {
        RunStart::Busy { holder, expires_at } => (TickDecision::Busy { holder, expires_at }, false),
        RunStart::NotDue { next_due } => (TickDecision::NotDue { next_due }, false),
        RunStart::Blocked {
            run,
            unresolved_effects,
            newly_uncertain,
        } => (
            TickDecision::Blocked {
                run,
                unresolved_effects,
            },
            newly_uncertain,
        ),
        RunStart::Started { run, fence } => {
            let context = PassRun {
                house: config.house.clone(),
                run,
                fence,
            };
            let mut ended = runner.run(pass, &context);
            // The ledger links at most MAX_RUN_EVIDENCE backend runs; more
            // must never cost the recorded outcome or abort the tick.
            ended.backend_runs.truncate(MAX_RUN_EVIDENCE);
            let outcome = ended.outcome;
            let decision = match store.finish_run(run, fence, ended, clock.now()) {
                Ok(()) => TickDecision::Ran { run, outcome },
                Err(crate::Error::Tick(TickError::Superseded)) => TickDecision::Superseded { run },
                Err(error) => return Err(error),
            };
            (decision, false)
        }
    };
    Ok(PassTick {
        pass,
        decision,
        newly_uncertain,
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
///
/// # Errors
/// [`TickError::TriggerInterval`] when the interval does not divide 60: cron
/// restarts its minute step each hour, so `*/7` would not fire every seven
/// minutes. launchd's `StartInterval` has no such limit.
pub fn trigger_cron(target: &TriggerTarget) -> Result<String, TickError> {
    if 60 % target.every.0 != 0 {
        return Err(TickError::TriggerInterval);
    }
    let command: Vec<String> = target.argv().iter().map(|arg| shell_quote(arg)).collect();
    Ok(format!(
        "*/{} * * * * {}",
        target.every.0,
        command.join(" ")
    ))
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
