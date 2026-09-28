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
        AttemptNumber, BackendUnavailable, BranchName, Capability, CapabilityRequirements, Effect,
        EffectExecutor, EffectFailure, EffectRequest, EvidenceRevision, ExternalRef, Grant,
        HouseGrants, IdempotencyKey, Liveness, Lookup, NotAppliedReason, Operation, Permission,
        Provenance, Repository, ResourceKind, ResourceRef, RetryPolicy, Role, ScheduleEffect,
        TaskAuthority, TaskSpec, Text, Timestamp, UncertainReason, WorkerBackend, WorkerOutcome,
        WorkerState, Workspace,
        conformance::{self, Check, CheckResult, ConformanceFixture},
    },
    scheduling::{
        AgentFamily, CronExpr, GraceMinutes, MAX_SCHEDULE_RUNS, ObservedScheduleState, Precheck,
        PrecheckTimeout, Readiness, ReadinessSignal, Recurrence, RunOutcome, RunVerdict,
        ScheduleField, ScheduleSpec, ScheduleState, ScheduleWorkspace, TimeOfDay, Timezone,
        WorkflowName,
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

fn config(sim: &SimOrca) -> TestResult<OrcaConfig> {
    Ok(OrcaConfig {
        backend: orca_id()?,
        house: house()?,
        credential: credential()?,
        run: ExternalRef::new("run_sim")?,
        coordinator: ExternalRef::new("term_coordinator")?,
        repo: ExternalRef::new("id:repo-1")?,
        base_branch: Some(ExternalRef::new("main")?),
        branch_prefix: Some(BranchName::new("lemarier")?),
        agent: AgentFamily::Claude,
        call_timeout: Duration::from_secs(5),
        launch_timeout: Duration::from_secs(60),
        runtime_dir: sim.runtime_dir()?,
        reservation_timeout: Duration::from_secs(10),
    })
}

fn connect(sim: &SimOrca) -> TestResult<OrcaBackend<&SimOrca>> {
    Ok(OrcaBackend::connect(config(sim)?, sim)?)
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
        branch: None,
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

fn assert_shared_suite_passed(report: &conformance::ConformanceReport) {
    for check in [
        Check::DescriptorHouse,
        Check::CrossHouseRefused,
        Check::ForeignBackendRefused,
        Check::UnsupportedRefused,
        Check::ProbeReceipt,
        Check::UnknownKeyNotApplied,
        Check::LookupMatchesReceipt,
        Check::IdempotentResubmission,
        Check::LaunchReceipt,
        Check::LaunchObservable,
        Check::InventoryListsLaunch,
        Check::MessageRecovery,
        Check::CancelObserved,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
}

#[test]
fn simulated_orca_passes_the_shared_worker_contract() -> TestResult {
    let sim = SimOrca::default();
    // The default suite launches on `kitchen/<run tag>`: a host whose branch
    // prefix is `kitchen`.
    sim.state().branch_prefix = "kitchen/";
    let backend = OrcaBackend::connect(conformance_config(&sim)?, &sim)?;
    let report = conformance::run_worker(&backend, &conformance_fixture()?)?;
    assert_shared_suite_passed(&report);
    assert!(
        sim.state()
            .calls
            .iter()
            .flatten()
            .all(|arg| !arg.starts_with("--retry-request")),
        "Kitchen keys never reach --retry-request"
    );
    // The exact branch was requested by name, under the host's prefix.
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    assert_eq!(
        starts.first().and_then(|call| flag(call, "name")),
        Some("sim-run-1")
    );
    Ok(())
}

fn conformance_fixture() -> TestResult<ConformanceFixture> {
    Ok(ConformanceFixture {
        house: house()?,
        foreign_house: other_house()?,
        foreign_backend: BackendId::new("orca-other")?,
        credential: credential()?,
        task: task_id("conformance")?,
        repository: Repository::new("lemarier/kitchen")?,
        run_tag: ExternalRef::new("sim-run-1")?,
        brief: Text::new("Conformance probe; exit immediately.")?,
    })
}

fn conformance_config(sim: &SimOrca) -> TestResult<OrcaConfig> {
    Ok(OrcaConfig {
        branch_prefix: Some(BranchName::new("kitchen")?),
        ..config(sim)?
    })
}

#[test]
fn simulated_orca_passes_the_shared_contract_on_a_branch_under_its_own_prefix() -> TestResult {
    // The host's prefix is `lemarier` (the sim's default, and the config's):
    // the backend under test supplies a branch it can obtain there.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let branch = BranchName::new("lemarier/kitchen-sim-run-1")?;
    let report = conformance::run_worker_on_branch(&backend, &conformance_fixture()?, &branch)?;
    assert_shared_suite_passed(&report);
    // The exact branch was requested by name, under the host's prefix, and
    // the receipt names exactly it.
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    assert_eq!(
        starts.first().and_then(|call| flag(call, "name")),
        Some("kitchen-sim-run-1")
    );
    let probe = backend.lookup_launch(&key("sim-run-1-probe")?)?;
    let Lookup::Applied(receipt) = probe else {
        return Err("the probe launch was not found by its key".into());
    };
    verify_branch(&receipt, branch.as_str())?;
    Ok(())
}

#[test]
fn a_supplied_branch_under_another_prefix_is_refused_before_anything_exists() -> TestResult {
    for (host, supplied) in [
        ("lemarier/", "kitchen/kitchen-sim-run-1"),
        ("lemarier/", "lemarierx/kitchen-sim-run-1"),
        ("lemarier/", "kitchen-sim-run-1"),
        ("kitchen/", "lemarier/kitchen-sim-run-1"),
    ] {
        let sim = SimOrca::default();
        sim.state().branch_prefix = host;
        let backend = OrcaBackend::connect(
            OrcaConfig {
                branch_prefix: Some(BranchName::new(host.trim_end_matches('/'))?),
                ..config(&sim)?
            },
            &sim,
        )?;
        let failure = conformance::run_worker_on_branch(
            &backend,
            &conformance_fixture()?,
            &BranchName::new(supplied)?,
        )
        .err()
        .ok_or("a launch on a branch the host cannot create passed")?;
        assert_eq!(failure.check, Check::ProbeReceipt, "{host} {supplied}");
        assert_eq!(failure.problem, "probe was refused", "{host} {supplied}");
        assert!(
            sim.calls_to(&["orchestration", "task-create"]).is_empty(),
            "{host} {supplied}"
        );
        assert!(
            sim.calls_to(&["orchestration", "worker-start"]).is_empty(),
            "{host} {supplied}"
        );
    }
    Ok(())
}

#[test]
fn a_host_whose_actual_prefix_differs_from_the_configured_one_fails_and_stops() -> TestResult {
    // The configuration says `lemarier`, the host says `kitchen`: the name is
    // accepted, Orca creates `kitchen/<name>`, and the launch is held after
    // the worker is stopped rather than passing on the wrong branch.
    let sim = SimOrca::default();
    sim.state().branch_prefix = "kitchen/";
    let backend = connect(&sim)?;
    let branch = BranchName::new("lemarier/kitchen-sim-run-1")?;
    let failure = conformance::run_worker_on_branch(&backend, &conformance_fixture()?, &branch)
        .err()
        .ok_or("a launch on another branch than requested passed")?;
    assert_eq!(failure.check, Check::ProbeReceipt);
    assert_eq!(failure.problem, "probe outcome was uncertain");
    assert_eq!(sim.calls_to(&["orchestration", "worker-stop"]).len(), 1);
    Ok(())
}

#[test]
fn the_shared_suite_is_refused_on_a_host_that_prefixes_otherwise() -> TestResult {
    // Orca's CLI cannot create `kitchen/<tag>` on a host that prefixes
    // `lemarier/`, so the exact-branch launch is refused before anything
    // exists rather than run on another branch.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    assert!(conformance::run_worker(&backend, &conformance_fixture()?).is_err());
    assert!(sim.calls_to(&["orchestration", "task-create"]).is_empty());
    assert!(sim.calls_to(&["orchestration", "worker-start"]).is_empty());
    Ok(())
}

#[test]
fn connect_refuses_unsupported_runtimes() -> TestResult {
    let old = SimOrca::default();
    old.state().version = "1.4.211";
    assert!(matches!(
        OrcaBackend::connect(config(&old)?, &old),
        Err(OrcaError::UnsupportedVersion { found, .. }) if found == "1.4.211"
    ));

    let next_minor = SimOrca::default();
    next_minor.state().version = "1.5.0";
    assert!(matches!(
        OrcaBackend::connect(config(&next_minor)?, &next_minor),
        Err(OrcaError::UnsupportedVersion { .. })
    ));

    let missing = SimOrca::default();
    missing.state().features = vec!["orchestration.contract.v1"];
    assert_eq!(
        OrcaBackend::connect(config(&missing)?, &missing).err(),
        Some(OrcaError::MissingRuntimeFeature(
            "orchestration.worker-stop-verdict.v1"
        ))
    );

    let down = SimOrca::default();
    down.state().ready = false;
    assert_eq!(
        OrcaBackend::connect(config(&down)?, &down).err(),
        Some(OrcaError::RuntimeNotReady)
    );

    let garbled = SimOrca::default();
    garbled.fault(Fault::Garbage);
    assert_eq!(
        OrcaBackend::connect(config(&garbled)?, &garbled).err(),
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
        // Orca raises this from inside a start, after a worktree and terminal
        // exist; only a message or reply gets it before any effect.
        (
            Fault::Refuse("dispatch_inactive"),
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
    // Lookup is declared per kind: a message has no key Orca records.
    assert_eq!(
        backend.lookup(&request(
            Operation::MessageWorker {
                worker: worker("ctx_1")?,
                body: Text::new("hello")?,
            },
            "k"
        )?),
        Err(BackendUnavailable::Unsupported(
            Capability::LookupMessageWorker
        )),
        "an undeclared kind is reported as unsupported"
    );
    assert_eq!(sim.state().calls.len(), calls_before);
    // A declared kind is looked up, and a key Orca never saw is unknown.
    assert_eq!(
        backend.lookup(&request(launch_op("x")?, "k")?),
        Ok(Lookup::Unknown)
    );
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
        "Orca sends a resubmitted reply again, which is why replies declare no idempotency"
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
        requires: CapabilityRequirements::new(),
        resources: BTreeSet::new(),
    })
}

/// A claimed task with an attempt running, holding `permissions`.
fn running_task(
    fixture: &Fixture,
    id: &str,
    permissions: &[Permission],
) -> TestResult<(kitchen::TaskId, kitchen::contracts::Fence)> {
    let task = task_id(id)?;
    let requested = permissions
        .iter()
        .map(|permission| Ok(Grant::house(*permission, orca_id()?, credential()?)))
        .collect::<TestResult<Vec<_>>>()?;
    let spec = TaskSpec {
        authority: TaskAuthority::delegate(&house_grants(permissions)?, requested)?,
        ..store_spec(id)?
    };
    fixture.store.create_task(spec, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator-a")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

fn plan(
    task: &kitchen::TaskId,
    fence: kitchen::contracts::Fence,
    name: &str,
    effect: impl Into<Effect>,
) -> TestResult<EffectPlan> {
    Ok(EffectPlan {
        task: task.clone(),
        fence,
        name: EffectName::new(name)?,
        decided_at: EvidenceRevision::INITIAL,
        effect: effect.into(),
        consent: None,
    })
}

#[test]
fn a_lost_launch_response_is_recovered_by_lookup_without_relaunching() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = running_task(&fixture, "task-orca", &[Permission::LaunchWorker])?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let clock = ManualClock::starting_at(1);
    let grants = house_grants(&[Permission::LaunchWorker])?;
    let launch = plan(&task, fence, "launch", launch_op("Implement the issue.")?)?;

    sim.fault_on(
        &["orchestration", "worker-start"],
        Fault::TimeoutAfterEffect,
    );
    let record = run_effect(&fixture.store, &backend, &grants, launch.clone(), &clock)?;
    assert!(matches!(
        record.state(),
        EffectState::Uncertain {
            reason: UncertainReason::Timeout,
            ..
        }
    ));
    // A restarted coordinator reconciles through the declared lookup: the
    // launch did land, so it resolves as applied.
    let report = reconcile(&fixture.reopen()?, &backend, &task, fence, &clock)?;
    assert!(report.unresolved.is_empty());
    let [resolved] = report.resolved.as_slice() else {
        return Err("expected one resolved effect".into());
    };
    let EffectState::Applied { receipt, .. } = resolved.state() else {
        return Err("the launch was not resolved as applied".into());
    };
    assert_eq!(
        backend.lookup_launch(record.request().key())?,
        Lookup::Applied(receipt.clone())
    );
    // Running the effect again finds it resolved and starts nothing.
    let again = run_effect(&fixture.store, &backend, &grants, launch, &clock)?;
    assert!(matches!(again.state(), EffectState::Applied { .. }));
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    Ok(())
}

#[test]
fn a_lost_message_response_is_held_and_never_resent() -> TestResult {
    let fixture = Fixture::new()?;
    let permissions = [Permission::LaunchWorker, Permission::MessageWorker];
    let (task, fence) = running_task(&fixture, "task-message", &permissions)?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let clock = ManualClock::starting_at(1);
    let grants = house_grants(&permissions)?;
    let landed = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "launch", launch_op("Implement the issue.")?)?,
        &clock,
    )?;
    let EffectState::Applied { receipt, .. } = landed.state() else {
        return Err("the launch was not applied".into());
    };
    let message = plan(
        &task,
        fence,
        "message",
        Operation::MessageWorker {
            worker: launched(receipt)?,
            body: Text::new("Start the next step.")?,
        },
    )?;
    sim.fault_on(&["orchestration", "send"], Fault::TimeoutAfterEffect);
    let record = run_effect(&fixture.store, &backend, &grants, message.clone(), &clock)?;
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    // Orca records no key for a message: it cannot be looked up, so the
    // effect stays unresolved, and resubmitting could send it twice.
    let report = reconcile(&fixture.reopen()?, &backend, &task, fence, &clock)?;
    assert!(report.resolved.is_empty());
    assert!(!report.unresolved.is_empty());
    assert!(run_effect(&fixture.store, &backend, &grants, message, &clock).is_err());
    assert_eq!(sim.calls_to(&["orchestration", "send"]).len(), 1);
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
            ..config(&sim)?
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
        ..SimAutomation::default()
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
    let readiness = Readiness::new(&[], at(0), Duration::from_secs(300));
    let observed = backend.inspect_schedule(&ours, &readiness)?;
    assert_eq!(observed.state, ObservedScheduleState::Paused);
    let judged: Vec<_> = observed
        .recent_runs
        .iter()
        .map(|judged| (judged.run.outcome, judged.verdict))
        .collect();
    assert_eq!(
        judged,
        [
            // No due time: counted as the newest run.
            (RunOutcome::Unknown, RunVerdict::Unknown),
            (RunOutcome::LaunchFailed, RunVerdict::LaunchFailed),
            (RunOutcome::PrecheckIdle, RunVerdict::Idle),
            (RunOutcome::PrecheckFailed, RunVerdict::PrecheckFailed),
            (RunOutcome::PrecheckFailed, RunVerdict::PrecheckFailed),
            // Not yet due for a verdict: the deadline has not passed.
            (RunOutcome::LaunchReported, RunVerdict::Pending),
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
        backend.inspect_schedule(&ours, &readiness)?.state,
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
        WorkerState::UserTakeover,
        "a takeover is neither a failure nor a settlement"
    );
    assert_eq!(
        backend.observe_worker(&target)?,
        WorkerState::UserTakeover,
        "observing it says so, whatever the agent looked like before"
    );
    // A worker that already reported its own outcome stays settled.
    sim.set_worker(
        "ctx_done",
        SimWorker {
            ownership: "user_owned",
            retained_reason: Some("user_takeover"),
            ..SimWorker::new("ready", "succeeded", "exited", false)
        },
    );
    assert_eq!(
        backend.observe_worker(&worker("ctx_done")?)?,
        WorkerState::Settled(WorkerOutcome::Succeeded)
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

/// Run `first` and `second` at the same time and return both results.
fn concurrently<T: Send>(
    first: impl FnOnce() -> T + Send,
    second: impl FnOnce() -> T + Send,
) -> TestResult<(T, T)> {
    std::thread::scope(|scope| {
        let first = scope.spawn(first);
        let second = scope.spawn(second);
        Ok((
            first.join().map_err(|_| "the first caller panicked")?,
            second.join().map_err(|_| "the second caller panicked")?,
        ))
    })
}

#[test]
fn concurrent_first_submissions_create_one_task() -> TestResult {
    let sim = SimOrca::default();
    // Both callers list the Tasks before either creates one.
    sim.interleave(
        &["orchestration", "task-list"],
        2,
        Duration::from_millis(400),
    );
    let launch = request(launch_op("Implement it.")?, "launch-race")?;
    let (first, second) = (connect(&sim)?, connect(&sim)?);
    let (a, b) = concurrently(|| first.execute(&launch), || second.execute(&launch))?;
    assert_eq!(a, b, "both callers get the one launch");
    assert!(a.is_ok(), "{a:?}");
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(sim.state().workers.len(), 1);
    Ok(())
}

#[test]
fn concurrent_installers_create_one_schedule() -> TestResult {
    let sim = SimOrca::default();
    // Both installers list the automations before either creates one.
    sim.interleave(&["automations", "list"], 2, Duration::from_millis(400));
    let spec = schedule_spec("pickup")?;
    let (first, second) = (connect(&sim)?, connect(&sim)?);
    let (a, b) = concurrently(
        || first.install_schedule(&spec),
        || second.install_schedule(&spec),
    )?;
    assert_eq!(a, b, "both installers get the one schedule");
    assert!(a.is_ok(), "{a:?}");
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    assert_eq!(sim.state().automations.len(), 1);
    Ok(())
}

#[test]
fn duplicates_created_outside_the_reservation_are_still_an_explicit_error() -> TestResult {
    // Two installers that do not share a reservation directory stand in for
    // an older process, or a person creating the automation by hand.
    let sim = SimOrca::default();
    // Released as soon as both have listed; the long wait only covers a
    // stalled machine, so both really read before either creates.
    sim.interleave(&["automations", "list"], 2, Duration::from_secs(5));
    let elsewhere = tempfile::tempdir()?;
    let spec = schedule_spec("pickup")?;
    let first = connect(&sim)?;
    let second = OrcaBackend::connect(
        OrcaConfig {
            runtime_dir: elsewhere.path().to_path_buf(),
            ..config(&sim)?
        },
        &sim,
    )?;
    let (a, b) = concurrently(
        || first.install_schedule(&spec),
        || second.install_schedule(&spec),
    )?;
    assert_eq!(sim.state().automations.len(), 2, "both created one");
    let duplicates = Err(OrcaError::DuplicateSchedules { count: 2 });
    assert!(
        a == duplicates || b == duplicates,
        "a read-back that shows both is an error, not an install: {a:?} / {b:?}"
    );
    // Nothing installs or activates on top of the duplicates afterwards.
    assert_eq!(first.install_schedule(&spec), duplicates);
    assert_eq!(
        first.execute(&request(install("pickup")?, "install-after")?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    Ok(())
}

#[test]
fn installing_disabled_never_settles_for_an_active_schedule() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let installed = backend.install_schedule(&schedule_spec("pickup")?)?;
    // A person switched the installed schedule on.
    for automation in &mut sim.state().automations {
        automation.enabled = true;
    }
    let effect = request(install("pickup")?, "install-active")?;
    assert_eq!(
        backend.execute(&effect),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
        "an active schedule is not a disabled install"
    );
    assert_eq!(
        backend.install_schedule(&schedule_spec("pickup")?),
        Err(OrcaError::ScheduleActive)
    );
    assert_eq!(
        backend.resolve(&effect)?,
        Lookup::Unknown,
        "lookup does not report an install that would be refused"
    );
    // Refusing is not activating or pausing: nothing changed.
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    assert_eq!(
        backend.installed_schedules()?.first().map(|s| s.state),
        Some(ObservedScheduleState::Active)
    );
    // Paused again, the same definition installs (reuses) as before.
    backend.set_schedule_state(&installed, ScheduleState::Paused)?;
    assert_eq!(
        backend.install_schedule(&schedule_spec("pickup")?)?,
        installed
    );
    Ok(())
}

#[test]
fn installing_disabled_refuses_a_schedule_that_differs_from_the_request() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    backend.install_schedule(&schedule_spec("pickup")?)?;
    let changed = |edit: &dyn Fn(ScheduleSpec) -> TestResult<ScheduleSpec>| -> TestResult<_> {
        Ok(backend.install_schedule(&edit(schedule_spec("pickup")?)?))
    };
    let differs = |fields: &[ScheduleField]| {
        Err(OrcaError::ScheduleDiffers {
            fields: fields.to_vec(),
        })
    };
    assert_eq!(
        changed(&|spec| Ok(ScheduleSpec::new(
            spec.workflow().clone(),
            spec.consumer().clone(),
            Recurrence::Cron(CronExpr::new("0 9 * * *")?),
            spec.timezone().clone(),
            Text::new("A different prompt.")?,
            AgentFamily::Codex,
        )))?,
        differs(&[
            ScheduleField::Prompt,
            ScheduleField::Agent,
            ScheduleField::Recurrence,
            ScheduleField::Precheck,
        ])
    );
    assert_eq!(
        changed(&|spec| Ok(spec.with_missed_run_grace(GraceMinutes::new(45)?)))?,
        differs(&[ScheduleField::MissedRunGrace])
    );
    assert_eq!(
        changed(&|spec| Ok(spec.with_workspace(ScheduleWorkspace::Existing(worktree("wt-9")?))))?,
        differs(&[ScheduleField::Workspace])
    );
    // A listing that omits the definition cannot confirm it either.
    sim.state()
        .automations
        .push(automation("bare", "kitchen:origin89:gardener", false));
    assert_eq!(
        backend.install_schedule(&schedule_spec("gardener")?),
        differs(&[
            ScheduleField::Prompt,
            ScheduleField::Agent,
            ScheduleField::Recurrence,
            ScheduleField::Timezone,
            ScheduleField::Precheck,
            ScheduleField::Workspace,
            ScheduleField::MissedRunGrace,
        ]),
        "Orca always reports session reuse, so only that field matches"
    );
    assert_eq!(
        backend.execute(&request(install("gardener")?, "install-bare")?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    Ok(())
}

fn worktree(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Worktree,
        backend: orca_id()?,
        handle: ExternalRef::new(handle)?,
    })
}

#[test]
fn presets_are_sent_and_compared_as_cron() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let spec = |recurrence| -> TestResult<ScheduleSpec> {
        Ok(ScheduleSpec::new(
            WorkflowName::new("pickup")?,
            ConsumerId::new("daily")?,
            recurrence,
            Timezone::new("UTC")?,
            Text::new("Run it.")?,
            AgentFamily::Claude,
        ))
    };
    let daily = spec(Recurrence::Daily(TimeOfDay::new(9, 5)?))?;
    let installed = backend.install_schedule(&daily)?;
    let creates = sim.calls_to(&["automations", "create"]);
    let create = creates.first().ok_or("one create")?;
    assert_eq!(flag(create, "trigger"), Some("5 9 * * *"));
    assert!(flag(create, "time").is_none() && flag(create, "day").is_none());
    assert_eq!(backend.install_schedule(&daily)?, installed);
    // The same consumer requested at another time is a different schedule.
    assert_eq!(
        backend.install_schedule(&spec(Recurrence::Weekdays(TimeOfDay::new(9, 5)?))?),
        Err(OrcaError::ScheduleDiffers {
            fields: vec![ScheduleField::Recurrence]
        })
    );
    Ok(())
}

#[test]
fn a_swallowed_scheduled_launch_is_reported_as_failed() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let installed = backend.install_schedule(&schedule_spec("pickup")?)?;
    let deadline = Duration::from_secs(300);
    let due = |seconds| json!({"status": "completed", "scheduledFor": seconds * 1000});
    // Orca recorded three launch steps as completed. Only the second run's
    // agent reported in, inside the deadline; a session that started long
    // after the third run does not vindicate it.
    sim.state().runs = vec![due(10_000), due(20_000), due(30_000)];
    let signals = [
        ReadinessSignal::new(at(20_010)),
        ReadinessSignal::new(at(30_000 + 301)),
    ];
    let inspect = |now, signals: &[ReadinessSignal]| -> TestResult<Vec<RunVerdict>> {
        let readiness = Readiness::new(signals, now, deadline);
        Ok(backend
            .inspect_schedule(&installed, &readiness)?
            .recent_runs
            .iter()
            .map(|judged| judged.verdict)
            .collect())
    };
    assert_eq!(
        inspect(at(30_000 + 3_600), &signals)?,
        [
            RunVerdict::LaunchFailed,
            RunVerdict::Started,
            RunVerdict::LaunchFailed
        ],
        "newest first: the first and last launches never became ready"
    );
    // Before the deadline the newest run is still pending, not failed.
    assert_eq!(
        inspect(at(30_000 + 60), &[])?,
        [
            RunVerdict::Pending,
            RunVerdict::LaunchFailed,
            RunVerdict::LaunchFailed
        ]
    );
    // A signal recorded before the run was due is not evidence for it.
    assert_eq!(
        inspect(at(30_000 + 3_600), &[ReadinessSignal::new(at(29_999))])?
            .first()
            .copied(),
        Some(RunVerdict::LaunchFailed)
    );
    // A run without a due time cannot be joined or aged out.
    sim.state().runs = vec![json!({"status": "completed"})];
    assert_eq!(inspect(at(999_999), &signals)?, [RunVerdict::Pending]);
    Ok(())
}

#[test]
fn runs_without_a_due_time_survive_the_cut_to_the_newest_runs() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let installed = backend.install_schedule(&schedule_spec("pickup")?)?;
    let readiness = Readiness::new(&[], at(0), Duration::from_secs(300));
    let dated = |seconds: u64| json!({"status": "completed", "scheduledFor": seconds * 1000});
    // A trial, or a run still dispatching, has no due time yet.
    let undated = || json!({"status": "running"});
    let observe = |runs: Vec<serde_json::Value>| -> TestResult<Vec<Option<Timestamp>>> {
        sim.state().runs = runs;
        Ok(backend
            .inspect_schedule(&installed, &readiness)?
            .recent_runs
            .iter()
            .map(|judged| judged.run.scheduled_for)
            .collect())
    };
    let newest_first = |seconds: std::ops::RangeInclusive<u64>| -> Vec<Option<Timestamp>> {
        seconds.rev().map(|seconds| Some(at(seconds))).collect()
    };
    let with_undated = |count: usize, dated: Vec<Option<Timestamp>>| -> Vec<Option<Timestamp>> {
        std::iter::repeat_n(None, count).chain(dated).collect()
    };
    assert_eq!(MAX_SCHEDULE_RUNS, 20);

    // Listed last, the undated run is the one a plain truncation would drop.
    let mut listing: Vec<_> = (1..=25).map(dated).collect();
    listing.push(undated());
    assert_eq!(
        observe(listing)?,
        with_undated(1, newest_first(7..=25)),
        "the undated run counts as the newest; the oldest dated runs are cut"
    );

    // Several undated runs take their places before any dated run does.
    let mut listing: Vec<_> = (1..=22).map(dated).collect();
    listing.extend([undated(), undated(), undated()]);
    assert_eq!(
        observe(listing)?,
        with_undated(3, newest_first(6..=22)),
        "three undated runs leave room for the 17 newest dated runs"
    );

    // Exactly the cap: nothing is dropped, and an undated run is not moved out.
    let mut listing: Vec<_> = (1..=19).map(dated).collect();
    listing.insert(0, undated());
    assert_eq!(
        observe(listing)?,
        with_undated(1, newest_first(1..=19)),
        "a listing at the cap keeps every run"
    );

    // More undated runs than the cap still yield only the cap.
    assert_eq!(
        observe((0..=MAX_SCHEDULE_RUNS).map(|_| undated()).collect())?,
        with_undated(MAX_SCHEDULE_RUNS, Vec::new())
    );

    // Without undated runs the newest dated runs are kept, as before.
    assert_eq!(
        observe((1..=21).map(dated).collect())?,
        newest_first(2..=21)
    );
    Ok(())
}

fn branch(value: &str) -> TestResult<BranchName> {
    Ok(BranchName::new(value)?)
}

/// A launch that must land on `requested`.
fn launch_on(requested: &str, workspace: Workspace) -> TestResult<Operation> {
    Ok(Operation::LaunchWorker {
        role: Role::StationCook,
        workspace,
        brief: Text::new("Implement it.")?,
        branch: Some(branch(requested)?),
    })
}

fn stops(sim: &SimOrca) -> usize {
    sim.calls_to(&["orchestration", "worker-stop"]).len()
}

#[test]
fn a_requested_branch_is_passed_as_the_name_that_yields_it() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "on-branch",
    )?;
    let receipt = backend.execute(&launch)?;
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    let [start] = starts.as_slice() else {
        return Err("expected one launch".into());
    };
    // Orca puts its own `lemarier/` in front of the name.
    assert_eq!(flag(start, "name"), Some("issue-6"));
    assert!(receipt.created().iter().any(|resource| {
        resource.kind == ResourceKind::Branch && resource.handle.as_str() == "lemarier/issue-6"
    }));
    assert_eq!(stops(&sim), 0);
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("lemarier/issue-6")?),
        Ok(())
    );
    assert_eq!(backend.resolve(&launch)?, Lookup::Applied(receipt.clone()));
    assert_eq!(
        backend.execute(&launch)?,
        receipt,
        "resubmission verifies again"
    );

    // A host that adds no prefix: a single name is the whole branch.
    sim.state().branch_prefix = "";
    let unprefixed = OrcaBackend::connect(
        OrcaConfig {
            branch_prefix: None,
            ..config(&sim)?
        },
        &sim,
    )?;
    unprefixed.execute(&request(launch_on("hotfix", Workspace::Isolated)?, "bare")?)?;
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    assert_eq!(
        starts.last().and_then(|call| flag(call, "name")),
        Some("hotfix")
    );
    // Configuration that disagrees with what Orca does is caught afterwards.
    sim.state().branch_prefix = "lemarier/";
    assert_eq!(
        unprefixed.execute(&request(
            launch_on("hotfix-2", Workspace::Isolated)?,
            "bare-prefixed"
        )?),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    Ok(())
}

#[test]
fn a_launch_on_another_branch_is_stopped_and_held() -> TestResult {
    let sim = SimOrca::default();
    // Configured `lemarier`, but Orca prefixes `other/`.
    sim.state().branch_prefix = "other/";
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "wrong-branch",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost)),
        "a worker on the wrong branch is not an accepted launch"
    );
    assert_eq!(stops(&sim), 1, "the worker is stopped before it can push");
    let workers: Vec<_> = sim.state().workers.keys().cloned().collect();
    let [dispatch] = workers.as_slice() else {
        return Err("expected one worker".into());
    };
    assert_eq!(
        backend.observe_worker(&worker(dispatch)?)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    // The report: both branches, named.
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("lemarier/issue-6")?),
        Err(OrcaError::BranchMismatch {
            requested: "lemarier/issue-6".to_owned(),
            actual: Some("other/issue-6".to_owned()),
        })
    );
    // Resubmitting starts nothing new, stops nothing again, and is held
    // again; lookup does not call the launch applied.
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(backend.resolve(&launch)?, Lookup::Unknown);
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(stops(&sim), 1);
    // A key that never dispatched a launch cannot be confirmed either.
    assert_eq!(
        backend.verify_launch_branch(&key("never-launched")?, &branch("lemarier/issue-6")?),
        Err(OrcaError::BranchMismatch {
            requested: "lemarier/issue-6".to_owned(),
            actual: None,
        })
    );
    Ok(())
}

#[test]
fn a_stop_that_fails_is_retried_then_reported_as_a_running_mismatch() -> TestResult {
    let sim = SimOrca::default();
    sim.state().branch_prefix = "other/";
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "stop-fails",
    )?;
    // Every stop attempt of the first submission fails.
    for _ in 0..3 {
        sim.fault_on(
            &["orchestration", "worker-stop"],
            Fault::TimeoutBeforeEffect,
        );
    }
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(
        stops(&sim),
        3,
        "the stop is retried, a bounded number of times"
    );
    let dispatch = sim
        .state()
        .workers
        .keys()
        .next()
        .cloned()
        .ok_or("a worker")?;
    assert_eq!(
        backend.observe_worker(&worker(&dispatch)?)?,
        WorkerState::Ready
    );
    // The report says the worker on the wrong branch still runs, unlike a
    // mismatch whose worker was stopped.
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("lemarier/issue-6")?),
        Err(OrcaError::WrongBranchRunning {
            requested: "lemarier/issue-6".to_owned(),
            actual: Some("other/issue-6".to_owned()),
            worker: dispatch.clone(),
        })
    );
    // Lookup never calls it applied, so the store resubmits within its
    // budget, and a resubmission tries the stop again without a new worker.
    assert_eq!(backend.resolve(&launch)?, Lookup::Unknown);
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(stops(&sim), 4);
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(
        backend.observe_worker(&worker(&dispatch)?)?,
        WorkerState::Settled(WorkerOutcome::Cancelled)
    );
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("lemarier/issue-6")?),
        Err(OrcaError::BranchMismatch {
            requested: "lemarier/issue-6".to_owned(),
            actual: Some("other/issue-6".to_owned()),
        })
    );
    Ok(())
}

#[test]
fn a_stop_that_fails_once_is_retried_within_the_launch() -> TestResult {
    let sim = SimOrca::default();
    sim.state().branch_prefix = "other/";
    let backend = connect(&sim)?;
    sim.fault_on(&["orchestration", "worker-stop"], Fault::Garbage);
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "stop-retried",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(stops(&sim), 2);
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("lemarier/issue-6")?),
        Err(OrcaError::BranchMismatch {
            requested: "lemarier/issue-6".to_owned(),
            actual: Some("other/issue-6".to_owned()),
        })
    );
    Ok(())
}

#[test]
fn a_branch_no_name_yields_is_refused_before_anything_is_created() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    // Another prefix, more than one name, and no name after the prefix.
    for (index, requested) in ["kitchen/x", "lemarier/area/topic", "lemarier"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            backend.execute(&request(
                launch_on(requested, Workspace::Isolated)?,
                &format!("unobtainable-{index}")
            )?),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            "{requested}"
        );
    }
    assert!(
        sim.calls_to(&["orchestration"]).is_empty(),
        "no Task, no worktree, not even a listing"
    );
    Ok(())
}

#[test]
fn an_existing_workspace_is_verified_not_renamed() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let existing = launch_on("lemarier/issue-6", Workspace::Existing(worktree("wt-9")?))?;
    sim.state().existing_branch = Some("lemarier/issue-6");
    backend.execute(&request(existing.clone(), "repair-ok")?)?;
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    assert_eq!(starts.last().and_then(|call| flag(call, "name")), None);

    sim.state().existing_branch = Some("lemarier/somebody-else");
    assert_eq!(
        backend.execute(&request(existing.clone(), "repair-wrong")?),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(stops(&sim), 1);

    // Orca reporting no branch at all cannot confirm the request.
    sim.state().existing_branch = None;
    assert_eq!(
        backend.execute(&request(existing, "repair-unknown")?),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(stops(&sim), 2);
    Ok(())
}

#[test]
fn an_unusable_existing_workspace_is_refused_before_a_task_exists() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let unusable = [
        (
            "not-a-worktree",
            ResourceRef {
                kind: ResourceKind::Worker,
                backend: orca_id()?,
                handle: ExternalRef::new("ctx_1")?,
            },
        ),
        (
            "other-backend",
            ResourceRef {
                kind: ResourceKind::Worktree,
                backend: BackendId::new("orca-other")?,
                handle: ExternalRef::new("wt-9")?,
            },
        ),
    ];
    for (label, resource) in unusable {
        let launch = launch_on("lemarier/issue-6", Workspace::Existing(resource))?;
        // A resubmission of the same key is refused the same way.
        for attempt in 0..2 {
            assert_eq!(
                backend.execute(&request(launch.clone(), label)?),
                Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
                "{label} attempt {attempt}"
            );
        }
    }
    assert!(
        sim.calls_to(&["orchestration"]).is_empty(),
        "no Task, no listing, no start"
    );
    assert!(sim.state().tasks.is_empty(), "no orphan Task in the Run");
    assert_eq!(sim.state().effects, 0);
    Ok(())
}

#[test]
fn a_launch_without_a_requested_branch_is_not_constrained() -> TestResult {
    let sim = SimOrca::default();
    // Even on a host whose prefix disagrees with the configured one.
    sim.state().branch_prefix = "other/";
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "free")?)?;
    let starts = sim.calls_to(&["orchestration", "worker-start"]);
    assert_eq!(
        starts.last().and_then(|call| flag(call, "name")),
        Some(format!("kitchen-{}", receipt.reference()).as_str())
    );
    assert_eq!(stops(&sim), 0);
    Ok(())
}

#[test]
fn a_repeated_cancel_is_answered_from_orcas_record() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "to-cancel")?)?;
    let target = launched(&receipt)?;
    let cancel = request(
        Operation::CancelWorker {
            worker: target.clone(),
        },
        "cancel-twice",
    )?;
    let first = backend.execute(&cancel)?;
    // However Orca answers a second stop, a repeat does not send one.
    sim.state().stop_state = "stop_unknown";
    assert_eq!(backend.execute(&cancel)?, first);
    assert_eq!(stops(&sim), 1);
    assert_eq!(backend.resolve(&cancel)?, Lookup::Applied(first));
    Ok(())
}

#[test]
fn cancelling_a_worker_that_already_settled_is_applied() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    for (index, settled) in ["failed", "succeeded", "abandoned"].into_iter().enumerate() {
        let receipt = backend.execute(&request(
            launch_op("Implement it.")?,
            &format!("settled-{index}"),
        )?)?;
        let target = launched(&receipt)?;
        if let Some(worker) = sim.state().workers.get_mut(target.handle.as_str()) {
            worker.worker_state = settled;
            worker.outcome = settled;
            worker.liveness = "exited";
        }
        let cancel = request(
            Operation::CancelWorker {
                worker: target.clone(),
            },
            &format!("cancel-settled-{index}"),
        )?;
        let applied = backend.execute(&cancel)?;
        assert_eq!(
            applied.touched(),
            std::slice::from_ref(&target),
            "{settled}"
        );
        assert_eq!(
            backend.resolve(&cancel)?,
            Lookup::Applied(applied),
            "{settled}"
        );
    }
    assert_eq!(
        stops(&sim),
        0,
        "a settled worker is answered from the record"
    );

    // The worker settles while the stop is sent: Orca answers
    // `alreadySettled` with the state it settled in.
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "racing")?)?;
    let target = launched(&receipt)?;
    sim.state().settle_before_stop = Some("failed");
    let cancel = request(Operation::CancelWorker { worker: target }, "cancel-racing")?;
    let applied = backend.execute(&cancel)?;
    assert_eq!(stops(&sim), 1);
    assert_eq!(backend.resolve(&cancel)?, Lookup::Applied(applied));

    // A stop Orca could not confirm is still not a cancel.
    let receipt = backend.execute(&request(launch_op("Implement it.")?, "unknown")?)?;
    sim.state().stop_state = "stop_unknown";
    let cancel = request(
        Operation::CancelWorker {
            worker: launched(&receipt)?,
        },
        "cancel-unknown",
    )?;
    assert_eq!(
        backend.execute(&cancel),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    assert_eq!(backend.resolve(&cancel)?, Lookup::Unknown);
    Ok(())
}

#[test]
fn an_exited_worker_keeps_its_launch_and_is_never_started_again() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let launch = request(launch_op("Implement it.")?, "exits")?;
    let receipt = backend.execute(&launch)?;
    let dispatch = launched(&receipt)?;
    // The agent's process ends without a stop: Orca returns the Task to
    // `ready`, where a new Dispatch could start it.
    sim.exit_worker(dispatch.handle.as_str())?;
    assert_eq!(
        backend.lookup_launch(launch.key())?,
        Lookup::Applied(receipt.clone())
    );
    assert_eq!(backend.resolve(&launch)?, Lookup::Applied(receipt.clone()));
    assert_eq!(
        backend.execute(&launch)?,
        receipt,
        "resubmission returns it"
    );
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(sim.state().workers.len(), 1);
    assert_eq!(
        backend.observe_worker(&dispatch)?,
        WorkerState::Settled(WorkerOutcome::Failed),
        "the exit is reported as the worker's failure"
    );
    // A requested branch is still verified against the dead worker's record.
    let on_branch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "exits-on-branch",
    )?;
    let receipt = backend.execute(&on_branch)?;
    sim.exit_worker(launched(&receipt)?.handle.as_str())?;
    assert_eq!(backend.execute(&on_branch)?, receipt);
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 2);
    assert_eq!(stops(&sim), 0);
    Ok(())
}

#[test]
fn a_lost_launch_whose_worker_exited_is_reconciled_without_a_second_worker() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = running_task(&fixture, "task-exit", &[Permission::LaunchWorker])?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let clock = ManualClock::starting_at(1);
    let grants = house_grants(&[Permission::LaunchWorker])?;
    let launch = plan(&task, fence, "launch", launch_op("Implement the issue.")?)?;

    sim.fault_on(
        &["orchestration", "worker-start"],
        Fault::TimeoutAfterEffect,
    );
    let record = run_effect(&fixture.store, &backend, &grants, launch.clone(), &clock)?;
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    let dispatch = sim
        .state()
        .workers
        .keys()
        .next()
        .cloned()
        .ok_or("a worker")?;
    sim.exit_worker(&dispatch)?;

    // Running the effect again reconciles first: the lookup finds the
    // original Dispatch although its Task is `ready` again.
    let again = run_effect(&fixture.store, &backend, &grants, launch.clone(), &clock)?;
    let EffectState::Applied { receipt, .. } = again.state() else {
        return Err("the launch was not resolved as applied".into());
    };
    assert_eq!(launched(receipt)?.handle.as_str(), dispatch);
    let once_more = run_effect(&fixture.store, &backend, &grants, launch, &clock)?;
    assert!(matches!(once_more.state(), EffectState::Applied { .. }));
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(sim.state().workers.len(), 1, "no second worker");
    Ok(())
}

#[test]
fn a_held_reservation_reports_uncertain_and_resubmission_finds_the_launch() -> TestResult {
    let sim = SimOrca::default();
    // The first launch holds its reservation while it waits after listing,
    // far longer than the impatient caller is willing to wait.
    sim.interleave(
        &["orchestration", "task-list"],
        2,
        Duration::from_millis(1500),
    );
    let launch = request(launch_op("Implement it.")?, "held")?;
    let holder = connect(&sim)?;
    let impatient = OrcaBackend::connect(
        OrcaConfig {
            reservation_timeout: Duration::from_millis(100),
            ..config(&sim)?
        },
        &sim,
    )?;
    let (held, waited) = concurrently(
        || holder.execute(&launch),
        || {
            // Start once the holder is inside its reservation.
            for _ in 0..500 {
                if !sim.calls_to(&["orchestration", "task-list"]).is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            impatient.execute(&launch)
        },
    )?;
    assert_eq!(
        waited,
        Err(EffectFailure::Uncertain(UncertainReason::Timeout)),
        "nothing was sent, but the holder may be about to apply the launch"
    );
    let receipt = held?;
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    assert_eq!(
        impatient.execute(&launch)?,
        receipt,
        "resubmission finds it"
    );
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    Ok(())
}

#[test]
fn an_unusable_runtime_directory_stops_launches_and_installs_before_orca() -> TestResult {
    let sim = SimOrca::default();
    let occupied = sim.runtime_dir()?.join("occupied");
    std::fs::write(&occupied, b"")?;
    let backend = OrcaBackend::connect(
        OrcaConfig {
            runtime_dir: occupied,
            ..config(&sim)?
        },
        &sim,
    )?;
    assert_eq!(
        backend.execute(&request(launch_op("Implement it.")?, "no-dir")?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert!(matches!(
        backend.install_schedule(&schedule_spec("pickup")?),
        Err(OrcaError::ReservationUnavailable(_))
    ));
    assert_eq!(
        backend.execute(&request(install("pickup")?, "no-dir-install")?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert!(
        sim.calls_to(&["orchestration", "task-create"]).is_empty()
            && sim.calls_to(&["automations"]).is_empty(),
        "an unreserved effect never reaches Orca"
    );
    Ok(())
}

#[test]
fn reservation_files_do_not_outlive_a_settled_effect() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let reservations = || -> TestResult<usize> {
        let directory = sim.runtime_dir()?.join("reservations");
        Ok(std::fs::read_dir(directory)?.count())
    };
    backend.execute(&request(launch_op("Implement it.")?, "settled")?)?;
    backend.install_schedule(&schedule_spec("pickup")?)?;
    assert_eq!(
        reservations()?,
        0,
        "a dispatched launch and an install leave none"
    );
    // A launch that never reached a dispatched Task keeps its key reservable.
    sim.fault_on(
        &["orchestration", "task-create"],
        Fault::TimeoutBeforeEffect,
    );
    assert!(
        backend
            .execute(&request(launch_op("Implement it.")?, "unsettled")?)
            .is_err()
    );
    assert_eq!(reservations()?, 1);
    backend.execute(&request(launch_op("Implement it.")?, "unsettled")?)?;
    assert_eq!(reservations()?, 0);
    Ok(())
}
