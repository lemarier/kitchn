//! Durable state failures.

use std::{fmt, io};

use crate::{
    BackendId, ConsumerId, ErrorClass, HolderId, TaskId,
    contracts::{AttemptNumber, EffectSeq, EvidenceRevision, Fence, Settlement, Timestamp},
};

/// A bounded collection that reached its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Limit {
    /// Tasks per house store.
    Tasks,
    /// Consumer leases per house store.
    Consumers,
    /// Effects per task.
    Effects,
    /// Evidence items per evidence revision.
    Evidence,
    /// Ownership history entries per task.
    OwnershipHistory,
    /// Remembered consumed message ids per task.
    ConsumedMessages,
}

impl fmt::Display for Limit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tasks => "tasks per house",
            Self::Consumers => "consumer leases per house",
            Self::Effects => "effects per task",
            Self::Evidence => "evidence items per revision",
            Self::OwnershipHistory => "ownership history per task",
            Self::ConsumedMessages => "consumed messages per task",
        })
    }
}

/// The storage step that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StorageOperation {
    /// Creating or resolving the store directory.
    Prepare,
    /// Opening or acquiring the lock file.
    Lock,
    /// Reading the state file.
    Read,
    /// Writing, syncing, or renaming the state file.
    Write,
}

impl fmt::Display for StorageOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Prepare => "prepare",
            Self::Lock => "lock",
            Self::Read => "read",
            Self::Write => "write",
        })
    }
}

/// Why persisted state was rejected. The store never silently resets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Corruption {
    /// The state file is not valid JSON or does not match the schema.
    Syntax {
        /// One-based line of the first problem.
        line: usize,
        /// One-based column of the first problem.
        column: usize,
    },
    /// A task is stored under a key different from its own id.
    TaskKey,
    /// A lease carries a fence the store never issued.
    FenceAhead,
    /// Attempts are not numbered consecutively from one.
    AttemptSequence,
    /// A running attempt does not belong to the current claim.
    RunningAttempt,
    /// Effects are not numbered consecutively from zero.
    EffectSequence,
    /// An effect names another house, task, or a missing attempt.
    EffectReference,
    /// A settled task still has an unresolved effect or running attempt.
    SettledWithWork,
    /// A stored collection exceeds its bound.
    LimitExceeded,
    /// The initialization marker is missing beside a snapshot, or unreadable.
    Marker,
    /// The snapshot belongs to a different store than its marker names.
    StoreIdentity,
    /// Ownership history does not replay to the current claim.
    Ownership,
}

impl fmt::Display for Corruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { line, column } => {
                write!(formatter, "malformed at line {line}, column {column}")
            }
            Self::TaskKey => formatter.write_str("task stored under a different id"),
            Self::FenceAhead => formatter.write_str("lease fence was never issued"),
            Self::AttemptSequence => formatter.write_str("attempt numbers are not consecutive"),
            Self::RunningAttempt => {
                formatter.write_str("running attempt does not belong to the current claim")
            }
            Self::EffectSequence => formatter.write_str("effect numbers are not consecutive"),
            Self::EffectReference => formatter.write_str("effect references another scope"),
            Self::SettledWithWork => formatter.write_str("settled task has unfinished work"),
            Self::LimitExceeded => formatter.write_str("stored collection exceeds its bound"),
            Self::Marker => formatter.write_str("store marker is missing or invalid"),
            Self::Ownership => formatter.write_str("ownership history contradicts the claim"),
            Self::StoreIdentity => {
                formatter.write_str("snapshot belongs to a different store than its marker")
            }
        }
    }
}

/// A durable state failure. Errors carry no file paths or stored text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateError {
    /// No task has this id.
    #[error("task {0} not found")]
    TaskNotFound(TaskId),
    /// A task id was reused with a different specification.
    #[error("task {0} already exists with a different specification")]
    TaskConflict(TaskId),
    /// The task is settled; no further changes are accepted.
    #[error("task {task} is already settled as {settlement}")]
    TaskSettled {
        /// The task.
        task: TaskId,
        /// Its settlement.
        settlement: Settlement,
    },
    /// Another holder has a live claim or lease.
    #[error("held by {holder} until {expires_at}")]
    ClaimHeld {
        /// The live holder.
        holder: HolderId,
        /// When its lease expires unless renewed.
        expires_at: Timestamp,
    },
    /// The lease expired; ownership is uncertain until an explicit takeover.
    #[error("lease expired at {expired_at}; ownership is uncertain until taken over")]
    LeaseExpired {
        /// Expiry time.
        expired_at: Timestamp,
    },
    /// The presented fence does not belong to the current owner.
    #[error("{presented} is stale")]
    StaleFence {
        /// The rejected fence.
        presented: Fence,
    },
    /// A takeover was requested while the current lease is still live.
    #[error("lease is still live until {expires_at}")]
    LeaseLive {
        /// When the live lease expires.
        expires_at: Timestamp,
    },
    /// No consumer lease exists for this scope.
    #[error("consumer {0} holds no lease")]
    ConsumerNotFound(ConsumerId),
    /// The task has no attempt with this number.
    #[error("{0} not found")]
    AttemptNotFound(AttemptNumber),
    /// The operation needs a running attempt owned by the presented fence.
    #[error("no running attempt for this claim")]
    NoRunningAttempt,
    /// Cancellation was requested; no new attempts or effects may start.
    #[error("cancellation requested")]
    CancelRequested,
    /// The target resource was not reported by an applied effect of this task.
    #[error("the task does not own the target resource")]
    ResourceNotOwned,
    /// Effects with unknown outcomes must be reconciled first.
    #[error("{count} effect(s) have unresolved outcomes; reconcile before continuing")]
    UnresolvedEffects {
        /// Number of unresolved effects.
        count: usize,
    },
    /// Resubmitting an uncertain effect is unsafe without provider idempotency.
    #[error("effect {0} has an uncertain outcome and the backend cannot deduplicate a retry")]
    UnsafeRetry(EffectSeq),
    /// The logical effect used its submission budget (the task's retry
    /// policy, by count and elapsed time) without an established outcome.
    #[error("effect {0} exhausted its submission budget; hand it over for a decision")]
    SubmissionBudgetExhausted(EffectSeq),
    /// No effect has this number.
    #[error("effect {0} not found")]
    EffectNotFound(EffectSeq),
    /// An effect name was reused within an attempt for a different operation.
    #[error("effect {0} already uses this name for a different operation")]
    EffectNameConflict(EffectSeq),
    /// The effect was persisted for another backend namespace; only that
    /// backend may execute or reconcile it.
    #[error("effect {seq} belongs to backend {recorded}")]
    BackendMismatch {
        /// The effect.
        seq: EffectSeq,
        /// The backend namespace recorded with its intent.
        recorded: BackendId,
    },
    /// A risk decision needs a handed-over effect without another decision.
    #[error("effect {0} is not handed over for a decision")]
    NotHandedOver(EffectSeq),
    /// A risk decision names a different effect.
    #[error("the decision does not name effect {0}")]
    DecisionScope(EffectSeq),
    /// A reported outcome contradicts the recorded one.
    #[error("effect {0} already has a contradicting recorded outcome")]
    ConflictingOutcome(EffectSeq),
    /// An attempt outcome contradicts the recorded one.
    #[error("attempt outcome contradicts the recorded outcome")]
    ConflictingAttemptOutcome,
    /// The decision was made on evidence that has since changed.
    #[error("decision made at {decided}, but the task is at {current}")]
    StaleDecision {
        /// Revision the caller decided on.
        decided: EvidenceRevision,
        /// Current revision.
        current: EvidenceRevision,
    },
    /// A bounded collection is full.
    #[error("limit reached: {limit}")]
    CapacityExceeded {
        /// Which bound.
        limit: Limit,
    },
    /// Runtime state must live outside any Git checkout.
    #[error("state directory is inside a Git checkout; use house-scoped runtime storage")]
    StorageInsideRepository,
    /// The store lock was not acquired before the deadline.
    #[error("state lock not acquired within {waited_ms} ms")]
    LockTimeout {
        /// Time spent waiting.
        waited_ms: u64,
    },
    /// A storage call failed.
    #[error("storage {operation} failed")]
    Io {
        /// The failed step.
        operation: StorageOperation,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The state file exceeds the configured size bound.
    #[error("state file exceeds {limit_bytes} bytes")]
    StateTooLarge {
        /// The bound.
        limit_bytes: u64,
    },
    /// The snapshot of an established store is missing. The store never
    /// recreates it implicitly, since that would silently forget ownership.
    #[error("state file is missing")]
    StateMissing,
    /// The directory holds no initialized store.
    #[error("no store is initialized in this directory")]
    NotInitialized,
    /// The directory already holds a store or a snapshot.
    #[error("a store is already initialized in this directory")]
    AlreadyInitialized,
    /// The store directory or a managed file is a symlink or not a regular file.
    #[error("store path is redirected or not a regular file")]
    RedirectedPath,
    /// The state file uses an unknown schema version.
    #[error("unsupported state schema version {found}")]
    UnsupportedSchema {
        /// The stored version.
        found: u64,
    },
    /// The state file is invalid.
    #[error("persisted state is invalid: {0}")]
    CorruptState(Corruption),
}

impl StateError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::SubmissionBudgetExhausted(_) => ErrorClass::Refused,
            Self::TaskNotFound(_)
            | Self::TaskConflict(_)
            | Self::TaskSettled { .. }
            | Self::ClaimHeld { .. }
            | Self::LeaseExpired { .. }
            | Self::StaleFence { .. }
            | Self::LeaseLive { .. }
            | Self::ConsumerNotFound(_)
            | Self::AttemptNotFound(_)
            | Self::NoRunningAttempt
            | Self::CancelRequested
            | Self::ResourceNotOwned
            | Self::UnresolvedEffects { .. }
            | Self::EffectNotFound(_)
            | Self::EffectNameConflict(_)
            | Self::BackendMismatch { .. }
            | Self::NotHandedOver(_)
            | Self::DecisionScope(_)
            | Self::ConflictingOutcome(_)
            | Self::ConflictingAttemptOutcome
            | Self::StaleDecision { .. }
            | Self::NotInitialized
            | Self::AlreadyInitialized => ErrorClass::Conflict,
            Self::UnsafeRetry(_)
            | Self::CapacityExceeded { .. }
            | Self::StorageInsideRepository
            | Self::RedirectedPath => ErrorClass::Refused,
            Self::LockTimeout { .. }
            | Self::Io { .. }
            | Self::StateTooLarge { .. }
            | Self::StateMissing
            | Self::UnsupportedSchema { .. }
            | Self::CorruptState(_) => ErrorClass::Execution,
        }
    }

    pub(crate) const fn io(operation: StorageOperation, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}
