//! Shared execution-backend contract and effect recovery, exercised with the
//! in-memory fake backend. These are simulated results, not live runtime
//! evidence; later adapters run `contracts::conformance::run` themselves.

mod common;

use common::{
    Fixture, ManualClock, TestResult, at, backend_id, grants, holder, house, launch, other_house,
    plan, spec, task_id, ttl,
};
use kitchen::{
    BackendId, Error, HouseId, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, BackendDescriptor, BackendUnavailable,
        Capability, CapabilitySet, ContractError, Disposition, EffectFailure, EffectRequest,
        ExecutionBackend, ExternalRef, Fence, IdempotencyKey, Lookup, NotAppliedReason, Operation,
        Receipt, ResourceKind, ResourceRef, Settlement, Text, UncertainReason, WorkerState,
        conformance::{self, Check, CheckResult, ConformanceFixture},
        fake::{ExecuteFault, FakeBackend},
    },
    state::{EffectState, StateError, reconcile, run_effect},
};

fn conformance_fixture() -> TestResult<ConformanceFixture> {
    Ok(ConformanceFixture {
        house: house()?,
        foreign_house: other_house()?,
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
    fixture.store.create_task(spec(id)?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &holder("coordinator-a")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

#[test]
fn fake_backend_passes_the_shared_contract() -> TestResult {
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let report = conformance::run(&backend, &conformance_fixture()?)?;
    for check in [
        Check::DescriptorHouse,
        Check::CrossHouseRefused,
        Check::UnsupportedRefused,
        Check::UnknownKeyNotApplied,
        Check::LaunchReceipt,
        Check::LookupMatchesReceipt,
        Check::IdempotentResubmission,
        Check::LaunchObservable,
        Check::CancelObserved,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
    assert_eq!(backend.effects_performed(), 2, "one launch and one cancel");
    Ok(())
}

#[test]
fn minimal_backend_passes_with_checks_marked_not_applicable() -> TestResult {
    let backend = fake([])?;
    let report = conformance::run(&backend, &conformance_fixture()?)?;
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
            requires: Capability::EffectLookup
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

impl ExecutionBackend for OverreachingBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.declared
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.inner.execute(request)
    }
    fn lookup(&self, key: &IdempotencyKey) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(key)
    }
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.inner.observe_worker(worker)
    }
}

/// Ignores the request's house, acting with its own credentials.
struct HouseBlindBackend(FakeBackend);

impl ExecutionBackend for HouseBlindBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let rewritten = EffectRequest::new(
            self.0.descriptor().house.clone(),
            request.task().clone(),
            request.attempt(),
            request.key().clone(),
            request.operation().clone(),
        );
        self.0.execute(&rewritten)
    }
    fn lookup(&self, key: &IdempotencyKey) -> Result<Lookup, BackendUnavailable> {
        self.0.lookup(key)
    }
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.0.observe_worker(worker)
    }
}

/// Claims every key it is asked about was applied.
struct OptimisticLookupBackend(FakeBackend);

impl ExecutionBackend for OptimisticLookupBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.0.execute(request)
    }
    fn lookup(&self, _key: &IdempotencyKey) -> Result<Lookup, BackendUnavailable> {
        let receipt = Receipt::new(
            ExternalRef::new("made-up").map_err(|_| BackendUnavailable::Transport)?,
            Vec::new(),
        )
        .map_err(|_| BackendUnavailable::Transport)?;
        Ok(Lookup::Applied(receipt))
    }
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
    let failure = conformance::run(&overreaching, &fixture)
        .err()
        .ok_or("overreach passed")?;
    assert_eq!(failure.check, Check::UnsupportedRefused);

    let blind = HouseBlindBackend(FakeBackend::fully_capable(backend_id()?, house()?));
    let failure = conformance::run(&blind, &fixture)
        .err()
        .ok_or("house-blind backend passed")?;
    assert_eq!(failure.check, Check::CrossHouseRefused);

    let optimistic = OptimisticLookupBackend(FakeBackend::fully_capable(backend_id()?, house()?));
    let failure = conformance::run(&optimistic, &fixture)
        .err()
        .ok_or("optimistic lookup passed")?;
    assert_eq!(
        failure.check,
        Check::CrossHouseRefused,
        "a refused request must not look applied"
    );

    let mut long_tag = fixture.clone();
    long_tag.run_tag = ExternalRef::new(&"t".repeat(250))?;
    let failure = conformance::run(
        &FakeBackend::fully_capable(backend_id()?, house()?),
        &long_tag,
    )
    .err()
    .ok_or("oversized run tag passed")?;
    assert_eq!(failure.check, Check::Fixture);

    let foreign = FakeBackend::fully_capable(backend_id()?, other_house()?);
    let failure = conformance::run(&foreign, &fixture)
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
        .resources()
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

    backend.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run("launch")?;
    assert!(matches!(lost.state(), EffectState::Uncertain { .. }));
    let resubmitted = run("launch")?;
    assert_eq!(resubmitted.request().key(), lost.request().key());
    assert!(matches!(resubmitted.state(), EffectState::Applied { .. }));
    assert_eq!(
        backend.effects_performed(),
        1,
        "the provider deduplicated the key"
    );

    backend.inject(ExecuteFault::TimeoutWithoutApplying);
    assert!(matches!(
        run("second")?.state(),
        EffectState::Uncertain {
            reason: UncertainReason::Timeout,
            ..
        }
    ));
    assert!(matches!(
        run("second")?.state(),
        EffectState::Applied { .. }
    ));
    assert_eq!(backend.effects_performed(), 2);
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
fn without_lookup_an_uncertain_effect_needs_an_explicit_owner_decision() -> TestResult {
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
        fixture
            .store
            .finish_attempt(&task, fence, AttemptOutcome::Succeeded, at(2)),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    fixture.store.record_effect_outcome(
        &task,
        fence,
        lost.seq(),
        kitchen::state::EffectOutcome::Unresolvable,
        at(3),
    )?;
    assert_eq!(
        fixture
            .store
            .finish_attempt(&task, fence, AttemptOutcome::Succeeded, at(4))?,
        Disposition::Settled(Settlement::Succeeded)
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
        kitchen::state::Resubmission::Refuse,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    backend.execute(intent.request())?;

    // Coordinator B starts in a new process after A's lease expired.
    let store = fixture.reopen()?;
    let clock = ManualClock::starting_at(120);
    let lease = store.take_over(&task, &holder("coordinator-b")?, ttl(60)?, at(120))?;
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
            .resources()
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
    fixture
        .store
        .finish_attempt(&task, fence, AttemptOutcome::Succeeded, at(2))?;

    // The settled record keeps its identity and keys; nothing deletes it.
    assert_eq!(
        fixture.store.create_task(spec("task-1")?, at(100))?,
        kitchen::state::Creation::AlreadyExists
    );
    assert!(matches!(
        fixture
            .store
            .claim(&task, &holder("coordinator-b")?, ttl(60)?, at(101)),
        Err(Error::State(StateError::TaskSettled { .. }))
    ));
    let record = fixture.store.task(&task)?;
    assert!(
        matches!(record.effects(), [effect] if effect.request().key() == first.request().key())
    );
    assert_eq!(backend.effects_performed(), 1);
    Ok(())
}
