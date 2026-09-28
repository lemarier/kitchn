//! Running effects through an executor with intent persisted first.

use crate::{
    Error, TaskId,
    contracts::{
        Clock, ContractError, EffectExecutor, EffectFailure, Fence, HouseGrants, Lookup,
        NotAppliedReason, UncertainReason,
    },
    state::{
        EffectOutcome, EffectPlan, EffectRecord, EffectStart, HouseStore, StateError, TaskState,
    },
};

type Result<T> = std::result::Result<T, Error>;

/// Execute one logical effect: check the executor, persist intent, call the
/// executor without holding the store lock, then record the outcome. Only the
/// executor namespace recorded with an intent may resubmit it.
///
/// Delivery is at most once per idempotency key only when the backend
/// declares the effect's kind idempotent
/// ([`crate::contracts::BackendDescriptor::idempotent`]); otherwise an uncertain
/// outcome is never resubmitted and must be reconciled with [`reconcile`].
/// With idempotent requests, a repeated call first looks the key up and
/// resubmits it only when the lookup cannot establish the outcome, within
/// the task's retry policy (count and elapsed time) per logical effect.
/// If recording the outcome fails (for example, the claim was taken over),
/// the effect stays `Intended` and the next owner reconciles it.
///
/// # Errors
/// Returns any error from [`HouseStore::begin_effect`] (including
/// [`ContractError::CrossHouse`] and [`ContractError::UnsupportedCapabilities`]
/// before anything is persisted) or [`HouseStore::record_effect_outcome`].
pub fn run_effect(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    grants: &HouseGrants,
    plan: EffectPlan,
    clock: &dyn Clock,
) -> Result<EffectRecord> {
    let task = plan.task.clone();
    let fence = plan.fence;
    let descriptor = executor.descriptor();
    let record = match store.begin_effect(plan.clone(), grants, descriptor, clock.now())? {
        EffectStart::Resolved(record) => return Ok(record),
        EffectStart::Execute(record) => record,
        EffectStart::ReconcileFirst(pending) => {
            let outcome = look_up(executor, &pending);
            // Applied only to the submission generation the lookup observed.
            store.record_submission_outcome(
                &task,
                fence,
                pending.seq(),
                pending.submissions(),
                outcome,
                clock.now(),
            )?;
            match store.begin_effect(plan, grants, descriptor, clock.now())? {
                EffectStart::Execute(record) => record,
                // Another handle changed the effect meanwhile; report it as is.
                EffectStart::Resolved(record) | EffectStart::ReconcileFirst(record) => {
                    return Ok(record);
                }
            }
        }
    };
    let outcome = match executor.execute(record.request()) {
        Ok(receipt) => EffectOutcome::Applied(receipt),
        Err(EffectFailure::NotApplied(reason)) => EffectOutcome::NotApplied(reason),
        Err(EffectFailure::Uncertain(reason)) => EffectOutcome::Uncertain(reason),
    };
    store.record_submission_outcome(
        &task,
        fence,
        record.seq(),
        record.submissions(),
        outcome,
        clock.now(),
    )
}

/// What a lookup establishes about one persisted effect.
fn look_up(executor: &dyn EffectExecutor, effect: &EffectRecord) -> EffectOutcome {
    if !executor
        .descriptor()
        .supports_lookup(effect.request().effect())
    {
        return EffectOutcome::Uncertain(UncertainReason::LookupUnsupported);
    }
    match executor.lookup(effect.request()) {
        Ok(Lookup::Applied(receipt)) => EffectOutcome::Applied(receipt),
        Ok(Lookup::Absent) => EffectOutcome::NotApplied(NotAppliedReason::ConfirmedAbsent),
        Ok(Lookup::Unknown) | Err(_) => {
            EffectOutcome::Uncertain(UncertainReason::LookupInconclusive)
        }
    }
}

/// The result of reconciling a task's unresolved effects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Effects whose outcome is now established.
    pub resolved: Vec<EffectRecord>,
    /// Effects whose outcome is still unknown. They block new effects,
    /// attempts, and settlement until resolved. When the owner cannot
    /// establish the outcome it records [`EffectOutcome::Unresolvable`] to
    /// hand the effect over; that still blocks until a
    /// [`crate::state::RiskDecision`] or positive evidence.
    pub unresolved: Vec<EffectRecord>,
    /// Unresolved effects persisted for another backend namespace. They were
    /// not looked up; reconcile them with the backend that received them.
    pub foreign: Vec<EffectRecord>,
}

/// Ask the executor what happened to each unresolved effect of `task` and
/// record the answers under `fence`.
///
/// Lookups use the persisted idempotency key and never re-execute an effect.
/// Only effects persisted for this backend's namespace are looked up.
/// [`Lookup::Absent`] is trusted only because the backend contract reserves
/// it for proven absence. Where the executor does not declare lookup for the
/// effect's kind ([`crate::contracts::BackendDescriptor::supports_lookup`]), outcomes stay
/// unknown.
///
/// # Errors
/// Returns [`ContractError::CrossHouse`] for a foreign backend and store
/// errors, including [`crate::state::StateError::StaleFence`] when `fence`
/// does not own the task.
pub fn reconcile(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    task: &TaskId,
    fence: Fence,
    clock: &dyn Clock,
) -> Result<ReconcileReport> {
    let descriptor = executor.descriptor();
    if &descriptor.house != store.house() {
        return Err(ContractError::CrossHouse {
            expected: store.house().clone(),
            found: descriptor.house.clone(),
        }
        .into());
    }
    let record = store.task(task)?;
    match record.state() {
        TaskState::Claimed { lease } if lease.fence() == fence => {}
        TaskState::Claimed { .. } | TaskState::Open => {
            return Err(StateError::StaleFence { presented: fence }.into());
        }
        TaskState::Settled { settlement, .. } => {
            return Err(StateError::TaskSettled {
                task: task.clone(),
                settlement: *settlement,
            }
            .into());
        }
    }
    let pending: Vec<EffectRecord> = record.unresolved_effects().cloned().collect();
    let mut report = ReconcileReport::default();
    for effect in pending {
        if effect.request().backend() != &descriptor.backend {
            report.foreign.push(effect);
            continue;
        }
        let outcome = look_up(executor, &effect);
        // A negative or uncertain answer applies only if no newer submission
        // was sent since this lookup's snapshot.
        let updated = store.record_submission_outcome(
            task,
            fence,
            effect.seq(),
            effect.submissions(),
            outcome,
            clock.now(),
        )?;
        if updated.state().is_resolved() {
            report.resolved.push(updated);
        } else {
            report.unresolved.push(updated);
        }
    }
    Ok(report)
}
