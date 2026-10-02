//! Owner cancellation with a fake backend; no live worker is contacted.

mod common;
mod workflows_support;

use common::{TestResult, holder, ttl};
use kitchen::{
    TaskId,
    contracts::{Settlement, Text, WorkerOutcome, WorkerState, Workspace, fake::ExecuteFault},
    state::{EffectState, OwnershipEvent, TaskState},
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
