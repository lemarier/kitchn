//! Supervised coordination against the fake backend: launch, supervision to
//! settlement, questions, and coordinator relinquish and adoption. Simulated
//! evidence only; no live worker is launched.

mod common;
mod workflows_support;

use common::{TestResult, commit, interactive, ttl};
use kitchen::contracts::EffectExecutor;
use kitchen::{
    ErrorClass, TaskId,
    contracts::{
        AskKind, AskRisk, Authorization, Capability, CapabilitySet, ContractError, Disposition,
        Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict, ExternalRef, Fence, Permission,
        PostingBudget, ResourceRef, Settlement, Text, WorkerOutcome, WorkerState, Workspace,
        fake::{ExecuteFault, FakeBackend},
    },
    state::{ConsumerEvent, OwnershipEvent, RecoveryItem, TaskState},
    workflows::{
        coordination::{
            Completion, CoordinatorStart, Escalation, HumanDecision, LaunchOutcome,
            QuestionEscalation, QuestionRoute, Response, RogerChannel, Supervision,
            TerminalControl, WorkerQuestion, handle_question, launch_worker,
            relinquish_coordinator, start_coordinator, supervise,
        },
        pickup::{ClaimOutcome, claim_issue, issue_task_id},
    },
};
use workflows_support::{
    Approves, World, branch, brief, consumer, issue, supervision, template, template_with,
    under_consumer,
};

fn claim(world: &World, holder: &str, number: u64, attempts: u32) -> TestResult<(TaskId, Fence)> {
    let (claimant, _) = under_consumer(world, holder)?;
    match claim_issue(
        &world.fixture.store,
        &template_with(attempts, workflows_support::provenance('a')?)?,
        &issue(number)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )? {
        ClaimOutcome::Claimed(lease) => Ok((issue_task_id(&issue(number)?)?, lease.fence())),
        other => Err(format!("unexpected claim outcome {other:?}").into()),
    }
}

fn launch(world: &World, task: &TaskId, fence: Fence, number: u64) -> TestResult<LaunchOutcome> {
    Ok(launch_worker(
        &world.ctx(),
        task,
        fence,
        Workspace::Isolated,
        &brief(number)?,
    )?)
}

fn launched(world: &World, task: &TaskId, fence: Fence, number: u64) -> TestResult<ResourceRef> {
    match launch(world, task, fence, number)? {
        LaunchOutcome::Accepted { worker, .. } => Ok(worker),
        other => Err(format!("launch not accepted: {other:?}").into()),
    }
}

fn step(world: &World, task: &TaskId, fence: Fence) -> TestResult<Supervision> {
    Ok(supervise(
        &world.ctx(),
        task,
        fence,
        &supervision()?,
        TerminalControl::Agent,
        None,
    )?)
}

fn report(verdict: EvidenceVerdict) -> TestResult<Evidence> {
    Ok(Evidence {
        kind: EvidenceKind::WorkerReport,
        verdict,
        subject: EvidenceSubject {
            head: commit('d')?,
            base: Some(commit('e')?),
        },
        source: ExternalRef::new("reports/issue.md")?,
        observed_at: common::at(1_000),
    })
}

fn completion(observed: &str, verdict: EvidenceVerdict) -> TestResult<Completion> {
    Ok(Completion {
        requested: branch("lemarier/issue-1")?,
        observed_branch: observed.to_owned(),
        report: report(verdict)?,
    })
}

#[test]
fn a_worker_settles_only_with_readable_passing_evidence_on_its_exact_branch() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Starting)
    );
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Ready)
    );

    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Escalate(Escalation::MissingEvidence)
    );
    let policy = supervision()?;
    let run = |completion: &Completion| {
        supervise(
            &world.ctx(),
            &task,
            fence,
            &policy,
            TerminalControl::Agent,
            Some(completion),
        )
    };
    let prefixed = completion("orca/lemarier/issue-1", EvidenceVerdict::Pass)?;
    assert_eq!(
        run(&prefixed)?,
        Supervision::Escalate(Escalation::BranchMismatch)
    );
    let failing = completion("lemarier/issue-1", EvidenceVerdict::Fail)?;
    assert_eq!(
        run(&failing)?,
        Supervision::Escalate(Escalation::MissingEvidence)
    );
    assert!(matches!(
        world.fixture.store.task(&task)?.state(),
        TaskState::Claimed { .. }
    ));

    let good = completion("lemarier/issue-1", EvidenceVerdict::Pass)?;
    assert_eq!(run(&good)?, Supervision::Settled(Settlement::Succeeded));
    let record = world.fixture.store.task(&task)?;
    assert!(matches!(
        record.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    assert_eq!(record.evidence().items(), &[good.report]);
    assert_eq!(world.backend.effects_performed(), 1);
    Ok(())
}

#[test]
fn an_uncertain_launch_is_reconciled_and_never_repeated() -> TestResult {
    // Lookup, but no provider-side idempotency: resubmission is unsafe.
    let capabilities = CapabilitySet::supporting(
        Capability::ALL
            .into_iter()
            .filter(|capability| *capability != Capability::EffectIdempotentRequests),
    );
    let world = World::with_capabilities(capabilities)?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(launch(&world, &task, fence, 1)?, LaunchOutcome::Uncertain);
    // A duplicate tick asks again: nothing is submitted.
    assert_eq!(launch(&world, &task, fence, 1)?, LaunchOutcome::Uncertain);
    assert_eq!(world.backend.execute_calls(), 1);

    // An unavailable lookup keeps the effect unresolved.
    world.backend.fail_lookups(1);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Reconciling { unresolved: 1 }
    );
    // The next lookup finds the applied launch; the claim was kept throughout.
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Starting)
    );
    assert_eq!(world.backend.effects_performed(), 1);
    assert_eq!(world.backend.execute_calls(), 1);
    Ok(())
}

#[test]
fn failed_workers_retry_within_the_budget_and_then_exhaust() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 2)?;
    let first = launched(&world, &task, fence, 1)?;
    world
        .backend
        .set_worker_state(&first, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Retry { remaining: 1 }
    );

    // A duplicate tick before the relaunch does not fail the finished
    // attempt's worker again or spend another attempt.
    assert_eq!(step(&world, &task, fence)?, Supervision::AwaitingLaunch);
    assert_eq!(world.fixture.store.task(&task)?.attempts().len(), 1);

    let second = launched(&world, &task, fence, 1)?;
    assert_ne!(first, second);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Starting)
    );
    world
        .backend
        .set_worker_state(&second, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Settled(Settlement::Exhausted)
    );
    assert_eq!(launch(&world, &task, fence, 1).ok(), None);
    assert_eq!(world.backend.effects_performed(), 2);
    Ok(())
}

#[test]
fn a_launch_without_readiness_evidence_is_stopped_and_counted_as_failed() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    world.clock.advance(119);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Starting)
    );
    world.clock.advance(2);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::LaunchStalled {
            disposition: Disposition::RetryAvailable { remaining: 2 }
        }
    );
    use kitchen::contracts::WorkerBackend;
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    Ok(())
}

#[test]
fn a_person_owns_a_taken_over_terminal() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    world.clock.advance(200);
    let calls = world.backend.execute_calls();
    let outcome = supervise(
        &world.ctx(),
        &task,
        fence,
        &supervision()?,
        TerminalControl::UserTakeover,
        None,
    )?;
    assert_eq!(outcome, Supervision::PersonOwnsTerminal);
    // Past the readiness deadline, but nothing is stopped or sent.
    assert_eq!(world.backend.execute_calls(), calls);
    use kitchen::contracts::WorkerBackend;
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Starting
    );
    Ok(())
}

#[test]
fn a_missing_or_unknown_worker_never_settles_the_task() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    assert_eq!(step(&world, &task, fence)?, Supervision::AwaitingLaunch);
    let worker = launched(&world, &task, fence, 1)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Missing);
    assert_eq!(step(&world, &task, fence)?, Supervision::WorkerMissing);
    world
        .backend
        .set_worker_state(&worker, WorkerState::Unknown);
    assert_eq!(step(&world, &task, fence)?, Supervision::Unobservable);
    assert!(matches!(
        world.fixture.store.task(&task)?.state(),
        TaskState::Claimed { .. }
    ));
    Ok(())
}

fn question(id: &str, asked_at: u64) -> TestResult<WorkerQuestion> {
    Ok(WorkerQuestion {
        id: ExternalRef::new(id)?,
        asked_at: common::at(asked_at),
    })
}

fn decision(task: &TaskId) -> TestResult<HumanDecision> {
    Ok(HumanDecision {
        action: Permission::OpenPullRequest,
        target: ExternalRef::new(&format!("task:{task}"))?,
        subject: commit('d')?,
        limits: Text::new("Open one draft pull request.")?,
        kind: AskKind::Approval,
        risk: AskRisk::Routine,
        title: Text::new("Open the pull request?")?,
        body: Text::new("The worker finished the driver change.")?,
    })
}

#[test]
fn an_answer_reaches_the_worker_exactly_once() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    let asked = question("msg-1", 1_000)?;
    let answer = Response::Answer(Text::new("Use the existing SPI helper.")?);
    let policy = supervision()?;
    let handle = |response: &Response| {
        handle_question(&world.ctx(), &task, fence, &policy, &asked, response, None)
    };
    assert_eq!(handle(&Response::Pending)?, QuestionRoute::Waiting);
    assert_eq!(handle(&answer)?, QuestionRoute::Replied);
    assert_eq!(handle(&answer)?, QuestionRoute::Duplicate);
    assert_eq!(world.backend.effects_performed(), 2);
    Ok(())
}

#[test]
fn human_decisions_go_through_roger_only_when_installed() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    let asked = question("msg-2", 1_000)?;
    let human = Response::Human(decision(&task)?);
    let calls = world.backend.execute_calls();
    assert_eq!(
        handle_question(
            &world.ctx(),
            &task,
            fence,
            &supervision()?,
            &asked,
            &human,
            None
        )?,
        QuestionRoute::Escalate(QuestionEscalation::NoHumanChannel)
    );
    assert_eq!(world.backend.execute_calls(), calls);

    // Roger declares only what asking needs; worker capabilities are not
    // imposed on it.
    let roger = &world.roger;
    let requester = ExternalRef::new("kitchen-origin89-pickup")?;
    let channel = RogerChannel {
        executor: roger,
        requester: &requester,
        budget: PostingBudget::new(3)?,
    };
    let policy = kitchen::workflows::coordination::SupervisionPolicy {
        question_deadline: std::time::Duration::from_secs(100),
        ..supervision()?
    };
    let route = |response: &Response| {
        handle_question(
            &world.ctx(),
            &task,
            fence,
            &policy,
            &asked,
            response,
            Some(&channel),
        )
    };
    assert_eq!(route(&human)?, QuestionRoute::AskedHuman);
    // Retries and duplicate ticks do not ask again.
    assert_eq!(route(&human)?, QuestionRoute::Waiting);
    assert_eq!(roger.effects_performed(), 1);
    let record = world.fixture.store.task(&task)?;
    let house = common::house()?;
    assert!(record.effects().iter().any(|effect| matches!(
        effect.request().effect(),
        kitchen::contracts::Effect::Roger(ask) if ask.ask.binding.task == task
            && ask.ask.binding.house == house
    )));

    // Past the deadline an unanswered decision is escalated, not re-asked.
    world.clock.advance(101);
    assert_eq!(
        route(&human)?,
        QuestionRoute::Escalate(QuestionEscalation::Unanswered)
    );
    assert_eq!(roger.effects_performed(), 1);
    // A late answer is still delivered once.
    assert_eq!(
        route(&Response::Answer(Text::new(
            "Approved: open it as a draft."
        )?))?,
        QuestionRoute::Replied
    );
    Ok(())
}

#[test]
fn a_question_without_a_worker_is_escalated() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let route = handle_question(
        &world.ctx(),
        &task,
        fence,
        &supervision()?,
        &question("msg-3", 1_000)?,
        &Response::Answer(Text::new("Proceed.")?),
        None,
    )?;
    assert_eq!(route, QuestionRoute::Escalate(QuestionEscalation::NoWorker));
    assert_eq!(world.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn duplicate_ticks_are_refused_by_the_consumer_lease() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let first = common::scheduled("tick-1")?;
    let second = common::scheduled("tick-2")?;
    assert!(matches!(
        start_coordinator(
            store,
            world.backend.descriptor(),
            &consumer()?,
            &first,
            ttl(600)?,
            world.now()
        )?,
        CoordinatorStart::Fresh(_)
    ));
    assert_eq!(
        start_coordinator(
            store,
            world.backend.descriptor(),
            &consumer()?,
            &second,
            ttl(600)?,
            world.now()
        )?,
        CoordinatorStart::Busy
    );
    Ok(())
}

#[test]
fn a_coordinator_hands_over_through_a_recorded_relinquish_and_adoption() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let old = common::scheduled("coordinator-a")?;
    let CoordinatorStart::Fresh(lease) = start_coordinator(
        store,
        world.backend.descriptor(),
        &consumer()?,
        &old,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("first coordinator did not start".into());
    };
    let claimant = old.clone().under(consumer()?, lease.fence());
    let ClaimOutcome::Claimed(task_lease) = claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let task = issue_task_id(&issue(1)?)?;
    let worker = launched(&world, &task, task_lease.fence(), 1)?;
    let asked = question("msg-4", 1_000)?;
    let answer = Response::Answer(Text::new("Use the existing SPI helper.")?);
    handle_question(
        &world.ctx(),
        &task,
        task_lease.fence(),
        &supervision()?,
        &asked,
        &answer,
        None,
    )?;
    let calls = world.backend.execute_calls();

    assert_eq!(
        relinquish_coordinator(store, &consumer()?, lease.fence(), world.now())?,
        vec![task.clone()]
    );
    let new = common::scheduled("coordinator-b")?;
    let CoordinatorStart::Adopted { tasks, skipped, .. } = start_coordinator(
        store,
        world.backend.descriptor(),
        &consumer()?,
        &new,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("relinquished scope was not adopted".into());
    };
    assert!(skipped.is_empty());
    let [(adopted, adopted_lease)] = tasks.as_slice() else {
        return Err("expected one adopted task".into());
    };
    assert_eq!(adopted, &task);
    let history: Vec<_> = store
        .consumer(&consumer()?)?
        .ok_or("consumer missing")?
        .history()
        .cloned()
        .collect();
    assert!(matches!(
        history.as_slice(),
        [
            ConsumerEvent::Acquired { .. },
            ConsumerEvent::Relinquished { .. },
            ConsumerEvent::Adopted { .. }
        ]
    ));
    assert!(matches!(
        store.task(&task)?.ownership().last(),
        Some(OwnershipEvent::Adopted { .. })
    ));
    // Workers were untouched, the new owner supervises the same worker, and
    // the answered question is not delivered again.
    assert_eq!(world.backend.execute_calls(), calls);
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    assert_eq!(
        step(&world, &task, adopted_lease.fence())?,
        Supervision::Running(WorkerState::Ready)
    );
    assert_eq!(
        handle_question(
            &world.ctx(),
            &task,
            adopted_lease.fence(),
            &supervision()?,
            &asked,
            &answer,
            None
        )?,
        QuestionRoute::Duplicate
    );
    assert_eq!(world.backend.execute_calls(), calls);
    // The previous coordinator can no longer act.
    let stale = step(&world, &task, task_lease.fence())
        .err()
        .ok_or("stale owner acted")?;
    assert_eq!(
        stale
            .downcast_ref::<kitchen::Error>()
            .map(kitchen::Error::class),
        Some(ErrorClass::Conflict)
    );
    Ok(())
}

#[test]
fn a_coordinator_exit_without_relinquish_is_uncertain_not_released() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let (task, fence) = claim(&world, "coordinator-a", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    world.clock.advance(601);
    let new = common::scheduled("coordinator-b")?;
    assert!(matches!(
        start_coordinator(
            store,
            world.backend.descriptor(),
            &consumer()?,
            &new,
            ttl(600)?,
            world.now()
        )?,
        CoordinatorStart::Uncertain { .. }
    ));
    let queue = store.recovery_queue(world.now())?;
    assert!(
        queue
            .iter()
            .any(|item| matches!(item, RecoveryItem::UncertainConsumer { .. }))
    );
    assert!(
        queue
            .iter()
            .any(|item| matches!(item, RecoveryItem::UncertainTaskOwner { .. }))
    );
    // No implicit adoption: the task stays with its silent owner.
    assert!(matches!(
        store.task(&task)?.ownership(),
        [OwnershipEvent::Claimed { .. }]
    ));
    // The silent owner cannot resume either.
    assert!(step(&world, &task, fence).is_err());
    Ok(())
}

#[test]
fn interactive_work_needs_consent_for_each_effect() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let person = interactive("david")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &person,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let task = issue_task_id(&issue(1)?)?;
    let requested = brief(1)?;
    let refused = launch_worker(
        &world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &requested,
    )
    .err()
    .ok_or("launch without consent")?;
    assert!(matches!(
        refused,
        kitchen::Error::Contract(ContractError::ConsentRequired {
            permission: Permission::LaunchWorker
        })
    ));
    assert_eq!(world.backend.execute_calls(), 0);

    let approves = Approves::new("david")?;
    let launched = launch_worker(
        &world.ctx_with(&approves),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &requested,
    )?;
    assert!(matches!(launched, LaunchOutcome::Accepted { .. }));
    let record = store.task(&task)?;
    assert!(matches!(
        record.effects().last().map(|effect| effect.authorization()),
        Some(Authorization::Consent { .. })
    ));
    Ok(())
}

#[test]
fn scheduled_work_refuses_a_persons_consent() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let approves = Approves::new("david")?;
    let error = launch_worker(
        &world.ctx_with(&approves),
        &task,
        fence,
        Workspace::Isolated,
        &brief(1)?,
    )
    .err()
    .ok_or("scheduled work accepted consent")?;
    assert!(matches!(
        error,
        kitchen::Error::Contract(ContractError::ConsentNotAccepted)
    ));
    assert_eq!(world.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn a_coordinator_does_not_start_on_a_backend_missing_required_capabilities() -> TestResult {
    let limited = CapabilitySet::supporting(
        Capability::ALL
            .into_iter()
            .filter(|capability| *capability != Capability::WorkerLaunchReadiness),
    );
    let world = World::with_capabilities(limited)?;
    let error = start_coordinator(
        &world.fixture.store,
        world.backend.descriptor(),
        &consumer()?,
        &common::scheduled("tick")?,
        ttl(600)?,
        world.now(),
    )
    .err()
    .ok_or("coordinator started without launch readiness")?;
    assert!(matches!(
        error,
        kitchen::Error::Contract(ContractError::UnsupportedCapabilities { ref missing, ref partial })
            if missing == &[Capability::WorkerLaunchReadiness] && partial.is_empty()
    ));
    assert_eq!(error.class(), ErrorClass::Refused);
    assert!(world.fixture.store.consumer(&consumer()?)?.is_none());
    Ok(())
}

#[test]
fn a_refused_human_ask_stays_escalated_and_is_not_retried_silently() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    let roger = FakeBackend::new(
        workflows_support::roger_backend_id()?,
        common::house()?,
        CapabilitySet::supporting([Capability::AskHuman]),
    );
    roger.inject(ExecuteFault::Reject);
    let requester = ExternalRef::new("kitchen-origin89-pickup")?;
    let channel = RogerChannel {
        executor: &roger,
        requester: &requester,
        budget: PostingBudget::new(3)?,
    };
    let asked = question("msg-5", 1_000)?;
    let human = Response::Human(decision(&task)?);
    for _ in 0..2 {
        assert_eq!(
            handle_question(
                &world.ctx(),
                &task,
                fence,
                &supervision()?,
                &asked,
                &human,
                Some(&channel)
            )?,
            QuestionRoute::Escalate(QuestionEscalation::NotApplied)
        );
    }
    assert_eq!(roger.execute_calls(), 1);
    assert_eq!(roger.effects_performed(), 0);
    Ok(())
}

fn ticks(world: &World, task: &TaskId) -> TestResult<Vec<kitchen::workflows::pickup::Exclusion>> {
    let tasks = world.fixture.store.tasks()?;
    let selection = kitchen::workflows::pickup::select(
        &workflows_support::policy(1)?,
        &[workflows_support::ready(2)?],
        &tasks,
        world.now(),
    )?;
    assert!(
        tasks.iter().any(|record| &record.spec().id == task),
        "the task under test exists"
    );
    Ok(selection
        .excluded
        .into_iter()
        .map(|(_, exclusion)| exclusion)
        .collect())
}

#[test]
fn a_refused_stop_keeps_the_claim_and_the_slot_while_the_worker_may_still_run() -> TestResult {
    use kitchen::contracts::WorkerBackend;
    use kitchen::state::AttemptState;
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    world.clock.advance(121);
    // The backend refuses to stop the worker that never became ready.
    world.backend.inject(ExecuteFault::Reject);
    let refused = step(&world, &task, fence)?;
    assert_eq!(
        refused,
        Supervision::Escalate(Escalation::StopRefused),
        "a refused stop is escalated, not counted as a stopped worker"
    );

    // The worker may still be running: the attempt stays open, the claim is
    // kept, and its slot is not free for a second writer.
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Starting
    );
    let record = world.fixture.store.task(&task)?;
    assert!(matches!(record.state(), TaskState::Claimed { .. }));
    let [attempt] = record.attempts() else {
        return Err("expected exactly one attempt".into());
    };
    assert_eq!(attempt.state(), AttemptState::Running);
    assert_eq!(
        ticks(&world, &task)?,
        vec![kitchen::workflows::pickup::Exclusion::CapacityFull]
    );

    // Ticks repeat the escalation without asking the backend again, and a
    // repeated launch returns the same worker instead of starting another.
    let calls = world.backend.execute_calls();
    assert_eq!(step(&world, &task, fence)?, refused);
    assert_eq!(world.backend.execute_calls(), calls);
    assert_eq!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::Accepted {
            attempt: attempt.number(),
            worker: worker.clone()
        }
    );
    assert_eq!(world.backend.effects_performed(), 1);

    // Once the backend positively reports the worker stopped, the attempt
    // fails and a replacement may launch.
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Cancelled));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Retry { remaining: 2 }
    );
    let replacement = launched(&world, &task, fence, 1)?;
    assert_ne!(replacement, worker);
    Ok(())
}

#[test]
fn an_uncertain_stop_blocks_until_reconciled_and_then_frees_the_attempt() -> TestResult {
    use kitchen::state::AttemptState;
    let capabilities = CapabilitySet::supporting(
        Capability::ALL
            .into_iter()
            .filter(|capability| *capability != Capability::EffectIdempotentRequests),
    );
    let world = World::with_capabilities(capabilities)?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    world.clock.advance(121);
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Reconciling { unresolved: 1 }
    );
    let record = world.fixture.store.task(&task)?;
    assert_eq!(
        record
            .attempts()
            .last()
            .map(kitchen::state::AttemptRecord::state),
        Some(AttemptState::Running)
    );
    assert_eq!(world.backend.effects_performed(), 2);
    // Reconciliation establishes that the stop applied; only then does the
    // observed cancelled worker fail the attempt.
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Retry { remaining: 2 }
    );
    assert_eq!(world.backend.effects_performed(), 2);
    let replacement = launched(&world, &task, fence, 1)?;
    assert_ne!(replacement, worker);
    Ok(())
}

/// A coordinator launched a worker and handed the scope over; a second
/// coordinator adopted the task and holds the returned fence.
fn adopted_with_live_worker(world: &World) -> TestResult<(TaskId, Fence, ResourceRef)> {
    let store = &world.fixture.store;
    let old = common::scheduled("coordinator-a")?;
    let CoordinatorStart::Fresh(lease) = start_coordinator(
        store,
        world.backend.descriptor(),
        &consumer()?,
        &old,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("first coordinator did not start".into());
    };
    let claimant = old.under(consumer()?, lease.fence());
    let ClaimOutcome::Claimed(task_lease) = claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let task = issue_task_id(&issue(1)?)?;
    let worker = launched(world, &task, task_lease.fence(), 1)?;
    relinquish_coordinator(store, &consumer()?, lease.fence(), world.now())?;
    let CoordinatorStart::Adopted { tasks, .. } = start_coordinator(
        store,
        world.backend.descriptor(),
        &consumer()?,
        &common::scheduled("coordinator-b")?,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("scope was not adopted".into());
    };
    let [(adopted, adopted_lease)] = tasks.as_slice() else {
        return Err("expected one adopted task".into());
    };
    assert_eq!(adopted, &task);
    Ok((task, adopted_lease.fence(), worker))
}

#[test]
fn an_adopted_worker_is_never_duplicated_by_a_launch_before_supervision() -> TestResult {
    // Every state that is not a positive failure or cancellation keeps the
    // earlier worker's branch reserved: launching again could run two writers.
    let reserved = [
        WorkerState::Starting,
        WorkerState::Ready,
        WorkerState::AwaitingReply,
        WorkerState::Missing,
        WorkerState::Unknown,
        WorkerState::Settled(WorkerOutcome::Succeeded),
    ];
    for state in reserved {
        let world = World::new()?;
        let (task, fence, worker) = adopted_with_live_worker(&world)?;
        world.backend.set_worker_state(&worker, state);
        let attempts = world.fixture.store.task(&task)?.attempts().len();
        let outcome = launch(&world, &task, fence, 1)?;
        assert!(
            matches!(&outcome, LaunchOutcome::SuperviseFirst { worker: earlier } if earlier == &worker),
            "worker {state:?} must be supervised before any launch: {outcome:?}"
        );
        assert_eq!(world.backend.effects_performed(), 1, "{state:?}");
        // The refused launch did not even start an attempt.
        assert_eq!(
            world.fixture.store.task(&task)?.attempts().len(),
            attempts,
            "{state:?}"
        );
    }
    Ok(())
}

#[test]
fn a_replacement_launches_only_after_the_adopted_worker_is_shown_stopped() -> TestResult {
    for stopped in [WorkerOutcome::Failed, WorkerOutcome::Cancelled] {
        let world = World::new()?;
        let (task, fence, worker) = adopted_with_live_worker(&world)?;
        world.backend.set_worker_state(&worker, WorkerState::Ready);
        assert_eq!(
            step(&world, &task, fence)?,
            Supervision::Running(WorkerState::Ready)
        );
        assert!(matches!(
            launch(&world, &task, fence, 1)?,
            LaunchOutcome::SuperviseFirst { .. }
        ));

        world
            .backend
            .set_worker_state(&worker, WorkerState::Settled(stopped));
        assert!(matches!(
            launch(&world, &task, fence, 1)?,
            LaunchOutcome::Accepted { .. }
        ));
        assert_eq!(world.backend.effects_performed(), 2);
    }
    Ok(())
}

fn launch_on(
    world: &World,
    reported: &str,
    refuse_stop: bool,
    task: &TaskId,
    fence: Fence,
) -> TestResult<LaunchOutcome> {
    let backend = workflows_support::ReportsBranch {
        inner: &world.backend,
        branch: reported,
        refuse_stop,
    };
    let ctx = kitchen::workflows::coordination::Context {
        backend: &backend,
        ..world.ctx()
    };
    Ok(launch_worker(
        &ctx,
        task,
        fence,
        Workspace::Isolated,
        &brief(1)?,
    )?)
}

#[test]
fn a_worker_placed_on_another_branch_is_stopped_before_it_works() -> TestResult {
    use kitchen::contracts::WorkerBackend;
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    // Orca prefixes the requested name.
    let outcome = launch_on(&world, "orca/lemarier/issue-1", false, &task, fence)?;
    let LaunchOutcome::BranchMismatch {
        worker,
        disposition,
    } = outcome
    else {
        return Err(format!("wrong branch was accepted: {outcome:?}").into());
    };
    // The worker was stopped, and the task needs a person: a retry would
    // put another worker on the same wrong branch.
    assert_eq!(disposition, Disposition::Settled(Settlement::Failed));
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    assert!(matches!(
        world.fixture.store.task(&task)?.state(),
        TaskState::Settled {
            settlement: Settlement::Failed,
            ..
        }
    ));
    Ok(())
}

#[test]
fn a_refused_stop_of_a_misplaced_worker_keeps_the_attempt_open() -> TestResult {
    use kitchen::contracts::WorkerBackend;
    use kitchen::state::AttemptState;
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let outcome = launch_on(&world, "orca/lemarier/issue-1", true, &task, fence)?;
    let LaunchOutcome::StopRefused { worker } = outcome else {
        return Err(format!("unexpected outcome {outcome:?}").into());
    };
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Starting
    );
    let record = world.fixture.store.task(&task)?;
    assert!(matches!(record.state(), TaskState::Claimed { .. }));
    assert_eq!(
        record
            .attempts()
            .last()
            .map(kitchen::state::AttemptRecord::state),
        Some(AttemptState::Running)
    );
    // A repeat reports the same refusal and asks nothing new of the backend.
    let calls = world.backend.execute_calls();
    assert_eq!(
        launch_on(&world, "orca/lemarier/issue-1", true, &task, fence)?,
        LaunchOutcome::StopRefused { worker }
    );
    assert_eq!(world.backend.execute_calls(), calls);
    Ok(())
}

#[test]
fn a_launch_is_accepted_when_the_backend_reports_the_exact_branch_or_none() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    assert!(matches!(
        launch_on(&world, "lemarier/issue-1", false, &task, fence)?,
        LaunchOutcome::Accepted { .. }
    ));
    // Until the contract carries the requested branch, a backend that
    // reports none is not refused; settlement still checks the branch.
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    assert!(matches!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::Accepted { .. }
    ));
    Ok(())
}

#[test]
fn supervising_an_adopted_worker_to_its_end_allows_the_replacement() -> TestResult {
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_live_worker(&world)?;
    // The adopting coordinator supervises first: the worker still runs, so
    // nothing new launches.
    assert!(matches!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::SuperviseFirst { .. }
    ));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Starting)
    );
    assert!(matches!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::SuperviseFirst { .. }
    ));

    // It then fails; supervision accounts for the failure, and only after
    // that does a replacement start.
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    assert!(matches!(
        step(&world, &task, fence)?,
        Supervision::Retry { .. }
    ));
    let replacement = launched(&world, &task, fence, 1)?;
    assert_ne!(replacement, worker);
    assert_eq!(world.backend.effects_performed(), 2);
    Ok(())
}

#[test]
fn a_retry_does_not_depend_on_a_finished_workers_record_surviving_cleanup() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let first = launched(&world, &task, fence, 1)?;
    world
        .backend
        .set_worker_state(&first, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Retry { remaining: 2 }
    );
    // The failed attempt was accounted for; the backend has since forgotten
    // its worker (a cleanup released it).
    world.backend.set_worker_state(&first, WorkerState::Missing);
    let second = launched(&world, &task, fence, 1)?;
    assert_ne!(first, second);
    Ok(())
}

#[test]
fn a_refused_stop_of_an_adopted_worker_still_reserves_its_branch() -> TestResult {
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_live_worker(&world)?;
    world.clock.advance(130);
    // Supervision starts an attempt to account for the adopted worker, and
    // the backend refuses to stop it. That attempt has no launch of its own.
    world.backend.inject(ExecuteFault::Reject);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Escalate(Escalation::StopRefused)
    );
    let effects = world.backend.effects_performed();
    // The worker may still run, so no other writer launches beside it.
    assert_eq!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::SuperviseFirst { worker }
    );
    assert_eq!(world.backend.effects_performed(), effects);
    Ok(())
}
