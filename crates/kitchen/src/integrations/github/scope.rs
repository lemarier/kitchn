//! Narrow house input supplied by configuration; contains no credentials.

use std::collections::BTreeSet;

use crate::{
    CredentialId, HouseId,
    contracts::{ExternalRef, Permission, Repository},
};

use super::{IntegrationError, PostingBudget};

/// Reference to a credential selected in private house storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRef {
    house: HouseId,
    name: CredentialId,
    requester: ExternalRef,
}

impl CredentialRef {
    /// Associate a private credential reference with its house and requester.
    #[must_use]
    pub const fn new(house: HouseId, name: CredentialId, requester: ExternalRef) -> Self {
        Self {
            house,
            name,
            requester,
        }
    }

    /// Owning house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Name resolved by the private credential provider, never a secret value.
    #[must_use]
    pub const fn name(&self) -> &CredentialId {
        &self.name
    }

    /// Expected authenticated requester.
    #[must_use]
    pub const fn requester(&self) -> &ExternalRef {
        &self.requester
    }
}

/// Selected house policy boundary. Configuration constructs this small input.
#[derive(Debug, Clone)]
pub struct HouseScope {
    house: HouseId,
    repositories: BTreeSet<Repository>,
    requester: ExternalRef,
    credential: CredentialRef,
    budget: PostingBudget,
    permitted: BTreeSet<Permission>,
}

impl HouseScope {
    /// Validate a house selection before any credential access.
    ///
    /// # Errors
    /// Refuses credentials from another house or requester, or an empty allowlist.
    pub fn new(
        house: HouseId,
        repositories: impl IntoIterator<Item = Repository>,
        requester: ExternalRef,
        credential: CredentialRef,
        budget: PostingBudget,
        permitted: impl IntoIterator<Item = Permission>,
    ) -> Result<Self, IntegrationError> {
        if credential.house != house || credential.requester != requester {
            return Err(IntegrationError::ScopeMismatch);
        }
        let repositories: BTreeSet<_> = repositories.into_iter().collect();
        if repositories.is_empty() {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(Self {
            house,
            repositories,
            requester,
            credential,
            budget,
            permitted: permitted.into_iter().collect(),
        })
    }

    /// Selected house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Selected requester.
    #[must_use]
    pub const fn requester(&self) -> &ExternalRef {
        &self.requester
    }

    /// Credential reference resolved only after scope validation.
    #[must_use]
    pub const fn credential(&self) -> &CredentialRef {
        &self.credential
    }

    /// Durable task submission ceiling.
    #[must_use]
    pub const fn budget(&self) -> PostingBudget {
        self.budget
    }

    /// Check a read destination before accessing credentials or transport.
    ///
    /// # Errors
    /// Refuses a foreign house or repository.
    pub fn authorize_read(
        &self,
        house: &HouseId,
        repository: &Repository,
    ) -> Result<(), IntegrationError> {
        if house != &self.house || !self.repositories.contains(repository) {
            return Err(IntegrationError::ScopeMismatch);
        }
        Ok(())
    }

    /// Check effect permission and durable submissions already spent.
    ///
    /// # Errors
    /// Refuses scope mismatch, disallowed effects, and exhausted budgets.
    pub fn authorize_effect(
        &self,
        house: &HouseId,
        repository: &Repository,
        permission: Permission,
        submissions: u32,
    ) -> Result<(), IntegrationError> {
        self.authorize_read(house, repository)?;
        if !self.permitted.contains(&permission) {
            return Err(IntegrationError::MissingPermission(permission));
        }
        if submissions >= self.budget.limit() {
            return Err(IntegrationError::BudgetExhausted);
        }
        Ok(())
    }
}
