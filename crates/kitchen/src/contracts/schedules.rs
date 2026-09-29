//! Schedule installation and inspection through the backend contract.
//!
//! Pausing, activating, removing, and trying a schedule are persisted
//! effects ([`crate::contracts::ScheduleEffect`]). Installing a schedule
//! paused and observing schedules and their runs are the calls a budget pass
//! or an activation check needs besides those, so a backend declaring
//! [`Capability::ScheduleManage`](crate::contracts::Capability::ScheduleManage)
//! offers them here.

use crate::{
    contracts::{EffectExecutor, ResourceRef},
    scheduling::{Readiness, ScheduleEvidence, ScheduleObservation, ScheduleSpec},
};

/// A backend that installs and observes a house's schedules.
///
/// Implementations keep their own structured refusals, such as a schedule
/// that already runs or one that breaks the house's schedule limits, and
/// convert them into [`crate::Error`] for callers.
pub trait ScheduleBackend: EffectExecutor {
    /// Why a schedule call failed.
    type Error: std::error::Error + Into<crate::Error>;

    /// Install `spec` paused, or reuse the identical paused schedule already
    /// installed for its consumer. Nothing here activates a schedule.
    ///
    /// # Errors
    /// Refusals the backend checked before creating anything, and outcomes
    /// it cannot establish.
    fn install_schedule(&self, spec: &ScheduleSpec) -> Result<ResourceRef, Self::Error>;

    /// Observe a schedule's state and its recent runs, each judged against
    /// `readiness`, so a launch the agent never started is not a completed
    /// run. A schedule the backend no longer lists is reported missing.
    ///
    /// # Errors
    /// Read failures.
    fn inspect_schedule(
        &self,
        schedule: &ResourceRef,
        readiness: &Readiness<'_>,
    ) -> Result<ScheduleObservation, Self::Error>;

    /// Observe every schedule of this house now, for judging budgets.
    ///
    /// # Errors
    /// Read failures, and listings beyond the evidence bound.
    fn schedule_evidence(&self) -> Result<ScheduleEvidence, Self::Error>;
}
