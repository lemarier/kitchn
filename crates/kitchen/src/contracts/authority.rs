//! House grants and the task authority delegated from them.
//!
//! A grant is an exact tuple: a permission, where it applies, the destination
//! backend it may act on, and the house-owned credential to use. A house has
//! two sets of grants:
//!
//! - *Limits*: everything house policy permits at all. Nothing, including a
//!   person's consent in an interactive session, can exceed them.
//! - *Standing grants*: the subset usable without a person present. Scheduled
//!   work acts only on these, through the task authority delegated from them.
//!
//! Task authority is checked against the house's *current* standing grants
//! before every external effect, so revocation takes effect immediately and a
//! persisted record cannot expand authority on its own.
//!
//! Grants of a [target-scoped](Permission::is_target_scoped) permission also
//! name the verification targets they cover. Such a grant with no targets,
//! including one stored before targets existed, covers no target.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, CredentialId, HouseId,
    contracts::{
        ContractError, MAX_VERIFICATION_ENVIRONMENTS, Repository, ValueKind, VerificationTarget,
    },
};

closed_names! {
    /// An action a task may be authorized to take.
    #[non_exhaustive]
    pub enum Permission(ValueKind::Permission) {
        /// Launch a worker through an execution backend.
        LaunchWorker = "launch-worker",
        /// Send a message or a reply to a running worker.
        MessageWorker = "message-worker",
        /// Cancel a running worker.
        CancelWorker = "cancel-worker",
        /// Release a backend resource such as a terminal or worktree.
        ReleaseResource = "release-resource",
        /// Ask a human for a decision.
        AskHuman = "ask-human",
        /// Post comments on issues or pull requests.
        PostComment = "post-comment",
        /// Create or change issue and pull-request labels.
        EditLabels = "edit-labels",
        /// Open an issue.
        CreateIssue = "create-issue",
        /// Close an issue. Granted only explicitly; no other permission implies it.
        CloseIssue = "close-issue",
        /// Change issue relationships such as blocked-by links and sub-issues.
        EditIssueRelationships = "edit-issue-relationships",
        /// Push commits to a branch.
        PushBranch = "push-branch",
        /// Open a pull request.
        OpenPullRequest = "open-pull-request",
        /// Request a review from a reviewer.
        RequestReview = "request-review",
        /// Merge a pull request.
        Merge = "merge",
        /// Install, inspect, pause, or remove a schedule without activating it.
        ManageSchedule = "manage-schedule",
        /// Activate a schedule so it runs unattended.
        ActivateSchedule = "activate-schedule",
        /// Run a schedule once as a trial.
        TrialSchedule = "trial-schedule",
        /// Publish a release or package.
        Publish = "publish",
        /// Operate physical equipment.
        OperateEquipment = "operate-equipment",
        /// Use a VM or device verification environment on a backend. Never
        /// implies operating equipment; a device also needs that permission.
        UseVerificationEnvironment = "use-verification-environment",
    }
}

impl Permission {
    /// Whether grants of this permission name the verification targets they
    /// cover. Only [`TaskAuthority::authorize_target`] authorizes these
    /// permissions, and only for a target a grant names.
    #[must_use]
    pub const fn is_target_scoped(self) -> bool {
        match self {
            Self::OperateEquipment | Self::UseVerificationEnvironment => true,
            Self::LaunchWorker
            | Self::MessageWorker
            | Self::CancelWorker
            | Self::ReleaseResource
            | Self::AskHuman
            | Self::PostComment
            | Self::EditLabels
            | Self::CreateIssue
            | Self::CloseIssue
            | Self::EditIssueRelationships
            | Self::PushBranch
            | Self::OpenPullRequest
            | Self::RequestReview
            | Self::Merge
            | Self::ManageSchedule
            | Self::ActivateSchedule
            | Self::TrialSchedule
            | Self::Publish => false,
        }
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

/// One permission within one scope, on one destination backend, with one
/// credential. There are no wildcard destinations or credential fallbacks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RawGrant")]
pub struct Grant {
    /// The permitted action.
    pub permission: Permission,
    /// Where the action is permitted.
    pub scope: GrantScope,
    /// The backend namespace the action may target.
    pub destination: BackendId,
    /// The house-owned credential the action uses.
    pub credential: CredentialId,
    /// The verification targets a [target-scoped](Permission::is_target_scoped)
    /// grant covers; empty for every other permission. Empty on a
    /// target-scoped grant covers no target, so a stored grant from before
    /// targets existed stays readable but authorizes nothing.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub targets: BTreeSet<VerificationTarget>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawGrant {
    permission: Permission,
    scope: GrantScope,
    destination: BackendId,
    credential: CredentialId,
    #[serde(default)]
    targets: BTreeSet<VerificationTarget>,
}

impl TryFrom<RawGrant> for Grant {
    type Error = ContractError;

    fn try_from(raw: RawGrant) -> Result<Self, ContractError> {
        Self {
            permission: raw.permission,
            scope: raw.scope,
            destination: raw.destination,
            credential: raw.credential,
            targets: BTreeSet::new(),
        }
        .with_targets(raw.targets)
    }
}

impl Grant {
    /// A grant covering every repository in the house.
    #[must_use]
    pub const fn house(
        permission: Permission,
        destination: BackendId,
        credential: CredentialId,
    ) -> Self {
        Self {
            permission,
            scope: GrantScope::House,
            destination,
            credential,
            targets: BTreeSet::new(),
        }
    }

    /// A grant limited to one repository.
    #[must_use]
    pub const fn repository(
        permission: Permission,
        repository: Repository,
        destination: BackendId,
        credential: CredentialId,
    ) -> Self {
        Self {
            permission,
            scope: GrantScope::Repository(repository),
            destination,
            credential,
            targets: BTreeSet::new(),
        }
    }

    /// Limit a [target-scoped](Permission::is_target_scoped) grant to
    /// `targets`, replacing any it named.
    ///
    /// # Errors
    /// Returns [`ContractError::InvalidValue`] when targets are named for a
    /// permission that is not target-scoped, or when there are more than
    /// [`MAX_VERIFICATION_ENVIRONMENTS`].
    pub fn with_targets(
        mut self,
        targets: impl IntoIterator<Item = VerificationTarget>,
    ) -> Result<Self, ContractError> {
        self.targets = targets.into_iter().collect();
        let misplaced = !self.targets.is_empty() && !self.permission.is_target_scoped();
        if misplaced || self.targets.len() > MAX_VERIFICATION_ENVIRONMENTS {
            return Err(ContractError::InvalidValue {
                kind: ValueKind::VerificationTarget,
            });
        }
        Ok(self)
    }

    /// Whether this grant includes `other`: the same permission, destination,
    /// and credential, in a scope that covers the other's, naming every
    /// target the other names.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.permission == other.permission
            && self.destination == other.destination
            && self.credential == other.credential
            && self.scope.covers(&other.scope)
            && other.targets.is_subset(&self.targets)
    }
}

/// A house's grants: the limits of its policy and the standing subset.
/// Supplied by house configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseGrants {
    house: HouseId,
    limits: BTreeSet<Grant>,
    standing: BTreeSet<Grant>,
}

impl HouseGrants {
    /// A house whose policy permits exactly its standing grants.
    #[must_use]
    pub fn new(house: HouseId, standing: impl IntoIterator<Item = Grant>) -> Self {
        let standing: BTreeSet<Grant> = standing.into_iter().collect();
        Self {
            house,
            limits: standing.clone(),
            standing,
        }
    }

    /// A house whose policy `limits` exceed its `standing` grants, so that
    /// some actions need a person's consent.
    ///
    /// # Errors
    /// Returns [`ContractError::AuthorityExpansion`] naming a standing grant
    /// the limits do not cover.
    pub fn with_limits(
        house: HouseId,
        limits: impl IntoIterator<Item = Grant>,
        standing: impl IntoIterator<Item = Grant>,
    ) -> Result<Self, ContractError> {
        let limits: BTreeSet<Grant> = limits.into_iter().collect();
        let standing: BTreeSet<Grant> = standing.into_iter().collect();
        ensure_covered(&limits, &standing)?;
        Ok(Self {
            house,
            limits,
            standing,
        })
    }

    /// The granting house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// A copy whose standing grants also include `extra`. Every extra grant
    /// must be covered by the house policy limits; none is dropped silently.
    ///
    /// # Errors
    /// Returns [`ContractError::AuthorityExpansion`] naming the first extra
    /// grant (in sorted order) the limits do not cover.
    pub fn with_added_standing(
        &self,
        extra: impl IntoIterator<Item = Grant>,
    ) -> Result<Self, ContractError> {
        let extra: BTreeSet<Grant> = extra.into_iter().collect();
        ensure_covered(&self.limits, &extra)?;
        let mut standing = self.standing.clone();
        standing.extend(extra);
        Ok(Self {
            house: self.house.clone(),
            limits: self.limits.clone(),
            standing,
        })
    }

    /// Whether some standing grant covers `grant`.
    #[must_use]
    pub fn covers(&self, grant: &Grant) -> bool {
        self.standing.iter().any(|held| held.covers(grant))
    }

    /// The credential house policy allows for `permission` in `scope` on
    /// `destination`, regardless of standing grants. Interactive consent is
    /// bounded by this check. A [target-scoped](Permission::is_target_scoped)
    /// permission is never permitted without a target.
    ///
    /// # Errors
    /// Returns [`ContractError::AuthorityExpansion`] when policy does not
    /// permit the action and [`ContractError::AmbiguousCredential`] when two
    /// equally specific grants name different credentials.
    pub fn permitted(
        &self,
        permission: Permission,
        scope: &GrantScope,
        destination: &BackendId,
    ) -> Result<CredentialId, ContractError> {
        select_credential(&self.limits, permission, scope, destination, None)?.ok_or_else(|| {
            ContractError::AuthorityExpansion {
                permission,
                scope: scope.clone(),
            }
        })
    }
}

/// Authority delegated to one task. Always a subset of its house's standing
/// grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskAuthority {
    house: HouseId,
    grants: BTreeSet<Grant>,
}

impl TaskAuthority {
    /// Delegate `requested` grants from `house`'s standing grants.
    ///
    /// # Errors
    /// Returns [`ContractError::AuthorityExpansion`] naming the first requested grant
    /// (in sorted order) that no standing grant covers.
    pub fn delegate(
        house: &HouseGrants,
        requested: impl IntoIterator<Item = Grant>,
    ) -> Result<Self, ContractError> {
        let grants: BTreeSet<Grant> = requested.into_iter().collect();
        ensure_covered(&house.standing, &grants)?;
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

    /// Check that `permission` on `destination` is delegated for `scope` and
    /// still granted by `current`, and return the credential to use. A
    /// [target-scoped](Permission::is_target_scoped) permission is refused
    /// here; use [`Self::authorize_target`].
    ///
    /// # Errors
    /// Returns [`ContractError::CrossHouse`] when `current` belongs to another house,
    /// [`ContractError::AuthorityExpansion`] when the house no longer grants something
    /// this task holds, [`ContractError::PermissionDenied`] when the task lacks it,
    /// and [`ContractError::AmbiguousCredential`] when two equally specific
    /// grants name different credentials.
    pub fn authorize(
        &self,
        current: &HouseGrants,
        permission: Permission,
        scope: &GrantScope,
        destination: &BackendId,
    ) -> Result<CredentialId, ContractError> {
        self.authorize_for(current, permission, scope, destination, None)
    }

    /// [`Self::authorize`] for one verification target: a
    /// [target-scoped](Permission::is_target_scoped) permission is authorized
    /// only by a grant naming `target`. Other permissions carry no targets, so
    /// for them `target` plays no part and this equals [`Self::authorize`].
    ///
    /// # Errors
    /// As for [`Self::authorize`], with [`ContractError::PermissionDenied`]
    /// when no delegated grant names `target`.
    pub fn authorize_target(
        &self,
        current: &HouseGrants,
        permission: Permission,
        scope: &GrantScope,
        destination: &BackendId,
        target: &VerificationTarget,
    ) -> Result<CredentialId, ContractError> {
        self.authorize_for(current, permission, scope, destination, Some(target))
    }

    fn authorize_for(
        &self,
        current: &HouseGrants,
        permission: Permission,
        scope: &GrantScope,
        destination: &BackendId,
        target: Option<&VerificationTarget>,
    ) -> Result<CredentialId, ContractError> {
        if current.house != self.house {
            return Err(ContractError::CrossHouse {
                expected: self.house.clone(),
                found: current.house.clone(),
            });
        }
        ensure_covered(&current.standing, &self.grants)?;
        select_credential(&self.grants, permission, scope, destination, target)?
            .ok_or(ContractError::PermissionDenied { permission })
    }
}

/// The credential of the most specific grant covering the action. A
/// repository grant is more specific than a house grant. A target-scoped
/// permission matches only grants naming `target`.
fn select_credential(
    grants: &BTreeSet<Grant>,
    permission: Permission,
    scope: &GrantScope,
    destination: &BackendId,
    target: Option<&VerificationTarget>,
) -> Result<Option<CredentialId>, ContractError> {
    let in_target = |grant: &Grant| {
        !permission.is_target_scoped() || target.is_some_and(|named| grant.targets.contains(named))
    };
    let matching = |specific: bool| {
        grants.iter().filter(move |grant| {
            grant.permission == permission
                && &grant.destination == destination
                && grant.scope.covers(scope)
                && in_target(grant)
                && matches!(grant.scope, GrantScope::Repository(_)) == specific
        })
    };
    for specific in [true, false] {
        let mut credentials = matching(specific).map(|grant| &grant.credential);
        if let Some(first) = credentials.next() {
            if credentials.any(|other| other != first) {
                return Err(ContractError::AmbiguousCredential { permission });
            }
            return Ok(Some(first.clone()));
        }
    }
    Ok(None)
}

fn ensure_covered(held: &BTreeSet<Grant>, grants: &BTreeSet<Grant>) -> Result<(), ContractError> {
    match grants
        .iter()
        .find(|grant| !held.iter().any(|holding| holding.covers(grant)))
    {
        Some(grant) => Err(ContractError::AuthorityExpansion {
            permission: grant.permission,
            scope: grant.scope.clone(),
        }),
        None => Ok(()),
    }
}
