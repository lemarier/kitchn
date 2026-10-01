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
//! Identity-bearing isolated launches create an Orca worktree and set its
//! worktree-local Git identity before `worker-start --worktree id:<id>`.
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
//! ([`Operation::LaunchWorker`]'s `branch`) passes its final component as the
//! worktree name, waits for Orca to report the prefixed branch, and stops
//! the worker when it reports a different branch or none within the bound. Before
//! the first start it refuses a branch an Orca worktree already has checked
//! out, and it reports a collision Orca still made as a [`BranchCollision`].

use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

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
        WorkerBackend, WorkerOutcome, WorkerState, Workspace, WorktreeStatus,
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
const BRANCH_POLLS: usize = 16;
const BRANCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const BRANCH_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

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

/// What `automations create` can launch: both agent families through
/// `--provider`, with no model or effort option (checked on Orca 1.4.216).
/// A schedule whose selection names either is refused, not run on the
/// agent's default model.
pub const SCHEDULE_SELECTION: SelectionSupport = SelectionSupport {
    families: &[AgentFamily::Claude, AgentFamily::Codex],
    model: false,
    effort: EffortSupport::Unsupported,
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
    /// The prefix Orca's Git branch-prefix setting puts before the worktree
    /// name, without the trailing `/`; `None` when the setting is off. The
    /// receipt must report this prefix and the requested name's final
    /// component for a new worktree.
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
    writer_identity: Option<(String, String)>,
    writer_identity_required: bool,
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
    #[serde(default)]
    spec: Option<String>,
    status: String,
    #[serde(default)]
    assignee_handle: Option<String>,
    #[serde(default)]
    dispatch_id: Option<String>,
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

#[derive(Clone, Deserialize)]
struct WorktreeRow {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default, rename = "isMainWorktree")]
    is_main_worktree: bool,
}

/// `worktree list`. Completeness is required, not defaulted: a listing that
/// does not say it is whole cannot show a branch is free.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeList {
    worktrees: Vec<WorktreeRow>,
    /// Every worktree of the repository, returned or not.
    total_count: usize,
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

/// Prefix of the last line of a Task spec that records the branch the launch
/// requested. Orca's Task list returns the spec, so the request is durable
/// with the Task that owns the launch.
const REQUESTED_BRANCH_PREFIX: &str = "kitchen-requested-branch: ";

/// The Task spec for `brief`: the brief and a final line recording the
/// requested branch, empty when none. The line is always ours, so a brief
/// that ends with such a line cannot forge a request.
fn task_spec(brief: &Text, requested: Option<&BranchName>) -> String {
    format!(
        "{}\n\n{REQUESTED_BRANCH_PREFIX}{}",
        brief.as_str(),
        requested.map_or("", BranchName::as_str)
    )
}

/// The branch a Task spec records as requested: the value on its last line,
/// when that line has the recording prefix and a value.
fn requested_in_spec(spec: &str) -> Option<&str> {
    spec.lines()
        .next_back()?
        .strip_prefix(REQUESTED_BRANCH_PREFIX)
        .filter(|branch| !branch.is_empty())
}

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
/// The receipt records the branch Orca actually created. An absent branch is
/// unconfirmed, not evidence of a different branch.
///
/// # Errors
/// [`OrcaError::BranchMismatch`] naming both branches, or
/// [`OrcaError::BranchUnconfirmed`] when no branch was reported.
pub fn verify_branch(receipt: &Receipt, requested: &str) -> Result<(), OrcaError> {
    let actual = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Branch)
        .map(|branch| branch.handle.as_str());
    match actual {
        Some(actual) if actual == requested => Ok(()),
        Some(actual) => Err(OrcaError::BranchMismatch {
            requested: requested.to_owned(),
            actual: Some(actual.to_owned()),
        }),
        None => Err(OrcaError::BranchUnconfirmed {
            requested: requested.to_owned(),
        }),
    }
}

fn receipt_branch(receipt: &Receipt) -> Option<BranchName> {
    receipt
        .created()
        .iter()
        .chain(receipt.touched())
        .find(|resource| resource.kind == ResourceKind::Branch)
        .and_then(|resource| BranchName::new(resource.handle.as_str()).ok())
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
        // A waiting worker counts as awaiting a reply only while its process
        // is shown live; otherwise it has no positive liveness evidence.
        ("in_progress", "ready") if waiting && liveness == "live" => WorkerState::AwaitingReply,
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

/// A dispatch that cannot supply a live worker for this launch attempt.
fn dispatch_ended(shown: &WorkerShow) -> bool {
    matches!(
        worker_state(&shown.worker.state, &shown.projection.outcome, "", false,),
        WorkerState::Settled(WorkerOutcome::Failed | WorkerOutcome::Cancelled)
    )
}

fn branch_prefix_differs(prefix: Option<&BranchName>, actual: &BranchName) -> bool {
    let actual_prefix = actual.as_str().rsplit_once('/').map(|(prefix, _)| prefix);
    actual_prefix != prefix.map(BranchName::as_str)
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
        OrcaError::LaunchEnded { .. } => EffectFailure::Uncertain(UncertainReason::DispatchEnded),
        OrcaError::ReservationRedirected
        | OrcaError::ReservationInsideRepository
        | OrcaError::ReservationUnavailable(_)
        | OrcaError::BranchUnobtainable { .. }
        | OrcaError::BranchTaken { .. }
        | OrcaError::BranchUnverified
        | OrcaError::ScheduleActive
        | OrcaError::ScheduleDiffers { .. }
        | OrcaError::ScheduleLimit(_)
        | OrcaError::Selection(_) => not_applied(),
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
        | OrcaError::BranchUnconfirmed { .. }
        | OrcaError::BranchUnconfirmedRunning { .. }
        | OrcaError::WrongBranchRunning { .. }
        | OrcaError::TrialRequiresPaused
        | OrcaError::ScheduleRequirementsUnknown
        | OrcaError::ScheduleRequirementsMismatch
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
        | OrcaError::LaunchEnded { .. }
        | OrcaError::BranchUnconfirmed { .. }
        | OrcaError::BranchUnconfirmedRunning { .. }
        | OrcaError::WrongBranchRunning { .. }
        | OrcaError::TrialRequiresPaused
        | OrcaError::ScheduleRequirementsUnknown
        | OrcaError::ScheduleRequirementsMismatch
        | OrcaError::ScheduleActive
        | OrcaError::ScheduleDiffers { .. }
        | OrcaError::ScheduleLimit(_)
        | OrcaError::Selection(_)
        | OrcaError::ReservationBusy
        | OrcaError::BranchUnobtainable { .. }
        | OrcaError::BranchTaken { .. }
        | OrcaError::BranchUnverified
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
    /// Its latest dispatch stopped or failed and cannot be adopted.
    Ended(Receipt),
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
            writer_identity: None,
            writer_identity_required: false,
        })
    }

    /// Require this house writer for new isolated worktrees before a worker
    /// starts. The identity is scoped to each created worktree.
    #[must_use]
    pub fn with_writer_identity(mut self, name: String, email: String) -> Self {
        self.writer_identity = Some((name, email));
        self.writer_identity_required = true;
        self
    }

    /// Refuse a worker launch if the house has no verified writer identity.
    #[must_use]
    pub fn require_writer_identity(mut self) -> Self {
        self.writer_identity_required = true;
        self
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
        self.launch_receipt_until(task, dispatch, shown, None)
    }

    fn launch_receipt_until(
        &self,
        task: &str,
        dispatch: &str,
        shown: &WorkerShow,
        deadline: Option<Instant>,
    ) -> Option<Receipt> {
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
            .or_else(|| {
                worktrees
                    .first()
                    .and_then(|id| self.worktree_branch_until(id, deadline))
            });
        let branch_resources = branch
            .as_deref()
            // Orca reports the full ref, such as `refs/heads/lemarier/x`.
            .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch))
            .and_then(external)
            .map(|branch| self.resource(ResourceKind::Branch, branch));
        let created_worktree = shown
            .worker
            .effects
            .iter()
            .any(|effect| effect.kind == "worktree");
        if created_worktree {
            resources.extend(branch_resources.clone());
        }
        resources.extend(
            worktrees
                .into_iter()
                .map(|handle| self.resource(ResourceKind::Worktree, handle)),
        );
        resources.truncate(MAX_RECEIPT_RESOURCES);
        let touched = if created_worktree {
            Vec::new()
        } else {
            branch_resources.into_iter().collect()
        };
        Receipt::new(external(task)?, resources, touched).ok()
    }

    /// The branch Orca's worktree record names, or `None` when the worktree
    /// is gone or cannot be read. A receipt without a branch confirms no
    /// requested branch, so a failed read holds such a launch rather than
    /// accepting it.
    fn worktree_branch_until(
        &self,
        worktree: &ExternalRef,
        deadline: Option<Instant>,
    ) -> Option<String> {
        let timeout = match deadline {
            Some(deadline) => self.observation_timeout(deadline)?,
            None => self.config.call_timeout,
        };
        let args = wire::Args::command(&["worktree", "show"])
            .value("worktree", &format!("id:{worktree}"))
            .json();
        let shown: WorktreeShow =
            wire::typed(self.call(args, timeout).ok()?, "worktree show").ok()?;
        shown.worktree.and_then(|worktree| worktree.branch)
    }

    fn observation_timeout(&self, deadline: Instant) -> Option<Duration> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        (!remaining.is_zero()).then_some(remaining.min(self.config.call_timeout))
    }

    /// The Run's Tasks, with specs cut down by Orca's `--brief` listing.
    fn run_tasks(&self) -> Result<Vec<OrcaTask>, OrcaError> {
        self.list_tasks(true)
    }

    /// Active Task assignments identify the Dispatch behind a mailbox
    /// sender terminal. Orca questions do not carry a Dispatch in payload.
    pub(crate) fn sender_dispatches(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, OrcaError> {
        let mut senders = std::collections::BTreeMap::new();
        let mut ambiguous = std::collections::BTreeSet::new();
        for task in self.run_tasks()? {
            if task.status != "dispatched" {
                continue;
            }
            if let (Some(handle), Some(dispatch)) = (task.assignee_handle, task.dispatch_id) {
                // A terminal assigned to several active Tasks is ambiguous.
                if ambiguous.contains(&handle) {
                    continue;
                }
                if senders.insert(handle.clone(), dispatch).is_some() {
                    senders.remove(&handle);
                    ambiguous.insert(handle);
                }
            }
        }
        Ok(senders)
    }

    /// The Run's Tasks with their full specs. `--brief` collapses whitespace
    /// and caps a spec at 160 characters, which loses the requested-branch
    /// line, so a caller that reads specs must not use it.
    fn run_tasks_with_specs(&self) -> Result<Vec<OrcaTask>, OrcaError> {
        self.list_tasks(false)
    }

    fn list_tasks(&self, brief: bool) -> Result<Vec<OrcaTask>, OrcaError> {
        let mut args = wire::Args::command(&["orchestration", "task-list"])
            .value("run", self.config.run.as_str());
        if brief {
            args = args.switch("brief");
        }
        let args = args.json();
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
        let receipt = self
            .launch_receipt(&task.id, &dispatch.id, &shown)
            .and_then(|receipt| self.complete_identity_receipt(key, receipt));
        Ok(receipt.map_or(TaskLaunch::Unclear, |receipt| {
            if dispatch_ended(&shown) {
                TaskLaunch::Ended(receipt)
            } else {
                TaskLaunch::Dispatched(receipt)
            }
        }))
    }

    /// The branch the launch for `key` recorded as requested, when its Task
    /// records one.
    fn recorded_branch(&self, key: &IdempotencyKey) -> Result<Option<String>, OrcaError> {
        let title = self.task_title(key);
        let mut matching = self
            .run_tasks_with_specs()?
            .into_iter()
            .filter(|task| task.task_title.as_deref() == Some(title.as_str()));
        // Several Tasks for one key are an unexplained duplicate: no record.
        Ok(match (matching.next(), matching.next()) {
            (Some(task), None) => task
                .spec
                .and_then(|spec| requested_in_spec(&spec).map(str::to_owned)),
            _ => None,
        })
    }

    fn create_task(
        &self,
        key: &IdempotencyKey,
        brief: &Text,
        requested: Option<&BranchName>,
    ) -> Result<String, EffectFailure> {
        let args = wire::Args::command(&["orchestration", "task-create"])
            .value("spec", &task_spec(brief, requested))
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
                // Orca's recorded Dispatch owns resources even when startup
                // failed. A failed or stopped dispatch is an ended attempt,
                // while a running dispatch can produce an accepted receipt.
                // A failed read leaves the launch uncertain.
                let shown = self
                    .show(&start.dispatch_id)
                    .ok()
                    .flatten()
                    .ok_or_else(response_lost)?;
                if dispatch_ended(&shown) {
                    return Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded));
                }
                self.launch_receipt(task, &start.dispatch_id, &shown)
                    .ok_or_else(response_lost)
            }
            // The Task was already dispatched, by an earlier submission of
            // this key: return that Dispatch rather than a refusal.
            Err(OrcaError::Refused { code, .. }) if code == "task_not_startable" => {
                match self.task_launch(key) {
                    Ok(TaskLaunch::Dispatched(receipt)) => Ok(receipt),
                    Ok(TaskLaunch::Ended(_)) => {
                        Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded))
                    }
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
        if self.writer_identity_required && self.writer_identity.is_none() {
            return Err(not_applied());
        }
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
            Workspace::Isolated => branch
                .map(|requested| {
                    branch::created_branch(self.config.branch_prefix.as_ref(), requested)
                })
                .transpose()
                .map_err(|error| call_failure(&error))?,
            Workspace::Existing(_) => None,
        };
        let name = branch
            .filter(|_| matches!(workspace, Workspace::Isolated))
            .map(|branch| branch::worktree_name(self.config.branch_prefix.as_ref(), branch))
            .transpose()
            .map_err(|error| call_failure(&error))?;
        let mut reservation = self
            .reserve(format!(
                "launch-{:032x}",
                key_digest(&self.config.house, key.as_str())
            ))
            .map_err(|error| call_failure(&error))?;
        let mut receipt = self.launch_reserved(
            key,
            workspace,
            brief,
            name,
            new_branch.as_ref(),
            branch,
            agent,
        )?;
        reservation.settle();
        let Some(requested) = branch else {
            return Ok(receipt);
        };
        let worker = receipt
            .created()
            .iter()
            .find(|resource| resource.kind == ResourceKind::Worker)
            .cloned();
        let deadline = Instant::now() + BRANCH_OBSERVATION_TIMEOUT.min(self.config.call_timeout);
        for poll in 0..BRANCH_POLLS {
            if poll > 0 && self.observation_timeout(deadline).is_none() {
                break;
            }
            if let Some(actual) = receipt_branch(&receipt) {
                if self.accepts_launch_branch(requested, &actual, workspace) {
                    return Ok(receipt);
                }
                break;
            }
            if poll + 1 < BRANCH_POLLS {
                let Some(remaining) = self.observation_timeout(deadline) else {
                    break;
                };
                thread::sleep(BRANCH_POLL_INTERVAL.min(remaining));
                if let Some(dispatch) = worker.as_ref().and_then(|worker| self.dispatch_of(worker))
                    && let Some(timeout) = self.observation_timeout(deadline)
                    && let Ok(Some(shown)) = self.show_with_timeout(dispatch, timeout)
                {
                    if dispatch_ended(&shown) {
                        return Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded));
                    }
                    if let Some(updated) = self.launch_receipt_until(
                        receipt.reference().as_str(),
                        dispatch,
                        &shown,
                        Some(deadline),
                    ) {
                        receipt = self
                            .complete_identity_receipt(key, updated)
                            .ok_or_else(response_lost)?;
                    }
                }
            }
        }
        let reason = match receipt_branch(&receipt) {
            Some(actual)
                if matches!(workspace, Workspace::Isolated)
                    && branch_prefix_differs(self.config.branch_prefix.as_ref(), &actual) =>
            {
                UncertainReason::BranchPrefixMismatchStopped
            }
            Some(_) => UncertainReason::BranchMismatchStopped,
            None => UncertainReason::BranchUnconfirmedStopped,
        };
        let stopped = worker.as_ref().is_some_and(|worker| {
            (0..WRONG_BRANCH_STOP_ATTEMPTS).any(|_| self.cancel(worker).is_ok())
        });
        if stopped && let Some(worker) = worker.as_ref() {
            self.release_stopped(worker);
        }
        Err(EffectFailure::Uncertain(if stopped {
            reason
        } else {
            UncertainReason::BranchStopUnconfirmed
        }))
    }

    /// The repository owner enables this once during house setup. Launch is
    /// read-only with respect to the shared Git config.
    fn worktree_config_enabled(&self) -> Result<bool, EffectFailure> {
        let args = wire::Args::command(&["worktree", "list"])
            .value("repo", self.config.repo.as_str())
            .value("limit", &MAX_REPO_WORKTREES.to_string())
            .json();
        let list: WorktreeList = wire::typed(
            self.call(args, self.config.call_timeout)
                .map_err(|_| response_lost())?,
            "worktree list",
        )
        .map_err(|_| response_lost())?;
        if list.truncated
            || list.total_count != list.worktrees.len()
            || !list.host_scope.omitted_host_ids.is_empty()
        {
            return Err(response_lost());
        }
        let mut mains = list.worktrees.iter().filter(|row| row.is_main_worktree);
        let path = match (mains.next(), mains.next()) {
            (Some(row), None) => row.path.as_deref().ok_or_else(response_lost)?,
            _ => return Err(response_lost()),
        };
        let result = crate::workflows::push::run_bounded(
            std::path::Path::new("git"),
            path,
            &[
                "config",
                "--local",
                "--bool",
                "--get",
                "extensions.worktreeConfig",
            ],
            &[
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ],
            self.config.call_timeout,
        );
        match result {
            Some((Some(0), output)) if output == b"true\n" => Ok(true),
            Some((Some(0), output)) if output == b"false\n" => Ok(false),
            Some((Some(1), output)) if output.is_empty() => Ok(false),
            _ => Err(response_lost()),
        }
    }

    /// Close the terminal of a worker stopped for running on the wrong
    /// branch. Orca archives its output and keeps its worktree and branch.
    /// The outcome is not needed here: [`OrcaBackend::launch_collision`]
    /// reads back whether the terminal was released for later cleanup.
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

    #[expect(
        clippy::too_many_arguments,
        reason = "launch needs both requested and resolved branch names"
    )]
    fn launch_reserved(
        &self,
        key: &IdempotencyKey,
        workspace: &Workspace,
        brief: &Text,
        name: Option<String>,
        new_branch: Option<&BranchName>,
        requested_branch: Option<&BranchName>,
        agent: Option<&AgentSelection>,
    ) -> Result<Receipt, EffectFailure> {
        let task = match self
            .task_launch(key)
            .map_err(|error| call_failure(&error))?
        {
            TaskLaunch::Dispatched(receipt) => return Ok(receipt),
            TaskLaunch::Ended(_) => {
                return Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded));
            }
            TaskLaunch::Unclear => return Err(response_lost()),
            TaskLaunch::Undispatched(task) => Some(task),
            TaskLaunch::None => None,
        };
        if self.writer_identity.is_some() && !self.worktree_config_enabled()? {
            return Err(EffectFailure::NotApplied(
                NotAppliedReason::WorktreeConfigDisabled,
            ));
        }
        // An identity-bearing launch creates and configures its worktree
        // before starting the worker. A retry finds that worktree by the
        // launch marker, so an interrupted setup cannot create a second one.
        if self.writer_identity.is_none()
            && let Some(branch) = new_branch
        {
            self.check_branch_free(branch)
                .map_err(|error| match error {
                    OrcaError::BranchTaken { .. } => {
                        EffectFailure::NotApplied(NotAppliedReason::BranchInUse)
                    }
                    _ => not_applied(),
                })?;
        }
        let task = match task {
            Some(task) => task,
            None => self.create_task(key, brief, new_branch)?,
        };
        if self.writer_identity.is_some() && matches!(workspace, Workspace::Isolated) {
            let branch = new_branch.ok_or_else(response_lost)?;
            let name = name.ok_or_else(response_lost)?;
            let worktree = self.prepare_identity_worktree(key, &task, &name, branch)?;
            let id = external(worktree.id.as_deref().ok_or_else(response_lost)?)
                .ok_or_else(response_lost)?;
            let existing = Workspace::Existing(self.resource(ResourceKind::Worktree, id));
            let receipt = self.start(key, &task, &existing, None, agent)?;
            return self
                .complete_identity_receipt(key, receipt)
                .ok_or_else(response_lost);
        }
        if self.writer_identity.is_some()
            && let Workspace::Existing(resource) = workspace
        {
            let branch = requested_branch.ok_or_else(response_lost)?;
            self.prepare_existing_identity_worktree(key, resource, branch)?;
            let receipt = self.start(key, &task, workspace, None, agent)?;
            return self
                .complete_identity_receipt(key, receipt)
                .ok_or_else(response_lost);
        }
        self.start(key, &task, workspace, name.as_deref(), agent)
    }

    fn prepare_existing_identity_worktree(
        &self,
        key: &IdempotencyKey,
        resource: &ResourceRef,
        branch: &BranchName,
    ) -> Result<(), EffectFailure> {
        let args = wire::Args::command(&["worktree", "show"])
            .value("worktree", &format!("id:{}", resource.handle))
            .json();
        let shown: WorktreeShow = wire::typed(
            self.call(args, self.config.call_timeout)
                .map_err(|_| response_lost())?,
            "worktree show",
        )
        .map_err(|_| response_lost())?;
        let row = shown.worktree.ok_or_else(response_lost)?;
        let path = row.path.as_ref().ok_or_else(response_lost)?;
        if row.id.as_deref() != Some(resource.handle.as_str())
            || row.branch.as_deref() != Some(format!("refs/heads/{branch}").as_str())
            || row.is_main_worktree
            || !path.is_absolute()
        {
            return Err(response_lost());
        }
        let (name, email) = self.writer_identity.as_ref().ok_or_else(response_lost)?;
        let base = configure_writer_worktree(path, name, email, self.config.call_timeout)
            .ok_or_else(response_lost)?;
        crate::adapters::orca::record_writer_base(
            &self.config.runtime_dir,
            key,
            &crate::adapters::orca::WriterBase {
                house: self.config.house.clone(),
                worktree: resource.handle.clone(),
                branch: branch.clone(),
                base,
                created: false,
            },
        )
        .map_err(|_| response_lost())
    }

    /// Find a worktree this launch marker owns. A partial or truncated list
    /// proves nothing, and duplicate markers require operator inspection.
    fn identity_worktree(
        &self,
        key: &IdempotencyKey,
    ) -> Result<Option<WorktreeRow>, EffectFailure> {
        let args = wire::Args::command(&["worktree", "list"])
            .value("repo", self.config.repo.as_str())
            .value("limit", &MAX_REPO_WORKTREES.to_string())
            .json();
        let list: WorktreeList = wire::typed(
            self.call(args, self.config.call_timeout)
                .map_err(|_| response_lost())?,
            "worktree list",
        )
        .map_err(|_| response_lost())?;
        if list.truncated
            || list.total_count > list.worktrees.len()
            || list.worktrees.len() >= MAX_REPO_WORKTREES
            || !list.host_scope.omitted_host_ids.is_empty()
        {
            return Err(response_lost());
        }
        let mut matches = list
            .worktrees
            .into_iter()
            .filter(|row| row.comment.as_deref() == Some(self.task_title(key).as_str()));
        match (matches.next(), matches.next()) {
            (None, _) => Ok(None),
            (Some(row), None) => Ok(Some(row)),
            _ => Err(response_lost()),
        }
    }

    fn prepare_identity_worktree(
        &self,
        key: &IdempotencyKey,
        task: &str,
        name: &str,
        branch: &BranchName,
    ) -> Result<WorktreeRow, EffectFailure> {
        let mut row = self.identity_worktree(key)?;
        let created_now = row.is_none();
        if row.is_none() {
            self.check_branch_free(branch)
                .map_err(|error| match error {
                    OrcaError::BranchTaken { .. } => {
                        EffectFailure::NotApplied(NotAppliedReason::BranchInUse)
                    }
                    _ => not_applied(),
                })?;
            let mut args = wire::Args::command(&["worktree", "create"])
                .value("repo", self.config.repo.as_str())
                .value("name", name)
                .value("comment", &self.task_title(key))
                .switch("no-parent");
            if let Some(base) = &self.config.base_branch {
                args = args.value("base-branch", base.as_str());
            }
            // A lost response is reconciled by the marker before another
            // create. The branch is not handed to a worker until configured.
            let _ = self
                .call(args.json(), self.config.launch_timeout)
                .map_err(|_| response_lost())?;
            row = self.identity_worktree(key)?;
        }
        let row = row.ok_or_else(response_lost)?;
        let expected = format!("refs/heads/{branch}");
        if row.branch.as_deref() != Some(expected.as_str())
            || row.is_main_worktree
            || row.id.is_none()
            || row.path.as_ref().is_none_or(|path| !path.is_absolute())
        {
            if created_now
                && let Some(receipt) = self.remove_failed_identity_worktree(key, task, &row)
            {
                return Err(EffectFailure::Ended(receipt));
            }
            return Err(response_lost());
        }
        let (name, email) = self.writer_identity.as_ref().ok_or_else(response_lost)?;
        let path = row.path.as_ref().ok_or_else(response_lost)?;
        let base = match configure_writer_worktree(path, name, email, self.config.call_timeout) {
            Some(base) => base,
            None => {
                if created_now
                    && let Some(receipt) = self.remove_failed_identity_worktree(key, task, &row)
                {
                    return Err(EffectFailure::Ended(receipt));
                }
                return Err(response_lost());
            }
        };
        let recorded = crate::adapters::orca::record_writer_base(
            &self.config.runtime_dir,
            key,
            &crate::adapters::orca::WriterBase {
                house: self.config.house.clone(),
                worktree: external(row.id.as_deref().ok_or_else(response_lost)?)
                    .ok_or_else(response_lost)?,
                branch: branch.clone(),
                base,
                created: true,
            },
        );
        if recorded.is_err() {
            if created_now
                && let Some(receipt) = self.remove_failed_identity_worktree(key, task, &row)
            {
                return Err(EffectFailure::Ended(receipt));
            }
            return Err(response_lost());
        }
        Ok(row)
    }

    fn undispatched_receipt(&self, task: &str, row: &WorktreeRow) -> Option<Receipt> {
        let worktree = self.resource(ResourceKind::Worktree, external(row.id.as_deref()?)?);
        let branch = self.resource(
            ResourceKind::Branch,
            external(row.branch.as_deref()?.strip_prefix("refs/heads/")?)?,
        );
        Receipt::new(external(task)?, vec![worktree, branch], Vec::new()).ok()
    }

    /// Remove only a worktree created by this invocation, after confirming
    /// that its Task has no Dispatch and its marker still identifies it.
    fn remove_failed_identity_worktree(
        &self,
        key: &IdempotencyKey,
        task: &str,
        row: &WorktreeRow,
    ) -> Option<Receipt> {
        let receipt = self.undispatched_receipt(task, row)?;
        if !matches!(self.task_launch(key), Ok(TaskLaunch::Undispatched(_)))
            || !self.identity_worktree(key).is_ok_and(|found| {
                found
                    .as_ref()
                    .is_some_and(|found| found.id == row.id && found.branch == row.branch)
            })
        {
            return Some(receipt);
        }
        let Some(id) = row.id.as_deref() else {
            return Some(receipt);
        };
        let args = wire::Args::command(&["worktree", "rm"])
            .value("worktree", &format!("id:{id}"))
            .json();
        let _ = self.call(args, self.config.call_timeout);
        // A complete reread proves removal; otherwise retain the receipt.
        if matches!(self.identity_worktree(key), Ok(None)) {
            None
        } else {
            Some(receipt)
        }
    }

    fn complete_identity_receipt(&self, key: &IdempotencyKey, receipt: Receipt) -> Option<Receipt> {
        if self.writer_identity.is_none() {
            return Some(receipt);
        }
        let recorded = match crate::adapters::orca::read_writer_base(&self.config.runtime_dir, key)
        {
            Ok(recorded) => recorded,
            Err(crate::adapters::orca::WriterBaseError::NotFound) => return Some(receipt),
            Err(_) => return None,
        };
        let mut created = receipt.created().to_vec();
        if !recorded.created {
            let mut touched = receipt.touched().to_vec();
            created.retain(|resource| {
                if matches!(resource.kind, ResourceKind::Branch | ResourceKind::Worktree) {
                    if !touched.contains(resource) {
                        touched.push(resource.clone());
                    }
                    false
                } else {
                    true
                }
            });
            let branch = self.resource(ResourceKind::Branch, external(recorded.branch.as_str())?);
            if !touched.contains(&branch) {
                touched.push(branch);
            }
            let worktree = self.resource(ResourceKind::Worktree, recorded.worktree);
            if !touched.contains(&worktree) {
                touched.push(worktree);
            }
            return Receipt::new(receipt.reference().clone(), created, touched).ok();
        }
        let row = self.identity_worktree(key).ok()??;
        let id = external(row.id.as_deref()?)?;
        if id != recorded.worktree {
            return None;
        }
        let branch = row.branch.as_deref()?.strip_prefix("refs/heads/")?;
        if branch != recorded.branch.as_str() {
            return None;
        }
        let branch = external(branch)?;
        let worktree = self.resource(ResourceKind::Worktree, id);
        if !created.contains(&worktree) {
            created.push(worktree);
        }
        let branch = self.resource(ResourceKind::Branch, branch);
        if !created.contains(&branch) {
            created.push(branch);
        }
        let mut touched = receipt.touched().to_vec();
        touched.retain(|resource| !created.contains(resource));
        Receipt::new(receipt.reference().clone(), created, touched).ok()
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
    /// [`OrcaError::BranchTaken`] when a worktree has the branch;
    /// [`OrcaError::BranchUnverified`] when the listing is incomplete.
    /// Other errors when Orca cannot be read.
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
            || list.total_count > list.worktrees.len()
            || list.worktrees.len() >= MAX_REPO_WORKTREES
            || !list.host_scope.omitted_host_ids.is_empty()
        {
            return Err(OrcaError::BranchUnverified);
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
        let in_run = self.in_run(&shown);
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
            Ok(Some(shown)) if self.in_run(&shown) => {}
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

    /// Whether Orca records the Dispatch in this backend's Run.
    fn in_run(&self, shown: &WorkerShow) -> bool {
        shown
            .dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.run_id.as_deref())
            == Some(self.config.run.as_str())
    }

    pub(crate) fn show(&self, dispatch: &str) -> Result<Option<WorkerShow>, OrcaError> {
        self.show_with_timeout(dispatch, self.config.call_timeout)
    }

    fn show_with_timeout(
        &self,
        dispatch: &str,
        timeout: Duration,
    ) -> Result<Option<WorkerShow>, OrcaError> {
        let args = wire::Args::command(&["orchestration", "worker-show"])
            .value("dispatch", dispatch)
            .json();
        match self.call(args, timeout) {
            Ok(value) => wire::typed(value, "worker show").map(Some),
            Err(OrcaError::Refused { code, .. }) if code == "dispatch_not_found" => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Find the Dispatch a launch key started, after a lost response.
    ///
    /// Reports [`Lookup::Applied`] for an active or successful dispatch,
    /// [`Lookup::Ended`] for a stopped or failed one, and [`Lookup::Unknown`]
    /// when the Task cannot establish either outcome. A missing Task is not
    /// proof of absence, since a lost `task-create` may still land.
    ///
    /// # Errors
    /// [`BackendUnavailable`] when Orca cannot be queried.
    pub fn lookup_launch(&self, key: &IdempotencyKey) -> Result<Lookup, BackendUnavailable> {
        match self
            .task_launch(key)
            .map_err(|error| read_failure(&error))?
        {
            TaskLaunch::Dispatched(receipt) => Ok(Lookup::Applied(receipt)),
            TaskLaunch::Ended(receipt) => Ok(Lookup::Ended(receipt)),
            TaskLaunch::Undispatched(task) if self.writer_identity.is_some() => Ok(self
                .identity_worktree(key)
                .ok()
                .flatten()
                .and_then(|row| self.undispatched_receipt(&task, &row))
                .map_or(Lookup::Unknown, Lookup::Ended)),
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
    /// [`OrcaError::WrongBranchRunning`] when a confirmed wrong branch's
    /// worker remains active; [`OrcaError::BranchUnconfirmedRunning`] when an
    /// active worker has no reported branch. The settled forms are
    /// [`OrcaError::BranchMismatch`] and [`OrcaError::BranchUnconfirmed`].
    pub fn verify_launch_branch(
        &self,
        key: &IdempotencyKey,
        requested: &BranchName,
    ) -> Result<(), OrcaError> {
        match self.task_launch(key)? {
            TaskLaunch::Ended(receipt) => {
                let actual = receipt_branch(&receipt);
                if actual.as_ref().is_some_and(|actual| {
                    branch::created_branch(self.config.branch_prefix.as_ref(), requested)
                        .is_ok_and(|expected| &expected == actual)
                }) {
                    return Err(OrcaError::LaunchEnded {
                        requested: requested.to_string(),
                    });
                }
                Err(actual.map_or_else(
                    || OrcaError::BranchUnconfirmed {
                        requested: requested.to_string(),
                    },
                    |actual| OrcaError::BranchMismatch {
                        requested: requested.to_string(),
                        actual: Some(actual.to_string()),
                    },
                ))
            }
            TaskLaunch::Dispatched(receipt) => {
                let actual = receipt_branch(&receipt);
                let recorded = self.recorded_branch(key)?;
                if actual
                    .as_ref()
                    .is_some_and(|actual| match recorded.as_deref() {
                        Some(recorded) => {
                            branch::created_branch(self.config.branch_prefix.as_ref(), requested)
                                .is_ok_and(|expected| {
                                    recorded == expected.as_str() && actual == &expected
                                })
                        }
                        None => actual == requested,
                    })
                {
                    return Ok(());
                }
                let mismatch = actual.as_ref().map_or_else(
                    || OrcaError::BranchUnconfirmed {
                        requested: requested.to_string(),
                    },
                    |actual| OrcaError::BranchMismatch {
                        requested: requested.to_string(),
                        actual: Some(actual.to_string()),
                    },
                );
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
                match mismatch {
                    OrcaError::BranchMismatch { requested, actual } => {
                        Err(OrcaError::WrongBranchRunning {
                            requested,
                            actual,
                            worker: dispatch.to_owned(),
                        })
                    }
                    OrcaError::BranchUnconfirmed { requested } => {
                        Err(OrcaError::BranchUnconfirmedRunning {
                            requested,
                            worker: dispatch.to_owned(),
                        })
                    }
                    other => Err(other),
                }
            }
            TaskLaunch::None | TaskLaunch::Undispatched(_) | TaskLaunch::Unclear => {
                Err(OrcaError::BranchUnconfirmed {
                    requested: requested.to_string(),
                })
            }
        }
    }

    /// The collision a launch with a requested branch ran into, if it did.
    ///
    /// Returns `Some` when the key's launch recorded the branch derived from
    /// `requested`, was dispatched, and Orca created it with a numeric
    /// suffix because it already existed, with the evidence that this launch
    /// owns the stray worker, worktree, and branch. Returns `None` for no
    /// dispatched launch, the requested branch itself, a launch that recorded
    /// another branch or none, or another mismatch
    /// ([`OrcaBackend::verify_launch_branch`] reports those).
    ///
    /// # Errors
    /// [`OrcaError`] when Orca cannot be read.
    pub fn launch_collision(
        &self,
        key: &IdempotencyKey,
        requested: &BranchName,
    ) -> Result<Option<BranchCollision>, OrcaError> {
        let receipt = match self.task_launch(key)? {
            TaskLaunch::Dispatched(receipt) | TaskLaunch::Ended(receipt) => receipt,
            TaskLaunch::None | TaskLaunch::Undispatched(_) | TaskLaunch::Unclear => {
                return Ok(None);
            }
        };
        // A numeric suffix alone is not ownership: the launch must have
        // recorded this very branch as its request.
        let expected = branch::created_branch(self.config.branch_prefix.as_ref(), requested)?;
        if self.recorded_branch(key)?.as_deref() != Some(expected.as_str()) {
            return Ok(None);
        }
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
        if !branch::is_collision(expected.as_str(), stray.handle.as_str()) {
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
    /// Dispatch's recorded state, for a Dispatch of this backend's Run only.
    /// Messages and replies carry no key Orca records, so they stay
    /// [`Lookup::Unknown`]. [`EffectExecutor::lookup`]
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
            Operation::LaunchWorker {
                branch, workspace, ..
            } => {
                let found = self.lookup_launch(request.key())?;
                // A launch on the wrong branch was held, not accepted.
                Ok(match (&found, branch) {
                    (Lookup::Applied(receipt), Some(branch))
                        if !receipt_branch(receipt).as_ref().is_some_and(|actual| {
                            self.accepts_launch_branch(branch, actual, workspace)
                        }) =>
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
                // A Dispatch of another Run was never this backend's to
                // stop or release, whatever its state.
                let applied = shown
                    .filter(|shown| self.in_run(shown))
                    .is_some_and(|shown| match operation {
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

/// Set identity only in the linked worktree's config, after Git enables
/// per-worktree configuration for the repository. An invalid or missing
/// worktree fails before an agent can receive its task.
fn configure_writer_worktree(
    path: &std::path::Path,
    name: &str,
    email: &str,
    deadline: Duration,
) -> Option<crate::contracts::CommitId> {
    if !path.is_absolute()
        || deadline.is_zero()
        || [name, email]
            .iter()
            .any(|value| value.is_empty() || value.chars().any(char::is_control))
    {
        return None;
    }
    let environment = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ];
    let run = |args: &[&str]| {
        matches!(
            crate::workflows::push::run_bounded(
                std::path::Path::new("git"),
                path,
                args,
                &environment,
                deadline,
            ),
            Some((Some(0), _))
        )
    };
    let Some((Some(0), root)) = crate::workflows::push::run_bounded(
        std::path::Path::new("git"),
        path,
        &["rev-parse", "--show-toplevel"],
        &environment,
        deadline,
    ) else {
        return None;
    };
    let reported = std::str::from_utf8(&root).ok().map(str::trim);
    if reported.and_then(|root| std::path::Path::new(root).canonicalize().ok())
        != path.canonicalize().ok()
    {
        return None;
    }
    let Some((Some(0), head)) = crate::workflows::push::run_bounded(
        std::path::Path::new("git"),
        path,
        &["rev-parse", "--verify", "HEAD"],
        &environment,
        deadline,
    ) else {
        return None;
    };
    let Ok(head) = std::str::from_utf8(&head) else {
        return None;
    };
    let head = head.trim();
    let base = crate::contracts::CommitId::new(head).ok()?;
    (run(&["config", "--worktree", "user.name", name])
        && run(&["config", "--worktree", "user.email", email])
        && run(&["config", "--worktree", "kitchen.launchBase", head]))
    .then_some(base)
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
    fn inspect_worktree(
        &self,
        worktree: &ResourceRef,
        branch: &BranchName,
        head: &crate::contracts::CommitId,
        report_path: &Text,
    ) -> Result<WorktreeStatus, BackendUnavailable> {
        if worktree.backend != self.config.backend || worktree.kind != ResourceKind::Worktree {
            return Ok(WorktreeStatus::Missing);
        }
        let args = wire::Args::command(&["worktree", "show"])
            .value("worktree", &format!("id:{}", worktree.handle))
            .json();
        let answer = match self.call(args, self.config.call_timeout) {
            Ok(answer) => answer,
            Err(OrcaError::Refused { code, .. }) if code == "worktree_not_found" => {
                return Ok(WorktreeStatus::Missing);
            }
            Err(error) => return Err(read_failure(&error)),
        };
        let shown: WorktreeShow =
            wire::typed(answer, "worktree show").map_err(|_| BackendUnavailable::Transport)?;
        let Some(row) = shown.worktree else {
            return Ok(WorktreeStatus::Missing);
        };
        if row.id.as_deref() != Some(worktree.handle.as_str()) || row.is_main_worktree {
            return Ok(WorktreeStatus::Missing);
        }
        if row.branch.as_deref() != Some(format!("refs/heads/{branch}").as_str()) {
            return Ok(WorktreeStatus::WrongBranch);
        }
        let Some(path) = row.path.filter(|path| path.is_absolute()) else {
            return Ok(WorktreeStatus::Missing);
        };
        let env = [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ];
        let git = |args: &[&str]| {
            crate::workflows::push::run_bounded(
                std::path::Path::new("git"),
                &path,
                args,
                &env,
                self.config.call_timeout,
            )
        };
        let Some((Some(0), root)) = git(&["rev-parse", "--show-toplevel"]) else {
            return Ok(WorktreeStatus::Missing);
        };
        let root = std::str::from_utf8(&root).map_err(|_| BackendUnavailable::Transport)?;
        if std::path::Path::new(root.trim()).canonicalize().ok() != path.canonicalize().ok() {
            return Ok(WorktreeStatus::Missing);
        }
        let Some((Some(0), actual)) = git(&["rev-parse", "--verify", "HEAD"]) else {
            return Err(BackendUnavailable::Transport);
        };
        if actual.as_slice() != format!("{}\n", head.as_str()).as_bytes() {
            return Ok(WorktreeStatus::WrongHead);
        }
        let clean = crate::workflows::push::checkout_clean_except_report(
            &path,
            std::path::Path::new(report_path.as_str()),
        )
        .map_err(|_| BackendUnavailable::Transport)?;
        Ok(if clean {
            WorktreeStatus::Ready
        } else {
            WorktreeStatus::Dirty
        })
    }

    fn accepts_launch_branch(
        &self,
        requested: &BranchName,
        actual: &BranchName,
        workspace: &Workspace,
    ) -> bool {
        match workspace {
            Workspace::Isolated => {
                branch::created_branch(self.config.branch_prefix.as_ref(), requested)
                    .is_ok_and(|created| &created == actual)
            }
            Workspace::Existing(_) => actual == requested,
        }
    }

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
    fn writer_identity_is_scoped_to_the_launched_worktree() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::process::Command;
        let temp = tempfile::tempdir()?;
        let main = temp.path().join("main");
        let worker = temp.path().join("worker");
        std::fs::create_dir(&main)?;
        let git =
            |path: &std::path::Path, args: &[&str]| -> Result<String, Box<dyn std::error::Error>> {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(path)
                    .args(args)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .output()?;
                if !output.status.success() {
                    return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
                }
                Ok(String::from_utf8(output.stdout)?.trim().to_owned())
            };
        git(&main, &["init", "-q", "-b", "main"])?;
        git(&main, &["config", "--local", "user.name", "Person"])?;
        git(
            &main,
            &["config", "--local", "user.email", "person@example.com"],
        )?;
        git(
            &main,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        )?;
        git(
            &main,
            &["config", "--local", "extensions.worktreeConfig", "true"],
        )?;
        let shared_config = std::fs::read(main.join(".git/config"))?;
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "writer",
                worker.to_str().ok_or("path")?,
            ],
        )?;
        assert!(
            configure_writer_worktree(
                &worker,
                "house[bot]",
                "123+house[bot]@users.noreply.github.com",
                Duration::from_secs(5)
            )
            .is_some()
        );
        assert_eq!(git(&worker, &["config", "user.name"])?, "house[bot]");
        assert_eq!(
            git(&worker, &["config", "user.email"])?,
            "123+house[bot]@users.noreply.github.com"
        );
        assert_eq!(git(&main, &["config", "user.name"])?, "Person");
        assert_eq!(git(&main, &["config", "user.email"])?, "person@example.com");
        assert_eq!(std::fs::read(main.join(".git/config"))?, shared_config);
        assert!(
            configure_writer_worktree(
                temp.path(),
                "house[bot]",
                "123+house[bot]@users.noreply.github.com",
                Duration::from_secs(5)
            )
            .is_none()
        );
        Ok(())
    }

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
    fn a_spec_records_the_requested_branch_on_its_last_line()
    -> Result<(), Box<dyn std::error::Error>> {
        let brief = Text::new("Do it.")?;
        let requested = BranchName::new("lemarier/x")?;
        let spec = task_spec(&brief, Some(&requested));
        assert_eq!(spec, "Do it.\n\nkitchen-requested-branch: lemarier/x");
        assert_eq!(requested_in_spec(&spec), Some("lemarier/x"));
        // No request records nothing.
        assert_eq!(requested_in_spec(&task_spec(&brief, None)), None);
        // A brief that imitates the line cannot forge a request.
        let forged = Text::new("Do it.\nkitchen-requested-branch: lemarier/forged")?;
        assert_eq!(requested_in_spec(&task_spec(&forged, None)), None);
        assert_eq!(
            requested_in_spec(&task_spec(&forged, Some(&requested))),
            Some("lemarier/x")
        );
        // A spec Kitchen did not write records nothing.
        assert_eq!(requested_in_spec("Do it."), None);
        assert_eq!(requested_in_spec(""), None);
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
            worker_state("ready", "in_progress", "live", true),
            WorkerState::AwaitingReply
        );
        assert_eq!(
            worker_state("ready", "in_progress", "unverifiable", true),
            WorkerState::Starting
        );
        assert_eq!(
            worker_state("ready", "in_progress", "exited", true),
            WorkerState::Starting
        );
        assert_eq!(
            worker_state("starting", "in_progress", "live", false),
            WorkerState::Starting
        );
    }
}
