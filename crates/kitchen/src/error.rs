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
            Self::Trust(error) => error.class(),
            Self::Workflow(error) => error.class(),
            Self::Selection(error) => error.class(),
            Self::Event(error) => error.class(),
        }
    }
}

/// Result alias for Kitchen library calls.
pub type Result<T, E = Error> = std::result::Result<T, E>;
