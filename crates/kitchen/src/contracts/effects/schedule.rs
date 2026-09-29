//! Schedule effects. Owned by #6.
//!
//! The portable definitions live in [`crate::scheduling`]. Installing never
//! activates: [`ScheduleEffect::InstallDisabled`] needs
//! [`Permission::ManageSchedule`], and turning a schedule on needs the
//! separate [`Permission::ActivateSchedule`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    contracts::{Capability, ContractError, EffectContext, GrantScope, Permission, ResourceRef},
    scheduling::{ScheduleSpec, ScheduleState},
};

/// The workflow requirements an effect must meet before it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleRequirements<'a> {
    /// The effect starts no scheduled run: pausing, removing, or not a
    /// schedule effect.
    None,
    /// An install, carrying its workflow's declared requirements.
    Declared(&'a BTreeSet<Capability>),
    /// Activating or trying this installed schedule starts runs, so the
    /// requirements recorded when it was installed apply; one with none
    /// recorded is refused.
    Installed(&'a ResourceRef),
}

/// A schedule change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ScheduleEffect {
    /// Install a schedule, disabled, for the workflow consumer scope its
    /// spec names. Executors reuse the one already installed for that
    /// consumer only when it is paused and matches the spec, instead of
    /// creating a second. They refuse, changing nothing, when it is active
    /// (turning a schedule on is [`Permission::ActivateSchedule`]), when it
    /// differs, or when several exist.
    #[serde(rename_all = "camelCase")]
    InstallDisabled {
        /// What to install, including its workflow and consumer scope.
        schedule: Box<ScheduleSpec>,
    },
    /// Pause or activate an installed schedule.
    #[serde(rename_all = "camelCase")]
    SetState {
        /// The schedule.
        schedule: ResourceRef,
        /// The requested state.
        state: ScheduleState,
    },
    /// Remove an installed schedule and its run history.
    #[serde(rename_all = "camelCase")]
    Remove {
        /// The schedule.
        schedule: ResourceRef,
    },
    /// Run a paused schedule once now without activating it. Executors
    /// refuse an active schedule. Each trial starts a run, so a trial is
    /// never resubmitted without reconciling first.
    #[serde(rename_all = "camelCase")]
    Trial {
        /// The schedule.
        schedule: ResourceRef,
    },
}

impl ScheduleEffect {
    /// The executor capability this effect needs.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::InstallDisabled { .. }
            | Self::SetState { .. }
            | Self::Remove { .. }
            | Self::Trial { .. } => Capability::ScheduleManage,
        }
    }

    /// Where the capabilities the scheduled workflow requires of the
    /// executor, beyond [`Self::required_capability`], come from.
    #[must_use]
    pub const fn schedule_requirements(&self) -> ScheduleRequirements<'_> {
        match self {
            Self::InstallDisabled { schedule } => {
                ScheduleRequirements::Declared(schedule.requires())
            }
            Self::SetState {
                schedule,
                state: ScheduleState::Active,
            }
            | Self::Trial { schedule } => ScheduleRequirements::Installed(schedule),
            Self::SetState {
                state: ScheduleState::Paused,
                ..
            }
            | Self::Remove { .. } => ScheduleRequirements::None,
        }
    }

    /// The task permission this effect needs. Activation is separate from
    /// management, and a trial run is separate from both.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::InstallDisabled { .. }
            | Self::Remove { .. }
            | Self::SetState {
                state: ScheduleState::Paused,
                ..
            } => Permission::ManageSchedule,
            Self::SetState {
                state: ScheduleState::Active,
                ..
            } => Permission::ActivateSchedule,
            Self::Trial { .. } => Permission::TrialSchedule,
        }
    }

    /// Schedules are house-wide.
    #[must_use]
    pub const fn scope(&self) -> GrantScope {
        match self {
            Self::InstallDisabled { .. }
            | Self::SetState { .. }
            | Self::Remove { .. }
            | Self::Trial { .. } => GrantScope::House,
        }
    }

    /// Per-submission check; see [`crate::contracts::Effect::check`].
    ///
    /// # Errors
    /// None: resubmission safety is the executor's reconciliation against
    /// its installed inventory.
    pub const fn check(&self, _context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::InstallDisabled { .. }
            | Self::SetState { .. }
            | Self::Remove { .. }
            | Self::Trial { .. } => Ok(()),
        }
    }

    /// Admission hook; see [`crate::contracts::Effect::admit`].
    ///
    /// # Errors
    /// None: schedule payloads are validated when constructed, and
    /// duplicate consumers are reconciled by the executor against its
    /// installed inventory.
    pub const fn admit(&self, _context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::InstallDisabled { .. }
            | Self::SetState { .. }
            | Self::Remove { .. }
            | Self::Trial { .. } => Ok(()),
        }
    }
}
