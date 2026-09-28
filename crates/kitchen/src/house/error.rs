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
    /// A create-only batch was blocked; no files were written.
    #[error("installation conflicts with existing content; inspect the report")]
    Conflicts(crate::adoption::InstallReport),
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
    /// The path is not inside a checkout, or none of its Git remotes names a
    /// GitHub `owner/name` repository.
    #[error("no Git remote of this checkout names a GitHub owner/name repository")]
    RepositoryUnidentified,
    /// More than one house claims the checkout and no choice is stored in the
    /// registry. Choosing a house with setup stores the choice.
    #[error("more than one house claims this repository ({}); choose one with house setup", list(.houses))]
    AmbiguousHouse {
        /// Every house that claims the checkout's repository.
        houses: Vec<crate::HouseId>,
    },
    /// Another remote belongs to a house, and the remote that identifies the
    /// checkout does not belong to that same house.
    #[error("remotes of this checkout do not agree on one house ({}); the first names the checkout, the others belong to another house or none", list(.remotes))]
    RemotesDisagree {
        /// The identifying remote first, then each remote that disagrees.
        remotes: Vec<crate::adoption::RemoteName>,
    },
    /// The legacy binding no longer matches the previewed one.
    #[error("the legacy binding differs from the one that was approved; preview it again")]
    LegacyChanged,
    /// A bounded Git read failed; nothing was decided from it.
    #[error("git could not be read: {0}")]
    Git(crate::git::GitReadError),
    /// A merge grant was attempted below the house's required readiness.
    #[error(
        "repository readiness {} is below the required {}; record an owner decision with a reason to proceed",
        assessed.as_str(),
        required.as_str()
    )]
    BelowReadiness {
        /// Level house policy requires.
        required: super::ReadinessLevel,
        /// Assessed level.
        assessed: super::ReadinessLevel,
    },
    /// An owner decision does not match the assessed scope or levels.
    #[error(
        "readiness decision does not match this house, repository, work type, or assessed level"
    )]
    ReadinessDecision,
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
            | Self::PinMismatch
            | Self::RepositoryUnidentified
            | Self::AmbiguousHouse { .. }
            | Self::RemotesDisagree { .. }
            | Self::BelowReadiness { .. }
            | Self::ReadinessDecision => ErrorClass::Refused,
            Self::Conflict | Self::Conflicts(_) | Self::LegacyChanged | Self::Busy => {
                ErrorClass::Conflict
            }
            Self::UnverifiedSnapshot
            | Self::PartialInstallation { .. }
            | Self::Git(_)
            | Self::Io(_) => ErrorClass::Execution,
        }
    }
}

fn list(values: &[impl std::fmt::Display]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl From<std::io::Error> for HouseError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}
