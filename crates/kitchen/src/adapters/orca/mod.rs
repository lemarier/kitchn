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
//!   truncates titles to 80 characters) and dispatches it; Orca refuses to dispatch a
//!   dispatched Task again, so resubmitting a launch key returns the original
//!   Dispatch. A response lost before the Task exists stays unknown: a
//!   missing Task is not proof the create will not land. Messages and replies
//!   carry no key, so lookup and idempotency are declared partial.
//! - Launch readiness is positive evidence: `worker-start` succeeds only for
//!   a ready worker, and `Ready` also needs a `live` fleet verdict. A failed
//!   start is a failed launch. For schedules, Orca's `completed` run only
//!   means the launch step finished; see [`crate::scheduling::run_verdict`].
//! - Branches. Orca prefixes the worktree name it is given; launch receipts
//!   name the branch Orca created, and [`verify_branch`] reports a mismatch
//!   before the first push.
//! - A terminal a person took over (`user_takeover`) is retained, not
//!   failed; the adapter sends such a worker no messages.
//! - Mailbox waits return whole batches, heartbeats included; heartbeats are
//!   liveness only ([`Delivery::actionable`]).
//! - Settlement comes from an accepted worker report or an explicit stop.
//!   Orca projects a stopped worker's outcome as `failed`; the worker state
//!   `stopped` makes it a cancellation. Liveness `exited` without a report,
//!   quiet terminals, and process age are never settlement.
//! - Coordinator transfer. Orca records no relinquish. After Kitchen's store
//!   records a relinquish and an adoption, the adopting coordinator builds a
//!   backend with its own terminal handle and calls
//!   [`OrcaBackend::adopt_run`]; the previous terminal is then fenced from the
//!   mailbox (`consumer_fenced`). Workers keep running.
//! - Released terminals. `worker-release` is idempotent and archives output;
//!   a released worker stays observable and keeps its settled state.
//! - Shutdown. Nothing runs in the background: a call that exceeds its
//!   deadline is killed and reported as a timeout, which is uncertain for
//!   effects and unavailable for reads.
//! - Schedules. `automations create`, `edit`, `remove`, and `run` take no
//!   request key. Installs are named `kitchen:<house>:<consumer>`, created
//!   disabled, reconciled against a complete listing before and after every
//!   create, and every change is read back. Orca cannot tell an idle precheck
//!   from a failed one, and cannot prevent overlapping runs of one schedule;
//!   Kitchen's consumer lease must.

mod backend;
mod error;
mod inspect;
mod process;
mod redact;
mod runtime;
mod schedule;
mod wire;

pub use backend::{
    DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, MAX_RUN_TASKS, OrcaBackend, OrcaConfig,
    launch_marker, verify_branch,
};
pub use error::OrcaError;
pub use inspect::{
    Delivery, MAX_INVENTORY_PAGES, MAX_MAILBOX_WAIT, MailMessage, MessageKind, RetainedReason,
    TerminalAccounting, WorkerRecord,
};
pub use process::{
    DEFAULT_MAX_STDOUT, ENV_ALLOWLIST, Invocation, OrcaRunner, RawOutput, SystemRunner,
};
pub use redact::{MAX_MESSAGE_BYTES, redact};
pub use runtime::{
    OrcaVersion, REQUIRED_FEATURES, RuntimeInfo, SUPPORTED_VERSIONS, capabilities, probe,
};
pub use schedule::{MAX_AUTOMATIONS, native_schedule_name};
