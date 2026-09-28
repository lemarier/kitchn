//! Durable task ownership: claims, fencing, attempts, cancellation, evidence,
//! consumer leases, and persisted-state validation. All runs use temporary
//! directories and the in-process store; none use a live backend.

mod common;

use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};

use common::{
    Fixture, TestResult, at, commit, creator, grants, grants_for, holder, house, launch,
    other_house, plan, scheduled, spec, spec_with, task_id, ttl,
};
use kitchen::{
    ConsumerId, Error, HouseId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, ContractError, Disposition, Evidence,
        EvidenceKind, EvidenceRevision, EvidenceVerdict, ExternalRef, FailureClass, Fence,
        NotAppliedReason, Permission, Receipt, RetryPolicy, Settlement, Timestamp, UncertainReason,
    },
    state::{
        AttemptState, CancelStatus, ConsumerEvent, ConsumerState, Consumption, Corruption,
        Creation, EffectOutcome, EffectStart, EffectState, HouseStore, MAX_EVIDENCE_PER_REVISION,
        OwnershipEvent, RecoveryItem, RiskAction, RiskDecision, StateError, StoreOptions,
        TaskState,
    },
};

const SECOND: AttemptNumber = match AttemptNumber::new(2) {
    Some(number) => number,
    None => AttemptNumber::FIRST,
};

fn claimed_attempt(fixture: &Fixture, id: &str, now: Timestamp) -> TestResult<Fence> {
    let task = task_id(id)?;
    fixture.store.create_task(spec(id)?, &creator()?, now)?;
    let lease = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(60)?, now)?;
    assert_eq!(
        fixture.store.start_attempt(&task, lease.fence(), now)?,
        AttemptStart::Started(AttemptNumber::FIRST)
    );
    Ok(lease.fence())
}

fn receipt(reference: &str) -> TestResult<Receipt> {
    Ok(Receipt::new(ExternalRef::new(reference)?, Vec::new())?)
}

fn evidence(subject: char, source: &str) -> TestResult<Evidence> {
    Ok(Evidence {
        kind: EvidenceKind::Check,
        verdict: EvidenceVerdict::Pass,
        subject: commit(subject)?,
        source: ExternalRef::new(source)?,
        observed_at: at(1),
    })
}

#[test]
fn successful_attempt_settles_and_releases_the_claim() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let disposition = fixture.store.finish_attempt(
        &task,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(5),
    )?;
    assert_eq!(disposition, Disposition::Settled(Settlement::Succeeded));

    let record = fixture.reopen()?.task(&task)?;
    assert_eq!(
        record.state(),
        &TaskState::Settled {
            settlement: Settlement::Succeeded,
            at: at(5)
        }
    );
    assert!(matches!(
        record.ownership(),
        [OwnershipEvent::Claimed { .. }, OwnershipEvent::Released { fence: released, .. }] if *released == fence
    ));
    assert!(matches!(
        fixture
            .store
            .claim(&task, &scheduled("late")?, ttl(60)?, at(6)),
        Err(Error::State(StateError::TaskSettled {
            settlement: Settlement::Succeeded,
            ..
        }))
    ));
    Ok(())
}

#[test]
fn task_creation_is_idempotent_and_scoped_to_the_house() -> TestResult {
    let fixture = Fixture::new()?;
    assert_eq!(
        fixture
            .store
            .create_task(spec("task-1")?, &creator()?, at(0))?,
        Creation::Created
    );
    assert_eq!(
        fixture
            .store
            .create_task(spec("task-1")?, &creator()?, at(9))?,
        Creation::AlreadyExists
    );
    assert_eq!(fixture.store.task(&task_id("task-1")?)?.created_at(), at(0));

    let different = spec_with(
        "task-1",
        RetryPolicy::new(1, Duration::from_secs(60))?,
        &[Permission::LaunchWorker],
    )?;
    assert!(matches!(
        fixture.store.create_task(different, &creator()?, at(1)),
        Err(Error::State(StateError::TaskConflict(_)))
    ));

    let dir = tempfile::tempdir()?;
    let foreign = HouseStore::initialize(dir.path(), other_house()?, StoreOptions::default())?;
    assert!(matches!(
        foreign.create_task(spec("task-1")?, &creator()?, at(0)),
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));
    assert!(matches!(
        fixture.store.task(&task_id("missing")?),
        Err(Error::State(StateError::TaskNotFound(_)))
    ));
    Ok(())
}

#[test]
fn concurrent_claims_have_exactly_one_winner() -> TestResult {
    let fixture = Fixture::new()?;
    fixture
        .store
        .create_task(spec("contested")?, &creator()?, at(0))?;
    let claimants = 8;
    let barrier = Arc::new(Barrier::new(claimants));
    let handles: Vec<_> = (0..claimants)
        .map(|index| {
            let store = fixture.reopen().map_err(|error| error.to_string());
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || -> Result<Result<Fence, Error>, String> {
                let store = store?;
                let task = task_id("contested").map_err(|error| error.to_string())?;
                let claimant =
                    scheduled(&format!("tick-{index}")).map_err(|error| error.to_string())?;
                let lease_ttl = ttl(60).map_err(|error| error.to_string())?;
                barrier.wait();
                Ok(store
                    .claim(&task, &claimant, lease_ttl, at(1))
                    .map(|lease| lease.fence()))
            })
        })
        .collect();
    let mut winners = Vec::new();
    for handle in handles {
        match handle.join().map_err(|_| "claimant thread panicked")?? {
            Ok(fence) => winners.push(fence),
            Err(Error::State(StateError::ClaimHeld { .. })) => {}
            Err(other) => return Err(format!("unexpected claim error: {other}").into()),
        }
    }
    assert_eq!(winners.len(), 1);
    let record = fixture.store.task(&task_id("contested")?)?;
    assert!(
        matches!(record.state(), TaskState::Claimed { lease } if Some(&lease.fence()) == winners.first())
    );
    assert_eq!(record.ownership().len(), 1);
    Ok(())
}

#[test]
fn concurrent_writers_do_not_lose_updates() -> TestResult {
    let fixture = Fixture::new()?;
    let tasks = 8;
    for index in 0..tasks {
        fixture
            .store
            .create_task(spec(&format!("task-{index}"))?, &creator()?, at(0))?;
    }
    let barrier = Arc::new(Barrier::new(tasks));
    let handles: Vec<_> = (0..tasks)
        .map(|index| {
            let store = fixture.reopen().map_err(|error| error.to_string());
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || -> Result<(), String> {
                let store = store?;
                let task = task_id(&format!("task-{index}")).map_err(|error| error.to_string())?;
                let claimant =
                    scheduled(&format!("worker-{index}")).map_err(|error| error.to_string())?;
                let lease_ttl = ttl(60).map_err(|error| error.to_string())?;
                barrier.wait();
                store
                    .claim(&task, &claimant, lease_ttl, at(1))
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
        })
        .collect();
    for handle in handles {
        handle.join().map_err(|_| "writer thread panicked")??;
    }
    let fences: std::collections::BTreeSet<_> = fixture
        .store
        .tasks()?
        .iter()
        .filter_map(|record| match record.state() {
            TaskState::Claimed { lease } => Some(lease.fence()),
            TaskState::Open | TaskState::Settled { .. } => None,
        })
        .collect();
    assert_eq!(
        fences.len(),
        tasks,
        "every claim persisted with a distinct fence"
    );
    Ok(())
}

#[test]
fn coordinator_exit_is_uncertain_not_released() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let later = at(61);

    assert!(matches!(
        fixture.store.claim(&task, &scheduled("coordinator-b")?, ttl(60)?, later),
        Err(Error::State(StateError::LeaseExpired { expired_at })) if expired_at == at(60)
    ));
    assert!(matches!(
        fixture.store.start_attempt(&task, fence, later),
        Err(Error::State(StateError::LeaseExpired { .. }))
    ));
    assert!(matches!(
        fixture.store.renew(&task, fence, ttl(60)?, later),
        Err(Error::State(StateError::LeaseExpired { .. }))
    ));
    assert_eq!(
        fixture.store.recovery_queue(later)?,
        vec![RecoveryItem::UncertainTaskOwner {
            task: task.clone(),
            holder: holder("coordinator-a")?,
            expired_at: at(60),
        }]
    );
    assert!(fixture.store.recovery_queue(at(59))?.is_empty());
    Ok(())
}

#[test]
fn stale_owner_is_fenced_after_takeover() -> TestResult {
    let fixture = Fixture::new()?;
    let old = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let grants = grants()?;

    assert!(matches!(
        fixture.store.take_over(&task, &scheduled("coordinator-b")?, ttl(60)?, at(30)),
        Err(Error::State(StateError::LeaseLive { expires_at })) if expires_at == at(60)
    ));
    let lease = fixture
        .store
        .take_over(&task, &scheduled("coordinator-b")?, ttl(60)?, at(61))?;
    assert!(lease.fence() > old);

    let stale = |result: Result<(), Error>| matches!(result, Err(Error::State(StateError::StaleFence { presented })) if presented == old);
    let store = &fixture.store;
    assert!(stale(store.renew(&task, old, ttl(60)?, at(62)).map(|_| ())));
    assert!(stale(store.start_attempt(&task, old, at(62)).map(|_| ())));
    assert!(stale(
        store
            .finish_attempt(
                &task,
                old,
                AttemptNumber::FIRST,
                AttemptOutcome::Succeeded,
                at(62)
            )
            .map(|_| ())
    ));
    assert!(stale(store.relinquish(&task, old, at(62))));
    assert!(stale(store.settle_cancelled(&task, old, at(62))));
    assert!(stale(
        store
            .record_evidence(&task, old, evidence('c', "ci-1")?, at(62))
            .map(|_| ())
    ));
    assert!(stale(
        store
            .consume_message(&task, old, &ExternalRef::new("msg-1")?, at(62))
            .map(|_| ())
    ));
    assert!(stale(
        store
            .begin_effect(
                plan(&task, old, "launch", launch()?)?,
                &grants,
                &common::refusing()?,
                at(62)
            )
            .map(|_| ())
    ));

    let record = store.task(&task)?;
    assert!(
        matches!(record.attempts(), [first] if first.state() == AttemptState::Interrupted { at: at(61) })
    );
    assert!(matches!(
        record.ownership().last(),
        Some(OwnershipEvent::TakenOver { previous, .. }) if *previous == old
    ));
    assert_eq!(
        store.start_attempt(&task, lease.fence(), at(62))?,
        AttemptStart::Started(AttemptNumber::new(2).ok_or("attempt 2")?)
    );
    Ok(())
}

#[test]
fn coordinator_relinquish_then_adopt() -> TestResult {
    let fixture = Fixture::new()?;
    let old = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let message = ExternalRef::new("worker-question-7")?;
    assert_eq!(
        fixture.store.consume_message(&task, old, &message, at(1))?,
        Consumption::New
    );

    fixture.store.relinquish(&task, old, at(2))?;
    assert_eq!(fixture.store.task(&task)?.state(), &TaskState::Open);
    assert!(fixture.store.recovery_queue(at(2))?.is_empty());

    let adopted = fixture
        .store
        .claim(&task, &scheduled("coordinator-b")?, ttl(60)?, at(3))?;
    assert!(adopted.fence() > old);
    assert_eq!(
        fixture
            .store
            .consume_message(&task, adopted.fence(), &message, at(4))?,
        Consumption::Duplicate,
        "a message consumed before the transfer is not delivered again"
    );
    let record = fixture.store.task(&task)?;
    assert!(matches!(
        record.ownership(),
        [
            OwnershipEvent::Claimed { .. },
            OwnershipEvent::Relinquished { fence, .. },
            OwnershipEvent::Adopted { previous, fence: adopted_fence, .. }
        ] if *fence == old && *previous == old && *adopted_fence == adopted.fence()
    ));
    assert!(
        matches!(record.attempts(), [first] if first.state() == AttemptState::Interrupted { at: at(2) })
    );
    Ok(())
}

#[test]
fn repeated_events_are_idempotent_and_contradictions_rejected() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let store = &fixture.store;

    assert_eq!(
        store.start_attempt(&task, fence, at(1))?,
        AttemptStart::AlreadyRunning(AttemptNumber::FIRST)
    );

    let EffectStart::Execute(intent) = store.begin_effect(
        plan(&task, fence, "launch", launch()?)?,
        &grants()?,
        &common::refusing()?,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    let seq = intent.seq();
    let applied = EffectOutcome::Applied(receipt("request-1")?);
    store.record_effect_outcome(&task, fence, seq, applied.clone(), at(2))?;
    let repeat = store.record_effect_outcome(&task, fence, seq, applied, at(3))?;
    assert!(
        matches!(repeat.state(), EffectState::Applied { at: recorded, .. } if *recorded == at(2))
    );
    store.record_effect_outcome(
        &task,
        fence,
        seq,
        EffectOutcome::Uncertain(UncertainReason::Timeout),
        at(3),
    )?;
    for contradiction in [
        EffectOutcome::Applied(receipt("request-2")?),
        EffectOutcome::NotApplied(NotAppliedReason::Rejected),
    ] {
        assert!(matches!(
            store.record_effect_outcome(&task, fence, seq, contradiction, at(4)),
            Err(Error::State(StateError::ConflictingOutcome(conflict))) if conflict == seq
        ));
    }
    assert!(matches!(
        store.begin_effect(plan(&task, fence, "launch", launch()?)?, &grants()?, &common::refusing()?, at(5))?,
        EffectStart::Resolved(record) if record.seq() == seq
    ));

    let failed = AttemptOutcome::Failed(FailureClass::Retryable);
    let first = store.finish_attempt(&task, fence, AttemptNumber::FIRST, failed, at(6))?;
    assert_eq!(first, Disposition::RetryAvailable { remaining: 2 });
    assert_eq!(
        store.finish_attempt(&task, fence, AttemptNumber::FIRST, failed, at(7))?,
        first
    );
    assert!(matches!(
        store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(7)
        ),
        Err(Error::State(StateError::ConflictingAttemptOutcome))
    ));

    store.start_attempt(&task, fence, at(8))?;
    let done = store.finish_attempt(&task, fence, SECOND, AttemptOutcome::Succeeded, at(9))?;
    assert_eq!(
        store.finish_attempt(&task, fence, SECOND, AttemptOutcome::Succeeded, at(10))?,
        done
    );
    assert_eq!(
        store.request_cancel(&task, &holder("operator")?, at(11))?,
        CancelStatus::AlreadySettled(Settlement::Succeeded)
    );
    Ok(())
}

#[test]
fn retry_budget_bounds_attempt_count_and_elapsed_time() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let retryable = AttemptOutcome::Failed(FailureClass::Retryable);
    let budget = RetryPolicy::new(2, Duration::from_secs(100))?;

    let counted = task_id("counted")?;
    store.create_task(
        spec_with("counted", budget, &[Permission::LaunchWorker])?,
        &creator()?,
        at(0),
    )?;
    let fence = store
        .claim(&counted, &scheduled("a")?, ttl(600)?, at(0))?
        .fence();
    store.start_attempt(&counted, fence, at(0))?;
    assert_eq!(
        store.finish_attempt(&counted, fence, AttemptNumber::FIRST, retryable, at(1))?,
        Disposition::RetryAvailable { remaining: 1 }
    );
    store.start_attempt(&counted, fence, at(2))?;
    assert_eq!(
        store.finish_attempt(&counted, fence, SECOND, retryable, at(3))?,
        Disposition::Settled(Settlement::Exhausted)
    );

    let timed = task_id("timed")?;
    store.create_task(
        spec_with("timed", budget, &[Permission::LaunchWorker])?,
        &creator()?,
        at(0),
    )?;
    let fence = store
        .claim(&timed, &scheduled("a")?, ttl(600)?, at(0))?
        .fence();
    store.start_attempt(&timed, fence, at(0))?;
    assert_eq!(
        store.finish_attempt(&timed, fence, AttemptNumber::FIRST, retryable, at(10))?,
        Disposition::RetryAvailable { remaining: 1 }
    );
    assert_eq!(
        store.start_attempt(&timed, fence, at(101))?,
        AttemptStart::Exhausted
    );
    assert!(matches!(
        store.task(&timed)?.state(),
        TaskState::Settled {
            settlement: Settlement::Exhausted,
            ..
        }
    ));

    let permanent = task_id("permanent")?;
    store.create_task(
        spec_with("permanent", budget, &[Permission::LaunchWorker])?,
        &creator()?,
        at(0),
    )?;
    let fence = store
        .claim(&permanent, &scheduled("a")?, ttl(600)?, at(0))?
        .fence();
    store.start_attempt(&permanent, fence, at(0))?;
    assert_eq!(
        store.finish_attempt(
            &permanent,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Failed(FailureClass::Permanent),
            at(1)
        )?,
        Disposition::Settled(Settlement::Failed)
    );
    Ok(())
}

#[test]
fn cancellation_stops_new_work_and_needs_resolved_effects() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;

    let open = task_id("open")?;
    store.create_task(spec("open")?, &creator()?, at(0))?;
    assert_eq!(
        store.request_cancel(&open, &holder("operator")?, at(1))?,
        CancelStatus::Settled
    );

    let fence = claimed_attempt(&fixture, "busy", at(0))?;
    let busy = task_id("busy")?;
    let EffectStart::Execute(intent) = store.begin_effect(
        plan(&busy, fence, "launch", launch()?)?,
        &grants()?,
        &common::refusing()?,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    assert_eq!(
        store.request_cancel(&busy, &holder("operator")?, at(2))?,
        CancelStatus::Pending
    );
    assert!(matches!(
        store.begin_effect(
            plan(&busy, fence, "message", launch()?)?,
            &grants()?,
            &common::refusing()?,
            at(3)
        ),
        Err(Error::State(StateError::CancelRequested))
    ));
    assert!(matches!(
        store.settle_cancelled(&busy, fence, at(3)),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    store.record_effect_outcome(
        &busy,
        fence,
        intent.seq(),
        EffectOutcome::Applied(receipt("request-1")?),
        at(4),
    )?;
    store.settle_cancelled(&busy, fence, at(5))?;
    store.settle_cancelled(&busy, fence, at(6))?;
    let record = store.task(&busy)?;
    assert!(
        matches!(record.state(), TaskState::Settled { settlement: Settlement::Cancelled, at: settled } if *settled == at(5))
    );
    assert!(
        matches!(record.attempts(), [first] if first.state() == AttemptState::Cancelled { at: at(5) })
    );
    assert_eq!(
        record
            .cancel_request()
            .map(|request| request.requested_by().as_str()),
        Some("operator")
    );
    Ok(())
}

#[test]
fn new_evidence_subject_invalidates_earlier_decisions() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "gate", at(0))?;
    let task = task_id("gate")?;
    let store = &fixture.store;

    let first = store.record_evidence(&task, fence, evidence('a', "ci-1")?, at(1))?;
    assert_eq!(
        store.record_evidence(&task, fence, evidence('a', "review-1")?, at(1))?,
        first
    );
    assert_eq!(
        store.record_evidence(&task, fence, evidence('a', "ci-1")?, at(1))?,
        first
    );
    assert_eq!(store.task(&task)?.evidence().items().len(), 2);

    let moved = store.record_evidence(&task, fence, evidence('b', "ci-2")?, at(2))?;
    assert!(moved > first);
    let record = store.task(&task)?;
    assert_eq!(
        record.evidence().items().len(),
        1,
        "evidence for the old head is dropped"
    );
    assert_eq!(record.evidence().subject(), Some(&commit('b')?));

    let mut stale = plan(&task, fence, "launch", launch()?)?;
    stale.decided_at = first;
    assert!(matches!(
        store.begin_effect(stale.clone(), &grants()?, &common::refusing()?, at(3)),
        Err(Error::State(StateError::StaleDecision { decided, current })) if decided == first && current == moved
    ));
    stale.decided_at = moved;
    assert!(matches!(
        store.begin_effect(stale, &grants()?, &common::refusing()?, at(3))?,
        EffectStart::Execute(_)
    ));
    assert_eq!(EvidenceRevision::INITIAL.get(), 0);
    Ok(())
}

#[test]
fn evidence_per_revision_is_bounded() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "gate", at(0))?;
    let task = task_id("gate")?;
    for index in 0..MAX_EVIDENCE_PER_REVISION {
        fixture.store.record_evidence(
            &task,
            fence,
            evidence('a', &format!("check-{index}"))?,
            at(1),
        )?;
    }
    assert!(matches!(
        fixture
            .store
            .record_evidence(&task, fence, evidence('a', "one-more")?, at(1)),
        Err(Error::State(StateError::CapacityExceeded { .. }))
    ));
    Ok(())
}

#[test]
fn effects_are_checked_against_current_house_grants() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let store = &fixture.store;
    let launch_plan = || plan(&task, fence, "launch", launch().map_err(|e| e.to_string())?);

    let revoked = grants_for(house()?, &[Permission::MessageWorker])?;
    assert!(matches!(
        store.begin_effect(launch_plan()?, &revoked, &common::refusing()?, at(1)),
        Err(Error::Contract(ContractError::AuthorityExpansion { .. }))
    ));
    let foreign = grants_for(other_house()?, &common::WORKER_PERMISSIONS)?;
    assert!(matches!(
        store.begin_effect(launch_plan()?, &foreign, &common::refusing()?, at(1)),
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));

    let narrow = task_id("narrow")?;
    store.create_task(
        spec_with(
            "narrow",
            RetryPolicy::new(1, Duration::from_secs(60))?,
            &[Permission::MessageWorker],
        )?,
        &creator()?,
        at(0),
    )?;
    let narrow_fence = store
        .claim(&narrow, &scheduled("a")?, ttl(60)?, at(0))?
        .fence();
    store.start_attempt(&narrow, narrow_fence, at(0))?;
    assert!(matches!(
        store.begin_effect(
            plan(&narrow, narrow_fence, "launch", launch()?)?,
            &grants()?,
            &common::refusing()?,
            at(1)
        ),
        Err(Error::Contract(ContractError::PermissionDenied {
            permission: Permission::LaunchWorker
        }))
    ));
    assert!(
        store.task(&task)?.effects().is_empty(),
        "refused effects persist no intent"
    );
    Ok(())
}

#[test]
fn unresolved_effect_blocks_new_work_until_resolved() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let store = &fixture.store;
    let EffectStart::Execute(intent) = store.begin_effect(
        plan(&task, fence, "launch", launch()?)?,
        &grants()?,
        &common::refusing()?,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    store.record_effect_outcome(
        &task,
        fence,
        intent.seq(),
        EffectOutcome::Uncertain(UncertainReason::Timeout),
        at(2),
    )?;

    assert!(matches!(
        store.begin_effect(plan(&task, fence, "launch", launch()?)?, &grants()?, &common::refusing()?, at(3)),
        Err(Error::State(StateError::UnsafeRetry(seq))) if seq == intent.seq()
    ));
    assert!(matches!(
        store.begin_effect(
            plan(&task, fence, "other", launch()?)?,
            &grants()?,
            &common::refusing()?,
            at(3)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert!(matches!(
        store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(3)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    // An idempotent backend must reconcile before the key is resubmitted.
    assert!(matches!(
        store.begin_effect(plan(&task, fence, "launch", launch()?)?, &grants()?, &common::idempotent()?, at(3))?,
        EffectStart::ReconcileFirst(record) if record.seq() == intent.seq() && record.submissions() == 1
    ));
    store.record_effect_outcome(
        &task,
        fence,
        intent.seq(),
        EffectOutcome::Uncertain(UncertainReason::LookupInconclusive),
        at(3),
    )?;
    assert!(matches!(
        store.begin_effect(plan(&task, fence, "launch", launch()?)?, &grants()?, &common::idempotent()?, at(3))?,
        EffectStart::Execute(record) if record.request().key() == intent.request().key() && record.submissions() == 2
    ));

    store.record_effect_outcome(
        &task,
        fence,
        intent.seq(),
        EffectOutcome::Applied(receipt("request-1")?),
        at(4),
    )?;
    assert_eq!(
        store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(5)
        )?,
        Disposition::Settled(Settlement::Succeeded)
    );
    Ok(())
}

#[test]
fn pickup_duplicate_tick_single_consumer() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let pickup = ConsumerId::new("pickup-origin89")?;

    let first = store.acquire_consumer(&pickup, &scheduled("tick-1")?, ttl(60)?, at(0))?;
    assert!(matches!(
        store.acquire_consumer(&pickup, &scheduled("tick-2")?, ttl(60)?, at(1)),
        Err(Error::State(StateError::ClaimHeld { holder: current, .. })) if current.as_str() == "tick-1"
    ));
    let renewed = store.renew_consumer(&pickup, first.fence(), ttl(60)?, at(30))?;
    assert_eq!(renewed.expires_at(), at(90));

    assert!(matches!(
        store.acquire_consumer(&pickup, &scheduled("tick-3")?, ttl(60)?, at(91)),
        Err(Error::State(StateError::LeaseExpired { .. }))
    ));
    assert!(
        store
            .recovery_queue(at(91))?
            .iter()
            .any(|item| matches!(item, RecoveryItem::UncertainConsumer { .. }))
    );
    let second = store.take_over_consumer(&pickup, &scheduled("tick-3")?, ttl(60)?, at(91))?;
    assert!(second.fence() > first.fence());
    assert!(matches!(
        store.release_consumer(&pickup, first.fence(), at(91)),
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    assert!(matches!(
        store.renew_consumer(&pickup, first.fence(), ttl(60)?, at(92)),
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    store.release_consumer(&pickup, second.fence(), at(92))?;
    store.release_consumer(&pickup, second.fence(), at(93))?;
    let record = store.consumer(&pickup)?.ok_or("consumer record kept")?;
    assert_eq!(record.state(), &ConsumerState::Idle);
    assert!(matches!(
        store.renew_consumer(&pickup, second.fence(), ttl(60)?, at(93)),
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    assert!(matches!(
        store.renew_consumer(
            &ConsumerId::new("unknown")?,
            second.fence(),
            ttl(60)?,
            at(93)
        ),
        Err(Error::State(StateError::ConsumerNotFound(_)))
    ));
    let events: Vec<_> = record.history().cloned().collect();
    assert!(matches!(
        events.as_slice(),
        [
            ConsumerEvent::Acquired { fence: a, .. },
            ConsumerEvent::TakenOver { previous, fence: b, .. },
            ConsumerEvent::Released { fence: c, .. },
        ] if *a == first.fence() && *previous == first.fence() && *b == second.fence() && *c == second.fence()
    ));
    Ok(())
}

#[test]
fn storage_inside_a_git_checkout_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join(".git"))?;
    assert!(matches!(
        HouseStore::initialize(
            dir.path().join("state").join("origin89"),
            house()?,
            StoreOptions::default()
        ),
        Err(Error::State(StateError::StorageInsideRepository))
    ));
    Ok(())
}

#[test]
fn lock_wait_is_bounded() -> TestResult {
    let dir = tempfile::tempdir()?;
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(50),
        ..StoreOptions::default()
    };
    let store = HouseStore::initialize(dir.path(), house()?, options)?;
    let blocker = fs::File::open(dir.path().join("state.lock"))?;
    blocker.lock()?;
    assert!(matches!(
        store.create_task(spec("task-1")?, &creator()?, at(0)),
        Err(Error::State(StateError::LockTimeout { waited_ms })) if waited_ms >= 50
    ));
    blocker.unlock()?;
    assert_eq!(
        store.create_task(spec("task-1")?, &creator()?, at(0))?,
        Creation::Created
    );
    Ok(())
}

type Expectation = fn(&Error) -> bool;

fn open_error(fixture: &Fixture) -> Option<Error> {
    fixture
        .reopen()
        .err()
        .and_then(|error| error.downcast::<Error>().ok())
        .map(|error| *error)
}

#[test]
fn invalid_persisted_data_is_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    claimed_attempt(&fixture, "task-1", at(0))?;
    let path = fixture.state_path();
    let valid = fs::read_to_string(&path)?;

    let cases: Vec<(String, Expectation)> = vec![
        ("{ not json".to_owned(), |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { line: 1, .. }))
            )
        }),
        (valid.replace("\"schema\": 1", "\"schema\": 2"), |error| {
            matches!(
                error,
                Error::State(StateError::UnsupportedSchema { found: 2 })
            )
        }),
        (
            valid.replace("\"house\": \"origin89\"", "\"house\": \"crabnebula\""),
            |error| matches!(error, Error::Contract(ContractError::CrossHouse { .. })),
        ),
        (
            valid.replace(
                "\"holder\": \"coordinator-a\"",
                "\"holder\": \"bad holder\"",
            ),
            |error| {
                matches!(
                    error,
                    Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
                )
            },
        ),
        (
            valid.replace("\"nextFence\": 2", "\"nextFence\": 1"),
            |error| {
                matches!(
                    error,
                    Error::State(StateError::CorruptState(Corruption::FenceAhead))
                )
            },
        ),
        (valid.replace("\"number\": 1", "\"number\": 2"), |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::AttemptSequence))
            )
        }),
        (
            valid.replacen("\"task-1\": {", "\"task-2\": {", 1),
            |error| {
                matches!(
                    error,
                    Error::State(StateError::CorruptState(Corruption::TaskKey))
                )
            },
        ),
    ];
    for (content, expected) in cases {
        assert_ne!(content, valid, "fixture edit must change the file");
        fs::write(&path, &content)?;
        let error = open_error(&fixture).ok_or("corrupt state was accepted")?;
        assert!(expected(&error), "unexpected error: {error:?}");
        assert_eq!(
            fs::read_to_string(&path)?,
            content,
            "rejected state must not be rewritten"
        );
    }
    fs::write(&path, &valid)?;
    assert!(fixture.reopen().is_ok());
    Ok(())
}

#[test]
fn oversized_or_missing_state_is_an_error() -> TestResult {
    let fixture = Fixture::new()?;
    fixture
        .store
        .create_task(spec("task-1")?, &creator()?, at(0))?;
    let tiny = StoreOptions {
        max_state_bytes: 64,
        ..StoreOptions::default()
    };
    assert!(matches!(
        HouseStore::open(fixture.dir.path().join("house"), house()?, tiny),
        Err(Error::State(StateError::StateTooLarge { limit_bytes: 64 }))
    ));

    fs::remove_file(fixture.state_path())?;
    assert!(matches!(
        fixture.store.task(&task_id("task-1")?),
        Err(Error::State(StateError::StateMissing))
    ));
    assert!(matches!(
        fixture
            .store
            .create_task(spec("task-2")?, &creator()?, at(0)),
        Err(Error::State(StateError::StateMissing))
    ));
    Ok(())
}

#[test]
fn interrupted_write_leaves_the_committed_state() -> TestResult {
    let fixture = Fixture::new()?;
    fixture
        .store
        .create_task(spec("task-1")?, &creator()?, at(0))?;
    fs::write(
        fixture.dir.path().join("house").join("state.json.tmp"),
        "{ torn",
    )?;
    let reopened = fixture.reopen()?;
    assert_eq!(reopened.tasks()?.len(), 1);
    reopened.create_task(spec("task-2")?, &creator()?, at(1))?;
    assert_eq!(fixture.store.tasks()?.len(), 2);
    Ok(())
}

#[test]
fn a_store_directory_belongs_to_one_house() -> TestResult {
    let fixture = Fixture::new()?;
    let foreign = HouseStore::open(
        fixture.dir.path().join("house"),
        HouseId::new(common::OTHER_HOUSE)?,
        StoreOptions::default(),
    );
    assert!(matches!(
        foreign,
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn runtime_state_is_private_to_the_owner() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new()?;
    fixture
        .store
        .create_task(spec("task-1")?, &creator()?, at(0))?;
    let mode = |path: std::path::PathBuf| -> TestResult<u32> {
        Ok(fs::metadata(path)?.permissions().mode() & 0o777)
    };
    assert_eq!(mode(fixture.dir.path().join("house"))?, 0o700);
    assert_eq!(mode(fixture.state_path())?, 0o600);
    assert_eq!(
        mode(fixture.dir.path().join("house").join("state.lock"))?,
        0o600
    );
    Ok(())
}

#[test]
fn reopening_an_established_store_without_its_snapshot_fails_closed() -> TestResult {
    let fixture = Fixture::new()?;
    claimed_attempt(&fixture, "task-1", at(0))?;
    fs::remove_file(fixture.state_path())?;
    assert!(matches!(
        open_error(&fixture),
        Some(Error::State(StateError::StateMissing))
    ));
    assert!(
        !fixture.state_path().exists(),
        "no empty snapshot was written"
    );
    Ok(())
}

#[test]
fn initialize_and_open_are_separate() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("house");
    assert!(matches!(
        HouseStore::open(&path, house()?, StoreOptions::default()),
        Err(Error::State(StateError::NotInitialized))
    ));
    assert!(!path.exists(), "open never creates a store");
    fs::create_dir(&path)?;
    assert!(matches!(
        HouseStore::open(&path, house()?, StoreOptions::default()),
        Err(Error::State(StateError::NotInitialized))
    ));
    let store = HouseStore::initialize(&path, house()?, StoreOptions::default())?;
    store.create_task(spec("task-1")?, &creator()?, at(0))?;
    assert!(matches!(
        HouseStore::initialize(&path, house()?, StoreOptions::default()),
        Err(Error::State(StateError::AlreadyInitialized))
    ));
    fs::remove_file(path.join("state.json"))?;
    assert!(matches!(
        HouseStore::initialize(&path, house()?, StoreOptions::default()),
        Err(Error::State(StateError::AlreadyInitialized))
    ));
    assert!(!path.join("state.json").exists());
    Ok(())
}

#[test]
fn a_snapshot_without_its_marker_or_from_another_store_is_rejected() -> TestResult {
    let first = Fixture::new()?;
    first
        .store
        .create_task(spec("task-1")?, &creator()?, at(0))?;
    let second = Fixture::new()?;
    let foreign_snapshot = fs::read(second.state_path())?;

    fs::copy(second.state_path(), first.state_path())?;
    assert!(matches!(
        open_error(&first),
        Some(Error::State(StateError::CorruptState(
            Corruption::StoreIdentity
        )))
    ));
    assert!(matches!(
        first.store.tasks(),
        Err(Error::State(StateError::CorruptState(
            Corruption::StoreIdentity
        )))
    ));
    assert_eq!(fs::read(first.state_path())?, foreign_snapshot);

    fs::remove_file(second.dir.path().join("house").join("store.json"))?;
    assert!(matches!(
        open_error(&second),
        Some(Error::State(StateError::CorruptState(Corruption::Marker)))
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn redirected_store_paths_are_refused_without_touching_the_target() -> TestResult {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new()?;
    fixture
        .store
        .create_task(spec("task-1")?, &creator()?, at(0))?;
    let outside = tempfile::tempdir()?;
    let target = outside.path().join("elsewhere.json");
    fs::write(&target, "untouched")?;

    let temp = fixture.dir.path().join("house").join("state.json.tmp");
    symlink(&target, &temp)?;
    assert!(matches!(
        fixture
            .store
            .create_task(spec("task-2")?, &creator()?, at(1)),
        Err(Error::State(StateError::RedirectedPath))
    ));
    assert!(matches!(
        fixture.store.tasks(),
        Err(Error::State(StateError::RedirectedPath))
    ));
    assert_eq!(fs::read_to_string(&target)?, "untouched");
    fs::remove_file(&temp)?;
    assert_eq!(fixture.store.tasks()?.len(), 1);

    let linked = outside.path().join("linked-house");
    symlink(fixture.dir.path().join("house"), &linked)?;
    assert!(matches!(
        HouseStore::open(&linked, house()?, StoreOptions::default()),
        Err(Error::State(StateError::RedirectedPath))
    ));
    assert!(matches!(
        HouseStore::initialize(
            outside.path().join("linked-house"),
            house()?,
            StoreOptions::default()
        ),
        Err(Error::State(StateError::RedirectedPath))
    ));
    Ok(())
}

#[test]
fn duplicate_persisted_task_keys_are_rejected() -> TestResult {
    let fixture = Fixture::new()?;
    claimed_attempt(&fixture, "task-1", at(0))?;
    fixture
        .store
        .create_task(spec("task-2")?, &creator()?, at(0))?;
    let path = fixture.state_path();
    let valid = fs::read_to_string(&path)?;
    // A second, open record for task-1 after the owned one.
    let duplicated = valid
        .replace("\"task-2\": {", "\"task-1\": {")
        .replace("\"id\": \"task-2\"", "\"id\": \"task-1\"");
    assert_ne!(duplicated, valid);
    fs::write(&path, &duplicated)?;
    assert!(matches!(
        open_error(&fixture),
        Some(Error::State(StateError::CorruptState(
            Corruption::Syntax { .. }
        )))
    ));
    assert_eq!(
        fs::read_to_string(&path)?,
        duplicated,
        "rejected state is not rewritten"
    );
    Ok(())
}

#[test]
fn ownership_history_must_match_the_current_lease() -> TestResult {
    let fixture = Fixture::new()?;
    claimed_attempt(&fixture, "task-1", at(0))?;
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut erased = valid.clone();
    erased["tasks"]["task-1"]["ownership"] = serde_json::json!([]);
    let mut swapped_holder = valid.clone();
    swapped_holder["tasks"]["task-1"]["state"]["lease"]["holder"] = "coordinator-z".into();
    let mut released_then_claimed = valid.clone();
    released_then_claimed["tasks"]["task-1"]["ownership"] = serde_json::json!([
        {"type": "claimed", "holder": "coordinator-a", "trigger": "scheduled", "fence": 1, "at": 0},
        {"type": "released", "trigger": "scheduled", "fence": 1, "at": 0},
        {"type": "claimed", "holder": "coordinator-a", "trigger": "scheduled", "fence": 1, "at": 0},
    ]);
    let mut switched_trigger = valid.clone();
    switched_trigger["tasks"]["task-1"]["state"]["lease"]["trigger"] = "interactive".into();
    let mut adopted_without_relinquish = valid.clone();
    adopted_without_relinquish["tasks"]["task-1"]["ownership"] = serde_json::json!([
        {"type": "adopted", "previous": 0, "holder": "coordinator-a", "trigger": "scheduled", "fence": 1, "at": 0},
    ]);
    for corrupt in [
        erased,
        swapped_holder,
        released_then_claimed,
        adopted_without_relinquish,
        switched_trigger,
    ] {
        fs::write(&path, serde_json::to_vec_pretty(&corrupt)?)?;
        assert!(matches!(
            open_error(&fixture),
            Some(Error::State(StateError::CorruptState(
                Corruption::Ownership
            )))
        ));
    }
    fs::write(&path, serde_json::to_vec_pretty(&valid)?)?;
    assert!(fixture.reopen().is_ok());
    Ok(())
}

#[test]
fn a_replayed_finish_from_an_earlier_attempt_does_not_finish_the_current_one() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let store = &fixture.store;
    let retryable = AttemptOutcome::Failed(FailureClass::Retryable);
    store.finish_attempt(&task, fence, AttemptNumber::FIRST, retryable, at(1))?;
    store.start_attempt(&task, fence, at(2))?;
    // Attempt 1's duplicate failure report arrives late: it replays attempt
    // 1's result and leaves attempt 2 running.
    assert_eq!(
        store.finish_attempt(&task, fence, AttemptNumber::FIRST, retryable, at(3))?,
        Disposition::RetryAvailable { remaining: 2 }
    );
    assert!(matches!(
        store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(3)
        ),
        Err(Error::State(StateError::ConflictingAttemptOutcome))
    ));
    let record = store.task(&task)?;
    assert!(
        matches!(record.attempts(), [_, second] if second.state() == AttemptState::Running),
        "attempt 2 must still be running: {:?}",
        record.attempts()
    );
    let third = AttemptNumber::new(3).ok_or("attempt 3")?;
    assert!(matches!(
        store.finish_attempt(&task, fence, third, retryable, at(4)),
        Err(Error::State(StateError::AttemptNotFound(number))) if number == third
    ));
    assert_eq!(
        store.finish_attempt(&task, fence, SECOND, AttemptOutcome::Succeeded, at(5))?,
        Disposition::Settled(Settlement::Succeeded)
    );
    Ok(())
}

#[test]
fn a_late_response_from_an_older_submission_cannot_clear_a_newer_one() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let store = &fixture.store;
    let backend = common::idempotent()?;
    let EffectStart::Execute(first) = store.begin_effect(
        plan(&task, fence, "launch", launch()?)?,
        &grants()?,
        &backend,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    // Submission 1 is still in flight; the owner reconciles and resubmits.
    assert!(matches!(
        store.begin_effect(
            plan(&task, fence, "launch", launch()?)?,
            &grants()?,
            &backend,
            at(2)
        )?,
        EffectStart::ReconcileFirst(_)
    ));
    store.record_effect_outcome(
        &task,
        fence,
        first.seq(),
        EffectOutcome::Uncertain(UncertainReason::LookupInconclusive),
        at(2),
    )?;
    let EffectStart::Execute(second) = store.begin_effect(
        plan(&task, fence, "launch", launch()?)?,
        &grants()?,
        &backend,
        at(3),
    )?
    else {
        return Err("expected a resubmission".into());
    };
    assert_eq!(second.submissions(), 2);

    let late = store.record_submission_outcome(
        &task,
        fence,
        first.seq(),
        first.submissions(),
        EffectOutcome::NotApplied(NotAppliedReason::Rejected),
        at(4),
    )?;
    assert!(matches!(
        late.state(),
        EffectState::Uncertain {
            reason: UncertainReason::LookupInconclusive,
            ..
        }
    ));
    let current = store.record_submission_outcome(
        &task,
        fence,
        second.seq(),
        second.submissions(),
        EffectOutcome::Applied(receipt("request-2")?),
        at(5),
    )?;
    assert!(matches!(current.state(), EffectState::Applied { .. }));
    Ok(())
}

#[test]
fn a_handed_over_effect_keeps_blocking_new_work_and_success() -> TestResult {
    let fixture = Fixture::new()?;
    let fence = claimed_attempt(&fixture, "task-1", at(0))?;
    let task = task_id("task-1")?;
    let store = &fixture.store;
    let EffectStart::Execute(intent) = store.begin_effect(
        plan(&task, fence, "launch", launch()?)?,
        &grants()?,
        &common::refusing()?,
        at(1),
    )?
    else {
        return Err("expected a new effect".into());
    };
    store.record_effect_outcome(
        &task,
        fence,
        intent.seq(),
        EffectOutcome::Unresolvable,
        at(2),
    )?;
    assert!(matches!(
        store.begin_effect(
            plan(&task, fence, "relaunch", launch()?)?,
            &grants()?,
            &common::refusing()?,
            at(3)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    assert!(matches!(
        store.finish_attempt(
            &task,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(3)
        ),
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));

    let decision = |action, revision| -> TestResult<RiskDecision> {
        Ok(RiskDecision {
            effect: intent.request().key().clone(),
            decided_by: holder("operator")?,
            revision,
            action,
        })
    };
    let other = RiskDecision {
        effect: kitchen::contracts::IdempotencyKey::from_ref(ExternalRef::new("another-key")?),
        ..decision(RiskAction::ContinueWork, EvidenceRevision::INITIAL)?
    };
    assert!(matches!(
        store.accept_risk(&task, fence, intent.seq(), other, at(4)),
        Err(Error::State(StateError::DecisionScope(seq))) if seq == intent.seq()
    ));
    let moved = store.record_evidence(&task, fence, evidence('c', "ci-1")?, at(4))?;
    assert!(matches!(
        store.accept_risk(
            &task,
            fence,
            intent.seq(),
            decision(RiskAction::ContinueWork, EvidenceRevision::INITIAL)?,
            at(4)
        ),
        Err(Error::State(StateError::StaleDecision { .. }))
    ));
    let proceed = decision(RiskAction::ContinueWork, moved)?;
    store.accept_risk(&task, fence, intent.seq(), proceed.clone(), at(5))?;
    assert_eq!(
        store
            .accept_risk(&task, fence, intent.seq(), proceed, at(6))?
            .state(),
        &EffectState::Waived {
            decision: decision(RiskAction::ContinueWork, moved)?,
            at: at(5)
        },
        "repeating the decision changes nothing"
    );
    // The explicit decision, not the owner's report, allows new work.
    let mut relaunch = plan(&task, fence, "relaunch", launch()?)?;
    relaunch.decided_at = moved;
    assert!(matches!(
        store.begin_effect(relaunch, &grants()?, &common::refusing()?, at(7))?,
        EffectStart::Execute(_)
    ));
    // Late positive evidence still replaces the decision.
    let applied = store.record_effect_outcome(
        &task,
        fence,
        intent.seq(),
        EffectOutcome::Applied(receipt("request-1")?),
        at(8),
    )?;
    assert!(matches!(applied.state(), EffectState::Applied { .. }));
    Ok(())
}

#[test]
fn coordinator_transfer_is_a_relinquish_adopt_pair_not_an_expiry() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let run = ConsumerId::new("coordinator-origin89")?;
    let first = store.acquire_consumer(&run, &scheduled("session-a")?, ttl(60)?, at(0))?;
    store.relinquish_consumer(&run, first.fence(), at(10))?;
    assert!(matches!(
        store.renew_consumer(&run, first.fence(), ttl(60)?, at(11)),
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    assert_eq!(
        store.recovery_queue(at(11))?,
        vec![RecoveryItem::AwaitingAdoption {
            consumer: run.clone(),
            holder: holder("session-a")?,
            since: at(10),
        }]
    );
    let adopted = store.acquire_consumer(&run, &scheduled("session-b")?, ttl(60)?, at(12))?;
    assert!(adopted.fence() > first.fence());
    assert!(store.recovery_queue(at(12))?.is_empty());
    let record = store.consumer(&run)?.ok_or("consumer record")?;
    assert_eq!(record.lease(), Some(&adopted));
    assert!(matches!(
        record.history().collect::<Vec<_>>().as_slice(),
        [
            ConsumerEvent::Acquired { .. },
            ConsumerEvent::Relinquished { fence, .. },
            ConsumerEvent::Adopted { previous, .. },
        ] if *fence == first.fence() && *previous == first.fence()
    ));

    // A quiet holder is not a relinquish: expiry needs an explicit takeover.
    assert!(matches!(
        store.acquire_consumer(&run, &scheduled("session-c")?, ttl(60)?, at(73)),
        Err(Error::State(StateError::LeaseExpired { .. }))
    ));
    store.take_over_consumer(&run, &scheduled("session-c")?, ttl(60)?, at(73))?;
    let record = store.consumer(&run)?.ok_or("consumer record")?;
    assert!(matches!(
        record.history().last(),
        Some(ConsumerEvent::TakenOver { previous, .. }) if *previous == adopted.fence()
    ));
    Ok(())
}

#[test]
fn consumer_history_is_bounded_and_validated() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let pickup = ConsumerId::new("pickup-origin89")?;
    for tick in 0..40 {
        let lease = store.acquire_consumer(&pickup, &scheduled("tick")?, ttl(60)?, at(tick))?;
        store.release_consumer(&pickup, lease.fence(), at(tick))?;
    }
    let record = store.consumer(&pickup)?.ok_or("consumer record")?;
    assert_eq!(
        record.history().count(),
        kitchen::state::MAX_CONSUMER_HISTORY
    );
    assert!(matches!(
        record.history().last(),
        Some(ConsumerEvent::Released { .. })
    ));

    let path = fixture.state_path();
    let mut state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    state["consumers"]["pickup-origin89"]["state"] = serde_json::json!({
        "type": "held",
        "lease": {"holder": "tick", "trigger": "scheduled", "fence": 1, "acquiredAt": 0, "expiresAt": 60000}
    });
    fs::write(&path, serde_json::to_vec_pretty(&state)?)?;
    assert!(matches!(
        open_error(&fixture),
        Some(Error::State(StateError::CorruptState(
            Corruption::Ownership
        )))
    ));
    Ok(())
}
