//! Idempotent Roger requests through the same core durable effect path.
use super::{
    RogerAsk,
    provider::{RogerMutationTransport, validate_receipt},
};
use crate::integrations::github::{HouseScope, IntegrationError, ReadLimits};
use crate::{
    BackendId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Effect, EffectExecutor,
        EffectFailure, EffectRequest, Lookup, NotAppliedReason, Permission, Receipt, RogerEffect,
        UncertainReason,
    },
};

/// House-scoped Roger executor. `--idem` makes uncertain submission replay safe;
/// this does not make the human's later approved action idempotent or authorized.
pub struct RogerExecutor<T> {
    descriptor: BackendDescriptor,
    scope: HouseScope,
    transport: T,
    limits: ReadLimits,
}
impl<T: RogerMutationTransport> RogerExecutor<T> {
    /// Bind the provider to exactly one house and credential namespace.
    pub fn new(backend: BackendId, scope: HouseScope, transport: T, limits: ReadLimits) -> Self {
        Self {
            descriptor: BackendDescriptor {
                backend,
                house: scope.house().clone(),
                worker_selection: None,
                capabilities: CapabilitySet::supporting([
                    Capability::AskHuman,
                    Capability::EffectLookup,
                    Capability::EffectIdempotentRequests,
                ]),
            },
            scope,
            transport,
            limits,
        }
    }
    /// Prepare a typed payload; core persists it and admits its budget atomically.
    ///
    /// # Errors
    /// Refuses scope, requester, content, or disabled-budget mismatches.
    pub fn effect(&self, ask: RogerAsk) -> Result<RogerEffect, IntegrationError> {
        let effect = RogerEffect {
            requester: self.scope.requester().clone(),
            ask,
            posting_budget: self.scope.budget(),
        };
        self.validate(&effect)?;
        Ok(effect)
    }
    /// Inspect a fake provider's call evidence.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }
    fn validate(&self, effect: &RogerEffect) -> Result<(), IntegrationError> {
        effect.ask.validate()?;
        effect.ask.binding.validate(&self.scope)?;
        if effect.requester != *self.scope.requester()
            || effect.posting_budget.limit() > self.scope.budget().limit()
        {
            return Err(IntegrationError::ScopeMismatch);
        }
        self.scope.authorize_effect(
            &effect.ask.binding.house,
            &effect.ask.binding.repository,
            Permission::AskHuman,
            0,
        )
    }
    fn payload<'a>(
        &self,
        request: &'a EffectRequest,
        writing: bool,
    ) -> Result<&'a RogerEffect, EffectFailure> {
        if request.house() != self.scope.house() {
            return Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse));
        }
        if request.backend() != &self.descriptor.backend {
            return Err(EffectFailure::NotApplied(NotAppliedReason::ForeignBackend));
        }
        if request.credential() != self.scope.credential().name() {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        let Effect::Roger(effect) = request.effect() else {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                request.effect().required_capability(),
            )));
        };
        if &effect.ask.binding.task != request.task() {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        if writing {
            self.validate(effect)
        } else {
            if effect.requester != *self.scope.requester() {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            effect.ask.binding.validate(&self.scope)
        }
        .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        Ok(effect)
    }
}
impl<T: RogerMutationTransport> EffectExecutor for RogerExecutor<T> {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let effect = self.payload(request, true)?;
        let bytes = self
            .transport
            .submit(
                self.scope.credential(),
                &effect.ask,
                request.key(),
                self.limits.timeout(),
                self.limits.bytes().min(128 * 1024),
            )
            .map_err(|error| match error {
                IntegrationError::InvalidInput | IntegrationError::ScopeMismatch => {
                    EffectFailure::NotApplied(NotAppliedReason::Rejected)
                }
                IntegrationError::Timeout => EffectFailure::Uncertain(UncertainReason::Timeout),
                _ => EffectFailure::Uncertain(UncertainReason::Transport),
            })?;
        let reference = validate_receipt(self.scope.credential(), &effect.ask, &bytes)
            .map_err(|_| EffectFailure::Uncertain(UncertainReason::ResponseLost))?;
        Receipt::new(reference, vec![], vec![])
            .map_err(|_| EffectFailure::Uncertain(UncertainReason::ResponseLost))
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let Ok(effect) = self.payload(request, false) else {
            return Ok(Lookup::Unknown);
        };
        match self
            .transport
            .find(
                self.scope.credential(),
                &effect.ask,
                self.limits.timeout(),
                self.limits.bytes(),
            )
            .map_err(|_| BackendUnavailable::Transport)?
        {
            Some(reference) => Ok(Lookup::Applied(
                Receipt::new(reference, vec![], vec![])
                    .map_err(|_| BackendUnavailable::Transport)?,
            )),
            None => Ok(Lookup::Unknown),
        }
    }
}
