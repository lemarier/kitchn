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
        BackendKind, CredentialKind, ForgeCredential, HouseConfig, PickupConfig, credential_path,
        forge_binding, runtime_config,
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
#[derive(Args, Default)]
pub(super) struct BackendArgs {
    /// Absolute path of the Orca executable, for a house bound to Orca.
    #[arg(long)]
    pub(super) orca: Option<PathBuf>,
    /// House-scoped Orca runtime storage shared by every caller.
    #[arg(long)]
    pub(super) runtime_dir: Option<PathBuf>,
    /// The Orca Run that owns the house's workers and mailbox.
    #[arg(long)]
    pub(super) orca_run: Option<ExternalRef>,
    /// The Orca coordinator terminal handle calls are attributed to.
    #[arg(long)]
    pub(super) orca_coordinator: Option<ExternalRef>,
    /// The Orca repository selector for worker workspaces, such as
    /// `id:<repo-id>`.
    #[arg(long)]
    pub(super) orca_repo: Option<ExternalRef>,
    /// Absolute path of `curl`, for a house bound to an HTTP backend.
    #[arg(long)]
    pub(super) curl: Option<PathBuf>,
}

/// The scheduled pickup settings. Unset flags use the house's stored ones,
/// or the defaults when none are stored; a flag that disagrees with the
/// stored settings is refused, and only `kitchn tick configure` changes them.
#[derive(Args, Default)]
pub(super) struct PickupArgs {
    /// The label that marks an issue ready for an agent (default: ready).
    #[arg(long)]
    ready_label: Option<String>,
    /// The label that marks an issue needing a specification pass (default:
    /// needs-spec).
    #[arg(long)]
    needs_spec_label: Option<String>,
    /// The label that reserves an issue for a person (default: human-only).
    #[arg(long)]
    human_label: Option<String>,
    /// Most unsettled scheduled pickup tasks in the repository, 1 to 64
    /// (default: 1). A pass still launches at most one writer, since file
    /// overlap is not observed.
    #[arg(long)]
    capacity: Option<u32>,
    /// Workers create `<prefix>/issue-<number>` (default: kitchen).
    #[arg(long)]
    branch_prefix: Option<String>,
    /// Where each worker writes its evidence report in its workspace
    /// (default: kitchen-report.md).
    #[arg(long)]
    report_path: Option<String>,
}

impl PickupArgs {
    /// Whether any setting was given.
    pub(super) const fn any(&self) -> bool {
        self.ready_label.is_some()
            || self.needs_spec_label.is_some()
            || self.human_label.is_some()
            || self.capacity.is_some()
            || self.branch_prefix.is_some()
            || self.report_path.is_some()
    }

    /// The given settings over `base`.
    pub(super) fn overlay(&self, base: PickupConfig) -> PickupConfig {
        PickupConfig {
            ready_label: self.ready_label.clone().unwrap_or(base.ready_label),
            needs_spec_label: self
                .needs_spec_label
                .clone()
                .unwrap_or(base.needs_spec_label),
            human_label: self.human_label.clone().unwrap_or(base.human_label),
            capacity: self.capacity.unwrap_or(base.capacity),
            branch_prefix: self.branch_prefix.clone().unwrap_or(base.branch_prefix),
            report_path: self.report_path.clone().unwrap_or(base.report_path),
        }
    }

    /// The settings a pass uses: the stored ones, or the defaults, and the
    /// flags only where they agree with what is stored.
    fn resolve(&self, stored: Option<PickupConfig>) -> Result<PickupConfig, RunError> {
        let Some(stored) = stored else {
            return Ok(self.overlay(PickupConfig::default()));
        };
        agree(
            "--ready-label",
            self.ready_label.as_ref(),
            Some(&stored.ready_label),
        )?;
        agree(
            "--needs-spec-label",
            self.needs_spec_label.as_ref(),
            Some(&stored.needs_spec_label),
        )?;
        agree(
            "--human-label",
            self.human_label.as_ref(),
            Some(&stored.human_label),
        )?;
        agree("--capacity", self.capacity.as_ref(), Some(&stored.capacity))?;
        agree(
            "--branch-prefix",
            self.branch_prefix.as_ref(),
            Some(&stored.branch_prefix),
        )?;
        agree(
            "--report-path",
            self.report_path.as_ref(),
            Some(&stored.report_path),
        )?;
        Ok(stored)
    }
}

/// The flag's value when it agrees with the stored one, the stored one when
/// no flag was given.
///
/// # Errors
/// [`RunError::RuntimeMismatch`] naming `flag` when both exist and differ.
pub(super) fn agree<'a, T: PartialEq>(
    flag: &'static str,
    given: Option<&'a T>,
    stored: Option<&'a T>,
) -> Result<Option<&'a T>, RunError> {
    match (given, stored) {
        (Some(given), Some(stored)) if given != stored => Err(RunError::RuntimeMismatch(flag)),
        (Some(value), _) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

/// The house, its store, and the repository one pass serves.
pub(super) struct Opened {
    pub(super) registry: HouseRegistry,
    pub(super) config: HouseConfig,
    pub(super) store: HouseStore,
    pub(super) repository: Repository,
}

impl Opened {
    fn open(args: &HouseArgs) -> Result<Self, kitchen::Error> {
        Self::open_parts(
            args.registry.clone(),
            &args.house,
            args.store.clone(),
            args.repository.clone(),
        )
    }

    /// Open `house` from `registry`, with `store` or the default one, for
    /// `repository` or the house's only one.
    pub(super) fn open_parts(
        registry: PathBuf,
        house: &HouseId,
        store: Option<PathBuf>,
        repository: Option<Repository>,
    ) -> Result<Self, kitchen::Error> {
        let root = super::house::canonical_root(registry)?;
        let store = super::house::store_or_default(store, Some(&root), house)?;
        let registry = HouseRegistry::new(root)?;
        let config = registry.load(house)?;
        let store = HouseStore::open(store, house.clone(), StoreOptions::default())?;
        // A house with several repositories may store which one its passes
        // serve. The stored file is read when that decides or a repository
        // was named, and a named one must agree with it.
        let repository = match repository {
            None if config.repositories.len() > 1 => {
                runtime_config(&registry, house)?.and_then(|stored| stored.repository)
            }
            None => None,
            Some(named) => {
                let stored = runtime_config(&registry, house)?.and_then(|stored| stored.repository);
                agree("--repository", Some(&named), stored.as_ref())?.cloned()
            }
        };
        let repository = pass_repository(&config, repository)?;
        Ok(Self {
            registry,
            config,
            store,
            repository,
        })
    }

    /// The house's forge reads, over its forge binding's checked credential,
    /// with `gh` (and `curl` for a GitHub App) from `PATH`.
    pub(super) fn forge(&self) -> Result<GitHubClient<GhCli>, kitchen::Error> {
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

    /// The house's bound worker backend for `caller` (a pass name, or the
    /// tick), which must support `required`. Orca names each worker's
    /// workspace after its branch without `branch_prefix`.
    pub(super) fn backend(
        &self,
        args: &BackendArgs,
        caller: &str,
        branch_prefix: Option<BranchName>,
        required: &[Capability],
    ) -> Result<Box<dyn CoordinatorMailbox>, kitchen::Error> {
        let (_, kind) = backend_binding(&self.config)?;
        let missing = |needs| kitchen::Error::from(RunError::BackendArguments(needs));
        // The house's stored runtime configuration fills what flags leave
        // out. A flag that disagrees with it is refused before connecting.
        let stored = runtime_config(&self.registry, &self.config.house)?;
        match kind {
            BackendKind::Orca => {
                let stored = stored.and_then(|stored| stored.orca);
                let stored = stored.as_ref();
                let (Some(orca), Some(runtime_dir), Some(run), Some(coordinator), Some(repo)) = (
                    agree(
                        "--orca",
                        args.orca.as_ref(),
                        stored.map(|orca| &orca.executable),
                    )?,
                    agree(
                        "--runtime-dir",
                        args.runtime_dir.as_ref(),
                        stored.map(|orca| &orca.runtime_dir),
                    )?,
                    agree(
                        "--orca-run",
                        args.orca_run.as_ref(),
                        stored.map(|orca| &orca.run),
                    )?,
                    agree(
                        "--orca-coordinator",
                        args.orca_coordinator.as_ref(),
                        stored.map(|orca| &orca.coordinator),
                    )?,
                    agree(
                        "--orca-repo",
                        args.orca_repo.as_ref(),
                        stored.map(|orca| &orca.repo),
                    )?,
                ) else {
                    return Err(missing(ORCA_ARGUMENTS));
                };
                if !orca.is_absolute() || !runtime_dir.is_absolute() {
                    return Err(missing("absolute --orca and --runtime-dir paths"));
                }
                Ok(Box::new(resolve_backend(
                    &self.config,
                    OrcaSession {
                        run: run.clone(),
                        coordinator: coordinator.clone(),
                        repo: repo.clone(),
                        base_branch: None,
                        branch_prefix,
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
                let curl = agree(
                    "--curl",
                    args.curl.as_ref(),
                    stored.as_ref().and_then(|stored| stored.curl.as_ref()),
                )?;
                let Some(curl) = curl.filter(|curl| curl.is_absolute()) else {
                    return Err(missing(HTTP_ARGUMENTS));
                };
                Ok(Box::new(resolve_http_backend(
                    &self.registry,
                    &self.config,
                    HttpSession {
                        run: ExternalRef::new(&format!("kitchen-{}", self.config.house))?,
                        coordinator: ExternalRef::new(caller)?,
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

/// What an Orca house's passes need, from flags or the stored runtime
/// configuration.
const ORCA_ARGUMENTS: &str = "--orca, --runtime-dir, --orca-run, --orca-coordinator, and --orca-repo (or store them with `kitchn tick configure`)";
/// What an HTTP house's passes need.
const HTTP_ARGUMENTS: &str = "an absolute --curl path (or store it with `kitchn tick configure`)";

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
            let settings = settings(&opened, &pickup)?;
            let backend = opened.backend(
                &backend,
                Pass::Pickup.as_str(),
                Some(settings.branch_prefix.clone()),
                MailboxRoute::House.worker_requirements(),
            )?;
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
                    tick: None,
                }
                .run()?,
            )
        }),
        RunCommand::Coordinate { house, backend } => Opened::open(&house).and_then(|opened| {
            let backend = opened.backend(
                &backend,
                Pass::Coordinate.as_str(),
                None,
                MailboxRoute::House.worker_requirements(),
            )?;
            let forge = opened.forge()?;
            render(
                CoordinatePass {
                    store: &opened.store,
                    house: &opened.config,
                    backend: backend.as_ref(),
                    forge: &forge,
                    clock: &clock,
                    take_over: house.take_over,
                    tick: None,
                }
                .run()?,
            )
        }),
        RunCommand::Repair { house, backend } => Opened::open(&house).and_then(|opened| {
            let backend = opened.backend(
                &backend,
                Pass::Repair.as_str(),
                None,
                &[Capability::WorkerStatusAndOutcome],
            )?;
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
                    tick: None,
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
                    tick: None,
                }
                .run()?,
            )
        }),
    };
    report(result)
}

pub(super) fn settings(
    opened: &Opened,
    args: &PickupArgs,
) -> Result<PickupSettings, kitchen::Error> {
    let stored =
        runtime_config(&opened.registry, &opened.config.house)?.and_then(|stored| stored.pickup);
    let pickup = args.resolve(stored)?;
    pickup.validate()?;
    let resolved = resolve_instructions(opened.registry.root(), &opened.config, None)?;
    Ok(PickupSettings {
        repository: opened.repository.clone(),
        labels: PickupLabels {
            ready: pickup.ready_label,
            needs_spec: pickup.needs_spec_label,
            human_only: pickup.human_label,
        },
        capacity: pickup.capacity,
        branch_prefix: BranchName::new(&pickup.branch_prefix)?,
        instructions: PinnedInstructions {
            house: resolved.house,
            provenance: resolved.provenance,
            entrypoint: Text::new(&resolved.entrypoint.to_string_lossy())?,
        },
        report_path: Text::new(&pickup.report_path)?,
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
