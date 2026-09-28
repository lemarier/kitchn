//! Shared execution-backend contract and effect recovery, exercised with the
//! in-memory fake backend. These are simulated results, not live runtime
//! evidence; later adapters run `contracts::conformance::run` themselves.

mod common;

use common::{
    Fixture, ManualClock, TestResult, at, backend_id, creator, grants, holder, house, launch,
    other_house, plan, scheduled, spec, task_id, ttl,
};
use kitchen::{
    BackendId, Error, HouseId, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, BackendDescriptor, BackendUnavailable,
        Capability, CapabilitySet, ContractError, Disposition, EffectExecutor, EffectFailure,
        EffectRequest, ExternalRef, Fence, Grant, HouseGrants, Lookup, NotAppliedReason, Operation,
        Permission, Receipt, ResourceKind, ResourceRef, Settlement, TaskAuthority, Text,
        UncertainReason, WorkerBackend, WorkerState,
        conformance::{self, Check, CheckResult, ConformanceFixture},
        fake::{ExecuteFault, FakeBackend},
    },
    state::{EffectState, StateError, reconcile, run_effect},
};

fn conformance_fixture() -> TestResult<ConformanceFixture> {
    Ok(ConformanceFixture {
        house: house()?,
        foreign_house: other_house()?,
        foreign_backend: BackendId::new("fake-other")?,
        credential: common::credential()?,
        repository: kitchen::contracts::Repository::new("origin89hq/km43")?,
        task: task_id("conformance")?,
        run_tag: ExternalRef::new("run-1")?,
        brief: Text::new("Conformance probe; exit immediately.")?,
    })
}

fn fake(capabilities: impl IntoIterator<Item = Capability>) -> TestResult<FakeBackend> {
    Ok(FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting(capabilities),
    ))
}

/// A worker-launching backend with lookup but no provider-side idempotency.
fn non_idempotent() -> TestResult<FakeBackend> {
    fake([
        Capability::WorkerLaunchIsolated,
        Capability::WorkerStatusAndOutcome,
        Capability::EffectLookup,
    ])
}

fn started(fixture: &Fixture, id: &str) -> TestResult<(TaskId, Fence)> {
    let task = task_id(id)?;
    fixture.store.create_task(spec(id)?, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

#[test]
fn fake_backend_passes_the_shared_contract() -> TestResult {
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let report = conformance::run_worker(&backend, &conformance_fixture()?)?;
    for check in [
        Check::DescriptorHouse,
        Check::CrossHouseRefused,
        Check::ForeignBackendRefused,
        Check::UnsupportedRefused,
        Check::UnknownKeyNotApplied,
        Check::ProbeReceipt,
        Check::LookupMatchesReceipt,
        Check::IdempotentResubmission,
        Check::LaunchReceipt,
        Check::LaunchObservable,
        Check::InventoryListsLaunch,
        Check::MessageRecovery,
        Check::CancelObserved,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
    assert_eq!(
        backend.effects_performed(),
        3,
        "one launch, one message, and one cancel"
    );
    Ok(())
}

#[test]
fn minimal_backend_passes_with_checks_marked_not_applicable() -> TestResult {
    let backend = fake([])?;
    let report = conformance::run_worker(&backend, &conformance_fixture()?)?;
    assert_eq!(
        report.result(Check::UnsupportedRefused),
        Some(CheckResult::Passed)
    );
    assert_eq!(
        report.result(Check::LaunchReceipt),
        Some(CheckResult::NotApplicable {
            requires: Capability::WorkerLaunchIsolated
        })
    );
    assert_eq!(
        report.result(Check::UnknownKeyNotApplied),
        Some(CheckResult::NotApplicable {
            requires: Capability::LookupLaunchWorker
        })
    );
    assert_eq!(backend.effects_performed(), 0);
    Ok(())
}

/// Declares fewer capabilities than it actually exercises.
struct OverreachingBackend {
    inner: FakeBackend,
    declared: BackendDescriptor,
}

impl EffectExecutor for OverreachingBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.declared
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.inner.execute(request)
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(request)
    }
}

impl WorkerBackend for OverreachingBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.inner.observe_worker(worker)
    }
}

/// Ignores the request's house, acting with its own credentials.
struct HouseBlindBackend(FakeBackend);

impl EffectExecutor for HouseBlindBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let rewritten = EffectRequest::new(
            self.0.descriptor().house.clone(),
            request.backend().clone(),
            request.credential().clone(),
            request.task().clone(),
            request.attempt(),
            request.key().clone(),
            request.effect().clone(),
        );
        self.0.execute(&rewritten)
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.0.lookup(request)
    }
}

impl WorkerBackend for HouseBlindBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.0.observe_worker(worker)
    }
}

/// Claims every key it is asked about was applied.
struct OptimisticLookupBackend(FakeBackend);

impl EffectExecutor for OptimisticLookupBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.0.execute(request)
    }
    fn lookup(&self, _request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let receipt = Receipt::new(
            ExternalRef::new("made-up").map_err(|_| BackendUnavailable::Transport)?,
            Vec::new(),
            Vec::new(),
        )
        .map_err(|_| BackendUnavailable::Transport)?;
        Ok(Lookup::Applied(receipt))
    }
}

impl WorkerBackend for OptimisticLookupBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.0.observe_worker(worker)
    }
}

#[test]
fn contract_detects_misbehaving_backends() -> TestResult {
    let fixture = conformance_fixture()?;
    let overreaching = OverreachingBackend {
        inner: FakeBackend::fully_capable(backend_id()?, house()?),
        declared: BackendDescriptor {
            backend: backend_id()?,
            house: house()?,
            capabilities: CapabilitySet::supporting([Capability::EffectLookup]),
        },
    };
    let failure = conformance::run_worker(&overreaching, &fixture)
        .err()
        .ok_or("overreach passed")?;
    assert_eq!(failure.check, Check::UnsupportedRefused);

    let blind = HouseBlindBackend(FakeBackend::fully_capable(backend_id()?, house()?));
    let failure = conformance::run_worker(&blind, &fixture)
        .err()
        .ok_or("house-blind backend passed")?;
    assert_eq!(failure.check, Check::CrossHouseRefused);

    let optimistic = OptimisticLookupBackend(FakeBackend::fully_capable(backend_id()?, house()?));
    let failure = conformance::run_worker(&optimistic, &fixture)
        .err()
        .ok_or("optimistic lookup passed")?;
    assert_eq!(
        failure.check,
        Check::CrossHouseRefused,
        "a refused request must not look applied"
    );

    let mut long_tag = fixture.clone();
    long_tag.run_tag = ExternalRef::new(&"t".repeat(250))?;
    let failure = conformance::run_worker(
        &FakeBackend::fully_capable(backend_id()?, house()?),
        &long_tag,
    )
    .err()
    .ok_or("oversized run tag passed")?;
    assert_eq!(failure.check, Check::Fixture);

    let foreign = FakeBackend::fully_capable(backend_id()?, other_house()?);
    let failure = conformance::run_worker(&foreign, &fixture)
        .err()
        .ok_or("foreign descriptor passed")?;
    assert_eq!(failure.check, Check::DescriptorHouse);
    Ok(())
}

#[test]
fn unsupported_capability_is_refused_before_intent_is_persisted() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = fake([Capability::EffectLookup])?;
    let result = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &ManualClock::starting_at(1),
    );
    assert!(matches!(
        result,
        Err(Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. }))
            if missing == &[Capability::WorkerLaunchIsolated]
    ));
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(backend.effects_performed(), 0);
    Ok(())
}

#[test]
fn a_backend_for_another_house_is_refused() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, other_house()?);
    let clock = ManualClock::starting_at(1);
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock
        ),
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));
    assert!(matches!(
        reconcile(&fixture.store, &backend, &task, fence, &clock),
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(backend.effects_performed(), 0);
    Ok(())
}

#[test]
fn launch_records_receipt_but_not_readiness() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    let EffectState::Applied { receipt, .. } = record.state() else {
        return Err("launch was not applied".into());
    };
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .ok_or("receipt names no worker")?;
    assert_eq!(backend.observe_worker(worker)?, WorkerState::Starting);
    backend.set_worker_state(worker, WorkerState::Ready);
    assert_eq!(backend.observe_worker(worker)?, WorkerState::Ready);

    let again = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert_eq!(
        again, record,
        "a duplicate trigger returns the recorded effect"
    );
    assert_eq!(backend.effects_performed(), 1);
    Ok(())
}

#[test]
fn pickup_uncertain_launch_no_retry() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = non_idempotent()?;
    let clock = ManualClock::starting_at(1);
    backend.inject(ExecuteFault::ApplyThenLoseResponse);

    let record = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert!(matches!(
        record.state(),
        EffectState::Uncertain {
            reason: UncertainReason::ResponseLost,
            ..
        }
    ));
    assert!(matches!(
        run_effect(&fixture.store, &backend, &grants()?, plan(&task, fence, "launch", launch()?)?, &clock),
        Err(Error::State(StateError::UnsafeRetry(seq))) if seq == record.seq()
    ));
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch-again", launch()?)?,
            &clock
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert_eq!(backend.effects_performed(), 1);

    let report = reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(report.unresolved.is_empty());
    assert!(
        matches!(report.resolved.as_slice(), [effect] if matches!(effect.state(), EffectState::Applied { .. }))
    );
    assert_eq!(
        backend.effects_performed(),
        1,
        "the launch was never repeated"
    );
    Ok(())
}

#[test]
fn idempotent_backend_resubmits_an_uncertain_effect_with_the_same_key() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let run = |name: &str| -> TestResult<_> {
        Ok(run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, name, launch()?)?,
            &clock,
        )?)
    };

    // A lost response is reconciled by lookup; nothing is resubmitted.
    backend.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run("launch")?;
    assert!(matches!(lost.state(), EffectState::Uncertain { .. }));
    let recovered = run("launch")?;
    assert_eq!(recovered.request().key(), lost.request().key());
    assert!(matches!(recovered.state(), EffectState::Applied { .. }));
    assert_eq!(recovered.submissions(), 1);
    assert_eq!(backend.execute_calls(), 1);

    // When the lookup is inconclusive, the same key is resubmitted and the
    // provider deduplicates it.
    backend.inject(ExecuteFault::ApplyThenLoseResponse);
    let second = run("second")?;
    backend.fail_lookups(1);
    let resubmitted = run("second")?;
    assert_eq!(resubmitted.request().key(), second.request().key());
    assert!(matches!(resubmitted.state(), EffectState::Applied { .. }));
    assert_eq!(resubmitted.submissions(), 2);
    assert_eq!(backend.execute_calls(), 3);
    assert_eq!(
        backend.effects_performed(),
        2,
        "the provider deduplicated the key"
    );
    Ok(())
}

#[test]
fn resubmission_is_bounded_by_elapsed_time() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    backend.fail_lookups(100);
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    let lost = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    fixture.store.renew(&task, fence, ttl(7200)?, at(2))?;
    // The retry policy allows one hour from the first intent.
    clock.advance(3601);
    assert!(matches!(
        run_effect(&fixture.store, &backend, &grants()?, plan(&task, fence, "launch", launch()?)?, &clock),
        Err(Error::State(StateError::SubmissionBudgetExhausted(seq))) if seq == lost.seq()
    ));
    assert_eq!(backend.execute_calls(), 1);
    assert!(matches!(
        fixture.store.task(&task)?.effects(),
        [effect] if matches!(effect.state(), EffectState::Uncertain { reason: UncertainReason::LookupInconclusive, .. })
    ));
    Ok(())
}

#[test]
fn absence_after_reconcile_still_counts_against_the_budget() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    for _ in 0..3 {
        backend.inject(ExecuteFault::TimeoutWithoutApplying);
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock,
        )?;
    }
    // Each timeout was confirmed absent and replaced by a fresh key; the
    // logical effect still used its three submissions.
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock
        ),
        Err(Error::State(StateError::SubmissionBudgetExhausted(_)))
    ));
    assert_eq!(backend.execute_calls(), 3);
    assert_eq!(backend.effects_performed(), 0);
    Ok(())
}

#[test]
fn reconcile_confirms_absence_and_allows_a_fresh_effect() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = non_idempotent()?;
    let clock = ManualClock::starting_at(1);
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    let lost = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;

    backend.fail_lookups(1);
    let inconclusive = reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(matches!(
        inconclusive.unresolved.as_slice(),
        [effect] if matches!(effect.state(), EffectState::Uncertain { reason: UncertainReason::LookupInconclusive, .. })
    ));

    let confirmed = reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(matches!(
        confirmed.resolved.as_slice(),
        [effect] if matches!(effect.state(), EffectState::NotApplied { reason: NotAppliedReason::ConfirmedAbsent, .. })
    ));
    let fresh = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert_ne!(
        fresh.request().key(),
        lost.request().key(),
        "a new effect gets a new key"
    );
    assert!(matches!(fresh.state(), EffectState::Applied { .. }));
    assert_eq!(backend.effects_performed(), 1);
    Ok(())
}

#[test]
fn without_lookup_an_uncertain_launch_is_handed_over_for_a_decision() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = fake([Capability::WorkerLaunchIsolated])?;
    let clock = ManualClock::starting_at(1);
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    let lost = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;

    let report = reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(matches!(
        report.unresolved.as_slice(),
        [effect] if matches!(effect.state(), EffectState::Uncertain { reason: UncertainReason::LookupUnsupported, .. })
    ));
    assert!(matches!(
        fixture.store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(2)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    fixture.store.record_effect_outcome(
        &task,
        fence,
        lost.seq(),
        kitchen::state::EffectOutcome::Unresolvable,
        at(3),
    )?;
    // Handing over is not success: the reservation holds.
    assert!(matches!(
        fixture.store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(4)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert!(matches!(
        fixture.store.settle_cancelled(&task, fence, at(4)),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert!(fixture.store.recovery_queue(at(4))?.contains(
        &kitchen::state::RecoveryItem::HandedOver {
            task: task.clone(),
            seq: lost.seq()
        }
    ));

    let stop = kitchen::state::RiskDecision {
        effect: lost.request().key().clone(),
        decided_by: holder("operator")?,
        revision: kitchen::contracts::EvidenceRevision::INITIAL,
        action: kitchen::state::RiskAction::SettleUnsuccessfully,
    };
    let waived = fixture
        .store
        .accept_risk(&task, fence, lost.seq(), stop.clone(), at(5))?;
    assert!(matches!(waived.state(), EffectState::Waived { decision, .. } if *decision == stop));
    assert!(matches!(
        fixture.store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(6)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "relaunch", launch()?)?,
            &clock
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert_eq!(
        fixture.store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Failed(kitchen::contracts::FailureClass::Retryable),
            at(6)
        )?,
        Disposition::Settled(Settlement::Failed),
        "the decision allows only an unsuccessful settlement"
    );
    Ok(())
}

#[test]
fn recovery_after_interruption_reconciles_before_relaunch() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, old) = started(&fixture, "task-1")?;
    let backend = non_idempotent()?;

    // Coordinator A persists intent, the backend applies it, and A exits
    // before recording the outcome.
    let kitchen::state::EffectStart::Execute(intent) = fixture.store.begin_effect(
        plan(&task, old, "launch", launch()?)?,
        &grants()?,
        &common::refusing()?,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    backend.execute(intent.request())?;

    // Coordinator B starts in a new process after A's lease expired.
    let store = fixture.reopen()?;
    let clock = ManualClock::starting_at(120);
    let lease = store.take_over(&task, &scheduled("coordinator-b")?, ttl(60)?, at(120))?;
    assert!(matches!(
        store.start_attempt(&task, lease.fence(), at(120)),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert!(matches!(
        reconcile(&store, &backend, &task, old, &clock),
        Err(Error::State(StateError::StaleFence { .. }))
    ));

    let report = reconcile(&store, &backend, &task, lease.fence(), &clock)?;
    let [recovered] = report.resolved.as_slice() else {
        return Err("launch was not reconciled".into());
    };
    let EffectState::Applied { receipt, .. } = recovered.state() else {
        return Err("launch not recorded as applied".into());
    };
    assert!(
        receipt
            .created()
            .iter()
            .any(|resource| resource.kind == ResourceKind::Worker)
    );
    assert_eq!(
        store.start_attempt(&task, lease.fence(), at(121))?,
        AttemptStart::Started(AttemptNumber::new(2).ok_or("attempt 2")?)
    );
    assert_eq!(
        backend.effects_performed(),
        1,
        "the interrupted launch was not repeated"
    );
    Ok(())
}

#[test]
fn backend_refusals_are_recorded_as_not_applied() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = non_idempotent()?;
    let clock = ManualClock::starting_at(1);
    backend.inject(ExecuteFault::Reject);
    let refused = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert!(matches!(
        refused.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));
    let absent = ResourceRef {
        kind: ResourceKind::Worker,
        backend: BackendId::new("fake")?,
        handle: ExternalRef::new("no-such-worker")?,
    };
    let unsupported = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(
            &task,
            fence,
            "cancel",
            Operation::CancelWorker { worker: absent },
        )?,
        &clock,
    );
    assert!(matches!(
        unsupported,
        Err(Error::Contract(
            ContractError::UnsupportedCapabilities { .. }
        ))
    ));
    assert_eq!(backend.effects_performed(), 0);
    assert!(HouseId::new(common::HOUSE).is_ok());
    Ok(())
}

#[test]
fn settled_task_identity_is_never_reused() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    fixture.store.finish_attempt(
        &task,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(2),
    )?;

    // The settled record keeps its identity and keys; nothing deletes it.
    assert_eq!(
        fixture
            .store
            .create_task(spec("task-1")?, &creator()?, at(100))?,
        kitchen::state::Creation::AlreadyExists
    );
    assert!(matches!(
        fixture
            .store
            .claim(&task, &scheduled("coordinator-b")?, ttl(60)?, at(101)),
        Err(Error::State(StateError::TaskSettled { .. }))
    ));
    let record = fixture.store.task(&task)?;
    assert!(
        matches!(record.effects(), [effect] if effect.request().key() == first.request().key())
    );
    assert_eq!(backend.effects_performed(), 1);
    Ok(())
}

#[test]
fn recovery_stays_on_the_backend_namespace_that_received_the_effect() -> TestResult {
    let fixture = Fixture::new()?;
    let other = BackendId::new("fake-other")?;
    // The house and task may launch on both namespaces.
    let both = [
        common::grant(Permission::LaunchWorker)?,
        Grant::house(
            Permission::LaunchWorker,
            other.clone(),
            common::credential()?,
        ),
    ];
    let grants = HouseGrants::new(house()?, both.clone());
    let mut workflow = spec("task-1")?;
    workflow.authority = TaskAuthority::delegate(&grants, both)?;
    let task = task_id("task-1")?;
    fixture.store.create_task(workflow, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let first = FakeBackend::fully_capable(backend_id()?, house()?);
    let second = FakeBackend::fully_capable(other, house()?);
    let clock = ManualClock::starting_at(1);
    first.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run_effect(
        &fixture.store,
        &first,
        &grants,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert!(matches!(lost.state(), EffectState::Uncertain { .. }));

    assert!(matches!(
        run_effect(&fixture.store, &second, &grants, plan(&task, fence, "launch", launch()?)?, &clock),
        Err(Error::State(StateError::BackendMismatch { seq, ref recorded })) if seq == lost.seq() && recorded == &backend_id()?
    ));
    let report = reconcile(&fixture.store, &second, &task, fence, &clock)?;
    assert!(report.resolved.is_empty() && report.unresolved.is_empty());
    assert!(matches!(report.foreign.as_slice(), [effect] if effect.seq() == lost.seq()));
    assert_eq!(
        second.effects_performed(),
        0,
        "the other namespace executed the key"
    );
    assert!(
        matches!(fixture.store.task(&task)?.effects(), [effect] if matches!(effect.state(), EffectState::Uncertain { .. })),
        "the other namespace resolved the effect"
    );
    // The namespace that received the launch still resolves it.
    let report = reconcile(&fixture.store, &first, &task, fence, &clock)?;
    assert!(
        matches!(report.resolved.as_slice(), [effect] if matches!(effect.state(), EffectState::Applied { .. }))
    );
    assert_eq!(first.effects_performed(), 1);
    Ok(())
}

#[test]
fn workflow_capability_requirements_are_checked_at_execution() -> TestResult {
    let fixture = Fixture::new()?;
    let task = task_id("task-1")?;
    let mut workflow = spec("task-1")?;
    workflow.requires = [
        Capability::WorkerLaunchReadiness,
        Capability::ScheduleRunTimeout,
    ]
    .into();
    fixture.store.create_task(workflow, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let clock = ManualClock::starting_at(1);

    // Launch alone is supported; the workflow's readiness requirement is
    // missing and its run timeout is only partial.
    let backend = FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting([Capability::WorkerLaunchIsolated]).with(
            Capability::ScheduleRunTimeout,
            kitchen::contracts::Support::Partial,
        ),
    );
    let refused = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    );
    assert!(matches!(
        refused,
        Err(Error::Contract(ContractError::UnsupportedCapabilities { ref missing, ref partial }))
            if missing == &[Capability::WorkerLaunchReadiness]
                && partial == &[Capability::ScheduleRunTimeout]
    ));
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(backend.effects_performed(), 0);

    let capable = FakeBackend::fully_capable(backend_id()?, house()?);
    let launched = run_effect(
        &fixture.store,
        &capable,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert!(matches!(launched.state(), EffectState::Applied { .. }));
    let stored = fixture.store.task(&task)?;
    assert_eq!(
        stored.spec().requires.iter().copied().collect::<Vec<_>>(),
        [
            Capability::ScheduleRunTimeout,
            Capability::WorkerLaunchReadiness
        ]
    );
    Ok(())
}

#[test]
fn same_key_resubmission_is_bounded_by_count() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    backend.fail_lookups(100);
    let mut outcomes = Vec::new();
    for _ in 0..5 {
        backend.inject(ExecuteFault::TimeoutWithoutApplying);
        outcomes.push(run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock,
        ));
    }
    // The retry policy allows three submissions of one logical effect.
    assert!(outcomes.iter().take(3).all(Result::is_ok));
    assert_eq!(backend.execute_calls(), 3);
    assert!(
        outcomes.iter().skip(3).all(|outcome| matches!(
            outcome,
            Err(Error::State(StateError::SubmissionBudgetExhausted(_)))
        )),
        "{outcomes:?}"
    );
    Ok(())
}

#[test]
fn a_requested_cancellation_can_stop_the_task_worker() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let launched = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    let EffectState::Applied { receipt, .. } = launched.state() else {
        return Err("launch was not applied".into());
    };
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("receipt names no worker")?;
    fixture
        .store
        .request_cancel(&task, &holder("operator")?, at(2))?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "more", launch()?)?,
            &clock
        ),
        Err(Error::State(StateError::CancelRequested))
    ));
    let stranger = ResourceRef {
        kind: ResourceKind::Worker,
        backend: backend_id()?,
        handle: ExternalRef::new("someone-elses-worker")?,
    };
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(
                &task,
                fence,
                "cancel-other",
                Operation::CancelWorker { worker: stranger }
            )?,
            &clock
        ),
        Err(Error::State(StateError::ResourceNotOwned))
    ));

    let cancelled = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(
            &task,
            fence,
            "cancel",
            Operation::CancelWorker {
                worker: worker.clone(),
            },
        )?,
        &clock,
    )?;
    assert!(matches!(cancelled.state(), EffectState::Applied { .. }));
    assert_eq!(
        backend.observe_worker(&worker)?,
        WorkerState::Settled(kitchen::contracts::WorkerOutcome::Cancelled)
    );
    fixture.store.settle_cancelled(&task, fence, at(3))?;
    Ok(())
}

#[test]
fn cancelling_an_uncertain_launch_keeps_its_uncertainty() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = fake([Capability::WorkerLaunchIsolated, Capability::WorkerCancel])?;
    let clock = ManualClock::starting_at(1);
    backend.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert_eq!(
        fixture
            .store
            .request_cancel(&task, &holder("operator")?, at(2))?,
        kitchen::state::CancelStatus::Pending
    );
    assert!(matches!(
        fixture.store.settle_cancelled(&task, fence, at(3)),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    fixture.store.record_effect_outcome(
        &task,
        fence,
        lost.seq(),
        kitchen::state::EffectOutcome::Unresolvable,
        at(3),
    )?;
    fixture.store.accept_risk(
        &task,
        fence,
        lost.seq(),
        kitchen::state::RiskDecision {
            effect: lost.request().key().clone(),
            decided_by: holder("operator")?,
            revision: kitchen::contracts::EvidenceRevision::INITIAL,
            action: kitchen::state::RiskAction::SettleUnsuccessfully,
        },
        at(4),
    )?;
    fixture.store.settle_cancelled(&task, fence, at(5))?;
    let record = fixture.store.task(&task)?;
    assert!(matches!(
        record.state(),
        kitchen::state::TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    assert!(
        matches!(record.effects(), [effect] if matches!(effect.state(), EffectState::Waived { .. })),
        "the launch stays recorded as unknown"
    );
    Ok(())
}

/// Exits the process inside `execute`, after intent is durable and before
/// anything reaches a provider.
struct ExitingBackend(FakeBackend);

impl EffectExecutor for ExitingBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }
    fn execute(&self, _request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        std::process::exit(44)
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.0.lookup(request)
    }
}

impl WorkerBackend for ExitingBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.0.observe_worker(worker)
    }
}

const CRASH_DIR: &str = "KITCHEN_CRASH_STORE";
const CRASH_FENCE: &str = "KITCHEN_CRASH_FENCE";

/// The child half of `abrupt_exit_after_intent_leaves_a_reconcilable_effect`.
/// It does nothing unless that test starts it.
#[test]
fn crash_child_exits_inside_execute() -> TestResult {
    let (Some(dir), Some(fence)) = (std::env::var_os(CRASH_DIR), std::env::var_os(CRASH_FENCE))
    else {
        return Ok(());
    };
    let store =
        kitchen::state::HouseStore::open(dir, house()?, kitchen::state::StoreOptions::default())?;
    let fence: u64 = fence.to_str().ok_or("fence")?.parse()?;
    let lease_fence = match store.task(&task_id("task-1")?)?.state() {
        kitchen::state::TaskState::Claimed { lease } if lease.fence().get() == fence => {
            lease.fence()
        }
        kitchen::state::TaskState::Claimed { .. }
        | kitchen::state::TaskState::Open
        | kitchen::state::TaskState::Settled { .. } => {
            return Err("the parent's claim is missing".into());
        }
    };
    let backend = ExitingBackend(FakeBackend::fully_capable(backend_id()?, house()?));
    run_effect(
        &store,
        &backend,
        &grants()?,
        plan(&task_id("task-1")?, lease_fence, "launch", launch()?)?,
        &ManualClock::starting_at(1),
    )?;
    Err("the child returned from execute".into())
}

#[test]
fn abrupt_exit_after_intent_leaves_a_reconcilable_effect() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "crash_child_exits_inside_execute", "--nocapture"])
        .env(CRASH_DIR, fixture.dir.path().join("house"))
        .env(CRASH_FENCE, fence.get().to_string())
        .status()?;
    assert_eq!(
        status.code(),
        Some(44),
        "the child must exit inside execute"
    );

    // A new process finds the durable intent and an owner that is gone.
    let store = fixture.reopen()?;
    let record = store.task(&task)?;
    assert!(matches!(
        record.effects(),
        [effect] if effect.state() == &EffectState::Intended && effect.submissions() == 1
    ));
    assert!(store.recovery_queue(at(61))?.contains(
        &kitchen::state::RecoveryItem::UncertainTaskOwner {
            task: task.clone(),
            holder: holder("coordinator-a")?,
            expired_at: at(60),
        }
    ));
    let lease = store.take_over(&task, &scheduled("coordinator-b")?, ttl(60)?, at(61))?;
    assert!(matches!(
        store.start_attempt(&task, lease.fence(), at(61)),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    // Without lookup nothing can establish the outcome, so nothing reruns.
    let backend = fake([Capability::WorkerLaunchIsolated])?;
    let report = reconcile(
        &store,
        &backend,
        &task,
        lease.fence(),
        &ManualClock::starting_at(62),
    )?;
    assert!(matches!(
        report.unresolved.as_slice(),
        [effect] if matches!(effect.state(), EffectState::Uncertain { reason: UncertainReason::LookupUnsupported, .. })
    ));
    assert_eq!(backend.execute_calls(), 0);
    Ok(())
}

/// Loses its worker record after an uncertain cancel, while the worker may run on.
struct ForgetfulCancelBackend(FakeBackend);

impl EffectExecutor for ForgetfulCancelBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        match request.effect() {
            kitchen::contracts::Effect::Worker(Operation::CancelWorker { worker }) => {
                self.0.set_worker_state(worker, WorkerState::Missing);
                Err(EffectFailure::Uncertain(UncertainReason::Timeout))
            }
            _ => self.0.execute(request),
        }
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.0.lookup(request)
    }
}

impl WorkerBackend for ForgetfulCancelBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.0.observe_worker(worker)
    }
    fn inventory(
        &self,
    ) -> Result<Vec<kitchen::contracts::ResourceObservation>, BackendUnavailable> {
        self.0.inventory()
    }
}

#[test]
fn a_missing_worker_is_not_evidence_of_cancellation() -> TestResult {
    let backend = ForgetfulCancelBackend(FakeBackend::fully_capable(backend_id()?, house()?));
    let failure = conformance::run_worker(&backend, &conformance_fixture()?)
        .err()
        .ok_or("a missing worker passed as cancelled")?;
    assert_eq!(failure.check, Check::CancelObserved);
    Ok(())
}

#[test]
fn fresh_and_same_key_retries_share_one_budget() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let run = || {
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock,
        )
        .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
    };
    backend.inject(ExecuteFault::Reject);
    run()?; // key A: refused, one submission
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    run()?; // key B: second submission
    backend.fail_lookups(100);
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    run()?; // key B resubmitted: third submission
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    let fourth = run();
    assert!(
        matches!(&fourth, Err(error) if matches!(error.downcast_ref::<Error>(), Some(Error::State(StateError::SubmissionBudgetExhausted(_))))),
        "{fourth:?}"
    );
    assert_eq!(backend.execute_calls(), 3);
    Ok(())
}

#[test]
fn the_elapsed_budget_runs_from_the_first_key() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    fixture.store.renew(&task, fence, ttl(7200)?, at(0))?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let run = || {
        run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock,
        )
        .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
    };
    backend.inject(ExecuteFault::Reject);
    run()?; // key A at t=1
    clock.advance(3500);
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    run()?; // key B at t=3501, inside the hour
    clock.advance(101);
    backend.fail_lookups(100);
    let late = run(); // t=3602: past one hour from key A
    assert!(
        matches!(&late, Err(error) if matches!(error.downcast_ref::<Error>(), Some(Error::State(StateError::SubmissionBudgetExhausted(_))))),
        "{late:?}"
    );
    assert_eq!(backend.execute_calls(), 2);
    Ok(())
}

/// Blocks its first lookup until the test releases it.
struct GatedLookup {
    inner: FakeBackend,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl EffectExecutor for GatedLookup {
    fn descriptor(&self) -> &BackendDescriptor {
        self.inner.descriptor()
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.inner.execute(request)
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let answer = self.inner.lookup(request);
        let _ = self.entered.send(());
        let _ = self
            .release
            .lock()
            .map_err(|_| BackendUnavailable::Transport)?
            .recv();
        answer
    }
}

#[test]
fn a_delayed_absence_cannot_clear_a_newer_submission() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = started(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch()?)?,
        &ManualClock::starting_at(1),
    )?;

    // Reconciler A looks up submission 1 and truthfully sees it absent, but
    // records the answer only after submission 2 was sent.
    let (entered, wait_entered) = std::sync::mpsc::sync_channel(1);
    let (release, wait_release) = std::sync::mpsc::sync_channel(1);
    let gated = GatedLookup {
        inner: FakeBackend::fully_capable(backend_id()?, house()?),
        entered,
        release: std::sync::Mutex::new(wait_release),
    };
    let store = fixture.reopen()?;
    let reconciled = std::thread::scope(|scope| -> TestResult<_> {
        let reconciler = scope.spawn(|| {
            reconcile(&store, &gated, &task, fence, &ManualClock::starting_at(2))
                .map_err(|error| error.to_string())
        });
        wait_entered.recv()?;
        backend.fail_lookups(1);
        backend.inject(ExecuteFault::TimeoutWithoutApplying);
        let resubmitted = run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &ManualClock::starting_at(3),
        )?;
        assert_eq!(resubmitted.submissions(), 2);
        release.send(())?;
        Ok(reconciler.join().map_err(|_| "reconciler panicked")??)
    })?;
    assert!(reconciled.resolved.is_empty(), "{reconciled:?}");
    let record = fixture.store.task(&task)?;
    let [effect] = record.effects() else {
        return Err("expected one effect".into());
    };
    assert!(
        !matches!(effect.state(), EffectState::NotApplied { .. }),
        "stale absence cleared submission 2: {:?}",
        effect.state()
    );
    assert_eq!(effect.submissions(), 2);
    Ok(())
}

#[test]
fn a_new_owner_can_stop_the_worker_after_cancellation() -> TestResult {
    for adopt in [false, true] {
        let fixture = Fixture::new()?;
        let (task, fence) = started(&fixture, "task-1")?;
        let backend = FakeBackend::fully_capable(backend_id()?, house()?);
        let clock = ManualClock::starting_at(1);
        let launched = run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock,
        )?;
        let EffectState::Applied { receipt, .. } = launched.state() else {
            return Err("launch not applied".into());
        };
        let worker = receipt
            .created()
            .iter()
            .find(|resource| resource.kind == ResourceKind::Worker)
            .cloned()
            .ok_or("receipt names no worker")?;
        fixture
            .store
            .request_cancel(&task, &holder("operator")?, at(2))?;
        let owner = if adopt {
            fixture.store.relinquish(&task, fence, at(3))?;
            fixture
                .store
                .claim(&task, &scheduled("coordinator-b")?, ttl(60)?, at(4))?
        } else {
            fixture
                .store
                .take_over(&task, &scheduled("coordinator-b")?, ttl(60)?, at(61))?
        };
        assert!(matches!(
            fixture.store.start_attempt(&task, owner.fence(), at(62)),
            Err(Error::State(StateError::CancelRequested))
        ));
        let stopped = run_effect(
            &fixture.store,
            &backend,
            &grants()?,
            plan(
                &task,
                owner.fence(),
                "cancel",
                Operation::CancelWorker {
                    worker: worker.clone(),
                },
            )?,
            &ManualClock::starting_at(62),
        )?;
        assert!(matches!(stopped.state(), EffectState::Applied { .. }));
        assert_eq!(
            stopped.request().attempt(),
            AttemptNumber::FIRST,
            "no new attempt was started"
        );
        assert!(matches!(
            run_effect(
                &fixture.store,
                &backend,
                &grants()?,
                plan(&task, owner.fence(), "relaunch", launch()?)?,
                &ManualClock::starting_at(62)
            ),
            Err(Error::State(StateError::CancelRequested))
        ));
        fixture
            .store
            .settle_cancelled(&task, owner.fence(), at(63))?;
        assert_eq!(fixture.store.task(&task)?.attempts().len(), 1);
    }
    Ok(())
}

/// Like Orca: launches, cancels, and releases can be looked up and deduplicated; messages cannot.
fn orca_like_capabilities() -> CapabilitySet {
    CapabilitySet::supporting([
        Capability::WorkerLaunchIsolated,
        Capability::WorkerMessaging,
        Capability::WorkerCancel,
        Capability::WorkerStatusAndOutcome,
        Capability::ResourceRelease,
        Capability::LookupLaunchWorker,
        Capability::IdempotentLaunchWorker,
        Capability::LookupCancelWorker,
        Capability::IdempotentCancelWorker,
        Capability::LookupReleaseResource,
        Capability::IdempotentReleaseResource,
    ])
}

#[test]
fn per_kind_declarations_pass_the_shared_contract() -> TestResult {
    let backend = FakeBackend::new(backend_id()?, house()?, orca_like_capabilities());
    let report = conformance::run_worker(&backend, &conformance_fixture()?)?;
    for check in [
        Check::UnknownKeyNotApplied,
        Check::LookupMatchesReceipt,
        Check::IdempotentResubmission,
        Check::MessageRecovery,
        Check::CancelObserved,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
    Ok(())
}

/// Declares lookup and idempotency for messages that it cannot honor.
struct OverclaimingMessages {
    inner: FakeBackend,
    declared: BackendDescriptor,
}

impl EffectExecutor for OverclaimingMessages {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.declared
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.inner.execute(request)
    }
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(request)
    }
}

impl WorkerBackend for OverclaimingMessages {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.inner.observe_worker(worker)
    }
}

#[test]
fn an_overclaimed_message_declaration_fails_the_contract() -> TestResult {
    for claim in [
        Capability::LookupMessageWorker,
        Capability::IdempotentMessageWorker,
    ] {
        let backend = OverclaimingMessages {
            inner: FakeBackend::new(backend_id()?, house()?, orca_like_capabilities()),
            declared: BackendDescriptor {
                backend: backend_id()?,
                house: house()?,
                capabilities: orca_like_capabilities()
                    .with(claim, kitchen::contracts::Support::Supported),
            },
        };
        let failure = conformance::run_worker(&backend, &conformance_fixture()?)
            .err()
            .ok_or("an overclaimed message declaration passed")?;
        assert_eq!(failure.check, Check::MessageRecovery, "{claim}");
    }
    Ok(())
}
