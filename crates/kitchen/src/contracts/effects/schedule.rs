//! Schedule effects. Owned by #6, which extends this payload.

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId,
    contracts::{Capability, ContractError, EffectContext, GrantScope, Permission},
};

/// A schedule change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ScheduleEffect {
    /// Install the schedule for one workflow consumer scope, disabled.
    /// Activating it is a separate effect needing
    /// [`Permission::ActivateSchedule`].
    #[serde(rename_all = "camelCase")]
    InstallDisabled {
        /// The workflow consumer scope the schedule runs.
        consumer: ConsumerId,
    },
}

impl ScheduleEffect {
    /// The executor capability this effect needs.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::InstallDisabled { .. } => Capability::ScheduleManage,
        }
    }

    /// The task permission this effect needs.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::InstallDisabled { .. } => Permission::ManageSchedule,
        }
    }

    /// Schedules are house-wide.
    #[must_use]
    pub const fn scope(&self) -> GrantScope {
        match self {
            Self::InstallDisabled { .. } => GrantScope::House,
        }
    }

    /// Admission hook; see [`crate::contracts::Effect::admit`].
    ///
    /// # Errors
    /// None yet; #6 adds its checks here.
    pub const fn admit(&self, _context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::InstallDisabled { .. } => Ok(()),
        }
    }
}
