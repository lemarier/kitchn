//! Structured trust failures without private payloads.
use crate::ErrorClass;

/// Evidence, autonomy, inspection, or runtime storage failure.
#[derive(Debug, thiserror::Error)]
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
    /// Storage or inspection bound exhausted.
    #[error("trust budget exhausted")]
    Exhausted,
    /// Runtime path is redirected, public, or within a repository.
    #[error("unsafe trust storage path")]
    UnsafePath,
    /// Persisted data is invalid or absent; never reset automatically.
    #[error("invalid trust storage")]
    Corrupt,
    /// Bounded lock acquisition failed.
    #[error("trust storage is busy")]
    Busy,
    /// Storage failed, without exposing file contents or paths.
    #[error("trust storage I/O failed: {0}")]
    Io(std::io::ErrorKind),
}
impl TrustError {
    /// Handling class shared by CLI callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Authority(error) => error.class(),
            Self::Invalid => ErrorClass::InvalidInput,
            Self::Refused | Self::Exhausted | Self::UnsafePath => ErrorClass::Refused,
            Self::Conflict | Self::Incomplete => ErrorClass::Conflict,
            Self::Corrupt | Self::Busy | Self::Io(_) => ErrorClass::Execution,
        }
    }
}
impl From<std::io::Error> for TrustError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}
