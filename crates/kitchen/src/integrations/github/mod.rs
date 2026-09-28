//! GitHub access constrained by an explicit house selection.
//!
//! Configuration supplies [`HouseScope`], an allowlist, requester identity,
//! core credential ID, permitted effects, and a per-task logical posting ceiling.
//! It never supplies secret values through a workflow payload. [`CredentialFile`]
//! resolves the private token only at the CLI boundary; [`GhCli`] clears ambient
//! CLI configuration and verifies the authenticated GitHub login before each call.
//! GitHub credentials that cannot authenticate `/user` are explicitly unavailable.
//!
//! Build a [`GitHubExecutor::effect`], put it in a [`crate::state::EffectPlan`],
//! and call [`crate::state::run_effect`]. The core owns claims, current authority,
//! revision checks, atomic posting budgets, and intent durability. After an
//! uncertain result call [`crate::state::reconcile`]; never submit it directly
//! again. GitHub offers no native idempotency guarantee, so an absent marker is
//! inconclusive and cannot authorize another post. Receipts for created issues
//! and comments contain their forge URL; other receipts identify the intent.
//!
//! Setup first previews [`LabelDefinition::inspect`]. Only missing labels need
//! intents, and execution rechecks the inventory. Conflicting existing labels
//! are refused without renaming, recoloring, or deleting them. Budgets count
//! distinct admitted effects conservatively, including unresolved or no-op
//! effects, rather than promising a wall-clock posting rate limit.

mod mutation;
pub(crate) mod process;
mod scope;
pub use crate::contracts::{
    GitHubAction, GitHubMutation, IssueNumber, LabelDefinition, MergeMethod, PostingBudget,
};
pub use mutation::LabelSetup;
pub use process::{CredentialFile, GhCli};
mod client;
mod evidence;
pub use client::{GitHubClient, GitHubReadTransport, ReadLimits, ReadRequest};
pub use evidence::*;

pub use scope::{CredentialRef, HouseScope};

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

impl From<crate::contracts::ContractError> for IntegrationError {
    fn from(error: crate::contracts::ContractError) -> Self {
        match error.class() {
            ErrorClass::InvalidInput => Self::InvalidInput,
            ErrorClass::Refused => Self::ScopeMismatch,
            ErrorClass::Conflict => Self::StaleDecision,
            ErrorClass::Execution => Self::Unknown,
        }
    }
}
mod executor;
mod provider;
pub use executor::GitHubExecutor;
pub use provider::{GitHubMutationTransport, MutationRequest};
