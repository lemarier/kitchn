//! Forge executor for the core persisted-intent path.
use super::{
    GitHubAction, GitHubMutation, HouseScope, IntegrationError, ReadLimits,
    provider::{GitHubMutationTransport, Inspection, Provider},
};
use crate::{
    BackendId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Effect, EffectExecutor,
        EffectFailure, EffectRequest, GitHubEffect, Lookup, NotAppliedReason, Receipt,
        UncertainReason,
    },
    workflows::gate::MergeGrant,
};
use serde_json::Value;

/// GitHub effects execute only after core has persisted their intent.
/// GitHub has no native idempotency key: this executor deliberately does not
/// declare idempotent submissions. Uncertain outcomes are reconciled, never retried.
/// A merge is admitted and executed only at a subject one of its
/// readiness-checked [`MergeGrant`]s covers; see [`Self::with_merge_grant`].
pub struct GitHubExecutor<T> {
    descriptor: BackendDescriptor,
    scope: HouseScope,
    transport: T,
    limits: ReadLimits,
    merges: Vec<MergeGrant>,
}
impl<T: GitHubMutationTransport> GitHubExecutor<T> {
    /// Bind a namespace, house policy, and credential-aware provider boundary.
    pub fn new(backend: BackendId, scope: HouseScope, transport: T, limits: ReadLimits) -> Self {
        Self {
            descriptor: BackendDescriptor {
                backend,
                house: scope.house().clone(),
                worker_selection: None,
                capabilities: CapabilitySet::supporting([
                    Capability::ForgeMutation,
                    Capability::EffectLookup,
                ]),
            },
            scope,
            transport,
            limits,
            merges: Vec::new(),
        }
    }
    /// Admit merges of exactly the pull request, head, and base `grant`
    /// covers. Without a covering grant, every merge is refused before any
    /// provider call, whatever the house scope permits.
    #[must_use]
    pub fn with_merge_grant(mut self, grant: MergeGrant) -> Self {
        self.merges.push(grant);
        self
    }
    /// Build the exact effect to put in a core `EffectPlan`.
    ///
    /// # Errors
    /// Refuses invalid input, destinations, missing permissions, a merge no
    /// readiness-checked grant covers, and disabled budgets.
    pub fn effect(&self, mutation: GitHubMutation) -> Result<GitHubEffect, IntegrationError> {
        let effect = GitHubEffect {
            requester: self.scope.requester().clone(),
            mutation,
            posting_budget: self.scope.budget(),
        };
        self.validate(&effect)?;
        Ok(effect)
    }
    /// Inspect provider evidence in offline tests.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }
    /// The selected house and forge identity for this executor.
    #[must_use]
    pub const fn scope(&self) -> &HouseScope {
        &self.scope
    }
    fn validate(&self, effect: &GitHubEffect) -> Result<(), IntegrationError> {
        effect.mutation.validate()?;
        if effect.requester != *self.scope.requester()
            || effect.posting_budget.limit() > self.scope.budget().limit()
        {
            return Err(IntegrationError::ScopeMismatch);
        }
        self.scope.authorize_effect(
            self.scope.house(),
            &effect.mutation.repository,
            effect.required_permission(),
            0,
        )?;
        if let GitHubAction::MergePullRequest {
            number,
            expected_head,
            expected_base_commit,
            ..
        } = &effect.mutation.action
        {
            // A merge grant always names a base commit, so a baseless merge
            // is never covered.
            let granted = expected_base_commit.as_ref().is_some_and(|base| {
                self.merges.iter().any(|grant| {
                    grant.covers(
                        self.scope.house(),
                        &effect.mutation.repository,
                        *number,
                        expected_head,
                        base,
                    )
                })
            });
            if !granted {
                return Err(IntegrationError::PermissionDenied);
            }
        }
        Ok(())
    }
    fn payload<'a>(
        &self,
        request: &'a EffectRequest,
        writing: bool,
    ) -> Result<&'a GitHubEffect, EffectFailure> {
        if request.house() != self.scope.house() {
            return Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse));
        }
        if request.backend() != &self.descriptor.backend {
            return Err(EffectFailure::NotApplied(NotAppliedReason::ForeignBackend));
        }
        if request.credential() != self.scope.credential().name() {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        let Effect::GitHub(effect) = request.effect() else {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                request.effect().required_capability(),
            )));
        };
        if writing {
            self.validate(effect)
        } else {
            if effect.requester != *self.scope.requester() {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            self.scope
                .authorize_read(request.house(), &effect.mutation.repository)
        }
        .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        Ok(effect)
    }
}
impl<T: GitHubMutationTransport> EffectExecutor for GitHubExecutor<T> {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let effect = self.payload(request, true)?;
        let mut provider = Provider::new(&self.scope, &self.transport, self.limits);
        match provider
            .inspect(&effect.mutation, request.key())
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?
        {
            Inspection::Applied(receipt) => return Ok(receipt),
            Inspection::Conflict
            | Inspection::ResolvedUnattributed
            | Inspection::Retargeted
            | Inspection::MergedAtOtherHead => {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            // A marked pull request may be this request's, moved since.
            Inspection::MarkedElsewhere => {
                return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
            }
            Inspection::Missing => {}
        }
        let mutation = provider
            .prepare(&effect.mutation, request.key())
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let timeout = provider
            .remaining()
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let response = self.transport.submit(
            self.scope.credential(),
            &mutation,
            timeout,
            self.limits.bytes(),
        )?;
        // A successful exit alone is insufficient; read back the exact desired effect.
        let mut readback = Provider::new(&self.scope, &self.transport, self.limits);
        match readback
            .inspect(&effect.mutation, request.key())
            .map_err(uncertain)?
        {
            Inspection::Applied(receipt) => Ok(receipt),
            Inspection::ResolvedUnattributed => {
                let GitHubAction::ResolveReviewThread { thread, .. } = &effect.mutation.action
                else {
                    return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
                };
                let confirmed =
                    serde_json::from_slice::<Value>(&response)
                        .ok()
                        .is_some_and(|value| {
                            value.get("errors").is_none()
                                && value
                                    .pointer("/data/resolveReviewThread/clientMutationId")
                                    .and_then(Value::as_str)
                                    == Some(request.key().as_str())
                                && value
                                    .pointer("/data/resolveReviewThread/thread/id")
                                    .and_then(Value::as_str)
                                    == Some(thread.as_str())
                                && value
                                    .pointer("/data/resolveReviewThread/thread/isResolved")
                                    .and_then(Value::as_bool)
                                    == Some(true)
                        });
                if confirmed {
                    super::provider::receipt(request.key()).map_err(uncertain)
                } else {
                    Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
                }
            }
            Inspection::Missing
            | Inspection::Conflict
            | Inspection::Retargeted
            | Inspection::MergedAtOtherHead
            | Inspection::MarkedElsewhere => {
                Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
            }
        }
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let Ok(effect) = self.payload(request, false) else {
            return Ok(Lookup::Unknown);
        };
        let mut provider = Provider::new(&self.scope, &self.transport, self.limits);
        match provider.inspect(&effect.mutation, request.key()) {
            Ok(Inspection::Applied(receipt)) => Ok(Lookup::Applied(receipt)),
            // Even complete absence cannot rule out an earlier request still in flight.
            // A merge conflict is a moved head, which that request's `sha` cannot merge.
            Ok(Inspection::Conflict)
                if matches!(
                    effect.mutation.action,
                    GitHubAction::MergePullRequest { .. }
                ) =>
            {
                Ok(Lookup::Absent)
            }
            Ok(
                Inspection::Missing
                | Inspection::Conflict
                | Inspection::ResolvedUnattributed
                | Inspection::Retargeted
                | Inspection::MergedAtOtherHead
                | Inspection::MarkedElsewhere,
            ) => Ok(Lookup::Unknown),
            Err(IntegrationError::Timeout) => Err(BackendUnavailable::Timeout),
            Err(IntegrationError::LimitExceeded) => Err(BackendUnavailable::LimitExceeded),
            Err(_) => Err(BackendUnavailable::Transport),
        }
    }
}
fn uncertain(error: IntegrationError) -> EffectFailure {
    EffectFailure::Uncertain(match error {
        IntegrationError::Timeout => UncertainReason::Timeout,
        _ => UncertainReason::ResponseLost,
    })
}
