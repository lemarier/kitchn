//! Backend capabilities and the activation check for workflow requirements.
//!
//! A backend declares what it supports at runtime (support can depend on the
//! installed runtime version). A workflow names what it requires. Activation
//! fails, naming every gap, unless each requirement is fully supported.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, HouseId,
    contracts::{ContractError, Effect, ValueKind},
    selection::{AgentSelection, SelectionGap, SelectionSupport},
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
        /// Deliver worker questions, reports, and escalations to the
        /// coordinator at least once, replaying each batch until it is
        /// acknowledged ([`crate::contracts::CoordinatorMailbox`]).
        WorkerDeliveries = "worker.deliveries",
        /// Cancel a worker.
        WorkerCancel = "worker.cancel",
        /// Inventory resources with ownership details.
        ResourceInventory = "resource.inventory",
        /// Idempotent, safety-retaining resource release. A release removes
        /// only what it releases: releasing a worktree removes the checkout
        /// and never deletes the branch checked out in it
        /// ([`crate::contracts::conformance::Check::ReleaseKeepsBranch`]).
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
        /// Deliver forge events, such as webhooks, to Kitchen's event intake.
        EventDelivery = "event.delivery",
        /// Look up any effect's outcome by its persisted request; shorthand
        /// for every per-kind lookup capability.
        EffectLookup = "effect.lookup",
        /// Resubmitting any effect's key never repeats it; shorthand for
        /// every per-kind idempotency capability.
        EffectIdempotentRequests = "effect.idempotent_requests",
        /// Look up a `launch_worker` effect's outcome by its persisted request.
        LookupLaunchWorker = "effect.lookup.launch_worker",
        /// Resubmitting a `launch_worker` effect's key never repeats it.
        IdempotentLaunchWorker = "effect.idempotent.launch_worker",
        /// Look up a `message_worker` effect's outcome by its persisted request.
        LookupMessageWorker = "effect.lookup.message_worker",
        /// Resubmitting a `message_worker` effect's key never repeats it.
        IdempotentMessageWorker = "effect.idempotent.message_worker",
        /// Look up a `reply_to_worker` effect's outcome by its persisted request.
        LookupReplyToWorker = "effect.lookup.reply_to_worker",
        /// Resubmitting a `reply_to_worker` effect's key never repeats it.
        IdempotentReplyToWorker = "effect.idempotent.reply_to_worker",
        /// Look up a `cancel_worker` effect's outcome by its persisted request.
        LookupCancelWorker = "effect.lookup.cancel_worker",
        /// Resubmitting a `cancel_worker` effect's key never repeats it.
        IdempotentCancelWorker = "effect.idempotent.cancel_worker",
        /// Look up a `release_resource` effect's outcome by its persisted request.
        LookupReleaseResource = "effect.lookup.release_resource",
        /// Resubmitting a `release_resource` effect's key never repeats it.
        IdempotentReleaseResource = "effect.idempotent.release_resource",
        /// Look up a `create_label` effect's outcome by its persisted request.
        LookupCreateLabel = "effect.lookup.create_label",
        /// Resubmitting a `create_label` effect's key never repeats it.
        IdempotentCreateLabel = "effect.idempotent.create_label",
        /// Look up a `post_comment` effect's outcome by its persisted request.
        LookupPostComment = "effect.lookup.post_comment",
        /// Resubmitting a `post_comment` effect's key never repeats it.
        IdempotentPostComment = "effect.idempotent.post_comment",
        /// Look up a `set_label` effect's outcome by its persisted request.
        LookupSetLabel = "effect.lookup.set_label",
        /// Resubmitting a `set_label` effect's key never repeats it.
        IdempotentSetLabel = "effect.idempotent.set_label",
        /// Look up a `create_issue` effect's outcome by its persisted request.
        LookupCreateIssue = "effect.lookup.create_issue",
        /// Resubmitting a `create_issue` effect's key never repeats it.
        IdempotentCreateIssue = "effect.idempotent.create_issue",
        /// Look up a `link_sub_issue` effect's outcome by its persisted request.
        LookupLinkSubIssue = "effect.lookup.link_sub_issue",
        /// Resubmitting a `link_sub_issue` effect's key never repeats it.
        IdempotentLinkSubIssue = "effect.idempotent.link_sub_issue",
        /// Look up a `link_dependency` effect's outcome by its persisted request.
        LookupLinkDependency = "effect.lookup.link_dependency",
        /// Resubmitting a `link_dependency` effect's key never repeats it.
        IdempotentLinkDependency = "effect.idempotent.link_dependency",
        /// Look up a `merge_pull_request` effect's outcome by its persisted request.
        LookupMergePullRequest = "effect.lookup.merge_pull_request",
        /// Resubmitting a `merge_pull_request` effect's key never repeats it.
        IdempotentMergePullRequest = "effect.idempotent.merge_pull_request",
        /// Look up a `close_issue` effect's outcome by its persisted request.
        LookupCloseIssue = "effect.lookup.close_issue",
        /// Resubmitting a `close_issue` effect's key never repeats it.
        IdempotentCloseIssue = "effect.idempotent.close_issue",
        /// Look up an `open_pull_request` effect's outcome by its persisted request.
        LookupOpenPullRequest = "effect.lookup.open_pull_request",
        /// Reconcile a pull request review by its durable marker.
        LookupReviewPullRequest = "effect.lookup.review_pull_request",
        /// Resubmitting an `open_pull_request` effect's key never repeats it.
        IdempotentOpenPullRequest = "effect.idempotent.open_pull_request",
        /// Native same-key review idempotency, when a backend provides it.
        IdempotentReviewPullRequest = "effect.idempotent.review_pull_request",
        /// Reconcile a marked review-thread reply.
        LookupReplyToReviewThread = "effect.lookup.reply_to_review_thread",
        /// Native same-key reply idempotency, when available.
        IdempotentReplyToReviewThread = "effect.idempotent.reply_to_review_thread",
        /// Reconcile review-thread resolution from its provider state.
        LookupResolveReviewThread = "effect.lookup.resolve_review_thread",
        /// Native same-key resolution idempotency, when available.
        IdempotentResolveReviewThread = "effect.idempotent.resolve_review_thread",
        /// Look up a `ask` effect's outcome by its persisted request.
        LookupAsk = "effect.lookup.ask",
        /// Resubmitting a `ask` effect's key never repeats it.
        IdempotentAsk = "effect.idempotent.ask",
        /// Look up a `install_disabled_schedule` effect's outcome by its persisted request.
        LookupInstallDisabledSchedule = "effect.lookup.install_disabled_schedule",
        /// Resubmitting a `install_disabled_schedule` effect's key never repeats it.
        IdempotentInstallDisabledSchedule = "effect.idempotent.install_disabled_schedule",
        /// Look up a `set_schedule_state` effect's outcome by its persisted request.
        LookupSetScheduleState = "effect.lookup.set_schedule_state",
        /// Resubmitting a `set_schedule_state` effect's key never repeats it.
        IdempotentSetScheduleState = "effect.idempotent.set_schedule_state",
        /// Look up a `remove_schedule` effect's outcome by its persisted request.
        LookupRemoveSchedule = "effect.lookup.remove_schedule",
        /// Resubmitting a `remove_schedule` effect's key never repeats it.
        IdempotentRemoveSchedule = "effect.idempotent.remove_schedule",
        /// Look up a `trial_schedule` effect's outcome by its persisted request.
        LookupTrialSchedule = "effect.lookup.trial_schedule",
        /// Resubmitting a `trial_schedule` effect's key never repeats it.
        IdempotentTrialSchedule = "effect.idempotent.trial_schedule",
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

impl BackendDescriptor {
    /// Declare what worker launches can honor of an agent selection.
    #[must_use]
    pub const fn with_worker_selection(mut self, support: SelectionSupport) -> Self {
        self.worker_selection = Some(support);
        self
    }

    /// Check that worker launches can provide all of `selection`.
    ///
    /// # Errors
    /// [`ContractError::UnsupportedCapabilities`] naming the selection
    /// capability behind every gap; with no declaration, the family
    /// capability plus model and effort when the selection sets them.
    pub fn check_worker_selection(&self, selection: &AgentSelection) -> Result<(), ContractError> {
        let mut missing: Vec<Capability> = match &self.worker_selection {
            Some(support) => support
                .gaps(selection)
                .into_iter()
                .map(SelectionGap::capability)
                .collect(),
            None => {
                let mut all = vec![Capability::AgentSelectFamily];
                if selection.model.is_some() || selection.effort.is_some() {
                    all.push(Capability::AgentSelectModel);
                }
                all
            }
        };
        missing.sort_unstable();
        missing.dedup();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ContractError::UnsupportedCapabilities {
                missing,
                partial: Vec::new(),
            })
        }
    }

    /// Whether the executor can look up `effect`'s outcome by its persisted
    /// request: [`Capability::EffectLookup`] or the effect kind's own lookup
    /// capability is fully supported.
    #[must_use]
    pub fn supports_lookup(&self, effect: &Effect) -> bool {
        self.capabilities.supports(Capability::EffectLookup)
            || self
                .capabilities
                .supports(effect.kind().lookup_capability())
    }

    /// Whether resubmitting `effect`'s key never repeats it:
    /// [`Capability::EffectIdempotentRequests`] or the effect kind's own
    /// idempotency capability is fully supported.
    #[must_use]
    pub fn idempotent(&self, effect: &Effect) -> bool {
        self.capabilities
            .supports(Capability::EffectIdempotentRequests)
            || self
                .capabilities
                .supports(effect.kind().idempotency_capability())
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
    /// What this backend's worker launches can honor of an agent selection.
    /// `None` means launches cannot honor any selection: the state store
    /// refuses a launch that names one rather than let it run on the
    /// backend's default agent.
    pub worker_selection: Option<SelectionSupport>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scheduling::AgentFamily,
        selection::{AgentModel, EffortLevel, EffortSupport},
    };

    fn descriptor(worker_selection: Option<SelectionSupport>) -> Option<BackendDescriptor> {
        Some(BackendDescriptor {
            backend: BackendId::new("orca-local").ok()?,
            house: HouseId::new("origin89").ok()?,
            capabilities: CapabilitySet::new(),
            worker_selection,
        })
    }

    fn chosen(model: bool, effort: bool) -> Option<AgentSelection> {
        Some(AgentSelection {
            agent: AgentFamily::Codex,
            model: if model {
                Some(AgentModel::new("gpt-6-sol").ok()?)
            } else {
                None
            },
            effort: if effort {
                Some(EffortLevel::new("high").ok()?)
            } else {
                None
            },
        })
    }

    const CODEX_MODEL: SelectionSupport = SelectionSupport {
        families: &[AgentFamily::Codex],
        model: true,
        effort: EffortSupport::WithModel,
    };

    #[test]
    fn an_undeclared_surface_provides_no_selection() {
        let none = descriptor(None);
        let family = none
            .as_ref()
            .zip(chosen(false, false))
            .map(|(descriptor, selection)| descriptor.check_worker_selection(&selection));
        assert_eq!(
            family,
            Some(Err(ContractError::UnsupportedCapabilities {
                missing: vec![Capability::AgentSelectFamily],
                partial: Vec::new(),
            }))
        );
        let detailed = none
            .zip(chosen(true, true))
            .map(|(descriptor, selection)| descriptor.check_worker_selection(&selection));
        assert_eq!(
            detailed,
            Some(Err(ContractError::UnsupportedCapabilities {
                missing: vec![Capability::AgentSelectFamily, Capability::AgentSelectModel],
                partial: Vec::new(),
            }))
        );
    }

    #[test]
    fn a_declared_surface_accepts_what_it_provides_and_names_the_rest() {
        let declared = descriptor(None).map(|d| d.with_worker_selection(CODEX_MODEL));
        let accepted = declared
            .as_ref()
            .zip(chosen(true, true))
            .map(|(descriptor, selection)| descriptor.check_worker_selection(&selection));
        assert_eq!(accepted, Some(Ok(())));
        let effort_only = declared
            .zip(chosen(false, true))
            .map(|(descriptor, selection)| descriptor.check_worker_selection(&selection));
        assert_eq!(
            effort_only,
            Some(Err(ContractError::UnsupportedCapabilities {
                missing: vec![Capability::AgentSelectModel],
                partial: Vec::new(),
            }))
        );
    }
}
