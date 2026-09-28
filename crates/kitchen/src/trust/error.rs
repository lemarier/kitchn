//! Structured trust failures without private payloads.
use crate::ErrorClass;

/// Evidence, autonomy, inspection, or runtime storage failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TrustError {
    /// The shared core authority model refused the action.
    #[error(transparent)]
    Authority(#[from] crate::contracts::ContractError),
    /// Malformed or inconsistent input.
    #[error("invalid trust input")]
    Invalid,
    /// House, scope, independence, or authority mismatch.
    #[error("trust policy refused the operation")]
    Refused,
    /// An immutable identity was reused with different content.
    #[error("trust history conflict")]
    Conflict,
    /// A required record or predecessor is absent.
    #[error("trust evidence is incomplete")]
    Incomplete,
    /// The requested grant identity has never been proposed or issued.
    #[error("trust grant not found")]
    NotFound,
    /// The history-entry or inspection budget is exhausted. A snapshot that
    /// would outgrow its byte bound is reported as [`Self::Storage`] with
    /// `StateTooLarge`.
    #[error("trust budget exhausted")]
    Exhausted,
    /// Persisted history violates a ledger invariant; never reset automatically.
    /// Any snapshot that fails validation on load is reported as this, not as
    /// the class the same violation would have at write time.
    #[error("invalid trust storage")]
    Corrupt,
    /// The shared snapshot store failed: lock deadline, unsafe path, missing
    /// or corrupted files, size bound, or I/O.
    #[error(transparent)]
    Storage(#[from] crate::state::StateError),
}
impl TrustError {
    /// Handling class shared by CLI callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Authority(error) => error.class(),
            Self::Invalid => ErrorClass::InvalidInput,
            Self::Refused | Self::NotFound | Self::Exhausted => ErrorClass::Refused,
            Self::Conflict | Self::Incomplete => ErrorClass::Conflict,
            Self::Corrupt => ErrorClass::Execution,
            Self::Storage(error) => error.class(),
        }
    }
}
