//! Forge executor for the core persisted-intent path.
use super::{
    GitHubMutation, HouseScope, IntegrationError, ReadLimits,
    provider::{GitHubMutationTransport, Inspection, Provider},
};
use crate::{
    BackendId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Effect, EffectExecutor,
        EffectFailure, EffectRequest, GitHubEffect, Lookup, NotAppliedReason, Receipt,
        UncertainReason,
    },
};

/// GitHub effects execute only after core has persisted their intent.
/// GitHub has no native idempotency key: this executor deliberately does not
/// declare idempotent submissions. Uncertain outcomes are reconciled, never retried.
pub struct GitHubExecutor<T> {
    descriptor: BackendDescriptor,
    scope: HouseScope,
    transport: T,
    limits: ReadLimits,
}
impl<T: GitHubMutationTransport> GitHubExecutor<T> {
    /// Bind a namespace, house policy, and credential-aware provider boundary.
    pub fn new(backend: BackendId, scope: HouseScope, transport: T, limits: ReadLimits) -> Self {
        Self {
            descriptor: BackendDescriptor {
                backend,
                house: scope.house().clone(),
                capabilities: CapabilitySet::supporting([
                    Capability::ForgeMutation,
                    Capability::EffectLookup,
                ]),
            },
            scope,
            transport,
            limits,
        }
    }
    /// Build the exact effect to put in a core `EffectPlan`.
    ///
    /// # Errors
    /// Refuses invalid input, destinations, missing permissions and disabled budgets.
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
        )
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
            Inspection::Conflict => {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            Inspection::Missing => {}
        }
        let mutation = provider
            .prepare(&effect.mutation, request.key())
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let timeout = provider
            .remaining()
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        self.transport.submit(
            self.scope.credential(),
            &mutation,
            timeout,
            self.limits.bytes(),
        )?;
        // A successful exit alone is insufficient; read back the exact desired effect.
        match provider
            .inspect(&effect.mutation, request.key())
            .map_err(uncertain)?
        {
            Inspection::Applied(receipt) => Ok(receipt),
            Inspection::Missing | Inspection::Conflict => {
                Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
            }
        }
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let effect = self
            .payload(request, false)
            .map_err(|_| BackendUnavailable::Transport)?;
        let mut provider = Provider::new(&self.scope, &self.transport, self.limits);
        match provider.inspect(&effect.mutation, request.key()) {
            Ok(Inspection::Applied(receipt)) => Ok(Lookup::Applied(receipt)),
            // Even complete absence cannot rule out an earlier request still in flight.
            Ok(Inspection::Missing | Inspection::Conflict) => Ok(Lookup::Unknown),
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
