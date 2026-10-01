//! Orca adapter behavior against a simulated Orca runtime (`orca_sim`).
//!
//! These are simulated results about argument construction, response
//! mapping, and recovery. Live runtime evidence comes only from `orca_live`.

mod common;
mod orca_sim;

use std::{
    collections::{BTreeSet, VecDeque},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, house, other_house, scheduled, task_id,
    ttl,
};
use kitchen::selection::{
    AgentModel, AgentSelection, EffortLevel, ResolvedSelection, SelectionError, SelectionGap,
    SelectionSource,
};
use kitchen::{
    BackendId, ConsumerId, CredentialId, EffectName,
    adapters::orca::{
        BranchCollision, Invocation, MAX_INVENTORY_PAGES, MAX_REPO_WORKTREES, OrcaBackend,
        OrcaConfig, OrcaError, OrcaRunner, RawOutput, RetainedReason, TerminalAccounting,
        launch_marker, verify_branch,
    },
    contracts::{
        AttemptNumber, BackendUnavailable, BranchName, Capability, CapabilityRequirements,
        CoordinatorMailbox, Effect, EffectExecutor, EffectFailure, EffectRequest, EvidenceRevision,
        ExternalRef, Grant, HouseGrants, IdempotencyKey, Liveness, Lookup, MailboxError,
        MessageKind, NotAppliedReason, Operation, Permission, PinnedCheckout, Provenance, Receipt,
        Repository, ResourceKind, ResourceRef, RetryPolicy, Role, ScheduleBackend, ScheduleEffect,
        TaskAuthority, TaskSpec, Text, Timestamp, UncertainReason, WorkerBackend, WorkerOutcome,
        WorkerState, Workspace, WorktreeStatus,
        conformance::{self, Check, CheckResult, ConformanceFixture},
    },
    scheduling::{
        AgentFamily, CronExpr, GraceMinutes, MAX_SCHEDULE_RUNS, ObservedScheduleState, Precheck,
        PrecheckTimeout, Readiness, ReadinessSignal, Recurrence, RunOutcome, RunVerdict,
        ScheduleField, ScheduleSpec, ScheduleState, ScheduleWorkspace, TimeOfDay, Timezone,
        WorkflowName,
    },
    state::{AttemptState, EffectPlan, EffectState, reconcile, run_effect},
    workflows::coordination::{
        Context, Standing, Supervision, SupervisionInput, SupervisionPolicy, supervise,
    },
};
use orca_sim::{Fault, SimAutomation, SimOrca, SimTask, SimWorker};
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
        pinned: None,
        agent: None,
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
        Check::SelectionRefused,
        Check::LaunchObservable,
        Check::InventoryListsLaunch,
        Check::MessageRecovery,
        Check::CancelObserved,
        Check::ReleaseKeepsBranch,
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
    let Lookup::Ended(receipt) = probe else {
        return Err("the probe launch was not found by its key".into());
    };
    verify_branch(&receipt, branch.as_str())?;
    Ok(())
}

#[test]
fn a_supplied_branch_label_uses_the_host_prefix() -> TestResult {
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
        let result = conformance::run_worker_on_branch(
            &backend,
            &conformance_fixture()?,
            &BranchName::new(supplied)?,
        );
        assert!(result.is_ok(), "{host} {supplied}: {result:?}");
        assert!(
            !sim.calls_to(&["orchestration", "task-create"]).is_empty(),
            "{host} {supplied}"
        );
        assert!(
            !sim.calls_to(&["orchestration", "worker-start"]).is_empty(),
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

    // Even a textual match with the caller's logical label cannot override
    // the host prefix that this backend was configured to verify.
    let sim = SimOrca::default();
    sim.state().branch_prefix = "kitchen/";
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("kitchen/issue-237", Workspace::Isolated)?,
        "stale-prefix",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchPrefixMismatchStopped
        ))
    );
    assert!(matches!(backend.resolve(&launch)?, Lookup::Ended(_)));
    assert_eq!(stops(&sim), 1);
    Ok(())
}

#[test]
fn the_shared_suite_accepts_the_host_prefix() -> TestResult {
    // Orca uses the final requested component under its configured prefix.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    assert_shared_suite_passed(&conformance::run_worker(&backend, &conformance_fixture()?)?);
    assert!(!sim.calls_to(&["orchestration", "worker-start"]).is_empty());
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
    let spec = format!("{brief}\n\nkitchen-requested-branch: ");
    assert_eq!(
        flag(create, "spec"),
        Some(spec.as_str()),
        "brief is one argv entry, followed by the empty branch record"
    );
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
fn identity_launch_configures_the_worktree_before_worker_start() -> TestResult {
    use std::{path::Path, process::Command};
    let temp = tempfile::tempdir()?;
    let main = temp.path().join("main");
    let worker_path = temp.path().join("worker");
    std::fs::create_dir(&main)?;
    let git = |path: &Path, args: &[&str]| -> TestResult<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    git(&main, &["init", "-q", "-b", "main"])?;
    git(&main, &["config", "user.name", "Person"])?;
    git(&main, &["config", "user.email", "person@example.com"])?;
    git(
        &main,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    )?;
    let config_before = std::fs::read(main.join(".git/config"))?;
    let sim = SimOrca::default();
    {
        let mut state = sim.state();
        state.identity_repo = Some(main.clone());
        state.identity_path = Some(worker_path.clone());
        state.branch_prefix = "lemarier/";
    }
    let backend = connect(&sim)?.with_writer_identity(
        "house[bot]".into(),
        "123+house[bot]@users.noreply.github.com".into(),
    );
    let effect = Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new("task")?,
        branch: Some(BranchName::new("lemarier/issue-1")?),
        pinned: None,
        agent: None,
    };
    let launch_request = request(effect, "identity-launch")?;
    assert!(matches!(
        backend.execute(&launch_request),
        Err(EffectFailure::NotApplied(
            NotAppliedReason::WorktreeConfigDisabled
        ))
    ));
    assert_eq!(std::fs::read(main.join(".git/config"))?, config_before);
    assert!(sim.calls_to(&["worktree", "create"]).is_empty());
    git(
        &main,
        &["config", "--local", "extensions.worktreeConfig", "true"],
    )?;
    let config_before = std::fs::read(main.join(".git/config"))?;
    let receipt = backend.execute(&launch_request)?;
    assert_eq!(std::fs::read(main.join(".git/config"))?, config_before);
    assert!(sim.state().identity_seen_at_start);
    assert_eq!(git(&main, &["config", "user.name"])?, "Person");
    assert_eq!(git(&worker_path, &["config", "user.name"])?, "house[bot]");
    let recorded =
        kitchen::adapters::orca::read_writer_base(&sim.runtime_dir()?, launch_request.key())?;
    assert_eq!(
        recorded.base,
        kitchen::contracts::CommitId::new(&git(&main, &["rev-parse", "HEAD"])?)?
    );
    assert!(recorded.created);
    assert!(
        receipt
            .created()
            .iter()
            .any(|item| item.kind == ResourceKind::Worktree)
    );
    assert!(
        !receipt
            .touched()
            .iter()
            .any(|item| item.kind == ResourceKind::Branch)
    );
    let repeated = backend.execute(&launch_request)?;
    assert_eq!(repeated, receipt);
    assert_eq!(sim.calls_to(&["worktree", "create"]).len(), 1);
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    let prior_worker = launched(&receipt)?;
    backend.execute(&request(
        Operation::CancelWorker {
            worker: prior_worker,
        },
        "stop-prior",
    )?)?;
    let worktree = receipt
        .created()
        .iter()
        .find(|item| item.kind == ResourceKind::Worktree)
        .cloned()
        .ok_or("no worktree")?;
    let resumed = request(
        Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Existing(worktree),
            brief: Text::new("follow-up")?,
            branch: Some(BranchName::new("lemarier/issue-1")?),
            pinned: None,
            agent: None,
        },
        "identity-follow-up",
    )?;
    let resumed_receipt = backend.execute(&resumed)?;
    assert!(
        !resumed_receipt
            .created()
            .iter()
            .any(|item| item.kind == ResourceKind::Branch)
    );
    assert!(
        !resumed_receipt
            .created()
            .iter()
            .any(|item| item.kind == ResourceKind::Worktree)
    );
    assert!(
        resumed_receipt
            .touched()
            .iter()
            .any(|item| item.kind == ResourceKind::Branch)
    );
    assert!(
        resumed_receipt
            .touched()
            .iter()
            .any(|item| item.kind == ResourceKind::Worktree)
    );
    let resumed_base =
        kitchen::adapters::orca::read_writer_base(&sim.runtime_dir()?, resumed.key())?;
    assert!(!resumed_base.created);
    assert_eq!(sim.calls_to(&["worktree", "create"]).len(), 1);
    Ok(())
}

fn failed_identity_launch(removal_refused: bool, base_write_failure: bool) -> TestResult {
    use std::process::Command;
    let temp = tempfile::tempdir()?;
    let main = temp.path().join("main");
    let worker = temp.path().join("worker");
    std::fs::create_dir(&main)?;
    let git = |args: &[&str]| -> TestResult<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    git(&["init", "-q", "-b", "main"])?;
    git(&[
        "-c",
        "user.name=Person",
        "-c",
        "user.email=person@example.com",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "base",
    ])?;
    git(&["config", "--local", "extensions.worktreeConfig", "true"])?;
    let sim = SimOrca::default();
    {
        let mut state = sim.state();
        state.identity_repo = Some(main.clone());
        state.identity_path = Some(worker.clone());
        state.branch_prefix = "lemarier/";
    }
    if removal_refused {
        sim.fault_on(&["worktree", "rm"], Fault::Refuse("busy"));
    }
    if base_write_failure {
        std::fs::write(sim.runtime_dir()?.join("writer-bases"), "blocked")?;
    }
    let backend = connect(&sim)?.with_writer_identity(
        if base_write_failure {
            "house[bot]"
        } else {
            "invalid\nname"
        }
        .into(),
        "123+house[bot]@users.noreply.github.com".into(),
    );
    let request = request(
        Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: Text::new("task")?,
            branch: Some(BranchName::new("lemarier/issue-1")?),
            pinned: None,
            agent: None,
        },
        "identity-setup-failure",
    )?;
    let result = backend.execute(&request);
    if removal_refused {
        let Err(EffectFailure::Ended(receipt)) = result else {
            return Err("failed removal did not return an owned receipt".into());
        };
        assert!(
            receipt
                .created()
                .iter()
                .any(|item| item.kind == ResourceKind::Worktree)
        );
    } else {
        assert!(matches!(result, Err(EffectFailure::Uncertain(_))));
    }
    assert_eq!(sim.calls_to(&["worktree", "create"]).len(), 1);
    assert_eq!(sim.calls_to(&["worktree", "rm"]).len(), 1);
    assert!(sim.calls_to(&["orchestration", "worker-start"]).is_empty());
    if removal_refused {
        assert!(worker.exists());
        let Lookup::Ended(receipt) = backend.lookup(&request)? else {
            return Err("undispatched worktree did not retain ownership".into());
        };
        assert!(
            receipt
                .created()
                .iter()
                .any(|item| item.kind == ResourceKind::Worktree)
        );
        assert!(
            receipt
                .created()
                .iter()
                .any(|item| item.kind == ResourceKind::Branch)
        );
    } else {
        assert!(!worker.exists());
        assert_eq!(
            git(&["worktree", "list", "--porcelain"])?
                .matches("worktree ")
                .count(),
            1
        );
        assert_eq!(git(&["branch", "--list", "lemarier/issue-1"])?, "");
    }
    Ok(())
}

#[test]
fn identity_setup_failure_removes_an_unused_worktree() -> TestResult {
    failed_identity_launch(false, false)
}

#[test]
fn identity_setup_failed_removal_retains_an_owned_receipt() -> TestResult {
    failed_identity_launch(true, false)
}

#[test]
fn writer_base_failure_removes_an_unused_worktree() -> TestResult {
    failed_identity_launch(false, true)
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
fn lookups_of_stops_and_releases_need_this_run() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    // Stopped and released: what a lookup accepts as an applied cancel and
    // release, but under another Run.
    let mut elsewhere = SimWorker::new("stopped", "failed", "exited", false);
    elsewhere.release_state = "released";
    elsewhere.run = "run_someone_else";
    sim.set_worker("ctx_elsewhere", elsewhere.clone());
    let target = worker("ctx_elsewhere")?;
    let cancel = request(
        Operation::CancelWorker {
            worker: target.clone(),
        },
        "cancel-elsewhere",
    )?;
    let release = request(
        Operation::ReleaseResource {
            resource: target.clone(),
        },
        "release-elsewhere",
    )?;
    assert_eq!(backend.resolve(&cancel)?, Lookup::Unknown);
    assert_eq!(backend.resolve(&release)?, Lookup::Unknown);
    assert_eq!(backend.lookup(&cancel)?, Lookup::Unknown);

    // The same record in this Run is applied.
    elsewhere.run = "run_sim";
    sim.set_worker("ctx_elsewhere", elsewhere);
    let applied = Receipt::new(target.handle.clone(), Vec::new(), vec![target.clone()])?;
    assert_eq!(backend.resolve(&cancel)?, Lookup::Applied(applied.clone()));
    assert_eq!(backend.resolve(&release)?, Lookup::Applied(applied));

    // A Dispatch Orca no longer knows stays unknown, and a failed read is
    // an error, not an answer.
    sim.state().workers.clear();
    assert_eq!(backend.resolve(&cancel)?, Lookup::Unknown);
    sim.fault_on(
        &["orchestration", "worker-show"],
        Fault::TimeoutBeforeEffect,
    );
    assert_eq!(backend.resolve(&release), Err(BackendUnavailable::Timeout));
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
        agent: None,
        work_type: None,
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
        basis: None,
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
    sim.state().mail = VecDeque::from([json!({
        "deliveryId": "delivery_1",
        "messages": [
            {"id": "msg_q", "type": "question", "subject": "Which parser?", "body": "A or B?", "payload": {"dispatchId": "ctx_1"}},
            {"id": "msg_d", "type": "worker_done", "subject": "done", "body": "All tests pass.", "payload": {"taskId": "task_1", "dispatchId": "ctx_1", "outcome": "succeeded"}},
            {"id": "msg_h", "type": "heartbeat", "subject": "alive", "payload": {"dispatchId": "ctx_1", "outcome": "succeeded"}},
            {"id": "has space", "type": "status", "subject": "x"},
        ],
    })]);
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
fn mailbox_routes_a_question_by_its_assigned_terminal() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.state().tasks.push(SimTask {
        id: "task_1".into(),
        title: "kitchen:launch".into(),
        spec: String::new(),
        status: "dispatched",
        dispatch: Some("ctx_1".into()),
    });
    sim.state().mail = VecDeque::from([json!({
        "deliveryId": "delivery_question",
        "messages": [{
            "id": "msg_question", "type": "question",
            "from_handle": "term_ctx_1", "subject": "Need a choice?"
        }, {
            "id": "msg_report", "type": "worker_done",
            "from_handle": "term_ctx_1",
            "payload": "{\"taskId\":\"task_1\",\"dispatchId\":\"ctx_1\",\"outcome\":\"failed\"}"
        }, {
            "id": "msg_other", "type": "question",
            "from_handle": "term_unknown"
        }, {
            "id": "msg_mismatch", "type": "question",
            "from_handle": "term_ctx_1",
            "payload": "{\"dispatchId\":\"ctx_other\"}"
        }]
    })]);
    let delivery = backend.next_delivery()?.ok_or("delivery")?;
    assert_eq!(delivery.messages[0].worker, Some(worker("ctx_1")?));
    assert_eq!(delivery.messages[1].worker, Some(worker("ctx_1")?));
    assert_eq!(delivery.messages[1].outcome, Some(WorkerOutcome::Failed));
    assert_eq!(delivery.messages[2].worker, None);
    assert_eq!(delivery.messages[3].worker, None);
    Ok(())
}

#[test]
fn mailbox_keeps_payload_dispatches_when_assignment_listing_fails() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.state().mail = VecDeque::from([json!({
        "deliveryId": "delivery_listing_failure",
        "messages": [
            {"id": "msg_done", "type": "worker_done", "from_handle": "term_ctx_1",
             "payload": {"dispatchId": "ctx_1", "outcome": "succeeded"}},
            {"id": "msg_question", "type": "question", "from_handle": "term_ctx_1"}
        ]
    })]);
    sim.fault_on(&["orchestration", "task-list"], Fault::TimeoutBeforeEffect);
    let delivery = backend.next_delivery()?.ok_or("delivery")?;
    assert_eq!(delivery.unreadable, 0);
    assert_eq!(delivery.messages.len(), 2);
    assert_eq!(delivery.messages[0].worker, Some(worker("ctx_1")?));
    assert_eq!(delivery.messages[0].outcome, Some(WorkerOutcome::Succeeded));
    assert_eq!(delivery.messages[1].worker, None);
    Ok(())
}

#[test]
fn adoption_moves_the_mailbox_to_the_new_coordinator() -> TestResult {
    let sim = SimOrca::default();
    let old = connect(&sim)?;
    old.adopt_run()?;
    assert_eq!(old.next_delivery()?, None);
    sim.state().mail = VecDeque::from([json!({"deliveryId": "delivery_1", "messages": [
        {"id": "msg_q", "type": "question", "payload": {"dispatchId": "ctx_1"}},
    ]})]);
    let held = old
        .next_delivery()?
        .ok_or("the first coordinator got nothing")?;
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
    // Orca redelivers the unacknowledged message under a new batch id.
    let adopted = adopting.next_delivery()?.ok_or("the adopter got nothing")?;
    assert_ne!(adopted.id, held.id);
    assert_eq!(adopted.messages, held.messages);
    assert!(
        old.next_delivery() == Err(MailboxError::Fenced),
        "the previous coordinator no longer reads the mailbox"
    );
    // Orca refuses the old id as fenced; the adopter still holds the Run, so
    // the acknowledgement consumed nothing and is not a fence.
    assert_eq!(adopting.acknowledge(&held.id)?, Some(adopted.clone()));
    assert_eq!(adopting.acknowledge(&adopted.id)?, None);
    assert!(sim.state().mail.is_empty());
    Ok(())
}

/// The mailbox contract on the simulated runtime: two unacknowledged batches,
/// a second coordinator terminal that adopts the Run, and the conformance
/// checks. Simulated evidence only; `orca_live.rs` covers the live runtime.
#[test]
fn mailbox_conforms_on_the_simulated_runtime() -> TestResult {
    let sim = SimOrca::default();
    sim.state().mail = VecDeque::from([
        json!({"deliveryId": "delivery_1", "messages": [
            {"id": "msg_q", "type": "question", "subject": "Which parser?", "payload": {"dispatchId": "ctx_1"}},
        ]}),
        json!({"deliveryId": "delivery_2", "messages": [
            {"id": "msg_d", "type": "worker_done", "payload": {"dispatchId": "ctx_1", "outcome": "succeeded"}},
            {"id": "msg_e", "type": "escalation", "payload": {"dispatchId": "ctx_2"}},
        ]}),
    ]);
    let coordinator = connect(&sim)?;
    let restarted = OrcaBackend::connect(
        OrcaConfig {
            coordinator: ExternalRef::new("term_restarted")?,
            ..config(&sim)?
        },
        &sim,
    )?;
    let sent = ["msg_q", "msg_d", "msg_e"]
        .into_iter()
        .map(ExternalRef::new)
        .collect::<Result<Vec<_>, _>>()?;
    let report = conformance::run_mailbox(&coordinator, &restarted, &sent)?;
    for check in [
        Check::DeliveryReplayed,
        Check::AdoptionReplays,
        Check::DuplicateAcknowledgement,
        Check::DeliveryOrder,
    ] {
        assert_eq!(report.result(check), Some(CheckResult::Passed), "{check}");
    }
    assert!(sim.state().mail.is_empty(), "every batch was acknowledged");
    Ok(())
}

#[test]
fn mailbox_failures_keep_their_meaning() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.fault_on(&["orchestration", "check"], Fault::TimeoutBeforeEffect);
    assert_eq!(
        backend.next_delivery(),
        Err(MailboxError::Unavailable(BackendUnavailable::Timeout))
    );
    sim.fault_on(&["orchestration", "check"], Fault::Refuse("run_not_found"));
    assert_eq!(
        backend.acknowledge(&ExternalRef::new("delivery_1")?),
        Err(MailboxError::Unavailable(BackendUnavailable::Transport)),
        "only consumer_fenced means another coordinator took the Run"
    );
    sim.fault_on(&["orchestration", "run-use"], Fault::Garbage);
    assert_eq!(
        backend.adopt_run(),
        Err(MailboxError::Unavailable(BackendUnavailable::Transport))
    );
    sim.fault_on(
        &["orchestration", "check"],
        Fault::Refuse("consumer_fenced"),
    );
    assert_eq!(
        backend.await_delivery(Duration::from_secs(1)),
        Err(MailboxError::Fenced)
    );
    Ok(())
}

/// A fenced acknowledgement is checked with a plain read: only a read that
/// is fenced too means the Run was lost, and a read that fails keeps its
/// failure rather than passing for either.
#[test]
fn a_fenced_acknowledgement_is_confirmed_by_a_read() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let gone = ExternalRef::new("delivery_old")?;
    sim.fault_on(
        &["orchestration", "check"],
        Fault::Refuse("consumer_fenced"),
    );
    assert_eq!(backend.acknowledge(&gone), Ok(None), "the read succeeded");
    for _ in 0..2 {
        sim.fault_on(
            &["orchestration", "check"],
            Fault::Refuse("consumer_fenced"),
        );
    }
    assert_eq!(backend.acknowledge(&gone), Err(MailboxError::Fenced));
    sim.fault_on(
        &["orchestration", "check"],
        Fault::Refuse("consumer_fenced"),
    );
    sim.fault_on(&["orchestration", "check"], Fault::TimeoutBeforeEffect);
    assert_eq!(
        backend.acknowledge(&gone),
        Err(MailboxError::Unavailable(BackendUnavailable::Timeout))
    );
    let checks = sim.calls_to(&["orchestration", "check"]);
    assert_eq!(
        checks.len(),
        6,
        "one read after each fenced acknowledgement"
    );
    Ok(())
}

fn schedule_spec(consumer: &str) -> TestResult<ScheduleSpec> {
    schedule_spec_for(
        consumer,
        ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Claude)),
    )
}

fn schedule_spec_for(consumer: &str, agent: ResolvedSelection) -> TestResult<ScheduleSpec> {
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
        agent,
    )
    .with_precheck(precheck))
}

fn install(consumer: &str) -> TestResult<ScheduleEffect> {
    Ok(ScheduleEffect::InstallDisabled {
        schedule: schedule_spec(consumer)?.into(),
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
    assert_eq!(
        flag(create, "name"),
        Some("kitchen:origin89:pickup:workflow=pickup")
    );
    assert_eq!(
        flag(create, "precheck"),
        Some(r"'kitchen' 'precheck' 'it'\''s pickup'")
    );
    assert_eq!(flag(create, "precheck-timeout"), Some("60"));
    assert_eq!(flag(create, "trigger"), Some("17,37,57 * * * *"));
    assert_eq!(flag(create, "timezone"), Some("America/Toronto"));

    assert!(receipt.touched().is_empty());

    // Reinstalling for the same consumer reuses it, and says so: the second
    // task touched the schedule and did not create it.
    let reused = backend.execute(&request(install("pickup")?, "install-2")?)?;
    assert_eq!(reused.reference(), receipt.reference());
    assert!(reused.created().is_empty(), "a reuse is not a creation");
    assert_eq!(reused.touched(), std::slice::from_ref(&installed));
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    // A lookup cannot tell which key created the schedule it finds, so it
    // never claims the creation either.
    assert_eq!(backend.resolve(&effect)?, Lookup::Applied(reused));
    let listed = backend.installed_schedules()?;
    assert_eq!(listed.len(), 1, "the live automation is not Kitchen's");
    assert_eq!(
        listed.first().map(|s| s.state),
        Some(ObservedScheduleState::Paused)
    );
    Ok(())
}

#[test]
fn a_schedule_naming_a_model_or_effort_is_refused_before_orca_is_called() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let resolved = |model: Option<&str>, effort: Option<&str>| -> TestResult<ResolvedSelection> {
        Ok(ResolvedSelection {
            selection: AgentSelection {
                agent: AgentFamily::Claude,
                model: model.map(AgentModel::new).transpose()?,
                effort: effort.map(EffortLevel::new).transpose()?,
            },
            source: SelectionSource::HouseRule,
        })
    };
    for (agent, gaps) in [
        (resolved(Some("sonnet"), None)?, vec![SelectionGap::Model]),
        (resolved(None, Some("low"))?, vec![SelectionGap::Effort]),
        (
            resolved(Some("sonnet"), Some("low"))?,
            vec![SelectionGap::Model, SelectionGap::Effort],
        ),
    ] {
        let spec = schedule_spec_for("pickup", agent)?;
        assert_eq!(
            backend.install_schedule(&spec),
            Err(OrcaError::Selection(SelectionError::Unsupported {
                agent: AgentFamily::Claude,
                gaps,
            }))
        );
        let effect = request(
            ScheduleEffect::InstallDisabled {
                schedule: spec.into(),
            },
            "install-model",
        )?;
        assert_eq!(
            backend.execute(&effect),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
    }
    assert!(
        sim.calls_to(&["automations"]).is_empty(),
        "nothing is listed, reserved, or created for a refused selection"
    );

    // The same policy source with only a family installs.
    let family = resolved(None, None)?;
    let installed = backend.install_schedule(&schedule_spec_for("pickup", family)?)?;
    let creates = sim.calls_to(&["automations", "create"]);
    let create = creates.first().ok_or("one create")?;
    assert_eq!(flag(create, "provider"), Some("claude"));
    assert_eq!(flag(create, "model"), None);
    assert_eq!(flag(create, "effort"), None);
    sim.state().runs = vec![json!({"status": "completed", "scheduledFor": 1000})];
    let readiness = Readiness::new(&[], at(0), Duration::from_secs(300));
    let observed = backend.inspect_schedule(&installed, &readiness)?;
    assert_eq!(
        observed
            .recent_runs
            .iter()
            .map(|judged| judged.run.agent.clone())
            .collect::<Vec<_>>(),
        [None],
        "Orca records no provider per run, so none is claimed"
    );
    Ok(())
}

#[test]
fn a_run_is_not_attributed_to_the_automations_current_provider() -> TestResult {
    // Orca run records carry no provider. An automation edited to another
    // provider after its runs must not relabel them, so every provider,
    // known or not, leaves the run's agent unrecorded.
    for provider in ["claude", "codex", "gemini"] {
        let sim = SimOrca::default();
        sim.state().automations.push(SimAutomation {
            flags: std::collections::BTreeMap::from([("provider".to_owned(), provider.to_owned())]),
            ..automation("auto-9", "kitchen:origin89:pickup", false)
        });
        sim.state().runs = vec![
            json!({"status": "completed", "scheduledFor": 1000}),
            json!({"status": "completed", "scheduledFor": 2000}),
        ];
        let backend = connect(&sim)?;
        let readiness = Readiness::new(&[], at(0), Duration::from_secs(300));
        let observed = backend.inspect_schedule(&schedule("auto-9")?, &readiness)?;
        let agents: Vec<_> = observed
            .recent_runs
            .iter()
            .map(|judged| judged.run.agent.clone())
            .collect();
        assert_eq!(agents, [None, None], "provider {provider}");
    }
    Ok(())
}

#[test]
fn a_lookup_never_finds_a_selection_orca_cannot_launch_applied() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    // A paused family-only schedule with the same definition exists, as
    // when an install's outcome was not recorded.
    backend.install_schedule(&schedule_spec_for(
        "pickup",
        ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Claude)),
    )?)?;
    let named = |model: Option<&str>, effort: Option<&str>| -> TestResult<ScheduleEffect> {
        Ok(ScheduleEffect::InstallDisabled {
            schedule: schedule_spec_for(
                "pickup",
                ResolvedSelection {
                    selection: AgentSelection {
                        agent: AgentFamily::Claude,
                        model: model.map(AgentModel::new).transpose()?,
                        effort: effort.map(EffortLevel::new).transpose()?,
                    },
                    source: SelectionSource::HouseRule,
                },
            )?
            .into(),
        })
    };
    for (model, effort) in [
        (Some("sonnet"), None),
        (None, Some("low")),
        (Some("sonnet"), Some("low")),
    ] {
        assert_eq!(
            backend.resolve(&request(named(model, effort)?, "lookup")?)?,
            Lookup::Unknown,
            "model {model:?} effort {effort:?}"
        );
    }
    // The family-only definition of the same schedule still resolves.
    assert!(matches!(
        backend.resolve(&request(named(None, None)?, "lookup")?)?,
        Lookup::Applied(_)
    ));
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
    // Through `execute`, that schedule is this install's creation: under the
    // reservation the listing before the create showed none.
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    sim.fault_on(&["automations", "create"], Fault::TimeoutAfterEffect);
    let receipt = backend.execute(&request(install("pickup")?, "install-late")?)?;
    assert_eq!(receipt.created(), [schedule("auto-1")?]);
    assert!(receipt.touched().is_empty());

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
                requires: None,
            },
            ScheduleEffect::Remove {
                schedule: schedule(foreign)?,
            },
            ScheduleEffect::Trial {
                schedule: schedule(foreign)?,
                requires: None,
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

    // Kitchen defines no scheduled `pickup` workflow, so Orca cannot
    // establish its requirements and neither tries nor activates it.
    let ours = backend.install_schedule(&schedule_spec("pickup")?)?;
    let activate = request(
        ScheduleEffect::SetState {
            schedule: ours.clone(),
            state: ScheduleState::Active,
            requires: None,
        },
        "activate",
    )?;
    for effect in [
        request(
            ScheduleEffect::Trial {
                schedule: ours.clone(),
                requires: None,
            },
            "trial",
        )?,
        activate.clone(),
    ] {
        assert_eq!(
            backend.execute(&effect),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
    }
    assert!(sim.calls_to(&["automations", "run"]).is_empty());
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    // Enabled in Orca by someone else: the lookup reports what Orca shows,
    // and a trial needs it paused.
    sim.state()
        .automations
        .iter_mut()
        .filter(|automation| automation.id == ours.handle.as_str())
        .for_each(|automation| automation.enabled = true);
    assert!(matches!(backend.resolve(&activate)?, Lookup::Applied(_)));
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
                requires: None,
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
    let launch = request(launch_op("Implement it.")?, "failed-start")?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded))
    );
    let Lookup::Ended(receipt) = backend.resolve(&launch)? else {
        return Err("failed dispatch was not ended".into());
    };
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
    // Waiting on a reply is positive liveness only while the process is live:
    // an unverifiable or exited waiting worker could not answer or push.
    for liveness in ["unverifiable", "exited"] {
        sim.set_worker(
            "ctx_waiting",
            SimWorker::new("ready", "in_progress", liveness, true),
        );
        assert_eq!(
            backend.observe_worker(&worker("ctx_waiting")?)?,
            WorkerState::Starting,
            "{liveness} waiting worker"
        );
    }
    sim.set_worker(
        "ctx_waiting",
        SimWorker::new("ready", "in_progress", "live", true),
    );
    assert_eq!(
        backend.observe_worker(&worker("ctx_waiting")?)?,
        WorkerState::AwaitingReply
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
    sim.state().mail = VecDeque::from([json!({
        "deliveryId": "delivery_h",
        "messages": [
            {"id": "msg_h1", "type": "heartbeat", "subject": "alive", "payload": {"dispatchId": "ctx_1"}},
            {"id": "msg_s", "type": "status", "subject": "note"},
        ],
    })]);
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
    let state = sim.state();
    let wait_index = state
        .calls
        .iter()
        .rposition(|call| call == wait)
        .ok_or("wait call recorded")?;
    assert_eq!(
        state.deadlines.get(wait_index).copied(),
        Some(Duration::from_secs(65)),
        "the subprocess outlives Orca's wait"
    );
    drop(state);
    backend.await_delivery(Duration::from_secs(86_400))?;
    let calls = sim.calls_to(&["orchestration", "check"]);
    assert_eq!(
        calls.last().and_then(|call| flag(call, "timeout-ms")),
        Some("900000"),
        "waits are bounded"
    );

    sim.state().mail = VecDeque::from([json!({
        "deliveryId": "delivery_d",
        "messages": [
            {"id": "msg_h2", "type": "heartbeat", "subject": "alive"},
            {"id": "msg_d", "type": "worker_done", "subject": "done", "payload": {"dispatchId": "ctx_1", "outcome": "failed"}},
        ],
    })]);
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
            ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Codex)),
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
            ScheduleField::Workflow,
        ]),
        "Orca always reports session reuse, so only that field matches; the \
         name records no workflow"
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
fn preserved_worktree_inspection_checks_the_real_checkout() -> TestResult {
    use std::process::Command;
    let temp = tempfile::tempdir()?;
    let main = temp.path().join("main");
    let checkout = temp.path().join("preserved");
    std::fs::create_dir(&main)?;
    let git = |args: &[&str]| -> TestResult<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    git(&["init", "-q", "-b", "main"])?;
    git(&[
        "-c",
        "user.name=Person",
        "-c",
        "user.email=person@example.com",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "base",
    ])?;
    git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "lemarier/issue-6",
        checkout.to_str().ok_or("path")?,
    ])?;
    let head = kitchen::contracts::CommitId::new(&git(&["rev-parse", "HEAD"])?)?;
    let sim = SimOrca::default();
    sim.state().identity_worktree = Some((
        "wt_preserved".to_owned(),
        "refs/heads/lemarier/issue-6".to_owned(),
        "kitchen-marker".to_owned(),
        checkout.clone(),
    ));
    let backend = connect(&sim)?;
    let resource = worktree("wt_preserved")?;
    let branch = BranchName::new("lemarier/issue-6")?;
    let report = Text::new("report.md")?;
    std::fs::write(checkout.join("report.md"), "evidence")?;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &head, &report)?,
        WorktreeStatus::Ready
    );
    let launch = |key_text: &str| -> TestResult<EffectRequest> {
        let mut operation = launch_on("lemarier/issue-6", Workspace::Existing(resource.clone()))?;
        if let Operation::LaunchWorker { pinned, .. } = &mut operation {
            *pinned = Some(PinnedCheckout {
                head: head.clone(),
                report_path: report.clone(),
            });
        }
        request(operation, key_text)
    };
    git(&[
        "-C",
        checkout.to_str().ok_or("path")?,
        "switch",
        "-q",
        "--detach",
        "HEAD",
    ])?;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &head, &report)?,
        WorktreeStatus::WrongBranch
    );
    git(&[
        "-C",
        checkout.to_str().ok_or("path")?,
        "switch",
        "-q",
        "lemarier/issue-6",
    ])?;
    git(&[
        "-C",
        checkout.to_str().ok_or("path")?,
        "switch",
        "-q",
        "-c",
        "other",
    ])?;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &head, &report)?,
        WorktreeStatus::WrongBranch
    );
    assert_eq!(
        backend.execute(&launch("moved-branch")?),
        Err(EffectFailure::NotApplied(
            NotAppliedReason::WorktreeChanged(WorktreeStatus::WrongBranch)
        ))
    );
    git(&[
        "-C",
        checkout.to_str().ok_or("path")?,
        "switch",
        "-q",
        "lemarier/issue-6",
    ])?;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &head, &report)?,
        WorktreeStatus::Ready
    );
    std::fs::write(checkout.join("changed.txt"), "after inspection")?;
    assert_eq!(
        backend.execute(&launch("changed-after-inspection")?),
        Err(EffectFailure::NotApplied(
            NotAppliedReason::WorktreeChanged(WorktreeStatus::Dirty)
        ))
    );
    assert!(sim.calls_to(&["orchestration", "worker-start"]).is_empty());
    std::fs::remove_file(checkout.join("changed.txt"))?;
    git(&[
        "-C",
        checkout.to_str().ok_or("path")?,
        "-c",
        "user.name=Person",
        "-c",
        "user.email=person@example.com",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "moved",
    ])?;
    assert_eq!(
        backend.execute(&launch("moved-head")?),
        Err(EffectFailure::NotApplied(
            NotAppliedReason::WorktreeChanged(WorktreeStatus::WrongHead)
        ))
    );
    assert!(sim.calls_to(&["orchestration", "worker-start"]).is_empty());
    git(&[
        "-C",
        checkout.to_str().ok_or("path")?,
        "reset",
        "--hard",
        "-q",
        head.as_str(),
    ])?;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &commit('a')?, &report)?,
        WorktreeStatus::WrongHead
    );
    std::fs::write(checkout.join("untracked.txt"), "keep")?;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &head, &report)?,
        WorktreeStatus::Dirty
    );
    std::fs::remove_file(checkout.join("untracked.txt"))?;
    sim.state().existing_branch = Some("lemarier/issue-6");
    backend.execute(&launch("unchanged-checkout")?)?;
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    sim.state().identity_worktree = None;
    assert_eq!(
        backend.inspect_worktree(&resource, &branch, &head, &report)?,
        WorktreeStatus::Missing
    );
    Ok(())
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
            ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Claude)),
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
    let cap = u64::try_from(MAX_SCHEDULE_RUNS)?;

    // Listed last, the undated run is the one a plain truncation would drop.
    let mut listing: Vec<_> = (1..=cap + 5).map(dated).collect();
    listing.push(undated());
    assert_eq!(
        observe(listing)?,
        with_undated(1, newest_first(7..=cap + 5)),
        "the undated run counts as the newest; the oldest dated runs are cut"
    );

    // Several undated runs take their places before any dated run does.
    let mut listing: Vec<_> = (1..=cap + 2).map(dated).collect();
    listing.extend([undated(), undated(), undated()]);
    assert_eq!(
        observe(listing)?,
        with_undated(3, newest_first(6..=cap + 2)),
        "three undated runs leave room for the newest dated runs after them"
    );

    // Exactly the cap: nothing is dropped, and an undated run is not moved out.
    let mut listing: Vec<_> = (1..cap).map(dated).collect();
    listing.insert(0, undated());
    assert_eq!(
        observe(listing)?,
        with_undated(1, newest_first(1..=cap - 1)),
        "a listing at the cap keeps every run"
    );

    // More undated runs than the cap still yield only the cap.
    assert_eq!(
        observe((0..=MAX_SCHEDULE_RUNS).map(|_| undated()).collect())?,
        with_undated(MAX_SCHEDULE_RUNS, Vec::new())
    );

    // Without undated runs the newest dated runs are kept, as before.
    assert_eq!(
        observe((1..=cap + 1).map(dated).collect())?,
        newest_first(2..=cap + 1)
    );
    Ok(())
}

#[test]
fn a_run_reports_when_orca_recorded_it_beside_its_due_time() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let installed = backend.install_schedule(&schedule_spec("pickup")?)?;
    let readiness = Readiness::new(&[], at(0), Duration::from_secs(300));
    sim.state().runs = vec![
        json!({"status": "completed", "scheduledFor": 9_000, "createdAt": 9_500}),
        json!({"status": "completed", "createdAt": 5_000}),
        json!({"status": "completed"}),
    ];
    let mut times: Vec<_> = backend
        .inspect_schedule(&installed, &readiness)?
        .recent_runs
        .iter()
        .map(|judged| (judged.run.scheduled_for, judged.run.created_at))
        .collect();
    times.sort_by_key(|(due, recorded)| (due.is_some(), recorded.is_some()));
    assert_eq!(
        times,
        [
            (None, None),
            (None, Some(Timestamp::from_unix_millis(5_000))),
            (
                Some(Timestamp::from_unix_millis(9_000)),
                Some(Timestamp::from_unix_millis(9_500))
            ),
        ],
        "each run keeps the times Orca reported"
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
        pinned: None,
        agent: None,
    })
}

fn stops(sim: &SimOrca) -> usize {
    sim.calls_to(&["orchestration", "worker-stop"]).len()
}

struct BranchPollRunner<'a> {
    sim: &'a SimOrca,
    show_count: AtomicUsize,
    settle_on_second_show: Option<(&'static str, &'static str)>,
    read_delay: Duration,
}

impl OrcaRunner for BranchPollRunner<'_> {
    fn run(&self, invocation: &Invocation) -> Result<RawOutput, OrcaError> {
        let path = invocation.args();
        let branch_read = path.starts_with(&["orchestration".to_owned(), "worker-show".to_owned()])
            || path.starts_with(&["worktree".to_owned(), "show".to_owned()]);
        if let Some((state, outcome)) = self.settle_on_second_show
            && path.starts_with(&["orchestration".to_owned(), "worker-show".to_owned()])
            && self.show_count.fetch_add(1, Ordering::SeqCst) == 1
        {
            for worker in self.sim.state().workers.values_mut() {
                worker.worker_state = state;
                worker.outcome = outcome;
            }
        }
        let result = self.sim.run(invocation)?;
        if branch_read && !self.read_delay.is_zero() {
            thread::sleep(self.read_delay.min(invocation.deadline()));
            if self.read_delay >= invocation.deadline() {
                return Err(OrcaError::Timeout);
            }
        }
        Ok(result)
    }
}

#[test]
fn branch_observation_shares_one_deadline_across_reads_and_sleeps() -> TestResult {
    let sim = SimOrca::default();
    sim.state().branch_reads_hidden = usize::MAX;
    let runner = BranchPollRunner {
        sim: &sim,
        show_count: AtomicUsize::new(0),
        settle_on_second_show: None,
        read_delay: Duration::from_millis(200),
    };
    let mut setup = config(&sim)?;
    setup.call_timeout = Duration::from_millis(800);
    let backend = OrcaBackend::connect(setup, &runner)?;
    let launch = request(
        launch_on("lemarier/deadline", Workspace::Isolated)?,
        "deadline",
    )?;
    let started = Instant::now();
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchUnconfirmedStopped
        ))
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let state = sim.state();
    let poll_reads: Vec<_> = state
        .calls
        .iter()
        .zip(&state.deadlines)
        .take_while(|(call, _)| {
            !call.starts_with(&["orchestration".to_owned(), "worker-stop".to_owned()])
        })
        .filter(|(call, _)| {
            call.starts_with(&["orchestration".to_owned(), "worker-show".to_owned()])
                || call.starts_with(&["worktree".to_owned(), "show".to_owned()])
        })
        .map(|(_, timeout)| *timeout)
        .collect();
    assert!(
        poll_reads.len() >= 4,
        "both reads must occur during polling"
    );
    assert!(
        poll_reads
            .iter()
            .skip(2)
            .take(2)
            .all(|timeout| *timeout < Duration::from_millis(800)),
        "branch reads before stop: {poll_reads:?}"
    );
    assert!(poll_reads[2..4].windows(2).all(|pair| pair[1] <= pair[0]));
    Ok(())
}

#[test]
fn a_dispatch_ended_during_branch_poll_is_never_accepted() -> TestResult {
    for (state, outcome) in [
        ("failed", "failed"),
        ("stopped", "failed"),
        ("ready", "stopped"),
    ] {
        let sim = SimOrca::default();
        sim.state().branch_reads_hidden = 2;
        let runner = BranchPollRunner {
            sim: &sim,
            show_count: AtomicUsize::new(0),
            settle_on_second_show: Some((state, outcome)),
            read_delay: Duration::ZERO,
        };
        let backend = OrcaBackend::connect(config(&sim)?, &runner)?;
        let launch = request(
            launch_on("lemarier/ended-poll", Workspace::Isolated)?,
            "ended-poll",
        )?;
        assert_eq!(
            backend.execute(&launch),
            Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded)),
            "{state}/{outcome}"
        );
        assert!(matches!(backend.resolve(&launch)?, Lookup::Ended(_)));
        assert_eq!(stops(&sim), 0);
    }
    Ok(())
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
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchPrefixMismatchStopped
        ))
    );
    Ok(())
}

#[test]
fn a_prefixed_branch_is_reported_after_checkout_and_kept_in_the_receipt() -> TestResult {
    let sim = SimOrca::default();
    sim.state().branch_reads_hidden = 6;
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("kitchen/issue-237", Workspace::Isolated)?,
        "delayed-prefix",
    )?;
    let receipt = backend.execute(&launch)?;
    assert_eq!(
        sim.calls_to(&["orchestration", "worker-start"])
            .first()
            .and_then(|call| flag(call, "name")),
        Some("issue-237")
    );
    verify_branch(&receipt, "lemarier/issue-237")?;
    assert_eq!(stops(&sim), 0);
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("kitchen/other")?),
        Err(OrcaError::WrongBranchRunning {
            requested: "kitchen/other".to_owned(),
            actual: Some("lemarier/issue-237".to_owned()),
            worker: launched(&receipt)?.handle.to_string(),
        })
    );
    assert_eq!(backend.resolve(&launch)?, Lookup::Applied(receipt));
    Ok(())
}

#[test]
fn a_branch_that_never_appears_is_stopped_as_unconfirmed() -> TestResult {
    let sim = SimOrca::default();
    sim.state().branch_reads_hidden = usize::MAX;
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("kitchen/issue-237", Workspace::Isolated)?,
        "never-reported",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchUnconfirmedStopped
        ))
    );
    assert_eq!(stops(&sim), 1);
    assert!(matches!(backend.resolve(&launch)?, Lookup::Ended(_)));
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("kitchen/issue-237")?),
        Err(OrcaError::BranchUnconfirmed {
            requested: "kitchen/issue-237".to_owned(),
        })
    );
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    Ok(())
}

#[test]
fn a_branch_revealed_after_stop_never_revives_the_launch() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = running_task(&fixture, "task-late-branch", &[Permission::LaunchWorker])?;
    let sim = SimOrca::default();
    sim.state().branch_reads_hidden = usize::MAX;
    let backend = connect(&sim)?;
    let clock = ManualClock::starting_at(1);
    let grants = house_grants(&[Permission::LaunchWorker])?;
    let launch = plan(
        &task,
        fence,
        "launch-1",
        launch_on("lemarier/issue-7", Workspace::Isolated)?,
    )?;
    let first = run_effect(&fixture.store, &backend, &grants, launch.clone(), &clock)?;
    assert!(matches!(
        first.state(),
        EffectState::Uncertain {
            reason: UncertainReason::BranchUnconfirmedStopped,
            ..
        }
    ));
    assert_eq!(stops(&sim), 1);

    sim.state().branch_reads_hidden = 0;
    assert_eq!(
        backend.verify_launch_branch(first.request().key(), &branch("lemarier/issue-7")?),
        Err(OrcaError::LaunchEnded {
            requested: "lemarier/issue-7".to_owned(),
        })
    );
    let report = reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(report.unresolved.is_empty());
    assert!(matches!(
        report.resolved[0].state(),
        EffectState::Ended { .. }
    ));
    let repeated = run_effect(&fixture.store, &backend, &grants, launch, &clock)?;
    assert!(matches!(repeated.state(), EffectState::Ended { .. }));
    assert_eq!(
        fixture.store.task(&task)?.attempts()[0].state(),
        AttemptState::Running
    );
    let ctx = Context {
        store: &fixture.store,
        backend: &backend,
        grants: &grants,
        clock: &clock,
        consent: &Standing,
    };
    let policy = SupervisionPolicy {
        readiness_deadline: Duration::from_secs(120),
        question_deadline: Duration::from_secs(600),
        idle_deadline: Duration::from_secs(240),
        claim_ttl: ttl(60)?,
    };
    assert_eq!(
        supervise(&ctx, &task, fence, &policy, &SupervisionInput::default())?,
        Supervision::AwaitingLaunch
    );
    assert!(matches!(
        fixture.store.task(&task)?.attempts()[0].state(),
        AttemptState::Finished { .. }
    ));
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(stops(&sim), 1);

    let next = request(
        launch_on("lemarier/issue-7-attempt-2", Workspace::Isolated)?,
        "launch-attempt-2",
    )?;
    let fresh = backend.execute(&next)?;
    verify_branch(&fresh, "lemarier/issue-7-attempt-2")?;
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 2);
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
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchPrefixMismatchStopped
        )),
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
        Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded))
    );
    assert!(matches!(backend.resolve(&launch)?, Lookup::Ended(_)));
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(stops(&sim), 1);
    // A key that never dispatched a launch cannot be confirmed either.
    assert_eq!(
        backend.verify_launch_branch(&key("never-launched")?, &branch("lemarier/issue-6")?),
        Err(OrcaError::BranchUnconfirmed {
            requested: "lemarier/issue-6".to_owned(),
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
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchStopUnconfirmed
        ))
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
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchPrefixMismatchStopped
        ))
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
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchPrefixMismatchStopped
        ))
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

fn nothing_launched(sim: &SimOrca) {
    assert!(sim.calls_to(&["orchestration", "task-create"]).is_empty());
    assert!(sim.calls_to(&["orchestration", "worker-start"]).is_empty());
}

#[test]
fn a_branch_an_orca_worktree_holds_is_refused_before_anything_is_created() -> TestResult {
    let sim = SimOrca::default();
    sim.state().worktrees.push((
        "wt_held".to_owned(),
        "refs/heads/lemarier/issue-6".to_owned(),
    ));
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "branch-held",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::NotApplied(NotAppliedReason::BranchInUse))
    );
    nothing_launched(&sim);
    let listings = sim.calls_to(&["worktree", "list"]);
    let [listing] = listings.as_slice() else {
        return Err("expected one worktree listing".into());
    };
    assert_eq!(flag(listing, "repo"), Some("id:repo-1"));
    // The reason, for a caller that asks.
    assert_eq!(
        backend.check_branch_free(&branch("lemarier/issue-6")?),
        Err(OrcaError::BranchTaken {
            requested: "lemarier/issue-6".to_owned()
        })
    );
    // Another branch of the same repository is free.
    assert_eq!(
        backend.check_branch_free(&branch("lemarier/issue-7")?),
        Ok(())
    );

    // Explicit reuse: launching in the worktree that holds the branch.
    sim.state().existing_branch = Some("lemarier/issue-6");
    let reuse = request(
        launch_on(
            "lemarier/issue-6",
            Workspace::Existing(worktree("wt_held")?),
        )?,
        "branch-reused",
    )?;
    let listed = sim.calls_to(&["worktree", "list"]).len();
    let receipt = backend.execute(&reuse)?;
    assert!(receipt.touched().iter().any(|resource| {
        resource.kind == ResourceKind::Branch && resource.handle.as_str() == "lemarier/issue-6"
    }));
    assert_eq!(
        sim.calls_to(&["worktree", "list"]).len(),
        listed,
        "no check for reuse"
    );
    Ok(())
}

#[test]
fn a_listing_that_cannot_show_the_branch_free_refuses_the_launch() -> TestResult {
    type Setup = fn(&SimOrca);
    let setups: [(&str, Setup); 6] = [
        ("truncated", |sim| sim.state().listing_truncated = true),
        ("counts more than it returns", |sim| {
            sim.state().unlisted_worktrees = 1;
        }),
        ("a host left out", |sim| {
            sim.state().omitted_hosts.push("remote-1")
        }),
        ("garbage", |sim| {
            sim.fault_on(&["worktree", "list"], Fault::Garbage)
        }),
        ("timeout", |sim| {
            sim.fault_on(&["worktree", "list"], Fault::TimeoutBeforeEffect);
        }),
        ("refused", |sim| {
            sim.fault_on(&["worktree", "list"], Fault::Refuse("repo_not_found"));
        }),
    ];
    for (case, setup) in setups {
        let sim = SimOrca::default();
        let backend = connect(&sim)?;
        setup(&sim);
        assert_eq!(
            backend.execute(&request(
                launch_on("lemarier/issue-6", Workspace::Isolated)?,
                "unproven",
            )?),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            "{case}"
        );
        nothing_launched(&sim);
    }
    // At the row limit the listing may be cut short, whatever it says.
    let sim = SimOrca::default();
    sim.state().worktrees = (0..MAX_REPO_WORKTREES)
        .map(|n| (format!("wt_{n}"), format!("refs/heads/lemarier/other-{n}")))
        .collect();
    let backend = connect(&sim)?;
    assert_eq!(
        backend.check_branch_free(&branch("lemarier/issue-6")?),
        Err(OrcaError::BranchUnverified)
    );
    Ok(())
}

#[test]
fn a_collision_is_stopped_released_and_reported_with_its_owner() -> TestResult {
    let sim = SimOrca::default();
    // The branch exists in Git only, so the listing cannot see it.
    sim.state().git_branches.push("lemarier/issue-6".to_owned());
    // A host whose stop leaves the terminal open.
    sim.state().stop_releases = false;
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "collision",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchMismatchStopped
        ))
    );
    assert_eq!(stops(&sim), 1);
    let releases = sim.calls_to(&["orchestration", "worker-release"]);
    assert_eq!(releases.len(), 1, "the stopped worker's terminal is closed");
    let (dispatch, stray_worktree) = {
        let state = sim.state();
        let (dispatch, sim_worker) = state.workers.iter().next().ok_or("a worker")?;
        (
            dispatch.clone(),
            sim_worker.worktree.clone().ok_or("a worktree")?,
        )
    };
    let collision = backend
        .launch_collision(launch.key(), &branch("lemarier/issue-6")?)?
        .ok_or("the collision is reported")?;
    assert_eq!(
        collision,
        BranchCollision {
            requested: branch("lemarier/issue-6")?,
            branch: ResourceRef {
                kind: ResourceKind::Branch,
                backend: orca_id()?,
                handle: ExternalRef::new("lemarier/issue-6-2")?,
            },
            worker: worker(&dispatch)?,
            worktrees: vec![worktree(&stray_worktree)?],
            owner: ExternalRef::new(&launch_marker(&house()?, launch.key()))?,
            settled: true,
            terminal_released: true,
        }
    );
    // The launch stays held, and a resubmission starts, stops, and releases
    // nothing again.
    assert_eq!(
        backend.verify_launch_branch(launch.key(), &branch("lemarier/issue-6")?),
        Err(OrcaError::BranchMismatch {
            requested: "lemarier/issue-6".to_owned(),
            actual: Some("lemarier/issue-6-2".to_owned()),
        })
    );
    assert!(matches!(backend.resolve(&launch)?, Lookup::Ended(_)));
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded))
    );
    assert_eq!(sim.calls_to(&["orchestration", "worker-start"]).len(), 1);
    assert_eq!(stops(&sim), 1);
    assert_eq!(sim.calls_to(&["orchestration", "worker-release"]).len(), 1);
    // The adapter never removes the worktree or the branch.
    assert!(sim.calls_to(&["worktree", "rm"]).is_empty());
    // No collision for a key that never launched, or for a branch Orca
    // did not suffix.
    assert_eq!(
        backend.launch_collision(&key("never-launched")?, &branch("lemarier/issue-6")?)?,
        None
    );
    assert_eq!(
        backend.launch_collision(launch.key(), &branch("lemarier/issue-7")?)?,
        None
    );
    Ok(())
}

#[test]
fn a_collision_whose_worker_keeps_running_is_reported_unsettled() -> TestResult {
    let sim = SimOrca::default();
    sim.state().git_branches.push("lemarier/issue-6".to_owned());
    let backend = connect(&sim)?;
    for _ in 0..3 {
        sim.fault_on(
            &["orchestration", "worker-stop"],
            Fault::TimeoutBeforeEffect,
        );
    }
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "collision-running",
    )?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchStopUnconfirmed
        ))
    );
    // A worker that was not stopped keeps its terminal.
    assert!(
        sim.calls_to(&["orchestration", "worker-release"])
            .is_empty()
    );
    let collision = backend
        .launch_collision(launch.key(), &branch("lemarier/issue-6")?)?
        .ok_or("the collision is reported")?;
    assert!(!collision.settled);
    assert!(!collision.terminal_released);
    // A release Orca retains leaves the terminal reported open.
    sim.state().stop_releases = false;
    sim.state().release_action = "retained";
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchMismatchStopped
        ))
    );
    let collision = backend
        .launch_collision(launch.key(), &branch("lemarier/issue-6")?)?
        .ok_or("the collision is reported")?;
    assert!(collision.settled);
    assert!(!collision.terminal_released);
    Ok(())
}

#[test]
fn a_matching_suffix_under_another_requested_branch_is_not_a_collision() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    // The launch asked for `lemarier/issue-2` and got it: no collision. A
    // caller naming `lemarier/issue` sees a numeric suffix, which is not
    // evidence that this launch asked for that branch.
    let launch = request(
        launch_on("lemarier/issue-2", Workspace::Isolated)?,
        "other-request",
    )?;
    backend.execute(&launch)?;
    assert_eq!(
        backend.launch_collision(launch.key(), &branch("lemarier/issue")?)?,
        None
    );
    Ok(())
}

#[test]
fn a_launch_without_a_requested_branch_is_not_a_collision() -> TestResult {
    let sim = SimOrca::default();
    // Orca names the branch after the Task when none is requested; this
    // one exists, so Orca suffixes it exactly as it does a collision.
    sim.state()
        .git_branches
        .push("lemarier/kitchen-task_1".to_owned());
    let backend = connect(&sim)?;
    let launch = request(launch_op("Implement it.")?, "no-request")?;
    let receipt = backend.execute(&launch)?;
    assert!(
        receipt.created().iter().any(|resource| {
            resource.kind == ResourceKind::Branch
                && resource.handle.as_str() == "lemarier/kitchen-task_1-2"
        }),
        "the launch reports a branch that looks like a collision: {receipt:?}"
    );
    assert_eq!(
        backend.launch_collision(launch.key(), &branch("lemarier/kitchen-task_1")?)?,
        None
    );
    Ok(())
}

#[test]
fn a_collision_is_found_when_the_brief_is_long_and_multiline() -> TestResult {
    let sim = SimOrca::default();
    sim.state().git_branches.push("lemarier/issue-6".to_owned());
    let backend = connect(&sim)?;
    // Orca's `--brief` listing would collapse this and cut it at 160
    // characters, dropping the requested-branch line.
    let long = format!("Implement it.\n\n{}", "Details.\n".repeat(60));
    let launch = request(
        Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: Text::new(&long)?,
            branch: Some(branch("lemarier/issue-6")?),
            pinned: None,
            agent: None,
        },
        "long-collision",
    )?;
    let _ = backend.execute(&launch);
    assert!(
        backend
            .launch_collision(launch.key(), &branch("lemarier/issue-6")?)?
            .is_some(),
        "the collision is reported from the full spec"
    );
    Ok(())
}

#[test]
fn duplicate_tasks_for_one_key_record_no_requested_branch() -> TestResult {
    let sim = SimOrca::default();
    sim.state().git_branches.push("lemarier/issue-6".to_owned());
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "duplicated",
    )?;
    let _ = backend.execute(&launch);
    let twin = {
        let state = sim.state();
        let mut twin = state.tasks.last().ok_or("a task")?.clone();
        twin.id = "task_twin".to_owned();
        twin
    };
    sim.state().tasks.push(twin);
    assert_eq!(
        backend.launch_collision(launch.key(), &branch("lemarier/issue-6")?)?,
        None,
        "an unexplained duplicate is not evidence"
    );
    Ok(())
}

#[test]
fn a_mismatch_under_another_prefix_is_not_a_collision() -> TestResult {
    let sim = SimOrca::default();
    sim.state().branch_prefix = "other/";
    let backend = connect(&sim)?;
    let launch = request(
        launch_on("lemarier/issue-6", Workspace::Isolated)?,
        "not-a-collision",
    )?;
    assert!(backend.execute(&launch).is_err());
    assert_eq!(
        backend.launch_collision(launch.key(), &branch("lemarier/issue-6")?)?,
        None
    );
    Ok(())
}

#[test]
fn a_branch_no_name_yields_is_refused_before_anything_is_created() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    // More than one name or a name Orca would rewrite is refused.
    for (index, requested) in ["lemarier/area/topic", "lemarier/-x", "lemarier/a@b"]
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
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchMismatchStopped
        ))
    );
    assert_eq!(stops(&sim), 1);

    // Orca reporting no branch at all cannot confirm the request.
    sim.state().existing_branch = None;
    assert_eq!(
        backend.execute(&request(existing, "repair-unknown")?),
        Err(EffectFailure::Uncertain(
            UncertainReason::BranchUnconfirmedStopped
        ))
    );
    assert_eq!(stops(&sim), 2);
    Ok(())
}

#[test]
fn pre_identity_launch_reconciles_without_writer_base() -> TestResult {
    let sim = SimOrca::default();
    sim.state().existing_branch = Some("lemarier/issue-6");
    let request = request(
        launch_on("lemarier/issue-6", Workspace::Existing(worktree("wt-9")?))?,
        "pre-identity",
    )?;
    let original = connect(&sim)?.execute(&request)?;
    let upgraded = connect(&sim)?.with_writer_identity(
        "house[bot]".into(),
        "123+house[bot]@users.noreply.github.com".into(),
    );
    assert_eq!(upgraded.lookup(&request)?, Lookup::Applied(original));
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
        Lookup::Ended(receipt.clone())
    );
    assert_eq!(backend.resolve(&launch)?, Lookup::Ended(receipt.clone()));
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded)),
        "resubmission never adopts the failed dispatch"
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
    assert_eq!(
        backend.execute(&on_branch),
        Err(EffectFailure::Uncertain(UncertainReason::DispatchEnded))
    );
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
    let EffectState::Ended { receipt, .. } = again.state() else {
        return Err("the failed dispatch was not resolved as ended".into());
    };
    assert_eq!(launched(receipt)?.handle.as_str(), dispatch);
    let once_more = run_effect(&fixture.store, &backend, &grants, launch, &clock)?;
    assert!(matches!(once_more.state(), EffectState::Ended { .. }));
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
