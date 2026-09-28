//! Controlled live smoke test against the installed Orca runtime.
//!
//! Opt-in only. It runs when `KITCHEN_ORCA_LIVE=1` and these are set:
//!
//! - `KITCHEN_ORCA_REPO`: repository selector for the throwaway worktree,
//!   such as `id:<repo-id>`.
//! - `KITCHEN_ORCA_WORKTREE`: path of an Orca worktree where the throwaway
//!   coordinator terminal is opened.
//! - `KITCHEN_ORCA_BASE_BRANCH` (optional): base ref for the worktree.
//! - `KITCHEN_ORCA_AGENT` (optional): `claude` (default) or `codex`.
//!
//! Orca's CLI can only create a branch as the host's branch prefix plus a
//! name, and the prefix is not a fixed value: it comes from Orca's setting,
//! Git config, or the `gh` login. Nothing here is configured for it. The test
//! first creates a throwaway worktree with a known name, reads back the
//! branch Orca gave it, derives the prefix from that, and removes the
//! worktree. The shared suite then launches on `<prefix>/kitchen-smoke-<tag>`,
//! and the launch's branch is read back again through Orca's worktree record.
//!
//! It creates its own coordinator terminal and Run, runs the shared contract
//! suite (which launches and stops one worker), launches a second worker
//! whose recovery signals it reads while it runs and after it is stopped
//! (then messages the stopped worker to see Orca refuse it), and launches a
//! third worker on its own requested branch whose agent terminal it closes,
//! so the worker's process exits without a stop: its launch must still be
//! found, and resubmitting it must start nothing. It then stops and releases
//! the workers, removes the worktrees it created, and closes the terminals. Orca has no command to delete a Run, so each invocation leaves
//! one empty Run whose objective marks it as a throwaway smoke test. It never
//! reads or changes automations, or any Run, worker, worktree, or terminal it
//! did not create. Without the gate it reports that it was skipped.

use std::{
    env,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use kitchen::{
    BackendId, CredentialId, HouseId, TaskId,
    adapters::orca::{
        DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT,
        DispatchActivity, Invocation, OrcaBackend, OrcaConfig, OrcaRunner, StartWindow,
        SystemRunner, TerminalOwner, launch_marker, redact, verify_branch,
    },
    contracts::{
        AttemptNumber, BranchName, Clock, EffectExecutor, EffectFailure, EffectRequest,
        ExternalRef, IdempotencyKey, Lookup, NotAppliedReason, Operation, Repository, ResourceKind,
        Role, SystemClock, Text, WorkerBackend, WorkerOutcome, WorkerState, Workspace,
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

/// The prefix Orca put in front of `name` in the branch it reports: none when
/// the branch is `name`.
fn prefix_of(branch: &str, name: &str) -> TestResult<Option<BranchName>> {
    let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if branch == name {
        return Ok(None);
    }
    let prefix = branch
        .strip_suffix(name)
        .and_then(|rest| rest.strip_suffix('/'))
        .ok_or_else(|| {
            format!("Orca gave the probe branch {branch}, which does not end in /{name}")
        })?;
    Ok(Some(BranchName::new(prefix)?))
}

/// The branch `name` becomes under `prefix`.
fn smoke_branch(prefix: Option<&BranchName>, name: &str) -> TestResult<BranchName> {
    Ok(match prefix {
        Some(prefix) => BranchName::new(&format!("{}/{name}", prefix.as_str()))?,
        None => BranchName::new(name)?,
    })
}

/// The branch prefix this Orca host applies, read from the host itself.
///
/// Orca's CLI does not expose the setting, and the prefix depends on it, on
/// Git config, and on the `gh` login, so the test creates a throwaway
/// worktree with a known name, reads the branch Orca gave it, and removes it.
/// The worktree is removed on every path after it exists.
fn host_branch_prefix(
    runner: &SystemRunner,
    settings: &LiveSettings,
    stamp: u64,
) -> TestResult<Option<BranchName>> {
    let name = format!("kitchen-smoke-{stamp}-prefix");
    let mut args = vec![
        "worktree".to_owned(),
        "create".to_owned(),
        format!("--repo={}", settings.repo),
        format!("--name={name}"),
        "--no-parent".to_owned(),
        "--setup=skip".to_owned(),
        "--json".to_owned(),
    ];
    if let Some(base) = &settings.base_branch {
        args.push(format!("--base-branch={base}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let created = orca(runner, &args)?;
    let id = created
        .pointer("/worktree/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            format!("worktree create returned no id; a worktree named {name} may remain")
        })?;
    println!("LIVE created prefix probe worktree {name}");
    let branch = created
        .pointer("/worktree/branch")
        .and_then(Value::as_str)
        .ok_or_else(|| "worktree create returned no branch".into())
        .and_then(|branch| prefix_of(branch, &name));
    let removed = orca(
        runner,
        &[
            "worktree",
            "rm",
            &format!("--worktree=id:{id}"),
            "--force",
            "--json",
        ],
    );
    println!("LIVE removed prefix probe worktree: {}", removed.is_ok());
    let prefix = branch?;
    removed?;
    Ok(prefix)
}

#[cfg(test)]
mod prefix_tests {
    use super::*;

    #[test]
    fn the_prefix_is_what_orca_put_before_the_name() -> TestResult {
        for (reported, expected) in [
            ("refs/heads/lemarier/probe-1", Some("lemarier")),
            ("lemarier/probe-1", Some("lemarier")),
            ("refs/heads/team/lemarier/probe-1", Some("team/lemarier")),
            ("refs/heads/probe-1", None),
            ("probe-1", None),
        ] {
            assert_eq!(
                prefix_of(reported, "probe-1")?
                    .as_ref()
                    .map(BranchName::as_str),
                expected,
                "{reported}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_branch_that_does_not_end_in_the_name_is_not_a_prefix() {
        for reported in [
            "refs/heads/lemarier/other",
            "refs/heads/lemarier-probe-1",
            "refs/heads/lemarier/x-probe-1",
            "",
        ] {
            assert!(prefix_of(reported, "probe-1").is_err(), "{reported}");
        }
    }

    #[test]
    fn the_smoke_branch_is_the_prefix_and_the_name() -> TestResult {
        let prefix = BranchName::new("lemarier")?;
        assert_eq!(
            smoke_branch(Some(&prefix), "kitchen-smoke-7")?.as_str(),
            "lemarier/kitchen-smoke-7"
        );
        assert_eq!(
            smoke_branch(None, "kitchen-smoke-7")?.as_str(),
            "kitchen-smoke-7"
        );
        Ok(())
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
    let prefix = host_branch_prefix(runner, settings, stamp)?;
    println!("LIVE host branch prefix: {prefix:?}");
    let branch = smoke_branch(prefix.as_ref(), &format!("kitchen-smoke-{stamp}"))?;
    println!("LIVE requested branch: {branch}");
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
        branch_prefix: prefix,
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
    let report = conformance::run_worker_on_branch(&backend, &fixture, &branch);
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
        .then(|| adapter_checks(runner, &backend, &fixture, &branch))
        .map(|checked| checked.and_then(|()| exit_checks(runner, &backend, &fixture, &branch)))
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
    runner: &SystemRunner,
    backend: &OrcaBackend<&SystemRunner>,
    fixture: &ConformanceFixture,
    requested: &BranchName,
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
    verify_branch(&probe, requested.as_str())?;
    println!("LIVE probe launch receipt names exactly {requested}");
    // A second reading, through Orca's own record of the worktree.
    let worktree = probe
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worktree)
        .map(|worktree| worktree.handle.as_str().to_owned())
        .ok_or("the launch receipt names no worktree")?;
    let shown = orca(
        runner,
        &[
            "worktree",
            "show",
            &format!("--worktree=id:{worktree}"),
            "--json",
        ],
    )?;
    let recorded = shown
        .pointer("/worktree/branch")
        .and_then(Value::as_str)
        .map(|branch| {
            branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_owned()
        });
    println!("LIVE probe worktree branch per `worktree show`: {recorded:?}");
    if recorded.as_deref() != Some(requested.as_str()) {
        return Err(format!("`worktree show` reports {recorded:?}, not {requested}").into());
    }
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
            agent: None,
        },
    )?;
    let launched_at = SystemClock.now();
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
    // Recovery signals, read-only, of a worker this test launched.
    let window = || StartWindow::new(launched_at, SystemClock.now(), Duration::from_secs(60));
    let running = backend
        .observe_signals(&worker, &window())?
        .ok_or("Orca has no record of the second worker")?;
    println!("LIVE second worker signals while running: {running:?}");
    if running.dispatch != DispatchActivity::Active || running.terminal != TerminalOwner::Supervised
    {
        return Err(format!("a fresh worker reads as {running:?}").into());
    }
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
    let stopped = backend
        .observe_signals(&worker, &window())?
        .ok_or("Orca has no record of the stopped worker")?;
    println!("LIVE second worker signals after cancel: {stopped:?}");
    if stopped.dispatch != DispatchActivity::Ended || stopped.accepts_messages() {
        return Err(format!("a stopped worker reads as {stopped:?}").into());
    }
    // Orca refuses a message to the Dispatch it now shows as ended
    // (`dispatch_inactive`, observed on 1.4.212), before queueing anything.
    let late = backend.execute(&smoke_request(
        fixture,
        &namespace,
        "after-stop",
        Operation::MessageWorker {
            worker: worker.clone(),
            body: Text::new("Kitchen smoke test: sent after the stop; no action needed.")?,
        },
    )?);
    println!("LIVE message to the stopped worker: {late:?}");
    if !matches!(
        late,
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    ) {
        return Err(format!("a message to a stopped worker was {late:?}").into());
    }
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

/// A worker whose process exits without a stop keeps its launch: Orca
/// returns its Task to `ready`, and the adapter must still find the original
/// Dispatch, return it on resubmission without a second worker, and read a
/// cancel of the settled worker as applied.
fn exit_checks(
    runner: &SystemRunner,
    backend: &OrcaBackend<&SystemRunner>,
    fixture: &ConformanceFixture,
    requested: &BranchName,
) -> TestResult {
    let namespace = backend.descriptor().backend.clone();
    let branch = BranchName::new(&format!("{requested}-exit"))?;
    let launch = smoke_request(
        fixture,
        &namespace,
        "exit",
        Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: fixture.brief.clone(),
            branch: Some(branch.clone()),
            agent: None,
        },
    )?;
    let launched_at = SystemClock.now();
    let receipt = backend.execute(&launch)?;
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("the exit receipt names no worker")?;
    let dispatch = worker.handle.as_str();
    let shown = orca(
        runner,
        &[
            "orchestration",
            "worker-show",
            &format!("--dispatch={dispatch}"),
            "--json",
        ],
    )?;
    let agent_terminal = shown
        .pointer("/worker/agentTerminalHandle")
        .and_then(Value::as_str)
        .ok_or("the exit worker has no agent terminal")?
        .to_owned();
    // End the agent's process without a stop: close the terminal this test's
    // worker runs in.
    orca(
        runner,
        &[
            "terminal",
            "close",
            &format!("--terminal={agent_terminal}"),
            "--json",
        ],
    )?;
    println!("LIVE closed the exit worker's terminal {agent_terminal}");
    let mut state = backend.observe_worker(&worker)?;
    for _ in 0..30 {
        if matches!(state, WorkerState::Settled(_)) {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        state = backend.observe_worker(&worker)?;
    }
    let exited = orca(
        runner,
        &[
            "orchestration",
            "worker-show",
            &format!("--dispatch={dispatch}"),
            "--json",
        ],
    )?;
    println!(
        "LIVE exit worker: {state:?}, worker {:?} at {:?}, dispatch {:?}, terminal shown: {}",
        exited.pointer("/worker/state").and_then(Value::as_str),
        exited.pointer("/worker/stage").and_then(Value::as_str),
        exited.pointer("/dispatch/status").and_then(Value::as_str),
        exited
            .pointer("/terminal")
            .is_some_and(|terminal| !terminal.is_null()),
    );
    let task = receipt.reference().as_str();
    let tasks = orca(
        runner,
        &[
            "orchestration",
            "task-list",
            &format!("--run={}", backend.config().run),
            "--json",
        ],
    )?;
    let status = tasks
        .get("tasks")
        .and_then(Value::as_array)
        .and_then(|tasks| {
            tasks
                .iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(task))
        })
        .and_then(|row| row.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    println!("LIVE exit worker's Task {task} is now {status:?}");
    if state != WorkerState::Settled(WorkerOutcome::Failed) {
        return Err(format!("an exited worker reads as {state:?}").into());
    }
    let found = backend.lookup_launch(launch.key())?;
    println!(
        "LIVE lookup after the exit finds the original launch: {}",
        found == Lookup::Applied(receipt.clone())
    );
    if found != Lookup::Applied(receipt.clone()) {
        return Err(format!("after the exit, lookup gives {found:?}, not {receipt:?}").into());
    }
    let resolved = backend.resolve(&launch)?;
    let again = backend.execute(&launch);
    let marker = launch_marker(&fixture.house, launch.key());
    let workers = backend
        .worker_records()?
        .into_iter()
        .filter(|record| record.owner.as_ref().map(ExternalRef::as_str) == Some(marker.as_str()))
        .count();
    println!(
        "LIVE resolve after the exit: {resolved:?}; resubmission returns the original: {}; \
         workers for the key: {workers}",
        again.as_ref() == Ok(&receipt)
    );
    if resolved != Lookup::Applied(receipt.clone()) || again != Ok(receipt.clone()) || workers != 1
    {
        return Err(format!(
            "after the exit: resolve {resolved:?}, resubmit {again:?}, {workers} workers"
        )
        .into());
    }
    let signals = backend
        .observe_signals(
            &worker,
            &StartWindow::new(launched_at, SystemClock.now(), Duration::from_secs(60)),
        )?
        .ok_or("Orca has no record of the exited worker")?;
    println!("LIVE exit worker signals: {signals:?}");
    let cancel = smoke_request(
        fixture,
        &namespace,
        "exit-cancel",
        Operation::CancelWorker { worker },
    )?;
    let cancelled = backend.execute(&cancel);
    println!(
        "LIVE cancel of the exited worker: {:?}; resolves as {:?}",
        cancelled
            .as_ref()
            .map(|receipt| receipt.reference().as_str()),
        backend.resolve(&cancel)?
    );
    cancelled?;
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
