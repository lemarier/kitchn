use crate::ErrorClass;

/// Configuration and installation failures never echo private input.
#[derive(Debug, thiserror::Error)]
pub enum HouseError {
    /// A document or bounded value is invalid.
    #[error("invalid house configuration or installation input")]
    InvalidInput,
    /// House selection did not identify exactly one allowed house.
    #[error("house selection is missing, ambiguous, or outside the repository allowlist")]
    HouseSelection,
    /// Repository policy attempts to weaken house policy.
    #[error("repository policy cannot relax house constraints")]
    PolicyRelaxation,
    /// Private configuration was placed inside a repository.
    #[error("house configuration and private state must be outside repositories")]
    InsideRepository,
    /// A write path is redirected or not a regular file/directory.
    #[error("redirected or non-regular installation path")]
    RedirectedPath,
    /// Existing content differs; it is preserved.
    #[error("installation conflicts with existing content; inspect the preview")]
    Conflict,
    /// A pinned installation is absent or no longer matches its manifest.
    #[error("pinned instructions are missing or modified; restore the verified bundle and sync")]
    UnverifiedSnapshot,
    /// The supplied update does not match the selected house or expected revision.
    #[error("bundle house or revision does not match the selected pins")]
    PinMismatch,
    /// A cooperating writer already owns this operation.
    #[error("installation is busy; retry after the owning operation settles")]
    Busy,
    /// Rollback could not safely remove these call-created paths.
    #[error("partial installation remains; inspect the reported paths before retrying")]
    PartialInstallation {
        /// Files or directories requiring inspection; never removed without ownership proof.
        remaining: Vec<std::path::PathBuf>,
    },
    /// Bounded filesystem I/O failed.
    #[error("house storage operation failed ({0:?})")]
    Io(std::io::ErrorKind),
}

impl HouseError {
    /// Common CLI/recovery handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidInput => ErrorClass::InvalidInput,
            Self::HouseSelection
            | Self::PolicyRelaxation
            | Self::InsideRepository
            | Self::RedirectedPath
            | Self::PinMismatch => ErrorClass::Refused,
            Self::Conflict | Self::Busy => ErrorClass::Conflict,
            Self::UnverifiedSnapshot | Self::PartialInstallation { .. } | Self::Io(_) => {
                ErrorClass::Execution
            }
        }
    }
}

impl From<std::io::Error> for HouseError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}
