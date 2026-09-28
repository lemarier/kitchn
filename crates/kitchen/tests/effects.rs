//! Executor-neutral effects: forge, decision, and schedule effects through
//! the same durable path as worker operations, scope checks, admission
//! budgets, replies, and inventory. Simulated with fake executors.

mod common;

use common::{
    Fixture, ManualClock, TestResult, at, backend_id, creator, credential, grant, house, launch,
    other_house, plan, scheduled, spec, task_id, ttl,
};
use kitchen::{
    BackendId, ConsumerId, Error, TaskId,
    contracts::{
        AskKind, AskRisk, Capability, CapabilitySet, CommitId, ContractError, DecisionBinding,
        DecisionOwner, Effect, EffectExecutor, EvidenceRevision, ExecutorKind, ExternalRef, Fence,
        GitHubAction, GitHubEffect, GitHubMutation, Grant, GrantScope, HouseGrants,
        LabelDefinition, Liveness, MAX_ASKS_PER_TASK, NotAppliedReason, Operation, Permission,
        PostingBudget, Repository, ResourceKind, ResourceRef, RogerAsk, RogerEffect,
        ScheduleEffect, TaskAuthority, Text, WorkerBackend,
        conformance::{self, Check, CheckResult, ConformanceFixture},
        fake::{ExecuteFault, FakeBackend},
    },
    state::{EffectState, reconcile, run_effect},
};

fn km43() -> TestResult<Repository> {
    Ok(Repository::new("origin89hq/km43")?)
}

fn namespace(executor: ExecutorKind) -> TestResult<BackendId> {
    Ok(BackendId::new(match executor {
        ExecutorKind::Worker => "fake",
        ExecutorKind::GitHub => "github-origin89",
        ExecutorKind::Roger => "roger-origin89",
        ExecutorKind::Schedule => "scheduler-origin89",
    })?)
}

/// A fake executor for one family, with lookup and idempotent requests.
fn executor(kind: ExecutorKind, capability: Capability) -> TestResult<FakeBackend> {
    Ok(FakeBackend::new(
        namespace(kind)?,
        house()?,
        CapabilitySet::supporting([
            capability,
            Capability::EffectLookup,
            Capability::EffectIdempotentRequests,
        ]),
    ))
}

fn label(repository: Repository, name: &str) -> TestResult<Effect> {
    Ok(GitHubEffect {
        requester: ExternalRef::new("fixture")?,
        mutation: GitHubMutation {
            repository,
            action: GitHubAction::CreateLabel {
                label: LabelDefinition {
                    name: name.into(),
                    color: "aabbcc".into(),
                    description: String::new(),
                },
            },
        },
        posting_budget: PostingBudget::new(100)?,
    }
    .into())
}

fn ask(task: &TaskId, revision: EvidenceRevision) -> TestResult<Effect> {

    Ok(RogerEffect {
        requester: ExternalRef::new("fixture")?,
        ask: RogerAsk {
            binding: DecisionBinding {
                house: house()?,
                task: task.clone(),
                action: Permission::Merge,
                revision,
                subject: CommitId::new(&"a".repeat(40))?,
                owner: DecisionOwner::Merge,
                repository: km43()?,
                target: ExternalRef::new("pr:origin89hq/km43#1")?,
                limits: Text::new("squash into main")?,
            },
            kind: AskKind::Approval,
            risk: AskRisk::Routine,
            title: Text::new("Merge at this head?")?,
            body: Text::new("Merge at this head?")?,
            supersedes: None,
        },
        posting_budget: PostingBudget::new(MAX_ASKS_PER_TASK)?,
    }
    .into())
}

fn install() -> TestResult<Effect> {
    Ok(ScheduleEffect::InstallDisabled {
        consumer: ConsumerId::new("pickup-origin89")?,
    }
    .into())
}

/// House grants for every executor family, on its own namespace.
fn grants_everywhere() -> TestResult<HouseGrants> {
    Ok(HouseGrants::new(house()?, grants_everywhere_list()?))
}

/// A claimed, running task with every grant, optionally for one repository.
fn task_for(
    fixture: &Fixture,
    id: &str,
    repository: Option<Repository>,
) -> TestResult<(TaskId, Fence)> {
    let grants = grants_everywhere()?;
    let mut work = spec(id)?;
    work.repository = repository;
    work.authority = TaskAuthority::delegate(&grants, grants_everywhere_list()?)?;
    let task = task_id(id)?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

fn grants_everywhere_list() -> TestResult<Vec<Grant>> {
    Ok(vec![
        grant(Permission::LaunchWorker)?,
        grant(Permission::MessageWorker)?,
        grant(Permission::CancelWorker)?,
        Grant::house(
            Permission::EditLabels,
            namespace(ExecutorKind::GitHub)?,
            credential()?,
        ),
        Grant::house(
            Permission::AskHuman,
            namespace(ExecutorKind::Roger)?,
            credential()?,
        ),
        Grant::house(
            Permission::ManageSchedule,
            namespace(ExecutorKind::Schedule)?,
            credential()?,
        ),
    ])
}

fn fixture_for(repository: Repository) -> TestResult<ConformanceFixture> {
    Ok(ConformanceFixture {
        house: house()?,
        foreign_house: other_house()?,
        foreign_backend: BackendId::new("elsewhere")?,
        credential: credential()?,
        task: task_id("conformance")?,
        repository,
        run_tag: ExternalRef::new("run-1")?,
        brief: Text::new("conformance probe")?,
    })
}

#[test]
fn non_worker_executors_pass_the_shared_contract() -> TestResult {
    let task = task_id("conformance")?;
    let cases = [
        (
            ExecutorKind::GitHub,
            Capability::ForgeMutation,
            label(km43()?, "agent-ready")?,
        ),
        (
            ExecutorKind::Roger,
            Capability::AskHuman,
            ask(&task, EvidenceRevision::INITIAL)?,
        ),
        (
            ExecutorKind::Schedule,
            Capability::ScheduleManage,
            install()?,
        ),
    ];
    for (kind, capability, probe) in cases {
        let fake = executor(kind, capability)?;
        let report = conformance::run(&fake, &fixture_for(km43()?)?, &probe)?;
        for check in [
            Check::DescriptorHouse,
            Check::CrossHouseRefused,
            Check::ForeignBackendRefused,
            Check::UnsupportedRefused,
            Check::UnknownKeyNotApplied,
            Check::ProbeReceipt,
            Check::LookupMatchesReceipt,
            Check::IdempotentResubmission,
        ] {
            assert_eq!(
                report.result(check),
                Some(CheckResult::Passed),
                "{kind:?} {check}"
            );
        }
        assert_eq!(
            report.result(Check::LaunchReceipt),
            None,
            "worker checks not run"
        );
        assert_eq!(
            fake.effects_performed(),
            1,
            "{kind:?}: only the probe applied"
        );
    }

    // The same executor without the probe's capability refuses it.
    let unable = executor(ExecutorKind::GitHub, Capability::AskHuman)?;
    let report = conformance::run(&unable, &fixture_for(km43()?)?, &label(km43()?, "x")?)?;
    assert_eq!(
        report.result(Check::ProbeReceipt),
        Some(CheckResult::NotApplicable {
            requires: Capability::ForgeMutation
        })
    );
    assert_eq!(unable.effects_performed(), 0);
    Ok(())
}

#[test]
fn a_forge_effect_uses_the_same_durable_path() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", Some(km43()?))?;
    let forge = executor(ExecutorKind::GitHub, Capability::ForgeMutation)?;
    let workers = executor(ExecutorKind::Worker, Capability::WorkerLaunchIsolated)?;
    let clock = ManualClock::starting_at(1);
    let grants = grants_everywhere()?;

    // A worker backend cannot perform a forge effect.
    assert!(matches!(
        run_effect(&fixture.store, &workers, &grants, plan(&task, fence, "label", label(km43()?, "agent-ready")?)?, &clock),
        Err(Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. }))
            if missing == &[Capability::ForgeMutation]
    ));

    forge.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run_effect(
        &fixture.store,
        &forge,
        &grants,
        plan(&task, fence, "label", label(km43()?, "agent-ready")?)?,
        &clock,
    )?;
    assert!(matches!(lost.state(), EffectState::Uncertain { .. }));
    assert_eq!(lost.request().backend(), &namespace(ExecutorKind::GitHub)?);
    assert_eq!(lost.request().effect().executor(), ExecutorKind::GitHub);

    // Reconciling with the worker backend leaves it for the forge namespace.
    let report = reconcile(&fixture.store, &workers, &task, fence, &clock)?;
    assert!(matches!(report.foreign.as_slice(), [effect] if effect.seq() == lost.seq()));
    let report = reconcile(&fixture.store, &forge, &task, fence, &clock)?;
    assert!(matches!(
        report.resolved.as_slice(),
        [effect] if matches!(effect.state(), EffectState::Applied { .. })
    ));
    assert_eq!(forge.effects_performed(), 1);
    Ok(())
}

#[test]
fn effect_scope_must_stay_within_the_task() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "km43-task", Some(km43()?))?;
    let forge = executor(ExecutorKind::GitHub, Capability::ForgeMutation)?;
    let scheduler = executor(ExecutorKind::Schedule, Capability::ScheduleManage)?;
    let clock = ManualClock::starting_at(1);
    let grants = grants_everywhere()?;
    let firmware = Repository::new("origin89hq/firmware")?;

    assert_eq!(
        run_effect(
            &fixture.store,
            &forge,
            &grants,
            plan(&task, fence, "label", label(firmware.clone(), "x")?)?,
            &clock
        )
        .err()
        .map(|error| error.to_string()),
        Some(
            ContractError::OutOfTaskScope {
                effect: GrantScope::Repository(firmware),
                task: GrantScope::Repository(km43()?),
            }
            .to_string()
        )
    );
    assert!(matches!(
        run_effect(
            &fixture.store,
            &scheduler,
            &grants,
            plan(&task, fence, "install", install()?)?,
            &clock
        ),
        Err(Error::Contract(ContractError::OutOfTaskScope {
            effect: GrantScope::House,
            ..
        }))
    ));
    assert!(fixture.store.task(&task)?.effects().is_empty());

    // A house-level task may install a schedule.
    let (house_task, house_fence) = task_for(&fixture, "house-task", None)?;
    let installed = run_effect(
        &fixture.store,
        &scheduler,
        &grants,
        plan(&house_task, house_fence, "install", install()?)?,
        &clock,
    )?;
    let EffectState::Applied { receipt, .. } = installed.state() else {
        return Err("schedule not installed".into());
    };
    assert!(matches!(receipt.created(), [resource] if resource.kind == ResourceKind::Schedule));
    Ok(())
}

#[test]
fn the_ask_budget_is_enforced_when_intent_is_persisted() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", None)?;
    let roger = executor(ExecutorKind::Roger, Capability::AskHuman)?;
    let clock = ManualClock::starting_at(1);
    let grants = grants_everywhere()?;
    let run = |name: &str, effect: Effect| {
        run_effect(
            &fixture.store,
            &roger,
            &grants,
            plan(&task, fence, name, effect)?,
            &clock,
        )
        .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
    };

    // A binding for another task or an old revision is refused.
    assert!(matches!(
        run("wrong-task", ask(&task_id("task-2")?, EvidenceRevision::INITIAL)?),
        Err(error) if matches!(error.downcast_ref::<Error>(), Some(Error::Contract(ContractError::DecisionBindingMismatch)))
    ));

    // A refused ask was never posted and does not count.
    roger.inject(ExecuteFault::Reject);
    let refused = run("ask-0", ask(&task, EvidenceRevision::INITIAL)?)?;
    assert!(matches!(
        refused.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));
    let asked: Vec<_> = (1..=MAX_ASKS_PER_TASK)
        .map(|index| {
            run(
                &format!("ask-{index}"),
                ask(&task, EvidenceRevision::INITIAL)?,
            )
        })
        .collect::<TestResult<_>>()?;
    assert!(
        asked
            .iter()
            .all(|effect| matches!(effect.state(), EffectState::Applied { .. }))
    );
    // Repeating an asked question returns it without counting again.
    assert_eq!(
        run("ask-1", ask(&task, EvidenceRevision::INITIAL)?)?.seq(),
        asked[0].seq()
    );

    let over = run("ask-extra", ask(&task, EvidenceRevision::INITIAL)?);
    assert!(matches!(
        over,
        Err(error) if matches!(
            error.downcast_ref::<Error>(),
            Some(Error::Contract(ContractError::EffectBudgetExhausted { executor: ExecutorKind::Roger, limit: MAX_ASKS_PER_TASK }))
        )
    ));
    assert_eq!(roger.effects_performed(), 3);
    assert!(
        fixture
            .store
            .task(&task)?
            .effects()
            .iter()
            .all(|effect| effect.name().as_str() != "ask-extra"),
        "no intent persisted beyond the budget"
    );
    Ok(())
}

#[test]
fn replies_go_to_a_live_worker_through_messaging() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", None)?;
    let workers = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let grants = grants_everywhere()?;
    let launched = run_effect(
        &fixture.store,
        &workers,
        &grants,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    let EffectState::Applied { receipt, .. } = launched.state() else {
        return Err("launch not applied".into());
    };
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("receipt names no worker")?;
    let reply = |worker: ResourceRef| -> TestResult<Operation> {
        Ok(Operation::ReplyToWorker {
            worker,
            question: ExternalRef::new("question-1")?,
            body: Text::new("Use the existing parser.")?,
        })
    };
    assert_eq!(
        reply(worker.clone())?.required_capability(),
        Capability::WorkerMessaging
    );
    assert_eq!(
        reply(worker.clone())?.required_permission(),
        Permission::MessageWorker
    );

    let answered = run_effect(
        &fixture.store,
        &workers,
        &grants,
        plan(&task, fence, "reply", reply(worker.clone())?)?,
        &clock,
    )?;
    assert!(matches!(answered.state(), EffectState::Applied { .. }));
    let stranger = ResourceRef {
        handle: ExternalRef::new("no-such-worker")?,
        ..worker
    };
    // Not a worker this task created: refused before any intent.
    assert!(matches!(
        run_effect(
            &fixture.store,
            &workers,
            &grants,
            plan(&task, fence, "reply-2", reply(stranger)?)?,
            &clock,
        ),
        Err(Error::State(kitchen::state::StateError::ResourceNotOwned))
    ));

    let silent = executor(ExecutorKind::Worker, Capability::WorkerLaunchIsolated)?;
    assert!(matches!(
        run_effect(&fixture.store, &silent, &grants, plan(&task, fence, "reply-3", reply(ResourceRef { handle: ExternalRef::new("w")?, kind: ResourceKind::Worker, backend: backend_id()? })?)?, &clock),
        Err(Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. })) if missing == &[Capability::WorkerMessaging]
    ));
    Ok(())
}

#[test]
fn inventory_reports_owner_and_liveness_within_its_bound() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", None)?;
    let workers = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let grants = grants_everywhere()?;
    assert!(workers.inventory()?.is_empty());
    let launched = run_effect(
        &fixture.store,
        &workers,
        &grants,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    let listed = workers.inventory()?;
    let [observation] = listed.as_slice() else {
        return Err("expected one resource".into());
    };
    assert_eq!(observation.liveness, Liveness::Live);
    assert_eq!(
        observation.owner.as_ref().map(ExternalRef::as_str),
        Some(launched.request().key().as_str()),
        "the owner links back to the persisted intent"
    );
    run_effect(
        &fixture.store,
        &workers,
        &grants,
        plan(
            &task,
            fence,
            "cancel",
            Operation::CancelWorker {
                worker: observation.resource.clone(),
            },
        )?,
        &clock,
    )?;
    assert_eq!(
        workers.inventory()?.first().map(|found| found.liveness),
        Some(Liveness::Exited)
    );

    // A person holding the worker keeps it live, and nothing is dispatched into it.
    workers.set_worker_state(
        &observation.resource,
        kitchen::contracts::WorkerState::UserTakeover,
    );
    assert_eq!(
        workers.inventory()?.first().map(|found| found.liveness),
        Some(Liveness::Live)
    );
    let held = run_effect(
        &fixture.store,
        &workers,
        &grants,
        plan(
            &task,
            fence,
            "message-held",
            Operation::MessageWorker {
                worker: observation.resource.clone(),
                body: Text::new("still there?")?,
            },
        )?,
        &clock,
    )?;
    assert!(matches!(
        held.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));

    // A lost record or an unknown state is not evidence of exit.
    for state in [
        kitchen::contracts::WorkerState::Missing,
        kitchen::contracts::WorkerState::Unknown,
    ] {
        workers.set_worker_state(&observation.resource, state);
        assert_eq!(
            workers.inventory()?.first().map(|found| found.liveness),
            Some(Liveness::Unverifiable),
            "{state:?}"
        );
    }

    // Without the capability, the default reports it unsupported.
    let blind = executor(ExecutorKind::Worker, Capability::WorkerLaunchIsolated)?;
    assert_eq!(
        blind.inventory(),
        Err(kitchen::contracts::BackendUnavailable::Unsupported(
            Capability::ResourceInventory
        ))
    );
    assert!(
        !blind
            .descriptor()
            .capabilities
            .supports(Capability::ResourceInventory)
    );
    Ok(())
}

fn launched_worker(
    fixture: &Fixture,
    task: &TaskId,
    fence: Fence,
    backend: &FakeBackend,
) -> TestResult<ResourceRef> {
    let launched = run_effect(
        &fixture.store,
        backend,
        &grants_everywhere()?,
        plan(task, fence, "launch", launch()?)?,
        &ManualClock::starting_at(1),
    )?;
    let EffectState::Applied { receipt, .. } = launched.state() else {
        return Err("launch not applied".into());
    };
    receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or_else(|| "receipt names no worker".into())
}

#[test]
fn targeted_operations_need_a_resource_the_task_owns() -> TestResult {
    let fixture = Fixture::new()?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let (owner, owner_fence) = task_for(&fixture, "task-b", None)?;
    let worker = launched_worker(&fixture, &owner, owner_fence, &backend)?;
    let (other, fence) = task_for(&fixture, "task-a", None)?;
    let targeted = [
        Operation::CancelWorker {
            worker: worker.clone(),
        },
        Operation::ReleaseResource {
            resource: worker.clone(),
        },
        Operation::MessageWorker {
            worker: worker.clone(),
            body: Text::new("hi")?,
        },
        Operation::ReplyToWorker {
            worker: worker.clone(),
            question: ExternalRef::new("q-1")?,
            body: Text::new("yes")?,
        },
        Operation::LaunchWorker {
            role: kitchen::contracts::Role::StationCook,
            workspace: kitchen::contracts::Workspace::Existing(worker.clone()),
            brief: Text::new("reuse")?,
            branch: None,
        },
    ];
    for (index, operation) in targeted.into_iter().enumerate() {
        let result = run_effect(
            &fixture.store,
            &backend,
            &grants_everywhere()?,
            plan(&other, fence, &format!("op-{index}"), operation)?,
            &ManualClock::starting_at(2),
        );
        assert!(
            matches!(
                result,
                Err(Error::State(kitchen::state::StateError::ResourceNotOwned))
            ),
            "{index}: {result:?}"
        );
    }
    assert!(fixture.store.task(&other)?.effects().is_empty());
    assert_eq!(
        backend.observe_worker(&worker)?,
        kitchen::contracts::WorkerState::Starting
    );

    // A worker on another backend namespace is not this task's either.
    let elsewhere = ResourceRef {
        backend: BackendId::new("elsewhere")?,
        ..worker.clone()
    };
    let mut given_elsewhere = spec("task-c")?;
    given_elsewhere.authority =
        TaskAuthority::delegate(&grants_everywhere()?, grants_everywhere_list()?)?;
    given_elsewhere.resources = [elsewhere.clone()].into();
    fixture
        .store
        .create_task(given_elsewhere, &creator()?, at(0))?;
    let task_c = task_id("task-c")?;
    let fence_c = fixture
        .store
        .claim(&task_c, &scheduled("coordinator-c")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task_c, fence_c, at(0))?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants_everywhere()?,
            plan(
                &task_c,
                fence_c,
                "cancel",
                Operation::CancelWorker { worker: elsewhere }
            )?,
            &ManualClock::starting_at(2)
        ),
        Err(Error::State(kitchen::state::StateError::ResourceNotOwned))
    ));

    // A task given the worker at creation may act on it.
    let mut adopting = spec("task-d")?;
    adopting.authority = TaskAuthority::delegate(&grants_everywhere()?, grants_everywhere_list()?)?;
    adopting.resources = [worker.clone()].into();
    fixture.store.create_task(adopting, &creator()?, at(0))?;
    let task_d = task_id("task-d")?;
    let fence_d = fixture
        .store
        .claim(&task_d, &scheduled("coordinator-d")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task_d, fence_d, at(0))?;
    let message = run_effect(
        &fixture.store,
        &backend,
        &grants_everywhere()?,
        plan(
            &task_d,
            fence_d,
            "message",
            Operation::MessageWorker {
                worker: worker.clone(),
                body: Text::new("status?")?,
            },
        )?,
        &ManualClock::starting_at(3),
    )?;
    let EffectState::Applied { receipt, .. } = message.state() else {
        return Err("message not applied".into());
    };
    assert!(receipt.created().is_empty());
    assert_eq!(receipt.touched(), std::slice::from_ref(&worker));
    let stopped = run_effect(
        &fixture.store,
        &backend,
        &grants_everywhere()?,
        plan(
            &task_d,
            fence_d,
            "cancel",
            Operation::CancelWorker {
                worker: worker.clone(),
            },
        )?,
        &ManualClock::starting_at(4),
    )?;
    assert!(matches!(stopped.state(), EffectState::Applied { .. }));
    Ok(())
}

#[test]
fn a_same_key_ask_retry_is_checked_against_the_current_revision() -> TestResult {
    for move_base in [false, true] {
        let fixture = Fixture::new()?;
        let (task, fence) = task_for(&fixture, "task-1", None)?;
        let roger = executor(ExecutorKind::Roger, Capability::AskHuman)?;
        let grants = grants_everywhere()?;
        let subject = |head: char, base: char| -> TestResult<kitchen::contracts::Evidence> {
            Ok(kitchen::contracts::Evidence {
                kind: kitchen::contracts::EvidenceKind::Check,
                verdict: kitchen::contracts::EvidenceVerdict::Pass,
                subject: kitchen::contracts::EvidenceSubject {
                    head: common::commit(head)?,
                    base: Some(common::commit(base)?),
                },
                source: ExternalRef::new("ci-1")?,
                observed_at: at(1),
            })
        };
        let asked_at = fixture
            .store
            .record_evidence(&task, fence, subject('a', 'b')?, at(1))?;
        let mut first = plan(
            &task,
            fence,
            "ask",
            ask_about(&task, asked_at, Some(subject('a', 'b')?.subject))?,
        )?;
        first.decided_at = asked_at;
        roger.inject(ExecuteFault::TimeoutWithoutApplying);
        let lost = run_effect(
            &fixture.store,
            &roger,
            &grants,
            first,
            &ManualClock::starting_at(2),
        )?;
        assert!(matches!(lost.state(), EffectState::Uncertain { .. }));

        let moved = if move_base {
            fixture
                .store
                .record_evidence(&task, fence, subject('a', 'c')?, at(3))?
        } else {
            fixture
                .store
                .record_evidence(&task, fence, subject('d', 'b')?, at(3))?
        };
        roger.fail_lookups(100);
        let mut retry = plan(
            &task,
            fence,
            "ask",
            ask_about(&task, asked_at, Some(subject('a', 'b')?.subject))?,
        )?;
        retry.decided_at = moved;
        let result = run_effect(
            &fixture.store,
            &roger,
            &grants,
            retry,
            &ManualClock::starting_at(4),
        );
        assert!(
            matches!(
                result,
                Err(Error::Contract(ContractError::DecisionBindingMismatch))
            ),
            "{result:?}"
        );
        assert_eq!(
            roger.execute_calls(),
            1,
            "the stale question was not sent again"
        );
        let record = fixture.store.task(&task)?;
        assert_eq!(
            record.effects().first().map(|effect| effect.submissions()),
            Some(1)
        );
    }
    Ok(())
}

#[test]
fn a_same_key_ask_retry_does_not_count_against_its_own_budget() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", None)?;
    let roger = executor(ExecutorKind::Roger, Capability::AskHuman)?;
    let grants = grants_everywhere()?;
    let clock = ManualClock::starting_at(1);
    for index in 1..MAX_ASKS_PER_TASK {
        run_effect(
            &fixture.store,
            &roger,
            &grants,
            plan(
                &task,
                fence,
                &format!("ask-{index}"),
                ask(&task, EvidenceRevision::INITIAL)?,
            )?,
            &clock,
        )?;
    }
    // The last ask the budget allows is lost; its retry is the same effect.
    roger.inject(ExecuteFault::TimeoutWithoutApplying);
    let last = run_effect(
        &fixture.store,
        &roger,
        &grants,
        plan(
            &task,
            fence,
            "ask-last",
            ask(&task, EvidenceRevision::INITIAL)?,
        )?,
        &clock,
    )?;
    assert!(matches!(last.state(), EffectState::Uncertain { .. }));
    roger.fail_lookups(1);
    let retried = run_effect(
        &fixture.store,
        &roger,
        &grants,
        plan(
            &task,
            fence,
            "ask-last",
            ask(&task, EvidenceRevision::INITIAL)?,
        )?,
        &clock,
    )?;
    assert_eq!(retried.seq(), last.seq());
    assert!(matches!(retried.state(), EffectState::Applied { .. }));
    assert_eq!(retried.submissions(), 2);
    assert!(matches!(
        run_effect(
            &fixture.store,
            &roger,
            &grants,
            plan(
                &task,
                fence,
                "ask-extra",
                ask(&task, EvidenceRevision::INITIAL)?
            )?,
            &clock
        ),
        Err(Error::Contract(ContractError::EffectBudgetExhausted { .. }))
    ));
    Ok(())
}

#[test]
fn an_ask_must_name_the_exact_evidence_subject() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", None)?;
    let roger = executor(ExecutorKind::Roger, Capability::AskHuman)?;
    let grants = grants_everywhere()?;
    let clock = ManualClock::starting_at(1);
    let at_head =
        |head: char, base: Option<char>| -> TestResult<kitchen::contracts::EvidenceSubject> {
            Ok(kitchen::contracts::EvidenceSubject {
                head: common::commit(head)?,
                base: base.map(common::commit).transpose()?,
            })
        };
    let revision = fixture.store.record_evidence(
        &task,
        fence,
        kitchen::contracts::Evidence {
            kind: kitchen::contracts::EvidenceKind::Check,
            verdict: kitchen::contracts::EvidenceVerdict::Pass,
            subject: at_head('a', Some('b'))?,
            source: ExternalRef::new("ci-1")?,
            observed_at: at(1),
        },
        at(1),
    )?;
    // The counter matches, but the question names another head, another
    // base, or no subject at all.
    for (name, wrong) in [
        ("other-head", Some(at_head('c', Some('b'))?)),
        ("other-base", Some(at_head('a', Some('d'))?)),
        ("no-base", Some(at_head('a', None)?)),
        ("no-subject", None),
    ] {
        let mut attempt = plan(&task, fence, name, ask_about(&task, revision, wrong)?)?;
        attempt.decided_at = revision;
        let result = run_effect(&fixture.store, &roger, &grants, attempt, &clock);
        assert!(
            matches!(
                result,
                Err(Error::Contract(ContractError::DecisionBindingMismatch))
            ),
            "{name}: {result:?}"
        );
    }
    assert_eq!(roger.execute_calls(), 0);
    let mut exact = plan(
        &task,
        fence,
        "exact",
        ask_about(&task, revision, Some(at_head('a', Some('b'))?))?,
    )?;
    exact.decided_at = revision;
    let asked = run_effect(&fixture.store, &roger, &grants, exact, &clock)?;
    assert!(matches!(asked.state(), EffectState::Applied { .. }));
    Ok(())
}

/// Like Orca: launches, cancels, and releases can be looked up and deduplicated; messages cannot.
fn orca_like() -> TestResult<FakeBackend> {
    Ok(FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting([
            Capability::WorkerLaunchIsolated,
            Capability::WorkerMessaging,
            Capability::WorkerCancel,
            Capability::WorkerStatusAndOutcome,
            Capability::ResourceRelease,
            Capability::LookupLaunchWorker,
            Capability::IdempotentLaunchWorker,
            Capability::LookupCancelWorker,
            Capability::IdempotentCancelWorker,
            Capability::LookupReleaseResource,
            Capability::IdempotentReleaseResource,
        ]),
    ))
}

#[test]
fn recovery_follows_the_per_kind_declaration() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = task_for(&fixture, "task-1", None)?;
    let orca = orca_like()?;
    let grants = grants_everywhere()?;
    let clock = ManualClock::starting_at(1);
    let message = |worker: &ResourceRef| -> TestResult<Operation> {
        Ok(Operation::MessageWorker {
            worker: worker.clone(),
            body: Text::new("status?")?,
        })
    };
    assert!(orca.descriptor().supports_lookup(&launch()?.into()));
    assert!(orca.descriptor().idempotent(&launch()?.into()));

    // A lost launch is looked up and resolved without a second launch.
    orca.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run_effect(
        &fixture.store,
        &orca,
        &grants,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    assert!(matches!(lost.state(), EffectState::Uncertain { .. }));
    let found = run_effect(
        &fixture.store,
        &orca,
        &grants,
        plan(&task, fence, "launch", launch()?)?,
        &clock,
    )?;
    let EffectState::Applied { receipt, .. } = found.state() else {
        return Err(format!("launch not recovered: {:?}", found.state()).into());
    };
    assert_eq!(orca.execute_calls(), 1);
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("receipt names no worker")?;

    // A lost message can be neither looked up nor resubmitted.
    let body: Effect = message(&worker)?.into();
    assert!(!orca.descriptor().supports_lookup(&body));
    assert!(!orca.descriptor().idempotent(&body));
    orca.inject(ExecuteFault::TimeoutWithoutApplying);
    let unsent = run_effect(
        &fixture.store,
        &orca,
        &grants,
        plan(&task, fence, "message", message(&worker)?)?,
        &clock,
    )?;
    assert!(matches!(unsent.state(), EffectState::Uncertain { .. }));
    assert!(matches!(
        run_effect(&fixture.store, &orca, &grants, plan(&task, fence, "message", message(&worker)?)?, &clock),
        Err(Error::State(kitchen::state::StateError::UnsafeRetry(seq))) if seq == unsent.seq()
    ));
    assert_eq!(orca.execute_calls(), 2);
    let report = reconcile(&fixture.store, &orca, &task, fence, &clock)?;
    assert!(matches!(
        report.unresolved.as_slice(),
        [effect] if matches!(effect.state(), EffectState::Uncertain { reason: kitchen::contracts::UncertainReason::LookupUnsupported, .. })
    ));
    // The fake refuses the lookup it did not declare.
    assert_eq!(
        orca.lookup(unsent.request()),
        Err(kitchen::contracts::BackendUnavailable::Unsupported(
            Capability::LookupMessageWorker
        ))
    );
    Ok(())
}

#[test]
fn capability_requirements_apply_only_to_their_executor() -> TestResult {
    let fixture = Fixture::new()?;
    let grants = grants_everywhere()?;
    let mut work = spec("task-1")?;
    work.repository = Some(km43()?);
    work.authority = TaskAuthority::delegate(&grants, grants_everywhere_list()?)?;
    work.requires = kitchen::contracts::CapabilityRequirements::new()
        .with(ExecutorKind::Worker, [Capability::WorkerLaunchReadiness]);
    let task = task_id("task-1")?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let clock = ManualClock::starting_at(1);

    // A forge executor does not need worker capabilities.
    let forge = executor(ExecutorKind::GitHub, Capability::ForgeMutation)?;
    let labelled = run_effect(
        &fixture.store,
        &forge,
        &grants,
        plan(&task, fence, "label", label(km43()?, "agent-ready")?)?,
        &clock,
    )?;
    assert!(matches!(labelled.state(), EffectState::Applied { .. }));

    // A worker backend without the required worker capability is refused.
    let workers = executor(ExecutorKind::Worker, Capability::WorkerLaunchIsolated)?;
    assert!(matches!(
        run_effect(&fixture.store, &workers, &grants, plan(&task, fence, "launch", launch()?)?, &clock),
        Err(Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. }))
            if missing == &[Capability::WorkerLaunchReadiness]
    ));
    Ok(())
}

#[test]
fn persisted_capability_requirements_reject_a_repeated_executor() -> TestResult {
    let requirements = kitchen::contracts::CapabilityRequirements::new()
        .with(ExecutorKind::Worker, [Capability::WorkerLaunchReadiness])
        .with(ExecutorKind::GitHub, [Capability::ForgeMutation]);
    let json = serde_json::to_string(&requirements)?;
    assert_eq!(
        json,
        r#"{"worker":["worker.launch_readiness"],"github":["forge.mutation"]}"#
    );
    assert_eq!(
        serde_json::from_str::<kitchen::contracts::CapabilityRequirements>(&json)?,
        requirements
    );
    let repeated = r#"{"worker":["worker.launch_readiness"],"worker":[]}"#;
    assert!(serde_json::from_str::<kitchen::contracts::CapabilityRequirements>(repeated).is_err());
    let unknown = r#"{"mainframe":["worker.launch_readiness"]}"#;
    assert!(serde_json::from_str::<kitchen::contracts::CapabilityRequirements>(unknown).is_err());

    // A stored task with a repeated executor is rejected on load, bytes kept.
    let fixture = Fixture::new()?;
    let mut work = spec("task-1")?;
    work.requires = requirements;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let path = fixture.state_path();
    let text = std::fs::read_to_string(&path)?;
    let corrupt = text.replacen("\"github\": [", "\"worker\": [", 1);
    assert_ne!(corrupt, text);
    std::fs::write(&path, &corrupt)?;
    assert!(fixture.reopen().is_err());
    assert_eq!(std::fs::read_to_string(&path)?, corrupt);
    Ok(())
}
