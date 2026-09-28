//! House grants and the task authority delegated from them.
//!
//! A house grants permissions; a task receives a subset. Task authority is
//! checked against the house's *current* grants before every external effect,
//! so revocation takes effect immediately and a persisted record cannot expand
//! authority on its own.

use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    HouseId,
    contracts::{ContractError, Repository, ValueKind},
};

/// An action a task may be authorized to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Permission {
    /// Launch a worker through an execution backend.
    LaunchWorker,
    /// Send a message to a running worker.
    MessageWorker,
    /// Cancel a running worker.
    CancelWorker,
    /// Release a backend resource such as a terminal or worktree.
    ReleaseResource,
    /// Ask a human for a decision.
    AskHuman,
    /// Post comments on issues or pull requests.
    PostComment,
    /// Change issue or pull-request labels.
    EditLabels,
    /// Push commits to a branch.
    PushBranch,
    /// Open a pull request.
    OpenPullRequest,
    /// Request a review from a reviewer.
    RequestReview,
    /// Merge a pull request.
    Merge,
    /// Publish a release or package.
    Publish,
    /// Operate physical equipment.
    OperateEquipment,
}

impl Permission {
    /// Every permission, in declaration order.
    pub const ALL: [Self; 13] = [
        Self::LaunchWorker,
        Self::MessageWorker,
        Self::CancelWorker,
        Self::ReleaseResource,
        Self::AskHuman,
        Self::PostComment,
        Self::EditLabels,
        Self::PushBranch,
        Self::OpenPullRequest,
        Self::RequestReview,
        Self::Merge,
        Self::Publish,
        Self::OperateEquipment,
    ];

    /// The stable kebab-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaunchWorker => "launch-worker",
            Self::MessageWorker => "message-worker",
            Self::CancelWorker => "cancel-worker",
            Self::ReleaseResource => "release-resource",
            Self::AskHuman => "ask-human",
            Self::PostComment => "post-comment",
            Self::EditLabels => "edit-labels",
            Self::PushBranch => "push-branch",
            Self::OpenPullRequest => "open-pull-request",
            Self::RequestReview => "request-review",
            Self::Merge => "merge",
            Self::Publish => "publish",
            Self::OperateEquipment => "operate-equipment",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Permission {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|permission| permission.as_str() == value)
            .ok_or(ContractError::InvalidValue {
                kind: ValueKind::Permission,
            })
    }
}

/// Where a grant applies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "repository", rename_all = "camelCase")]
pub enum GrantScope {
    /// Every repository the house manages.
    House,
    /// One repository.
    Repository(Repository),
}

impl GrantScope {
    /// Whether this scope includes `other`.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::House, Self::House | Self::Repository(_)) => true,
            (Self::Repository(mine), Self::Repository(theirs)) => mine == theirs,
            (Self::Repository(_), Self::House) => false,
        }
    }
}

impl fmt::Display for GrantScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::House => formatter.write_str("house"),
            Self::Repository(repository) => write!(formatter, "repository {repository}"),
        }
    }
}

/// One permission within one scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    /// The permitted action.
    pub permission: Permission,
    /// Where the action is permitted.
    pub scope: GrantScope,
}

impl Grant {
    /// A grant covering every repository in the house.
    #[must_use]
    pub const fn house(permission: Permission) -> Self {
        Self {
            permission,
            scope: GrantScope::House,
        }
    }

    /// A grant limited to one repository.
    #[must_use]
    pub const fn repository(permission: Permission, repository: Repository) -> Self {
        Self {
            permission,
            scope: GrantScope::Repository(repository),
        }
    }

    /// Whether this grant includes `other`.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.permission == other.permission && self.scope.covers(&other.scope)
    }
}

/// The permissions a house currently grants. Supplied by house configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseGrants {
    house: HouseId,
    grants: BTreeSet<Grant>,
}

impl HouseGrants {
    /// Record a house's grants.
    #[must_use]
    pub fn new(house: HouseId, grants: impl IntoIterator<Item = Grant>) -> Self {
        Self {
            house,
            grants: grants.into_iter().collect(),
        }
    }

    /// The granting house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Whether some house grant covers `grant`.
    #[must_use]
    pub fn covers(&self, grant: &Grant) -> bool {
        self.grants.iter().any(|held| held.covers(grant))
    }
}

/// Authority delegated to one task. Always a subset of its house's grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAuthority {
    house: HouseId,
    grants: BTreeSet<Grant>,
}

impl TaskAuthority {
    /// Delegate `requested` grants from `house`.
    ///
    /// # Errors
    /// Returns [`ContractError::AuthorityExpansion`] naming the first requested grant
    /// (in sorted order) that no house grant covers.
    pub fn delegate(
        house: &HouseGrants,
        requested: impl IntoIterator<Item = Grant>,
    ) -> Result<Self, ContractError> {
        let grants: BTreeSet<Grant> = requested.into_iter().collect();
        ensure_covered(house, &grants)?;
        Ok(Self {
            house: house.house.clone(),
            grants,
        })
    }

    /// The house this authority was delegated from.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// The delegated grants.
    pub fn grants(&self) -> impl Iterator<Item = &Grant> {
        self.grants.iter()
    }

    /// Check that `permission` is delegated for `scope` and still granted by `current`.
    ///
    /// # Errors
    /// Returns [`ContractError::CrossHouse`] when `current` belongs to another house,
    /// [`ContractError::AuthorityExpansion`] when the house no longer grants something
    /// this task holds, and [`ContractError::PermissionDenied`] when the task lacks it.
    pub fn authorize(
        &self,
        current: &HouseGrants,
        permission: Permission,
        scope: &GrantScope,
    ) -> Result<(), ContractError> {
        if current.house != self.house {
            return Err(ContractError::CrossHouse {
                expected: self.house.clone(),
                found: current.house.clone(),
            });
        }
        ensure_covered(current, &self.grants)?;
        let needed = Grant {
            permission,
            scope: scope.clone(),
        };
        if self.grants.iter().any(|held| held.covers(&needed)) {
            Ok(())
        } else {
            Err(ContractError::PermissionDenied { permission })
        }
    }
}

fn ensure_covered(house: &HouseGrants, grants: &BTreeSet<Grant>) -> Result<(), ContractError> {
    match grants.iter().find(|grant| !house.covers(grant)) {
        Some(grant) => Err(ContractError::AuthorityExpansion {
            permission: grant.permission,
            scope: grant.scope.clone(),
        }),
        None => Ok(()),
    }
}
