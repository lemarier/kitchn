//! Recovery from the failures a supervised run met: a start that never
//! began, an idle worker, a person's terminal, a provider refusal, an
//! environment fault during validation, and follow-ups to a settled worker.
//! Fake backend and temporary stores only: simulated evidence, not live
//! runtime evidence.

mod common;
mod workflows_support;

use common::{TestResult, at, ttl};
use kitchen::{
    TaskId,
    contracts::{
        Disposition, Effect, Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict, ExternalRef,
        Fence, Operation, ResourceRef, Settlement, Text, Timestamp, WorkerBackend, WorkerOutcome,
        WorkerState, Workspace, fake::ExecuteFault,
    },
    state::AttemptState,
    workflows::{
        coordination::{
            Completion, EnvironmentNext, Escalation, FollowUpRoute, LaunchOutcome, Revalidation,
            Supervision, SupervisionInput, launch_worker, outstanding_follow_ups, retry_validation,
            send_follow_up, supervise,
        },
        pickup::{ClaimOutcome, claim_issue, issue_task_id},
        recovery::{
            EnvironmentFault, FollowUp, PromptState, ProviderCheck, ProviderInterruption,
            RecoverySignals, StartEvidence, TerminalHolder, TranscriptProgress, ValidationFailure,
            ValidationReport,
        },
    },
};
use workflows_support::{
    World, branch, brief, issue, never_started, signals, supervision, template_with, under_consumer,
};

fn claim(world: &World, attempts: u32) -> TestResult<(TaskId, Fence)> {
    let (claimant, _) = under_consumer(world, "coordinator")?;
    match claim_issue(
        &world.fixture.store,
        &template_with(attempts, workflows_support::provenance('a')?)?,
        &issue(1)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )? {
        ClaimOutcome::Claimed(lease) => Ok((issue_task_id(&issue(1)?)?, lease.fence())),
        other => Err(format!("unexpected claim outcome {other:?}").into()),
    }
}

fn launched(world: &World, task: &TaskId, fence: Fence) -> TestResult<ResourceRef> {
    match launch_worker(&world.ctx(), task, fence, Workspace::Isolated, &brief(1)?)? {
        LaunchOutcome::Accepted { worker, .. } => Ok(worker),
        other => Err(format!("launch not accepted: {other:?}").into()),
    }
}

fn step(
    world: &World,
    task: &TaskId,
    fence: Fence,
    input: &SupervisionInput<'_>,
) -> TestResult<Supervision> {
    Ok(supervise(
        &world.ctx(),
        task,
        fence,
        &supervision()?,
        input,
    )?)
}

fn with_signals(signals: &RecoverySignals) -> SupervisionInput<'_> {
    SupervisionInput {
        signals: Some(signals),
        ..SupervisionInput::default()
    }
}

fn idle(worker: &ResourceRef, last_activity: Option<Timestamp>) -> RecoverySignals {
    RecoverySignals {
        prompt: PromptState::Idle,
        ..signals(worker, last_activity)
    }
}

fn attempt_states(world: &World, task: &TaskId) -> TestResult<Vec<AttemptState>> {
    Ok(world
        .fixture
        .store
        .task(task)?
        .attempts()
        .iter()
        .map(kitchen::state::AttemptRecord::state)
        .collect())
}

fn completion(addressed: Vec<ExternalRef>) -> TestResult<Completion> {
    Ok(Completion {
        requested: branch("lemarier/issue-1")?,
        observed_branch: "lemarier/issue-1".to_owned(),
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

/// The brief text of the task's latest launch.
fn latest_brief(world: &World, task: &TaskId) -> TestResult<String> {
    world
        .fixture
        .store
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

// Start accepted but never observed.

#[test]
fn a_start_is_retried_only_on_proof_it_never_began() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world.clock.advance(121);
    let proof = never_started(&worker);
    // Anything short of proof keeps waiting and sends nothing.
    let partial = [
        RecoverySignals {
            transcript: None,
            ..proof.clone()
        },
        RecoverySignals {
            prompt: PromptState::Working,
            ..proof.clone()
        },
        RecoverySignals {
            start: StartEvidence::Unknown,
            ..proof.clone()
        },
        RecoverySignals {
            transcript: Some(TranscriptProgress {
                agent_spoke: true,
                last_activity: None,
            }),
            ..proof.clone()
        },
    ];
    let calls = world.backend.execute_calls();
    assert_eq!(
        step(&world, &task, fence, &SupervisionInput::default())?,
        Supervision::StartUnconfirmed
    );
    for signals in &partial {
        assert_eq!(
            step(&world, &task, fence, &with_signals(signals))?,
            Supervision::StartUnconfirmed
        );
    }
    // Proof about another worker says nothing about this one.
    let other = never_started(&ResourceRef {
        handle: ExternalRef::new("worker-other")?,
        ..worker.clone()
    });
    assert_eq!(
        step(&world, &task, fence, &with_signals(&other))?,
        Supervision::StartUnconfirmed
    );
    assert_eq!(world.backend.execute_calls(), calls);
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Starting
    );

    // With proof the worker is stopped and the task retries as its next,
    // linked attempt within the budget.
    assert_eq!(
        step(&world, &task, fence, &with_signals(&proof))?,
        Supervision::LaunchStalled {
            disposition: Disposition::RetryAvailable { remaining: 2 }
        }
    );
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    match launch_worker(&world.ctx(), &task, fence, Workspace::Isolated, &brief(1)?)? {
        LaunchOutcome::Accepted {
            attempt,
            worker: retry,
        } => {
            assert_eq!(attempt.get(), 2);
            assert_ne!(retry, worker);
        }
        other => return Err(format!("retry not launched: {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_never_started_retry_stops_at_the_retry_budget() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 1)?;
    let worker = launched(&world, &task, fence)?;
    world.clock.advance(121);
    assert_eq!(
        step(&world, &task, fence, &with_signals(&never_started(&worker)))?,
        Supervision::LaunchStalled {
            disposition: Disposition::Settled(Settlement::Exhausted)
        }
    );
    assert_eq!(
        launch_worker(&world.ctx(), &task, fence, Workspace::Isolated, &brief(1)?).ok(),
        None,
        "a settled task launches nothing"
    );
    Ok(())
}

// Worker idle without completion.

#[test]
fn an_idle_worker_is_stopped_only_after_the_bound_without_progress() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let last = world.now();
    world.clock.advance(240);
    let calls = world.backend.execute_calls();
    // At the bound, silence alone, a working agent, or one waiting for a
    // person is not a stall.
    for signals in [
        idle(&worker, Some(last)),
        idle(&worker, None),
        RecoverySignals {
            prompt: PromptState::AwaitingHuman,
            ..signals(&worker, Some(last))
        },
        RecoverySignals {
            transcript: None,
            ..idle(&worker, Some(last))
        },
    ] {
        assert_eq!(
            step(&world, &task, fence, &with_signals(&signals))?,
            Supervision::Running(WorkerState::Ready)
        );
    }
    world.clock.advance(1);
    // Past the bound, only an idle prompt with a stale transcript stalls.
    assert_eq!(
        step(&world, &task, fence, &SupervisionInput::default())?,
        Supervision::Running(WorkerState::Ready),
        "no signals never establish a stall"
    );
    for busy in [
        PromptState::Working,
        PromptState::AwaitingHuman,
        PromptState::Unknown,
    ] {
        let signals = RecoverySignals {
            prompt: busy,
            ..signals(&worker, Some(last))
        };
        assert_eq!(
            step(&world, &task, fence, &with_signals(&signals))?,
            Supervision::Running(WorkerState::Ready)
        );
    }
    assert_eq!(world.backend.execute_calls(), calls);

    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &with_signals(&idle(&worker, Some(last)))
        )?,
        Supervision::IdleStopped {
            disposition: Disposition::RetryAvailable { remaining: 2 }
        }
    );
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    Ok(())
}

#[test]
fn a_refused_idle_stop_keeps_the_attempt_open() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let last = world.now();
    world.clock.advance(241);
    world.backend.inject(ExecuteFault::Reject);
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &with_signals(&idle(&worker, Some(last)))
        )?,
        Supervision::Escalate(Escalation::StopRefused)
    );
    assert_eq!(attempt_states(&world, &task)?, vec![AttemptState::Running]);
    Ok(())
}

#[test]
fn a_persons_idle_terminal_is_left_alone_and_replaced_in_a_fresh_workspace() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::UserTakeover);
    let last = world.now();
    // A person holds the terminal and it is not idle: theirs, untouched.
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &with_signals(&signals(&worker, Some(last)))
        )?,
        Supervision::PersonOwnsTerminal
    );
    world.clock.advance(241);
    let calls = world.backend.execute_calls();
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &with_signals(&idle(&worker, Some(last)))
        )?,
        Supervision::Replace {
            worker: worker.clone(),
            disposition: Disposition::RetryAvailable { remaining: 2 }
        }
    );
    // Nothing was sent to the person's terminal, and it still runs.
    assert_eq!(world.backend.execute_calls(), calls);
    assert_eq!(
        world.backend.observe_worker(&worker)?,
        WorkerState::UserTakeover
    );
    // The replacement starts in a fresh workspace as the next attempt.
    match launch_worker(&world.ctx(), &task, fence, Workspace::Isolated, &brief(1)?)? {
        LaunchOutcome::Accepted {
            attempt,
            worker: replacement,
        } => {
            assert_eq!(attempt.get(), 2);
            assert_ne!(replacement, worker);
        }
        other => return Err(format!("replacement not launched: {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_terminal_a_person_holds_is_never_stopped_even_when_the_backend_says_ready() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let last = world.now();
    world.clock.advance(241);
    let held = RecoverySignals {
        terminal: TerminalHolder::Person,
        ..idle(&worker, Some(last))
    };
    let calls = world.backend.execute_calls();
    assert!(matches!(
        step(&world, &task, fence, &with_signals(&held))?,
        Supervision::Replace { .. }
    ));
    assert_eq!(world.backend.execute_calls(), calls);
    assert_eq!(world.backend.observe_worker(&worker)?, WorkerState::Ready);
    Ok(())
}

// Provider interruption.

#[test]
fn a_provider_refusal_parks_the_task_reports_once_and_spends_no_attempt() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 1)?;
    let worker = launched(&world, &task, fence)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let last = world.now();
    let refused = RecoverySignals {
        provider: Some(ProviderInterruption::Auth),
        ..idle(&worker, Some(last))
    };
    // Past both deadlines, still parked: never stopped, never failed.
    world.clock.advance(250);
    let calls = world.backend.execute_calls();
    assert_eq!(
        step(&world, &task, fence, &with_signals(&refused))?,
        Supervision::Parked {
            interruption: ProviderInterruption::Auth,
            report: true
        }
    );
    assert_eq!(
        step(&world, &task, fence, &with_signals(&refused))?,
        Supervision::Parked {
            interruption: ProviderInterruption::Auth,
            report: false
        }
    );
    assert_eq!(world.backend.execute_calls(), calls);
    assert_eq!(attempt_states(&world, &task)?, vec![AttemptState::Running]);

    // Once the provider works again the worker is told to continue, once.
    let resume = SupervisionInput {
        provider: ProviderCheck::Working,
        ..with_signals(&refused)
    };
    assert_eq!(step(&world, &task, fence, &resume)?, Supervision::Resumed);
    assert_eq!(step(&world, &task, fence, &resume)?, Supervision::Resumed);
    assert_eq!(world.backend.effects_performed(), 2);
    assert_eq!(world.backend.observe_worker(&worker)?, WorkerState::Ready);

    // A later interruption, after new activity, is reported again.
    world.clock.advance(10);
    let again = RecoverySignals {
        provider: Some(ProviderInterruption::Quota),
        ..idle(&worker, Some(world.now()))
    };
    assert_eq!(
        step(&world, &task, fence, &with_signals(&again))?,
        Supervision::Parked {
            interruption: ProviderInterruption::Quota,
            report: true
        }
    );
    // The single-attempt budget is intact.
    assert_eq!(attempt_states(&world, &task)?, vec![AttemptState::Running]);
    Ok(())
}

#[test]
fn a_resume_the_worker_cannot_receive_is_escalated() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    let refused = RecoverySignals {
        provider: Some(ProviderInterruption::RateLimit),
        ..idle(&worker, None)
    };
    world.backend.inject(ExecuteFault::Reject);
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &SupervisionInput {
                provider: ProviderCheck::Working,
                ..with_signals(&refused)
            }
        )?,
        Supervision::Escalate(Escalation::ResumeRefused)
    );
    Ok(())
}

// Environment failures during validation.

#[test]
fn validation_output_is_classified_by_the_operating_system_error() {
    let enospc = "error: failed to write target/debug/deps/libkitchen.rlib: No space left on device (os error 28)";
    assert_eq!(
        EnvironmentFault::classify(enospc),
        Some(EnvironmentFault::NoSpace)
    );
    assert_eq!(
        ValidationFailure::classify("npm ERR! code ENOSPC"),
        ValidationFailure::Environment(EnvironmentFault::NoSpace)
    );
    assert_eq!(
        ValidationFailure::classify("fatal: Cannot allocate memory"),
        ValidationFailure::Environment(EnvironmentFault::OutOfMemory)
    );
    // A failing test is a test failure, even when it mentions disks.
    assert_eq!(
        ValidationFailure::classify("test disk_usage_report ... FAILED\nassertion failed"),
        ValidationFailure::Tests
    );
    assert_eq!(ValidationFailure::classify(""), ValidationFailure::Tests);
    // Only the tail is scanned, and a multi-byte boundary does not panic.
    let old = format!(
        "No space left on device\n{}",
        "é".repeat(kitchen::workflows::recovery::MAX_VALIDATION_SCAN_BYTES)
    );
    assert_eq!(ValidationFailure::classify(&old), ValidationFailure::Tests);
}

#[test]
fn an_environment_fault_is_inspected_and_the_validation_retried_once() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world.backend.set_worker_state(&worker, WorkerState::Ready);
    let failed = |finished_at, failure| SupervisionInput {
        validation: Some(ValidationReport {
            failure,
            finished_at,
        }),
        ..SupervisionInput::default()
    };
    let first = failed(
        world.now(),
        ValidationFailure::Environment(EnvironmentFault::NoSpace),
    );
    // A run from before this worker launched is about an earlier worker.
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &failed(
                at(999),
                ValidationFailure::Environment(EnvironmentFault::NoSpace)
            )
        )?,
        Supervision::Running(WorkerState::Ready)
    );
    // A test failure is the worker's to fix; supervision does not act.
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &failed(world.now(), ValidationFailure::Tests)
        )?,
        Supervision::Running(WorkerState::Ready)
    );
    // An environment fault asks for an inspection; nothing fails.
    for _ in 0..2 {
        assert_eq!(
            step(&world, &task, fence, &first)?,
            Supervision::EnvironmentFailure {
                fault: EnvironmentFault::NoSpace,
                workspace: worker.clone(),
                next: EnvironmentNext::InspectThenRevalidate,
            }
        );
    }
    assert_eq!(world.backend.effects_performed(), 1);
    assert_eq!(attempt_states(&world, &task)?, vec![AttemptState::Running]);

    // After the inspection the validation is retried once.
    world.clock.advance(5);
    assert_eq!(
        retry_validation(&world.ctx(), &task, fence)?,
        Revalidation::Sent
    );
    assert_eq!(
        retry_validation(&world.ctx(), &task, fence)?,
        Revalidation::Sent
    );
    assert_eq!(world.backend.effects_performed(), 2);
    // The run that led to the retry is not counted again, nor is one that
    // finished at the moment the retry was sent.
    assert_eq!(
        step(&world, &task, fence, &first)?,
        Supervision::Running(WorkerState::Ready)
    );
    let simultaneous = failed(
        world.now(),
        ValidationFailure::Environment(EnvironmentFault::NoSpace),
    );
    assert_eq!(
        step(&world, &task, fence, &simultaneous)?,
        Supervision::Running(WorkerState::Ready)
    );
    // The same fault after the retry escalates as an environment problem.
    world.clock.advance(5);
    let again = failed(
        world.now(),
        ValidationFailure::Environment(EnvironmentFault::NoSpace),
    );
    assert_eq!(
        step(&world, &task, fence, &again)?,
        Supervision::Escalate(Escalation::EnvironmentPersistent(EnvironmentFault::NoSpace))
    );
    assert_eq!(attempt_states(&world, &task)?, vec![AttemptState::Running]);
    Ok(())
}

#[test]
fn a_worker_that_ended_on_an_environment_fault_retries_as_a_new_attempt() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    let failed = |finished_at| SupervisionInput {
        validation: Some(ValidationReport {
            failure: ValidationFailure::Environment(EnvironmentFault::NoSpace),
            finished_at,
        }),
        ..SupervisionInput::default()
    };
    world.clock.advance(1);
    assert_eq!(
        step(&world, &task, fence, &failed(world.now()))?,
        Supervision::EnvironmentFailure {
            fault: EnvironmentFault::NoSpace,
            workspace: worker,
            next: EnvironmentNext::InspectThenRetry(Disposition::RetryAvailable { remaining: 2 }),
        }
    );
    Ok(())
}

// Follow-ups to a settled worker.

fn follow_up(id: &str, body: &str) -> TestResult<FollowUp> {
    Ok(FollowUp {
        id: ExternalRef::new(id)?,
        body: Text::new(body)?,
    })
}

#[test]
fn a_completion_missing_a_follow_up_becomes_a_follow_up_round() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let request = follow_up("review-1", "Rename the driver constant.")?;
    assert_eq!(
        send_follow_up(&world.ctx(), &task, fence, &request)?,
        FollowUpRoute::NoWorker
    );
    let worker = launched(&world, &task, fence)?;
    let FollowUpRoute::Delivered { id } = send_follow_up(&world.ctx(), &task, fence, &request)?
    else {
        return Err("follow-up not delivered".into());
    };
    // A repeated send delivers nothing new.
    assert_eq!(
        send_follow_up(&world.ctx(), &task, fence, &request)?,
        FollowUpRoute::Delivered { id: id.clone() }
    );
    assert_eq!(world.backend.effects_performed(), 2);

    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let silent = completion(Vec::new())?;
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &SupervisionInput {
                completion: Some(&silent),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::FollowUpRound {
            missing: vec![id.clone()],
            disposition: Disposition::RetryAvailable { remaining: 2 }
        }
    );
    // The next brief carries the request with the id to report.
    let next = launched(&world, &task, fence)?;
    let text = latest_brief(&world, &task)?;
    assert!(text.contains(&format!("- {id}: ")));
    assert!(text.contains("Rename the driver constant."));

    // A completion that addresses it settles the task.
    world
        .backend
        .set_worker_state(&next, WorkerState::Settled(WorkerOutcome::Succeeded));
    let done = completion(vec![id])?;
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &SupervisionInput {
                completion: Some(&done),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::Settled(Settlement::Succeeded)
    );
    assert!(outstanding_follow_ups(&world.fixture.store.task(&task)?).is_empty());
    Ok(())
}

#[test]
fn a_follow_up_to_a_completed_worker_is_queued_into_the_next_brief() -> TestResult {
    let world = World::new()?;
    let (task, fence) = claim(&world, 3)?;
    let worker = launched(&world, &task, fence)?;
    // The worker already completed; its dispatch refuses messages.
    world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let request = follow_up("review-2", "Add a test for the timeout path.")?;
    let FollowUpRoute::Queued { id } = send_follow_up(&world.ctx(), &task, fence, &request)? else {
        return Err("follow-up not queued".into());
    };
    let queued = outstanding_follow_ups(&world.fixture.store.task(&task)?);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued.first().map(|queued| &queued.id), Some(&id));

    // The completion predates the request, so it cannot address it.
    let report = completion(Vec::new())?;
    assert_eq!(
        step(
            &world,
            &task,
            fence,
            &SupervisionInput {
                completion: Some(&report),
                ..SupervisionInput::default()
            }
        )?,
        Supervision::FollowUpRound {
            missing: vec![id.clone()],
            disposition: Disposition::RetryAvailable { remaining: 2 }
        }
    );
    launched(&world, &task, fence)?;
    let text = latest_brief(&world, &task)?;
    assert!(text.contains(&format!("- {id}: ")));
    assert!(text.contains("Add a test for the timeout path."));
    Ok(())
}
