//! Workflow policy built on the core contracts and the durable house store.
//!
//! Workflows are trigger-neutral: the same code serves a scheduled run and an
//! interactive session. The [`crate::contracts::Claimant`] passed in decides
//! where effect authority comes from, and both triggers share the same
//! durable claims.

pub mod cleanup;
pub mod coordination;
pub mod gardener;
pub mod gate;
pub mod inspector;
pub mod pickup;
pub mod repair;
pub mod triage;

use crate::{integrations::github::Observation, scheduling::PrecheckOutcome};

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 50 && !label.chars().any(char::is_control)
}

/// A read that did not complete stops the pass: unavailable is a failure,
/// incomplete or malformed evidence is never treated as empty.
fn known<T>(observation: Observation<T>) -> Result<T, WorkflowError> {
    match observation {
        Observation::Known(value) => Ok(value),
        Observation::Unavailable(_) => Err(WorkflowError::PrecheckFailed),
        Observation::Unknown => Err(WorkflowError::IncompleteEvidence),
    }
}

/// Workflow input or evidence failure. Private issue content is never included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    /// The input is incomplete or inconsistent.
    #[error("incomplete workflow evidence")]
    IncompleteEvidence,
    /// An answered decision has no declared consumer.
    #[error("unknown decision owner")]
    UnknownDecisionOwner,
    /// The decision does not match this workflow's scope or revision.
    #[error("decision scope mismatch")]
    DecisionMismatch,
    /// A precheck read failed.
    #[error("workflow precheck failed")]
    PrecheckFailed,
}

impl WorkflowError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(self) -> crate::ErrorClass {
        match self {
            Self::IncompleteEvidence | Self::UnknownDecisionOwner => {
                crate::ErrorClass::InvalidInput
            }
            Self::DecisionMismatch => crate::ErrorClass::Refused,
            Self::PrecheckFailed => crate::ErrorClass::Execution,
        }
    }
}

/// Typed precheck outcome; read failures cannot masquerade as idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precheck {
    /// There is no changed input to inspect.
    Idle,
    /// At least one item needs inspection.
    Actionable,
}

/// The schedule outcome of a precheck result. Any error is
/// [`PrecheckOutcome::Error`], never idle.
#[must_use]
pub const fn precheck_outcome(result: Result<Precheck, WorkflowError>) -> PrecheckOutcome {
    match result {
        Ok(Precheck::Actionable) => PrecheckOutcome::Actionable,
        Ok(Precheck::Idle) => PrecheckOutcome::Idle,
        Err(_) => PrecheckOutcome::Error,
    }
}

/// Durable ownership observation. An unknown claim cannot be treated as free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    /// The house store confirms no active claim.
    Unclaimed,
    /// Another worker owns the item.
    ClaimedByOther,
    /// The store could not establish ownership.
    Unknown,
}
