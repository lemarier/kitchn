use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Subcommand};
use kitchen::{
    BackendId, CredentialId, HolderId, HouseId,
    adoption::HouseRegistry,
    contracts::{
        Claimant, Clock, ExternalRef, Grant, IssueNumber, LeaseTtl, Permission, PostingBudget,
        Provenance, Repository, SystemClock, Text,
    },
    integrations::github::{
        CredentialFile, CredentialRef, GhCli, GitHubClient, GitHubExecutor, HouseScope,
        IntegrationError, ReadLimits,
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
    /// Post the gardener's stale report on an issue and record the issue
    /// as handled once GitHub shows the post applied. Exits 0 when recorded
    /// or already handled at the issue's current revision, 1 when the post
    /// did not apply (nothing is recorded), 2 for invalid arguments, and 3
    /// when GitHub, the house, or its store refuse or cannot be read; nothing
    /// is recorded then. Running it again after a crash never posts twice.
    ReportStale(ReportStaleArgs),
}

#[derive(Args)]
struct ReportStaleArgs {
    /// The house registry holding the house configuration.
    #[arg(long)]
    registry: PathBuf,
    #[arg(long)]
    house: HouseId,
    /// Absolute path of the house's initialized state store.
    #[arg(long)]
    store: PathBuf,
    /// The stale issue's repository, one of the house's posting destinations.
    #[arg(long)]
    repository: Repository,
    /// The stale issue.
    #[arg(long)]
    issue: u64,
    /// The report comment's text.
    #[arg(long)]
    body: String,
    /// The GitHub backend namespace the house's comment grant names.
    #[arg(long)]
    github_backend: BackendId,
    /// The GitHub login the credential must authenticate as.
    #[arg(long)]
    requester: ExternalRef,
    /// The house credential the comment grant names.
    #[arg(long)]
    credential: CredentialId,
    /// Absolute path of the private file holding that credential's token.
    #[arg(long)]
    credential_file: PathBuf,
    /// Absolute path of the GitHub CLI.
    #[arg(long)]
    gh: PathBuf,
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

/// The claimant posting stale reports.
const REPORTER: &str = "gardener-report-stale";
/// Lease of one report run: two issue reads, a lookup, a post, and its
/// read-back, each bounded by the client's timeouts.
const REPORT_LEASE: Duration = Duration::from_secs(15 * 60);
/// Posts one report run may make.
const REPORT_POSTS: u32 = 1;

/// Where a precheck failed: exit 2 before any read, 3 after.
enum Failure {
    Invalid,
    Read(WorkflowError),
    /// Reporting failed after the arguments were accepted.
    Reported(kitchen::Error),
}

fn invalid<E>(_: E) -> Failure {
    Failure::Invalid
}

pub fn run(args: GardenerArgs) -> ExitCode {
    match args.command {
        GardenerCommand::Precheck(args) => report(precheck(args)),
        GardenerCommand::ReportStale(args) => report_stale_outcome(report_stale(args)),
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
    if args
        .store
        .as_ref()
        .is_some_and(|store| !store.is_absolute())
    {
        return Err(Failure::Invalid);
    }
    let client = read_client(
        &args.house,
        &args.repository,
        args.requester,
        args.credential,
        args.credential_file,
        args.gh,
    )?;
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

/// A read-only client for one repository of one house.
fn read_client(
    house: &HouseId,
    repository: &Repository,
    requester: ExternalRef,
    credential: CredentialId,
    credential_file: PathBuf,
    gh: PathBuf,
) -> Result<GitHubClient<GhCli>, Failure> {
    let reference = CredentialRef::new(house.clone(), credential, requester.clone());
    let scope = HouseScope::new(
        house.clone(),
        [repository.clone()],
        requester,
        reference.clone(),
        PostingBudget::new(0).map_err(invalid)?,
        std::iter::empty::<Permission>(),
    )
    .map_err(invalid)?;
    let credential = CredentialFile::new(reference, credential_file).map_err(invalid)?;
    let gh = GhCli::new(gh, credential).map_err(invalid)?;
    Ok(GitHubClient::new(scope, gh, ReadLimits::default()))
}

fn report_stale(args: ReportStaleArgs) -> Result<gardener::StaleReportOutcome, Failure> {
    if !args.store.is_absolute() {
        return Err(Failure::Invalid);
    }
    let issue = IssueNumber::new(args.issue).map_err(invalid)?;
    let body = Text::new(&args.body).map_err(invalid)?;
    let claimant = Claimant::scheduled(HolderId::new(REPORTER).map_err(invalid)?);
    let ttl = LeaseTtl::new(REPORT_LEASE).map_err(invalid)?;
    let client = read_client(
        &args.house,
        &args.repository,
        args.requester.clone(),
        args.credential.clone(),
        args.credential_file.clone(),
        args.gh.clone(),
    )?;
    let reference = CredentialRef::new(
        args.house.clone(),
        args.credential.clone(),
        args.requester.clone(),
    );
    let scope = HouseScope::new(
        args.house.clone(),
        [args.repository.clone()],
        args.requester,
        reference.clone(),
        PostingBudget::new(REPORT_POSTS).map_err(invalid)?,
        [Permission::PostComment],
    )
    .map_err(invalid)?;
    let credential = CredentialFile::new(reference, args.credential_file).map_err(invalid)?;
    let executor = GitHubExecutor::new(
        args.github_backend.clone(),
        scope,
        GhCli::new(args.gh, credential).map_err(invalid)?,
        ReadLimits::default(),
    );
    let reported = Failure::Reported;
    let config = HouseRegistry::new(&args.registry)
        .and_then(|registry| registry.load(&args.house))
        .map_err(|error| reported(error.into()))?;
    // The destination comes from house policy; the flag only picks an issue
    // in one of its declared destinations.
    if !config.posting_destinations.contains(&args.repository) {
        return Err(reported(IntegrationError::PermissionDenied.into()));
    }
    let grants = config.authority().map_err(|error| reported(error.into()))?;
    let store =
        HouseStore::open(args.store, args.house, StoreOptions::default()).map_err(reported)?;
    let pass = gardener::StaleReportPass {
        store: &store,
        client: &client,
        executor: &executor,
        grants: &grants,
        authority: Grant::repository(
            Permission::PostComment,
            args.repository.clone(),
            args.github_backend,
            args.credential,
        ),
        provenance: Provenance {
            kitchen: config.kitchen.clone(),
            house_guidance: config.guidance.clone(),
            repository_instructions: None,
        },
        claimant: &claimant,
        ttl,
        clock: &SystemClock,
    };
    gardener::report_stale(&pass, &args.repository, issue, body).map_err(reported)
}

fn report_stale_outcome(result: Result<gardener::StaleReportOutcome, Failure>) -> ExitCode {
    let (written, code) = match result {
        Ok(gardener::StaleReportOutcome::Recorded {
            receipt,
            later_activity: false,
            ..
        }) => (writeln!(io::stdout().lock(), "recorded {receipt}"), 0),
        Ok(gardener::StaleReportOutcome::Recorded {
            receipt,
            later_activity: true,
            ..
        }) => (
            writeln!(
                io::stdout().lock(),
                "recorded {receipt}; later activity stays unhandled"
            ),
            0,
        ),
        Ok(gardener::StaleReportOutcome::AlreadyHandled) => {
            (writeln!(io::stdout().lock(), "already handled"), 0)
        }
        Ok(gardener::StaleReportOutcome::NotPosted(_)) => (
            writeln!(
                io::stdout().lock(),
                "not posted; nothing recorded, run again later"
            ),
            1,
        ),
        Err(Failure::Invalid) => (
            writeln!(
                io::stderr().lock(),
                "error: invalid gardener report-stale input"
            ),
            2,
        ),
        Err(Failure::Read(error)) => (writeln!(io::stderr().lock(), "error: {error}"), 3),
        Err(Failure::Reported(error)) => (writeln!(io::stderr().lock(), "error: {error}"), 3),
    };
    if written.is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
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
        Err(Failure::Reported(error)) => (writeln!(io::stderr().lock(), "error: {error}"), 3),
    };
    // A result that could not be reported is an error, never idle.
    if written.is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}
