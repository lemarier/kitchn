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
    let spec = world.fixture.store.task(task)?.spec().clone();
    Ok(launch_worker(
        &world.ctx(),
        task,
        fence,
        Workspace::Isolated,
        brief(number)?.render(&spec)?,
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
    let spec = store.task(&task)?.spec().clone();
    let text = brief(1)?.render(&spec)?;
    let refused = launch_worker(
        &world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        text.clone(),
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
        text,
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
    let spec = world.fixture.store.task(&task)?.spec().clone();
    let approves = Approves::new("david")?;
    let error = launch_worker(
        &world.ctx_with(&approves),
        &task,
        fence,
        Workspace::Isolated,
        brief(1)?.render(&spec)?,
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
