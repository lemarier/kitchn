//! Owner cancellation with a fake backend; no live worker is contacted.

mod common;
mod workflows_support;

use common::{TestResult, holder, plan, ttl};
use kitchen::{
    TaskId,
    contracts::{
        Capability, CapabilitySet, Effect, ExternalRef, Lookup, Operation, Receipt, ResourceKind,
        Settlement, Text, WorkerOutcome, WorkerState, Workspace, fake::ExecuteFault,
    },
    state::{EffectState, OwnershipEvent, TaskState, run_effect},
    workflows::{
        coordination::{LaunchOutcome, launch_worker},
        pickup::{ClaimOutcome, claim_issue, issue_task_id},
        task_cancel::{self, CancelError},
    },
};
use workflows_support::{World, brief, issue, template, under_consumer};

fn launched(world: &World) -> TestResult<(TaskId, kitchen::contracts::ResourceRef)> {
    let (claimant, _) = under_consumer(world, "coordinator")?;
    let issue = issue(290)?;
    let lease = match claim_issue(
        &world.fixture.store,
        &template()?,
        &issue,
        &claimant,
        ttl(300)?,
        world.now(),
    )? {
        ClaimOutcome::Claimed(lease) => lease,
        other => return Err(format!("unexpected claim: {other:?}").into()),
    };
    let task = issue_task_id(&issue)?;
    let worker = match launch_worker(
        &world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &brief(290)?,
    )? {
        LaunchOutcome::Accepted { worker, .. } => worker,
        other => return Err(format!("unexpected launch: {other:?}").into()),
    };
    Ok((task, worker))
}

fn launch_receipt(
    world: &World,
    task: &TaskId,
) -> TestResult<(kitchen::contracts::IdempotencyKey, Receipt)> {
    let record = world.fixture.store.task(task)?;
    let effect = record.effects().first().ok_or("missing launch")?;
    let EffectState::Applied { receipt, .. } = effect.state() else {
        return Err("launch was not applied".into());
    };
    Ok((effect.request().key().clone(), receipt.clone()))
}

fn claimed_fence(world: &World, task: &TaskId) -> TestResult<kitchen::contracts::Fence> {
    let record = world.fixture.store.task(task)?;
    let TaskState::Claimed { lease } = record.state() else {
        return Err("task is not claimed".into());
    };
    Ok(lease.fence())
}

#[test]
fn applied_messages_and_reply_use_receipts_after_worker_settles() -> TestResult {
    let capabilities =
        CapabilitySet::supporting(Capability::ALL.iter().copied().filter(|capability| {
            !matches!(
                capability,
                Capability::EffectLookup
                    | Capability::LookupMessageWorker
                    | Capability::LookupReplyToWorker
            )
        }));
    let world = World::with_capabilities(capabilities)?;
    let (task, worker) = launched(&world)?;
    let fence = claimed_fence(&world, &task)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    for (name, operation) in [
        (
            "message-1",
            Operation::MessageWorker {
                worker: worker.clone(),
                body: Text::new("Follow up")?,
            },
        ),
        (
            "reply-1",
            Operation::ReplyToWorker {
                worker: worker.clone(),
                question: ExternalRef::new("question-1")?,
                body: Text::new("Proceed")?,
            },
        ),
    ] {
        let effect = run_effect(
            &world.fixture.store,
            &world.backend,
            &world.grants,
            plan(&task, fence, name, Effect::Worker(operation))?,
            &world.clock,
        )?;
        assert!(matches!(effect.state(), EffectState::Applied { .. }));
    }
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let preview = task_cancel::preview(&world.fixture.store, &world.backend, &task)?;
    assert_eq!(preview.effects(), 3);
    task_cancel::cancel(
        &world.fixture.store,
        &world.backend,
        preview,
        holder("person")?,
        Text::new("settled round")?,
        &world.clock,
    )?;
    assert!(matches!(
        world.fixture.reopen()?.task(&task)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    Ok(())
}

#[test]
fn stopped_launch_with_reconstructed_receipt_cancels() -> TestResult {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/task_cancel/stopped_launch.json"))?;
    assert_eq!(fixture["name"], "launch-1");
    assert_eq!(
        fixture["request"]["effect"]["effect"]["type"],
        "launch-worker"
    );
    assert_eq!(fixture["state"]["type"], "applied");
    assert_eq!(fixture["lookup"], "ended");
    assert_eq!(fixture["workerState"], "stopped");
    let example: Receipt = serde_json::from_value(fixture["state"]["receipt"].clone())?;
    assert!(
        example
            .created()
            .iter()
            .any(|resource| resource.kind == ResourceKind::Worker)
    );
    let world = World::new()?;
    let (task, worker) = launched(&world)?;
    let (key, persisted) = launch_receipt(&world, &task)?;
    let reconstructed = Receipt::new(
        persisted.reference().clone(),
        vec![worker.clone()],
        example.touched().to_vec(),
    )?;
    assert_ne!(reconstructed, persisted);
    world.backend.set_lookup(key, Lookup::Ended(reconstructed));
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Cancelled));
    let preview = task_cancel::preview(&world.fixture.store, &world.backend, &task)?;
    assert_eq!(preview.workers(), 1);
    task_cancel::cancel(
        &world.fixture.store,
        &world.backend,
        preview,
        holder("person")?,
        Text::new("stopped launch")?,
        &world.clock,
    )?;
    assert!(matches!(
        world.fixture.reopen()?.task(&task)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    Ok(())
}

#[test]
fn failed_worker_allows_changed_applied_lookup_but_foreign_identity_refuses() -> TestResult {
    let world = World::new()?;
    let (task, worker) = launched(&world)?;
    let (key, persisted) = launch_receipt(&world, &task)?;
    let changed = Receipt::new(persisted.reference().clone(), vec![worker.clone()], vec![])?;
    assert_ne!(changed, persisted);
    world.backend.set_lookup(key, Lookup::Applied(changed));
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        task_cancel::preview(&world.fixture.store, &world.backend, &task)?.workers(),
        1
    );

    let other = World::new()?;
    let (task, worker) = launched(&other)?;
    let (key, persisted) = launch_receipt(&other, &task)?;
    let foreign = Receipt::new(
        persisted.reference().clone(),
        vec![kitchen::contracts::ResourceRef {
            handle: ExternalRef::new("ctx_foreign")?,
            ..worker.clone()
        }],
        vec![],
    )?;
    other.backend.set_lookup(key, Lookup::Ended(foreign));
    other
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Cancelled));
    let error = task_cancel::preview(&other.fixture.store, &other.backend, &task)
        .err()
        .ok_or("different launch identity was accepted")?;
    assert!(matches!(error, CancelError::Effect { .. }));
    assert!(error.to_string().contains("launch-1"));
    Ok(())
}

#[test]
fn uncertain_or_live_launch_names_the_cause() -> TestResult {
    let world = World::new()?;
    let (task, worker) = launched(&world)?;
    let (key, _) = launch_receipt(&world, &task)?;
    world.backend.set_lookup(key, Lookup::Unknown);
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Cancelled));
    let error = task_cancel::preview(&world.fixture.store, &world.backend, &task)
        .err()
        .ok_or("uncertain lookup was accepted")?;
    assert!(matches!(error, CancelError::Effect { .. }));
    assert!(error.to_string().contains("launch-1"));
    assert!(error.to_string().contains("uncertain"));

    let live = World::new()?;
    let (task, worker) = launched(&live)?;
    live.backend.set_worker_state(&worker, WorkerState::Ready);
    let error = task_cancel::preview(&live.fixture.store, &live.backend, &task)
        .err()
        .ok_or("live launch was accepted")?;
    assert!(matches!(error, CancelError::Worker { .. }));
    assert!(error.to_string().contains("Ready"));
    Ok(())
}

#[test]
fn stopped_worker_can_be_cancelled_under_a_fresh_fence_with_reason() -> TestResult {
    let world = World::new()?;
    let (task, worker) = launched(&world)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Cancelled));
    let preview = task_cancel::preview(&world.fixture.store, &world.backend, &task)?;
    assert_eq!(preview.workers(), 1);
    task_cancel::cancel(
        &world.fixture.store,
        &world.backend,
        preview,
        holder("person")?,
        Text::new("old round cannot relaunch")?,
        &world.clock,
    )?;
    let record = world.fixture.reopen()?.task(&task)?;
    assert!(matches!(
        record.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    assert_eq!(
        record
            .cancel_request()
            .and_then(|request| request.reason())
            .map(Text::as_str),
        Some("old round cannot relaunch")
    );
    assert!(matches!(
        record.ownership(),
        [
            ..,
            OwnershipEvent::TakenOver { .. },
            OwnershipEvent::Released { .. }
        ]
    ));
    Ok(())
}

#[test]
fn live_or_unobservable_worker_names_the_blocker() -> TestResult {
    let world = World::new()?;
    let (task, worker) = launched(&world)?;
    for state in [
        WorkerState::Starting,
        WorkerState::Ready,
        WorkerState::Unknown,
        WorkerState::Missing,
    ] {
        world.backend.set_worker_state(&worker, state);
        let error = task_cancel::preview(&world.fixture.store, &world.backend, &task)
            .err()
            .ok_or("preview accepted unsafe worker")?;
        assert!(matches!(error, CancelError::Worker { .. }));
        assert!(error.to_string().contains(&format!("{state:?}")));
    }
    Ok(())
}

#[test]
fn uncertain_effect_lookup_and_changed_preview_refuse_without_settlement() -> TestResult {
    let world = World::new()?;
    let (task, worker) = launched(&world)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    world.backend.fail_lookups(1);
    let error = task_cancel::preview(&world.fixture.store, &world.backend, &task)
        .err()
        .ok_or("lookup failure was accepted")?;
    assert!(matches!(error, CancelError::Effect { .. }));
    assert!(error.to_string().contains("launch"));
    let preview = task_cancel::preview(&world.fixture.store, &world.backend, &task)?;
    world
        .fixture
        .store
        .request_cancel(&task, &holder("other")?, world.now())?;
    assert!(matches!(
        task_cancel::cancel(
            &world.fixture.store,
            &world.backend,
            preview,
            holder("person")?,
            Text::new("reason")?,
            &world.clock
        ),
        Err(CancelError::Changed)
    ));
    assert!(matches!(
        world.fixture.store.task(&task)?.state(),
        TaskState::Claimed { .. }
    ));
    Ok(())
}

#[test]
fn not_applied_launch_can_be_cancelled_without_a_worker() -> TestResult {
    let world = World::new()?;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    let issue = issue(291)?;
    let lease = match claim_issue(
        &world.fixture.store,
        &template()?,
        &issue,
        &claimant,
        ttl(300)?,
        world.now(),
    )? {
        ClaimOutcome::Claimed(lease) => lease,
        other => return Err(format!("unexpected claim: {other:?}").into()),
    };
    let task = issue_task_id(&issue)?;
    world.backend.inject(ExecuteFault::Reject);
    let _ = launch_worker(
        &world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &brief(291)?,
    );
    let before = world.fixture.store.task(&task)?;
    assert!(matches!(
        before.effects().first().map(|effect| effect.state()),
        Some(EffectState::NotApplied { .. })
    ));
    let preview = task_cancel::preview(&world.fixture.store, &world.backend, &task)?;
    assert_eq!(preview.workers(), 0);
    task_cancel::cancel(
        &world.fixture.store,
        &world.backend,
        preview,
        holder("person")?,
        Text::new("launch never applied")?,
        &world.clock,
    )?;
    assert!(matches!(
        world.fixture.reopen()?.task(&task)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    Ok(())
}

#[test]
fn uncertain_launch_effect_names_the_effect_and_keeps_the_task_claimed() -> TestResult {
    let world = World::new()?;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    let issue = issue(292)?;
    let lease = match claim_issue(
        &world.fixture.store,
        &template()?,
        &issue,
        &claimant,
        ttl(300)?,
        world.now(),
    )? {
        ClaimOutcome::Claimed(lease) => lease,
        other => return Err(format!("unexpected claim: {other:?}").into()),
    };
    let task = issue_task_id(&issue)?;
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    let _ = launch_worker(
        &world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &brief(292)?,
    );
    assert!(matches!(
        world
            .fixture
            .store
            .task(&task)?
            .effects()
            .first()
            .map(|effect| effect.state()),
        Some(EffectState::Uncertain { .. })
    ));
    let error = task_cancel::preview(&world.fixture.store, &world.backend, &task)
        .err()
        .ok_or("uncertain effect was accepted")?;
    assert!(matches!(error, CancelError::Effect { .. }));
    assert!(error.to_string().contains("launch"));
    assert!(matches!(
        world.fixture.reopen()?.task(&task)?.state(),
        TaskState::Claimed { .. }
    ));
    Ok(())
}
