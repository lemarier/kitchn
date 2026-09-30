//! `kitchn tick`: the one command every trigger runs for a house.
//!
//! Without a subcommand it runs every due pass the house configures and
//! records each run in the house store's run ledger. `runs` lists the ledger;
//! `trigger` prints a launchd plist or crontab line that runs the tick and
//! installs nothing. Due decisions, leases, and the ledger stay in
//! [`kitchen::workflows::tick`].

use std::{fmt::Write as _, path::PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    HolderId, HouseId,
    adoption::HouseRegistry,
    contracts::{Clock, SystemClock},
    house::HouseError,
    state::{HouseStore, RunState, StoreOptions},
    workflows::tick::{
        self, Pass, PassFailure, PassOutcome, PassReport, PassRun, PassRunner, TickDecision,
        TriggerMinutes, TriggerTarget, trigger_cron, trigger_plist,
    },
};

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct TickArgs {
    #[command(subcommand)]
    command: Option<TickCommand>,
    #[command(flatten)]
    house: Option<HouseScope>,
}

#[derive(Subcommand)]
enum TickCommand {
    /// List the house's run ledger, oldest first. Reads only.
    Runs {
        #[command(flatten)]
        house: HouseScope,
    },
    /// Print a launchd plist or crontab line that runs the tick. Installs
    /// nothing.
    Trigger {
        #[arg(value_enum)]
        format: TriggerFormat,
        /// Absolute path of the kitchn executable the trigger runs.
        #[arg(long)]
        kitchn: PathBuf,
        /// Absolute path of the house registry.
        #[arg(long)]
        registry: PathBuf,
        /// The house the trigger ticks.
        #[arg(long)]
        house: HouseId,
        /// Minutes between ticks, 1 to 59. Each pass still runs only when due.
        #[arg(long, default_value_t = 5)]
        every_minutes: u8,
    },
}

#[derive(Args)]
struct HouseScope {
    /// The house whose passes run.
    #[arg(long, required = true)]
    house: Option<HouseId>,
    /// The house registry, which holds the house's tick passes.
    #[arg(long, required = true)]
    registry: Option<PathBuf>,
    /// Absolute path of the house's state store (default: the one `house
    /// init` created in --registry).
    #[arg(long)]
    store: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum TriggerFormat {
    Launchd,
    Cron,
}

/// Stands in for the scheduled pass commands of #225 until they land: every
/// pass reports [`PassFailure::NotAvailable`], so the ledger shows the tick
/// ran and nothing was done, and the tick exits 1.
struct PendingPasses;

impl PassRunner for PendingPasses {
    fn run(&mut self, _pass: Pass, _run: &PassRun) -> PassReport {
        PassReport::new(PassOutcome::Failed {
            reason: PassFailure::NotAvailable,
        })
    }
}

pub fn run(args: TickArgs) -> Result<(String, bool), kitchen::Error> {
    match (args.command, args.house) {
        (Some(TickCommand::Runs { house }), _) => runs(house),
        (
            Some(TickCommand::Trigger {
                format,
                kitchn,
                registry,
                house,
                every_minutes,
            }),
            _,
        ) => {
            let target = TriggerTarget::new(
                &kitchn,
                &registry,
                house,
                TriggerMinutes::new(every_minutes)?,
            )?;
            let text = match format {
                TriggerFormat::Launchd => trigger_plist(&target),
                TriggerFormat::Cron => trigger_cron(&target),
            };
            Ok((text.trim_end().to_owned(), true))
        }
        (None, Some(house)) => run_tick(house),
        (None, None) => Err(HouseError::InvalidInput.into()),
    }
}

fn run_tick(scope: HouseScope) -> Result<(String, bool), kitchen::Error> {
    let (store, registry, house) = scope.open()?;
    let config = registry.load(&house)?;
    let clock = SystemClock;
    let holder = HolderId::new(&format!(
        "tick-{}-{}",
        std::process::id(),
        clock.now().as_unix_millis()
    ))?;
    let report = tick::tick(&store, &config, &holder, &mut PendingPasses, &clock)?;
    let mut text = String::new();
    for pass in &report.passes {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = match &pass.decision {
            TickDecision::Ran { run, outcome } => {
                write!(text, "{}: {run}: {}", pass.pass, outcome_text(*outcome))
            }
            TickDecision::NotDue { next_due } => write!(
                text,
                "{}: not due until {}",
                pass.pass,
                next_due.as_unix_millis()
            ),
            TickDecision::Busy { holder, expires_at } => write!(
                text,
                "{}: busy: {} holds it until {}",
                pass.pass,
                holder.as_str(),
                expires_at.as_unix_millis()
            ),
        };
        if let Some(run) = pass.reconciled {
            let _ = write!(text, " ({run} recorded as uncertain)");
        }
    }
    Ok((text, report.healthy()))
}

fn runs(scope: HouseScope) -> Result<(String, bool), kitchen::Error> {
    let (store, _, _) = scope.open()?;
    let runs = store.runs()?;
    if runs.is_empty() {
        return Ok(("no runs".to_owned(), true));
    }
    let mut text = String::new();
    for run in &runs {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = write!(
            text,
            "{} {} started {}: ",
            run.id,
            run.pass,
            run.started_at.as_unix_millis()
        );
        let _ = match &run.state {
            RunState::Running => write!(text, "running"),
            RunState::Ended {
                ended_at,
                outcome,
                backend_runs,
                ..
            } => write!(
                text,
                "{} at {} ({} backend runs linked)",
                outcome_text(*outcome),
                ended_at.as_unix_millis(),
                backend_runs.len()
            ),
            RunState::Uncertain { reconciled_at } => write!(
                text,
                "uncertain, recorded at {}",
                reconciled_at.as_unix_millis()
            ),
        };
    }
    Ok((text, true))
}

const fn outcome_text(outcome: PassOutcome) -> &'static str {
    match outcome {
        PassOutcome::Done => "done",
        PassOutcome::Idle => "idle",
        PassOutcome::Failed { reason } => match reason {
            PassFailure::NotAvailable => "failed: pass not available in this build",
            PassFailure::Refused => "failed: refused",
            PassFailure::Execution => "failed",
        },
    }
}

impl HouseScope {
    fn open(self) -> Result<(HouseStore, HouseRegistry, HouseId), kitchen::Error> {
        let (Some(house), Some(registry)) = (self.house, self.registry) else {
            return Err(HouseError::InvalidInput.into());
        };
        let registry = super::house::canonical_root(registry)?;
        let store = super::house::store_or_default(self.store, Some(&registry), &house)?;
        if !store.is_absolute() {
            return Err(HouseError::InvalidInput.into());
        }
        let opened = HouseStore::open(&store, house.clone(), StoreOptions::default())?;
        Ok((opened, HouseRegistry::new(registry)?, house))
    }
}
