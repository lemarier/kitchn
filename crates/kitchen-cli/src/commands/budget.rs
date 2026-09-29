//! `kitchn budget`: the schedule budget tick on the house's Orca schedules.
//!
//! `precheck` reads only and reports through its exit status. `run` claims
//! the window's task, pauses exhausted schedules, and posts each due report
//! as a comment on the report issue. `install` creates the tick's schedule,
//! paused; nothing here activates a schedule, and the tick is budgeted like
//! any other schedule, so an exhausted house budget pauses it too. Policy, claims, and effects stay
//! in [`kitchen::workflows::budget`].

use std::{
    fmt::Write as _,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    BackendId, CredentialId, HolderId, HouseId,
    adapters::orca::{
        DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT, OrcaBackend,
        OrcaConfig, SystemRunner,
    },
    adoption::HouseRegistry,
    contracts::{
        Claimant, Effect, EffectExecutor, ExternalRef, GitHubAction, GitHubMutation, Grant,
        IssueNumber, LeaseTtl, Permission, PostingBudget, Provenance, Repository, SystemClock,
        Text,
    },
    house::{HouseConfig, HouseError},
    integrations::github::{
        CredentialFile, CredentialRef, GhCli, GitHubExecutor, HouseScope, IntegrationError,
        ReadLimits,
    },
    scheduling::{AgentFamily, CronExpr, PrecheckOutcome, Recurrence, SchedulePolicy, Timezone},
    selection::{AgentSelection, ResolvedSelection},
    state::{HouseStore, StoreOptions},
    workflows::{
        Precheck, WorkflowError,
        budget::{
            self, BudgetPass, Delivery, PassAction, ReportArgs, ReportChannel, Tick, TickArgs,
        },
        precheck_outcome,
    },
};

#[derive(Args)]
pub struct BudgetArgs {
    #[command(subcommand)]
    command: BudgetCommand,
}

#[derive(Subcommand)]
enum BudgetCommand {
    /// Scheduled precheck: exit 0 when a schedule must be paused or reported,
    /// 1 when none must, 2 for invalid input, and 3 when the schedules or
    /// report records cannot be read. Only reads.
    Precheck(Source),
    /// Pause exhausted schedules and post each due owner report. Exits 1 when
    /// a pause or report did not go through.
    Run {
        #[command(flatten)]
        source: Source,
        #[command(flatten)]
        report: ReportFlags,
    },
    /// Install the budget tick's schedule, paused. Activating it is the
    /// owner's separate decision.
    Install {
        #[command(flatten)]
        source: Source,
        #[command(flatten)]
        report: ReportFlags,
        /// Absolute path of the installed kitchn executable the schedule runs.
        #[arg(long)]
        kitchen: PathBuf,
        /// Cron expression for the tick, such as `15 * * * *`.
        #[arg(long)]
        cron: String,
        #[arg(long)]
        timezone: String,
        /// The agent family that relays the run's output.
        #[arg(long, value_enum)]
        agent: AgentArg,
    },
}

/// The house, its store, and its Orca schedules.
#[derive(Args)]
struct Source {
    /// The house registry holding the house configuration.
    #[arg(long)]
    registry: PathBuf,
    #[arg(long)]
    house: HouseId,
    /// The house's initialized state store.
    #[arg(long)]
    store: PathBuf,
    /// Absolute path of the Orca executable.
    #[arg(long)]
    orca: PathBuf,
    /// The Orca backend namespace the house's schedule grant names.
    #[arg(long)]
    backend: BackendId,
    /// The Orca host session credential that grant names.
    #[arg(long)]
    credential: CredentialId,
    /// House-scoped Orca runtime storage shared by every caller.
    #[arg(long)]
    runtime_dir: PathBuf,
}

/// Where owner reports are posted. Without `--report-issue` a run still
/// pauses, prints each report as undeliverable, and records that once per
/// window, so later ticks in the window stay idle.
#[derive(Args)]
struct ReportFlags {
    /// The report issue as `owner/repo#number`, in a house posting destination.
    #[arg(long, requires_all = ["github_backend", "requester", "github_credential", "credential_file", "gh"])]
    report_issue: Option<String>,
    /// The GitHub backend namespace the house's comment grant names.
    #[arg(long)]
    github_backend: Option<BackendId>,
    /// The GitHub login the credential must authenticate as.
    #[arg(long)]
    requester: Option<ExternalRef>,
    /// The house credential the comment grant names.
    #[arg(long)]
    github_credential: Option<CredentialId>,
    /// Absolute path of the private file holding that credential's token.
    #[arg(long)]
    credential_file: Option<PathBuf>,
    /// Absolute path of the GitHub CLI.
    #[arg(long)]
    gh: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentArg {
    Claude,
    Codex,
}

impl From<AgentArg> for AgentFamily {
    fn from(agent: AgentArg) -> Self {
        match agent {
            AgentArg::Claude => Self::Claude,
            AgentArg::Codex => Self::Codex,
        }
    }
}

/// How long a tick holds its window's task. A pass makes at most one Orca
/// edit per schedule and one post per report, each under a call deadline.
const TICK_LEASE: Duration = Duration::from_secs(15 * 60);

/// Reports one window's task may post: the ceiling of a posting budget.
const REPORT_POSTS: u32 = 100;

/// A command's result: the precheck reports through its exit status alone.
pub enum Outcome {
    Exit(ExitCode),
    Output(Result<(String, bool), kitchen::Error>),
}

pub fn run(args: BudgetArgs) -> Outcome {
    Outcome::Output(match args.command {
        BudgetCommand::Precheck(source) => return Outcome::Exit(precheck(&source)),
        BudgetCommand::Run { source, report } => tick(source, &report),
        BudgetCommand::Install {
            source,
            report,
            kitchen,
            cron,
            timezone,
            agent,
        } => install(source, &report, kitchen, &cron, &timezone, agent),
    })
}

fn precheck(source: &Source) -> ExitCode {
    let result = Opened::open(source).and_then(|opened| {
        let evidence = opened.backend.schedule_evidence()?;
        budget::precheck(
            &opened.store,
            &opened.config.house,
            &opened.policy,
            &evidence,
        )
    });
    report_precheck(result)
}

fn report_precheck(result: Result<Precheck, kitchen::Error>) -> ExitCode {
    let (written, code) = match result {
        Ok(precheck) => match precheck_outcome(Ok(precheck)) {
            PrecheckOutcome::Actionable => (writeln!(io::stdout().lock(), "actionable"), 0),
            PrecheckOutcome::Idle => (writeln!(io::stdout().lock(), "idle"), 1),
            PrecheckOutcome::Error => (Ok(()), 3),
        },
        Err(error) => {
            let code = match error.class() {
                kitchen::ErrorClass::InvalidInput => 2,
                kitchen::ErrorClass::Refused
                | kitchen::ErrorClass::Conflict
                | kitchen::ErrorClass::Execution => 3,
            };
            (writeln!(io::stderr().lock(), "error: {error}"), code)
        }
    };
    // A result that could not be reported is an error, never idle.
    if written.is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}

/// The house configuration, store, and Orca backend one command acts on.
struct Opened {
    config: HouseConfig,
    policy: SchedulePolicy,
    store: HouseStore,
    backend: OrcaBackend<SystemRunner>,
}

impl Opened {
    fn open(source: &Source) -> Result<Self, kitchen::Error> {
        if !source.orca.is_absolute() {
            return Err(HouseError::InvalidInput.into());
        }
        let config = HouseRegistry::new(&source.registry)?.load(&source.house)?;
        // Without a policy there is no budget to enforce: refuse rather than
        // report every schedule as within budget.
        let policy = config
            .schedules
            .clone()
            .ok_or(WorkflowError::IncompleteEvidence)?;
        let store = HouseStore::open(&source.store, source.house.clone(), StoreOptions::default())?;
        let backend = OrcaBackend::connect(
            OrcaConfig {
                backend: source.backend.clone(),
                house: source.house.clone(),
                credential: source.credential.clone(),
                // Schedule calls name no Run, coordinator, or repository.
                run: ExternalRef::new(budget::WORKFLOW)?,
                coordinator: ExternalRef::new(budget::WORKFLOW)?,
                repo: ExternalRef::new(budget::WORKFLOW)?,
                base_branch: None,
                branch_prefix: None,
                agent: AgentFamily::Claude,
                call_timeout: DEFAULT_CALL_TIMEOUT,
                launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
                runtime_dir: source.runtime_dir.clone(),
                reservation_timeout: DEFAULT_RESERVATION_TIMEOUT,
            },
            SystemRunner::new(&source.orca),
        )?
        .with_schedule_policy(policy.clone());
        Ok(Self {
            config,
            policy,
            store,
            backend,
        })
    }

    /// The standing grant the tick pauses schedules under.
    fn schedule_grant(&self, source: &Source) -> Grant {
        Grant::house(
            Permission::ManageSchedule,
            source.backend.clone(),
            source.credential.clone(),
        )
    }
}

/// The report issue and GitHub access, validated against the house.
struct Report {
    args: ReportArgs,
    grant: Grant,
}

impl Report {
    fn from_flags(
        flags: &ReportFlags,
        config: &HouseConfig,
    ) -> Result<Option<Self>, kitchen::Error> {
        let Some(issue) = &flags.report_issue else {
            return Ok(None);
        };
        let (repository, number) = issue.rsplit_once('#').ok_or(HouseError::InvalidInput)?;
        let repository: Repository = repository.parse()?;
        let number: u64 = number.parse().map_err(|_| HouseError::InvalidInput)?;
        // The destination comes from house policy; the flag only picks an
        // issue in one of its declared destinations.
        if !config.posting_destinations.contains(&repository) {
            return Err(IntegrationError::PermissionDenied.into());
        }
        let missing = || kitchen::Error::from(HouseError::InvalidInput);
        let args = ReportArgs {
            repository: repository.clone(),
            issue: IssueNumber::new(number)?,
            backend: flags.github_backend.clone().ok_or_else(missing)?,
            requester: flags.requester.clone().ok_or_else(missing)?,
            credential: flags.github_credential.clone().ok_or_else(missing)?,
            credential_file: flags.credential_file.clone().ok_or_else(missing)?,
            gh: flags.gh.clone().ok_or_else(missing)?,
        };
        let grant = Grant::repository(
            Permission::PostComment,
            repository,
            args.backend.clone(),
            args.credential.clone(),
        );
        Ok(Some(Self { args, grant }))
    }

    fn executor(&self, house: &HouseId) -> Result<GitHubExecutor<GhCli>, kitchen::Error> {
        let args = &self.args;
        let reference = CredentialRef::new(
            house.clone(),
            args.credential.clone(),
            args.requester.clone(),
        );
        let scope = HouseScope::new(
            house.clone(),
            [args.repository.clone()],
            args.requester.clone(),
            reference.clone(),
            PostingBudget::new(REPORT_POSTS)?,
            [Permission::PostComment],
        )?;
        let credential = CredentialFile::new(reference, args.credential_file.clone())?;
        let gh = GhCli::new(args.gh.clone(), credential)?;
        Ok(GitHubExecutor::new(
            args.backend.clone(),
            scope,
            gh,
            ReadLimits::default(),
        ))
    }
}

fn tick(source: Source, flags: &ReportFlags) -> Result<(String, bool), kitchen::Error> {
    let opened = Opened::open(&source)?;
    let report = Report::from_flags(flags, &opened.config)?;
    let executor = report
        .as_ref()
        .map(|report| report.executor(&opened.config.house))
        .transpose()?;
    let effect = |exhaustion: &kitchen::scheduling::BudgetExhaustion| {
        report_effect(executor.as_ref(), report.as_ref(), exhaustion)
    };
    let channel = executor.as_ref().map(|executor| ReportChannel {
        executor: executor as &dyn EffectExecutor,
        effect: &effect,
    });
    let mut authority = vec![opened.schedule_grant(&source)];
    authority.extend(report.as_ref().map(|report| report.grant.clone()));
    let grants = opened.config.authority()?;
    let claimant = Claimant::scheduled(HolderId::new(budget::WORKFLOW)?);
    let tick = Tick {
        store: &opened.store,
        schedules: &opened.backend,
        reports: channel,
        grants: &grants,
        authority,
        provenance: Provenance {
            kitchen: opened.config.kitchen.clone(),
            house_guidance: opened.config.guidance.clone(),
            repository_instructions: None,
        },
        claimant: &claimant,
        ttl: LeaseTtl::new(TICK_LEASE)?,
        clock: &SystemClock,
    };
    let evidence = opened.backend.schedule_evidence()?;
    let outcome = budget::tick(&tick, &opened.policy, &evidence)?;
    Ok(render(&outcome))
}

fn report_effect(
    executor: Option<&GitHubExecutor<GhCli>>,
    report: Option<&Report>,
    exhaustion: &kitchen::scheduling::BudgetExhaustion,
) -> Result<Effect, kitchen::Error> {
    let (Some(executor), Some(report)) = (executor, report) else {
        return Err(HouseError::InvalidInput.into());
    };
    let effect = executor.effect(GitHubMutation {
        repository: report.args.repository.clone(),
        action: GitHubAction::PostComment {
            issue: report.args.issue,
            body: Text::new(&exhaustion.report())?,
        },
    })?;
    Ok(Effect::GitHub(effect))
}

/// One line per action; healthy only when every pause applied and every
/// due report was posted.
fn render(outcome: &budget::TickReport) -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    for task in &outcome.settled {
        let _ = writeln!(out, "settled {task}");
    }
    match &outcome.pass {
        BudgetPass::Idle => out.push_str("idle"),
        BudgetPass::Acted(actions) => {
            for action in actions {
                let _ = match action {
                    PassAction::Report(exhaustion) => {
                        writeln!(out, "paused {}", exhaustion.consumer)
                    }
                    PassAction::Repaused(exhaustion) => {
                        writeln!(out, "paused again {}", exhaustion.consumer)
                    }
                    PassAction::PauseNotApplied { exhaustion, record } => {
                        healthy = false;
                        writeln!(
                            out,
                            "pause not applied {}: {:?}",
                            exhaustion.consumer,
                            record.state()
                        )
                    }
                };
            }
        }
    }
    for delivery in &outcome.deliveries {
        let _ = match delivery {
            Delivery::Delivered(exhaustion) => writeln!(out, "reported {}", exhaustion.consumer),
            Delivery::Undeliverable(exhaustion) => {
                healthy = false;
                writeln!(out, "undeliverable: {}", exhaustion.report())
            }
            Delivery::NotDelivered { exhaustion, record } => {
                healthy = false;
                writeln!(
                    out,
                    "report not delivered {}: {:?}",
                    exhaustion.consumer,
                    record.state()
                )
            }
        };
    }
    (out.trim_end().to_owned(), healthy)
}

fn install(
    source: Source,
    flags: &ReportFlags,
    kitchen: PathBuf,
    cron: &str,
    timezone: &str,
    agent: AgentArg,
) -> Result<(String, bool), kitchen::Error> {
    let opened = Opened::open(&source)?;
    let report = Report::from_flags(flags, &opened.config)?;
    // Installing needs the standing grants every tick delegates: the
    // schedule grant it pauses under and the comment grant it reports under,
    // or every tick would fail before its first pause. The backend must run
    // a precheck-gated schedule.
    let grants = opened.config.authority()?;
    if !grants.covers(&opened.schedule_grant(&source)) {
        return Err(kitchen::contracts::ContractError::PermissionDenied {
            permission: Permission::ManageSchedule,
        }
        .into());
    }
    if report
        .as_ref()
        .is_some_and(|report| !grants.covers(&report.grant))
    {
        return Err(kitchen::contracts::ContractError::PermissionDenied {
            permission: Permission::PostComment,
        }
        .into());
    }
    let capabilities = &opened.backend.descriptor().capabilities;
    if !budget::REQUIRED_CAPABILITIES
        .iter()
        .all(|capability| capabilities.supports(*capability))
    {
        return Err(WorkflowError::IncompleteEvidence.into());
    }
    let args = TickArgs {
        kitchen,
        registry: source.registry.clone(),
        house: source.house.clone(),
        store: source.store.clone(),
        orca: source.orca.clone(),
        backend: source.backend.clone(),
        credential: source.credential.clone(),
        runtime_dir: source.runtime_dir.clone(),
        report: report.map(|report| report.args),
    };
    let tick = budget::install(
        Recurrence::Cron(CronExpr::new(cron).map_err(|_| HouseError::InvalidInput)?),
        Timezone::new(timezone).map_err(|_| HouseError::InvalidInput)?,
        ResolvedSelection::owner(AgentSelection::agent_default(agent.into())),
        &args,
    )?;
    let installed = opened.backend.install_schedule(&tick)?;
    Ok((
        format!(
            "installed paused {} ({})",
            tick.consumer(),
            installed.handle
        ),
        true,
    ))
}
