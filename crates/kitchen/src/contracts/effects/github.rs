//! Forge effects. Owned by #7, which extends this payload.

use serde::{Deserialize, Serialize};

use crate::contracts::{
    Capability, ContractError, EffectContext, GrantScope, Permission, Repository, Text,
};

/// A forge mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum GitHubEffect {
    /// Create a label in one repository.
    #[serde(rename_all = "camelCase")]
    CreateLabel {
        /// The repository.
        repository: Repository,
        /// The label name.
        name: Text,
    },
}

impl GitHubEffect {
    /// The executor capability this effect needs.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::CreateLabel { .. } => Capability::ForgeMutation,
        }
    }

    /// The task permission this effect needs.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::CreateLabel { .. } => Permission::EditLabels,
        }
    }

    /// The repository the effect changes.
    #[must_use]
    pub fn scope(&self) -> GrantScope {
        match self {
            Self::CreateLabel { repository, .. } => GrantScope::Repository(repository.clone()),
        }
    }

    /// Per-submission check; see [`crate::contracts::Effect::check`].
    ///
    /// # Errors
    /// None yet.
    pub const fn check(&self, _context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::CreateLabel { .. } => Ok(()),
        }
    }

    /// Admission hook; see [`crate::contracts::Effect::admit`].
    ///
    /// # Errors
    /// None yet; #7 adds posting budgets here.
    pub const fn admit(&self, _context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::CreateLabel { .. } => Ok(()),
        }
    }
}
