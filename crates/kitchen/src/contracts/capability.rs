//! Backend capabilities and the activation check for workflow requirements.
//!
//! A backend declares what it supports at runtime (support can depend on the
//! installed runtime version). A workflow names what it requires. Activation
//! fails, naming every gap, unless each requirement is fully supported.

use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, HouseId,
    contracts::{ContractError, ValueKind},
};

/// A backend capability, named as in the migration parity table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Capability {
    /// Install, inspect, enable, disable, and remove schedules.
    #[serde(rename = "schedule.manage")]
    ScheduleManage,
    /// Prechecks with typed idle and error results.
    #[serde(rename = "schedule.precheck")]
    SchedulePrecheck,
    /// No overlapping runs of one scheduled job.
    #[serde(rename = "schedule.single_consumer")]
    ScheduleSingleConsumer,
    /// An enforced run timeout.
    #[serde(rename = "schedule.run_timeout")]
    ScheduleRunTimeout,
    /// Launch a worker in an isolated or named workspace.
    #[serde(rename = "worker.launch_isolated")]
    WorkerLaunchIsolated,
    /// Positive evidence that a launched agent actually started.
    #[serde(rename = "worker.launch_readiness")]
    WorkerLaunchReadiness,
    /// Fenced worker messaging.
    #[serde(rename = "worker.messaging")]
    WorkerMessaging,
    /// Worker status and settled outcome.
    #[serde(rename = "worker.status_and_outcome")]
    WorkerStatusAndOutcome,
    /// Relinquish and adopt a supervised run.
    #[serde(rename = "run.transfer")]
    RunTransfer,
    /// Cancel a worker.
    #[serde(rename = "worker.cancel")]
    WorkerCancel,
    /// Inventory resources with ownership details.
    #[serde(rename = "resource.inventory")]
    ResourceInventory,
    /// Idempotent, safety-retaining resource release.
    #[serde(rename = "resource.release")]
    ResourceRelease,
    /// Close a console owned by a settled run.
    #[serde(rename = "resource.close_console")]
    ResourceCloseConsole,
    /// Reuse an agent session across runs.
    #[serde(rename = "session.reuse")]
    SessionReuse,
    /// Select the agent family.
    #[serde(rename = "agent.select_family")]
    AgentSelectFamily,
    /// Select the agent model.
    #[serde(rename = "agent.select_model")]
    AgentSelectModel,
    /// Report token and model usage per run.
    #[serde(rename = "usage.attribution")]
    UsageAttribution,
    /// Inject house-scoped credentials per effect.
    #[serde(rename = "house.credentials")]
    HouseCredentials,
    /// Look up an effect's outcome by idempotency key.
    #[serde(rename = "effect.lookup")]
    EffectLookup,
    /// Resubmitting an idempotency key never repeats the effect.
    #[serde(rename = "effect.idempotent_requests")]
    EffectIdempotentRequests,
}

impl Capability {
    /// Every capability, in declaration order.
    pub const ALL: [Self; 20] = [
        Self::ScheduleManage,
        Self::SchedulePrecheck,
        Self::ScheduleSingleConsumer,
        Self::ScheduleRunTimeout,
        Self::WorkerLaunchIsolated,
        Self::WorkerLaunchReadiness,
        Self::WorkerMessaging,
        Self::WorkerStatusAndOutcome,
        Self::RunTransfer,
        Self::WorkerCancel,
        Self::ResourceInventory,
        Self::ResourceRelease,
        Self::ResourceCloseConsole,
        Self::SessionReuse,
        Self::AgentSelectFamily,
        Self::AgentSelectModel,
        Self::UsageAttribution,
        Self::HouseCredentials,
        Self::EffectLookup,
        Self::EffectIdempotentRequests,
    ];

    /// The stable dotted name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScheduleManage => "schedule.manage",
            Self::SchedulePrecheck => "schedule.precheck",
            Self::ScheduleSingleConsumer => "schedule.single_consumer",
            Self::ScheduleRunTimeout => "schedule.run_timeout",
            Self::WorkerLaunchIsolated => "worker.launch_isolated",
            Self::WorkerLaunchReadiness => "worker.launch_readiness",
            Self::WorkerMessaging => "worker.messaging",
            Self::WorkerStatusAndOutcome => "worker.status_and_outcome",
            Self::RunTransfer => "run.transfer",
            Self::WorkerCancel => "worker.cancel",
            Self::ResourceInventory => "resource.inventory",
            Self::ResourceRelease => "resource.release",
            Self::ResourceCloseConsole => "resource.close_console",
            Self::SessionReuse => "session.reuse",
            Self::AgentSelectFamily => "agent.select_family",
            Self::AgentSelectModel => "agent.select_model",
            Self::UsageAttribution => "usage.attribution",
            Self::HouseCredentials => "house.credentials",
            Self::EffectLookup => "effect.lookup",
            Self::EffectIdempotentRequests => "effect.idempotent_requests",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Capability {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|capability| capability.as_str() == value)
            .ok_or(ContractError::InvalidValue {
                kind: ValueKind::Capability,
            })
    }
}

/// How far a backend supports a declared capability. Undeclared means unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Support {
    /// Fully supported; satisfies a requirement.
    Supported,
    /// Present with known gaps; does not satisfy a requirement.
    Partial,
}

/// The capabilities a backend declares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeMap<Capability, Support>);

impl CapabilitySet {
    /// An empty set: nothing supported.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Declare `capability` with `support`, replacing an earlier declaration.
    #[must_use]
    pub fn with(mut self, capability: Capability, support: Support) -> Self {
        self.0.insert(capability, support);
        self
    }

    /// Declare every capability in `capabilities` as fully supported.
    #[must_use]
    pub fn supporting(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self(
            capabilities
                .into_iter()
                .map(|capability| (capability, Support::Supported))
                .collect(),
        )
    }

    /// The declared support, or `None` when undeclared.
    #[must_use]
    pub fn support(&self, capability: Capability) -> Option<Support> {
        self.0.get(&capability).copied()
    }

    /// Whether `capability` is fully supported.
    #[must_use]
    pub fn supports(&self, capability: Capability) -> bool {
        self.support(capability) == Some(Support::Supported)
    }

    /// Check that every required capability is fully supported.
    ///
    /// # Errors
    /// Returns [`ContractError::UnsupportedCapabilities`] listing every undeclared and
    /// every partial requirement, so a diagnosis names all gaps at once.
    pub fn require(
        &self,
        required: impl IntoIterator<Item = Capability>,
    ) -> Result<(), ContractError> {
        let mut missing = Vec::new();
        let mut partial = Vec::new();
        for capability in required {
            match self.support(capability) {
                Some(Support::Supported) => {}
                Some(Support::Partial) => partial.push(capability),
                None => missing.push(capability),
            }
        }
        missing.sort_unstable();
        missing.dedup();
        partial.sort_unstable();
        partial.dedup();
        if missing.is_empty() && partial.is_empty() {
            Ok(())
        } else {
            Err(ContractError::UnsupportedCapabilities { missing, partial })
        }
    }
}

/// A backend's identity, the single house it serves, and its declared capabilities.
///
/// One backend instance serves one house so credentials and destinations are
/// selected before any effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDescriptor {
    /// Backend identity: one provider namespace (instance and account). Effect
    /// intents record it, and only this backend may execute or reconcile them.
    pub backend: BackendId,
    /// The only house this instance may act for.
    pub house: HouseId,
    /// Declared capabilities.
    pub capabilities: CapabilitySet,
}
