//! The Orca execution and scheduling adapter.
//!
//! Supported runtime: Orca [`SUPPORTED_VERSIONS`], verified live against
//! 1.4.212 with the runtime features in [`REQUIRED_FEATURES`].
//! [`OrcaBackend::connect`] probes `orca status --json` and refuses other
//! versions, a runtime that is not ready, and missing features.
//!
//! Every call is a bounded subprocess ([`SystemRunner`]): separated
//! `--flag=value` arguments and no shell, a cleared environment without
//! terminal identity variables, a deadline, and a stdout limit. Responses are
//! parsed into typed shapes; unrecognized states map to "unknown", never to
//! success or settlement. Orca messages are redacted before they reach an
//! error.
//!
//! Runtime behavior the adapter relies on, all documented by Orca's
//! orchestration guide or observed on 1.4.212:
//!
//! - Idempotency. `--retry-request` accepts only request ids Orca issued, so a
//!   Kitchen key cannot be one. A launch creates one Orca Task titled with
//!   [`launch_marker`] (a fixed-length digest of house and key, because Orca
//!   truncates titles to 80 characters) and dispatches it. Orca returns a
//!   Task to `ready` when its worker's process exits (after a stop it does
//!   not) and would then dispatch it again, so the Task's newest Dispatch, as
//!   `dispatch-show --task` reports it whatever the Task's status, is the
//!   launch: resubmitting a launch key returns that Dispatch and starts
//!   nothing. A response lost before the Task exists stays unknown: a
//!   missing Task is not proof the create will not land. Messages and replies
//!   carry no key, and a trial starts a new run each time, so lookup and
//!   idempotency are declared per effect kind and not for those.
//! - Concurrent first submissions. A Task title is a marker, not a uniqueness
//!   constraint, so two callers that both list before either creates would
//!   both create. Launches and schedule installs therefore hold a per-key
//!   reservation, an advisory lock file in [`OrcaConfig::runtime_dir`], across
//!   list, create, and start. The state store serializes the effects of one
//!   task, but a lookup that finds no Task may be looking at a first
//!   submission still in flight, and the resubmission it allows then races
//!   it. The adapter is also called without the store and by several
//!   processes against one Orca, and the store never holds its lock across a
//!   backend call. A wait that ends is reported as uncertain, never as proof
//!   the effect did not happen.
//! - Launch readiness is positive evidence: `worker-start` succeeds only for
//!   a ready worker, and `Ready` also needs a `live` fleet verdict. A failed
//!   start is a failed launch. For schedules, Orca's `completed` run only
//!   means the launch step finished:
//!   [`ScheduleBackend::inspect_schedule`](crate::contracts::ScheduleBackend::inspect_schedule) joins
//!   each run with Kitchen's own [`crate::scheduling::ReadinessSignal`]s and
//!   a deadline, so a swallowed launch is reported as
//!   [`crate::scheduling::RunVerdict::LaunchFailed`].
//! - Branches. Orca puts its configured prefix in front of a worktree name.
//!   The adapter passes the requested branch's plain final component, waits
//!   briefly for Orca to report the branch, and verifies the configured
//!   prefix and name. A confirmed wrong branch or an unconfirmed branch after
//!   the wait causes a stop and a distinct failure reason. Receipts name the
//!   actual branch for later push and gate decisions. [`verify_branch`] checks
//!   a receipt for an exact branch when that is needed.
//! - Branch collisions. When the requested branch already exists, Orca
//!   creates it with a numeric suffix (`<branch>-2`) instead. Before the
//!   first start the adapter lists the repository's Orca worktrees and
//!   refuses a branch one of them has checked out
//!   ([`OrcaBackend::check_branch_free`]). A branch that exists only in Git
//!   is not listed, so the collision can still happen: the worker is
//!   stopped and its terminal released as for any wrong branch, and
//!   [`OrcaBackend::launch_collision`] reports the stray worker, worktree,
//!   and branch with the evidence that the launch owns them. The adapter
//!   never removes a worktree or branch; that is left to the dishwasher.
//! - Recovery signals. [`OrcaBackend::observe_signals`] reads what a
//!   coordinator needs to recover without a person, as typed observations
//!   with an explicit "cannot tell": whether the agent's first turn was seen
//!   ([`StartOutcome`]), the transcript's progress, whether the agent sits at
//!   its prompt ([`AgentPrompt`]), who holds the terminal ([`TerminalOwner`]),
//!   the class of a provider failure ([`ProviderErrorClass`], never its text),
//!   and whether the Dispatch still accepts messages
//!   ([`DispatchActivity`]). Absence is never promoted to a fact, and
//!   nothing in it stops, retries, or releases a worker.
//! - A terminal a person took over (`user_takeover`) reads as
//!   [`crate::contracts::WorkerState::UserTakeover`], unless the worker
//!   already reported its own outcome.
//! - The adapter sends a worker whose terminal a person took over no
//!   messages, replies, or stops.
//! - Mailbox waits return whole batches, heartbeats included; heartbeats are
//!   liveness only
//!   ([`Delivery::actionable`](crate::contracts::Delivery::actionable)).
//! - Settlement comes from an accepted worker report or an explicit stop.
//!   Orca projects a stopped worker's outcome as `failed`; the worker state
//!   `stopped` makes it a cancellation. Liveness `exited` without a report,
//!   quiet terminals, and process age are never settlement.
//! - Coordinator transfer. Orca records no relinquish. After Kitchen's store
//!   records a relinquish and an adoption, the adopting coordinator builds a
//!   backend with its own terminal handle and calls
//!   [`CoordinatorMailbox::adopt_run`](crate::contracts::CoordinatorMailbox::adopt_run);
//!   the previous terminal is then fenced from the mailbox (`consumer_fenced`,
//!   reported as [`crate::contracts::MailboxError::Fenced`]). Workers keep
//!   running. Orca 1.4.216 redelivers the unacknowledged messages to the
//!   adopter under a new delivery id and refuses an acknowledgement of the
//!   old id with `consumer_fenced`; the adapter confirms such a refusal with
//!   a plain read before reporting a fence.
//! - Released terminals. `worker-release` is idempotent and archives output;
//!   a released worker stays observable and keeps its settled state. Orca
//!   reads an archive from its oldest message forward, so signals follow its
//!   cursor, a bounded number of pages, to the newest.
//! - Shutdown. Nothing runs in the background: a call that exceeds its
//!   deadline is killed and reported as a timeout, which is uncertain for
//!   effects and unavailable for reads.
//! - Schedules. `automations create`, `edit`, `remove`, and `run` take no
//!   request key. Installs are named
//!   `kitchen:<house>:<consumer>:workflow=<workflow>`, created disabled,
//!   reconciled against a complete listing before and after every create,
//!   and every change is read back. An existing schedule is reused only when
//!   it is paused and matches the requested definition and workflow, and
//!   its receipt then lists it as touched, not created; an active or
//!   different one is refused and never changed. Orca skips a run on any
//!   non-zero precheck exit; only its run history tells an idle precheck from
//!   a failed one. It cannot prevent overlapping runs of one schedule or
//!   enforce a run timeout, so a schedule whose workflow requires either is
//!   refused at install, activation, and trial. Activation and trial derive
//!   the requirements from Kitchen's definition of the workflow the name
//!   records; a schedule naming no such workflow, or a consumer that
//!   workflow cannot serve, is not activated or tried.
//!   For other schedules, Kitchen's consumer lease must prevent overlap.

mod accounts;
mod backend;
mod branch;
mod error;
mod inspect;
mod process;
mod recovery;
mod redact;
mod reserve;
mod runtime;
mod schedule;
mod signals;
mod wire;

pub use accounts::{ACCOUNT_LIST_TIMEOUT, ManagedAccounts, managed_accounts};
pub use backend::{
    BranchCollision, DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT,
    MAX_REPO_WORKTREES, MAX_RUN_TASKS, OrcaBackend, OrcaConfig, SCHEDULE_SELECTION,
    WORKER_SELECTION, launch_marker, verify_branch,
};
pub use error::OrcaError;
pub use inspect::{MAX_INVENTORY_PAGES, RetainedReason, TerminalAccounting, WorkerRecord};
pub use process::{
    DEFAULT_MAX_STDOUT, ENV_ALLOWLIST, Invocation, OrcaRunner, RawOutput, SystemRunner,
};
pub use redact::{MAX_MESSAGE_BYTES, redact};
pub use runtime::{
    OrcaVersion, REQUIRED_FEATURES, RuntimeInfo, SUPPORTED_VERSIONS, capabilities, probe,
};
pub use schedule::{MAX_AUTOMATIONS, native_schedule_name};
pub use signals::{
    AgentPrompt, DispatchActivity, MAX_ARCHIVE_PAGES, ProviderErrorClass, SIGNAL_WINDOW_ROWS,
    StartOutcome, StartWindow, TerminalOwner, TranscriptProgress, WorkerSignals,
};
