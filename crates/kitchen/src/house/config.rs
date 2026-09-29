use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{BackendBinding, HouseError, ReadinessLevel, Workflow, readiness::validate_work_type};
use crate::{
    HouseId,
    contracts::{CommitId, Grant, HouseGrants, Repository, Text},
    scheduling::{BudgetError, SchedulePolicy},
    selection::{AgentPolicy, SelectionError},
    workflows::{
        cleanup::DiskPressurePolicy,
        pickup::{DEFAULT_FIX_ROUNDS, DEFAULT_REVIEW_REQUESTS, FollowUpBudget},
    },
};

/// Largest fix-round or review-request budget a house may set.
pub const MAX_FOLLOW_UP: u8 = 10;

/// The tool a house requires for dependent branches and stacked pull
/// requests. When one is configured, Kitchen creates, rebases, retargets,
/// and pushes dependent branches only through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StackTool {
    /// The `gh stack` GitHub CLI extension (`github/gh-stack`).
    GhStack,
}

impl StackTool {
    /// The command a person runs, for diagnostics.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::GhStack => "gh stack",
        }
    }
}

/// How many follow-up rounds a house allows per pull request. Repair, the
/// merge gate, and interactive `pr` can only obtain a
/// [`FollowUpBudget`] through [`HouseConfig::follow_up_budget`]; a session may
/// lower it, never raise it. A field left out keeps the library default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FollowUpPolicy {
    /// Review-feedback fix and repair rounds per pull request.
    #[serde(default = "default_fix_rounds")]
    pub fix_rounds: u8,
    /// Independent review requests per pull request head.
    #[serde(default = "default_review_requests")]
    pub review_requests: u8,
}

const fn default_fix_rounds() -> u8 {
    DEFAULT_FIX_ROUNDS
}

const fn default_review_requests() -> u8 {
    DEFAULT_REVIEW_REQUESTS
}

impl Default for FollowUpPolicy {
    fn default() -> Self {
        Self {
            fix_rounds: DEFAULT_FIX_ROUNDS,
            review_requests: DEFAULT_REVIEW_REQUESTS,
        }
    }
}

impl FollowUpPolicy {
    /// The budget this policy grants.
    #[must_use]
    pub(crate) const fn budget(self) -> FollowUpBudget {
        FollowUpBudget::new(self.fix_rounds, self.review_requests)
    }

    fn validate(self) -> Result<(), HouseError> {
        if self.fix_rounds > MAX_FOLLOW_UP || self.review_requests > MAX_FOLLOW_UP {
            return Err(HouseError::InvalidInput);
        }
        Ok(())
    }
}

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
    /// Maximum permitted actions, including actions requiring per-action interactive consent.
    pub policy_limits: BTreeSet<Grant>,
    /// Standing scheduled grants, separate from interactive consent.
    pub grants: BTreeSet<Grant>,
    /// Agent family, model, and effort per role and work type. Absent means
    /// launches use the backend's default agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentPolicy>,
    /// The stack tool dependent branches must go through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_tool: Option<StackTool>,
    /// Minimum interval and usage budgets for scheduled work. Absent means
    /// schedule installs are not limited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<SchedulePolicy>,
    /// Readiness level each work type must reach before a merge grant.
    /// Readiness never grants merge authority itself.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub merge_readiness: BTreeMap<Text, ReadinessLevel>,
    /// Free space below which the dishwasher starts a preview-only
    /// inspection. Absent means free space is not watched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_pressure: Option<DiskPressurePolicy>,
    /// Fix-round and review-request budgets per pull request. Absent means
    /// the library defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<FollowUpPolicy>,
    /// The worker backend commands build for this house. Absent means no
    /// backend is built: a house registered before bindings existed must
    /// write its Orca binding explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendBinding>,
}

impl HouseConfig {
    /// The house's follow-up budget: its policy, or the library defaults.
    /// Every consumer reads the budget here.
    #[must_use]
    pub fn follow_up_budget(&self) -> FollowUpBudget {
        self.follow_up.unwrap_or_default().budget()
    }

    /// Check bounds and ensure no grant or destination escapes the allowlist.
    pub fn validate(&self) -> Result<(), HouseError> {
        if self.schema != 1
            || self.repositories.is_empty()
            || self.repositories.len() > 256
            || !self.posting_destinations.is_subset(&self.repositories)
            || self.grants.len() > 256
            || self.policy_limits.len() > 256
            || self.merge_readiness.len() > super::MAX_WORK_TYPES
        {
            return Err(HouseError::InvalidInput);
        }
        if let Some(follow_up) = self.follow_up {
            follow_up.validate()?;
        }
        validate_names(&self.required_reviewers)?;
        validate_names(&self.required_checks)?;
        for work_type in self.merge_readiness.keys() {
            validate_work_type(work_type)?;
        }
        if let Some(agents) = &self.agents {
            agents.validate(&self.repositories).map_err(|error| {
                if error == SelectionError::RepositoryNotServed {
                    HouseError::PolicyRelaxation
                } else {
                    HouseError::InvalidInput
                }
            })?;
        }
        for grant in self.grants.iter().chain(&self.policy_limits) {
            use crate::contracts::{GrantScope, Permission};
            let repository_effect = match grant.permission {
                Permission::LaunchWorker
                | Permission::PostComment
                | Permission::EditLabels
                | Permission::CreateIssue
                | Permission::CloseIssue
                | Permission::EditIssueRelationships
                | Permission::PushBranch
                | Permission::OpenPullRequest
                | Permission::RequestReview
                | Permission::Merge
                | Permission::Publish => true,
                Permission::MessageWorker
                | Permission::CancelWorker
                | Permission::ReleaseResource
                | Permission::AskHuman
                | Permission::ManageSchedule
                | Permission::ActivateSchedule
                | Permission::TrialSchedule
                | Permission::OperateEquipment
                | Permission::UseVerificationEnvironment => false,
            };
            if repository_effect {
                let GrantScope::Repository(repository) = &grant.scope else {
                    return Err(HouseError::PolicyRelaxation);
                };
                if grant.permission != Permission::LaunchWorker
                    && !self.posting_destinations.contains(repository)
                {
                    return Err(HouseError::PolicyRelaxation);
                }
            }
            if let crate::contracts::GrantScope::Repository(repo) = &grant.scope
                && !self.repositories.contains(repo)
            {
                return Err(HouseError::PolicyRelaxation);
            }
        }
        if self
            .grants
            .iter()
            .any(|grant| !self.policy_limits.iter().any(|limit| limit.covers(grant)))
        {
            return Err(HouseError::PolicyRelaxation);
        }
        if let Some(schedules) = &self.schedules {
            schedules.validate().map_err(|error| {
                if matches!(error, BudgetError::Relaxation { .. }) {
                    HouseError::PolicyRelaxation
                } else {
                    HouseError::InvalidInput
                }
            })?;
        }
        Ok(())
    }

    /// Translate explicit configured grants to the core authority contract.
    ///
    /// # Errors
    /// Refuses a configuration with a merge grant: those go through
    /// [`HouseConfig::issue_authority`], which checks repository readiness.
    pub fn authority(&self) -> Result<HouseGrants, HouseError> {
        let authority = self.build_authority()?;
        if self
            .grants
            .iter()
            .any(|grant| grant.permission == crate::contracts::Permission::Merge)
        {
            return Err(HouseError::MergeNeedsReadiness);
        }
        Ok(authority)
    }

    pub(super) fn build_authority(&self) -> Result<HouseGrants, HouseError> {
        self.validate()?;
        HouseGrants::with_limits(
            self.house.clone(),
            self.policy_limits.iter().cloned(),
            self.grants.iter().cloned(),
        )
        .map_err(|_| HouseError::PolicyRelaxation)
    }
}

/// Schema of a repository binding stored in the house registry. Schema 1 was
/// the `.kitchen.json` file in a working tree; it is accepted only by the
/// explicit legacy import and rejected everywhere else.
pub const REPOSITORY_BINDING_SCHEMA: u32 = 2;

/// A repository binding, stored in the house registry outside every working
/// tree and keyed by the repository. It contains no credential, private
/// context, or grants. Unknown keys (including attempted house-policy
/// overrides such as `agents`) are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryConfig {
    /// Schema version; currently [`REPOSITORY_BINDING_SCHEMA`].
    pub schema: u32,
    /// The chosen house; never inferred from the only available credential.
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
        if self.schema != REPOSITORY_BINDING_SCHEMA {
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

pub(super) fn validate_names(names: &BTreeSet<String>) -> Result<(), HouseError> {
    if names.len() > 64
        || names
            .iter()
            .any(|name| name.is_empty() || name.len() > 128 || name.chars().any(char::is_control))
    {
        return Err(HouseError::InvalidInput);
    }
    Ok(())
}
