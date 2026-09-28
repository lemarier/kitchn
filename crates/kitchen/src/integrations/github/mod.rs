//! GitHub access constrained by an explicit house selection.

mod mutation;
pub(crate) mod process;
mod scope;
pub use mutation::{GitHubAction, GitHubMutation, LabelDefinition, LabelSetup};
pub use process::{CredentialFile, GhCli};
mod client;
mod evidence;
pub use client::{GitHubClient, GitHubReadTransport, ReadLimits, ReadRequest};
pub use evidence::*;

pub use scope::{CredentialRef, HouseScope, PostingBudget};

use crate::ErrorClass;

/// Integration failures never contain credential values or response bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationError {
    /// Invalid bounded input or provider response.
    #[error("invalid integration input")]
    InvalidInput,
    /// The selected house, requester, credential, or destination disagrees.
    #[error("integration scope mismatch")]
    ScopeMismatch,
    /// The house does not permit this effect.
    #[error("integration effect is not permitted")]
    PermissionDenied,
    /// All permitted submissions for this durable task have been spent.
    #[error("posting budget exhausted")]
    BudgetExhausted,
    /// A bounded operation timed out.
    #[error("integration deadline exceeded")]
    Timeout,
    /// The provider could not be reached or returned an error.
    #[error("integration unavailable")]
    Unavailable,
    /// Input/output or pagination reached a configured bound.
    #[error("integration resource bound exceeded")]
    LimitExceeded,
    /// A response was malformed or ambiguous.
    #[error("integration response is unknown")]
    Unknown,
    /// The answer refers to an obsolete revision.
    #[error("decision revision is stale")]
    StaleDecision,
}

impl IntegrationError {
    /// Broad handling class used at executable boundaries.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::InvalidInput => ErrorClass::InvalidInput,
            Self::ScopeMismatch | Self::PermissionDenied | Self::BudgetExhausted => {
                ErrorClass::Refused
            }
            Self::StaleDecision => ErrorClass::Conflict,
            Self::Timeout | Self::Unavailable | Self::LimitExceeded | Self::Unknown => {
                ErrorClass::Execution
            }
        }
    }
}
