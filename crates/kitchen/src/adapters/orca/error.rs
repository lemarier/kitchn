//! Orca adapter failures.

use std::io;

use crate::{
    ErrorClass,
    contracts::ContractError,
    scheduling::{BudgetError, ScheduleError, ScheduleField},
};

/// An Orca adapter failure. Messages from Orca are redacted and truncated;
/// raw command output is never included.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OrcaError {
    /// The Orca executable could not be started; nothing reached Orca.
    #[error("orca could not be started: {0}")]
    Spawn(io::ErrorKind),
    /// The call exceeded its deadline and was killed; its effect is unknown.
    #[error("orca call exceeded its deadline")]
    Timeout,
    /// The call produced more output than allowed and was killed.
    #[error("orca output exceeded {limit} bytes")]
    OutputLimit {
        /// The byte limit.
        limit: usize,
    },
    /// Reading from or waiting on the process failed after it started.
    #[error("orca call failed: {0}")]
    Io(io::ErrorKind),
    /// Orca exited without a JSON result.
    #[error("orca exited with code {code:?} and no JSON result")]
    NoResult {
        /// The exit code, if any.
        code: Option<i32>,
    },
    /// Orca's output did not match the expected shape.
    #[error("orca returned malformed {what}")]
    Malformed {
        /// Which response was malformed.
        what: &'static str,
    },
    /// Orca answered with an error.
    #[error("orca refused the request ({code}): {message}")]
    Refused {
        /// Orca's stable error code.
        code: String,
        /// Orca's message, redacted and truncated.
        message: String,
    },
    /// The Orca runtime is not running or not reachable.
    #[error("orca runtime is not ready")]
    RuntimeNotReady,
    /// The installed Orca version is outside the supported range.
    #[error("orca {found} is outside the supported range {supported}")]
    UnsupportedVersion {
        /// The reported version, redacted and truncated.
        found: String,
        /// The supported range.
        supported: &'static str,
    },
    /// The runtime does not advertise a feature the adapter requires.
    #[error("orca runtime lacks required feature {0}")]
    MissingRuntimeFeature(&'static str),
    /// A listing was longer than the adapter reads; acting on part of it is unsafe.
    #[error("orca listing exceeded {limit} entries")]
    ListingTooLong {
        /// The entry limit.
        limit: usize,
    },
    /// The schedule was not installed by Kitchen for this house.
    #[error("schedule is not owned by Kitchen for this house")]
    NotKitchenOwned,
    /// The named schedule was not found.
    #[error("schedule not found")]
    ScheduleNotFound,
    /// Several schedules share one Kitchen name; nothing was created.
    #[error("{count} schedules share this name; resolve the duplicates first")]
    DuplicateSchedules {
        /// How many share the name.
        count: usize,
    },
    /// The schedule for this consumer is already active. Installing disabled
    /// never counts an active schedule as installed, and only the separate
    /// activation authority changes state; nothing was changed.
    #[error("the schedule for this consumer is already active; nothing was installed")]
    ScheduleActive,
    /// The schedule installed for this consumer is not the one requested.
    /// Nothing was changed; remove or edit it deliberately, then install again.
    #[error("the installed schedule differs from the requested one in {fields:?}")]
    ScheduleDiffers {
        /// Which parts differ, or could not be read back to compare.
        fields: Vec<ScheduleField>,
    },
    /// A trial run needs a paused schedule.
    #[error("a trial run needs a paused schedule")]
    TrialRequiresPaused,
    /// A create may or may not have happened and no installed schedule was
    /// found; it must not be retried until an inventory shows its outcome.
    #[error("schedule install outcome unknown; reconcile before retrying")]
    InstallUncertain,
    /// After a change, Orca reported a different state than requested.
    #[error("schedule state read back does not match the request")]
    StateMismatch,
    /// The launched worker's branch is not the one requested. Orca prefixes
    /// the name it is given; rename the branch before the first push.
    #[error("launched branch {actual:?} is not the requested {requested}")]
    BranchMismatch {
        /// The branch the caller wanted.
        requested: String,
        /// The branch Orca created, if the receipt names one.
        actual: Option<String>,
    },
    /// The launched worker's branch is not the one requested, and no stop
    /// took effect: the worker still runs on that branch and could push to
    /// it. This needs a person or a later stop; Kitchen holds the launch.
    #[error("worker {worker} still runs on branch {actual:?}, not the requested {requested}")]
    WrongBranchRunning {
        /// The branch the caller wanted.
        requested: String,
        /// The branch Orca created, if the receipt names one.
        actual: Option<String>,
        /// The Orca Dispatch of the worker that still runs.
        worker: String,
    },
    /// Another caller held the reservation for this key for the whole wait.
    /// Nothing was sent to Orca; the effect may still be in flight under that
    /// caller, so reconcile before submitting it again.
    #[error("another caller holds the reservation for this key")]
    ReservationBusy,
    /// The reservation file or directory is a symlink or not a regular file.
    #[error("the reservation directory or file is redirected")]
    ReservationRedirected,
    /// The runtime directory is inside a Git checkout; Kitchen writes nothing
    /// into a working tree. Nothing was sent to Orca.
    #[error("the reservation directory is inside a Git checkout")]
    ReservationInsideRepository,
    /// The reservation could not be taken for another I/O reason; nothing was
    /// sent to Orca.
    #[error("the reservation could not be taken: {0}")]
    ReservationUnavailable(io::ErrorKind),
    /// No worktree name makes Orca create the requested branch on this host
    /// (see [`OrcaConfig::branch_prefix`](crate::adapters::orca::OrcaConfig::branch_prefix)),
    /// so the launch was refused before anything was created.
    #[error("no worktree name makes Orca create the requested branch {requested}")]
    BranchUnobtainable {
        /// The branch the caller wanted.
        requested: String,
    },
    /// An Orca worktree of the repository already has the requested branch
    /// checked out, or the worktree listing could not prove that none does.
    /// Orca would create another branch instead, so the launch was refused
    /// before anything was created. To work on that branch, launch in its
    /// worktree ([`Workspace::Existing`](crate::contracts::Workspace::Existing)).
    #[error("the requested branch {requested} may already exist in an Orca worktree")]
    BranchTaken {
        /// The branch the caller wanted.
        requested: String,
    },
    /// A schedule value was rejected.
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    /// The house's schedule limits refused the change; nothing was sent.
    #[error(transparent)]
    ScheduleLimit(#[from] BudgetError),
    /// A contract value was rejected.
    #[error(transparent)]
    Contract(#[from] ContractError),
}

impl OrcaError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Schedule(_) => ErrorClass::InvalidInput,
            Self::ScheduleLimit(error) => error.class(),
            Self::Contract(error) => error.class(),
            Self::UnsupportedVersion { .. }
            | Self::MissingRuntimeFeature(_)
            | Self::NotKitchenOwned
            | Self::TrialRequiresPaused
            | Self::ReservationRedirected
            | Self::ReservationInsideRepository
            | Self::BranchUnobtainable { .. }
            | Self::Refused { .. } => ErrorClass::Refused,
            Self::DuplicateSchedules { .. }
            | Self::ScheduleActive
            | Self::ScheduleDiffers { .. }
            | Self::ReservationBusy
            | Self::BranchMismatch { .. }
            | Self::BranchTaken { .. }
            | Self::WrongBranchRunning { .. }
            | Self::InstallUncertain
            | Self::StateMismatch
            | Self::ScheduleNotFound => ErrorClass::Conflict,
            Self::Spawn(_)
            | Self::Timeout
            | Self::OutputLimit { .. }
            | Self::Io(_)
            | Self::NoResult { .. }
            | Self::Malformed { .. }
            | Self::RuntimeNotReady
            | Self::ReservationUnavailable(_)
            | Self::ListingTooLong { .. } => ErrorClass::Execution,
        }
    }
}
