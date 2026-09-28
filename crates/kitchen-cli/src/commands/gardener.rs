use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Args, Subcommand};
use kitchen::{
    CredentialId, HouseId,
    contracts::{Clock, ExternalRef, Permission, PostingBudget, Repository, SystemClock},
    integrations::github::{
        CredentialFile, CredentialRef, GhCli, GitHubClient, HouseScope, ReadLimits,
    },
    scheduling::PrecheckOutcome,
    state::{HouseStore, StoreOptions},
    workflows::{Precheck, WorkflowError, gardener, precheck_outcome},
};

#[derive(Args)]
pub struct GardenerArgs {
    #[command(subcommand)]
    command: GardenerCommand,
}

#[derive(Subcommand)]
enum GardenerCommand {
    /// Scheduled precheck: exit 0 when the repository needs a hygiene pass,
    /// 1 when it does not, 2 for invalid arguments, and 3 when the inventory
    /// or the house store cannot be read. Only reads GitHub and the store.
    Precheck(PrecheckArgs),
}

#[derive(Args)]
struct PrecheckArgs {
    #[arg(long)]
    house: HouseId,
    #[arg(long)]
    repository: Repository,
    /// The GitHub login the credential must authenticate as.
    #[arg(long)]
    requester: ExternalRef,
    #[arg(long)]
    credential: CredentialId,
    /// Absolute path of the private file holding the read token.
    #[arg(long)]
    credential_file: PathBuf,
    /// Absolute path of the GitHub CLI.
    #[arg(long)]
    gh: PathBuf,
    /// Absolute path of the house's initialized state store holding
    /// handled-stale markers. Schedules installed before this argument
    /// existed omit it; without it every stale or changed issue counts.
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long)]
    ready_label: String,
    #[arg(long)]
    working_label: String,
    /// Hours of changes to inspect, 1–168.
    #[arg(long)]
    lookback_hours: u16,
    /// Days without an update before an open issue is stale, 1–365.
    #[arg(long)]
    stale_days: u16,
}

/// Where a precheck failed: exit 2 before any read, 3 after.
enum Failure {
    Invalid,
    Read(WorkflowError),
}

fn invalid<E>(_: E) -> Failure {
    Failure::Invalid
}

pub fn run(args: GardenerArgs) -> ExitCode {
    match args.command {
        GardenerCommand::Precheck(args) => report(precheck(args)),
    }
}

fn precheck(args: PrecheckArgs) -> Result<Precheck, Failure> {
    let labels = gardener::AgentLabels {
        ready: args.ready_label,
        working: args.working_label,
    };
    labels.validate().map_err(invalid)?;
    let window =
        gardener::PrecheckWindow::new(args.lookback_hours, args.stale_days).map_err(invalid)?;
    let reference = CredentialRef::new(args.house.clone(), args.credential, args.requester.clone());
    let scope = HouseScope::new(
        args.house.clone(),
        [args.repository.clone()],
        args.requester,
        reference.clone(),
        PostingBudget::new(0).map_err(invalid)?,
        std::iter::empty::<Permission>(),
    )
    .map_err(invalid)?;
    if args
        .store
        .as_ref()
        .is_some_and(|store| !store.is_absolute())
    {
        return Err(Failure::Invalid);
    }
    let credential = CredentialFile::new(reference, args.credential_file).map_err(invalid)?;
    let gh = GhCli::new(args.gh, credential).map_err(invalid)?;
    let client = GitHubClient::new(scope, gh, ReadLimits::default());
    let window = window.window(SystemClock.now()).map_err(Failure::Read)?;
    let store = args
        .store
        .map(|store| HouseStore::open(store, args.house.clone(), StoreOptions::default()))
        .transpose()
        .map_err(|_| Failure::Read(WorkflowError::PrecheckFailed))?;
    let handled = store
        .as_ref()
        .map(gardener::StaleMarkers::new)
        .transpose()
        .map_err(Failure::Read)?;
    gardener::precheck(gardener::signal(
        &client,
        &args.house,
        &args.repository,
        &labels,
        window,
        handled.as_ref(),
    ))
    .map_err(Failure::Read)
}

fn report(result: Result<Precheck, Failure>) -> ExitCode {
    let (written, code) = match result.map(|precheck| precheck_outcome(Ok(precheck))) {
        Ok(PrecheckOutcome::Actionable) => (writeln!(io::stdout().lock(), "actionable"), 0),
        Ok(PrecheckOutcome::Idle) => (writeln!(io::stdout().lock(), "idle"), 1),
        Ok(PrecheckOutcome::Error) => (Ok(()), 3),
        Err(Failure::Invalid) => (
            writeln!(
                io::stderr().lock(),
                "error: invalid gardener precheck input"
            ),
            2,
        ),
        Err(Failure::Read(error)) => (writeln!(io::stderr().lock(), "error: {error}"), 3),
    };
    // A result that could not be reported is an error, never idle.
    if written.is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}
