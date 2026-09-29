//! The crate error and its handling classes.
//!
//! Each area owns its error type and converts into [`Error`] through one
//! `#[from]` variant. Callers such as the CLI branch on [`Error::class`], so a
//! new variant in one area never forces edits to unrelated callers.

use crate::{
    IdentifierError, adapters::orca::OrcaError, contracts::ContractError, scaffold::ScaffoldError,
    state::StateError,
};

/// Broad handling class for an [`Error`], for exit codes and retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// The caller supplied invalid input; retrying the same input cannot succeed.
    InvalidInput,
    /// Policy refused the request: authority, capability, house scope, or a bound.
    Refused,
    /// The request conflicts with current durable state; re-read before acting.
    Conflict,
    /// Execution failed: storage I/O, lock deadlines, or invalid persisted state.
    Execution,
}

/// Any Kitchen library failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An identifier was rejected.
    #[error(transparent)]
    Identifier(#[from] IdentifierError),
    /// A contract value or policy check failed.
    #[error(transparent)]
    Contract(#[from] ContractError),
    /// House configuration or adoption failed.
    #[error(transparent)]
    House(#[from] crate::house::HouseError),
    /// A durable state operation failed.
    #[error(transparent)]
    State(#[from] StateError),
    /// An Orca adapter call failed.
    #[error(transparent)]
    Orca(#[from] OrcaError),
    /// A house-scoped integration failed.
    #[error(transparent)]
    Integration(#[from] crate::integrations::github::IntegrationError),
    /// A template, rendering, or scaffold planning operation failed.
    #[error(transparent)]
    Scaffold(#[from] ScaffoldError),
    /// A dishwasher inspection or cleanup failed.
    #[error(transparent)]
    Cleanup(#[from] crate::workflows::cleanup::CleanupError),
    /// A project decomposition was refused or failed.
    #[error(transparent)]
    Decomposition(#[from] crate::workflows::decomposition::DecompositionError),
    /// Trust evidence or autonomy operation failed.
    #[error(transparent)]
    Trust(#[from] crate::trust::TrustError),
    /// A triage or hygiene decision failed.
    #[error(transparent)]
    Workflow(#[from] crate::workflows::WorkflowError),
    /// An agent policy or selection was rejected.
    #[error(transparent)]
    Selection(#[from] crate::selection::SelectionError),
    /// An event was refused or malformed.
    #[error(transparent)]
    Event(#[from] crate::events::EventError),
    /// A verification environment declaration, policy, or access check failed.
    #[error(transparent)]
    Verification(#[from] crate::contracts::VerificationError),
    /// A pickup, coordination, or repair workflow refused its input.
    #[error(transparent)]
    Coordination(#[from] crate::workflows::coordination::CoordinationError),
    /// A schedule interval or usage budget check refused the request.
    #[error(transparent)]
    Budget(#[from] crate::scheduling::BudgetError),
    /// An intake source or report was refused or malformed.
    #[error(transparent)]
    Intake(#[from] crate::workflows::intake::IntakeError),
    /// Guided house registration failed or had missing answers.
    #[error(transparent)]
    HouseInit(#[from] crate::house::HouseInitError),
    /// A forge binding was missing or refused an approved write.
    #[error(transparent)]
    Forge(#[from] crate::house::ForgeError),
    /// A deliberation thread, record, or pin was refused.
    #[error(transparent)]
    Deliberation(#[from] crate::workflows::deliberation::DeliberationError),
    /// An interactive entrypoint refused its input.
    #[error(transparent)]
    Interactive(#[from] crate::workflows::interactive::InteractiveError),
    /// A house's worker backend could not be built or lacks a required capability.
    #[error(transparent)]
    Backend(#[from] crate::adapters::BackendError),
}

impl Error {
    /// The broad handling class of this error.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Identifier(_) => ErrorClass::InvalidInput,
            Self::Contract(error) => error.class(),
            Self::State(error) => error.class(),
            Self::Orca(error) => error.class(),
            Self::House(error) => error.class(),
            Self::Integration(error) => error.class(),
            Self::Scaffold(error) => error.class(),
            Self::Cleanup(error) => error.class(),
            Self::Decomposition(error) => error.class(),
            Self::Trust(error) => error.class(),
            Self::Workflow(error) => error.class(),
            Self::Selection(error) => error.class(),
            Self::Event(error) => error.class(),
            Self::Verification(error) => error.class(),
            Self::Coordination(error) => error.class(),
            Self::Budget(error) => error.class(),
            Self::Intake(error) => error.class(),
            Self::HouseInit(error) => error.class(),
            Self::Forge(error) => error.class(),
            Self::Deliberation(error) => error.class(),
            Self::Interactive(error) => error.class(),
            Self::Backend(error) => error.class(),
        }
    }
}

/// Result alias for Kitchen library calls.
pub type Result<T, E = Error> = std::result::Result<T, E>;
