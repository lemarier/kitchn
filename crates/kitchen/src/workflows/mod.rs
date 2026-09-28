//! Workflow policy built on the core contracts and the durable house store.

pub mod cleanup;
pub mod gardener;
pub mod inspector;
pub mod triage;

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
