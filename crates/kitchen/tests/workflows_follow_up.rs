//! Follow-ups for a worker whose terminal a person holds are kept in the
//! house store, survive a coordinator restart, and reach the worker once:
//! by message after the person releases the terminal, or in a replacement's
//! brief. Fake backend and temporary stores only: simulated evidence, not
//! live runtime evidence.

mod common;
mod workflows_support;

use common::{TestResult, at, ttl};
use kitchen::{
    Error, TaskId, WorkflowId,
    contracts::{
        Claimant, Disposition, Effect, Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict,
        ExternalRef, Fence, Operation, ResourceRef, Settlement, Text, WorkerOutcome, WorkerState,
        Workspace, fake::ExecuteFault,
    },
    state::{EffectState, HouseStore, TaskState},
    workflows::{
        coordination::{
            Completion, Context, CoordinationError, FollowUpRoute, LaunchOutcome, Release,
            Supervision, SupervisionInput, launch_worker, outstanding_follow_ups,
            release_held_branch, send_follow_up, supervise,
        },
        follow_up::{MAX_HELD_FOLLOW_UP_BYTES, MAX_HELD_FOLLOW_UPS},
        pickup::{ClaimOutcome, claim_issue, issue_task_id},
        recovery::{FollowUp, PromptState, RecoverySignals},
    },
};
use workflows_support::{
    World, branch, brief, issue, signals, supervision, template_with, under_consumer,
};

/// A coordinator whose memory is gone: a fresh store handle on the same
/// directory, the same backend, and nothing else carried over.
fn restarted<'a>(world: &'a World, store: &'a HouseStore) -> Context<'a> {
    Context {
        store,
        ..world.ctx()
    }
}

/// The fence of the task's live claim, read back from the store.
fn claimed_fence(store: &HouseStore, task: &TaskId) -> TestResult<Fence> {
    match store.task(task)?.state() {
        TaskState::Claimed { lease } => Ok(lease.fence()),
        other => Err(format!("task not claimed: {other:?}").into()),
    }
}

/// A scheduled coordinator claimed the task and launched its worker, and a
/// person took the worker's terminal over, as supervision recorded.
fn person_took_over(world: &World) -> TestResult<(TaskId, Fence, Claimant, ResourceRef)> {
    let (claimant, _) = under_consumer(world, "coordinator")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        &world.fixture.store,
        &template_with(3, workflows_support::provenance('a')?)?,
        &issue(1)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let task = issue_task_id(&issue(1)?)?;
    let fence = lease.fence();
    let LaunchOutcome::Accepted { worker, .. } =
        launch_worker(&world.ctx(), &task, fence, Workspace::Isolated, &brief(1)?)?
    else {
        return Err("launch not accepted".into());
    };
    world
        .backend
        .set_worker_state(&worker, WorkerState::UserTakeover);
    assert_eq!(
        step(&world.ctx(), &task, fence, &SupervisionInput::default())?,
        Supervision::PersonOwnsTerminal
    );
    Ok((task, fence, claimant, worker))
}

fn step(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
    input: &SupervisionInput<'_>,
) -> TestResult<Supervision> {
    Ok(supervise(ctx, task, fence, &supervision()?, input)?)
}

fn follow_up(id: &str, body: &str) -> TestResult<FollowUp> {
    Ok(FollowUp {
        id: ExternalRef::new(id)?,
        body: Text::new(body)?,
    })
}

/// The follow-up messages sent for the task: effect name, worker, state.
fn messages(store: &HouseStore, task: &TaskId) -> TestResult<Vec<(String, ResourceRef, String)>> {
    Ok(store
        .task(task)?
        .effects()
        .iter()
        .filter_map(|effect| match effect.request().effect() {
            Effect::Worker(Operation::MessageWorker { worker, body }) => Some((
                effect.name().as_str().to_owned(),
                worker.clone(),
                format!(
                    "{:?}|{}",
                    std::mem::discriminant(effect.state()),
                    body.as_str()
                ),
            )),
            _ => None,
        })
        .collect())
}

fn held_markers(store: &HouseStore) -> TestResult<usize> {
    Ok(store.markers(&WorkflowId::new("held-follow-up")?)?.len())
}

/// The brief text of the task's latest launch.
fn latest_brief(store: &HouseStore, task: &TaskId) -> TestResult<String> {
    store
        .task(task)?
        .effects()
        .iter()
        .rev()
        .find_map(|effect| match effect.request().effect() {
            Effect::Worker(Operation::LaunchWorker { brief, .. }) => {
                Some(brief.as_str().to_owned())
            }
            _ => None,
        })
        .ok_or_else(|| "no launch recorded".into())
}

fn completion(branch_name: &str, addressed: Vec<ExternalRef>) -> TestResult<Completion> {
    Ok(Completion {
        requested: branch(branch_name)?,
        observed_branch: branch_name.to_owned(),
        report: Evidence {
            kind: EvidenceKind::WorkerReport,
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: common::commit('d')?,
                base: Some(common::commit('e')?),
            },
            source: ExternalRef::new("reports/issue.md")?,
            observed_at: at(1_000),
        },
        addressed,
    })
}

/// The person claims the task, releases the branch, and hands the task back
/// to `coordinator`, whose new fence is returned.
fn person_releases(
    world: &World,
    store: &HouseStore,
    task: &TaskId,
    fence: Fence,
    coordinator: &Claimant,
) -> TestResult<Fence> {
    store.relinquish(task, fence, world.now())?;
    let person = common::interactive("david")?;
    let lease = store.claim(task, &person, ttl(300)?, world.now())?;
    assert_eq!(
        release_held_branch(
            store,
            &world.clock,
            task,
            lease.fence(),
            &branch("lemarier/issue-1")?
        )?,
        Release::Released
    );
    store.relinquish(task, lease.fence(), world.now())?;
    Ok(store
        .claim(task, coordinator, ttl(300)?, world.now())?
        .fence())
}

#[test]
fn a_held_follow_up_survives_a_restart_and_reaches_the_released_worker_once() -> TestResult {
    let world = World::new()?;
    let (task, fence, coordinator, worker) = person_took_over(&world)?;
    let request = follow_up("review-1", "Rename the driver constant.")?;
    let calls = world.backend.execute_calls();
    let FollowUpRoute::Held { id } = send_follow_up(&world.ctx(), &task, fence, &request)? else {
        return Err("follow-up not held".into());
    };
    assert_eq!(
        world.backend.execute_calls(),
        calls,
        "sent into the person's terminal"
    );

    // The coordinator restarts: only the store remembers the request.
    let store = world.fixture.reopen()?;
    let ctx = restarted(&world, &store);
    let queued = outstanding_follow_ups(&store, &store.task(&task)?)?;
    let [only] = queued.as_slice() else {
        return Err(format!("expected one held follow-up, found {}", queued.len()).into());
    };
    assert_eq!(only.id, id);
    assert_eq!(only.body.as_str(), "Rename the driver constant.");

    // The release is recorded, but the person still has the terminal:
    // nothing is sent yet.
    let fence = person_releases(
        &world,
        &store,
        &task,
        claimed_fence(&store, &task)?,
        &coordinator,
    )?;
    assert_eq!(
        step(&ctx, &task, fence, &SupervisionInput::default())?,
        Supervision::PersonOwnsTerminal
    );
    assert!(messages(&store, &task)?.is_empty());

    // The person hands the terminal back: the request goes once, to that
    // worker, quoted as data and naming the id to report.
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    assert_eq!(
        step(&ctx, &task, fence, &SupervisionInput::default())?,
        Supervision::Running(WorkerState::Ready)
    );
    let sent = messages(&store, &task)?;
    let [(name, to, body)] = sent.as_slice() else {
        return Err(format!("expected one message, found {sent:?}").into());
    };
    assert_eq!(name, id.as_str());
    assert_eq!(to, &worker);
    assert!(body.contains(r#"Request: "Rename the driver constant.""#));
    assert!(body.contains(&format!("List {id} under")));

    // Later ticks, another restart, and sending it again deliver nothing new.
    let performed = world.backend.effects_performed();
    step(&ctx, &task, fence, &SupervisionInput::default())?;
    let again = world.fixture.reopen()?;
    let ctx = restarted(&world, &again);
    step(&ctx, &task, fence, &SupervisionInput::default())?;
    assert_eq!(
        send_follow_up(&ctx, &task, fence, &request)?,
        FollowUpRoute::Delivered { id: id.clone() }
    );
    assert_eq!(world.backend.effects_performed(), performed);
    assert_eq!(messages(&again, &task)?.len(), 1);
    assert_eq!(held_markers(&again)?, 0, "the sent follow-up is still held");

    // The worker's completion must address it, and then the task settles.
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let done = completion("lemarier/issue-1", vec![id])?;
    assert_eq!(
        step(
            &ctx,
            &task,
            fence,
            &SupervisionInput {
                completion: Some(&done),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::Settled(Settlement::Succeeded)
    );
    assert!(outstanding_follow_ups(&again, &again.task(&task)?)?.is_empty());
    Ok(())
}

#[test]
fn a_replacement_receives_held_follow_ups_in_its_brief_and_never_by_message() -> TestResult {
    let world = World::new()?;
    let (task, fence, _, worker) = person_took_over(&world)?;
    let request = follow_up("review-2", "Add a test for the timeout path.")?;
    let FollowUpRoute::Held { id } = send_follow_up(&world.ctx(), &task, fence, &request)? else {
        return Err("follow-up not held".into());
    };

    // The person's terminal goes idle: the attempt ends and a replacement
    // is due. The coordinator restarts before launching it.
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let last = world.now();
    world.clock.advance(241);
    let idle = RecoverySignals {
        prompt: PromptState::Idle,
        ..signals(&worker, Some(last))
    };
    assert!(matches!(
        step(
            &world.ctx(),
            &task,
            fence,
            &SupervisionInput {
                signals: Some(&idle),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::Replace { .. }
    ));
    let store = world.fixture.reopen()?;
    let ctx = restarted(&world, &store);
    let fence = claimed_fence(&store, &task)?;
    let mut next = brief(1)?;
    next.branch = branch("lemarier/issue-1-retry")?;
    let LaunchOutcome::Accepted {
        worker: replacement,
        ..
    } = launch_worker(&ctx, &task, fence, Workspace::Isolated, &next)?
    else {
        return Err("replacement not launched".into());
    };
    let text = latest_brief(&store, &task)?;
    assert!(text.contains(&format!("- {id}: \"Add a test for the timeout path.\"")));

    // The replacement runs: the request is not sent again by message, and
    // sending it again reports it held rather than sending it.
    world
        .backend
        .set_worker_state(&replacement, WorkerState::Ready);
    let calls = world.backend.execute_calls();
    assert_eq!(
        step(&ctx, &task, fence, &SupervisionInput::default())?,
        Supervision::Running(WorkerState::Ready)
    );
    assert_eq!(
        send_follow_up(&ctx, &task, fence, &request)?,
        FollowUpRoute::Held { id: id.clone() }
    );
    assert_eq!(world.backend.execute_calls(), calls);
    assert!(messages(&store, &task)?.is_empty());

    // A completion that leaves it unaddressed is a follow-up round, and the
    // next brief carries it again.
    world
        .backend
        .set_worker_state(&replacement, WorkerState::Settled(WorkerOutcome::Succeeded));
    let silent = completion("lemarier/issue-1-retry", Vec::new())?;
    assert_eq!(
        step(
            &ctx,
            &task,
            fence,
            &SupervisionInput {
                completion: Some(&silent),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::FollowUpRound {
            missing: vec![id.clone()],
            disposition: Disposition::RetryAvailable { remaining: 1 }
        }
    );
    let mut last_try = brief(1)?;
    last_try.branch = branch("lemarier/issue-1-last")?;
    assert!(matches!(
        launch_worker(&ctx, &task, fence, Workspace::Isolated, &last_try)?,
        LaunchOutcome::Accepted { .. }
    ));
    assert!(latest_brief(&store, &task)?.contains(&format!("- {id}: ")));
    assert!(messages(&store, &task)?.is_empty());
    Ok(())
}

#[test]
fn an_uncertain_delivery_is_reconciled_after_a_restart_never_resent() -> TestResult {
    let world = World::new()?;
    let (task, fence, coordinator, worker) = person_took_over(&world)?;
    let request = follow_up("review-3", "Rename the driver constant.")?;
    assert!(matches!(
        send_follow_up(&world.ctx(), &task, fence, &request)?,
        FollowUpRoute::Held { .. }
    ));
    let store = &world.fixture.store;
    let fence = person_releases(&world, store, &task, fence, &coordinator)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);

    // The message applies but its response is lost.
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(
        step(&world.ctx(), &task, fence, &SupervisionInput::default())?,
        Supervision::Reconciling { unresolved: 1 }
    );
    let performed = world.backend.effects_performed();

    // After a restart, reconciliation finds it applied; nothing is resent.
    let reopened = world.fixture.reopen()?;
    let ctx = restarted(&world, &reopened);
    assert_eq!(
        step(&ctx, &task, fence, &SupervisionInput::default())?,
        Supervision::Running(WorkerState::Ready)
    );
    step(&ctx, &task, fence, &SupervisionInput::default())?;
    assert_eq!(world.backend.effects_performed(), performed);
    let sent = messages(&reopened, &task)?;
    let [(_, _, state)] = sent.as_slice() else {
        return Err(format!("expected one message, found {sent:?}").into());
    };
    let applied = reopened.task(&task)?.effects().iter().any(|effect| {
        matches!(
            effect.request().effect(),
            Effect::Worker(Operation::MessageWorker { .. })
        ) && matches!(effect.state(), EffectState::Applied { .. })
    });
    assert!(applied, "message not reconciled as applied: {state}");
    assert_eq!(held_markers(&reopened)?, 0);
    Ok(())
}

#[test]
fn a_task_holds_a_bounded_number_of_follow_ups_and_refuses_more() -> TestResult {
    let world = World::new()?;
    let (task, fence, _, _) = person_took_over(&world)?;
    let ctx = world.ctx();
    for index in 0..MAX_HELD_FOLLOW_UPS {
        let request = follow_up(&format!("review-{index}"), "Add a test.")?;
        assert!(matches!(
            send_follow_up(&ctx, &task, fence, &request)?,
            FollowUpRoute::Held { .. }
        ));
    }
    // Holding one of them again is not a new follow-up.
    assert!(matches!(
        send_follow_up(&ctx, &task, fence, &follow_up("review-0", "Add a test.")?)?,
        FollowUpRoute::Held { .. }
    ));
    let over = follow_up("review-over", "One too many.")?;
    let refused = send_follow_up(&ctx, &task, fence, &over)
        .err()
        .ok_or("a follow-up past the bound was held")?;
    assert!(matches!(
        refused,
        Error::Coordination(CoordinationError::FollowUpsFull)
    ));
    let store = &world.fixture.store;
    assert_eq!(
        outstanding_follow_ups(store, &store.task(&task)?)?.len(),
        MAX_HELD_FOLLOW_UPS
    );
    Ok(())
}

#[test]
fn a_follow_up_longer_than_the_bound_is_refused_and_one_at_it_is_held() -> TestResult {
    let world = World::new()?;
    let (task, fence, _, _) = person_took_over(&world)?;
    let at_bound = follow_up("review-long", &"a".repeat(MAX_HELD_FOLLOW_UP_BYTES))?;
    assert!(matches!(
        send_follow_up(&world.ctx(), &task, fence, &at_bound)?,
        FollowUpRoute::Held { .. }
    ));
    let over = follow_up("review-longer", &"a".repeat(MAX_HELD_FOLLOW_UP_BYTES + 1))?;
    assert!(matches!(
        send_follow_up(&world.ctx(), &task, fence, &over),
        Err(Error::Coordination(CoordinationError::FollowUpTooLarge))
    ));
    // Within the byte bound, but too long for one marker once encoded.
    let escaped = follow_up("review-escaped", &"\u{1}".repeat(MAX_HELD_FOLLOW_UP_BYTES))?;
    assert!(matches!(
        send_follow_up(&world.ctx(), &task, fence, &escaped),
        Err(Error::Coordination(CoordinationError::FollowUpTooLarge))
    ));
    let store = &world.fixture.store;
    assert!(
        outstanding_follow_ups(store, &store.task(&task)?)?
            .iter()
            .map(|queued| queued.body.as_str().len())
            .collect::<Vec<_>>()
            == vec![MAX_HELD_FOLLOW_UP_BYTES]
    );
    Ok(())
}

#[test]
fn a_superseded_coordinator_cannot_hold_a_follow_up() -> TestResult {
    let world = World::new()?;
    let (task, fence, _, _) = person_took_over(&world)?;
    let store = &world.fixture.store;
    store.relinquish(&task, fence, world.now())?;
    let person = common::interactive("david")?;
    store.claim(&task, &person, ttl(300)?, world.now())?;
    let request = follow_up("review-stale", "Rename the driver constant.")?;
    assert!(send_follow_up(&world.ctx(), &task, fence, &request).is_err());
    assert_eq!(held_markers(store)?, 0);
    Ok(())
}

#[test]
fn held_follow_ups_are_retired_when_the_task_settles() -> TestResult {
    let world = World::new()?;
    let (task, fence, _, worker) = person_took_over(&world)?;
    send_follow_up(
        &world.ctx(),
        &task,
        fence,
        &follow_up("review-5", "Add a test.")?,
    )?;
    assert_eq!(held_markers(&world.fixture.store)?, 1);
    // The person cancels the task and ends the terminal.
    let store = &world.fixture.store;
    store.request_cancel(&task, &common::holder("david")?, world.now())?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Cancelled));
    assert_eq!(
        step(&world.ctx(), &task, fence, &SupervisionInput::default())?,
        Supervision::Settled(Settlement::Cancelled)
    );
    assert_eq!(held_markers(store)?, 0);
    Ok(())
}

#[test]
fn a_held_follow_up_a_completion_addressed_is_never_held_again() -> TestResult {
    let world = World::new()?;
    let (task, fence, _, worker) = person_took_over(&world)?;
    let ctx = world.ctx();
    let first = follow_up("review-6", "Rename the driver constant.")?;
    let second = follow_up("review-7", "Add a test for the timeout path.")?;
    let FollowUpRoute::Held { id: addressed } = send_follow_up(&ctx, &task, fence, &first)? else {
        return Err("first follow-up not held".into());
    };
    let FollowUpRoute::Held { id: open } = send_follow_up(&ctx, &task, fence, &second)? else {
        return Err("second follow-up not held".into());
    };

    // The person ends the terminal; a replacement's brief carries both, and
    // its completion addresses only the first.
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    assert!(matches!(
        step(&ctx, &task, fence, &SupervisionInput::default())?,
        Supervision::Retry { .. }
    ));
    let mut next = brief(1)?;
    next.branch = branch("lemarier/issue-1-retry")?;
    let LaunchOutcome::Accepted {
        worker: replacement,
        ..
    } = launch_worker(&ctx, &task, fence, Workspace::Isolated, &next)?
    else {
        return Err("replacement not launched".into());
    };
    world
        .backend
        .set_worker_state(&replacement, WorkerState::Settled(WorkerOutcome::Succeeded));
    let partial = completion("lemarier/issue-1-retry", vec![addressed.clone()])?;
    assert!(matches!(
        step(
            &ctx,
            &task,
            fence,
            &SupervisionInput {
                completion: Some(&partial),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::FollowUpRound { missing, .. } if missing == vec![open.clone()]
    ));

    // Sending the addressed one again holds nothing, and the next brief
    // carries only the open one.
    assert_eq!(
        send_follow_up(&ctx, &task, fence, &first)?,
        FollowUpRoute::Delivered {
            id: addressed.clone()
        }
    );
    let store = &world.fixture.store;
    assert_eq!(held_markers(store)?, 1);
    let mut last = brief(1)?;
    last.branch = branch("lemarier/issue-1-last")?;
    assert!(matches!(
        launch_worker(&ctx, &task, fence, Workspace::Isolated, &last)?,
        LaunchOutcome::Accepted { .. }
    ));
    let text = latest_brief(store, &task)?;
    assert!(text.contains(&format!("- {open}: ")));
    assert!(!text.contains(addressed.as_str()));
    Ok(())
}
