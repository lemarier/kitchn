//! An in-memory, non-Orca executor for offline tests.
//!
//! It follows the [`EffectExecutor`] and [`WorkerBackend`] contracts for
//! every effect family its capabilities declare and can inject faults (lost
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
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, ContractError, Effect,
        EffectExecutor, EffectFailure, EffectRequest, ExternalRef, IdempotencyKey, Liveness,
        Lookup, MAX_INVENTORY_RESOURCES, NotAppliedReason, Operation, Receipt, ResourceKind,
        ResourceObservation, ResourceRef, ScheduleEffect, UncertainReason, WorkerBackend,
        WorkerOutcome, WorkerState, Workspace,
    },
    scheduling::AgentFamily,
    selection::{AgentSelection, EffortSupport, SelectionSupport},
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
    owners: BTreeMap<ExternalRef, ExternalRef>,
    execute_faults: VecDeque<ExecuteFault>,
    lookup_outages: usize,
    effects_performed: usize,
    launched_agents: Vec<Option<AgentSelection>>,
    execute_calls: usize,
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
                worker_selection: None,
                capabilities,
            },
            state: Mutex::new(FakeState::default()),
        }
    }

    /// A fake declaring every capability as fully supported and worker
    /// launches that honor either family and a model, with an effort only
    /// alongside a model, so conformance exercises the selection refusal.
    #[must_use]
    pub fn fully_capable(backend: BackendId, house: HouseId) -> Self {
        Self::new(backend, house, CapabilitySet::supporting(Capability::ALL)).with_worker_selection(
            SelectionSupport {
                families: &[AgentFamily::Claude, AgentFamily::Codex],
                model: true,
                effort: EffortSupport::WithModel,
            },
        )
    }

    /// Declare what this fake's worker launches honor of an agent selection.
    /// Without it the fake refuses any launch that names a selection.
    #[must_use]
    pub fn with_worker_selection(mut self, support: SelectionSupport) -> Self {
        self.descriptor = self.descriptor.with_worker_selection(support);
        self
    }

    /// The agent selection each performed launch used, in order; `None` for
    /// a launch that named none.
    #[must_use]
    pub fn launched_agents(&self) -> Vec<Option<AgentSelection>> {
        self.lock().launched_agents.clone()
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

    /// How many times `execute` was called, including refused and
    /// deduplicated calls.
    #[must_use]
    pub fn execute_calls(&self) -> usize {
        self.lock().execute_calls
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
        let (created, touched) = match request.effect() {
            Effect::Worker(Operation::LaunchWorker {
                workspace,
                branch,
                agent,
                ..
            }) => {
                // A selection the fake cannot honor is refused, never
                // replaced by a default agent.
                if let Some(agent) = agent
                    && let Err(ContractError::UnsupportedCapabilities { missing, .. }) =
                        self.descriptor.check_worker_selection(agent)
                {
                    return Err(EffectFailure::NotApplied(
                        missing
                            .first()
                            .map_or(NotAppliedReason::Rejected, |capability| {
                                NotAppliedReason::Unsupported(*capability)
                            }),
                    ));
                }
                state.launched_agents.push(agent.clone());
                let worker = self.handle(state, "worker")?;
                let mut created = vec![self.resource(ResourceKind::Worker, worker.clone())];
                if let Some(branch) = branch {
                    let handle = ExternalRef::new(branch.as_str()).map_err(|_| rejected)?;
                    created.push(self.resource(ResourceKind::Branch, handle));
                }
                let mut touched = Vec::new();
                match workspace {
                    Workspace::Isolated => {
                        let worktree = self.handle(state, "worktree")?;
                        created.push(self.resource(ResourceKind::Worktree, worktree));
                    }
                    Workspace::Existing(existing) => touched.push(existing.clone()),
                }
                if let Ok(owner) = ExternalRef::new(request.key().as_str()) {
                    state.owners.insert(worker.clone(), owner);
                }
                state.workers.insert(worker, WorkerState::Starting);
                (created, touched)
            }
            Effect::Worker(
                Operation::MessageWorker { worker, .. } | Operation::ReplyToWorker { worker, .. },
            ) => {
                if !self.owns_live_worker(state, worker) {
                    return Err(rejected);
                }
                (Vec::new(), vec![worker.clone()])
            }
            Effect::Worker(Operation::CancelWorker { worker }) => {
                if !self.owns_live_worker(state, worker) {
                    return Err(rejected);
                }
                state.workers.insert(
                    worker.handle.clone(),
                    WorkerState::Settled(WorkerOutcome::Cancelled),
                );
                (Vec::new(), vec![worker.clone()])
            }
            Effect::Worker(Operation::ReleaseResource { resource }) => {
                if resource.backend != self.descriptor.backend {
                    return Err(rejected);
                }
                state.workers.remove(&resource.handle);
                state.owners.remove(&resource.handle);
                (Vec::new(), vec![resource.clone()])
            }
            Effect::GitHub(_) | Effect::Roger(_) => (Vec::new(), Vec::new()),
            Effect::Schedule(ScheduleEffect::InstallDisabled { .. }) => {
                let schedule = self.handle(state, "schedule")?;
                (
                    vec![self.resource(ResourceKind::Schedule, schedule)],
                    Vec::new(),
                )
            }
            Effect::Schedule(
                ScheduleEffect::SetState { schedule, .. }
                | ScheduleEffect::Remove { schedule }
                | ScheduleEffect::Trial { schedule },
            ) => (Vec::new(), vec![schedule.clone()]),
        };
        let reference = self.handle(state, "request")?;
        let receipt = Receipt::new(reference, created, touched).map_err(|_| rejected)?;
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

impl EffectExecutor for FakeBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        {
            let mut state = self.lock();
            state.execute_calls = state.execute_calls.saturating_add(1);
        }
        if request.house() != &self.descriptor.house {
            return Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse));
        }
        if request.backend() != &self.descriptor.backend {
            return Err(EffectFailure::NotApplied(NotAppliedReason::ForeignBackend));
        }
        let capability = request.effect().required_capability();
        if !self.descriptor.capabilities.supports(capability) {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
                capability,
            )));
        }
        let mut state = self.lock();
        if self.descriptor.idempotent(request.effect())
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

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        if !self.descriptor.supports_lookup(request.effect()) {
            return Err(BackendUnavailable::Unsupported(
                request.effect().kind().lookup_capability(),
            ));
        }
        let mut state = self.lock();
        if state.lookup_outages > 0 {
            state.lookup_outages = state.lookup_outages.saturating_sub(1);
            return Err(BackendUnavailable::Timeout);
        }
        Ok(state
            .applied
            .get(request.key())
            .map_or(Lookup::Absent, |receipt| Lookup::Applied(receipt.clone())))
    }
}

impl WorkerBackend for FakeBackend {
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

    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        if !self
            .descriptor
            .capabilities
            .supports(Capability::ResourceInventory)
        {
            return Err(BackendUnavailable::Unsupported(
                Capability::ResourceInventory,
            ));
        }
        let state = self.lock();
        if state.workers.len() > MAX_INVENTORY_RESOURCES {
            return Err(BackendUnavailable::LimitExceeded);
        }
        Ok(state
            .workers
            .iter()
            .map(|(handle, worker)| ResourceObservation {
                resource: self.resource(ResourceKind::Worker, handle.clone()),
                owner: state.owners.get(handle).cloned(),
                liveness: match worker {
                    WorkerState::Starting
                    | WorkerState::Ready
                    | WorkerState::AwaitingReply
                    | WorkerState::UserTakeover => Liveness::Live,
                    WorkerState::Settled(_) => Liveness::Exited,
                    // A lost record is not evidence that the process ended.
                    WorkerState::Missing | WorkerState::Unknown => Liveness::Unverifiable,
                },
            })
            .collect())
    }
}
