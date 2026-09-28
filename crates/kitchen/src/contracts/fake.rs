//! An in-memory, non-Orca backend for offline tests.
//!
//! It follows the [`ExecutionBackend`] contract and can inject faults (lost
//! responses, timeouts, refusals, lookup outages) so recovery paths can be
//! tested without a live orchestrator. Results from it are simulated
//! evidence, never live runtime evidence.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, MutexGuard, PoisonError},
};

use crate::{
    BackendId, HouseId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, EffectFailure,
        EffectRequest, ExecutionBackend, ExternalRef, IdempotencyKey, Lookup, NotAppliedReason,
        Operation, Receipt, ResourceKind, ResourceRef, UncertainReason, WorkerOutcome, WorkerState,
        Workspace,
    },
};

/// A fault applied to the next `execute` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecuteFault {
    /// Apply the effect, then report the response as lost.
    ApplyThenLoseResponse,
    /// Time out without applying.
    TimeoutWithoutApplying,
    /// Refuse before acting.
    Reject,
}

#[derive(Debug, Default)]
struct FakeState {
    applied: BTreeMap<IdempotencyKey, Receipt>,
    workers: BTreeMap<ExternalRef, WorkerState>,
    execute_faults: VecDeque<ExecuteFault>,
    lookup_outages: usize,
    effects_performed: usize,
    next_id: u64,
}

/// An in-memory execution backend.
#[derive(Debug)]
pub struct FakeBackend {
    descriptor: BackendDescriptor,
    state: Mutex<FakeState>,
}

impl FakeBackend {
    /// A fake with exactly `capabilities`.
    #[must_use]
    pub fn new(backend: BackendId, house: HouseId, capabilities: CapabilitySet) -> Self {
        Self {
            descriptor: BackendDescriptor {
                backend,
                house,
                capabilities,
            },
            state: Mutex::new(FakeState::default()),
        }
    }

    /// A fake declaring every capability as fully supported.
    #[must_use]
    pub fn fully_capable(backend: BackendId, house: HouseId) -> Self {
        Self::new(backend, house, CapabilitySet::supporting(Capability::ALL))
    }

    /// Queue a fault for a later `execute` call (first in, first out).
    pub fn inject(&self, fault: ExecuteFault) {
        self.lock().execute_faults.push_back(fault);
    }

    /// Make the next `count` lookups fail with a timeout.
    pub fn fail_lookups(&self, count: usize) {
        self.lock().lookup_outages = count;
    }

    /// How many effects were actually performed, counting duplicates a
    /// non-idempotent provider would repeat.
    #[must_use]
    pub fn effects_performed(&self) -> usize {
        self.lock().effects_performed
    }

    /// Set a worker's observed state, simulating agent progress.
    pub fn set_worker_state(&self, worker: &ResourceRef, state: WorkerState) {
        self.lock().workers.insert(worker.handle.clone(), state);
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        // A panic while holding the lock cannot leave FakeState half-updated
        // in a way that matters to tests, so recover the guard.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn handle(&self, state: &mut FakeState, prefix: &str) -> Result<ExternalRef, EffectFailure> {
        state.next_id = state.next_id.saturating_add(1);
        ExternalRef::new(&format!(
            "{prefix}-{}-{}",
            self.descriptor.backend, state.next_id
        ))
        .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))
    }

    fn resource(&self, kind: ResourceKind, handle: ExternalRef) -> ResourceRef {
        ResourceRef {
            kind,
            backend: self.descriptor.backend.clone(),
            handle,
        }
    }

    fn apply(
        &self,
        state: &mut FakeState,
        request: &EffectRequest,
    ) -> Result<Receipt, EffectFailure> {
        let rejected = EffectFailure::NotApplied(NotAppliedReason::Rejected);
        let resources = match request.operation() {
            Operation::LaunchWorker { workspace, .. } => {
                let worker = self.handle(state, "worker")?;
                state.workers.insert(worker.clone(), WorkerState::Starting);
                let mut resources = vec![self.resource(ResourceKind::Worker, worker)];
                match workspace {
                    Workspace::Isolated => {
                        let worktree = self.handle(state, "worktree")?;
                        resources.push(self.resource(ResourceKind::Worktree, worktree));
                    }
                    Workspace::Existing(existing) => resources.push(existing.clone()),
                }
                resources
            }
            Operation::MessageWorker { worker, .. } => {
                if !self.owns_live_worker(state, worker) {
                    return Err(rejected);
                }
                vec![worker.clone()]
            }
            Operation::CancelWorker { worker } => {
                if !self.owns_live_worker(state, worker) {
                    return Err(rejected);
                }
                state.workers.insert(
                    worker.handle.clone(),
                    WorkerState::Settled(WorkerOutcome::Cancelled),
                );
                vec![worker.clone()]
            }
            Operation::ReleaseResource { resource } => {
                if resource.backend != self.descriptor.backend {
                    return Err(rejected);
                }
                state.workers.remove(&resource.handle);
                vec![resource.clone()]
            }
        };
        let reference = self.handle(state, "request")?;
        let receipt = Receipt::new(reference, resources).map_err(|_| rejected)?;
        state.effects_performed = state.effects_performed.saturating_add(1);
        state.applied.insert(request.key().clone(), receipt.clone());
        Ok(receipt)
    }

    fn owns_live_worker(&self, state: &FakeState, worker: &ResourceRef) -> bool {
        worker.backend == self.descriptor.backend
            && matches!(
                state.workers.get(&worker.handle),
                Some(WorkerState::Starting | WorkerState::Ready | WorkerState::AwaitingReply)
            )
    }
}

impl ExecutionBackend for FakeBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        if request.house() != &self.descriptor.house {
            return Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse));
        }
        let capability = request.operation().required_capability();
        if !self.descriptor.capabilities.supports(capability) {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                capability,
            )));
        }
        let mut state = self.lock();
        if self
            .descriptor
            .capabilities
            .supports(Capability::EffectIdempotentRequests)
            && let Some(receipt) = state.applied.get(request.key())
        {
            return Ok(receipt.clone());
        }
        match state.execute_faults.pop_front() {
            None => self.apply(&mut state, request),
            Some(ExecuteFault::ApplyThenLoseResponse) => {
                self.apply(&mut state, request)?;
                Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
            }
            Some(ExecuteFault::TimeoutWithoutApplying) => {
                Err(EffectFailure::Uncertain(UncertainReason::Timeout))
            }
            Some(ExecuteFault::Reject) => {
                Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
            }
        }
    }

    fn lookup(&self, key: &IdempotencyKey) -> Result<Lookup, BackendUnavailable> {
        if !self
            .descriptor
            .capabilities
            .supports(Capability::EffectLookup)
        {
            return Err(BackendUnavailable::Unsupported(Capability::EffectLookup));
        }
        let mut state = self.lock();
        if state.lookup_outages > 0 {
            state.lookup_outages = state.lookup_outages.saturating_sub(1);
            return Err(BackendUnavailable::Timeout);
        }
        Ok(state
            .applied
            .get(key)
            .map_or(Lookup::Absent, |receipt| Lookup::Applied(receipt.clone())))
    }

    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        if !self
            .descriptor
            .capabilities
            .supports(Capability::WorkerStatusAndOutcome)
        {
            return Err(BackendUnavailable::Unsupported(
                Capability::WorkerStatusAndOutcome,
            ));
        }
        if worker.backend != self.descriptor.backend {
            return Ok(WorkerState::Missing);
        }
        Ok(self
            .lock()
            .workers
            .get(&worker.handle)
            .copied()
            .unwrap_or(WorkerState::Missing))
    }
}
