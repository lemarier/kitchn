//! [`WorkerBackend`] over the Orca orchestration CLI.
//!
//! | Kitchen | Orca |
//! | --- | --- |
//! | `LaunchWorker` | `task-create --task-title <launch marker>`, then `worker-start --task --agent [--model [--effort]]` |
//! | `MessageWorker` | `send --to dispatch:<id>` |
//! | `CancelWorker` | `worker-stop --dispatch` |
//! | `ReleaseResource` (workers only) | `worker-release --dispatch` |
//! | `observe_worker` | `worker-show --dispatch` |
//!
//! Orca's `--retry-request` accepts only request ids Orca issued, so a
//! Kitchen idempotency key cannot be an Orca request id. Launches are keyed by
//! an Orca Task instead: one Task per key, titled with [`launch_marker`]. A
//! Task that ever had a Dispatch is the launch for its key, whatever its
//! status: Orca returns a Task to `ready` when its worker's process exits,
//! and would dispatch it again, so the adapter asks for the Task's Dispatch
//! before starting anything. Resubmitting a launch key therefore returns the
//! original Dispatch without starting a second worker, and
//! [`OrcaBackend::lookup_launch`] finds it after a lost response. Lookup
//! and same-key idempotency are declared per effect kind, for launches,
//! cancels, releases, and schedule installs, state changes, and removals.
//! Messages and replies carry no key Orca records and a trial starts a new
//! run each time, so those kinds declare neither and `lookup` reports them
//! unsupported.
//!
//! A launch holds a per-key reservation (see [`OrcaConfig::runtime_dir`])
//! across listing, creating, and starting, so concurrent first submissions of
//! one key create one Task, not two. A launch with a requested branch
//! ([`Operation::LaunchWorker`]'s `branch`) passes the worktree name that
//! yields it under [`OrcaConfig::branch_prefix`], verifies the branch Orca
//! created, and stops the worker it just started when they differ. Before
//! the first start it refuses a branch an Orca worktree already has checked
//! out, and it reports a collision Orca still made as a [`BranchCollision`].

use std::{path::PathBuf, time::Duration};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    BackendId, CredentialId, HouseId,
    adapters::orca::{
        Invocation, OrcaError, OrcaRunner, RuntimeInfo, branch, reserve::Reservation, runtime, wire,
    },
    contracts::{
        BackendDescriptor, BackendUnavailable, BranchName, Clock, Effect, EffectExecutor,
        EffectFailure, EffectRequest, ExternalRef, IdempotencyKey, Lookup, MAX_INVENTORY_RESOURCES,
        MAX_RECEIPT_RESOURCES, NotAppliedReason, Operation, Receipt, ResourceKind,
        ResourceObservation, ResourceRef, SystemClock, Text, Timestamp, UncertainReason,
        WorkerBackend, WorkerOutcome, WorkerState, Workspace,
    },
    scheduling::{AgentFamily, SchedulePolicy},
    selection::{AgentSelection, EffortSupport, SelectionSupport},
};

/// Default deadline for one non-launch Orca call.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Default readiness wait Orca applies to a worker launch.
pub const DEFAULT_LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Default longest wait for a reservation another caller holds: longer than
/// one launch ([`DEFAULT_LAUNCH_TIMEOUT`] plus its margin) and the calls
/// around it.
pub const DEFAULT_RESERVATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Most Orca Tasks one Run listing may hold before it is refused as incomplete.
pub const MAX_RUN_TASKS: usize = 1000;

/// Extra time the subprocess gets beyond Orca's own launch timeout.
const LAUNCH_MARGIN: Duration = Duration::from_secs(30);

/// Orca error codes documented or observed as refusals before any effect.
const PREFLIGHT_REFUSALS: [&str; 5] = [
    "task_not_found",
    "task_not_startable",
    "inject_rejected",
    "dispatch_not_found",
    "no_active_sender_terminal",
];

/// Refused before any effect by a message or reply only: observed on 1.4.212
/// for a message to a stopped worker, whose mailbox will never be read and
/// where nothing is queued. `worker-start` raises the same code after it
/// created a worktree and terminal, so it is not a refusal there.
const MESSAGE_REFUSALS: [&str; 1] = ["dispatch_inactive"];

/// Worker states Orca treats as settled: `worker-stop` changes nothing for
/// them and answers `alreadySettled`.
const SETTLED_WORKER_STATES: [&str; 4] = ["stopped", "failed", "succeeded", "abandoned"];

/// Stop attempts for a worker a launch started on the wrong branch, before
/// the launch is left held with the worker reported as running.
const WRONG_BRANCH_STOP_ATTEMPTS: usize = 3;

/// Most worktrees of the repository the branch check reads; a longer listing
/// cannot show a branch is free, and the launch is refused.
pub const MAX_REPO_WORKTREES: usize = 1000;

/// What `worker-start` can launch: both agent families, any opaque model id
/// through `--model`, and `--effort` only together with `--model`.
pub const WORKER_SELECTION: SelectionSupport = SelectionSupport {
    families: &[AgentFamily::Claude, AgentFamily::Codex],
    model: true,
    effort: EffortSupport::WithModel,
};

/// Where and as whom one backend instance acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaConfig {
    /// Backend namespace: one Orca host and account.
    pub backend: BackendId,
    /// The only house this instance serves.
    pub house: HouseId,
    /// The house credential this instance acts under: the Orca host session
    /// it runs with. Requests naming another credential are refused, because
    /// Orca cannot switch credentials per call.
    pub credential: CredentialId,
    /// The Orca Run that owns this instance's workers and mailbox.
    pub run: ExternalRef,
    /// The live coordinator terminal handle Orca attributes calls to. A new
    /// coordinator uses its own handle after Kitchen records the adoption.
    pub coordinator: ExternalRef,
    /// Repository selector for isolated workspaces, such as `id:<repo-id>`.
    pub repo: ExternalRef,
    /// Base ref for isolated workspaces; Orca's repository default when unset.
    pub base_branch: Option<ExternalRef>,
    /// The prefix Orca's Git branch-prefix setting puts before the branch of
    /// every worktree it creates, as a branch name without the trailing `/`;
    /// `None` when the setting is off. Orca's CLI cannot override it, so a
    /// launch's requested branch must be this prefix and one more name (or a
    /// single name when there is no prefix); any other branch is refused
    /// before anything is created.
    pub branch_prefix: Option<BranchName>,
    /// The agent family a launch without an agent selection starts with.
    pub agent: AgentFamily,
    /// Deadline for each non-launch call.
    pub call_timeout: Duration,
    /// How long Orca waits for a launched worker to become ready.
    pub launch_timeout: Duration,
    /// House-scoped runtime storage, outside any Git checkout (a directory
    /// inside one is refused) and private to the Kitchen user, where reservation files serialize launches and
    /// schedule installs across callers and processes. Every caller acting on
    /// one house and Orca host must use the same directory.
    pub runtime_dir: PathBuf,
    /// Longest wait for a reservation another caller holds; it should outlast
    /// a launch. A wait that ends sent nothing to Orca and reports the effect
    /// as uncertain, because the holder may be about to apply it.
    pub reservation_timeout: Duration,
}

/// An Orca-backed [`WorkerBackend`] and schedule executor for one house.
#[derive(Debug)]
pub struct OrcaBackend<R> {
    config: OrcaConfig,
    descriptor: BackendDescriptor,
    runtime: RuntimeInfo,
    runner: R,
    schedule_policy: Option<SchedulePolicy>,
    now: fn() -> Timestamp,
}

fn system_now() -> Timestamp {
    SystemClock.now()
}

#[derive(Deserialize)]
struct TaskList {
    tasks: Vec<OrcaTask>,
}

#[derive(Deserialize)]
struct OrcaTask {
    id: String,
    #[serde(default)]
    task_title: Option<String>,
    status: String,
}

#[derive(Deserialize)]
struct DispatchShow {
    dispatch: Option<DispatchRow>,
}

#[derive(Deserialize)]
struct DispatchRow {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedTask {
    #[serde(default)]
    task: Option<DispatchRow>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct WorktreeShow {
    #[serde(default)]
    worktree: Option<WorktreeRow>,
}

#[derive(Deserialize)]
struct WorktreeRow {
    #[serde(default)]
    branch: Option<String>,
}

/// `worktree list`. Completeness is required, not defaulted: a listing that
/// does not say it is whole cannot show a branch is free.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeList {
    worktrees: Vec<WorktreeRow>,
    truncated: bool,
    host_scope: HostScope,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostScope {
    /// Hosts the listing does not cover; they may hold worktrees.
    omitted_host_ids: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartResult {
    dispatch_id: String,
}

#[derive(Deserialize)]
struct WireEffect {
    kind: String,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MutationResult {
    #[serde(default)]
    mutation: Option<MutationMeta>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    verdict: Option<String>,
    #[serde(default)]
    action: Option<String>,
    /// Set by `worker-stop` for a worker that had already settled; `state`
    /// then names the state it settled in.
    #[serde(default)]
    already_settled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MutationMeta {
    request_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerShow {
    #[serde(default)]
    pub(crate) dispatch: Option<ShowDispatch>,
    pub(crate) worker: ShowWorker,
    pub(crate) projection: Projection,
    #[serde(default)]
    pub(crate) observation: Option<ShowObservation>,
    #[serde(default)]
    pub(crate) terminal: Option<ShowTerminal>,
    #[serde(default)]
    pub(crate) terminal_resource: Option<TerminalResource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalResource {
    #[serde(default)]
    pub(crate) ownership_state: Option<String>,
    #[serde(default)]
    pub(crate) release_state: Option<String>,
    #[serde(default)]
    pub(crate) retained_reason: Option<String>,
}

impl TerminalResource {
    /// Whether a person took the worker's terminal over. Kitchen then sends
    /// it nothing without asking.
    pub(crate) fn person_owns(&self) -> bool {
        self.ownership_state.as_deref() == Some("user_owned")
            || self.retained_reason.as_deref() == Some("user_takeover")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ShowWorker {
    pub(crate) state: String,
    #[serde(default)]
    effects: Vec<WireEffect>,
    /// Orca's last error for the start, a string or an object.
    #[serde(default)]
    pub(crate) last_error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ShowDispatch {
    #[serde(default)]
    pub(crate) run_id: Option<String>,
    /// `dispatched` while the Dispatch is the live attempt.
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// Set when the Dispatch was fenced, for example by a stop.
    #[serde(default)]
    pub(crate) capability_revoked_at: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct ShowTerminal {
    #[serde(default)]
    branch: Option<String>,
    /// The last line of terminal output Orca previews.
    #[serde(default)]
    pub(crate) preview: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ShowObservation {
    #[serde(default)]
    pub(crate) agent_wait: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct Projection {
    pub(crate) outcome: String,
    pub(crate) liveness: Liveness,
    /// Absent from older hosts.
    #[serde(default)]
    pub(crate) stage: Option<ProjectionStage>,
}

/// Orca's projected stage: where the worker and its Dispatch are and what
/// the agent is doing. Values are kept as reported and mapped by the caller,
/// so an unrecognized one stays unknown.
#[derive(Deserialize)]
pub(crate) struct ProjectionStage {
    #[serde(default)]
    pub(crate) worker: Option<String>,
    #[serde(default)]
    pub(crate) dispatch: Option<String>,
    #[serde(default)]
    pub(crate) detail: Option<String>,
    #[serde(default)]
    pub(crate) activity: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct Liveness {
    pub(crate) verdict: String,
}

/// Prefix of every launch marker.
const MARKER_PREFIX: &str = "kitchen:";

/// The FNV-1a 128-bit hash of `house` and `name`, which are separated so
/// neither can run into the other. Stable across releases: the launch marker
/// persists in Orca Task titles.
pub(crate) fn key_digest(house: &HouseId, name: &str) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    house
        .as_str()
        .bytes()
        .chain(std::iter::once(b'\n'))
        .chain(name.bytes())
        .fold(OFFSET, |hash, byte| {
            (hash ^ u128::from(byte)).wrapping_mul(PRIME)
        })
}

/// The Orca Task title that marks the launch for `key` in `house`.
///
/// Orca truncates Task titles to 80 characters, and idempotency keys can be
/// longer, so the title is a fixed-length digest: `kitchen:` and the
/// FNV-1a 128-bit hash of the house and key in hex (40 characters). The
/// hash is stable across releases because Task titles persist in Orca.
/// Inventory reports this marker as a worker's owner.
#[must_use]
pub fn launch_marker(house: &HouseId, key: &IdempotencyKey) -> String {
    format!("{MARKER_PREFIX}{:032x}", key_digest(house, key.as_str()))
}

/// Check that a launch receipt names exactly the `requested` branch.
///
/// Orca prefixes the worktree name it is given (on the verified host,
/// `--name lemarier/x` became `lemarier/lemarier-x`), so the receipt records
/// the branch Orca actually created. Call this before the worker's first
/// push; on a mismatch, rename the branch or hand the task over.
///
/// # Errors
/// [`OrcaError::BranchMismatch`] naming both branches.
pub fn verify_branch(receipt: &Receipt, requested: &str) -> Result<(), OrcaError> {
    let actual = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Branch)
        .map(|branch| branch.handle.as_str());
    if actual == Some(requested) {
        Ok(())
    } else {
        Err(OrcaError::BranchMismatch {
            requested: requested.to_owned(),
            actual: actual.map(str::to_owned),
        })
    }
}

/// What Orca shows about a launch whose requested branch already existed,
/// so Orca created [`BranchCollision::branch`] instead.
///
/// It is positive ownership evidence for the stray resources: exactly one
/// Orca Task in the Run carries [`BranchCollision::owner`], this house's
/// launch marker for the key; the worker is that Task's Dispatch; and Orca's
/// effect record for the Dispatch names the worktrees it created. The
/// adapter stops the worker and closes its terminal, but never removes the
/// worktree or branch: that is the dishwasher's decision, after its own
/// preservation checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCollision {
    /// The branch the launch asked for.
    pub requested: BranchName,
    /// The branch Orca created instead: the requested one with a numeric
    /// suffix.
    pub branch: ResourceRef,
    /// The worker the launch started.
    pub worker: ResourceRef,
    /// The worktrees Orca records the worker's Dispatch as creating.
    pub worktrees: Vec<ResourceRef>,
    /// The launch marker on the owning Orca Task ([`launch_marker`]).
    pub owner: ExternalRef,
    /// Whether Orca's record shows the worker settled. Until it does, the
    /// worker could still push to [`BranchCollision::branch`].
    pub settled: bool,
    /// Whether Orca's record shows the worker's terminal released.
    pub terminal_released: bool,
}

/// Map Orca's worker projection to a [`WorkerState`].
///
/// Settlement comes only from Orca's accepted worker report (`succeeded`,
/// `failed`), an explicit stop (`stopped`), or a failed start. Readiness needs
/// a `live` fleet verdict; without one a started worker is `Starting`. Process age,
/// quiet terminals, `exited` liveness without a report (`finished_unverified`),
/// and unrecognized values are [`WorkerState::Unknown`].
pub(crate) fn worker_state(
    worker: &str,
    outcome: &str,
    liveness: &str,
    waiting: bool,
) -> WorkerState {
    match (outcome, worker) {
        // Orca 1.4.212 projects a stopped worker's outcome as `failed`; the
        // worker state keeps the stop.
        ("stopped", _) | (_, "stopped") => WorkerState::Settled(WorkerOutcome::Cancelled),
        ("succeeded", _) => WorkerState::Settled(WorkerOutcome::Succeeded),
        ("failed", _) | ("in_progress", "failed") => WorkerState::Settled(WorkerOutcome::Failed),
        ("in_progress", "starting") => WorkerState::Starting,
        ("in_progress", "ready") if waiting => WorkerState::AwaitingReply,
        ("in_progress", "ready") if liveness == "live" => WorkerState::Ready,
        // Accepted, but the agent is not yet shown to be running.
        ("in_progress", "ready") => WorkerState::Starting,
        _ => WorkerState::Unknown,
    }
}

/// Whether Orca's record shows the worker no longer runs under its
/// Dispatch: stopped, or settled by its own report, a failure (including a
/// process exit), or an abandonment. A cancel of such a worker has nothing
/// left to do.
fn is_settled(shown: &WorkerShow) -> bool {
    SETTLED_WORKER_STATES.contains(&shown.worker.state.as_str())
        || shown.projection.outcome == "stopped"
}

/// A person who took over a worker's terminal keeps it: that is neither
/// failure nor settlement, unless the worker itself reported one.
pub(crate) const fn with_takeover(state: WorkerState, person_owns: bool) -> WorkerState {
    match state {
        WorkerState::Settled(WorkerOutcome::Succeeded | WorkerOutcome::Failed) => state,
        WorkerState::Starting
        | WorkerState::Ready
        | WorkerState::AwaitingReply
        | WorkerState::UserTakeover
        | WorkerState::Settled(WorkerOutcome::Cancelled)
        | WorkerState::Missing
        | WorkerState::Unknown => {
            if person_owns {
                WorkerState::UserTakeover
            } else {
                state
            }
        }
    }
}

fn external(value: &str) -> Option<ExternalRef> {
    ExternalRef::new(value).ok()
}

fn not_applied() -> EffectFailure {
    EffectFailure::NotApplied(NotAppliedReason::Rejected)
}

fn response_lost() -> EffectFailure {
    EffectFailure::Uncertain(UncertainReason::ResponseLost)
}

fn call_failure(error: &OrcaError) -> EffectFailure {
    match error {
        OrcaError::Spawn(_) => not_applied(),
        OrcaError::Refused { code, .. } if PREFLIGHT_REFUSALS.contains(&code.as_str()) => {
            not_applied()
        }
        // Nothing was sent, but the holder may be about to apply the effect.
        OrcaError::Timeout | OrcaError::ReservationBusy => {
            EffectFailure::Uncertain(UncertainReason::Timeout)
        }
        OrcaError::Io(_) => EffectFailure::Uncertain(UncertainReason::Transport),
        OrcaError::ReservationRedirected
        | OrcaError::ReservationInsideRepository
        | OrcaError::ReservationUnavailable(_)
        | OrcaError::BranchUnobtainable { .. }
        | OrcaError::BranchTaken { .. }
        | OrcaError::ScheduleActive
        | OrcaError::ScheduleDiffers { .. }
        | OrcaError::ScheduleLimit(_) => not_applied(),
        OrcaError::Refused { .. }
        | OrcaError::OutputLimit { .. }
        | OrcaError::NoResult { .. }
        | OrcaError::Malformed { .. }
        | OrcaError::RuntimeNotReady
        | OrcaError::UnsupportedVersion { .. }
        | OrcaError::MissingRuntimeFeature(_)
        | OrcaError::ListingTooLong { .. }
        | OrcaError::NotKitchenOwned
        | OrcaError::ScheduleNotFound
        | OrcaError::DuplicateSchedules { .. }
        | OrcaError::BranchMismatch { .. }
        | OrcaError::WrongBranchRunning { .. }
        | OrcaError::TrialRequiresPaused
        | OrcaError::InstallUncertain
        | OrcaError::StateMismatch
        | OrcaError::Schedule(_)
        | OrcaError::Contract(_) => response_lost(),
    }
}

/// Setup failures need a change on this machine; anything else, including a
/// kind added to `io::ErrorKind` later, may pass on a retry.
fn reservation_failure(kind: std::io::ErrorKind) -> BackendUnavailable {
    use std::io::ErrorKind;
    match kind {
        ErrorKind::NotFound
        | ErrorKind::PermissionDenied
        | ErrorKind::InvalidInput
        | ErrorKind::InvalidData
        | ErrorKind::NotADirectory
        | ErrorKind::IsADirectory
        | ErrorKind::ReadOnlyFilesystem
        | ErrorKind::Unsupported => BackendUnavailable::LocalConfiguration,
        _ => BackendUnavailable::Transport,
    }
}

pub(crate) fn read_failure(error: &OrcaError) -> BackendUnavailable {
    match error {
        OrcaError::Timeout => BackendUnavailable::Timeout,
        // Kitchen's own reservation directory failed before any Orca request:
        // a retry cannot help until the local setup changes.
        OrcaError::ReservationInsideRepository | OrcaError::ReservationRedirected => {
            BackendUnavailable::LocalConfiguration
        }
        OrcaError::ReservationUnavailable(kind) => reservation_failure(*kind),
        OrcaError::Spawn(_)
        | OrcaError::OutputLimit { .. }
        | OrcaError::Io(_)
        | OrcaError::NoResult { .. }
        | OrcaError::Malformed { .. }
        | OrcaError::Refused { .. }
        | OrcaError::RuntimeNotReady
        | OrcaError::UnsupportedVersion { .. }
        | OrcaError::MissingRuntimeFeature(_)
        | OrcaError::ListingTooLong { .. }
        | OrcaError::NotKitchenOwned
        | OrcaError::ScheduleNotFound
        | OrcaError::DuplicateSchedules { .. }
        | OrcaError::BranchMismatch { .. }
        | OrcaError::WrongBranchRunning { .. }
        | OrcaError::TrialRequiresPaused
        | OrcaError::ScheduleActive
        | OrcaError::ScheduleDiffers { .. }
        | OrcaError::ScheduleLimit(_)
        | OrcaError::ReservationBusy
        | OrcaError::BranchUnobtainable { .. }
        | OrcaError::BranchTaken { .. }
        | OrcaError::InstallUncertain
        | OrcaError::StateMismatch
        | OrcaError::Schedule(_)
        | OrcaError::Contract(_) => BackendUnavailable::Transport,
    }
}

/// What a launch key's Orca Task shows.
enum TaskLaunch {
    /// No Task carries the key.
    None,
    /// The Task exists and was never dispatched.
    Undispatched(String),
    /// The Task was dispatched; this is its receipt.
    Dispatched(Receipt),
    /// The Task's state cannot be read as either.
    Unclear,
}

impl<R: OrcaRunner> OrcaBackend<R> {
    /// Probe the runtime and build a backend for `config`.
    ///
    /// # Errors
    /// Returns the probe's [`OrcaError`] when the runtime is down, its version
    /// is unsupported, or a required feature is missing.
    pub fn connect(config: OrcaConfig, runner: R) -> Result<Self, OrcaError> {
        let runtime = runtime::probe(&runner, config.call_timeout)?;
        let descriptor = BackendDescriptor {
            backend: config.backend.clone(),
            house: config.house.clone(),
            worker_selection: Some(WORKER_SELECTION),
            capabilities: runtime::capabilities(),
        };
        Ok(Self {
            config,
            descriptor,
            runtime,
            runner,
            schedule_policy: None,
            now: system_now,
        })
    }

    /// Enforce the house's schedule limits: installs that break them are
    /// refused with [`OrcaError::ScheduleLimit`] before anything is created.
    #[must_use]
    pub fn with_schedule_policy(mut self, policy: SchedulePolicy) -> Self {
        self.schedule_policy = Some(policy);
        self
    }

    /// Read the current time from `now` instead of the host clock. Activation
    /// checks judge usage in the window containing it.
    #[must_use]
    pub fn with_clock(mut self, now: fn() -> Timestamp) -> Self {
        self.now = now;
        self
    }

    /// The current time for budget windows.
    pub(crate) fn now(&self) -> Timestamp {
        (self.now)()
    }

    /// The schedule limits installs are checked against, if any.
    #[must_use]
    pub const fn schedule_policy(&self) -> Option<&SchedulePolicy> {
        self.schedule_policy.as_ref()
    }

    /// What the runtime probe established.
    #[must_use]
    pub const fn runtime(&self) -> &RuntimeInfo {
        &self.runtime
    }

    /// This instance's configuration.
    #[must_use]
    pub const fn config(&self) -> &OrcaConfig {
        &self.config
    }

    pub(crate) fn call(&self, args: Vec<String>, deadline: Duration) -> Result<Value, OrcaError> {
        let output = self.runner.run(&Invocation::new(args, deadline))?;
        wire::result(&output)
    }

    fn resource(&self, kind: ResourceKind, handle: ExternalRef) -> ResourceRef {
        ResourceRef {
            kind,
            backend: self.config.backend.clone(),
            handle,
        }
    }

    /// The Orca Dispatch id behind a worker reference, when it belongs to this backend.
    pub(crate) fn dispatch_of<'a>(&self, worker: &'a ResourceRef) -> Option<&'a str> {
        (worker.kind == ResourceKind::Worker && worker.backend == self.config.backend)
            .then(|| worker.handle.as_str())
    }

    fn dispatch_or_reject<'a>(&self, worker: &'a ResourceRef) -> Result<&'a str, EffectFailure> {
        self.dispatch_of(worker).ok_or_else(not_applied)
    }

    /// The Orca Task title that carries a launch key.
    fn task_title(&self, key: &IdempotencyKey) -> String {
        launch_marker(&self.config.house, key)
    }

    /// A launch receipt, built from Orca's record of the Dispatch so a later
    /// lookup derives the same one: the Task id as reference, and the worker,
    /// branch, and worktrees Orca created for it. An existing workspace is not
    /// listed: Orca's records do not name it, and the task already owns it.
    fn launch_receipt(&self, task: &str, dispatch: &str, shown: &WorkerShow) -> Option<Receipt> {
        let mut resources = vec![self.resource(ResourceKind::Worker, external(dispatch)?)];
        let worktrees: Vec<ExternalRef> = shown
            .worker
            .effects
            .iter()
            .filter(|effect| effect.kind == "worktree")
            .filter_map(|effect| effect.id.as_deref().and_then(external))
            .collect();
        // The branch Orca actually created: Orca prefixes the requested
        // name, so the receipt names the real branch rather than a guess.
        // Orca reports it with the agent's terminal only, so when that
        // terminal can no longer be shown it is read from the worktree record.
        let branch = shown
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.branch.clone())
            .or_else(|| worktrees.first().and_then(|id| self.worktree_branch(id)));
        resources.extend(
            branch
                .as_deref()
                // Orca reports the full ref, such as `refs/heads/lemarier/x`.
                .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch))
                .and_then(external)
                .map(|branch| self.resource(ResourceKind::Branch, branch)),
        );
        resources.extend(
            worktrees
                .into_iter()
                .map(|handle| self.resource(ResourceKind::Worktree, handle)),
        );
        resources.truncate(MAX_RECEIPT_RESOURCES);
        Receipt::new(external(task)?, resources, Vec::new()).ok()
    }

    /// The branch Orca's worktree record names, or `None` when the worktree
    /// is gone or cannot be read. A receipt without a branch confirms no
    /// requested branch, so a failed read holds such a launch rather than
    /// accepting it.
    fn worktree_branch(&self, worktree: &ExternalRef) -> Option<String> {
        let args = wire::Args::command(&["worktree", "show"])
            .value("worktree", &format!("id:{worktree}"))
            .json();
        let shown: WorktreeShow = wire::typed(
            self.call(args, self.config.call_timeout).ok()?,
            "worktree show",
        )
        .ok()?;
        shown.worktree.and_then(|worktree| worktree.branch)
    }

    fn run_tasks(&self) -> Result<Vec<OrcaTask>, OrcaError> {
        let args = wire::Args::command(&["orchestration", "task-list"])
            .value("run", self.config.run.as_str())
            .switch("brief")
            .json();
        let list: TaskList = wire::typed(self.call(args, self.config.call_timeout)?, "task list")?;
        if list.tasks.len() > MAX_RUN_TASKS {
            return Err(OrcaError::ListingTooLong {
                limit: MAX_RUN_TASKS,
            });
        }
        Ok(list.tasks)
    }

    /// Launch markers by Orca Task id, for Tasks Kitchen created.
    pub(crate) fn launch_owners(
        &self,
    ) -> Result<std::collections::BTreeMap<String, ExternalRef>, OrcaError> {
        Ok(self
            .run_tasks()?
            .into_iter()
            .filter_map(|task| {
                let title = task.task_title.as_deref()?;
                title
                    .starts_with(MARKER_PREFIX)
                    .then(|| external(title))
                    .flatten()
                    .map(|marker| (task.id, marker))
            })
            .collect())
    }

    /// Read what Orca holds for a launch key.
    ///
    /// The Task's newest Dispatch decides, not its status: a Task whose
    /// worker process exited is `ready` again but still names that Dispatch.
    fn task_launch(&self, key: &IdempotencyKey) -> Result<TaskLaunch, OrcaError> {
        let title = self.task_title(key);
        let mut matching = self
            .run_tasks()?
            .into_iter()
            .filter(|task| task.task_title.as_deref() == Some(title.as_str()));
        let task = match (matching.next(), matching.next()) {
            (None, _) => return Ok(TaskLaunch::None),
            (Some(task), None) => task,
            // Several Tasks for one key would be an unexplained duplicate.
            (Some(_), Some(_)) => return Ok(TaskLaunch::Unclear),
        };
        let args = wire::Args::command(&["orchestration", "dispatch-show"])
            .value("task", &task.id)
            .json();
        let found: DispatchShow =
            wire::typed(self.call(args, self.config.call_timeout)?, "dispatch show")?;
        let Some(dispatch) = found.dispatch else {
            return Ok(match task.status.as_str() {
                "pending" | "ready" => TaskLaunch::Undispatched(task.id),
                // Past `ready` with no Dispatch is not a state Orca produces.
                _ => TaskLaunch::Unclear,
            });
        };
        let Some(shown) = self.show(&dispatch.id)? else {
            return Ok(TaskLaunch::Unclear);
        };
        Ok(self
            .launch_receipt(&task.id, &dispatch.id, &shown)
            .map_or(TaskLaunch::Unclear, TaskLaunch::Dispatched))
    }

    fn create_task(&self, key: &IdempotencyKey, brief: &Text) -> Result<String, EffectFailure> {
        let args = wire::Args::command(&["orchestration", "task-create"])
            .value("spec", brief.as_str())
            .value("task-title", &self.task_title(key))
            .value("run", self.config.run.as_str())
            .value("from", self.config.coordinator.as_str())
            .json();
        let value = self
            .call(args, self.config.call_timeout)
            .map_err(|error| call_failure(&error))?;
        let created: CreatedTask =
            wire::typed(value, "task create").map_err(|_| response_lost())?;
        created
            .task
            .map(|task| task.id)
            .or(created.task_id)
            .or(created.id)
            .ok_or_else(response_lost)
    }

    /// Refuse a workspace this backend cannot start a worker in.
    ///
    /// An existing workspace must be a worktree of this backend. Whether the
    /// task owns it is decided before the request reaches a backend, by the
    /// state store. `launch` calls this before a Task exists, so a refusal
    /// leaves nothing in the Run; `start` repeats it as a guard.
    fn check_workspace(&self, workspace: &Workspace) -> Result<(), EffectFailure> {
        match workspace {
            Workspace::Isolated => Ok(()),
            Workspace::Existing(resource)
                if resource.kind == ResourceKind::Worktree
                    && resource.backend == self.config.backend =>
            {
                Ok(())
            }
            Workspace::Existing(_) => Err(not_applied()),
        }
    }

    fn start(
        &self,
        key: &IdempotencyKey,
        task: &str,
        workspace: &Workspace,
        name: Option<&str>,
        agent: Option<&AgentSelection>,
    ) -> Result<Receipt, EffectFailure> {
        self.check_workspace(workspace)?;
        let timeout_ms = u64::try_from(self.config.launch_timeout.as_millis()).unwrap_or(u64::MAX);
        let mut args = wire::Args::command(&["orchestration", "worker-start"])
            .value("task", task)
            .value("run", self.config.run.as_str())
            .value("from", self.config.coordinator.as_str())
            .value(
                "agent",
                agent
                    .map_or(self.config.agent, |agent| agent.agent)
                    .as_str(),
            )
            .value("timeout-ms", &timeout_ms.to_string());
        if let Some(model) = agent.and_then(|agent| agent.model.as_ref()) {
            args = args.value("model", model.as_str());
        }
        if let Some(effort) = agent.and_then(|agent| agent.effort.as_ref()) {
            args = args.value("effort", effort.as_str());
        }
        args = match workspace {
            Workspace::Isolated => {
                let args = args
                    .value("worktree", "new-top-level")
                    .value("repo", self.config.repo.as_str())
                    .value(
                        "name",
                        &name.map_or_else(|| format!("kitchen-{task}"), str::to_owned),
                    );
                match &self.config.base_branch {
                    Some(base) => args.value("base-branch", base.as_str()),
                    None => args,
                }
            }
            Workspace::Existing(resource) => {
                args.value("worktree", &format!("id:{}", resource.handle))
            }
        };
        let deadline = self.config.launch_timeout.saturating_add(LAUNCH_MARGIN);
        match self.call(args.json(), deadline) {
            Ok(value) => {
                let start: StartResult =
                    wire::typed(value, "worker start").map_err(|_| response_lost())?;
                // Any recorded Dispatch means the launch applied, including a
                // failed or unknown start: its resources exist, and
                // `observe_worker` reports how far it got. The receipt comes
                // from Orca's record so a lookup derives the same one; if that
                // read fails, the launch is uncertain, not refused.
                let shown = self
                    .show(&start.dispatch_id)
                    .ok()
                    .flatten()
                    .ok_or_else(response_lost)?;
                self.launch_receipt(task, &start.dispatch_id, &shown)
                    .ok_or_else(response_lost)
            }
            // The Task was already dispatched, by an earlier submission of
            // this key: return that Dispatch rather than a refusal.
            Err(OrcaError::Refused { code, .. }) if code == "task_not_startable" => {
                match self.task_launch(key) {
                    Ok(TaskLaunch::Dispatched(receipt)) => Ok(receipt),
                    Ok(TaskLaunch::Undispatched(_)) => Err(not_applied()),
                    Ok(TaskLaunch::None | TaskLaunch::Unclear) | Err(_) => Err(response_lost()),
                }
            }
            Err(error) => Err(call_failure(&error)),
        }
    }

    /// Launch, or return the launch this key already made.
    ///
    /// The reservation for the key is held across listing, creating, and
    /// starting: without it two first submissions could both list, both find
    /// no Task, and both create one. A launch that reached Orca as a
    /// dispatched Task settles the reservation, since no later submission can
    /// repeat it.
    fn launch(
        &self,
        key: &IdempotencyKey,
        workspace: &Workspace,
        brief: &Text,
        branch: Option<&BranchName>,
        agent: Option<&AgentSelection>,
    ) -> Result<Receipt, EffectFailure> {
        // A workspace, branch, or agent selection no launch can honor is
        // refused before anything exists: no reservation, no Task. A
        // selection Orca cannot provide is never replaced by another.
        self.check_workspace(workspace)?;
        if let Some(agent) = agent
            && let Some(gap) = WORKER_SELECTION.gaps(agent).first()
        {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                gap.capability(),
            )));
        }
        // The branch a new worktree is to be created on.
        let new_branch = match workspace {
            Workspace::Isolated => branch,
            Workspace::Existing(_) => None,
        };
        let name = new_branch
            .map(|branch| branch::worktree_name(self.config.branch_prefix.as_ref(), branch))
            .transpose()
            .map_err(|error| call_failure(&error))?;
        let mut reservation = self
            .reserve(format!(
                "launch-{:032x}",
                key_digest(&self.config.house, key.as_str())
            ))
            .map_err(|error| call_failure(&error))?;
        let receipt = self.launch_reserved(key, workspace, brief, name, new_branch, agent)?;
        reservation.settle();
        let Some(branch) = branch else {
            return Ok(receipt);
        };
        if verify_branch(&receipt, branch.as_str()).is_ok() {
            return Ok(receipt);
        }
        // The worker may run on another branch and could push to it. Stop
        // it, retrying a bounded number of times. The launch is held as
        // uncertain either way, because a worker, its worktree, and its
        // branch exist. A stop that never took effect leaves the launch
        // unresolved: lookup reports it unknown, each resubmission within the
        // store's budget stops again, and `verify_launch_branch` reports
        // [`OrcaError::WrongBranchRunning`] until the worker is stopped.
        if let Some(worker) = receipt
            .created()
            .iter()
            .find(|resource| resource.kind == ResourceKind::Worker)
            && (0..WRONG_BRANCH_STOP_ATTEMPTS).any(|_| self.cancel(worker).is_ok())
        {
            self.release_stopped(worker);
        }
        Err(response_lost())
    }

    /// Close the terminal of a worker stopped for running on the wrong
    /// branch. Orca archives its output and keeps its worktree and branch.
    /// The outcome is not needed here: a failed release is tried again by the
    /// next submission, and [`OrcaBackend::launch_collision`] reads back
    /// whether the terminal was released.
    fn release_stopped(&self, worker: &ResourceRef) {
        let released = self
            .dispatch_of(worker)
            .and_then(|dispatch| self.show(dispatch).ok().flatten())
            .and_then(|shown| shown.terminal_resource)
            .is_some_and(|terminal| terminal.release_state.as_deref() == Some("released"));
        if !released {
            let _ = self.release(worker);
        }
    }

    fn launch_reserved(
        &self,
        key: &IdempotencyKey,
        workspace: &Workspace,
        brief: &Text,
        name: Option<String>,
        new_branch: Option<&BranchName>,
        agent: Option<&AgentSelection>,
    ) -> Result<Receipt, EffectFailure> {
        let task = match self
            .task_launch(key)
            .map_err(|error| call_failure(&error))?
        {
            TaskLaunch::Dispatched(receipt) => return Ok(receipt),
            TaskLaunch::Unclear => return Err(response_lost()),
            TaskLaunch::Undispatched(task) => Some(task),
            TaskLaunch::None => None,
        };
        // Only a first start is checked: once dispatched, the launch's own
        // worktree holds the branch. Only reads happened so far, so any
        // failure of the check is a refusal.
        if let Some(branch) = new_branch {
            self.check_branch_free(branch).map_err(|_| not_applied())?;
        }
        let task = match task {
            Some(task) => task,
            None => self.create_task(key, brief)?,
        };
        self.start(key, &task, workspace, name.as_deref(), agent)
    }

    /// Check that no Orca worktree of the repository has `branch` checked
    /// out. A launch requesting such a branch would get `<branch>-2` from
    /// Orca, so it runs this check before creating anything and refuses on
    /// any error; call it to learn why. To work on an existing branch, launch
    /// in its worktree ([`Workspace::Existing`]).
    ///
    /// A branch that exists in Git without an Orca worktree is not visible
    /// here; the check after the launch still catches that collision, and
    /// [`OrcaBackend::launch_collision`] reports it.
    ///
    /// # Errors
    /// [`OrcaError::BranchTaken`] when a worktree has the branch, or the
    /// listing is truncated, holds [`MAX_REPO_WORKTREES`] or more rows, or
    /// leaves out a host. Other errors when Orca cannot be read.
    pub fn check_branch_free(&self, branch: &BranchName) -> Result<(), OrcaError> {
        let args = wire::Args::command(&["worktree", "list"])
            .value("repo", self.config.repo.as_str())
            .value("limit", &MAX_REPO_WORKTREES.to_string())
            .json();
        let list: WorktreeList =
            wire::typed(self.call(args, self.config.call_timeout)?, "worktree list")?;
        let taken = || OrcaError::BranchTaken {
            requested: branch.as_str().to_owned(),
        };
        if list.truncated
            || list.worktrees.len() >= MAX_REPO_WORKTREES
            || !list.host_scope.omitted_host_ids.is_empty()
        {
            return Err(taken());
        }
        if list.worktrees.iter().any(|worktree| {
            worktree
                .branch
                .as_deref()
                .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name))
                == Some(branch.as_str())
        }) {
            return Err(taken());
        }
        Ok(())
    }

    /// Reserve `stem` under this instance's runtime directory.
    pub(crate) fn reserve(&self, stem: String) -> Result<Reservation, OrcaError> {
        Reservation::acquire(
            &self.config.runtime_dir,
            &stem,
            self.config.reservation_timeout,
        )
    }

    /// A receipt for a mutation on one Dispatch, referenced by that Dispatch
    /// so a later lookup derives the same receipt.
    fn dispatch_receipt(&self, worker: &ResourceRef) -> Result<Receipt, EffectFailure> {
        Receipt::new(worker.handle.clone(), Vec::new(), vec![worker.clone()])
            .map_err(|_| response_lost())
    }

    fn mutation(&self, args: Vec<String>) -> Result<MutationResult, EffectFailure> {
        let value = self
            .call(args, self.config.call_timeout)
            .map_err(|error| call_failure(&error))?;
        wire::typed(value, "mutation").map_err(|_| response_lost())
    }

    /// Confirm a Dispatch is one this backend may act on: it belongs to this
    /// backend's Run, and no person took its terminal over. A person's
    /// terminal gets no messages and is not stopped without asking. Nothing
    /// was sent when this refuses.
    fn check_target(&self, dispatch: &str) -> Result<WorkerShow, EffectFailure> {
        let Ok(Some(shown)) = self.show(dispatch) else {
            return Err(not_applied());
        };
        let in_run = shown
            .dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.run_id.as_deref())
            == Some(self.config.run.as_str());
        let person_owned = shown
            .terminal_resource
            .as_ref()
            .is_some_and(TerminalResource::person_owns);
        if in_run && !person_owned {
            Ok(shown)
        } else {
            Err(not_applied())
        }
    }

    /// A message or reply mutation: `dispatch_inactive` is a refusal here.
    fn message_mutation(&self, args: Vec<String>) -> Result<MutationResult, EffectFailure> {
        match self.call(args, self.config.call_timeout) {
            Ok(value) => wire::typed(value, "mutation").map_err(|_| response_lost()),
            Err(OrcaError::Refused { code, .. }) if MESSAGE_REFUSALS.contains(&code.as_str()) => {
                Err(not_applied())
            }
            Err(error) => Err(call_failure(&error)),
        }
    }

    fn message(&self, worker: &ResourceRef, body: &Text) -> Result<Receipt, EffectFailure> {
        let dispatch = self.dispatch_or_reject(worker)?;
        self.check_target(dispatch)?;
        let args = wire::Args::command(&["orchestration", "send"])
            .value("run", self.config.run.as_str())
            .value("from", self.config.coordinator.as_str())
            .value("to", &format!("dispatch:{dispatch}"))
            .value("type", "status")
            .value("subject", "Kitchen coordinator")
            .value("body", body.as_str())
            .json();
        self.message_receipt(worker, &self.message_mutation(args)?)
    }

    fn reply(
        &self,
        worker: &ResourceRef,
        question: &ExternalRef,
        body: &Text,
    ) -> Result<Receipt, EffectFailure> {
        let dispatch = self.dispatch_or_reject(worker)?;
        self.check_target(dispatch)?;
        let args = wire::Args::command(&["orchestration", "reply"])
            .value("id", question.as_str())
            .value("body", body.as_str())
            .value("run", self.config.run.as_str())
            .value("from", self.config.coordinator.as_str())
            .json();
        self.message_receipt(worker, &self.message_mutation(args)?)
    }

    /// Messages are referenced by Orca's request id for the send.
    fn message_receipt(
        &self,
        worker: &ResourceRef,
        result: &MutationResult,
    ) -> Result<Receipt, EffectFailure> {
        let reference = result
            .mutation
            .as_ref()
            .and_then(|mutation| external(&mutation.request_id))
            .ok_or_else(response_lost)?;
        Receipt::new(reference, Vec::new(), vec![worker.clone()]).map_err(|_| response_lost())
    }

    /// Stop a worker. A worker that already settled (stopped, reported,
    /// failed, exited, or abandoned) has nothing left to cancel, so the
    /// cancel is applied without changing anything.
    fn cancel(&self, worker: &ResourceRef) -> Result<Receipt, EffectFailure> {
        let dispatch = self.dispatch_or_reject(worker)?;
        // A repeat is answered from Orca's record, so cancel is idempotent
        // without depending on how Orca answers a second stop.
        if is_settled(&self.check_target(dispatch)?) {
            return self.dispatch_receipt(worker);
        }
        let args = wire::Args::command(&["orchestration", "worker-stop"])
            .value("dispatch", dispatch)
            .json();
        let result = self.mutation(args)?;
        let settled = result.already_settled == Some(true);
        match result.state.or(result.verdict).as_deref() {
            Some("stopped") => self.dispatch_receipt(worker),
            // The worker settled on its own before the stop reached it.
            Some(state) if settled && SETTLED_WORKER_STATES.contains(&state) => {
                self.dispatch_receipt(worker)
            }
            // `stop_unknown`, `stopping`, and anything new: not proven stopped.
            _ => Err(response_lost()),
        }
    }

    fn release(&self, resource: &ResourceRef) -> Result<Receipt, EffectFailure> {
        // Orca releases worker terminals only; worktrees and consoles have no
        // ownership-aware release, so they are refused before acting.
        let dispatch = self.dispatch_or_reject(resource)?;
        // Orca itself retains a person's terminal, so only the Run is checked.
        match self.show(dispatch) {
            Ok(Some(shown))
                if shown
                    .dispatch
                    .as_ref()
                    .and_then(|dispatch| dispatch.run_id.as_deref())
                    == Some(self.config.run.as_str()) => {}
            Ok(_) | Err(_) => return Err(not_applied()),
        }
        let args = wire::Args::command(&["orchestration", "worker-release"])
            .value("dispatch", dispatch)
            .json();
        let result = self.mutation(args)?;
        // Orca 1.4.212 reports the release verdict in `state`.
        match result.state.or(result.action).as_deref() {
            Some("released" | "already_released") => self.dispatch_receipt(resource),
            // Retained resources were deliberately left alone, including a
            // terminal a person took over: not a failure, and nothing closed.
            Some("retained") => Err(not_applied()),
            _ => Err(response_lost()),
        }
    }

    pub(crate) fn show(&self, dispatch: &str) -> Result<Option<WorkerShow>, OrcaError> {
        let args = wire::Args::command(&["orchestration", "worker-show"])
            .value("dispatch", dispatch)
            .json();
        match self.call(args, self.config.call_timeout) {
            Ok(value) => wire::typed(value, "worker show").map(Some),
            Err(OrcaError::Refused { code, .. }) if code == "dispatch_not_found" => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Find the Dispatch a launch key started, after a lost response.
    ///
    /// Reports [`Lookup::Applied`] with the same receipt `execute` returned,
    /// and [`Lookup::Unknown`] otherwise: a missing Task is not proof, since
    /// a lost `task-create` may still land.
    ///
    /// # Errors
    /// [`BackendUnavailable`] when Orca cannot be queried.
    pub fn lookup_launch(&self, key: &IdempotencyKey) -> Result<Lookup, BackendUnavailable> {
        match self
            .task_launch(key)
            .map_err(|error| read_failure(&error))?
        {
            TaskLaunch::Dispatched(receipt) => Ok(Lookup::Applied(receipt)),
            TaskLaunch::None | TaskLaunch::Undispatched(_) | TaskLaunch::Unclear => {
                Ok(Lookup::Unknown)
            }
        }
    }

    /// Check that the launch `key` started is on the `requested` branch.
    ///
    /// This is how a caller learns why a launch with a requested branch was
    /// held: [`EffectExecutor::execute`] reports only that the launch is
    /// uncertain.
    ///
    /// # Errors
    /// [`OrcaError::WrongBranchRunning`] when the launched worker is on
    /// another branch and Orca's record does not show it settled: it could
    /// still push, and needs a stop or a person. [`OrcaError::BranchMismatch`]
    /// naming both branches otherwise. `actual` is `None` when Orca records no
    /// branch or the key has no dispatched launch: the request cannot be
    /// confirmed either way. Other errors when Orca cannot be read.
    pub fn verify_launch_branch(
        &self,
        key: &IdempotencyKey,
        requested: &BranchName,
    ) -> Result<(), OrcaError> {
        match self.task_launch(key)? {
            TaskLaunch::Dispatched(receipt) => {
                let Err(mismatch) = verify_branch(&receipt, requested.as_str()) else {
                    return Ok(());
                };
                let Some(dispatch) = receipt
                    .created()
                    .iter()
                    .find(|resource| resource.kind == ResourceKind::Worker)
                    .and_then(|worker| self.dispatch_of(worker))
                else {
                    return Err(mismatch);
                };
                // Only a record showing the worker settled clears it.
                if self.show(dispatch)?.is_some_and(|shown| is_settled(&shown)) {
                    return Err(mismatch);
                }
                let OrcaError::BranchMismatch { requested, actual } = mismatch else {
                    return Err(mismatch);
                };
                Err(OrcaError::WrongBranchRunning {
                    requested,
                    actual,
                    worker: dispatch.to_owned(),
                })
            }
            TaskLaunch::None | TaskLaunch::Undispatched(_) | TaskLaunch::Unclear => {
                Err(OrcaError::BranchMismatch {
                    requested: requested.to_string(),
                    actual: None,
                })
            }
        }
    }

    /// The collision a launch with a requested branch ran into, if it did.
    ///
    /// Returns `Some` when the key's launch was dispatched and Orca created
    /// the requested branch with a numeric suffix because the branch already
    /// existed, with the evidence that this launch owns the stray worker,
    /// worktree, and branch. Returns `None` for no dispatched launch, the
    /// requested branch itself, or another mismatch
    /// ([`OrcaBackend::verify_launch_branch`] reports those).
    ///
    /// # Errors
    /// [`OrcaError`] when Orca cannot be read.
    pub fn launch_collision(
        &self,
        key: &IdempotencyKey,
        requested: &BranchName,
    ) -> Result<Option<BranchCollision>, OrcaError> {
        let TaskLaunch::Dispatched(receipt) = self.task_launch(key)? else {
            return Ok(None);
        };
        let created = receipt.created();
        let (Some(stray), Some(worker)) = (
            created
                .iter()
                .find(|resource| resource.kind == ResourceKind::Branch),
            created
                .iter()
                .find(|resource| resource.kind == ResourceKind::Worker),
        ) else {
            return Ok(None);
        };
        if !branch::is_collision(requested.as_str(), stray.handle.as_str()) {
            return Ok(None);
        }
        let shown = match self.dispatch_of(worker) {
            Some(dispatch) => self.show(dispatch)?,
            None => None,
        };
        Ok(Some(BranchCollision {
            requested: requested.clone(),
            branch: stray.clone(),
            worker: worker.clone(),
            worktrees: created
                .iter()
                .filter(|resource| resource.kind == ResourceKind::Worktree)
                .cloned()
                .collect(),
            owner: ExternalRef::new(&self.task_title(key))?,
            settled: shown.as_ref().is_some_and(is_settled),
            terminal_released: shown
                .and_then(|shown| shown.terminal_resource)
                .is_some_and(|terminal| terminal.release_state.as_deref() == Some("released")),
        }))
    }

    /// Look up a persisted request, operation by operation.
    ///
    /// Launches are found through their Task, stops and releases through the
    /// Dispatch's recorded state. Messages and replies carry no key Orca
    /// records, so they stay [`Lookup::Unknown`]. [`EffectExecutor::lookup`]
    /// delegates here for the kinds the backend declares lookup for.
    ///
    /// # Errors
    /// [`BackendUnavailable`] when Orca cannot be queried or the effect is
    /// not one this backend performs.
    pub fn resolve(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let operation = match request.effect() {
            Effect::Worker(operation) => operation,
            Effect::Schedule(effect) => return self.resolve_schedule(effect),
            Effect::GitHub(_) | Effect::Roger(_) => {
                return Err(BackendUnavailable::Unsupported(
                    request.effect().required_capability(),
                ));
            }
        };
        match operation {
            Operation::LaunchWorker { branch, .. } => {
                let found = self.lookup_launch(request.key())?;
                // A launch on the wrong branch was held, not accepted.
                Ok(match (&found, branch) {
                    (Lookup::Applied(receipt), Some(branch))
                        if verify_branch(receipt, branch.as_str()).is_err() =>
                    {
                        Lookup::Unknown
                    }
                    _ => found,
                })
            }
            Operation::MessageWorker { .. } | Operation::ReplyToWorker { .. } => {
                Ok(Lookup::Unknown)
            }
            Operation::CancelWorker { worker }
            | Operation::ReleaseResource { resource: worker } => {
                let Some(dispatch) = self.dispatch_of(worker) else {
                    return Ok(Lookup::Unknown);
                };
                let shown = self.show(dispatch).map_err(|error| read_failure(&error))?;
                let applied = shown.is_some_and(|shown| match operation {
                    Operation::CancelWorker { .. } => is_settled(&shown),
                    _ => shown.terminal_resource.is_some_and(|terminal| {
                        terminal.release_state.as_deref() == Some("released")
                    }),
                });
                if applied {
                    self.dispatch_receipt(worker)
                        .map(Lookup::Applied)
                        .map_err(|_| BackendUnavailable::Transport)
                } else {
                    Ok(Lookup::Unknown)
                }
            }
        }
    }
}

impl<R: OrcaRunner> EffectExecutor for OrcaBackend<R> {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        if request.house() != &self.descriptor.house {
            return Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse));
        }
        if request.backend() != &self.descriptor.backend {
            return Err(EffectFailure::NotApplied(NotAppliedReason::ForeignBackend));
        }
        let capability = request.effect().required_capability();
        if !self.descriptor.capabilities.supports(capability) {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                capability,
            )));
        }
        // Orca acts under one host session and cannot switch per call.
        if request.credential() != &self.config.credential {
            return Err(not_applied());
        }
        let operation = match request.effect() {
            Effect::Worker(operation) => operation,
            Effect::Schedule(effect) => return self.execute_schedule(effect),
            // Refused above: no GitHub or Roger capability is declared.
            Effect::GitHub(_) | Effect::Roger(_) => {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                    capability,
                )));
            }
        };
        match operation {
            Operation::LaunchWorker {
                role: _,
                workspace,
                brief,
                branch,
                agent,
            } => self.launch(
                request.key(),
                workspace,
                brief,
                branch.as_ref(),
                agent.as_ref(),
            ),
            Operation::MessageWorker { worker, body } => self.message(worker, body),
            Operation::ReplyToWorker {
                worker,
                question,
                body,
            } => self.reply(worker, question, body),
            Operation::CancelWorker { worker } => self.cancel(worker),
            Operation::ReleaseResource { resource } => self.release(resource),
        }
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        if !self.descriptor.supports_lookup(request.effect()) {
            return Err(BackendUnavailable::Unsupported(
                request.effect().kind().lookup_capability(),
            ));
        }
        self.resolve(request)
    }
}

impl<R: OrcaRunner> WorkerBackend for OrcaBackend<R> {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        let Some(dispatch) = self.dispatch_of(worker) else {
            return Ok(WorkerState::Missing);
        };
        let Some(shown) = self.show(dispatch).map_err(|error| read_failure(&error))? else {
            return Ok(WorkerState::Missing);
        };
        let waiting = shown
            .observation
            .as_ref()
            .and_then(|observation| observation.agent_wait.as_ref())
            .is_some_and(|wait| !wait.is_null());
        let person_owns = shown
            .terminal_resource
            .as_ref()
            .is_some_and(TerminalResource::person_owns);
        Ok(with_takeover(
            worker_state(
                &shown.worker.state,
                &shown.projection.outcome,
                &shown.projection.liveness.verdict,
                waiting,
            ),
            person_owns,
        ))
    }

    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        let records = self.worker_records().map_err(|error| match error {
            OrcaError::ListingTooLong { .. } => BackendUnavailable::LimitExceeded,
            other => read_failure(&other),
        })?;
        if records.len() > MAX_INVENTORY_RESOURCES {
            return Err(BackendUnavailable::LimitExceeded);
        }
        Ok(records
            .into_iter()
            .map(|record| ResourceObservation {
                resource: record.worker,
                owner: record.owner,
                liveness: record.liveness,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_needs_a_report_or_an_explicit_stop() {
        let settled = [
            ("succeeded", "succeeded", WorkerOutcome::Succeeded),
            ("failed", "failed", WorkerOutcome::Failed),
            ("in_progress", "failed", WorkerOutcome::Failed),
            ("stopped", "stopped", WorkerOutcome::Cancelled),
            // As Orca 1.4.212 reports a stop.
            ("failed", "stopped", WorkerOutcome::Cancelled),
        ];
        for (outcome, worker, expected) in settled {
            assert_eq!(
                worker_state(worker, outcome, "exited", false),
                WorkerState::Settled(expected)
            );
        }
        // An exited agent without a report, an abandoned fence, and values
        // Kitchen does not know are never settlement.
        for (outcome, worker) in [
            ("finished_unverified", "ready"),
            ("outcome_unknown", "start_unknown"),
            ("abandoned", "abandoned"),
            ("in_progress", "stopping"),
            ("something_new", "ready"),
        ] {
            assert_eq!(
                worker_state(worker, outcome, "exited", false),
                WorkerState::Unknown,
                "{outcome}/{worker}"
            );
        }
    }

    #[test]
    fn launch_markers_fit_orca_titles_and_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let house = HouseId::new(&"h".repeat(64))?;
        let long = IdempotencyKey::from_ref(ExternalRef::new(&"k".repeat(256))?);
        let marker = launch_marker(&house, &long);
        assert_eq!(marker.len(), 40, "within Orca's 80-character title limit");
        // FNV-1a 128 of "home\nkey-1", computed independently.
        let home = HouseId::new("home")?;
        let key = IdempotencyKey::from_ref(ExternalRef::new("key-1")?);
        assert_eq!(
            launch_marker(&home, &key),
            "kitchen:c5c59ffffb49efd808dcc9593d194f39"
        );
        let other_house = HouseId::new("away")?;
        assert_ne!(
            launch_marker(&other_house, &key),
            launch_marker(&home, &key)
        );
        Ok(())
    }

    #[test]
    fn a_local_reservation_refusal_is_local_configuration() {
        assert_eq!(
            read_failure(&OrcaError::ReservationInsideRepository),
            BackendUnavailable::LocalConfiguration,
            "the guard runs before any Orca request"
        );
        assert_eq!(
            read_failure(&OrcaError::ReservationRedirected),
            BackendUnavailable::LocalConfiguration
        );
        assert_eq!(
            read_failure(&OrcaError::ReservationUnavailable(
                std::io::ErrorKind::PermissionDenied
            )),
            BackendUnavailable::LocalConfiguration
        );
        // Another holder can release a busy key, so it stays retryable.
        assert_eq!(
            read_failure(&OrcaError::ReservationBusy),
            BackendUnavailable::Transport
        );
        assert_eq!(
            read_failure(&OrcaError::Io(std::io::ErrorKind::BrokenPipe)),
            BackendUnavailable::Transport
        );
        assert_eq!(
            read_failure(&OrcaError::Timeout),
            BackendUnavailable::Timeout
        );
    }

    #[test]
    fn a_transient_reservation_failure_stays_retryable() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::OutOfMemory,
        ] {
            assert_eq!(
                read_failure(&OrcaError::ReservationUnavailable(kind)),
                BackendUnavailable::Transport,
                "{kind:?} can pass on a retry"
            );
        }
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidInput,
            ErrorKind::NotADirectory,
            ErrorKind::ReadOnlyFilesystem,
        ] {
            assert_eq!(
                read_failure(&OrcaError::ReservationUnavailable(kind)),
                BackendUnavailable::LocalConfiguration,
                "{kind:?} needs a local setup change"
            );
        }
    }

    #[test]
    fn a_failed_reservation_reads_as_local_configuration() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        // A runtime "directory" that is a file cannot hold the reservation.
        let file = root.path().join("runtime");
        std::fs::write(&file, b"")?;
        let error =
            crate::adapters::orca::reserve::Reservation::acquire(&file, "k", Duration::ZERO)
                .err()
                .ok_or("a reservation was taken under a file")?;
        assert!(matches!(error, OrcaError::ReservationUnavailable(_)));
        assert_eq!(read_failure(&error), BackendUnavailable::LocalConfiguration);
        Ok(())
    }

    #[test]
    fn readiness_needs_live_evidence() {
        assert_eq!(
            worker_state("ready", "in_progress", "live", false),
            WorkerState::Ready
        );
        assert_eq!(
            worker_state("ready", "in_progress", "unverifiable", false),
            WorkerState::Starting
        );
        assert_eq!(
            worker_state("ready", "in_progress", "unverifiable", true),
            WorkerState::AwaitingReply
        );
        assert_eq!(
            worker_state("starting", "in_progress", "live", false),
            WorkerState::Starting
        );
    }
}
