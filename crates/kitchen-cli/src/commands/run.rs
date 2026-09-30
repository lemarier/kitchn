//! `kitchn run <pass>`: one bounded scheduled pass of pickup, coordination,
//! repair, or the merge gate, for a trigger to start with the house identity.
//!
//! The store defaults to the one `house init` created, the forge to the
//! house's forge binding with `gh` from `PATH`, and the worker backend to
//! the house's binding. Exit status: 0 when the pass acted or was idle, 1
//! for an execution failure or refusal, 2 for invalid input, 3 when another
//! pass of the same kind holds the lease, and 4 when a previous pass's lease
//! expired and `--take-over` was not given. Passes and their leases live in
//! [`kitchen::workflows::run`].

use std::{
    fmt::{Display, Write as _},
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Subcommand};
use kitchen::{
    ErrorClass, HouseId,
    adapters::{
        HttpSession, OrcaSession, backend_binding,
        orca::{
            DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT, SystemRunner,
        },
        resolve_backend, resolve_http_backend,
    },
    adoption::{HouseRegistry, resolve_instructions},
    contracts::{
        BranchName, Capability, CoordinatorMailbox, ExternalRef, Repository, SystemClock, Text,
    },
    house::{
        BackendKind, CredentialKind, ForgeCredential, HouseConfig, credential_path, forge_binding,
    },
    integrations::github::{CredentialFile, GhCli, GitHubClient, ReadLimits},
    scheduling::AgentFamily,
    state::{HouseStore, StoreOptions},
    workflows::{
        coordination::MailboxRoute,
        pickup::PinnedInstructions,
        run::{
            CoordinatePass, GatePass, Outcome, Pass, PickupLabels, PickupPass, PickupSettings,
            RepairPass, RunError, pass_repository,
        },
    },
};

#[derive(Args)]
pub struct RunArgs {
    #[command(subcommand)]
    pass: RunCommand,
}

#[derive(Subcommand)]
enum RunCommand {
    /// Claim ready issues of one repository and launch their workers, and
    /// launch the next attempt of scheduled tasks whose attempt ended.
    Pickup {
        #[command(flatten)]
        house: HouseArgs,
        #[command(flatten)]
        backend: BackendArgs,
        #[command(flatten)]
        pickup: PickupArgs,
    },
    /// Read worker deliveries and supervise every scheduled task once.
    Coordinate {
        #[command(flatten)]
        house: HouseArgs,
        #[command(flatten)]
        backend: BackendArgs,
    },
    /// Assess the pull requests of settled scheduled tasks for conflict
    /// repair and report each decision. Launches no writer.
    Repair {
        #[command(flatten)]
        house: HouseArgs,
        #[command(flatten)]
        backend: BackendArgs,
    },
    /// Evaluate the pull requests of settled scheduled tasks at their exact
    /// heads and report each verdict. Records nothing and merges nothing.
    Gate {
        #[command(flatten)]
        house: HouseArgs,
    },
}

/// The house a pass runs for.
#[derive(Args)]
struct HouseArgs {
    /// The house registry holding the house configuration.
    #[arg(long)]
    registry: PathBuf,
    #[arg(long)]
    house: HouseId,
    /// The house's initialized state store (default: the one `house init`
    /// created in the registry).
    #[arg(long)]
    store: Option<PathBuf>,
    /// The repository, as `owner/name` (default: the house's only one).
    #[arg(long)]
    repository: Option<Repository>,
    /// Continue after a previous pass's lease, or a scheduled task's claim,
    /// expired without a release. The takeover is recorded.
    #[arg(long)]
    take_over: bool,
}

/// Where the house's bound worker backend runs on this host.
#[derive(Args)]
struct BackendArgs {
    /// Absolute path of the Orca executable, for a house bound to Orca.
    #[arg(long)]
    orca: Option<PathBuf>,
    /// House-scoped Orca runtime storage shared by every caller.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    /// The Orca Run that owns the house's workers and mailbox.
    #[arg(long)]
    orca_run: Option<ExternalRef>,
    /// The Orca coordinator terminal handle calls are attributed to.
    #[arg(long)]
    orca_coordinator: Option<ExternalRef>,
    /// The Orca repository selector for worker workspaces, such as
    /// `id:<repo-id>`.
    #[arg(long)]
    orca_repo: Option<ExternalRef>,
    /// Absolute path of `curl`, for a house bound to an HTTP backend.
    #[arg(long)]
    curl: Option<PathBuf>,
}

#[derive(Args)]
struct PickupArgs {
    /// The label that marks an issue ready for an agent.
    #[arg(long, default_value = "ready")]
    ready_label: String,
    /// The label that marks an issue needing a specification pass.
    #[arg(long, default_value = "needs-spec")]
    needs_spec_label: String,
    /// The label that reserves an issue for a person.
    #[arg(long, default_value = "human-only")]
    human_label: String,
    /// Most unsettled scheduled pickup tasks in the repository.
    #[arg(long, default_value_t = 1)]
    capacity: u32,
    /// Workers create `<prefix>/issue-<number>`.
    #[arg(long, default_value = "kitchen")]
    branch_prefix: String,
    /// Where each worker writes its evidence report in its workspace.
    #[arg(long, default_value = "kitchen-report.md")]
    report_path: String,
}

/// The house, its store, and the repository one pass serves.
struct Opened {
    registry: HouseRegistry,
    config: HouseConfig,
    store: HouseStore,
    repository: Repository,
}

impl Opened {
    fn open(args: &HouseArgs) -> Result<Self, kitchen::Error> {
        let registry = HouseRegistry::new(super::house::canonical_root(args.registry.clone())?)?;
        let config = registry.load(&args.house)?;
        let store =
            super::house::store_or_default(args.store.clone(), Some(&args.registry), &args.house)?;
        let store = HouseStore::open(store, args.house.clone(), StoreOptions::default())?;
        let repository = pass_repository(&config, args.repository.clone())?;
        Ok(Self {
            registry,
            config,
            store,
            repository,
        })
    }

    /// The house's forge reads, over its forge binding's checked credential,
    /// with `gh` (and `curl` for a GitHub App) from `PATH`.
    fn forge(&self) -> Result<GitHubClient<GhCli>, kitchen::Error> {
        let binding = forge_binding(&self.registry, &self.config.house)?;
        let scope = binding.scope(&self.config)?;
        let file = CredentialFile::new(
            binding.credential_ref(),
            credential_path(&self.registry, &binding)?,
        )?;
        let transport = super::forge::connect_gh(match binding.credential_kind {
            CredentialKind::Token => ForgeCredential::Token(file),
            CredentialKind::GitHubApp(app) => ForgeCredential::App { app, key: file },
        })?;
        Ok(GitHubClient::new(scope, transport, ReadLimits::default()))
    }

    /// The house's bound worker backend, which must support `required`.
    fn backend(
        &self,
        args: &BackendArgs,
        required: &[Capability],
    ) -> Result<Box<dyn CoordinatorMailbox>, kitchen::Error> {
        let (_, kind) = backend_binding(&self.config)?;
        let missing = |needs| kitchen::Error::from(RunError::BackendArguments(needs));
        match kind {
            BackendKind::Orca => {
                let (Some(orca), Some(runtime_dir), Some(run), Some(coordinator), Some(repo)) = (
                    &args.orca,
                    &args.runtime_dir,
                    &args.orca_run,
                    &args.orca_coordinator,
                    &args.orca_repo,
                ) else {
                    return Err(missing(
                        "--orca, --runtime-dir, --orca-run, --orca-coordinator, and --orca-repo",
                    ));
                };
                if !orca.is_absolute() {
                    return Err(missing("an absolute --orca path"));
                }
                Ok(Box::new(resolve_backend(
                    &self.config,
                    OrcaSession {
                        run: run.clone(),
                        coordinator: coordinator.clone(),
                        repo: repo.clone(),
                        base_branch: None,
                        branch_prefix: None,
                        agent: AgentFamily::Claude,
                        call_timeout: DEFAULT_CALL_TIMEOUT,
                        launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
                        runtime_dir: runtime_dir.clone(),
                        reservation_timeout: DEFAULT_RESERVATION_TIMEOUT,
                    },
                    SystemRunner::new(orca),
                    required,
                )?))
            }
            BackendKind::Http => {
                let Some(curl) = args.curl.as_ref().filter(|curl| curl.is_absolute()) else {
                    return Err(missing("an absolute --curl path"));
                };
                Ok(Box::new(resolve_http_backend(
                    &self.registry,
                    &self.config,
                    HttpSession {
                        run: ExternalRef::new(&format!("kitchen-{}", self.config.house))?,
                        coordinator: ExternalRef::new(Pass::Coordinate.as_str())?,
                        curl: curl.clone(),
                        call_timeout: HTTP_CALL_TIMEOUT,
                    },
                    required,
                )?))
            }
            _ => Err(missing("a backend this command knows")),
        }
    }
}

/// Per-call deadline for an HTTP worker backend.
const HTTP_CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub fn run(args: RunArgs) -> ExitCode {
    let clock = SystemClock;
    let result = match args.pass {
        RunCommand::Pickup {
            house,
            backend,
            pickup,
        } => Opened::open(&house).and_then(|opened| {
            // The route is decided by the backend's own descriptor inside
            // the pass; only supervision is required to connect.
            let backend = opened.backend(&backend, MailboxRoute::House.worker_requirements())?;
            let settings = settings(&opened, pickup)?;
            let forge = opened.forge()?;
            render(
                PickupPass {
                    store: &opened.store,
                    house: &opened.config,
                    backend: backend.as_ref(),
                    forge: &forge,
                    clock: &clock,
                    settings: &settings,
                    take_over: house.take_over,
                }
                .run()?,
            )
        }),
        RunCommand::Coordinate { house, backend } => Opened::open(&house).and_then(|opened| {
            let backend = opened.backend(&backend, MailboxRoute::House.worker_requirements())?;
            let forge = opened.forge()?;
            render(
                CoordinatePass {
                    store: &opened.store,
                    house: &opened.config,
                    backend: backend.as_ref(),
                    forge: &forge,
                    clock: &clock,
                    take_over: house.take_over,
                }
                .run()?,
            )
        }),
        RunCommand::Repair { house, backend } => Opened::open(&house).and_then(|opened| {
            let backend = opened.backend(&backend, &[Capability::WorkerStatusAndOutcome])?;
            let forge = opened.forge()?;
            render(
                RepairPass {
                    store: &opened.store,
                    house: &opened.config,
                    backend: backend.as_ref(),
                    forge: &forge,
                    clock: &clock,
                    repository: &opened.repository,
                    take_over: house.take_over,
                }
                .run()?,
            )
        }),
        RunCommand::Gate { house } => Opened::open(&house).and_then(|opened| {
            let forge = opened.forge()?;
            let authors = [forge_binding(&opened.registry, &opened.config.house)?
                .requester
                .to_string()];
            render(
                GatePass {
                    store: &opened.store,
                    house: &opened.config,
                    forge: &forge,
                    clock: &clock,
                    repository: &opened.repository,
                    authors: &authors,
                    take_over: house.take_over,
                }
                .run()?,
            )
        }),
    };
    report(result)
}

fn settings(opened: &Opened, args: PickupArgs) -> Result<PickupSettings, kitchen::Error> {
    let resolved = resolve_instructions(opened.registry.root(), &opened.config, None)?;
    Ok(PickupSettings {
        repository: opened.repository.clone(),
        labels: PickupLabels {
            ready: args.ready_label,
            needs_spec: args.needs_spec_label,
            human_only: args.human_label,
        },
        capacity: args.capacity,
        branch_prefix: BranchName::new(&args.branch_prefix)?,
        instructions: PinnedInstructions {
            house: resolved.house,
            provenance: resolved.provenance,
            entrypoint: Text::new(&resolved.entrypoint.to_string_lossy())?,
        },
        report_path: Text::new(&args.report_path)?,
    })
}

/// The pass's output and exit status.
fn render<A: Display>(outcome: Outcome<A>) -> Result<(String, u8), kitchen::Error> {
    Ok(match outcome {
        Outcome::Idle => ("idle".to_owned(), 0),
        Outcome::Busy => (
            "busy: another pass of this kind holds the lease".to_owned(),
            3,
        ),
        Outcome::OwnerUncertain { expired_at } => (
            format!(
                "owner uncertain: the previous pass's lease expired at {} without a release; rerun with --take-over to continue",
                expired_at.as_unix_millis()
            ),
            4,
        ),
        Outcome::Acted(actions) => {
            let mut out = String::new();
            for action in actions {
                let _ = writeln!(out, "{action}");
            }
            (out.trim_end().to_owned(), 0)
        }
    })
}

fn report(result: Result<(String, u8), kitchen::Error>) -> ExitCode {
    let (written, code) = match result {
        Ok((output, code)) => (writeln!(io::stdout().lock(), "{output}"), code),
        Err(error) => {
            let code = match error.class() {
                ErrorClass::InvalidInput => 2,
                ErrorClass::Refused | ErrorClass::Conflict | ErrorClass::Execution => 1,
            };
            (writeln!(io::stderr().lock(), "error: {error}"), code)
        }
    };
    if written.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::from(code)
}
