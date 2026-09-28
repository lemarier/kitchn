use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{HouseError, Workflow};
use crate::{
    HouseId,
    contracts::{CommitId, Grant, HouseGrants, Repository},
};

/// Strict house policy. Stored outside all repository checkouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HouseConfig {
    /// Schema version; currently one.
    pub schema: u32,
    /// Stable house identity.
    pub house: HouseId,
    /// Immutable Kitchen instruction revision.
    pub kitchen: CommitId,
    /// Immutable house guidance revision.
    pub guidance: CommitId,
    /// Repositories this house may serve.
    pub repositories: BTreeSet<Repository>,
    /// Destinations at which explicitly authorized tasks may post.
    pub posting_destinations: BTreeSet<Repository>,
    /// Required reviewer identities; repository additions cannot remove these.
    pub required_reviewers: BTreeSet<String>,
    /// Required checks; repository additions cannot remove these.
    pub required_checks: BTreeSet<String>,
    /// Standing scheduled grants, separate from interactive consent.
    pub grants: BTreeSet<Grant>,
}

impl HouseConfig {
    /// Check bounds and ensure no grant or destination escapes the allowlist.
    pub fn validate(&self) -> Result<(), HouseError> {
        if self.schema != 1
            || self.repositories.is_empty()
            || self.repositories.len() > 256
            || !self.posting_destinations.is_subset(&self.repositories)
            || self.grants.len() > 256
        {
            return Err(HouseError::InvalidInput);
        }
        validate_names(&self.required_reviewers)?;
        validate_names(&self.required_checks)?;
        for grant in &self.grants {
            if let crate::contracts::GrantScope::Repository(repo) = &grant.scope
                && !self.repositories.contains(repo)
            {
                return Err(HouseError::PolicyRelaxation);
            }
        }
        Ok(())
    }

    /// Translate explicit configured grants to the core authority contract.
    pub fn authority(&self) -> Result<HouseGrants, HouseError> {
        self.validate()?;
        Ok(HouseGrants::new(
            self.house.clone(),
            self.grants.iter().cloned(),
        ))
    }
}

/// Public repository settings contain no credential, private context, or grants.
/// Unknown keys (including attempted house-policy overrides) are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryConfig {
    /// Schema version; currently one.
    pub schema: u32,
    /// Explicit house; never inferred from the only available credential.
    pub house: HouseId,
    /// Forge identity, checked against the house allowlist.
    pub repository: Repository,
    /// Workflows selected for this repository; selection does not start them.
    pub workflows: BTreeSet<Workflow>,
    /// Additional reviewers, unioned with house requirements.
    pub additional_reviewers: BTreeSet<String>,
    /// Additional checks, unioned with house requirements.
    pub additional_checks: BTreeSet<String>,
}

impl RepositoryConfig {
    /// Validate this repository under the selected house.
    pub fn validate(&self, house: &HouseConfig) -> Result<(), HouseError> {
        house.validate()?;
        if self.schema != 1 {
            return Err(HouseError::InvalidInput);
        }
        if self.house != house.house || !house.repositories.contains(&self.repository) {
            return Err(HouseError::HouseSelection);
        }
        validate_names(&self.additional_reviewers)?;
        validate_names(&self.additional_checks)
    }

    /// Effective reviewer requirements can only grow at repository scope.
    pub fn reviewers(&self, house: &HouseConfig) -> Result<BTreeSet<String>, HouseError> {
        self.validate(house)?;
        Ok(house
            .required_reviewers
            .union(&self.additional_reviewers)
            .cloned()
            .collect())
    }

    /// Effective check requirements can only grow at repository scope.
    pub fn checks(&self, house: &HouseConfig) -> Result<BTreeSet<String>, HouseError> {
        self.validate(house)?;
        Ok(house
            .required_checks
            .union(&self.additional_checks)
            .cloned()
            .collect())
    }
}

/// Resolve one explicit repository binding; duplicate matching house entries fail
/// closed even when their current policies happen to match.
pub fn resolve_house<'a>(
    repository: &RepositoryConfig,
    houses: &'a [HouseConfig],
) -> Result<&'a HouseConfig, HouseError> {
    let mut matches = houses
        .iter()
        .filter(|house| house.house == repository.house);
    let house = matches.next().ok_or(HouseError::HouseSelection)?;
    if matches.next().is_some() {
        return Err(HouseError::HouseSelection);
    }
    repository.validate(house)?;
    Ok(house)
}

fn validate_names(names: &BTreeSet<String>) -> Result<(), HouseError> {
    if names.len() > 64
        || names
            .iter()
            .any(|name| name.is_empty() || name.len() > 128 || name.chars().any(char::is_control))
    {
        return Err(HouseError::InvalidInput);
    }
    Ok(())
}
