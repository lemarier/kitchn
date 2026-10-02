//! Owner cancellation of a stuck task after read-only backend checks.

use std::collections::BTreeSet;

use crate::{
    ErrorClass, HolderId, TaskId,
    contracts::{
        Capability, Claimant, Clock, Effect, Lookup, Operation, Receipt, ResourceKind, ResourceRef,
        Text, WorkerBackend, WorkerState,
    },
    state::{EffectState, HouseStore, TaskRecord, TaskState},
};

/// A refusal names the evidence that prevented cancellation.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    /// The task is already terminal.
    #[error("task is already settled")]
    Settled,
    /// A backend belongs to another house or lacks worker observations.
    #[error("backend cannot inspect this house's workers")]
    Backend,
    /// The command is running in a worker environment or another repository.
    #[error("task cancellation requires the owner in a separate checkout of the task repository")]
    Checkout,
    /// The owner did not confirm the displayed preview in a terminal.
    #[error("task cancellation needs interactive confirmation of the preview")]
    Confirmation,
    /// An effect cannot be proven reconciled.
    #[error("effect {name} blocks cancellation: {cause}")]
    Effect {
        /// Logical effect name.
        name: String,
        /// Why its outcome blocks cancellation.
        cause: &'static str,
    },
    /// A worker has not positively stopped or settled.
    #[error("worker {worker:?} blocks cancellation: {cause}")]
    Worker {
        /// Worker reported by the task.
        worker: ResourceRef,
        /// Backend observation or failure.
        cause: String,
    },
    /// The task changed while being previewed.
    #[error("task changed since cancellation preview; preview again")]
    Changed,
    /// The store refused the transition.
    #[error(transparent)]
    Store(Box<crate::Error>),
}

impl From<crate::Error> for CancelError {
    fn from(error: crate::Error) -> Self {
        Self::Store(Box::new(error))
    }
}

impl CancelError {
    /// CLI handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Settled
            | Self::Backend
            | Self::Checkout
            | Self::Confirmation
            | Self::Effect { .. }
            | Self::Worker { .. } => ErrorClass::Refused,
            Self::Changed => ErrorClass::Conflict,
            Self::Store(error) => error.class(),
        }
    }
}

/// A checked snapshot. The store compares the complete record again before
/// changing it; a preview by itself grants no authority.
pub struct CancelPreview {
    record: TaskRecord,
    workers: usize,
}

impl CancelPreview {
    /// Number of workers positively settled by the backend.
    #[must_use]
    pub const fn workers(&self) -> usize {
        self.workers
    }
    /// Number of effects checked.
    #[must_use]
    pub fn effects(&self) -> usize {
        self.record.effects().len()
    }
}

/// Inspect every effect and worker without changing the store or backend.
/// Applied worker effects whose outcomes can change are looked up under their
/// persisted keys. Applied messages and replies use their durable receipts.
pub fn preview(
    store: &HouseStore,
    backend: &dyn WorkerBackend,
    task: &TaskId,
) -> Result<CancelPreview, CancelError> {
    let record = store.task(task)?;
    if matches!(record.state(), TaskState::Settled { .. }) {
        return Err(CancelError::Settled);
    }
    if &backend.descriptor().house != store.house()
        || !backend
            .descriptor()
            .capabilities
            .supports(Capability::WorkerStatusAndOutcome)
    {
        return Err(CancelError::Backend);
    }
    let mut workers = BTreeSet::new();
    for resource in &record.spec().resources {
        if resource.kind == ResourceKind::Worker {
            workers.insert(resource.clone());
        }
    }
    for effect in record.effects() {
        match effect.state() {
            EffectState::NotApplied { .. } => {}
            EffectState::Applied { receipt, .. } => {
                if let Effect::Worker(operation) = effect.request().effect() {
                    if effect.request().backend() != &backend.descriptor().backend {
                        return Err(CancelError::Effect {
                            name: effect.name().to_string(),
                            cause: "bound backend cannot reconcile this effect",
                        });
                    }
                    if !matches!(
                        operation,
                        Operation::MessageWorker { .. } | Operation::ReplyToWorker { .. }
                    ) {
                        if !backend
                            .descriptor()
                            .supports_lookup(effect.request().effect())
                        {
                            return Err(CancelError::Effect {
                                name: effect.name().to_string(),
                                cause: "bound backend cannot reconcile this effect",
                            });
                        }
                        let outcome =
                            backend
                                .lookup(effect.request())
                                .map_err(|_| CancelError::Effect {
                                    name: effect.name().to_string(),
                                    cause: "backend lookup unavailable",
                                })?;
                        let consistent = match (&outcome, operation) {
                            (Lookup::Applied(found) | Lookup::Ended(found), _)
                                if found == receipt =>
                            {
                                true
                            }
                            (Lookup::Ended(found), Operation::LaunchWorker { .. }) => {
                                same_launch(receipt, found)
                            }
                            (Lookup::Applied(found), Operation::LaunchWorker { .. })
                                if same_launch(receipt, found) =>
                            {
                                receipt_workers(receipt).all(|worker| {
                                    matches!(
                                        backend.observe_worker(worker),
                                        Ok(WorkerState::Settled(
                                            crate::contracts::WorkerOutcome::Cancelled
                                                | crate::contracts::WorkerOutcome::Failed
                                        ))
                                    )
                                })
                            }
                            _ => false,
                        };
                        if !consistent {
                            return Err(CancelError::Effect {
                                name: effect.name().to_string(),
                                cause: "backend outcome is absent, uncertain, or differs from recorded receipt",
                            });
                        }
                    }
                }
                for resource in receipt.created().iter().chain(receipt.touched()) {
                    if resource.kind == ResourceKind::Worker {
                        workers.insert(resource.clone());
                    }
                }
            }
            _ => {
                return Err(CancelError::Effect {
                    name: effect.name().to_string(),
                    cause: "outcome is unresolved or not eligible",
                });
            }
        }
    }
    for worker in &workers {
        let state = backend
            .observe_worker(worker)
            .map_err(|error| CancelError::Worker {
                worker: worker.clone(),
                cause: error.to_string(),
            })?;
        if !matches!(state, WorkerState::Settled(_)) {
            return Err(CancelError::Worker {
                worker: worker.clone(),
                cause: format!("state is {state:?}"),
            });
        }
    }
    Ok(CancelPreview {
        record,
        workers: workers.len(),
    })
}

fn receipt_workers(receipt: &Receipt) -> impl Iterator<Item = &ResourceRef> {
    receipt
        .created()
        .iter()
        .chain(receipt.touched())
        .filter(|resource| resource.kind == ResourceKind::Worker)
}

// Orca reconstructs a stopped dispatch's receipt from its current records;
// branch and worktree details can differ after the worker stops. The task id
// and worker identity must still identify the same launch.
fn same_launch(recorded: &Receipt, found: &Receipt) -> bool {
    recorded.reference() == found.reference()
        && receipt_workers(recorded).next().is_some()
        && receipt_workers(recorded).eq(receipt_workers(found))
}

/// Settle the exact previewed task under a fresh fence. The caller must be a
/// person in a separate checkout and must have explicitly confirmed preview.
pub fn cancel(
    store: &HouseStore,
    backend: &dyn WorkerBackend,
    previewed: CancelPreview,
    person: HolderId,
    reason: Text,
    clock: &dyn Clock,
) -> Result<(), CancelError> {
    let task = previewed.record.spec().id.clone();
    let current = preview(store, backend, &task)?;
    if current.record != previewed.record {
        return Err(CancelError::Changed);
    }
    store.owner_cancel(
        &current.record,
        &Claimant::interactive(person),
        &reason,
        clock.now(),
    )?;
    Ok(())
}
