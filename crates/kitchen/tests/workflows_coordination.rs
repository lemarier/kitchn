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
    state::{ConsumerEvent, ConsumerState, OwnershipEvent, RecoveryItem, TaskState},
    workflows::{
        coordination::{
            AnswerSource, Completion, CoordinatorStart, Escalation, HumanDecision, LaunchOutcome,
            QuestionEscalation, QuestionRoute, Response, RogerChannel, Supervision,
            SupervisionInput, WorkerQuestion, handle_question, launch_worker,
            relinquish_coordinator, start_coordinator, start_coordinator_recording, supervise,
        },
        pickup::{ClaimOutcome, claim_issue, issue_task_id},
    },
};
use kitchen::{
    scheduling::AgentFamily,
    selection::{AgentModel, AgentSelection, EffortSupport, SelectionSource, SelectionSupport},
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
        &SupervisionInput::default(),
    )?)
}

/// A step with positive proof that `worker`'s first turn never started.
fn stalled_step(
    world: &World,
    task: &TaskId,
    fence: Fence,
    worker: &ResourceRef,
) -> TestResult<Supervision> {
    let proof = workflows_support::never_started(worker);
    Ok(supervise(
        &world.ctx(),
        task,
        fence,
        &supervision()?,
        &SupervisionInput {
            signals: Some(&proof),
            ..SupervisionInput::default()
        },
    )?)
}

fn report(verdict: EvidenceVerdict) -> TestResult<Evidence> {
    Ok(Evidence {
        kind: EvidenceKind::WorkerReport(kitchen::contracts::CheckoutReport::default()),
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
        addressed: Vec::new(),
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
            &SupervisionInput {
                completion: Some(completion),
                ..SupervisionInput::default()
            },
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
            .filter(|capability| !capability.as_str().starts_with("effect.idempotent")),
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
        stalled_step(&world, &task, fence, &worker)?,
        Supervision::Running(WorkerState::Starting)
    );
    world.clock.advance(2);
    assert_eq!(
        stalled_step(&world, &task, fence, &worker)?,
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
    world
        .backend
        .set_worker_state(&worker, WorkerState::UserTakeover);
    assert_eq!(step(&world, &task, fence)?, Supervision::PersonOwnsTerminal);
    // Past the readiness deadline, but nothing is stopped or sent.
    assert_eq!(world.backend.execute_calls(), calls);
    use kitchen::contracts::WorkerBackend;
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::UserTakeover
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
    let answer = Response::Answer {
        body: Text::new("Use the existing SPI helper.")?,
        source: AnswerSource::Coordinator,
    };
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
    // Without an evidence subject there is no exact head to decide on.
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
        QuestionRoute::Escalate(QuestionEscalation::NoSubject)
    );
    assert_eq!(roger.execute_calls(), 0);
    let evidence = report(EvidenceVerdict::Pass)?;
    let subject = evidence.subject.clone();
    world
        .fixture
        .store
        .record_evidence(&task, fence, evidence, world.now())?;
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
            && ask.ask.binding.subject.as_ref() == Some(&subject)
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
        route(&Response::Answer {
            body: Text::new("Approved: open it as a draft.")?,
            source: AnswerSource::Person,
        })?,
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
        &Response::Answer {
            body: Text::new("Proceed.")?,
            source: AnswerSource::Coordinator,
        },
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
fn a_coordinator_records_each_relinquished_task_before_claiming_it() -> TestResult {
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
    let ClaimOutcome::Claimed(_) = claim_issue(
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
    relinquish_coordinator(store, &consumer()?, lease.fence(), world.now())?;

    // A recorder that fails stops the start before it takes the scope or
    // claims anything.
    let new = common::scheduled("coordinator-b")?;
    let refused = start_coordinator_recording(
        store,
        world.backend.descriptor(),
        &consumer()?,
        &new,
        ttl(600)?,
        world.now(),
        |_| Err(kitchen::workflows::run::RunError::NoBackend.into()),
    );
    assert!(matches!(
        refused,
        Err(kitchen::Error::Run(
            kitchen::workflows::run::RunError::NoBackend
        ))
    ));
    assert!(matches!(store.task(&task)?.state(), TaskState::Open));
    assert!(matches!(
        store.consumer(&consumer()?)?.ok_or("consumer missing")?.state(),
        ConsumerState::Relinquished { lease, .. } if lease.holder() == &old.holder
    ));

    // The next start records the task while it is still open, then adopts it.
    let mut seen = Vec::new();
    let CoordinatorStart::Adopted { tasks, skipped, .. } = start_coordinator_recording(
        store,
        world.backend.descriptor(),
        &consumer()?,
        &new,
        ttl(600)?,
        world.now(),
        |recorded| {
            seen.push((recorded.clone(), store.task(recorded)?.state().clone()));
            Ok(())
        },
    )?
    else {
        return Err("relinquished scope was not adopted".into());
    };
    assert_eq!(seen, [(task.clone(), TaskState::Open)]);
    assert!(skipped.is_empty());
    assert!(matches!(tasks.as_slice(), [(adopted, _)] if *adopted == task));
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
    let answer = Response::Answer {
        body: Text::new("Use the existing SPI helper.")?,
        source: AnswerSource::Coordinator,
    };
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
    // A backend without deliveries starts on the house mailbox instead
    // (`house_mailbox.rs`); one without launch readiness never starts.
    let absent = Capability::WorkerLaunchReadiness;
    let limited = CapabilitySet::supporting(
        Capability::ALL
            .into_iter()
            .filter(|capability| *capability != absent),
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
    .ok_or_else(|| format!("coordinator started without {absent}"))?;
    assert!(
        matches!(
            error,
            kitchen::Error::Contract(ContractError::UnsupportedCapabilities { ref missing, ref partial })
                if missing == &[absent] && partial.is_empty()
        ),
        "{absent}: {error}"
    );
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
    world.fixture.store.record_evidence(
        &task,
        fence,
        report(EvidenceVerdict::Pass)?,
        world.now(),
    )?;
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
    let refused = stalled_step(&world, &task, fence, &worker)?;
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
    assert_eq!(stalled_step(&world, &task, fence, &worker)?, refused);
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
            .filter(|capability| !capability.as_str().starts_with("effect.idempotent")),
    );
    let world = World::with_capabilities(capabilities)?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    world.clock.advance(121);
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(
        stalled_step(&world, &task, fence, &worker)?,
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
    adopted_with_budget(world, 3)
}

/// [`adopted_with_live_worker`] for a task allowed `attempts` attempts.
fn adopted_with_budget(world: &World, attempts: u32) -> TestResult<(TaskId, Fence, ResourceRef)> {
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
        &template_with(attempts, workflows_support::provenance('a')?)?,
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
            LaunchOutcome::Accepted { attempt, worker: current }
                if attempt.get() == 1 && current == worker
        ));

        world
            .backend
            .set_worker_state(&worker, WorkerState::Settled(stopped));
        assert!(matches!(
            step(&world, &task, fence)?,
            Supervision::Retry { .. }
        ));
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
    accepted_prefix: Option<&str>,
    refuse_stop: bool,
    task: &TaskId,
    fence: Fence,
) -> TestResult<LaunchOutcome> {
    let backend = workflows_support::ReportsBranch {
        inner: &world.backend,
        branch: reported,
        accepted_prefix,
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
fn a_backend_accepted_prefix_is_the_durable_task_branch() -> TestResult {
    use kitchen::workflows::coordination::task_branch;

    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let outcome = launch_on(&world, "orca/issue-1", Some("orca"), false, &task, fence)?;
    assert!(matches!(outcome, LaunchOutcome::Accepted { .. }));
    let record = world.fixture.store.task(&task)?;
    assert_eq!(task_branch(&record), Some(branch("orca/issue-1")?));
    Ok(())
}

#[test]
fn a_worker_placed_on_another_branch_is_stopped_before_it_works() -> TestResult {
    use kitchen::contracts::WorkerBackend;
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    // Orca prefixes the requested name.
    let outcome = launch_on(&world, "orca/lemarier/issue-1", None, false, &task, fence)?;
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
    let outcome = launch_on(&world, "orca/lemarier/issue-1", None, true, &task, fence)?;
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
        launch_on(&world, "orca/lemarier/issue-1", None, true, &task, fence)?,
        LaunchOutcome::StopRefused { worker }
    );
    assert_eq!(world.backend.execute_calls(), calls);
    Ok(())
}

#[test]
fn a_launch_is_accepted_when_the_backend_reports_the_exact_branch() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    assert!(matches!(
        launch_on(&world, "lemarier/issue-1", None, false, &task, fence)?,
        LaunchOutcome::Accepted { .. }
    ));
    // The launch request names the exact branch, so the backend creates it
    // rather than learning it from prose in the brief.
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    assert!(matches!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::Accepted { .. }
    ));
    let record = world.fixture.store.task(&task)?;
    let requested = branch("lemarier/issue-1")?;
    assert!(record.effects().iter().any(|effect| matches!(
        effect.request().effect(),
        kitchen::contracts::Effect::Worker(kitchen::contracts::Operation::LaunchWorker {
            branch: Some(named),
            ..
        }) if named == &requested
    )));
    let launch = record
        .effects()
        .iter()
        .find_map(|effect| match effect.request().effect() {
            kitchen::contracts::Effect::Worker(kitchen::contracts::Operation::LaunchWorker {
                brief,
                ..
            }) => Some(brief.as_str()),
            _ => None,
        })
        .ok_or("launch brief missing")?;
    assert!(launch.contains("Push: run '/"), "{launch}");
    assert!(
        launch.contains(&format!(
            "push --store '{}' --house {} --task {}",
            world.fixture.store.dir().display(),
            world.fixture.store.house(),
            task
        )),
        "{launch}"
    );
    Ok(())
}

#[test]
fn supervising_an_adopted_worker_to_its_end_allows_the_replacement() -> TestResult {
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_live_worker(&world)?;
    // Before supervision, the adopted attempt still needs to be resumed.
    assert!(matches!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::SuperviseFirst { .. }
    ));
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Ready)
    );
    assert!(matches!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::Accepted { attempt, worker: current }
            if attempt.get() == 1 && current == worker
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
    // Supervision continues the adopted worker's attempt to stop it, and the
    // backend refuses. No attempt is started for the stop.
    world.backend.inject(ExecuteFault::Reject);
    assert_eq!(
        stalled_step(&world, &task, fence, &worker)?,
        Supervision::Escalate(Escalation::StopRefused)
    );
    assert_eq!(attempt_count(&world, &task)?, 1);
    let effects = world.backend.effects_performed();
    // The worker may still run, so no other writer launches beside it: the
    // continued attempt replays its own accepted launch of that worker.
    assert_eq!(
        launch(&world, &task, fence, 1)?,
        LaunchOutcome::Accepted {
            attempt: kitchen::contracts::AttemptNumber::new(1).ok_or("attempt")?,
            worker
        }
    );
    assert_eq!(world.backend.effects_performed(), effects);
    assert_eq!(attempt_count(&world, &task)?, 1);
    Ok(())
}

fn attempt_count(world: &World, task: &TaskId) -> TestResult<usize> {
    Ok(world.fixture.store.task(task)?.attempts().len())
}

#[test]
fn an_adopted_worker_on_its_last_attempt_settles_with_its_own_outcome() -> TestResult {
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_budget(&world, 1)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let good = completion("lemarier/issue-1", EvidenceVerdict::Pass)?;
    let outcome = supervise(
        &world.ctx(),
        &task,
        fence,
        &supervision()?,
        &SupervisionInput {
            completion: Some(&good),
            ..SupervisionInput::default()
        },
    )?;
    // The adopted worker's attempt records its success; no second attempt.
    assert_eq!(outcome, Supervision::Settled(Settlement::Succeeded));
    assert_eq!(attempt_count(&world, &task)?, 1);
    Ok(())
}

#[test]
fn an_adopted_idle_worker_on_its_last_attempt_is_stopped_before_the_task_settles() -> TestResult {
    use kitchen::contracts::WorkerBackend;
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_budget(&world, 1)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    world.clock.advance(500);
    let idle = kitchen::workflows::recovery::RecoverySignals {
        prompt: kitchen::workflows::recovery::PromptState::Idle,
        ..workflows_support::signals(&worker, Some(common::at(1)))
    };
    let outcome = supervise(
        &world.ctx(),
        &task,
        fence,
        &supervision()?,
        &SupervisionInput {
            signals: Some(&idle),
            ..SupervisionInput::default()
        },
    )?;
    assert_eq!(
        outcome,
        Supervision::IdleStopped {
            disposition: Disposition::Settled(Settlement::Exhausted)
        }
    );
    // The worker was stopped before the task settled, never left running.
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    assert_eq!(attempt_count(&world, &task)?, 1);
    Ok(())
}

#[test]
fn an_adopted_failure_keeps_the_exact_retry_budget() -> TestResult {
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_budget(&world, 3)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    // The adopted worker failed in attempt 1: two attempts are left, as
    // they would be without the hand-over.
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Retry { remaining: 2 }
    );
    assert_eq!(attempt_count(&world, &task)?, 1);
    let replacement = launched(&world, &task, fence, 1)?;
    assert_ne!(replacement, worker);
    assert_eq!(attempt_count(&world, &task)?, 2);
    Ok(())
}

#[test]
fn messages_reach_an_adopted_worker_without_starting_an_attempt() -> TestResult {
    use kitchen::workflows::coordination::{
        FollowUpRoute, Revalidation, retry_validation, send_follow_up,
    };
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_budget(&world, 1)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let request = kitchen::workflows::recovery::FollowUp {
        id: ExternalRef::new("review-1")?,
        body: Text::new("Rename the driver module.")?,
    };
    assert!(matches!(
        send_follow_up(&world.ctx(), &task, fence, &request)?,
        FollowUpRoute::Delivered { .. }
    ));
    assert_eq!(
        retry_validation(&world.ctx(), &task, fence)?,
        Revalidation::Sent
    );
    assert_eq!(attempt_count(&world, &task)?, 1);
    Ok(())
}

#[test]
fn a_parked_adopted_worker_resumes_when_the_provider_works() -> TestResult {
    use kitchen::workflows::recovery::{ProviderCheck, ProviderInterruption, RecoverySignals};
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_budget(&world, 1)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let refused = RecoverySignals {
        provider: Some(ProviderInterruption::Quota),
        ..workflows_support::signals(&worker, Some(common::at(1)))
    };
    let tick = |provider| {
        supervise(
            &world.ctx(),
            &task,
            fence,
            &supervision()?,
            &SupervisionInput {
                signals: Some(&refused),
                provider,
                ..SupervisionInput::default()
            },
        )
        .map_err(Into::into)
    };
    let parked: TestResult<Supervision> = tick(ProviderCheck::NotChecked);
    assert_eq!(
        parked?,
        Supervision::Parked {
            interruption: ProviderInterruption::Quota,
            report: true
        }
    );
    let resumed: TestResult<Supervision> = tick(ProviderCheck::Working);
    assert_eq!(resumed?, Supervision::Resumed);
    assert_eq!(attempt_count(&world, &task)?, 1);
    Ok(())
}

#[test]
fn provider_recovery_does_not_resume_an_adopted_worker_still_starting() -> TestResult {
    use kitchen::state::AttemptState;
    use kitchen::workflows::recovery::{ProviderCheck, ProviderInterruption, RecoverySignals};
    let world = World::new()?;
    let (task, fence, worker) = adopted_with_budget(&world, 1)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Starting);
    let refused = RecoverySignals {
        provider: Some(ProviderInterruption::Quota),
        ..workflows_support::signals(&worker, Some(common::at(1)))
    };
    let effects = world.backend.effects_performed();
    let outcome = supervise(
        &world.ctx(),
        &task,
        fence,
        &supervision()?,
        &SupervisionInput {
            signals: Some(&refused),
            provider: ProviderCheck::Working,
            ..SupervisionInput::default()
        },
    )?;
    assert_eq!(outcome, Supervision::Running(WorkerState::Starting));
    assert_eq!(world.backend.effects_performed(), effects);
    assert!(matches!(
        world
            .fixture
            .store
            .task(&task)?
            .attempts()
            .last()
            .map(|attempt| attempt.state()),
        Some(AttemptState::Interrupted { .. })
    ));
    Ok(())
}

fn claim_with_policy(world: &World, number: u64) -> TestResult<(TaskId, Fence)> {
    let (claimant, _) = under_consumer(world, "coordinator")?;
    let mut template = template()?;
    template.agents = Some(workflows_support::agent_policy()?);
    match claim_issue(
        &world.fixture.store,
        &template,
        &issue(number)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )? {
        ClaimOutcome::Claimed(lease) => Ok((issue_task_id(&issue(number)?)?, lease.fence())),
        other => Err(format!("unexpected claim outcome {other:?}").into()),
    }
}

fn codex_model() -> TestResult<AgentSelection> {
    Ok(AgentSelection {
        agent: AgentFamily::Codex,
        model: Some(AgentModel::new("gpt-6-sol")?),
        effort: None,
    })
}

#[test]
fn a_pickup_launch_carries_the_tasks_resolved_selection() -> TestResult {
    let mut world = World::new()?;
    world.backend = FakeBackend::new(
        common::backend_id()?,
        common::house()?,
        CapabilitySet::supporting(Capability::ALL),
    )
    .with_worker_selection(SelectionSupport {
        families: &[AgentFamily::Claude, AgentFamily::Codex],
        model: true,
        effort: EffortSupport::WithModel,
    });
    let (task, fence) = claim_with_policy(&world, 1)?;
    let record = world.fixture.store.task(&task)?;
    assert_eq!(
        record
            .spec()
            .agent
            .as_ref()
            .map(|resolved| &resolved.source),
        Some(&SelectionSource::Repository {
            repository: workflows_support::repo()?
        })
    );
    launched(&world, &task, fence, 1)?;
    assert_eq!(world.backend.launched_agents(), vec![Some(codex_model()?)]);
    Ok(())
}

#[test]
fn a_pickup_launch_is_refused_when_the_executor_lacks_the_selection() -> TestResult {
    // The default fake declares no selection support at all.
    let world = World::new()?;
    let (task, fence) = claim_with_policy(&world, 1)?;
    let error = launch(&world, &task, fence, 1)
        .err()
        .ok_or("a launch ran on an executor that cannot honor the selection")?;
    assert!(matches!(
        error.downcast_ref::<kitchen::Error>(),
        Some(kitchen::Error::Contract(
            ContractError::UnsupportedCapabilities { .. }
        ))
    ));
    assert_eq!(world.backend.effects_performed(), 0);
    assert_eq!(world.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn an_unsupported_selection_never_spends_an_attempt_over_many_ticks() -> TestResult {
    // More ticks than the budget of three: none may open an attempt.
    let world = World::new()?;
    let (task, fence) = claim_with_policy(&world, 1)?;
    for _ in 0..5 {
        let error = launch(&world, &task, fence, 1)
            .err()
            .ok_or("a launch ran on an executor that cannot honor the selection")?;
        assert!(matches!(
            error.downcast_ref::<kitchen::Error>(),
            Some(kitchen::Error::Contract(
                ContractError::UnsupportedCapabilities { missing, .. }
            )) if missing.contains(&Capability::AgentSelectFamily)
        ));
    }
    let record = world.fixture.store.task(&task)?;
    assert!(record.attempts().is_empty());
    assert!(matches!(record.state(), TaskState::Claimed { .. }));
    assert_eq!(world.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn a_partly_supported_selection_is_refused_before_an_attempt() -> TestResult {
    // The family is launchable but its model is not.
    let mut world = World::new()?;
    world.backend = FakeBackend::new(
        common::backend_id()?,
        common::house()?,
        CapabilitySet::supporting(Capability::ALL),
    )
    .with_worker_selection(SelectionSupport {
        families: &[AgentFamily::Claude, AgentFamily::Codex],
        model: false,
        effort: EffortSupport::Unsupported,
    });
    let (task, fence) = claim_with_policy(&world, 1)?;
    let error = launch(&world, &task, fence, 1)
        .err()
        .ok_or("a launch ran without the selection's model")?;
    assert!(matches!(
        error.downcast_ref::<kitchen::Error>(),
        Some(kitchen::Error::Contract(
            ContractError::UnsupportedCapabilities { missing, .. }
        )) if missing == &[Capability::AgentSelectModel]
    ));
    assert_eq!(attempt_count(&world, &task)?, 0);
    assert_eq!(world.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn a_refused_selection_launches_on_its_first_attempt_once_supported() -> TestResult {
    let mut world = World::new()?;
    let (task, fence) = claim_with_policy(&world, 1)?;
    for _ in 0..3 {
        assert!(launch(&world, &task, fence, 1).is_err());
    }
    world.backend = FakeBackend::new(
        common::backend_id()?,
        common::house()?,
        CapabilitySet::supporting(Capability::ALL),
    )
    .with_worker_selection(SelectionSupport {
        families: &[AgentFamily::Codex],
        model: true,
        effort: EffortSupport::WithModel,
    });
    match launch(&world, &task, fence, 1)? {
        LaunchOutcome::Accepted { attempt, .. } => assert_eq!(attempt.get(), 1),
        other => return Err(format!("launch not accepted: {other:?}").into()),
    }
    assert_eq!(world.backend.launched_agents(), vec![Some(codex_model()?)]);
    Ok(())
}

#[test]
fn a_pickup_without_a_policy_launches_the_backend_default() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    assert_eq!(world.fixture.store.task(&task)?.spec().agent, None);
    launched(&world, &task, fence, 1)?;
    assert_eq!(world.backend.launched_agents(), vec![None]);
    Ok(())
}

fn person(body: &str) -> TestResult<Response> {
    Ok(Response::Answer {
        body: Text::new(body)?,
        source: AnswerSource::Person,
    })
}

/// The replies recorded on the task's latest attempt.
fn replies(world: &World, task: &TaskId) -> TestResult<Vec<kitchen::state::HumanReply>> {
    Ok(world
        .fixture
        .store
        .task(task)?
        .attempts()
        .last()
        .ok_or("no attempt")?
        .replies()
        .to_vec())
}

#[test]
fn a_persons_answer_records_its_reply_latency_once_delivered() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    let asked = question("msg-person", 1_000)?;
    world.clock.advance(90);
    let answer = person("Use the existing SPI helper.")?;
    let policy = supervision()?;
    let handle = || handle_question(&world.ctx(), &task, fence, &policy, &asked, &answer, None);
    assert_eq!(handle()?, QuestionRoute::Replied);
    let recorded = replies(&world, &task)?;
    let [reply] = recorded.as_slice() else {
        return Err(format!("expected one reply, got {recorded:?}").into());
    };
    assert_eq!(reply.question, asked.id);
    assert_eq!(reply.answered_at, common::at(1_090));
    assert_eq!(reply.latency(), std::time::Duration::from_secs(90));
    let usage = world.fixture.store.attempt_usage()?;
    assert_eq!(
        usage.first().map(|entry| entry.human.replies),
        Some(std::time::Duration::from_secs(90))
    );

    // A repeated tick delivers nothing and records nothing more.
    world.clock.advance(30);
    assert_eq!(handle()?, QuestionRoute::Duplicate);
    assert_eq!(replies(&world, &task)?, recorded);
    assert_eq!(world.backend.effects_performed(), 2);
    Ok(())
}

#[test]
fn a_coordinator_answer_records_no_person_time() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    let asked = question("msg-coordinator", 1_000)?;
    let answer = Response::Answer {
        body: Text::new("Proceed.")?,
        source: AnswerSource::Coordinator,
    };
    let policy = supervision()?;
    let handle = |response: &Response| {
        handle_question(&world.ctx(), &task, fence, &policy, &asked, response, None)
    };
    assert_eq!(handle(&answer)?, QuestionRoute::Replied);
    assert!(replies(&world, &task)?.is_empty());
    // Repeating the delivered answer as a person's cannot relabel it: the
    // source recorded with the delivery wins, whatever the body.
    world.clock.advance(30);
    for body in ["Proceed.", "A different answer."] {
        assert_eq!(handle(&person(body)?)?, QuestionRoute::Duplicate);
        assert!(replies(&world, &task)?.is_empty());
    }
    assert_eq!(world.backend.effects_performed(), 2);
    let usage = world.fixture.store.attempt_usage()?;
    assert_eq!(
        usage.first().map(|entry| entry.human.replies),
        Some(std::time::Duration::ZERO)
    );
    Ok(())
}

#[test]
fn a_reply_that_did_not_apply_records_nothing() -> TestResult {
    let policy = supervision()?;
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    launched(&world, &task, fence, 1)?;
    let asked = question("msg-rejected", 1_000)?;
    world.backend.inject(ExecuteFault::Reject);
    assert_eq!(
        handle_question(
            &world.ctx(),
            &task,
            fence,
            &policy,
            &asked,
            &person("Use the existing SPI helper.")?,
            None
        )?,
        QuestionRoute::Escalate(QuestionEscalation::NotApplied)
    );
    assert!(replies(&world, &task)?.is_empty());
    Ok(())
}

#[test]
fn an_uncertain_reply_is_recorded_only_once_reconciled_as_applied() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    let asked = question("msg-uncertain", 1_000)?;
    let answer = person("Use the existing SPI helper.")?;
    let policy = supervision()?;
    let handle = || handle_question(&world.ctx(), &task, fence, &policy, &asked, &answer, None);
    world.clock.advance(20);
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(handle()?, QuestionRoute::Uncertain);
    assert!(replies(&world, &task)?.is_empty());

    // Reconciliation finds the reply applied; the repeated question then
    // records it, dated when delivery applied, not when it was re-read.
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    world.clock.advance(40);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Ready)
    );
    world.clock.advance(40);
    assert_eq!(handle()?, QuestionRoute::Duplicate);
    let recorded = replies(&world, &task)?;
    let [reply] = recorded.as_slice() else {
        return Err(format!("expected one reply, got {recorded:?}").into());
    };
    let applied_at = world
        .fixture
        .store
        .task(&task)?
        .effects()
        .iter()
        .find_map(|effect| match effect.state() {
            kitchen::state::EffectState::Applied { at, .. }
                if effect.name().as_str().starts_with("reply-") =>
            {
                Some(*at)
            }
            _ => None,
        })
        .ok_or("reply never applied")?;
    assert_eq!(reply.answered_at, applied_at);
    assert!(reply.answered_at < world.now());
    assert_eq!(handle()?, QuestionRoute::Duplicate);
    assert_eq!(replies(&world, &task)?.len(), 1);
    Ok(())
}

#[test]
fn a_restart_records_a_persons_reply_from_its_recorded_source() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 3)?;
    let worker = launched(&world, &task, fence, 1)?;
    let asked = question("msg-recovered", 1_000)?;
    let policy = supervision()?;
    world.clock.advance(20);
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(
        handle_question(
            &world.ctx(),
            &task,
            fence,
            &policy,
            &asked,
            &person("Use the existing SPI helper.")?,
            None
        )?,
        QuestionRoute::Uncertain
    );
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Running(WorkerState::Ready)
    );
    // The restarted caller no longer knows who answered; the person's
    // reply is still recorded from the source stored with its delivery.
    let pending = Response::Pending;
    assert_eq!(
        handle_question(&world.ctx(), &task, fence, &policy, &asked, &pending, None)?,
        QuestionRoute::Duplicate
    );
    assert_eq!(replies(&world, &task)?.len(), 1);
    Ok(())
}

/// A person's reply that applied, but whose recording was lost: the reply's
/// outcome was lost, then reconciliation found it applied, and the
/// coordinator stopped before the question was handled again. The worker
/// then fails, ending attempt 1.
fn reply_applied_then_attempt_failed(
    world: &World,
    attempts: u32,
) -> TestResult<(TaskId, Fence, Supervision)> {
    let (task, fence) = claim(world, "coordinator", 1, attempts)?;
    let worker = launched(world, &task, fence, 1)?;
    world.clock.advance(20);
    world.backend.inject(ExecuteFault::ApplyThenLoseResponse);
    assert_eq!(
        handle_question(
            &world.ctx(),
            &task,
            fence,
            &supervision()?,
            &question("msg-lost", 1_000)?,
            &person("Use the existing SPI helper.")?,
            None
        )?,
        QuestionRoute::Uncertain
    );
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    assert_eq!(
        step(world, &task, fence)?,
        Supervision::Running(WorkerState::Ready)
    );
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    let ended = step(world, &task, fence)?;
    assert!(replies(world, &task)?.is_empty());
    Ok((task, fence, ended))
}

/// The replies on each of the task's attempts, oldest attempt first.
fn replies_by_attempt(
    world: &World,
    task: &TaskId,
) -> TestResult<Vec<Vec<kitchen::state::HumanReply>>> {
    Ok(world
        .fixture
        .store
        .task(task)?
        .attempts()
        .iter()
        .map(|attempt| attempt.replies().to_vec())
        .collect())
}

#[test]
fn a_replayed_reply_is_recorded_on_its_ended_attempt_once() -> TestResult {
    let world = World::new()?;
    let (task, fence, ended) = reply_applied_then_attempt_failed(&world, 2)?;
    assert_eq!(ended, Supervision::Retry { remaining: 1 });
    launched(&world, &task, fence, 1)?;
    world.clock.advance(60);
    // The replay knows neither who answered nor, exactly, when the question
    // was asked; both come from the delivered reply.
    let replayed = question("msg-lost", 1_010)?;
    let policy = supervision()?;
    for _ in 0..2 {
        assert_eq!(
            handle_question(
                &world.ctx(),
                &task,
                fence,
                &policy,
                &replayed,
                &Response::Pending,
                None
            )?,
            QuestionRoute::Duplicate
        );
    }
    let recorded = replies_by_attempt(&world, &task)?;
    let [first, second] = recorded.as_slice() else {
        return Err(format!("expected two attempts, got {recorded:?}").into());
    };
    let [reply] = first.as_slice() else {
        return Err(format!("expected one reply on attempt 1, got {first:?}").into());
    };
    assert_eq!(reply.question.as_str(), "msg-lost");
    assert_eq!(reply.asked_at, common::at(1_000));
    assert_eq!(reply.answered_at, common::at(1_020));
    assert!(second.is_empty());
    assert_eq!(world.backend.effects_performed(), 3);
    Ok(())
}

#[test]
fn a_replayed_reply_is_recorded_after_its_attempt_settled_the_task() -> TestResult {
    let world = World::new()?;
    let (task, fence, ended) = reply_applied_then_attempt_failed(&world, 1)?;
    assert_eq!(ended, Supervision::Settled(Settlement::Exhausted));
    let policy = supervision()?;
    let replayed = question("msg-lost", 1_000)?;
    for _ in 0..2 {
        assert_eq!(
            handle_question(
                &world.ctx(),
                &task,
                fence,
                &policy,
                &replayed,
                &Response::Pending,
                None
            )?,
            QuestionRoute::Duplicate
        );
    }
    let recorded = replies_by_attempt(&world, &task)?;
    assert_eq!(recorded.iter().map(Vec::len).collect::<Vec<_>>(), [1]);
    Ok(())
}

#[test]
fn an_adopted_task_does_not_record_a_persons_reply_twice() -> TestResult {
    let policy = supervision()?;
    let world = World::new()?;
    let (task, fence, _worker) = adopted_with_live_worker(&world)?;
    let asked = question("msg-adopted", 1_000)?;
    let answer = person("Use the existing SPI helper.")?;
    assert_eq!(
        handle_question(&world.ctx(), &task, fence, &policy, &asked, &answer, None)?,
        QuestionRoute::Replied
    );
    // A restarted coordinator repeats the same answer under the same fence.
    world.clock.advance(10);
    assert_eq!(
        handle_question(&world.ctx(), &task, fence, &policy, &asked, &answer, None)?,
        QuestionRoute::Duplicate
    );
    let entries = world.fixture.store.attempt_usage()?;
    let replies: usize = world
        .fixture
        .store
        .task(&task)?
        .attempts()
        .iter()
        .map(|attempt| attempt.replies().len())
        .sum();
    assert_eq!(replies, 1);
    assert_eq!(entries.len(), 1);
    Ok(())
}

fn usage_report(worker: &ResourceRef) -> kitchen::state::UsageReport {
    kitchen::state::UsageReport {
        source: worker.handle.clone(),
        agent: Some(AgentFamily::Claude),
        model: AgentModel::new("claude-opus-5-5").ok(),
        tokens: kitchen::state::TokenCounts {
            input: Some(1_200),
            output: Some(800),
            cache_read: None,
            cache_write: None,
        },
        cost: None,
    }
}

fn attempt_usage(world: &World, task: &TaskId) -> TestResult<Vec<kitchen::state::AttemptUsage>> {
    Ok(world
        .fixture
        .store
        .task(task)?
        .attempts()
        .iter()
        .map(|attempt| attempt.usage().clone())
        .collect())
}

#[test]
fn a_usage_report_is_recorded_on_the_ended_attempt_once() -> TestResult {
    use kitchen::state::AttemptUsage;
    use kitchen::workflows::coordination::{UsageRoute, record_worker_usage};
    let world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 1)?;
    let worker = launched(&world, &task, fence, 1)?;
    // While the attempt runs, a report is not recorded.
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &worker, usage_report(&worker))?,
        UsageRoute::AttemptOpen
    );
    assert_eq!(
        attempt_usage(&world, &task)?,
        vec![AttemptUsage::NotReported]
    );

    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Settled(Settlement::Exhausted)
    );
    // The settling owner records the report after settlement; repeating it
    // after a restart changes nothing.
    let attempt = kitchen::contracts::AttemptNumber::FIRST;
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &worker, usage_report(&worker))?,
        UsageRoute::Recorded(attempt)
    );
    world.clock.advance(60);
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &worker, usage_report(&worker))?,
        UsageRoute::Recorded(attempt)
    );
    let usage = attempt_usage(&world, &task)?;
    let [AttemptUsage::Reported { report, at, .. }] = usage.as_slice() else {
        return Err(format!("expected one report, got {usage:?}").into());
    };
    assert_eq!(report, &usage_report(&worker));
    assert_eq!(*at, common::at(1_000));

    // A different report for the same attempt is refused, not overwritten.
    let mut other = usage_report(&worker);
    other.tokens.output = Some(900);
    let refused = record_worker_usage(&world.ctx(), &task, fence, &worker, other);
    assert!(
        matches!(
            refused,
            Err(kitchen::Error::Usage(
                kitchen::state::UsageError::AlreadyReported(_)
            ))
        ),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn a_late_report_lands_on_its_own_attempt_after_a_retry() -> TestResult {
    use kitchen::contracts::AttemptNumber;
    use kitchen::state::AttemptUsage;
    use kitchen::workflows::coordination::{UsageRoute, record_worker_usage};
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
    let second = launched(&world, &task, fence, 1)?;

    // Attempt 1's report arrives while attempt 2 runs.
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &first, usage_report(&first))?,
        UsageRoute::Recorded(AttemptNumber::FIRST)
    );
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &second, usage_report(&second))?,
        UsageRoute::AttemptOpen
    );
    assert_eq!(
        attempt_usage(&world, &task)?,
        vec![
            AttemptUsage::Reported {
                backend: common::backend_id()?,
                report: usage_report(&first),
                at: world.now(),
            },
            AttemptUsage::NotReported,
        ]
    );

    // Once attempt 2 ends, its own report lands on it; attempt 1 keeps its.
    world
        .backend
        .set_worker_state(&second, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Settled(Settlement::Exhausted)
    );
    let second_attempt = world
        .fixture
        .store
        .task(&task)?
        .attempts()
        .last()
        .map(kitchen::state::AttemptRecord::number)
        .ok_or("no attempt")?;
    assert_eq!(second_attempt.get(), 2);
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &second, usage_report(&second))?,
        UsageRoute::Recorded(second_attempt)
    );
    let usage = attempt_usage(&world, &task)?;
    let reports: Vec<_> = usage
        .iter()
        .map(|entry| match entry {
            AttemptUsage::Reported { report, .. } => Some(report.source.clone()),
            AttemptUsage::NotReported => None,
        })
        .collect();
    assert_eq!(
        reports,
        vec![Some(first.handle.clone()), Some(second.handle.clone())]
    );
    Ok(())
}

#[test]
fn a_report_for_an_unknown_worker_or_another_backend_is_not_recorded() -> TestResult {
    use kitchen::state::AttemptUsage;
    use kitchen::workflows::coordination::{UsageRoute, record_worker_usage};
    let mut world = World::new()?;
    let (task, fence) = claim(&world, "coordinator", 1, 2)?;
    let first = launched(&world, &task, fence, 1)?;
    world
        .backend
        .set_worker_state(&first, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Retry { remaining: 1 }
    );
    launched(&world, &task, fence, 1)?;

    // A worker this task never launched is unknown.
    let stranger = ResourceRef {
        handle: ExternalRef::new("worker-not-launched")?,
        ..first.clone()
    };
    assert_eq!(
        record_worker_usage(
            &world.ctx(),
            &task,
            fence,
            &stranger,
            usage_report(&stranger)
        )?,
        UsageRoute::UnknownWorker
    );
    // A worker named with another backend is not this task's worker.
    let foreign = ResourceRef {
        backend: kitchen::BackendId::new("other-backend")?,
        ..first.clone()
    };
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &foreign, usage_report(&first))?,
        UsageRoute::UnknownWorker
    );
    // Another backend cannot report for this backend's worker.
    world.backend = FakeBackend::new(
        kitchen::BackendId::new("other-backend")?,
        common::house()?,
        CapabilitySet::supporting(Capability::ALL),
    );
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &first, usage_report(&first))?,
        UsageRoute::BackendMismatch
    );
    assert_eq!(
        attempt_usage(&world, &task)?,
        vec![AttemptUsage::NotReported, AttemptUsage::NotReported]
    );
    Ok(())
}

#[test]
fn a_report_keeps_its_run_id_source_on_the_named_workers_attempt() -> TestResult {
    use kitchen::contracts::AttemptNumber;
    use kitchen::state::AttemptUsage;
    use kitchen::workflows::coordination::{UsageRoute, record_worker_usage};
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
    launched(&world, &task, fence, 1)?;
    // The backend reports under its own run ID, not the worker's handle;
    // the caller names the worker it launched.
    let mut report = usage_report(&first);
    report.source = ExternalRef::new("orca-run:42")?;
    assert_ne!(report.source, first.handle);
    assert_eq!(
        record_worker_usage(&world.ctx(), &task, fence, &first, report.clone())?,
        UsageRoute::Recorded(AttemptNumber::FIRST)
    );
    assert_eq!(
        attempt_usage(&world, &task)?,
        vec![
            AttemptUsage::Reported {
                backend: common::backend_id()?,
                report,
                at: world.now(),
            },
            AttemptUsage::NotReported,
        ]
    );
    Ok(())
}

#[test]
fn without_a_usage_report_or_capability_the_attempt_stays_not_reported() -> TestResult {
    use kitchen::state::AttemptUsage;
    use kitchen::workflows::coordination::record_worker_usage;
    let capabilities = CapabilitySet::supporting(
        Capability::ALL
            .into_iter()
            .filter(|capability| *capability != Capability::UsageAttribution),
    );
    let world = World::with_capabilities(capabilities)?;
    let (task, fence) = claim(&world, "coordinator", 1, 1)?;
    let worker = launched(&world, &task, fence, 1)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    assert_eq!(
        step(&world, &task, fence)?,
        Supervision::Settled(Settlement::Exhausted)
    );
    // No report was returned: nothing is called and the attempt stays
    // not reported.
    assert_eq!(
        attempt_usage(&world, &task)?,
        vec![AttemptUsage::NotReported]
    );
    // A backend without usage attribution cannot report.
    let refused = record_worker_usage(&world.ctx(), &task, fence, &worker, usage_report(&worker));
    assert!(
        matches!(
            refused,
            Err(kitchen::Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. }))
                if missing == &[Capability::UsageAttribution]
        ),
        "{refused:?}"
    );
    assert_eq!(
        attempt_usage(&world, &task)?,
        vec![AttemptUsage::NotReported]
    );
    Ok(())
}
