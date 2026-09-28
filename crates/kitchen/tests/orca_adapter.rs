//! Orca adapter behavior against a simulated Orca runtime (`orca_sim`).
//!
//! These are simulated results about argument construction, response
//! mapping, and recovery. Live runtime evidence comes only from `orca_live`.

mod common;
mod orca_sim;

use std::{collections::BTreeSet, time::Duration};

use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, house, other_house, scheduled, task_id,
    ttl,
};
use kitchen::{
    BackendId, ConsumerId, CredentialId, EffectName,
    adapters::orca::{
        MAX_INVENTORY_PAGES, MessageKind, OrcaBackend, OrcaConfig, OrcaError, RetainedReason,
        TerminalAccounting, launch_marker, verify_branch,
    },
    contracts::{
        AttemptNumber, BackendUnavailable, Capability, Effect, EffectExecutor, EffectFailure,
        EffectRequest, EvidenceRevision, ExternalRef, Grant, HouseGrants, IdempotencyKey, Liveness,
        Lookup, NotAppliedReason, Operation, Permission, Provenance, Repository, ResourceKind,
        ResourceRef, RetryPolicy, Role, ScheduleEffect, TaskAuthority, TaskSpec, Text,
        UncertainReason, WorkerBackend, WorkerOutcome, WorkerState, Workspace,
        conformance::{self, Check, CheckResult, ConformanceFixture},
    },
    scheduling::{
        AgentFamily, CronExpr, ObservedScheduleState, Precheck, PrecheckTimeout, Recurrence,
        RunOutcome, ScheduleSpec, ScheduleState, Timezone, WorkflowName,
    },
    state::{EffectPlan, EffectState, reconcile, run_effect},
};
use orca_sim::{Fault, SimAutomation, SimOrca, SimWorker};
use serde_json::json;

fn orca_id() -> TestResult<BackendId> {
    Ok(BackendId::new("orca-local")?)
}

fn credential() -> TestResult<CredentialId> {
    Ok(CredentialId::new("orca-host-session")?)
}

fn config() -> TestResult<OrcaConfig> {
    Ok(OrcaConfig {
        backend: orca_id()?,
        house: house()?,
        credential: credential()?,
        run: ExternalRef::new("run_sim")?,
        coordinator: ExternalRef::new("term_coordinator")?,
        repo: ExternalRef::new("id:repo-1")?,
        base_branch: Some(ExternalRef::new("main")?),
        agent: AgentFamily::Claude,
        call_timeout: Duration::from_secs(5),
        launch_timeout: Duration::from_secs(60),
    })
}

fn connect(sim: &SimOrca) -> TestResult<OrcaBackend<&SimOrca>> {
    Ok(OrcaBackend::connect(config()?, sim)?)
}

fn key(value: &str) -> TestResult<IdempotencyKey> {
    Ok(IdempotencyKey::from_ref(ExternalRef::new(value)?))
}

fn request(effect: impl Into<Effect>, key_text: &str) -> TestResult<EffectRequest> {
    Ok(EffectRequest::new(
        house()?,
        orca_id()?,
        credential()?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        key(key_text)?,
        effect.into(),
    ))
}

fn launch_op(brief: &str) -> TestResult<Operation> {
    Ok(Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new(brief)?,
    })
}

fn worker(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Worker,
        backend: orca_id()?,
        handle: ExternalRef::new(handle)?,
    })
}

fn launched(receipt: &kitchen::contracts::Receipt) -> TestResult<ResourceRef> {
    receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or_else(|| "receipt names no worker".into())
}

fn flag<'a>(call: &'a [String], name: &str) -> Option<&'a str> {
    call.iter()
        .find_map(|arg| arg.strip_prefix(&format!("--{name}=")))
}

#[test]
fn simulated_orca_passes_the_shared_worker_contract() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let fixture = ConformanceFixture {
        house: house()?,
        foreign_house: other_house()?,
        foreign_backend: BackendId::new("orca-other")?,
        credential: credential()?,
        task: task_id("conformance")?,
        repository: Repository::new("lemarier/kitchen")?,
        run_tag: ExternalRef::new("sim-run-1")?,
        brief: Text::new("Conformance probe; exit immediately.")?,
    };
    let report = conformance::run_worker(&backend, &fixture)?;
    for check in [
        Check::DescriptorHouse,
        Check::CrossHouseRefused,
        Check::ForeignBackendRefused,
        Check::UnsupportedRefused,
        Check::ProbeReceipt,
        Check::LaunchReceipt,
        Check::LaunchObservable,
        Check::InventoryListsLaunch,
        Check::CancelObserved,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
    assert!(
        sim.state()
            .calls
            .iter()
            .flatten()
            .all(|arg| !arg.starts_with("--retry-request")),
        "Kitchen keys never reach --retry-request"
    );
    // Lookup and idempotency are partial until the contract declares them
    // per operation; the suite must see them as not applicable.
    for (check, requires) in [
        (Check::UnknownKeyNotApplied, Capability::EffectLookup),
        (Check::LookupMatchesReceipt, Capability::EffectLookup),
        (
            Check::IdempotentResubmission,
            Capability::EffectIdempotentRequests,
        ),
    ] {
        assert_eq!(
            report.result(check),
            Some(CheckResult::NotApplicable { requires }),
            "{check}"
        );
    }
    Ok(())
}

#[test]
fn connect_refuses_unsupported_runtimes() -> TestResult {
    let old = SimOrca::default();
    old.state().version = "1.4.211";
    assert!(matches!(
        OrcaBackend::connect(config()?, &old),
        Err(OrcaError::UnsupportedVersion { found, .. }) if found == "1.4.211"
    ));

    let next_minor = SimOrca::default();
    next_minor.state().version = "1.5.0";
    assert!(matches!(
        OrcaBackend::connect(config()?, &next_minor),
        Err(OrcaError::UnsupportedVersion { .. })
    ));

    let missing = SimOrca::default();
    missing.state().features = vec!["orchestration.contract.v1"];
    assert_eq!(
        OrcaBackend::connect(config()?, &missing).err(),
        Some(OrcaError::MissingRuntimeFeature(
            "orchestration.worker-stop-verdict.v1"
        ))
    );

    let down = SimOrca::default();
    down.state().ready = false;
    assert_eq!(
        OrcaBackend::connect(config()?, &down).err(),
        Some(OrcaError::RuntimeNotReady)
    );

    let garbled = SimOrca::default();
    garbled.fault(Fault::Garbage);
    assert_eq!(
        OrcaBackend::connect(config()?, &garbled).err(),
        Some(OrcaError::Malformed { what: "envelope" })
    );

    let newer_patch = SimOrca::default();
    newer_patch.state().version = "1.4.230";
    let backend = connect(&newer_patch)?;
    assert_eq!(backend.runtime().version.to_string(), "1.4.230");
    assert!(
        !backend
            .descriptor()
            .capabilities
            .supports(Capability::ScheduleSingleConsumer),
        "Orca cannot prevent overlapping runs; Kitchen's lease must"
    );
    Ok(())
}

#[test]
fn launch_creates_one_keyed_task_with_separated_arguments() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let brief = "--help; $(rm -rf ~) 'quoted' \"double\"";
    let receipt = backend.execute(&request(launch_op(brief)?, "launch-1")?)?;
    let creates = sim.calls_to(&["orchestration", "task-create"]);
    let [create] = creates.as_slice() else {
        return Err("expected one task".into());
    };
    assert_eq!(flag(create, "spec"), Some(brief), "brief is one argv entry");
    let marker = launch_marker(&house()?, &key("launch-1")?);
    assert_eq!(flag(create, "task-title"), Some(marker.as_str()));
    assert_eq!(flag(create, "run"), Some("run_sim"));
    assert_eq!(flag(create, "from"), Some("term_coordinator"));
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    let [start] = starts.as_slice() else {
        return Err("expected one launch".into());
    };
    assert_eq!(flag(start, "task"), Some(receipt.reference().as_str()));
    assert_eq!(flag(start, "worktree"), Some("new-top-level"));
    assert_eq!(flag(start, "agent"), Some("claude"));
    assert_eq!(flag(start, "timeout-ms"), Some("60000"));
    assert_eq!(flag(start, "base-branch"), Some("main"));
    assert!(start.iter().all(|arg| !arg.starts_with("--retry-request")));
    let deadline = sim.state().deadlines.iter().max().copied();
    assert_eq!(
        deadline,
        Some(Duration::from_secs(90)),
        "the subprocess outlives Orca's own wait"
    );
    assert!(
        receipt
            .created()
            .iter()
            .any(|resource| resource.kind == ResourceKind::Worktree),
        "the created worktree is recorded for cleanup"
    );
    Ok(())
}

#[test]
fn resubmitting_a_launch_key_never_starts_a_second_worker() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let launch = request(launch_op("Implement it.")?, "launch-lost")?;
    sim.fault_on(
        &["orchestration", "worker-start"],
        Fault::TimeoutAfterEffect,
    );
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::Timeout))
    );
    let Lookup::Applied(found) = backend.lookup_launch(launch.key())? else {
        return Err("a dispatched launch must be found".into());
    };
    assert_eq!(backend.resolve(&launch)?, Lookup::Applied(found.clone()));
    assert_eq!(backend.execute(&launch)?, found, "resubmission returns it");
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(sim.state().workers.len(), 1);

    // The Task was created but its response lost: resubmission reuses it.
    sim.fault_on(&["orchestration", "task-create"], Fault::TimeoutAfterEffect);
    let second = request(launch_op("Implement it.")?, "launch-task-lost")?;
    assert_eq!(
        backend.execute(&second),
        Err(EffectFailure::Uncertain(UncertainReason::Timeout))
    );
    assert_eq!(
        backend.lookup_launch(second.key())?,
        Lookup::Unknown,
        "an undispatched Task is not an applied launch"
    );
    backend.execute(&second)?;
    assert_eq!(sim.state().tasks.len(), 2, "one Task per key");

    // Nothing reached Orca: still unknown, never proven absent.
    sim.fault_on(
        &["orchestration", "task-create"],
        Fault::TimeoutBeforeEffect,
    );
    let never = request(launch_op("Implement it.")?, "launch-never")?;
    assert!(backend.execute(&never).is_err());
    assert_eq!(backend.lookup_launch(never.key())?, Lookup::Unknown);
    Ok(())
}

#[test]
fn long_keys_still_deduplicate_under_orca_title_truncation() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let long = format!("kitchen-origin89-{}-00ff00ff00ff00ff-0", "t".repeat(64));
    let other = format!("kitchen-origin89-{}-00ff00ff00ff00ff-1", "t".repeat(64));
    let first = backend.execute(&request(launch_op("One.")?, &long)?)?;
    assert_eq!(
        backend.execute(&request(launch_op("One.")?, &long)?)?,
        first
    );
    let second = backend.execute(&request(launch_op("Two.")?, &other)?)?;
    assert_ne!(
        second, first,
        "keys sharing an 80-character prefix stay distinct"
    );
    assert_eq!(sim.state().workers.len(), 2);
    Ok(())
}

#[test]
fn refusals_distinguish_preflight_from_unknown_failures() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let cases = [
        (
            Fault::Refuse("task_not_found"),
            EffectFailure::NotApplied(NotAppliedReason::Rejected),
        ),
        (
            Fault::Refuse("runtime_error"),
            EffectFailure::Uncertain(UncertainReason::ResponseLost),
        ),
        (
            Fault::Garbage,
            EffectFailure::Uncertain(UncertainReason::ResponseLost),
        ),
    ];
    for (index, (fault, expected)) in cases.into_iter().enumerate() {
        sim.fault_on(&["orchestration", "worker-start"], fault);
        let launch = request(launch_op("Implement it.")?, &format!("refused-{index}"))?;
        assert_eq!(backend.execute(&launch), Err(expected), "{fault:?}");
    }
    sim.fault(Fault::Spawn);
    assert_eq!(
        backend.execute(&request(launch_op("x")?, "spawn")?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
        "nothing reached Orca"
    );
    assert!(sim.state().workers.is_empty(), "no worker was started");
    Ok(())
}

#[test]
fn requests_outside_this_instance_are_refused_before_any_call() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let calls_before = sim.state().calls.len();
    let other_credential = EffectRequest::new(
        house()?,
        orca_id()?,
        CredentialId::new("someone-else")?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        key("k")?,
        launch_op("Implement it.")?.into(),
    );
    assert_eq!(
        backend.execute(&other_credential),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    let worktree = ResourceRef {
        kind: ResourceKind::Worktree,
        ..worker("wt_1")?
    };
    assert_eq!(
        backend.execute(&request(
            Operation::ReleaseResource { resource: worktree },
            "release-worktree"
        )?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
        "Orca has no ownership-aware worktree release"
    );
    let foreign_worker = ResourceRef {
        backend: BackendId::new("orca-other")?,
        ..worker("ctx_9")?
    };
    assert_eq!(
        backend.execute(&request(
            Operation::CancelWorker {
                worker: foreign_worker.clone()
            },
            "cancel-foreign"
        )?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert_eq!(
        backend.observe_worker(&foreign_worker)?,
        WorkerState::Missing
    );
    assert_eq!(
        backend.lookup(&request(launch_op("x")?, "k")?),
        Err(BackendUnavailable::Unsupported(Capability::EffectLookup)),
        "partial lookup is reported as unsupported"
    );
    assert_eq!(sim.state().calls.len(), calls_before);
    Ok(())
}

#[test]
fn workers_outside_this_run_are_never_changed() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let mut elsewhere = SimWorker::new("ready", "in_progress", "live", false);
    elsewhere.run = "run_someone_else";
    sim.set_worker("ctx_elsewhere", elsewhere);
    let target = worker("ctx_elsewhere")?;
    for (index, operation) in [
        Operation::CancelWorker {
            worker: target.clone(),
        },
        Operation::ReleaseResource {
            resource: target.clone(),
        },
        Operation::MessageWorker {
            worker: target.clone(),
            body: Text::new("hello")?,
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            backend.execute(&request(operation, &format!("elsewhere-{index}"))?),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
    }
    assert!(sim.calls_to(&["orchestration", "worker-stop"]).is_empty());
    assert!(
        sim.calls_to(&["orchestration", "worker-release"])
            .is_empty()
    );
    assert!(sim.calls_to(&["orchestration", "send"]).is_empty());
    assert_eq!(
        backend.observe_worker(&target)?,
        WorkerState::Ready,
        "reading another Run's worker is harmless"
    );
    Ok(())
}

#[test]
fn worker_observation_needs_positive_evidence() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let cases = [
        (
            "starting",
            "in_progress",
            "unverifiable",
            false,
            WorkerState::Starting,
        ),
        ("ready", "in_progress", "live", false, WorkerState::Ready),
        (
            "ready",
            "in_progress",
            "live",
            true,
            WorkerState::AwaitingReply,
        ),
        (
            "ready",
            "in_progress",
            "unverifiable",
            false,
            WorkerState::Starting,
        ),
        // An exited agent without a report is not settled.
        (
            "ready",
            "finished_unverified",
            "exited",
            false,
            WorkerState::Unknown,
        ),
        (
            "succeeded",
            "succeeded",
            "exited",
            false,
            WorkerState::Settled(WorkerOutcome::Succeeded),
        ),
        (
            "failed",
            "failed",
            "exited",
            false,
            WorkerState::Settled(WorkerOutcome::Failed),
        ),
        (
            "abandoned",
            "abandoned",
            "unverifiable",
            false,
            WorkerState::Unknown,
        ),
    ];
    for (index, (worker_state, outcome, liveness, waiting, expected)) in
        cases.into_iter().enumerate()
    {
        let dispatch = format!("ctx_case{index}");
        sim.set_worker(
            &dispatch,
            SimWorker::new(worker_state, outcome, liveness, waiting),
        );
        assert_eq!(
            backend.observe_worker(&worker(&dispatch)?)?,
            expected,
            "{dispatch}"
        );
    }
    assert_eq!(
        backend.observe_worker(&worker("ctx_gone")?)?,
        WorkerState::Missing
    );
    sim.fault(Fault::TimeoutBeforeEffect);
    assert_eq!(
        backend.observe_worker(&worker("ctx_case1")?),
        Err(BackendUnavailable::Timeout)
    );
    sim.fault(Fault::Refuse("host_unavailable"));
    assert_eq!(
        backend.observe_worker(&worker("ctx_case1")?),
        Err(BackendUnavailable::Transport)
    );
    Ok(())
}

#[test]
fn messages_cancellation_and_release_map_orca_verdicts() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "launch")?)?;
    let target = launched(&receipt)?;

    let body = "--subject=forged\nplease rebase";
    let message = request(
        Operation::MessageWorker {
            worker: target.clone(),
            body: Text::new(body)?,
        },
        "message",
    )?;
    let sent = backend.execute(&message)?;
    assert!(
        sent.reference().as_str().starts_with("req-"),
        "Orca's request id"
    );
    let calls = sim.calls_to(&["orchestration", "send"]);
    let send = calls.first().ok_or("one send")?;
    assert_eq!(flag(send, "body"), Some(body));
    assert_eq!(flag(send, "subject"), Some("Kitchen coordinator"));
    assert_eq!(
        flag(send, "to").map(str::to_owned),
        Some(format!("dispatch:{}", target.handle))
    );
    assert_eq!(
        backend.resolve(&message)?,
        Lookup::Unknown,
        "Orca records no key for a message"
    );

    sim.state().stop_state = "stop_unknown";
    let cancel = request(
        Operation::CancelWorker {
            worker: target.clone(),
        },
        "cancel",
    )?;
    assert_eq!(
        backend.execute(&cancel),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost)),
        "an unproven stop is not a cancellation"
    );
    assert_eq!(backend.resolve(&cancel)?, Lookup::Unknown);
    sim.state().stop_state = "stopped";
    let stopped = backend.execute(&cancel)?;
    assert_eq!(backend.resolve(&cancel)?, Lookup::Applied(stopped.clone()));
    assert_eq!(
        backend.execute(&cancel)?,
        stopped,
        "stopping twice is harmless"
    );

    assert_eq!(
        backend.observe_worker(&target)?,
        WorkerState::Settled(WorkerOutcome::Cancelled),
        "a stop Orca projects as failed is still a cancellation"
    );

    // Release a second, settled worker.
    let other = launched(&backend.execute(&request(launch_op("Second.")?, "launch-2")?)?)?;
    sim.state()
        .workers
        .entry(other.handle.as_str().to_owned())
        .and_modify(|worker| {
            worker.worker_state = "succeeded";
            worker.outcome = "succeeded";
        });
    sim.state().release_action = "retained";
    let release = request(
        Operation::ReleaseResource {
            resource: other.clone(),
        },
        "release",
    )?;
    assert_eq!(
        backend.execute(&release),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert_eq!(backend.resolve(&release)?, Lookup::Unknown);
    sim.state().release_action = "released";
    let released = backend.execute(&release)?;
    assert_eq!(
        backend.resolve(&release)?,
        Lookup::Applied(released.clone())
    );
    sim.state().release_action = "already_released";
    assert_eq!(
        backend.execute(&release)?,
        released,
        "releasing twice is harmless"
    );
    Ok(())
}

#[test]
fn replies_answer_the_question_and_are_not_deduplicated() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "launch")?)?;
    let reply = request(
        Operation::ReplyToWorker {
            worker: launched(&receipt)?,
            question: ExternalRef::new("msg_question")?,
            body: Text::new("Use the existing parser.")?,
        },
        "reply-1",
    )?;
    backend.execute(&reply)?;
    let calls = sim.calls_to(&["orchestration", "reply"]);
    assert_eq!(
        calls.first().and_then(|call| flag(call, "id")),
        Some("msg_question")
    );
    backend.execute(&reply)?;
    assert_eq!(
        sim.calls_to(&["orchestration", "reply"]).len(),
        2,
        "Orca sends a resubmitted reply again, which is why idempotency is partial"
    );
    Ok(())
}

fn house_grants(permissions: &[Permission]) -> TestResult<HouseGrants> {
    let grants = permissions
        .iter()
        .map(|permission| Ok(Grant::house(*permission, orca_id()?, credential()?)))
        .collect::<TestResult<Vec<_>>>()?;
    Ok(HouseGrants::new(house()?, grants))
}

fn store_spec(id: &str) -> TestResult<TaskSpec> {
    let permissions = [Permission::LaunchWorker];
    let requested = permissions
        .iter()
        .map(|permission| Ok(Grant::house(*permission, orca_id()?, credential()?)))
        .collect::<TestResult<Vec<_>>>()?;
    Ok(TaskSpec {
        id: task_id(id)?,
        role: Role::StationCook,
        repository: None,
        authority: TaskAuthority::delegate(&house_grants(&permissions)?, requested)?,
        retry: RetryPolicy::new(3, Duration::from_secs(3600))?,
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
        requires: BTreeSet::new(),
        resources: BTreeSet::new(),
    })
}

#[test]
fn durable_uncertain_launch_is_held_not_relaunched() -> TestResult {
    let fixture = Fixture::new()?;
    let task = task_id("task-orca")?;
    fixture
        .store
        .create_task(store_spec("task-orca")?, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let clock = ManualClock::starting_at(1);
    let grants = house_grants(&[Permission::LaunchWorker])?;
    let plan = EffectPlan {
        task: task.clone(),
        fence,
        name: EffectName::new("launch")?,
        decided_at: EvidenceRevision::INITIAL,
        effect: launch_op("Implement the issue.")?.into(),
        consent: None,
    };

    sim.fault_on(
        &["orchestration", "worker-start"],
        Fault::TimeoutAfterEffect,
    );
    let record = run_effect(&fixture.store, &backend, &grants, plan.clone(), &clock)?;
    assert!(matches!(
        record.state(),
        EffectState::Uncertain {
            reason: UncertainReason::Timeout,
            ..
        }
    ));
    // A restarted coordinator cannot reconcile through partial lookup, so
    // the effect stays unresolved for a decision instead of relaunching.
    let report = reconcile(&fixture.reopen()?, &backend, &task, fence, &clock)?;
    assert!(report.resolved.is_empty());
    assert!(!report.unresolved.is_empty());
    assert!(run_effect(&fixture.store, &backend, &grants, plan, &clock).is_err());
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    // The evidence a decision needs is available: the launch did land.
    assert!(matches!(
        backend.lookup_launch(record.request().key())?,
        Lookup::Applied(_)
    ));
    Ok(())
}

#[test]
fn inventory_reports_owner_keys_and_refuses_partial_listings() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "launch-owned")?)?;
    let target = launched(&receipt)?;
    sim.set_worker(
        "ctx_foreign",
        SimWorker::new("succeeded", "succeeded", "exited", false),
    );
    let observations = backend.inventory()?;
    let ours = observations
        .iter()
        .find(|observation| observation.resource == target)
        .ok_or("launched worker listed")?;
    assert_eq!(
        ours.owner.as_ref().map(ExternalRef::as_str),
        Some(launch_marker(&house()?, &key("launch-owned")?).as_str())
    );
    assert_eq!(ours.liveness, Liveness::Live);
    let foreign = observations
        .iter()
        .find(|observation| observation.resource.handle.as_str() == "ctx_foreign")
        .ok_or("foreign worker listed")?;
    assert_eq!(foreign.owner, None, "not launched by this house");
    assert_eq!(foreign.liveness, Liveness::Exited);

    let row = |dispatch: &str, terminal: &str, outcome: &str, verdict: &str| {
        json!({
            "dispatchId": dispatch,
            "taskId": "task_x",
            "workerState": "succeeded",
            "terminalState": terminal,
            "projection": {"outcome": outcome, "liveness": {"verdict": verdict}},
        })
    };
    sim.state().worker_pages = vec![
        json!({"workers": [row("ctx_a", "reclaimable", "succeeded", "exited")], "page": {"hasMore": true, "nextCursor": "p1"}}),
        json!({"workers": [row("ctx_b", "odd_state", "in_progress", "stale")], "page": {"hasMore": false}}),
    ];
    let records = backend.worker_records()?;
    assert_eq!(
        records.iter().map(|r| r.terminal).collect::<Vec<_>>(),
        [TerminalAccounting::Reclaimable, TerminalAccounting::Unknown]
    );
    assert_eq!(
        records.get(1).map(|r| (r.liveness, r.state)),
        Some((Liveness::Unverifiable, WorkerState::Unknown))
    );
    let pages = sim.calls_to(&["orchestration", "worker-list"]);
    assert_eq!(
        pages.last().and_then(|call| flag(call, "cursor")),
        Some("p1")
    );

    let endless: Vec<_> = (0..=MAX_INVENTORY_PAGES)
        .map(|n| json!({"workers": [], "page": {"hasMore": true, "nextCursor": format!("p{}", n + 1)}}))
        .collect();
    sim.state().worker_pages = endless;
    assert_eq!(backend.inventory(), Err(BackendUnavailable::LimitExceeded));
    Ok(())
}

#[test]
fn mailbox_messages_are_typed_and_acknowledged_explicitly() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    assert_eq!(backend.next_delivery()?, None);
    sim.state().mail = json!({
        "deliveryId": "delivery_1",
        "messages": [
            {"id": "msg_q", "type": "question", "subject": "Which parser?", "body": "A or B?", "payload": {"dispatchId": "ctx_1"}},
            {"id": "msg_d", "type": "worker_done", "subject": "done", "body": "All tests pass.", "payload": {"taskId": "task_1", "dispatchId": "ctx_1", "outcome": "succeeded"}},
            {"id": "msg_h", "type": "heartbeat", "subject": "alive", "payload": {"dispatchId": "ctx_1", "outcome": "succeeded"}},
            {"id": "has space", "type": "status", "subject": "x"},
        ],
    });
    let delivery = backend.next_delivery()?.ok_or("a delivery")?;
    assert_eq!(delivery.id.as_str(), "delivery_1");
    assert_eq!(delivery.unreadable, 1);
    let kinds: Vec<_> = delivery
        .messages
        .iter()
        .map(|m| (m.kind, m.outcome))
        .collect();
    assert_eq!(
        kinds,
        [
            (MessageKind::Question, None),
            (MessageKind::WorkerDone, Some(WorkerOutcome::Succeeded)),
            (MessageKind::Heartbeat, None),
        ],
        "only worker_done carries an outcome; a heartbeat is not completion"
    );
    assert_eq!(
        delivery.messages.first().and_then(|m| m.worker.clone()),
        Some(worker("ctx_1")?)
    );
    backend.acknowledge(&delivery.id)?;
    let checks = sim.calls_to(&["orchestration", "check"]);
    assert_eq!(
        checks
            .iter()
            .filter(|call| flag(call, "ack").is_some())
            .count(),
        1
    );
    assert!(
        checks
            .iter()
            .all(|call| flag(call, "terminal") == Some("term_coordinator"))
    );
    Ok(())
}

#[test]
fn adoption_moves_the_mailbox_to_the_new_coordinator() -> TestResult {
    let sim = SimOrca::default();
    let old = connect(&sim)?;
    old.adopt_run()?;
    assert_eq!(old.next_delivery()?, None);
    let adopting = OrcaBackend::connect(
        OrcaConfig {
            coordinator: ExternalRef::new("term_adopter")?,
            ..config()?
        },
        &sim,
    )?;
    adopting.adopt_run()?;
    let calls = sim.calls_to(&["orchestration", "run-use"]);
    assert_eq!(
        calls.last().and_then(|call| flag(call, "from")),
        Some("term_adopter")
    );
    assert_eq!(
        calls.last().and_then(|call| flag(call, "id")),
        Some("run_sim")
    );
    assert_eq!(adopting.next_delivery()?, None);
    assert!(
        matches!(old.next_delivery(), Err(OrcaError::Refused { code, .. }) if code == "consumer_fenced"),
        "the previous coordinator no longer reads the mailbox"
    );
    Ok(())
}

fn schedule_spec(consumer: &str) -> TestResult<ScheduleSpec> {
    let precheck = Precheck::new(
        vec![
            Text::new("kitchen")?,
            Text::new("precheck")?,
            Text::new("it's pickup")?,
        ],
        PrecheckTimeout::new(Duration::from_secs(60))?,
    )?;
    Ok(ScheduleSpec::new(
        WorkflowName::new("pickup")?,
        ConsumerId::new(consumer)?,
        Recurrence::Cron(CronExpr::new("17,37,57 * * * *")?),
        Timezone::new("America/Toronto")?,
        Text::new("Run Kitchen pickup.")?,
        AgentFamily::Claude,
    )
    .with_precheck(precheck))
}

fn install(consumer: &str) -> TestResult<ScheduleEffect> {
    Ok(ScheduleEffect::InstallDisabled {
        schedule: schedule_spec(consumer)?,
    })
}

fn automation(id: &str, name: &str, enabled: bool) -> SimAutomation {
    SimAutomation {
        id: id.to_owned(),
        name: name.to_owned(),
        enabled,
    }
}

fn schedule(id: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Schedule,
        backend: orca_id()?,
        handle: ExternalRef::new(id)?,
    })
}

#[test]
fn install_creates_paused_once_and_reuses_it() -> TestResult {
    let sim = SimOrca::default();
    sim.state()
        .automations
        .push(automation("live-1", "Origin89 issue coordinator", true));
    let backend = connect(&sim)?;
    let effect = request(install("pickup")?, "install-1")?;
    let receipt = backend.execute(&effect)?;
    let installed = receipt.created().first().cloned().ok_or("a schedule")?;
    assert_eq!(installed.kind, ResourceKind::Schedule);
    let creates = sim.calls_to(&["automations", "create"]);
    let create = creates.first().ok_or("one create")?;
    assert!(create.iter().any(|arg| arg == "--disabled"));
    assert!(!create.iter().any(|arg| arg == "--enabled"));
    assert_eq!(flag(create, "name"), Some("kitchen:origin89:pickup"));
    assert_eq!(
        flag(create, "precheck"),
        Some(r"'kitchen' 'precheck' 'it'\''s pickup'")
    );
    assert_eq!(flag(create, "precheck-timeout"), Some("60"));
    assert_eq!(flag(create, "trigger"), Some("17,37,57 * * * *"));
    assert_eq!(flag(create, "timezone"), Some("America/Toronto"));

    assert_eq!(
        backend.execute(&request(install("pickup")?, "install-2")?)?,
        receipt,
        "reinstalling for the same consumer reuses it"
    );
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    assert_eq!(backend.resolve(&effect)?, Lookup::Applied(receipt));
    let listed = backend.installed_schedules()?;
    assert_eq!(listed.len(), 1, "the live automation is not Kitchen's");
    assert_eq!(
        listed.first().map(|s| s.state),
        Some(ObservedScheduleState::Paused)
    );
    Ok(())
}

#[test]
fn lost_install_response_is_reconciled_never_recreated() -> TestResult {
    // Orca created the schedule but the response was lost: the listing finds it.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.fault_on(&["automations", "create"], Fault::TimeoutAfterEffect);
    let installed = backend.install_schedule(&schedule_spec("pickup")?)?;
    assert_eq!(installed.handle.as_str(), "auto-1");
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);

    // The create timed out and no listing shows it: unknown, not retried.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.fault_on(&["automations", "create"], Fault::TimeoutBeforeEffect);
    let effect = request(install("pickup")?, "install-lost")?;
    assert_eq!(
        backend.execute(&effect),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    assert_eq!(backend.resolve(&effect)?, Lookup::Unknown);

    // An unreadable listing blocks the create entirely.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.fault_on(&["automations", "list"], Fault::Garbage);
    assert_eq!(
        backend.install_schedule(&schedule_spec("pickup")?),
        Err(OrcaError::Malformed { what: "envelope" })
    );
    assert!(sim.calls_to(&["automations", "create"]).is_empty());
    Ok(())
}

#[test]
fn duplicate_schedules_block_installation() -> TestResult {
    let sim = SimOrca::default();
    {
        let mut state = sim.state();
        state
            .automations
            .push(automation("a1", "kitchen:origin89:pickup", false));
        state
            .automations
            .push(automation("a2", "kitchen:origin89:pickup", true));
        state
            .automations
            .push(automation("a3", "kitchen:crabnebula:pickup", false));
    }
    let backend = connect(&sim)?;
    assert_eq!(
        backend.execute(&request(install("pickup")?, "install")?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert_eq!(
        backend.install_schedule(&schedule_spec("pickup")?),
        Err(OrcaError::DuplicateSchedules { count: 2 })
    );
    assert!(sim.calls_to(&["automations", "create"]).is_empty());
    Ok(())
}

#[test]
fn schedule_changes_are_owned_and_read_back() -> TestResult {
    let sim = SimOrca::default();
    {
        let mut state = sim.state();
        state
            .automations
            .push(automation("live-1", "Origin89 issue coordinator", true));
        state.automations.push(automation(
            "other-house",
            "kitchen:crabnebula:pickup",
            false,
        ));
    }
    let backend = connect(&sim)?;
    for foreign in ["live-1", "other-house"] {
        for effect in [
            ScheduleEffect::SetState {
                schedule: schedule(foreign)?,
                state: ScheduleState::Paused,
            },
            ScheduleEffect::Remove {
                schedule: schedule(foreign)?,
            },
            ScheduleEffect::Trial {
                schedule: schedule(foreign)?,
            },
        ] {
            assert_eq!(
                backend.execute(&request(effect, "foreign")?),
                Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
                "{foreign}"
            );
        }
    }
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(sim.calls_to(&["automations", "remove"]).is_empty());
    assert!(sim.calls_to(&["automations", "run"]).is_empty());

    let ours = backend.install_schedule(&schedule_spec("pickup")?)?;
    backend.execute(&request(
        ScheduleEffect::Trial {
            schedule: ours.clone(),
        },
        "trial",
    )?)?;
    assert_eq!(
        sim.calls_to(&["automations", "run"]).len(),
        1,
        "a trial runs while paused"
    );
    let activate = request(
        ScheduleEffect::SetState {
            schedule: ours.clone(),
            state: ScheduleState::Active,
        },
        "activate",
    )?;
    let active = backend.execute(&activate)?;
    assert_eq!(backend.resolve(&activate)?, Lookup::Applied(active));
    assert_eq!(
        backend.trial_schedule(&ours),
        Err(OrcaError::TrialRequiresPaused)
    );
    sim.state().ignore_edits = true;
    assert_eq!(
        backend.execute(&request(
            ScheduleEffect::SetState {
                schedule: ours.clone(),
                state: ScheduleState::Paused,
            },
            "pause-ignored",
        )?),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost)),
        "an edit that did not take effect is not reported as done"
    );
    sim.state().ignore_edits = false;
    backend.set_schedule_state(&ours, ScheduleState::Paused)?;

    // Orca records every non-zero precheck exit as a skip; the recorded
    // result keeps idle apart from errors.
    sim.state().runs = vec![
        json!({"status": "completed", "scheduledFor": 1000}),
        json!({"status": "dispatch_failed", "scheduledFor": 5000}),
        json!({"status": "skipped_precheck", "scheduledFor": 4000,
               "precheckResult": {"exitCode": 1, "timedOut": false, "error": null}}),
        json!({"status": "skipped_precheck", "scheduledFor": 3000,
               "precheckResult": {"exitCode": 2, "timedOut": false, "error": null}}),
        json!({"status": "skipped_precheck", "scheduledFor": 2000,
               "precheckResult": {"exitCode": null, "timedOut": true, "error": null}}),
        json!({"status": "surprise"}),
    ];
    let observed = backend.inspect_schedule(&ours)?;
    assert_eq!(observed.state, ObservedScheduleState::Paused);
    let outcomes: Vec<_> = observed.recent_runs.iter().map(|run| run.outcome).collect();
    assert_eq!(
        outcomes,
        [
            RunOutcome::LaunchFailed,
            RunOutcome::PrecheckIdle,
            RunOutcome::PrecheckFailed,
            RunOutcome::PrecheckFailed,
            RunOutcome::LaunchReported,
            RunOutcome::Unknown
        ]
    );

    let remove = request(
        ScheduleEffect::Remove {
            schedule: ours.clone(),
        },
        "remove",
    )?;
    let removed = backend.execute(&remove)?;
    assert_eq!(
        backend.inspect_schedule(&ours)?.state,
        ObservedScheduleState::Missing
    );
    assert_eq!(
        backend.execute(&remove)?,
        removed,
        "removing twice is harmless"
    );
    assert_eq!(backend.resolve(&remove)?, Lookup::Applied(removed));
    Ok(())
}

#[test]
fn a_launch_that_never_became_ready_is_a_failed_launch() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.state().start_state = "failed";
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "failed-start")?)?;
    assert_eq!(
        backend.observe_worker(&launched(&receipt)?)?,
        WorkerState::Settled(WorkerOutcome::Failed),
        "a failed start is a failed launch, never ready or completed"
    );
    // Accepted but never shown running: not ready, not settled.
    sim.set_worker(
        "ctx_quiet",
        SimWorker::new("ready", "in_progress", "unverifiable", false),
    );
    assert_eq!(
        backend.observe_worker(&worker("ctx_quiet")?)?,
        WorkerState::Starting
    );
    Ok(())
}

#[test]
fn launch_receipts_name_the_branch_orca_created() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "branch")?)?;
    let requested = format!("kitchen-{}", receipt.reference());
    let actual = format!("lemarier/{requested}");
    assert!(receipt.created().iter().any(
        |resource| resource.kind == ResourceKind::Branch && resource.handle.as_str() == actual
    ));
    assert_eq!(verify_branch(&receipt, &actual), Ok(()));
    assert_eq!(
        verify_branch(&receipt, &requested),
        Err(OrcaError::BranchMismatch {
            requested: requested.clone(),
            actual: Some(actual),
        }),
        "the prefix Orca added is reported, not hidden"
    );
    assert_eq!(
        backend.lookup_launch(&key("branch")?)?,
        Lookup::Applied(receipt),
        "lookup derives the same receipt from Orca's record"
    );
    Ok(())
}

#[test]
fn a_terminal_a_person_took_over_is_retained_and_left_alone() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "taken")?)?;
    let target = launched(&receipt)?;
    sim.state()
        .workers
        .entry(target.handle.as_str().to_owned())
        .and_modify(|worker| {
            worker.ownership = "user_owned";
            worker.retained_reason = Some("user_takeover");
        });
    for (index, operation) in [
        Operation::MessageWorker {
            worker: target.clone(),
            body: Text::new("Start the next step.")?,
        },
        Operation::ReplyToWorker {
            worker: target.clone(),
            question: ExternalRef::new("msg_q")?,
            body: Text::new("Yes.")?,
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            backend.execute(&request(operation, &format!("to-person-{index}"))?),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
    }
    assert_eq!(
        backend.execute(&request(
            Operation::CancelWorker {
                worker: target.clone()
            },
            "stop-person"
        )?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
        "a person's terminal is not stopped without asking"
    );
    assert!(sim.calls_to(&["orchestration", "send"]).is_empty());
    assert!(sim.calls_to(&["orchestration", "reply"]).is_empty());
    assert!(sim.calls_to(&["orchestration", "worker-stop"]).is_empty());
    let records = backend.worker_records()?;
    let record = records
        .iter()
        .find(|record| record.worker == target)
        .ok_or("listed")?;
    assert_eq!(record.retained, Some(RetainedReason::UserTakeover));
    assert_eq!(record.terminal, TerminalAccounting::Retained);
    assert_eq!(
        record.state,
        WorkerState::Ready,
        "a takeover is not a failure"
    );
    sim.state().release_action = "retained";
    assert_eq!(
        backend.execute(&request(
            Operation::ReleaseResource { resource: target },
            "release-taken"
        )?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
        "a retained terminal is not released and not failed"
    );
    Ok(())
}

#[test]
fn heartbeats_do_not_end_a_wait() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.state().mail = json!({
        "deliveryId": "delivery_h",
        "messages": [
            {"id": "msg_h1", "type": "heartbeat", "subject": "alive", "payload": {"dispatchId": "ctx_1"}},
            {"id": "msg_s", "type": "status", "subject": "note"},
        ],
    });
    let delivery = backend
        .await_delivery(Duration::from_secs(60))?
        .ok_or("a batch")?;
    assert_eq!(
        delivery.actionable().count(),
        0,
        "heartbeats and notes are liveness only"
    );
    let calls = sim.calls_to(&["orchestration", "check"]);
    let wait = calls.last().ok_or("a wait")?;
    assert!(wait.iter().any(|arg| arg == "--wait"));
    assert_eq!(flag(wait, "types"), Some("worker_done,escalation,question"));
    assert_eq!(flag(wait, "timeout-ms"), Some("60000"));
    assert_eq!(
        sim.state().deadlines.last().copied(),
        Some(Duration::from_secs(65)),
        "the subprocess outlives Orca's wait"
    );
    backend.await_delivery(Duration::from_secs(86_400))?;
    let calls = sim.calls_to(&["orchestration", "check"]);
    assert_eq!(
        calls.last().and_then(|call| flag(call, "timeout-ms")),
        Some("900000"),
        "waits are bounded"
    );

    sim.state().mail = json!({
        "deliveryId": "delivery_d",
        "messages": [
            {"id": "msg_h2", "type": "heartbeat", "subject": "alive"},
            {"id": "msg_d", "type": "worker_done", "subject": "done", "payload": {"dispatchId": "ctx_1", "outcome": "failed"}},
        ],
    });
    let delivery = backend
        .await_delivery(Duration::from_secs(60))?
        .ok_or("a batch")?;
    let actionable: Vec<_> = delivery
        .actionable()
        .map(|message| (message.kind, message.outcome))
        .collect();
    assert_eq!(
        actionable,
        [(MessageKind::WorkerDone, Some(WorkerOutcome::Failed))]
    );
    Ok(())
}
