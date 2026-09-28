//! Interactive entrypoints behind the `/kitchn` skill: `work`, `pr`,
//! `issue new`, `issue refine`, and `hand-back`.
//!
//! Every command resolves the house from the checkout through the registry
//! and refuses to run when it is missing or ambiguous. The person names
//! themselves with `--holder`; claims are taken as an interactive claimant
//! on the same durable tasks scheduled pickup and repair use. The commands
//! cannot tell a person from a script, so scheduled jobs must not run them.
//! Nothing here posts to the forge or launches a worker: `issue` commands
//! print the preview and digest a person approves, and `work`/`pr` print the
//! plan the session follows.

use std::{fmt::Write as _, path::PathBuf, time::Duration};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    HolderId, TaskId,
    adoption::{HouseRegistry, RepositoryMatch, decode, encode},
    contracts::{
        Claimant, Clock, CommitId, ExecutorKind, IssueNumber, LeaseTtl, Repository, RetryPolicy,
        SystemClock, TaskAuthority,
    },
    house::HouseError,
    state::{HouseStore, Lease, StoreOptions},
    workflows::{
        coordination::REQUIRED_WORKER_CAPABILITIES,
        interactive::{
            DraftTarget, Entrypoint, ExecutionMode, HouseResolution, IssueDraft, IssueFacts,
            MAX_ORCA_OUTPUT_BYTES, Orchestrator, PrFacts, PrIntent, PrPlan, PrRequest,
            ResolvedHouse, ReviewState, Unavailable, WorkPlan, WorkRequest, draft_preview,
            execution_mode, hand_back, pull_request, work,
        },
        pickup::{IssueRef, TaskTemplate},
        repair::{Mergeability, PullRequestState, PullRequestView},
    },
};
use serde::{Deserialize, Serialize};

/// Attempts an interactively created task may use.
const ATTEMPTS: u32 = 3;
/// Longest an interactively created task keeps retrying.
const RETRY_BUDGET: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Interactive entrypoints. Registered flat on the top-level command.
#[derive(Subcommand)]
pub enum InteractiveCommand {
    /// Coordinate or implement one issue as the person present.
    Work(WorkArgs),
    /// Review, follow up, repair, or judge one pull request at its exact head.
    Pr(PrArgs),
    /// Draft or refine an issue. Prints the preview to approve; posts nothing.
    Issue(IssueArgs),
    /// Give your interactive claim back so the next claimant adopts it.
    HandBack(HandBackArgs),
}

/// Where the session runs.
#[derive(Args)]
struct Session {
    /// The house registry.
    #[arg(long)]
    registry: PathBuf,
    /// A path inside the checkout whose remotes identify the repository.
    #[arg(long, default_value = ".")]
    repository_path: PathBuf,
    /// The commit whose repository instructions are pinned, such as the
    /// output of `git rev-parse HEAD`.
    #[arg(long)]
    revision: CommitId,
    /// Captured `orca status --json`. Without it and --orca-worktree the
    /// session works as a single agent.
    #[arg(long, requires = "orca_worktree")]
    orca_status: Option<PathBuf>,
    /// Captured `orca worktree current --json` from this worktree.
    #[arg(long, requires = "orca_status")]
    orca_worktree: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

/// The durable claim the person takes.
#[derive(Args)]
struct Claim {
    /// The house's initialized state store.
    #[arg(long)]
    store: PathBuf,
    /// The person present.
    #[arg(long)]
    holder: HolderId,
    /// Claim lease, in minutes.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=1440))]
    lease_minutes: u64,
    /// Take over a claim whose lease expired without a hand-back.
    #[arg(long)]
    take_over: bool,
}

#[derive(Args)]
pub struct WorkArgs {
    /// The issue number.
    issue: u64,
    /// Issue facts (JSON): status, sub-issues, and whether it has
    /// independent parts.
    #[arg(long)]
    facts: PathBuf,
    #[command(flatten)]
    session: Session,
    #[command(flatten)]
    claim: Claim,
}

#[derive(Clone, Copy, ValueEnum)]
enum IntentArg {
    Review,
    FollowUp,
    Repair,
    Gate,
}

impl From<IntentArg> for PrIntent {
    fn from(intent: IntentArg) -> Self {
        match intent {
            IntentArg::Review => Self::Review,
            IntentArg::FollowUp => Self::FollowUp,
            IntentArg::Repair => Self::Repair,
            IntentArg::Gate => Self::Gate,
        }
    }
}

#[derive(Args)]
pub struct PrArgs {
    /// The pull request number.
    number: u64,
    /// Pull request facts (JSON) read at one head.
    #[arg(long)]
    facts: PathBuf,
    /// What to do; routed from the facts when omitted.
    #[arg(long = "as", value_enum)]
    intent: Option<IntentArg>,
    /// Review-fix and repair rounds the house allows per pull request.
    #[arg(long, default_value_t = 3)]
    fix_rounds: u8,
    #[command(flatten)]
    session: Session,
    #[command(flatten)]
    claim: Claim,
}

#[derive(Args)]
pub struct IssueArgs {
    #[command(subcommand)]
    command: IssueCommand,
}

#[derive(Subcommand)]
enum IssueCommand {
    /// Preview a new issue drafted with the person.
    New {
        /// The draft (JSON) whose target is a new issue.
        #[arg(long)]
        draft: PathBuf,
        #[command(flatten)]
        session: Session,
    },
    /// Preview a refinement of an existing issue.
    Refine {
        /// The issue number.
        issue: u64,
        /// The draft (JSON) whose target is this issue.
        #[arg(long)]
        draft: PathBuf,
        #[command(flatten)]
        session: Session,
    },
}

#[derive(Args)]
pub struct HandBackArgs {
    /// The task id `work` or `pr` printed.
    task: TaskId,
    /// The house registry.
    #[arg(long)]
    registry: PathBuf,
    /// A path inside the checkout whose remotes identify the repository.
    #[arg(long, default_value = ".")]
    repository_path: PathBuf,
    /// The house's initialized state store.
    #[arg(long)]
    store: PathBuf,
    /// The person who holds the claim.
    #[arg(long)]
    holder: HolderId,
}

/// Run an interactive entrypoint and return its output and whether it
/// succeeded.
pub fn run(command: InteractiveCommand) -> Result<(String, bool), kitchen::Error> {
    match command {
        InteractiveCommand::Work(args) => run_work(args),
        InteractiveCommand::Pr(args) => run_pr(args),
        InteractiveCommand::Issue(args) => run_issue(args),
        InteractiveCommand::HandBack(args) => run_hand_back(args),
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf, HouseError> {
    // Keep path redirects for the library to reject; only make a relative
    // path absolute.
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn json_text(value: &impl Serialize) -> Result<String, kitchen::Error> {
    Ok(String::from_utf8(encode(value)?).map_err(|_| HouseError::InvalidInput)?)
}

/// Read an Orca capture, bounded.
fn capture(path: &PathBuf) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_ORCA_OUTPUT_BYTES).ok()?.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_ORCA_OUTPUT_BYTES).then_some(bytes)
}

/// A resolved session: the bound house and the execution mode.
struct Opened {
    house: ResolvedHouse,
    mode: ExecutionMode,
    json: bool,
}

/// Resolve the house, or explain the one-time setup the person must do.
fn open(session: Session) -> Result<Result<Opened, String>, kitchen::Error> {
    let registry = HouseRegistry::new(absolute(session.registry)?)?;
    let start = absolute(session.repository_path)?;
    let house = match kitchen::workflows::interactive::resolve_house(
        &registry,
        &start,
        session.revision,
    )? {
        HouseResolution::Ready(house) => *house,
        HouseResolution::NeedsSetup { repository, house } => {
            return Ok(Err(format!(
                "Repository {repository} is claimed by house {house} but not set up.\nNext: ask the person whether to bind it, then run kitchen house setup --registry '{}' --repository {repository} --house {house}",
                registry.root().display()
            )));
        }
    };
    let orchestrator = match (&session.orca_status, &session.orca_worktree) {
        (Some(status), Some(worktree)) => match (capture(status), capture(worktree)) {
            (Some(status), Some(worktree)) => Orchestrator::from_orca(&status, &worktree),
            _ => Orchestrator::Unavailable(Unavailable::Unreadable),
        },
        _ => Orchestrator::Unavailable(Unavailable::NotObserved),
    };
    let mode = execution_mode(&house.binding.repository, &orchestrator);
    Ok(Ok(Opened {
        house,
        mode,
        json: session.json,
    }))
}

/// The task template scheduled pickup uses for this house: the house's
/// standing grants, pinned instructions, and worker capability needs. An
/// interactive claim never acts on the standing grants; they matter only if
/// the task is later handed to a scheduled run.
fn template(house: &ResolvedHouse) -> Result<TaskTemplate, kitchen::Error> {
    let grants = house.config.authority()?;
    Ok(TaskTemplate {
        authority: TaskAuthority::delegate(&grants, house.config.grants.iter().cloned())?,
        retry: RetryPolicy::new(ATTEMPTS, RETRY_BUDGET)?,
        provenance: house.instructions.provenance.clone(),
        requires: kitchen::contracts::CapabilityRequirements::new()
            .with(ExecutorKind::Worker, REQUIRED_WORKER_CAPABILITIES),
        agents: house.config.agents.clone(),
    })
}

fn ttl(claim: &Claim) -> Result<LeaseTtl, kitchen::Error> {
    Ok(LeaseTtl::new(Duration::from_secs(
        claim.lease_minutes.saturating_mul(60),
    ))?)
}

fn store(claim: &Claim, house: &ResolvedHouse) -> Result<HouseStore, kitchen::Error> {
    HouseStore::open(
        absolute(claim.store.clone())?,
        house.binding.house.clone(),
        StoreOptions::default(),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Pins<'a> {
    kitchen: &'a CommitId,
    house_guidance: &'a CommitId,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_instructions: Option<&'a CommitId>,
    entrypoint: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a, P> {
    entrypoint: Entrypoint,
    house: &'a kitchen::HouseId,
    repository: &'a Repository,
    instructions: Pins<'a>,
    mode: &'a ExecutionMode,
    plan: &'a P,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease: Option<&'a Lease>,
}

fn header(opened: &Opened, entrypoint: Entrypoint) -> String {
    let house = &opened.house;
    let pins = &house.instructions.provenance;
    let mut out = format!(
        "{entrypoint} in {} for house {}.\nHouse rules: read {} (kitchen {}, guidance {}",
        house.binding.repository,
        house.binding.house,
        house.instructions.entrypoint.display(),
        pins.kitchen,
        pins.house_guidance,
    );
    if let Some(revision) = &pins.repository_instructions {
        let _ = write!(out, ", repository {revision}");
    }
    out.push_str(").\n");
    out.push_str(&match &opened.mode {
        ExecutionMode::FanOut => "Orchestrator: Orca; workers may fan out.".to_owned(),
        ExecutionMode::Solo { reason } => {
            format!("Single agent: fan-out unavailable ({reason}).")
        }
    });
    out
}

fn report<P: Serialize>(
    opened: &Opened,
    entrypoint: Entrypoint,
    plan: &P,
    lease: Option<&Lease>,
    text: impl FnOnce() -> String,
) -> Result<String, kitchen::Error> {
    if opened.json {
        let house = &opened.house;
        let pins = &house.instructions.provenance;
        json_text(&Report {
            entrypoint,
            house: &house.binding.house,
            repository: &house.binding.repository,
            instructions: Pins {
                kitchen: &pins.kitchen,
                house_guidance: &pins.house_guidance,
                repository_instructions: pins.repository_instructions.as_ref(),
                entrypoint: house.instructions.entrypoint.display().to_string(),
            },
            mode: &opened.mode,
            plan,
            lease,
        })
    } else {
        Ok(format!("{}\n{}", header(opened, entrypoint), text()))
    }
}

fn numbers(values: &[IssueNumber]) -> String {
    if values.is_empty() {
        return "none".to_owned();
    }
    values
        .iter()
        .map(|number| format!("#{}", number.get()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn run_work(args: WorkArgs) -> Result<(String, bool), kitchen::Error> {
    let number = IssueNumber::new(args.issue)?;
    let facts: IssueFacts = decode(&args.facts)?;
    let opened = match open(args.session)? {
        Ok(opened) => opened,
        Err(setup) => return Ok((setup, false)),
    };
    let store = store(&args.claim, &opened.house)?;
    let template = template(&opened.house)?;
    let issue = IssueRef {
        repository: opened.house.binding.repository.clone(),
        number,
    };
    let claimant = Claimant::interactive(args.claim.holder.clone());
    let (plan, lease) = work(&WorkRequest {
        store: &store,
        template: &template,
        issue: &issue,
        facts: &facts,
        claimant: &claimant,
        ttl: ttl(&args.claim)?,
        now: SystemClock.now(),
        mode: &opened.mode,
        take_over: args.claim.take_over,
    })?;
    let proceed = !matches!(plan, WorkPlan::Skipped { .. });
    let output = report(
        &opened,
        Entrypoint::Work { issue: number },
        &plan,
        lease.as_ref(),
        || render_work(&plan),
    )?;
    Ok((output, proceed))
}

fn claimed(task: &TaskId, adopted: bool) -> String {
    let how = if adopted {
        "Adopted handed-back work"
    } else {
        "Claimed"
    };
    format!("{how} as task {task}; scheduled runs skip it until you hand it back.")
}

fn render_work(plan: &WorkPlan) -> String {
    match plan {
        WorkPlan::Idle { reason } => format!("Nothing to do: {}.", idle(*reason)),
        WorkPlan::Skipped { refusal } => refused(refusal),
        WorkPlan::Coordinate {
            task,
            adopted,
            ready,
            waiting,
            fan_out,
        } => format!(
            "{}\nReady sub-issues: {}. Waiting on blockers: {}.\n{}",
            claimed(task, *adopted),
            numbers(ready),
            numbers(waiting),
            if *fan_out {
                "Ask the person before starting a worker for each ready sub-issue."
            } else {
                "Work the ready sub-issues one at a time in this session."
            }
        ),
        WorkPlan::ProposeSplit { task, adopted, .. } => format!(
            "{}\nThe issue has independent parts and no sub-issues. Propose a split to the person. Creating sub-issues needs the decomposition workflow (#47), which this build lacks; with the person's approval, implement it here instead.",
            claimed(task, *adopted)
        ),
        WorkPlan::Implement { task, adopted } => {
            format!(
                "{}\nImplement the issue in this session.",
                claimed(task, *adopted)
            )
        }
    }
}

fn idle(reason: kitchen::workflows::interactive::Idle) -> &'static str {
    use kitchen::workflows::interactive::Idle;
    match reason {
        Idle::IssueClosed => "the issue is closed",
        Idle::SubIssuesDone => "every sub-issue is closed; closing the parent is the person's call",
        Idle::Merged => "the pull request merged",
        Idle::Closed => "the pull request is closed",
        Idle::NothingToRepair => "the pull request merges cleanly",
    }
}

fn refused(refusal: &kitchen::workflows::interactive::ClaimRefusal) -> String {
    use kitchen::workflows::interactive::ClaimRefusal;
    match refusal {
        ClaimRefusal::Held { trigger } => format!(
            "Skipped: held by a {trigger} claim. Ask its owner to hand it back; do not work on it."
        ),
        ClaimRefusal::OwnerUncertain => "Skipped: the previous claim expired without a hand-back. Only the person may choose --take-over.".to_owned(),
        ClaimRefusal::Settled { settlement } => format!("Skipped: the task already settled ({settlement:?})."),
    }
}

/// Pull request facts as the session reads them at one head.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrFactsFile {
    state: StateArg,
    head: CommitId,
    head_branch: String,
    base_branch: String,
    mergeability: MergeabilityArg,
    review: ReviewState,
    #[serde(default)]
    rounds_used: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StateArg {
    Open,
    Closed,
    Merged,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum MergeabilityArg {
    Clean,
    Conflicting,
    Behind,
    Unknown,
}

fn run_pr(args: PrArgs) -> Result<(String, bool), kitchen::Error> {
    let number = IssueNumber::new(args.number)?;
    let file: PrFactsFile = decode(&args.facts)?;
    let facts = PrFacts {
        view: PullRequestView {
            number,
            state: match file.state {
                StateArg::Open => PullRequestState::Open,
                StateArg::Closed => PullRequestState::Closed,
                StateArg::Merged => PullRequestState::Merged,
            },
            head: file.head,
            head_branch: file.head_branch,
            base_branch: file.base_branch,
            mergeability: match file.mergeability {
                MergeabilityArg::Clean => Mergeability::Clean,
                MergeabilityArg::Conflicting => Mergeability::Conflicting,
                MergeabilityArg::Behind => Mergeability::Behind,
                MergeabilityArg::Unknown => Mergeability::Unknown,
            },
        },
        review: file.review,
        rounds_used: file.rounds_used,
    };
    let opened = match open(args.session)? {
        Ok(opened) => opened,
        Err(setup) => return Ok((setup, false)),
    };
    let store = store(&args.claim, &opened.house)?;
    let template = template(&opened.house)?;
    let claimant = Claimant::interactive(args.claim.holder.clone());
    let (plan, lease) = pull_request(&PrRequest {
        store: &store,
        template: &template,
        repository: &opened.house.binding.repository,
        facts: &facts,
        intent: args.intent.map(PrIntent::from),
        fix_rounds: args.fix_rounds,
        claimant: &claimant,
        ttl: ttl(&args.claim)?,
        now: SystemClock.now(),
        take_over: args.claim.take_over,
    })?;
    let proceed = !matches!(
        plan,
        PrPlan::Skipped { .. } | PrPlan::BudgetExhausted { .. }
    );
    let output = report(
        &opened,
        Entrypoint::PullRequest { number },
        &plan,
        lease.as_ref(),
        || render_pr(&plan),
    )?;
    Ok((output, proceed))
}

fn render_pr(plan: &PrPlan) -> String {
    match plan {
        PrPlan::Idle { reason } => format!("Nothing to do: {}.", idle(*reason)),
        PrPlan::Recheck { head } => format!(
            "Mergeability at {head} is not computed yet. Read the pull request again before acting."
        ),
        PrPlan::Skipped { refusal } => refused(refusal),
        PrPlan::BudgetExhausted { rounds_used } => format!(
            "The house's fix-round budget is spent ({rounds_used} rounds). The person decides what happens next."
        ),
        PrPlan::Review { head } => format!(
            "Review head {head}. Post findings only with the person's approval of each comment."
        ),
        PrPlan::Gate { head } => format!(
            "Evaluate the gate at head {head}. The verdict is void if the head or base moves; merging stays the person's decision."
        ),
        PrPlan::FollowUp { head, task, round } => format!(
            "Address review feedback on {head} as round {round} (task {task}). Scheduled repair skips this pull request until you hand the task back."
        ),
        PrPlan::Repair { head, task, round } => format!(
            "Resolve conflicts on {head} as round {round} (task {task}). Scheduled repair skips this pull request until you hand the task back."
        ),
    }
}

fn run_issue(args: IssueArgs) -> Result<(String, bool), kitchen::Error> {
    let (entrypoint, draft, session) = match args.command {
        IssueCommand::New { draft, session } => (Entrypoint::IssueNew, draft, session),
        IssueCommand::Refine {
            issue,
            draft,
            session,
        } => (
            Entrypoint::IssueRefine {
                issue: IssueNumber::new(issue)?,
            },
            draft,
            session,
        ),
    };
    let draft: IssueDraft = decode(&draft)?;
    let matches = match (&entrypoint, &draft.target) {
        (Entrypoint::IssueNew, DraftTarget::New { .. }) => true,
        (Entrypoint::IssueRefine { issue }, DraftTarget::Refine { issue: target, .. }) => {
            issue == target
        }
        _ => false,
    };
    if !matches {
        return Err(
            kitchen::workflows::interactive::InteractiveError::InvalidDraft("target").into(),
        );
    }
    let preview = draft_preview(&draft)?;
    let opened = match open(session)? {
        Ok(opened) => opened,
        Err(setup) => return Ok((setup, false)),
    };
    if !preview
        .repository
        .as_str()
        .eq_ignore_ascii_case(opened.house.binding.repository.as_str())
    {
        return Err(HouseError::HouseSelection.into());
    }
    let ready = preview.ready();
    let output = report(&opened, entrypoint, &preview, None, || {
        format!("{}\nNothing was posted.", preview.render())
    })?;
    Ok((output, ready))
}

fn run_hand_back(args: HandBackArgs) -> Result<(String, bool), kitchen::Error> {
    use kitchen::workflows::interactive::HandBack;
    let registry = HouseRegistry::new(absolute(args.registry)?)?;
    let house = match registry.resolve_repository(&absolute(args.repository_path)?)? {
        RepositoryMatch::Bound(binding) => binding.house,
        RepositoryMatch::Unbound { .. } => return Err(HouseError::HouseSelection.into()),
    };
    let store = HouseStore::open(absolute(args.store)?, house, StoreOptions::default())?;
    let result = hand_back(&store, &args.task, &args.holder, SystemClock.now())?;
    let released = result == HandBack::Released;
    let text = if released {
        format!(
            "Handed back task {}; the next claimant adopts it.",
            args.task
        )
    } else {
        format!(
            "You hold no interactive claim on task {}; nothing changed.",
            args.task
        )
    };
    Ok((text, released))
}
