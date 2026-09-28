//! Backend capabilities and the activation check for workflow requirements.
//!
//! A backend declares what it supports at runtime (support can depend on the
//! installed runtime version). A workflow names what it requires. Activation
//! fails, naming every gap, unless each requirement is fully supported.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, HouseId,
    contracts::{ContractError, ValueKind},
};

closed_names! {
    /// A backend capability, named as in the migration parity table.
    #[non_exhaustive]
    pub enum Capability(ValueKind::Capability) {
        /// Install, inspect, enable, disable, and remove schedules.
        ScheduleManage = "schedule.manage",
        /// Prechecks with typed idle and error results.
        SchedulePrecheck = "schedule.precheck",
        /// No overlapping runs of one scheduled job.
        ScheduleSingleConsumer = "schedule.single_consumer",
        /// An enforced run timeout.
        ScheduleRunTimeout = "schedule.run_timeout",
        /// Launch a worker in an isolated or named workspace.
        WorkerLaunchIsolated = "worker.launch_isolated",
        /// Positive evidence that a launched agent actually started.
        WorkerLaunchReadiness = "worker.launch_readiness",
        /// Fenced worker messaging.
        WorkerMessaging = "worker.messaging",
        /// Worker status and settled outcome.
        WorkerStatusAndOutcome = "worker.status_and_outcome",
        /// Relinquish and adopt a supervised run.
        RunTransfer = "run.transfer",
        /// Cancel a worker.
        WorkerCancel = "worker.cancel",
        /// Inventory resources with ownership details.
        ResourceInventory = "resource.inventory",
        /// Idempotent, safety-retaining resource release.
        ResourceRelease = "resource.release",
        /// Close a console owned by a settled run.
        ResourceCloseConsole = "resource.close_console",
        /// Reuse an agent session across runs.
        SessionReuse = "session.reuse",
        /// Select the agent family.
        AgentSelectFamily = "agent.select_family",
        /// Select the agent model.
        AgentSelectModel = "agent.select_model",
        /// Report token and model usage per run.
        UsageAttribution = "usage.attribution",
        /// Inject house-scoped credentials per effect.
        HouseCredentials = "house.credentials",
        /// Mutate a forge: labels, issues, issue relationships.
        ForgeMutation = "forge.mutation",
        /// Ask a human through a decision service and read the answer.
        AskHuman = "human.ask",
        /// Look up an effect's outcome by idempotency key.
        EffectLookup = "effect.lookup",
        /// Resubmitting an idempotency key never repeats the effect.
        EffectIdempotentRequests = "effect.idempotent_requests",
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
