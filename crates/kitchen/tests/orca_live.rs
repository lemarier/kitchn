//! Controlled live smoke test against the installed Orca runtime.
//!
//! Opt-in only. It runs when `KITCHEN_ORCA_LIVE=1` and these are set:
//!
//! - `KITCHEN_ORCA_REPO`: repository selector for the throwaway worktree,
//!   such as `id:<repo-id>`.
//! - `KITCHEN_ORCA_WORKTREE`: path of an Orca worktree where the throwaway
//!   coordinator terminal is opened.
//! - `KITCHEN_ORCA_BRANCH_PREFIX`: must be `kitchen`, and must be what Orca's
//!   Git branch-prefix setting is on this host. The shared suite launches on
//!   the exact branch `kitchen/<tag>`, and Orca's CLI can only create a
//!   branch as the host's prefix plus a name, so on a host with another
//!   prefix (or none) the adapter refuses that launch. Setting this without
//!   changing Orca's setting makes the launch fail its branch check and stop
//!   the worker; the test then fails.
//! - `KITCHEN_ORCA_BASE_BRANCH` (optional): base ref for the worktree.
//! - `KITCHEN_ORCA_AGENT` (optional): `claude` (default) or `codex`.
//!
//! It creates its own coordinator terminal and Run, runs the shared contract
//! suite (which launches and stops one worker), then stops and releases that
//! worker, removes the worktree it created, and closes the terminal. Orca has
//! no command to delete a Run, so each invocation leaves one empty Run whose
//! objective marks it as a throwaway smoke test. It never
//! reads or changes automations, or any Run, worker, worktree, or terminal it
//! did not create. Without the gate it reports that it was skipped.

use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

use kitchen::{
    BackendId, CredentialId, HouseId, TaskId,
    adapters::orca::{
        DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT, Invocation,
        OrcaBackend, OrcaConfig, OrcaRunner, SystemRunner, redact, verify_branch,
    },
    contracts::{
        AttemptNumber, BranchName, EffectExecutor, EffectRequest, ExternalRef, IdempotencyKey,
        Lookup, Operation, Repository, ResourceKind, Role, Text, WorkerBackend, WorkerOutcome,
        WorkerState, Workspace,
        conformance::{self, ConformanceFixture},
    },
    scheduling::AgentFamily,
};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const GATE: &str = "KITCHEN_ORCA_LIVE";

struct LiveSettings {
    repo: ExternalRef,
    worktree: String,
    base_branch: Option<ExternalRef>,
    branch_prefix: BranchName,
    agent: AgentFamily,
}

fn settings() -> TestResult<Option<LiveSettings>> {
    if env::var(GATE).ok().as_deref() != Some("1") {
        return Ok(None);
    }
    let required = |name: &str| env::var(name).map_err(|_| format!("{GATE}=1 requires {name}"));
    Ok(Some(LiveSettings {
        repo: ExternalRef::new(&required("KITCHEN_ORCA_REPO")?)?,
        worktree: required("KITCHEN_ORCA_WORKTREE")?,
        base_branch: env::var("KITCHEN_ORCA_BASE_BRANCH")
            .ok()
            .map(|base| ExternalRef::new(&base))
            .transpose()?,
        branch_prefix: match required("KITCHEN_ORCA_BRANCH_PREFIX")?.as_str() {
            "kitchen" => BranchName::new("kitchen")?,
            _ => {
                return Err("KITCHEN_ORCA_BRANCH_PREFIX must be kitchen: the shared suite launches on kitchen/<tag>".into());
            }
        },
        agent: match env::var("KITCHEN_ORCA_AGENT").ok().as_deref() {
            None | Some("claude") => AgentFamily::Claude,
            Some("codex") => AgentFamily::Codex,
            Some(_) => return Err("KITCHEN_ORCA_AGENT must be claude or codex".into()),
        },
    }))
}

fn orca(runner: &SystemRunner, args: &[&str]) -> TestResult<Value> {
    let owned = args.iter().map(|arg| (*arg).to_owned()).collect();
    let output = runner.run(&Invocation::new(owned, DEFAULT_CALL_TIMEOUT))?;
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    if envelope.get("ok") == Some(&Value::Bool(true)) {
        return Ok(envelope.get("result").cloned().unwrap_or(Value::Null));
    }
    // Name the command, never its argument values.
    let command: Vec<&str> = args
        .iter()
        .copied()
        .filter(|arg| !arg.starts_with("--"))
        .collect();
    let code = envelope
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = envelope
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(redact)
        .unwrap_or_default();
    Err(format!("orca {} failed: {code}: {message}", command.join(" ")).into())
}

/// The first string at `key` anywhere in `value` that starts with `prefix`.
fn find(value: &Value, key: &str, prefix: &str) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .filter(|found| found.starts_with(prefix))
            .map(str::to_owned)
            .or_else(|| map.values().find_map(|nested| find(nested, key, prefix))),
        Value::Array(items) => items.iter().find_map(|nested| find(nested, key, prefix)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

#[test]
fn live_orca_smoke() -> TestResult {
    let Some(settings) = settings()? else {
        println!("skipped: live Orca smoke test needs {GATE}=1 (see tests/orca_live.rs)");
        return Ok(());
    };
    let runner = SystemRunner::new("orca");
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut terminals = Vec::new();
    let handle = create_terminal(&runner, &settings, &mut terminals)?;
    let mut run_id = None;
    let outcome = exercise(
        &runner,
        &settings,
        &handle,
        stamp,
        &mut run_id,
        &mut terminals,
    );
    let cleanup = clean_up(&runner, &terminals, run_id.as_deref());
    println!("LIVE result: {outcome:?}");
    println!("LIVE cleanup: {cleanup:?}");
    outcome.and(cleanup)
}

/// Open a throwaway coordinator terminal and remember it for cleanup.
fn create_terminal(
    runner: &SystemRunner,
    settings: &LiveSettings,
    terminals: &mut Vec<String>,
) -> TestResult<String> {
    let terminal = orca(
        runner,
        &[
            "terminal",
            "create",
            &format!("--worktree=path:{}", settings.worktree),
            "--title=kitchen-smoke",
            "--json",
        ],
    )?;
    let handle = find(&terminal, "handle", "term_").ok_or("terminal create returned no handle")?;
    println!("LIVE created coordinator terminal {handle}");
    terminals.push(handle.clone());
    Ok(handle)
}

fn exercise(
    runner: &SystemRunner,
    settings: &LiveSettings,
    handle: &str,
    stamp: u64,
    created_run: &mut Option<String>,
    terminals: &mut Vec<String>,
) -> TestResult {
    let run = orca(
        runner,
        &[
            "orchestration",
            "run-create",
            "--objective=Kitchen #6 adapter smoke test (throwaway)",
            &format!("--from={handle}"),
            "--json",
        ],
    )?;
    let run_id = find(&run, "id", "run_")
        .or_else(|| find(&run, "runId", "run_"))
        .ok_or("run-create returned no run id")?;
    println!("LIVE created run {run_id}");
    *created_run = Some(run_id.clone());
    // Reservation files for this test's launches; removed with the directory.
    let runtime = tempfile::tempdir()?;
    let config = OrcaConfig {
        backend: BackendId::new("orca-local")?,
        house: HouseId::new("kitchen-smoke")?,
        credential: CredentialId::new("orca-local-session")?,
        run: ExternalRef::new(&run_id)?,
        coordinator: ExternalRef::new(handle)?,
        repo: settings.repo.clone(),
        base_branch: settings.base_branch.clone(),
        branch_prefix: Some(settings.branch_prefix.clone()),
        agent: settings.agent,
        call_timeout: DEFAULT_CALL_TIMEOUT,
        launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
        runtime_dir: runtime.path().to_path_buf(),
        reservation_timeout: DEFAULT_RESERVATION_TIMEOUT,
    };
    let backend = OrcaBackend::connect(config, runner)?;
    println!(
        "LIVE runtime {} (supported range accepted)",
        backend.runtime().version
    );
    let fixture = ConformanceFixture {
        house: HouseId::new("kitchen-smoke")?,
        foreign_house: HouseId::new("kitchen-smoke-other")?,
        foreign_backend: BackendId::new("orca-other")?,
        credential: CredentialId::new("orca-local-session")?,
        task: TaskId::new("smoke")?,
        repository: Repository::new("lemarier/kitchen")?,
        run_tag: ExternalRef::new(&format!("smoke-{stamp}"))?,
        brief: Text::new(
            "Kitchen adapter smoke test. Do nothing: do not edit files, run commands, or send \
             messages. You will be stopped within a minute.",
        )?,
    };
    let report = conformance::run_worker(&backend, &fixture);
    match &report {
        Ok(report) => {
            for (check, result) in &report.results {
                println!("LIVE check {check}: {result:?}");
            }
        }
        Err(failure) => println!("LIVE conformance failure: {failure}"),
    }
    let adapter = report
        .is_ok()
        .then(|| adapter_checks(&backend, &fixture))
        .map(|checked| checked.and_then(|()| adoption(runner, settings, &backend, terminals)));
    if let Some(Err(error)) = &adapter {
        println!("LIVE adapter check failed: {error}");
    }
    let inventory = backend.worker_records();
    match &inventory {
        Ok(records) => {
            for record in records {
                println!(
                    "LIVE inventory {} owner={:?} state={:?} liveness={:?} terminal={:?}",
                    record.worker.handle,
                    record.owner.as_ref().map(ExternalRef::as_str),
                    record.state,
                    record.liveness,
                    record.terminal
                );
            }
        }
        Err(error) => println!("LIVE inventory failed: {error}"),
    }
    report?;
    inventory?;
    adapter.unwrap_or(Ok(()))
}

fn smoke_request(
    fixture: &ConformanceFixture,
    backend: &BackendId,
    suffix: &str,
    operation: Operation,
) -> TestResult<EffectRequest> {
    Ok(EffectRequest::new(
        fixture.house.clone(),
        backend.clone(),
        fixture.credential.clone(),
        fixture.task.clone(),
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new(&format!("{}-{suffix}", fixture.run_tag))?),
        operation.into(),
    ))
}

/// Checks beyond the shared suite: the probe launch is found by its key and
/// reads as cancelled; a second worker receives a message, is stopped, and
/// its release is idempotent.
fn adapter_checks(
    backend: &OrcaBackend<&SystemRunner>,
    fixture: &ConformanceFixture,
) -> TestResult {
    let namespace = backend.descriptor().backend.clone();
    let probe_key =
        IdempotencyKey::from_ref(ExternalRef::new(&format!("{}-probe", fixture.run_tag))?);
    let Lookup::Applied(probe) = backend.lookup_launch(&probe_key)? else {
        return Err("the probe launch was not found by its key".into());
    };
    let probe_worker = probe
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("probe receipt names no worker")?;
    let branch = probe
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Branch)
        .map(|branch| branch.handle.as_str().to_owned())
        .ok_or("the launch receipt names no branch")?;
    println!("LIVE probe branch as created by Orca: {branch}");
    verify_branch(&probe, &branch)?;
    let observed = backend.observe_worker(&probe_worker)?;
    println!("LIVE probe after cancel: {observed:?}");
    if observed != WorkerState::Settled(WorkerOutcome::Cancelled) {
        return Err("a stopped worker did not read as cancelled".into());
    }

    let launch = smoke_request(
        fixture,
        &namespace,
        "second",
        Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: fixture.brief.clone(),
            branch: None,
        },
    )?;
    let receipt = backend.execute(&launch)?;
    let again = backend.execute(&launch)?;
    println!(
        "LIVE relaunch with the same key returned the same receipt: {}",
        again == receipt
    );
    if again != receipt {
        return Err("resubmitting a launch key changed the receipt".into());
    }
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("second receipt names no worker")?;
    println!(
        "LIVE second worker {} state {:?}",
        worker.handle,
        backend.observe_worker(&worker)?
    );
    let message = backend.execute(&smoke_request(
        fixture,
        &namespace,
        "message",
        Operation::MessageWorker {
            worker: worker.clone(),
            body: Text::new("Kitchen smoke test: no action needed; you will be stopped.")?,
        },
    )?)?;
    println!("LIVE message receipt reference {}", message.reference());
    let delivery = backend.next_delivery()?;
    println!(
        "LIVE mailbox batch: {}",
        delivery.map_or(0, |delivery| delivery.messages.len())
    );
    let cancel = smoke_request(
        fixture,
        &namespace,
        "second-cancel",
        Operation::CancelWorker {
            worker: worker.clone(),
        },
    )?;
    backend.execute(&cancel)?;
    println!(
        "LIVE second worker after cancel: {:?}",
        backend.observe_worker(&worker)?
    );
    println!("LIVE cancel resolves as {:?}", backend.resolve(&cancel)?);
    let release = smoke_request(
        fixture,
        &namespace,
        "second-release",
        Operation::ReleaseResource { resource: worker },
    )?;
    let first = backend.execute(&release)?;
    let second = backend.execute(&release)?;
    println!(
        "LIVE release twice gives the same receipt: {}",
        first == second
    );
    Ok(())
}

/// A second coordinator terminal adopts the Run: it reads the mailbox, and
/// the first terminal no longer can.
fn adoption(
    runner: &SystemRunner,
    settings: &LiveSettings,
    backend: &OrcaBackend<&SystemRunner>,
    terminals: &mut Vec<String>,
) -> TestResult {
    let adopter = create_terminal(runner, settings, terminals)?;
    let adopting = OrcaBackend::connect(
        OrcaConfig {
            coordinator: ExternalRef::new(&adopter)?,
            ..backend.config().clone()
        },
        runner,
    )?;
    adopting.adopt_run()?;
    let waited = adopting.await_delivery(std::time::Duration::from_secs(2))?;
    println!(
        "LIVE bounded wait returned {} actionable messages",
        waited.map_or(0, |delivery| delivery.actionable().count())
    );
    let read = adopting.next_delivery();
    println!("LIVE adopter reads the mailbox: {}", read.is_ok());
    read?;
    match backend.next_delivery() {
        Ok(_) => println!("LIVE previous coordinator can still read the mailbox"),
        Err(error) => println!("LIVE previous coordinator after adoption: {error}"),
    }
    Ok(())
}

/// Stop, release, and remove only what this test created, then close its terminals.
fn clean_up(runner: &SystemRunner, terminals: &[String], run_id: Option<&str>) -> TestResult {
    let mut problems = Vec::new();
    if let Some(run_id) = run_id {
        let workers = orca(
            runner,
            &[
                "orchestration",
                "worker-list",
                &format!("--run={run_id}"),
                "--json",
            ],
        );
        let rows = workers
            .as_ref()
            .ok()
            .and_then(|list| list.get("workers"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for row in rows {
            let Some(dispatch) = row.get("dispatchId").and_then(Value::as_str) else {
                continue;
            };
            let shown = orca(
                runner,
                &[
                    "orchestration",
                    "worker-show",
                    &format!("--dispatch={dispatch}"),
                    "--json",
                ],
            );
            let worktree = shown
                .as_ref()
                .ok()
                .and_then(|show| show.pointer("/worker/worktreeId"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            // Only a reported or stopped outcome ends the worker; anything
            // else, including an unknown outcome, is stopped explicitly.
            let settled = shown
                .as_ref()
                .ok()
                .and_then(|show| show.pointer("/projection/outcome"))
                .and_then(Value::as_str)
                .is_some_and(|outcome| matches!(outcome, "succeeded" | "failed" | "stopped"));
            if !settled
                && let Err(error) = orca(
                    runner,
                    &[
                        "orchestration",
                        "worker-stop",
                        &format!("--dispatch={dispatch}"),
                        "--json",
                    ],
                )
            {
                problems.push(format!("stop {dispatch}: {error}"));
            }
            match orca(
                runner,
                &[
                    "orchestration",
                    "worker-release",
                    &format!("--dispatch={dispatch}"),
                    "--json",
                ],
            ) {
                Ok(release) => println!(
                    "LIVE release {dispatch}: {}",
                    release.get("state").and_then(Value::as_str).unwrap_or("?")
                ),
                Err(error) => problems.push(format!("release {dispatch}: {error}")),
            }
            if let Some(worktree) = worktree {
                match orca(
                    runner,
                    &[
                        "worktree",
                        "rm",
                        &format!("--worktree=id:{worktree}"),
                        "--force",
                        "--json",
                    ],
                ) {
                    Ok(_) => println!("LIVE removed worktree of {dispatch}"),
                    Err(error) => problems.push(format!("worktree of {dispatch}: {error}")),
                }
            }
        }
        if let Err(error) = workers {
            problems.push(format!("worker-list: {error}"));
        }
    }
    for handle in terminals {
        if let Err(error) = orca(
            runner,
            &[
                "terminal",
                "close",
                &format!("--terminal={handle}"),
                "--tab",
                "--json",
            ],
        ) {
            problems.push(format!("terminal close {handle}: {error}"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; ").into())
    }
}
