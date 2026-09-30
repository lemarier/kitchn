//! The house mailbox: worker posts kept in the house store, the fenced
//! coordinator mailbox over them, answers, bounds, retention, and
//! coordination on a backend without worker deliveries. Everything runs on
//! temporary stores and the in-memory fake backend: simulated evidence, not
//! live runtime evidence.

mod common;
mod workflows_support;

use std::time::Duration;

use common::{TestResult, ttl};
use kitchen::{
    ErrorClass, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, BackendUnavailable, Capability, CapabilitySet,
        ContractError, CoordinatorMailbox, Delivery, Effect, EffectExecutor, ExternalRef,
        FailureClass, Fence, MailboxError, MessageKind, Operation, ResourceRef, Text,
        WorkerOutcome, Workspace,
        conformance::{self, Check, CheckResult},
        fake::FakeBackend,
    },
    state::{
        AnswerState, Answered, Answerer, HouseMailbox, Inventory, MAX_MAIL_BATCH,
        MAX_MAIL_BODY_BYTES, MAX_MAIL_PER_TASK, MAX_UNACKNOWLEDGED_PER_TASK, MailAnswer, MailError,
        MailRetirement, MailSender, PostKind, ReportedOutcome, RetentionPolicy, StateError,
        WorkerPost,
    },
    workflows::{
        coordination::{
            AnswerSource, CoordinatorStart, LaunchOutcome, MailboxRoute, QuestionRoute, Response,
            WorkerQuestion, handle_question, launch_worker, relinquish_coordinator,
            start_coordinator,
        },
        pickup::{ClaimOutcome, TaskTemplate, claim_issue, issue_task_id},
    },
};
use workflows_support::{World, brief, consumer, issue, supervision, template, under_consumer};

/// A fake worker backend that carries no worker deliveries and no run
/// transfer, as a backend without threaded messages.
fn world() -> TestResult<World> {
    World::with_capabilities(CapabilitySet::supporting(
        Capability::ALL.into_iter().filter(|capability| {
            !matches!(
                capability,
                Capability::WorkerDeliveries | Capability::RunTransfer
            )
        }),
    ))
}

/// Tasks record what the house route needs of the worker backend.
fn house_template(world: &World) -> TestResult<TaskTemplate> {
    let route = MailboxRoute::select(world.backend.descriptor());
    assert_eq!(route, MailboxRoute::House);
    let mut template = template()?;
    template.requires = kitchen::contracts::CapabilityRequirements::new().with(
        kitchen::contracts::ExecutorKind::Worker,
        route.worker_requirements().iter().copied(),
    );
    Ok(template)
}

struct Launched {
    task: TaskId,
    fence: Fence,
    consumer_fence: Fence,
    worker: ResourceRef,
}

/// A coordinator's consumer lease, as its claimant and fence.
fn coordinator(world: &World) -> TestResult<(kitchen::contracts::Claimant, Fence)> {
    let (claimant, lease) = under_consumer(world, "coordinator")?;
    Ok((claimant, lease.fence()))
}

/// Claim issue `number` under a fresh coordinator lease and launch its worker.
fn launched(world: &World, number: u64) -> TestResult<Launched> {
    let coordinator = coordinator(world)?;
    launched_under(world, &coordinator, number)
}

/// Claim issue `number` under `coordinator` and launch its worker.
fn launched_under(
    world: &World,
    (claimant, consumer_fence): &(kitchen::contracts::Claimant, Fence),
    number: u64,
) -> TestResult<Launched> {
    let ClaimOutcome::Claimed(claim) = claim_issue(
        &world.fixture.store,
        &house_template(world)?,
        &issue(number)?,
        claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("issue not claimed".into());
    };
    let task = issue_task_id(&issue(number)?)?;
    let LaunchOutcome::Accepted { worker, .. } = launch_worker(
        &world.ctx(),
        &task,
        claim.fence(),
        Workspace::Isolated,
        &brief(number)?,
    )?
    else {
        return Err("launch not accepted".into());
    };
    Ok(Launched {
        task,
        fence: claim.fence(),
        consumer_fence: *consumer_fence,
        worker,
    })
}

fn first_attempt() -> TestResult<AttemptNumber> {
    Ok(AttemptNumber::new(1).ok_or("attempt number")?)
}

fn sender(launched: &Launched) -> MailSender {
    MailSender::new(launched.task.clone(), launched.fence.get())
}

fn post(kind: PostKind, body: &str) -> TestResult<WorkerPost> {
    Ok(WorkerPost {
        kind,
        subject: None,
        body: Text::new(body)?,
    })
}

fn mailbox<'a>(world: &'a World, fence: Fence) -> TestResult<HouseMailbox<'a>> {
    Ok(HouseMailbox::new(
        &world.fixture.store,
        &world.backend,
        &world.clock,
        consumer()?,
        fence,
    )?)
}

fn ids(delivery: &Delivery) -> Vec<ExternalRef> {
    delivery
        .messages
        .iter()
        .map(|message| message.id.clone())
        .collect()
}

fn mail_error(error: &kitchen::Error) -> Option<MailError> {
    match error {
        kitchen::Error::Mail(error) => Some(*error),
        _ => None,
    }
}

#[test]
fn the_house_mailbox_conforms_with_adoption_fencing() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    let first = mailbox(&world, work.consumer_fence)?;
    first.adopt_run()?;
    let sent = [
        post(PostKind::Question, "Which driver revision?")?,
        post(
            PostKind::Report {
                outcome: ReportedOutcome::Succeeded,
            },
            "Done.",
        )?,
        post(PostKind::Escalation, "The base moved.")?,
    ]
    .into_iter()
    .map(|post| {
        world
            .fixture
            .store
            .post_mail(&sender(&work), post, world.now())
    })
    .collect::<Result<Vec<_>, _>>()?;
    // The first coordinator hands over; the next one adopts its scope.
    relinquish_coordinator(
        &world.fixture.store,
        &consumer()?,
        work.consumer_fence,
        world.now(),
    )?;
    let CoordinatorStart::Adopted { lease, .. } = start_coordinator(
        &world.fixture.store,
        world.backend.descriptor(),
        &consumer()?,
        &common::scheduled("coordinator-2")?,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("the scope was not adopted".into());
    };
    let restarted = mailbox(&world, lease.fence())?;
    let report = conformance::run_mailbox(&first, &restarted, &sent)?;
    for check in [
        Check::DeliveriesDeclared,
        Check::DeliveryReplayed,
        Check::AdoptionReplays,
        Check::DuplicateAcknowledgement,
        Check::DeliveryOrder,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
    // A coordinator with an older lease cannot take the mailbox back.
    assert_eq!(first.adopt_run(), Err(MailboxError::Fenced));
    Ok(())
}

#[test]
fn a_restart_between_post_and_acknowledgement_loses_nothing() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    let question = world.fixture.store.post_mail(
        &sender(&work),
        post(PostKind::Question, "May I rename the module?")?,
        world.now(),
    )?;
    let crashed = mailbox(&world, work.consumer_fence)?;
    let read = crashed.next_delivery()?.ok_or("a batch")?;
    assert_eq!(ids(&read), std::slice::from_ref(&question));
    assert_eq!(
        read.messages
            .first()
            .and_then(|message| message.worker.as_ref()),
        Some(&work.worker)
    );
    // The coordinator stops before acknowledging; its lease expires and a
    // new process takes the scope over.
    world.clock.advance(601);
    let reopened = world.fixture.reopen()?;
    let taken = reopened.take_over_consumer(
        &consumer()?,
        &common::scheduled("coordinator-2")?,
        ttl(600)?,
        world.now(),
    )?;
    let adopter = HouseMailbox::new(
        &reopened,
        &world.backend,
        &world.clock,
        consumer()?,
        taken.fence(),
    )?;
    adopter.adopt_run()?;
    let adopted = adopter.next_delivery()?.ok_or("the adopter got nothing")?;
    assert_eq!(ids(&adopted), [question]);
    assert_ne!(
        adopted.id, read.id,
        "adoption regroups under a new batch id"
    );
    assert_eq!(crashed.acknowledge(&read.id), Err(MailboxError::Fenced));
    assert_eq!(crashed.next_delivery(), Err(MailboxError::Fenced));
    // The stale id consumes nothing; the adopter's own id does.
    assert_eq!(adopter.acknowledge(&read.id)?, Some(adopted.clone()));
    assert_eq!(adopter.acknowledge(&adopted.id)?, None);
    assert_eq!(adopter.await_delivery(Duration::ZERO)?, None);
    Ok(())
}

#[test]
fn a_first_reader_needs_a_live_lease() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    // Nobody reads yet, and this lease has expired: it cannot register.
    world.clock.advance(601);
    let expired = mailbox(&world, work.consumer_fence)?;
    assert_eq!(expired.next_delivery(), Err(MailboxError::Fenced));
    Ok(())
}

#[test]
fn workers_post_and_read_only_for_their_own_task_and_attempt() -> TestResult {
    let world = world()?;
    let coordinator = coordinator(&world)?;
    let one = launched_under(&world, &coordinator, 1)?;
    let two = launched_under(&world, &coordinator, 2)?;
    let store = &world.fixture.store;
    let asked = store.post_mail(
        &sender(&one),
        post(PostKind::Question, "Why?")?,
        world.now(),
    )?;

    // Another task's fence, and a fence the task never issued.
    for fence in [two.fence.get(), one.fence.get() + 1_000] {
        let error = store
            .post_mail(
                &MailSender::new(one.task.clone(), fence),
                post(PostKind::Escalation, "stuck")?,
                world.now(),
            )
            .err()
            .ok_or("a foreign fence posted")?;
        assert_eq!(mail_error(&error), Some(MailError::NotSender), "{fence}");
        assert_eq!(error.class(), ErrorClass::Refused);
    }
    // Task two's worker cannot read task one's question, even by its id.
    let error = store
        .mail_answer(&sender(&two), &asked)
        .err()
        .ok_or("another task read the question")?;
    assert_eq!(mail_error(&error), Some(MailError::UnknownMessage));
    // A task this house's store does not hold.
    let error = store
        .post_mail(
            &MailSender::new(common::task_id("other-house-task")?, one.fence.get()),
            post(PostKind::Question, "Hello?")?,
            world.now(),
        )
        .err()
        .ok_or("an unknown task posted")?;
    assert!(matches!(
        error,
        kitchen::Error::State(StateError::TaskNotFound(_))
    ));
    // Once the attempt ends, its worker can no longer post or read.
    store.finish_attempt(
        &one.task,
        one.fence,
        first_attempt()?,
        AttemptOutcome::Failed(FailureClass::Retryable),
        world.now(),
    )?;
    let error = store
        .mail_answer(&sender(&one), &asked)
        .err()
        .ok_or("an ended attempt read its answer")?;
    assert_eq!(mail_error(&error), Some(MailError::NoOpenAttempt));
    Ok(())
}

#[test]
fn an_adopted_attempts_worker_keeps_posting_with_its_launch_fence() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    relinquish_coordinator(
        &world.fixture.store,
        &consumer()?,
        work.consumer_fence,
        world.now(),
    )?;
    // While nobody owns the task the worker can still post.
    world.fixture.store.post_mail(
        &sender(&work),
        post(PostKind::Question, "Still there?")?,
        world.now(),
    )?;
    let CoordinatorStart::Adopted { tasks, .. } = start_coordinator(
        &world.fixture.store,
        world.backend.descriptor(),
        &consumer()?,
        &common::scheduled("coordinator-2")?,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("the scope was not adopted".into());
    };
    let (_, claim) = tasks.first().ok_or("the task was not adopted")?;
    world
        .fixture
        .store
        .continue_attempt(&work.task, claim.fence(), world.now())?;
    assert!(claim.fence() > work.fence);
    world.fixture.store.post_mail(
        &sender(&work),
        post(PostKind::Escalation, "Base moved.")?,
        world.now(),
    )?;
    // The adopter's newer fence is not the worker's, but it belongs to the
    // same attempt, so it is accepted too; an older one is not.
    world.fixture.store.post_mail(
        &MailSender::new(work.task.clone(), claim.fence().get()),
        post(PostKind::Escalation, "Again.")?,
        world.now(),
    )?;
    Ok(())
}

#[test]
fn the_house_mailbox_refuses_another_houses_backend() -> TestResult {
    let world = world()?;
    let (_, fence) = coordinator(&world)?;
    let foreign = FakeBackend::new(
        common::backend_id()?,
        common::other_house()?,
        CapabilitySet::supporting(Capability::ALL),
    );
    let error = HouseMailbox::new(
        &world.fixture.store,
        &foreign,
        &world.clock,
        consumer()?,
        fence,
    )
    .err()
    .ok_or("a foreign backend was accepted")?;
    assert!(matches!(
        error,
        kitchen::Error::Contract(ContractError::CrossHouse { .. })
    ));
    Ok(())
}

#[test]
fn posts_are_bounded_per_message_task_and_batch() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    let store = &world.fixture.store;
    let long = "x".repeat(MAX_MAIL_BODY_BYTES + 1);
    let error = store
        .post_mail(
            &sender(&work),
            post(PostKind::Question, &long)?,
            world.now(),
        )
        .err()
        .ok_or("an oversized body was stored")?;
    assert_eq!(mail_error(&error), Some(MailError::TooLarge));
    assert_eq!(error.class(), ErrorClass::InvalidInput);
    let exact = "x".repeat(MAX_MAIL_BODY_BYTES);
    store.post_mail(
        &sender(&work),
        post(PostKind::Escalation, &exact)?,
        world.now(),
    )?;
    for n in 1..MAX_UNACKNOWLEDGED_PER_TASK {
        store.post_mail(
            &sender(&work),
            post(PostKind::Escalation, &format!("note {n}"))?,
            world.now(),
        )?;
    }
    let error = store
        .post_mail(
            &sender(&work),
            post(PostKind::Escalation, "one more")?,
            world.now(),
        )
        .err()
        .ok_or("the task's waiting share overflowed")?;
    assert_eq!(mail_error(&error), Some(MailError::Full));

    // Batches hold at most MAX_MAIL_BATCH messages, oldest first.
    let reader = mailbox(&world, work.consumer_fence)?;
    let first = reader.next_delivery()?.ok_or("a batch")?;
    assert_eq!(first.messages.len(), MAX_MAIL_BATCH);
    let second = reader.acknowledge(&first.id)?.ok_or("a second batch")?;
    assert_eq!(
        second.messages.len(),
        MAX_UNACKNOWLEDGED_PER_TASK - MAX_MAIL_BATCH
    );
    assert_eq!(reader.acknowledge(&second.id)?, None);
    // Acknowledged messages still count toward the task's total.
    for n in MAX_UNACKNOWLEDGED_PER_TASK..MAX_MAIL_PER_TASK {
        store.post_mail(
            &sender(&work),
            post(PostKind::Escalation, &format!("late {n}"))?,
            world.now(),
        )?;
    }
    let error = store
        .post_mail(
            &sender(&work),
            post(PostKind::Escalation, "full")?,
            world.now(),
        )
        .err()
        .ok_or("the task's total overflowed")?;
    assert_eq!(mail_error(&error), Some(MailError::Full));
    assert_eq!(store.mailbox_usage()?.used, MAX_MAIL_PER_TASK);
    Ok(())
}

#[test]
fn retention_removes_acknowledged_mail_only_after_its_attempt_ended() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    let store = &world.fixture.store;
    let handled = store.post_mail(&sender(&work), post(PostKind::Question, "Q1")?, world.now())?;
    let reader = mailbox(&world, work.consumer_fence)?;
    let batch = reader.next_delivery()?.ok_or("a batch")?;
    reader.acknowledge(&batch.id)?;
    let waiting = store.post_mail(&sender(&work), post(PostKind::Question, "Q2")?, world.now())?;
    let policy = RetentionPolicy::default();
    // The attempt is open: its worker may still read the answer.
    let preview = store.preview_retention(&policy, &Inventory::new(), world.now())?;
    assert!(preview.mail.is_empty());

    store.finish_attempt(
        &work.task,
        work.fence,
        first_attempt()?,
        AttemptOutcome::Failed(FailureClass::Retryable),
        world.now(),
    )?;
    let preview = store.preview_retention(&policy, &Inventory::new(), world.now())?;
    assert_eq!(
        preview
            .mail
            .iter()
            .map(|retired| (retired.id.clone(), retired.reason))
            .collect::<Vec<_>>(),
        [(handled, MailRetirement::AttemptEnded)]
    );
    assert_eq!(store.mailbox_usage()?.used, 2, "a preview removes nothing");
    let applied = store.retain(
        &policy,
        &Inventory::new(),
        &common::scheduled("retention")?,
        world.now(),
    )?;
    assert!(applied.applied);
    assert_eq!(applied.mail.len(), 1);
    // The unacknowledged question stays for the coordinator.
    assert_eq!(store.mailbox_usage()?.used, 1);
    let next = reader.next_delivery()?.ok_or("the waiting question")?;
    assert_eq!(ids(&next), [waiting]);
    Ok(())
}

#[test]
fn answers_reach_only_the_asking_worker_and_record_a_persons_time() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    let store = &world.fixture.store;
    let asked_at = world.now();
    let question = store.post_mail(
        &sender(&work),
        post(PostKind::Question, "Which?")?,
        asked_at,
    )?;
    assert_eq!(
        store.mail_answer(&sender(&work), &question)?,
        AnswerState::Pending
    );
    assert_eq!(store.open_questions(10)?.len(), 1);

    world.clock.advance(120);
    let answer = MailAnswer {
        body: Text::new("Use revision B.")?,
        by: Answerer::Person,
        at: world.now(),
    };
    assert_eq!(
        store.answer_mail(&question, answer.clone())?,
        Answered::Recorded
    );
    assert_eq!(
        store.answer_mail(&question, answer.clone())?,
        Answered::Duplicate
    );
    let conflict = store
        .answer_mail(
            &question,
            MailAnswer {
                body: Text::new("Use revision C.")?,
                ..answer.clone()
            },
        )
        .err()
        .ok_or("a second answer replaced the first")?;
    assert_eq!(mail_error(&conflict), Some(MailError::AlreadyAnswered));
    assert_eq!(conflict.class(), ErrorClass::Conflict);
    assert_eq!(
        store.mail_answer(&sender(&work), &question)?,
        AnswerState::Answered(answer)
    );
    assert!(store.open_questions(10)?.is_empty());
    // The person's two minutes are recorded once on the asking attempt.
    let record = store.task(&work.task)?;
    let replies = record.attempts().first().ok_or("an attempt")?.replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies
            .first()
            .map(|reply| (reply.question.clone(), reply.asked_at, reply.answered_at)),
        Some((question, asked_at, world.now()))
    );

    // A coordinator's answer records no human time; a report takes none.
    let second = store.post_mail(
        &sender(&work),
        post(PostKind::Question, "And?")?,
        world.now(),
    )?;
    store.answer_mail(
        &second,
        MailAnswer {
            body: Text::new("Nothing else.")?,
            by: Answerer::Coordinator,
            at: world.now(),
        },
    )?;
    assert_eq!(
        store
            .task(&work.task)?
            .attempts()
            .first()
            .map(|a| a.replies().len()),
        Some(1)
    );
    let report = store.post_mail(
        &sender(&work),
        post(
            PostKind::Report {
                outcome: ReportedOutcome::Failed,
            },
            "Blocked.",
        )?,
        world.now(),
    )?;
    let error = store
        .answer_mail(
            &report,
            MailAnswer {
                body: Text::new("ok")?,
                by: Answerer::Coordinator,
                at: world.now(),
            },
        )
        .err()
        .ok_or("a report took an answer")?;
    assert_eq!(mail_error(&error), Some(MailError::NotAQuestion));
    let error = store
        .answer_mail(
            &ExternalRef::new("house-mail-999")?,
            MailAnswer {
                body: Text::new("ok")?,
                by: Answerer::Coordinator,
                at: world.now(),
            },
        )
        .err()
        .ok_or("an unknown question took an answer")?;
    assert_eq!(mail_error(&error), Some(MailError::UnknownMessage));
    Ok(())
}

#[test]
fn a_persons_answer_waits_for_a_task_owner_during_a_handover() -> TestResult {
    let world = world()?;
    let work = launched(&world, 1)?;
    let store = &world.fixture.store;
    let question = store.post_mail(
        &sender(&work),
        post(PostKind::Question, "Which?")?,
        world.now(),
    )?;
    relinquish_coordinator(store, &consumer()?, work.consumer_fence, world.now())?;
    let answer = MailAnswer {
        body: Text::new("B.")?,
        by: Answerer::Person,
        at: world.now(),
    };
    let error = store
        .answer_mail(&question, answer.clone())
        .err()
        .ok_or("an answer was recorded without an owner")?;
    assert_eq!(mail_error(&error), Some(MailError::NoOwner));
    // Nothing was stored: the worker still sees no answer.
    assert_eq!(
        store.mail_answer(&sender(&work), &question)?,
        AnswerState::Pending
    );
    Ok(())
}

#[test]
fn the_mailbox_route_is_chosen_from_the_backends_deliveries() -> TestResult {
    let orca_like = World::new()?;
    assert_eq!(
        MailboxRoute::select(orca_like.backend.descriptor()),
        MailboxRoute::Backend
    );
    assert!(
        MailboxRoute::Backend
            .worker_requirements()
            .contains(&Capability::WorkerDeliveries)
    );
    let world = world()?;
    assert_eq!(
        MailboxRoute::select(world.backend.descriptor()),
        MailboxRoute::House
    );
    assert!(
        !MailboxRoute::House
            .worker_requirements()
            .contains(&Capability::WorkerDeliveries)
    );
    // The house route still needs everything else supervision uses.
    let without_messaging = World::with_capabilities(CapabilitySet::supporting(
        Capability::ALL.into_iter().filter(|capability| {
            !matches!(
                capability,
                Capability::WorkerDeliveries | Capability::WorkerMessaging
            )
        }),
    ))?;
    let error = start_coordinator(
        &without_messaging.fixture.store,
        without_messaging.backend.descriptor(),
        &consumer()?,
        &common::scheduled("tick")?,
        ttl(600)?,
        without_messaging.now(),
    )
    .err()
    .ok_or("started without messaging")?;
    assert!(matches!(
        error,
        kitchen::Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. })
            if missing == &[Capability::WorkerMessaging]
    ));
    // The backend's own mailbox stays unsupported; only the house one reads.
    assert_eq!(
        world.backend.next_delivery(),
        Err(MailboxError::Unavailable(BackendUnavailable::Unsupported(
            Capability::WorkerDeliveries
        )))
    );
    Ok(())
}

#[test]
fn coordination_runs_on_a_backend_without_deliveries_through_the_house_mailbox() -> TestResult {
    let world = world()?;
    let started = start_coordinator(
        &world.fixture.store,
        world.backend.descriptor(),
        &consumer()?,
        &common::scheduled("coordinator")?,
        ttl(600)?,
        world.now(),
    )?;
    let CoordinatorStart::Fresh(lease) = started else {
        return Err("coordinator did not start".into());
    };
    world
        .fixture
        .store
        .release_consumer(&consumer()?, lease.fence(), world.now())?;
    let work = launched(&world, 1)?;
    // The brief tells the worker exactly how to reach the house mailbox.
    let record = world.fixture.store.task(&work.task)?;
    let brief = record
        .effects()
        .iter()
        .find_map(|effect| match effect.request().effect() {
            Effect::Worker(Operation::LaunchWorker { brief, .. }) => Some(brief.clone()),
            _ => None,
        })
        .ok_or("no launch recorded")?;
    let scope = format!("--task {} --fence {}", work.task, work.fence.get());
    assert!(
        brief.as_str().contains("kitchn mailbox ask --store"),
        "brief"
    );
    assert!(
        brief.as_str().contains(&scope),
        "brief names the task and fence"
    );

    let coordinator = mailbox(&world, work.consumer_fence)?;
    coordinator.adopt_run()?;
    let asked = world.fixture.store.post_mail(
        &sender(&work),
        post(PostKind::Question, "Keep the old API?")?,
        world.now(),
    )?;
    let batch = coordinator
        .await_delivery(Duration::ZERO)?
        .ok_or("the question did not arrive")?;
    let message = batch.actionable().next().ok_or("nothing actionable")?;
    assert_eq!(message.kind, MessageKind::Question);
    assert_eq!(message.id, asked);
    assert_eq!(message.worker.as_ref(), Some(&work.worker));
    let route = handle_question(
        &world.ctx(),
        &work.task,
        work.fence,
        &supervision()?,
        &WorkerQuestion {
            id: message.id.clone(),
            asked_at: world.now(),
        },
        &Response::Answer {
            body: Text::new("Yes.")?,
            source: AnswerSource::Coordinator,
        },
        None,
    )?;
    assert_eq!(route, QuestionRoute::Replied);
    assert_eq!(coordinator.acknowledge(&batch.id)?, None);

    world.fixture.store.post_mail(
        &sender(&work),
        post(
            PostKind::Report {
                outcome: ReportedOutcome::Succeeded,
            },
            "Implemented; report at reports/issue.md.",
        )?,
        world.now(),
    )?;
    let batch = coordinator
        .await_delivery(Duration::ZERO)?
        .ok_or("the report did not arrive")?;
    let done = batch.actionable().next().ok_or("nothing actionable")?;
    assert_eq!(done.kind, MessageKind::WorkerDone);
    assert_eq!(done.outcome, Some(WorkerOutcome::Succeeded));
    Ok(())
}
