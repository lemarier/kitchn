//! `kitchn tick`: the one command every trigger runs for a house.
//!
//! Without a subcommand it runs every due pass the house configures, in
//! process through the same scheduled passes as `kitchn run`, and records
//! each run in the house store's run ledger. The forge and worker backend
//! are resolved only for a pass that is due. `runs` lists the ledger;
//! `settle` records that a person settled an uncertain run; `trigger` prints
//! a launchd plist or crontab line that runs the tick and installs nothing;
//! `configure` stores the house's backend and pickup settings.
//! Due decisions, leases, and the ledger stay in
//! [`kitchen::workflows::tick`]; the passes in [`kitchen::workflows::run`].

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    HolderId, HouseId,
    adoption::HouseRegistry,
    contracts::{Capability, Claimant, Clock, Repository, SystemClock, Text},
    house::{
        HouseError, OrcaHost, RUNTIME_SCHEMA, RuntimeConfig, RuntimeOutcome, forge_binding,
        runtime_config, store_runtime,
    },
    state::{HouseStore, RunId, RunSettle, RunState, StoreOptions},
    workflows::{
        coordination::MailboxRoute,
        run::{TickPasses, failed_report},
        tick::{
            self, Pass, PassFailure, PassOutcome, PassReport, PassRun, PassRunner, TickDecision,
            TriggerMinutes, TriggerTarget, trigger_cron, trigger_plist,
        },
    },
};

use super::run::{BackendArgs, Opened, PickupArgs};

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct TickArgs {
    #[command(subcommand)]
    command: Option<TickCommand>,
    #[command(flatten)]
    house: Option<HouseScope>,
    /// The repository pickup, repair, and the gate serve, as `owner/name`
    /// (default: the house's only one).
    #[arg(long)]
    repository: Option<Repository>,
    #[command(flatten)]
    backend: BackendArgs,
    #[command(flatten)]
    pickup: PickupArgs,
}

#[derive(Subcommand)]
enum TickCommand {
    /// List the house's run ledger, oldest first. Reads only.
    Runs {
        #[command(flatten)]
        house: HouseScope,
    },
    /// Let a pass run again after an uncertain run blocked it. Check what
    /// the run did first: it may have acted without recording a task.
    /// Records who settled it, when, and why. A scheduled run cannot do it.
    Settle {
        #[command(flatten)]
        house: HouseScope,
        /// The blocked pass.
        #[arg(long)]
        pass: Pass,
        /// The uncertain run's number, as `kitchn tick` reported it.
        #[arg(long)]
        run: RunId,
        /// Why the pass may run again, such as what you checked.
        #[arg(long)]
        reason: String,
        /// Your session, recorded as who settled the run.
        #[arg(long)]
        holder: HolderId,
    },
    /// Print a launchd plist or crontab line that runs the tick. Installs
    /// nothing and stores nothing: the line names only the registry and the
    /// house, and every tick reads its backend and pickup settings from the
    /// runtime configuration `tick configure` stores.
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
    /// Store the house's backend, repository, and pickup settings, owner-only,
    /// in its private runtime configuration in the registry. Every tick and
    /// `kitchn run` reads them there. This is the only command that changes
    /// what is stored: a tick or run flag that disagrees with it is refused.
    /// Flags given here overlay what is already stored; nothing is stored
    /// unless the whole configuration validates.
    Configure {
        /// Absolute path of the house registry.
        #[arg(long)]
        registry: PathBuf,
        /// The house the settings belong to.
        #[arg(long)]
        house: HouseId,
        /// The repository a multi-repository house's passes serve, as
        /// `owner/name`.
        #[arg(long)]
        repository: Option<Repository>,
        #[command(flatten)]
        backend: BackendArgs,
        /// Scheduled pickup settings.
        #[command(flatten)]
        pickup: Box<PickupArgs>,
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

/// Runs each due pass through [`TickPasses`], resolving first what that pass
/// needs of the house's forge, worker backend, and pickup settings, as
/// `kitchn run` does. A pass that cannot start records a failed run;
/// `errors` keeps why.
struct DuePasses<'a> {
    opened: &'a Opened,
    backend: &'a BackendArgs,
    pickup: &'a PickupArgs,
    clock: &'a SystemClock,
    errors: BTreeMap<Pass, kitchen::Error>,
}

impl DuePasses<'_> {
    fn run_pass(&self, pass: Pass, run: &PassRun) -> Result<PassReport, kitchen::Error> {
        let opened = self.opened;
        let settings = match pass {
            Pass::Pickup => Some(super::run::settings(opened, self.pickup)?),
            Pass::Coordinate | Pass::Repair | Pass::Gate => None,
        };
        let backend = match pass {
            Pass::Pickup | Pass::Coordinate => Some(
                opened.backend(
                    self.backend,
                    pass.as_str(),
                    settings
                        .as_ref()
                        .map(|settings| settings.branch_prefix.clone()),
                    MailboxRoute::House.worker_requirements(),
                )?,
            ),
            Pass::Repair => Some(opened.backend(
                self.backend,
                pass.as_str(),
                None,
                &[Capability::WorkerStatusAndOutcome],
            )?),
            Pass::Gate => None,
        };
        let forge = opened.forge()?;
        let authors = [forge_binding(&opened.registry, &opened.config.house)?
            .requester
            .to_string()];
        TickPasses {
            store: &opened.store,
            house: &opened.config,
            backend: backend.as_deref(),
            forge: &forge,
            clock: self.clock,
            repository: &opened.repository,
            pickup: settings.as_ref(),
            authors: &authors,
        }
        .run_pass(pass, run)
    }
}

impl PassRunner for DuePasses<'_> {
    fn run(&mut self, pass: Pass, run: &PassRun) -> PassReport {
        self.run_pass(pass, run).unwrap_or_else(|error| {
            let report = failed_report(&error);
            self.errors.insert(pass, error);
            report
        })
    }
}

pub fn run(args: TickArgs) -> Result<(String, bool), kitchen::Error> {
    match (args.command, args.house) {
        (Some(TickCommand::Runs { house }), _) => runs(house),
        (
            Some(TickCommand::Settle {
                house,
                pass,
                run,
                reason,
                holder,
            }),
            _,
        ) => settle(house, pass, run, &Text::new(&reason)?, holder),
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
                TriggerFormat::Cron => trigger_cron(&target)?,
            };
            Ok((text.trim_end().to_owned(), true))
        }
        (
            Some(TickCommand::Configure {
                registry,
                house,
                repository,
                backend,
                pickup,
            }),
            _,
        ) => configure(&registry, &house, repository, &backend, &pickup),
        (None, Some(house)) => run_tick(house, args.repository, &args.backend, &args.pickup),
        (None, None) => Err(HouseError::InvalidInput.into()),
    }
}

/// Store the backend, repository, and pickup flags, overlaid on what the
/// house already stores. Nothing is written when none was given or the
/// whole configuration does not validate.
fn configure(
    registry: &Path,
    house: &HouseId,
    repository: Option<Repository>,
    flags: &BackendArgs,
    pickup: &PickupArgs,
) -> Result<(String, bool), kitchen::Error> {
    let orca_given = [
        flags.orca.is_some(),
        flags.runtime_dir.is_some(),
        flags.orca_run.is_some(),
        flags.orca_coordinator.is_some(),
        flags.orca_repo.is_some(),
    ];
    if repository.is_none() && flags.curl.is_none() && !pickup.any() && !orca_given.contains(&true)
    {
        return Err(HouseError::InvalidInput.into());
    }
    let registry = HouseRegistry::new(registry)?;
    let mut runtime = runtime_config(&registry, house)?.unwrap_or(RuntimeConfig {
        schema: RUNTIME_SCHEMA,
        house: house.clone(),
        orca: None,
        curl: None,
        repository: None,
        pickup: None,
    });
    if orca_given.contains(&true) {
        let stored = runtime.orca.take();
        let orca = (
            flags
                .orca
                .clone()
                .or(stored.as_ref().map(|orca| orca.executable.clone())),
            flags
                .runtime_dir
                .clone()
                .or(stored.as_ref().map(|orca| orca.runtime_dir.clone())),
            flags
                .orca_run
                .clone()
                .or(stored.as_ref().map(|orca| orca.run.clone())),
            flags
                .orca_coordinator
                .clone()
                .or(stored.as_ref().map(|orca| orca.coordinator.clone())),
            flags
                .orca_repo
                .clone()
                .or(stored.as_ref().map(|orca| orca.repo.clone())),
        );
        let (Some(executable), Some(runtime_dir), Some(run), Some(coordinator), Some(repo)) = orca
        else {
            return Err(kitchen::workflows::run::RunError::BackendArguments(
                "all of --orca, --runtime-dir, --orca-run, --orca-coordinator, and --orca-repo",
            )
            .into());
        };
        runtime.orca = Some(OrcaHost {
            executable,
            runtime_dir,
            run,
            coordinator,
            repo,
        });
    }
    runtime.curl = flags.curl.clone().or(runtime.curl);
    runtime.repository = repository.or(runtime.repository);
    if pickup.any() {
        runtime.pickup = Some(pickup.overlay(runtime.pickup.take().unwrap_or_default()));
    }
    let outcome = match store_runtime(&registry, &runtime)? {
        RuntimeOutcome::Created => "stored",
        RuntimeOutcome::Replaced => "updated",
        RuntimeOutcome::Unchanged => "unchanged",
    };
    Ok((
        format!("{outcome} the runtime configuration of house {house}"),
        true,
    ))
}

fn run_tick(
    scope: HouseScope,
    repository: Option<Repository>,
    backend: &BackendArgs,
    pickup: &PickupArgs,
) -> Result<(String, bool), kitchen::Error> {
    let (Some(house), Some(registry)) = (scope.house, scope.registry) else {
        return Err(HouseError::InvalidInput.into());
    };
    if scope
        .store
        .as_ref()
        .is_some_and(|store| !store.is_absolute())
    {
        return Err(HouseError::InvalidInput.into());
    }
    let opened = Opened::open_parts(registry, &house, scope.store, repository)?;
    let clock = SystemClock;
    let holder = HolderId::new(&format!(
        "tick-{}-{}",
        std::process::id(),
        clock.now().as_unix_millis()
    ))?;
    let mut passes = DuePasses {
        opened: &opened,
        backend,
        pickup,
        clock: &clock,
        errors: BTreeMap::new(),
    };
    let report = tick::tick(&opened.store, &opened.config, &holder, &mut passes, &clock)?;
    let mut text = String::new();
    for pass in &report.passes {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = match &pass.decision {
            TickDecision::Ran { run, outcome } => {
                write!(text, "{}: {run}: {}", pass.pass, outcome_text(*outcome)).and_then(|()| {
                    match passes.errors.get(&pass.pass) {
                        Some(error) => write!(text, ": {error}"),
                        None => Ok(()),
                    }
                })
            }
            TickDecision::NotDue { next_due } => write!(
                text,
                "{}: not due until {}",
                pass.pass,
                next_due.as_unix_millis()
            ),
            TickDecision::Blocked {
                run,
                unresolved_effects,
            } => write!(
                text,
                "{pass}: blocked: {run} is uncertain{newly} ({unresolved_effects} unresolved effects on the tasks it recorded; it may have done more). Check what it did, then run `kitchn tick settle --pass {pass} --run {number}`.",
                pass = pass.pass,
                newly = if pass.newly_uncertain {
                    ", recorded now"
                } else {
                    ""
                },
                number = run.get(),
            ),
            TickDecision::Superseded { run } => write!(
                text,
                "{}: {run} outlasted its lease or runtime; its end was refused",
                pass.pass
            ),
            TickDecision::Busy { holder, expires_at } => write!(
                text,
                "{}: busy: {} holds it until {}",
                pass.pass,
                holder.as_str(),
                expires_at.as_unix_millis()
            ),
        };
    }
    Ok((text, report.healthy()))
}

fn settle(
    scope: HouseScope,
    pass: Pass,
    run: RunId,
    reason: &Text,
    holder: HolderId,
) -> Result<(String, bool), kitchen::Error> {
    let (store, _, _) = scope.open()?;
    let settled = store.settle_run(
        pass,
        run,
        &Claimant::interactive(holder),
        reason,
        SystemClock.now(),
    )?;
    let (record, already) = match &settled {
        RunSettle::Settled(record) => (record, false),
        RunSettle::AlreadySettled(record) => (record, true),
    };
    let RunState::Settled {
        by,
        settled_at,
        reason,
        unresolved_effects,
        ..
    } = &record.state
    else {
        return Err(HouseError::InvalidInput.into());
    };
    let text = if already {
        format!(
            "{pass}: {run} was already settled by {} at {}: {}\nNothing changed.",
            by.as_str(),
            settled_at.as_unix_millis(),
            reason.as_str()
        )
    } else {
        format!(
            "{pass}: settled {run} for {}: {} ({unresolved_effects} unresolved effects recorded)\nThe pass runs again when due.",
            by.as_str(),
            reason.as_str()
        )
    };
    Ok((text, true))
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
            RunState::Uncertain { recorded_at } => write!(
                text,
                "uncertain, recorded at {}; blocks the pass until settled",
                recorded_at.as_unix_millis()
            ),
            RunState::Settled {
                by,
                settled_at,
                reason,
                unresolved_effects,
                ..
            } => write!(
                text,
                "uncertain, settled by {} at {} ({unresolved_effects} unresolved effects): {}",
                by.as_str(),
                settled_at.as_unix_millis(),
                reason.as_str()
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
            PassFailure::Busy => "failed: another run of the pass holds its workflow lease",
            PassFailure::OwnerUncertain => {
                "failed: its workflow lease expired without a release; check what the last run did, then run `kitchn run <pass> --take-over`"
            }
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
