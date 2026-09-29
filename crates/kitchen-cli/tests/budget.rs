//! The `kitchn budget` process contract. Orca is a fake `orca` script that
//! keeps one schedule's enabled flag in a file; no live Orca or automation is
//! contacted, so none of this is live runtime evidence.
#![cfg(unix)]

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use kitchen::{
    BackendId, CredentialId, HouseId,
    adoption::HouseRegistry,
    contracts::{Grant, Permission},
    house::{BackendBinding, BackendKind, HouseConfig},
    state::{HouseStore, StoreOptions},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_ORCA: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
ok() { printf '{"id":"fake","ok":true,"result":%s}' "$1"; }
case "$1 $2" in
  "status --json")
    [ -f "$dir/down" ] && exit 1
    ok '{"runtime":{"state":"ready","reachable":true,"appVersion":"1.4.212","capabilities":["orchestration.contract.v1","orchestration.worker-stop-verdict.v1"]}}' ;;
  "automations list")
    ok "{\"automations\":[{\"id\":\"auto-1\",\"name\":\"kitchen:origin89:pickup\",\"enabled\":$(cat "$dir/enabled")}]}" ;;
  "automations runs")
    ok "{\"runs\":$(cat "$dir/runs")}" ;;
  "automations edit")
    case "$*" in
      *--disabled*) echo false > "$dir/enabled" ;;
      *--enabled*) echo true > "$dir/enabled" ;;
    esac
    ok '{"id":"auto-1"}' ;;
  *) exit 1 ;;
esac
"#;

/// A house, its store, and a fake Orca with one active schedule that ran
/// `runs` times just now.
struct Kitchen {
    _dir: tempfile::TempDir,
    /// The directory without symlinks, which the store and registry refuse.
    root: PathBuf,
}

impl Kitchen {
    fn new(runs: u64, config: impl FnOnce(&mut HouseConfig) -> TestResult) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let kitchen = Self { _dir: dir, root };
        let mut house: HouseConfig = serde_json::from_str(include_str!(
            "../../kitchen/tests/fixtures/house/origin89.json"
        ))?;
        let grant = Grant::house(Permission::ManageSchedule, orca()?, credential()?);
        house.policy_limits.insert(grant.clone());
        house.grants.insert(grant);
        house.backend = Some(BackendBinding {
            kind: BackendKind::Orca.into(),
            backend: orca()?,
            credential: credential()?,
        });
        house.schedules = Some(serde_json::from_value(serde_json::json!({
            "windowHours": 24,
            "minIntervalMinutes": 60,
            "houseBudget": {"runs": 10},
            "scheduleBudget": {"runs": 4},
        }))?);
        config(&mut house)?;
        HouseRegistry::new(kitchen.path("registry"))?.initialize(&house)?;
        HouseStore::initialize(
            kitchen.path("store"),
            HouseId::new("origin89")?,
            StoreOptions::default(),
        )?;
        let bin = kitchen.path("orca-bin");
        fs::create_dir(&bin)?;
        fs::create_dir(kitchen.path("runtime"))?;
        let script = bin.join("orca");
        fs::write(&script, FAKE_ORCA)?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        fs::write(bin.join("enabled"), "true")?;
        fs::write(bin.join("calls"), "")?;
        kitchen.set_runs(runs)?;
        Ok(kitchen)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn orca_file(&self, name: &str) -> PathBuf {
        self.path("orca-bin").join(name)
    }

    /// `count` completed runs due in the last few seconds.
    fn set_runs(&self, count: u64) -> TestResult {
        let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let runs: Vec<_> = (0..count)
            .map(|index| {
                serde_json::json!({"id": format!("run-{index}"), "status": "completed",
                    "scheduledFor": now.saturating_sub(index * 1000)})
            })
            .collect();
        fs::write(self.orca_file("runs"), serde_json::to_string(&runs)?)?;
        Ok(())
    }

    fn enabled(&self) -> TestResult<bool> {
        Ok(fs::read_to_string(self.orca_file("enabled"))?.trim() == "true")
    }

    fn calls(&self, prefix: &str) -> TestResult<Vec<String>> {
        Ok(fs::read_to_string(self.orca_file("calls"))?
            .lines()
            .filter(|line| line.starts_with(prefix))
            .map(str::to_owned)
            .collect())
    }

    fn budget(&self, command: &str, extra: &[&str]) -> TestResult<Output> {
        let mut args = self.source(command)?;
        args.extend([
            "--backend".into(),
            "orca-local".into(),
            "--credential".into(),
            "orca-host-session".into(),
        ]);
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(&args)
            .output()?)
    }

    /// `command` with the house, store, and Orca host, but no backend flags.
    fn source(&self, command: &str) -> TestResult<Vec<String>> {
        Ok(vec![
            "budget".into(),
            command.into(),
            "--registry".into(),
            path_arg(&self.path("registry"))?,
            "--house".into(),
            "origin89".into(),
            "--store".into(),
            path_arg(&self.path("store"))?,
            "--orca".into(),
            path_arg(&self.orca_file("orca"))?,
            "--runtime-dir".into(),
            path_arg(&self.path("runtime"))?,
        ])
    }

    fn run(&self, args: &[String]) -> TestResult<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(args)
            .output()?)
    }
}

fn path_arg(path: &Path) -> TestResult<String> {
    Ok(path.to_str().ok_or("non-UTF-8 path")?.to_owned())
}

fn orca() -> TestResult<BackendId> {
    Ok(BackendId::new("orca-local")?)
}

fn credential() -> TestResult<CredentialId> {
    Ok(CredentialId::new("orca-host-session")?)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_budget_tick_pauses_an_exhausted_schedule_and_records_an_undeliverable_report_once()
-> TestResult {
    let kitchen = Kitchen::new(4, |_| Ok(()))?;
    let precheck = kitchen.budget("precheck", &[])?;
    assert_eq!(precheck.status.code(), Some(0), "{precheck:?}");
    assert_eq!(stdout(&precheck).trim(), "actionable");
    assert!(
        kitchen.calls("automations edit")?.is_empty(),
        "precheck only reads"
    );

    let run = kitchen.budget("run", &[])?;
    // Without a report issue the pause applies but the report is undelivered.
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    let out = stdout(&run);
    assert!(out.contains("paused pickup"), "{out}");
    assert!(
        out.contains("undeliverable: Paused schedule pickup"),
        "{out}"
    );
    assert!(!kitchen.enabled()?, "paused on the fake Orca");
    let edits = kitchen.calls("automations edit")?;
    assert!(
        edits.iter().all(|edit| edit.contains("--disabled")),
        "never activates: {edits:?}"
    );

    // The undeliverable report is recorded for the window, so the schedule
    // starts no agent again until the window ends.
    let again = kitchen.budget("precheck", &[])?;
    assert_eq!(again.status.code(), Some(1), "{again:?}");
    assert_eq!(stdout(&again).trim(), "idle");
    let edits_before = kitchen.calls("automations edit")?.len();
    let next = kitchen.budget("run", &[])?;
    assert_eq!(next.status.code(), Some(0), "{next:?}");
    assert_eq!(stdout(&next).trim(), "idle");
    assert_eq!(kitchen.calls("automations edit")?.len(), edits_before);
    Ok(())
}

#[test]
fn the_budget_tick_is_idle_within_budget() -> TestResult {
    let kitchen = Kitchen::new(3, |_| Ok(()))?;
    let precheck = kitchen.budget("precheck", &[])?;
    assert_eq!(precheck.status.code(), Some(1), "{precheck:?}");
    assert_eq!(stdout(&precheck).trim(), "idle");
    let run = kitchen.budget("run", &[])?;
    assert_eq!(run.status.code(), Some(0), "{run:?}");
    assert_eq!(stdout(&run).trim(), "idle");
    assert!(kitchen.enabled()?);
    assert!(kitchen.calls("automations edit")?.is_empty());
    Ok(())
}

#[test]
fn a_report_issue_outside_the_house_destinations_is_refused_before_any_pause() -> TestResult {
    let kitchen = Kitchen::new(4, |_| Ok(()))?;
    let token = kitchen.path("token");
    fs::write(&token, "sanitized-fixture-token")?;
    let run = kitchen.budget(
        "run",
        &[
            "--report-issue",
            "someone/else#7",
            "--github-backend",
            "github",
            "--requester",
            "kitchen-bot",
            "--github-credential",
            "github-bot",
            "--credential-file",
            &path_arg(&token)?,
            "--gh",
            "/usr/bin/false",
        ],
    )?;
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("integration effect is not permitted"),
        "{run:?}"
    );
    assert!(kitchen.calls("automations edit")?.is_empty());
    assert!(kitchen.enabled()?);
    // The report flags travel together.
    let partial = kitchen.budget("run", &["--report-issue", "origin89hq/firmware#7"])?;
    assert_eq!(partial.status.code(), Some(2), "{partial:?}");
    Ok(())
}

#[test]
fn the_budget_precheck_separates_invalid_input_from_unreadable_schedules() -> TestResult {
    // No schedule policy: nothing to judge budgets against.
    let unconfigured = Kitchen::new(4, |house| {
        house.schedules = None;
        Ok(())
    })?;
    let output = unconfigured.budget("precheck", &[])?;
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    // Orca unavailable: an error, never idle.
    let down = Kitchen::new(4, |_| Ok(()))?;
    fs::write(down.orca_file("down"), "")?;
    let output = down.budget("precheck", &[])?;
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn installing_the_tick_needs_the_schedule_grant() -> TestResult {
    let kitchen = Kitchen::new(0, |house| {
        house.grants.clear();
        Ok(())
    })?;
    let output = kitchen.budget(
        "install",
        &[
            "--kitchen",
            env!("CARGO_BIN_EXE_kitchn"),
            "--cron",
            "15 * * * *",
            "--timezone",
            "America/Toronto",
            "--agent",
            "claude",
        ],
    )?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("manage-schedule"),
        "{output:?}"
    );
    assert!(kitchen.calls("automations create")?.is_empty());
    Ok(())
}

#[test]
fn installing_the_tick_on_orca_is_refused_naming_the_missing_capabilities() -> TestResult {
    // Every grant is in place; Orca lacks what the tick requires.
    let kitchen = Kitchen::new(0, |_| Ok(()))?;
    let output = kitchen.budget(
        "install",
        &[
            "--kitchen",
            env!("CARGO_BIN_EXE_kitchn"),
            "--cron",
            "15 * * * *",
            "--timezone",
            "America/Toronto",
            "--agent",
            "claude",
        ],
    )?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for capability in [
        "schedule.single_consumer",
        "schedule.run_timeout",
        "schedule.precheck",
    ] {
        assert!(stderr.contains(capability), "{capability}: {stderr}");
    }
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(kitchen.calls("automations")?.is_empty());
    Ok(())
}

#[test]
fn installing_the_tick_with_a_report_issue_needs_the_comment_grant() -> TestResult {
    // The schedule grant is there; the house grants no comments.
    let kitchen = Kitchen::new(0, |_| Ok(()))?;
    let token = kitchen.path("token");
    fs::write(&token, "sanitized-fixture-token")?;
    let output = kitchen.budget(
        "install",
        &[
            "--kitchen",
            env!("CARGO_BIN_EXE_kitchn"),
            "--cron",
            "15 * * * *",
            "--timezone",
            "America/Toronto",
            "--agent",
            "claude",
            "--report-issue",
            "origin89hq/firmware#7",
            "--github-backend",
            "github",
            "--requester",
            "kitchen-bot",
            "--github-credential",
            "github-bot",
            "--credential-file",
            &path_arg(&token)?,
            "--gh",
            "/usr/bin/false",
        ],
    )?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("post-comment"),
        "{output:?}"
    );
    assert!(
        kitchen.calls("automations list")?.is_empty(),
        "nothing read"
    );
    assert!(kitchen.calls("automations create")?.is_empty());
    Ok(())
}

#[test]
fn the_budget_tick_builds_the_backend_the_house_is_bound_to() -> TestResult {
    // No --backend or --credential: both come from the house's binding.
    let kitchen = Kitchen::new(4, |_| Ok(()))?;
    let precheck = kitchen.run(&kitchen.source("precheck")?)?;
    assert_eq!(precheck.status.code(), Some(0), "{precheck:?}");
    assert_eq!(stdout(&precheck).trim(), "actionable");
    let run = kitchen.run(&kitchen.source("run")?)?;
    assert!(stdout(&run).contains("paused pickup"), "{run:?}");
    assert!(!kitchen.enabled()?, "paused under the bound grant");
    Ok(())
}

#[test]
fn a_house_without_a_backend_binding_is_refused_naming_it() -> TestResult {
    // A house registered before bindings: nothing defaults to Orca.
    let kitchen = Kitchen::new(4, |house| {
        house.backend = None;
        Ok(())
    })?;
    // A scheduled precheck reports it as unreadable (3), a run as failed (1).
    for (command, code) in [("precheck", 3), ("run", 1)] {
        let output = kitchen.budget(command, &[])?;
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("house origin89 has no worker backend binding"),
            "{stderr}"
        );
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    assert!(kitchen.calls("")?.is_empty(), "Orca never contacted");
    assert!(kitchen.enabled()?);
    Ok(())
}

#[test]
fn a_house_bound_to_an_unknown_backend_is_refused_naming_it() -> TestResult {
    let kitchen = Kitchen::new(4, |house| {
        if let Some(binding) = &mut house.backend {
            binding.kind = "sandbox".parse()?;
        }
        Ok(())
    })?;
    let output = kitchen.budget("run", &[])?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bound to worker backend `sandbox`, which this Kitchen does not support"),
        "{stderr}"
    );
    assert!(kitchen.calls("")?.is_empty(), "Orca never contacted");
    Ok(())
}

#[test]
fn backend_flags_that_disagree_with_the_binding_are_refused() -> TestResult {
    let kitchen = Kitchen::new(4, |_| Ok(()))?;
    for (flag, value) in [
        ("--backend", "orca-other"),
        ("--credential", "someone-else"),
    ] {
        let mut args = kitchen.source("run")?;
        args.extend([flag.to_owned(), value.to_owned()]);
        let output = kitchen.run(&args)?;
        assert_eq!(output.status.code(), Some(2), "{flag}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("differs from its worker backend binding"),
            "{output:?}"
        );
    }
    assert!(kitchen.calls("")?.is_empty(), "Orca never contacted");
    assert!(kitchen.enabled()?);
    Ok(())
}
