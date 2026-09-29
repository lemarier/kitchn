//! The gardener precheck process contract. GitHub is a fake `gh` script; no
//! live forge is contacted.
#![cfg(unix)]

use std::{
    error::Error,
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[path = "../../kitchen/tests/common/executable.rs"]
mod executable;

use kitchen::{
    CredentialId, HolderId, HouseId,
    contracts::{Claimant, ExternalRef, IssueNumber, Repository, Text, Timestamp},
    scheduling::PrecheckOutcome,
    state::{HouseStore, StoreOptions},
    workflows::gardener,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn issue(number: u64, state: &str, updated: &str, labels: &[&str]) -> String {
    let labels: Vec<_> = labels
        .iter()
        .map(|name| serde_json::json!({"name": name, "color": "000000", "description": null}))
        .collect();
    serde_json::json!({"repository_url":"https://api.github.com/repos/sample/project","id":number,"number":number,"title":"issue","state":state,"assignees":[],"labels":labels,"updated_at":updated,"closed_at":null}).to_string()
}

/// A fake `gh` that authenticates as `sample-bot` and answers the changed
/// (`since=`) and open inventory reads. `$6` is the API endpoint.
fn fake_gh(root: &Path, changed: &str, open: &str) -> TestResult<PathBuf> {
    let script = format!(
        "#!/bin/sh\nif [ \"$4\" = user ]; then printf '%s' '{{\"login\":\"sample-bot\"}}'; exit 0; fi\ncase \"$6\" in\n  *since=*) printf '%s' '{changed}' ;;\n  *state=open*) printf '%s' '{open}' ;;\n  *) exit 1 ;;\nesac\n"
    );
    let path = root.join("gh");
    executable::write_executable(&path, script)?;
    Ok(path)
}

/// The house store under `root`, initialized on first use.
fn house_store(root: &Path) -> TestResult<HouseStore> {
    let dir = root.join("house");
    let house = HouseId::new("sample")?;
    Ok(if dir.exists() {
        HouseStore::open(dir, house, StoreOptions::default())?
    } else {
        HouseStore::initialize(dir, house, StoreOptions::default())?
    })
}

/// The argument vector the schedule records, built by the library.
fn scheduled_argv(root: &Path, gh: PathBuf) -> TestResult<Vec<String>> {
    let token = root.join("token");
    std::fs::write(&token, "sanitized-fixture-token")?;
    house_store(root)?;
    let args = gardener::PrecheckArgs {
        kitchen: env!("CARGO_BIN_EXE_kitchen").into(),
        house: HouseId::new("sample")?,
        repository: Repository::new("sample/project")?,
        requester: ExternalRef::new("sample-bot")?,
        credential: CredentialId::new("read")?,
        credential_file: token,
        gh,
        store: root.join("house"),
        labels: gardener::AgentLabels {
            ready: "agent-ready".into(),
            working: "agent-working".into(),
        },
        window: gardener::PrecheckWindow::new(48, 30)?,
    };
    Ok(args
        .argv()?
        .iter()
        .map(Text::as_str)
        .map(str::to_owned)
        .collect())
}

fn run(argv: &[String]) -> TestResult<Output> {
    let (program, args) = argv.split_first().ok_or("empty argv")?;
    Ok(Command::new(program).args(args).output()?)
}

fn outcome(output: &Output) -> PrecheckOutcome {
    PrecheckOutcome::from_exit_code(output.status.code())
}

#[test]
fn scheduled_precheck_reports_idle_and_actionable_by_exit_status() -> TestResult {
    let root = tempfile::tempdir()?;
    let fresh = issue(1, "open", "2999-01-01T00:00:00Z", &[]);

    let gh = fake_gh(root.path(), "[]", &format!("[{fresh}]"))?;
    let idle = run(&scheduled_argv(root.path(), gh)?)?;
    assert_eq!(outcome(&idle), PrecheckOutcome::Idle);
    assert_eq!(String::from_utf8(idle.stdout)?, "idle\n");

    let residue = issue(2, "closed", "2999-01-01T00:00:00Z", &["agent-working"]);
    let gh = fake_gh(root.path(), &format!("[{residue}]"), &format!("[{fresh}]"))?;
    let changed = run(&scheduled_argv(root.path(), gh)?)?;
    assert_eq!(outcome(&changed), PrecheckOutcome::Actionable);
    assert_eq!(String::from_utf8(changed.stdout)?, "actionable\n");

    let stale = issue(3, "open", "2000-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{stale}]"))?;
    let old = run(&scheduled_argv(root.path(), gh)?)?;
    assert_eq!(outcome(&old), PrecheckOutcome::Actionable);
    Ok(())
}

#[test]
fn a_handled_stale_issue_keeps_later_daily_runs_idle() -> TestResult {
    let root = tempfile::tempdir()?;
    let stale = issue(3, "open", "2000-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{stale}]"))?;
    let argv = scheduled_argv(root.path(), gh)?;
    assert_eq!(outcome(&run(&argv)?), PrecheckOutcome::Actionable);

    // The pass reports the stale issue and records it as handled at its
    // current revision; the following days have nothing to do.
    let store = house_store(root.path())?;
    let markers = gardener::StaleMarkers::new(&store)?;
    let project = Repository::new("sample/project")?;
    let updated = Timestamp::from_unix_millis(946_684_800_000); // 2000-01-01
    markers.record(
        &project,
        IssueNumber::new(3)?,
        updated,
        &Claimant::scheduled(HolderId::new("gardener-tick")?),
        Timestamp::from_unix_millis(946_684_900_000),
    )?;
    for day in 0..3 {
        let output = run(&argv)?;
        assert_eq!(outcome(&output), PrecheckOutcome::Idle, "day {day}");
        assert_eq!(String::from_utf8(output.stdout)?, "idle\n");
    }

    // Another stale issue that was never handled wakes the schedule.
    let other = issue(5, "open", "2000-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{stale},{other}]"))?;
    assert_eq!(
        outcome(&run(&scheduled_argv(root.path(), gh)?)?),
        PrecheckOutcome::Actionable
    );
    // So does the handled issue once it changes, even while still stale.
    let touched = issue(3, "open", "2000-02-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{touched}]"))?;
    assert_eq!(
        outcome(&run(&scheduled_argv(root.path(), gh)?)?),
        PrecheckOutcome::Actionable
    );
    Ok(())
}

/// The argument vector a gardener schedule installed before `--store`
/// existed recorded, verbatim.
fn pre_store_argv(root: &Path, gh: &Path) -> TestResult<Vec<String>> {
    let token = root.join("token");
    std::fs::write(&token, "sanitized-fixture-token")?;
    let path = |path: &Path| path.to_str().map(str::to_owned).ok_or("non-UTF-8 path");
    Ok(vec![
        env!("CARGO_BIN_EXE_kitchen").into(),
        "gardener".into(),
        "precheck".into(),
        "--house".into(),
        "sample".into(),
        "--repository".into(),
        "sample/project".into(),
        "--requester".into(),
        "sample-bot".into(),
        "--credential".into(),
        "read".into(),
        "--credential-file".into(),
        path(&token)?,
        "--gh".into(),
        path(gh)?,
        "--ready-label".into(),
        "agent-ready".into(),
        "--working-label".into(),
        "agent-working".into(),
        "--lookback-hours".into(),
        "48".into(),
        "--stale-days".into(),
        "30".into(),
    ])
}

#[test]
fn a_schedule_installed_without_a_store_keeps_its_behavior() -> TestResult {
    let root = tempfile::tempdir()?;
    let fresh = issue(1, "open", "2999-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{fresh}]"))?;
    let idle = run(&pre_store_argv(root.path(), &gh)?)?;
    assert_eq!(outcome(&idle), PrecheckOutcome::Idle);
    assert_eq!(String::from_utf8(idle.stdout)?, "idle\n");

    let residue = issue(2, "closed", "2999-01-01T00:00:00Z", &["agent-working"]);
    let gh = fake_gh(root.path(), &format!("[{residue}]"), &format!("[{fresh}]"))?;
    assert_eq!(
        outcome(&run(&pre_store_argv(root.path(), &gh)?)?),
        PrecheckOutcome::Actionable
    );

    // Without a store nothing is known to be handled, so every stale issue
    // counts, as it did before handled markers existed.
    let stale = issue(3, "open", "2000-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{stale}]"))?;
    for _ in 0..2 {
        assert_eq!(
            outcome(&run(&pre_store_argv(root.path(), &gh)?)?),
            PrecheckOutcome::Actionable
        );
    }
    // The precheck only reads: it creates no store.
    assert!(!root.path().join("house").exists());
    Ok(())
}

#[test]
fn the_gardener_report_does_not_wake_the_next_run() -> TestResult {
    let root = tempfile::tempdir()?;
    let stale = issue(3, "open", "2000-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{stale}]"))?;
    assert_eq!(
        outcome(&run(&scheduled_argv(root.path(), gh)?)?),
        PrecheckOutcome::Actionable
    );

    // The pass comments on the issue, which moves its last update into the
    // next run's change window, then records the revision it read back.
    let reported = issue(3, "open", "2999-01-01T00:00:00Z", &[]);
    let store = house_store(root.path())?;
    gardener::StaleMarkers::new(&store)?.record(
        &Repository::new("sample/project")?,
        IssueNumber::new(3)?,
        Timestamp::from_unix_millis(32_472_144_000_000), // 2999-01-01
        &Claimant::scheduled(HolderId::new("gardener-tick")?),
        Timestamp::from_unix_millis(946_684_900_000),
    )?;
    let gh = fake_gh(
        root.path(),
        &format!("[{reported}]"),
        &format!("[{reported}]"),
    )?;
    let next = run(&scheduled_argv(root.path(), gh)?)?;
    assert_eq!(outcome(&next), PrecheckOutcome::Idle);
    assert_eq!(String::from_utf8(next.stdout)?, "idle\n");

    // Someone else comments afterwards: that is a change to look at.
    let answered = issue(3, "open", "2999-01-02T00:00:00Z", &[]);
    let gh = fake_gh(
        root.path(),
        &format!("[{answered}]"),
        &format!("[{answered}]"),
    )?;
    assert_eq!(
        outcome(&run(&scheduled_argv(root.path(), gh)?)?),
        PrecheckOutcome::Actionable
    );
    Ok(())
}

#[test]
fn precheck_failures_never_exit_as_idle() -> TestResult {
    let root = tempfile::tempdir()?;
    // The forge read fails: an execution error, not a quiet day.
    let gh = fake_gh(root.path(), "not json", "[]")?;
    let failed = run(&scheduled_argv(root.path(), gh.clone())?)?;
    assert_eq!(outcome(&failed), PrecheckOutcome::Error);
    assert_eq!(failed.status.code(), Some(3));
    assert!(failed.stdout.is_empty());
    assert!(!String::from_utf8(failed.stderr)?.contains("sanitized-fixture-token"));

    // An unknown lifecycle is incomplete evidence.
    let locked = issue(4, "locked", "2999-01-01T00:00:00Z", &[]);
    let gh_locked = fake_gh(root.path(), &format!("[{locked}]"), "[]")?;
    let unknown = run(&scheduled_argv(root.path(), gh_locked)?)?;
    assert_eq!(outcome(&unknown), PrecheckOutcome::Error);

    // Invalid arguments exit 2, before any forge read.
    let mut argv = scheduled_argv(root.path(), gh)?;
    let lookback = argv
        .iter()
        .position(|arg| arg == "--lookback-hours")
        .ok_or("missing lookback")?;
    *argv.get_mut(lookback + 1).ok_or("missing value")? = "0".into();
    let invalid = run(&argv)?;
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(outcome(&invalid), PrecheckOutcome::Error);

    let mut argv = scheduled_argv(root.path(), root.path().join("gh"))?;
    let credential = argv
        .iter()
        .position(|arg| arg == "--credential-file")
        .ok_or("missing credential")?;
    *argv.get_mut(credential + 1).ok_or("missing value")? = "token".into();
    let relative = run(&argv)?;
    assert_eq!(relative.status.code(), Some(2));

    // A relative store path is invalid input; a missing store is a read
    // failure, never an idle day.
    let fresh = issue(1, "open", "2999-01-01T00:00:00Z", &[]);
    let gh = fake_gh(root.path(), "[]", &format!("[{fresh}]"))?;
    let mut argv = scheduled_argv(root.path(), gh)?;
    let store = argv
        .iter()
        .position(|arg| arg == "--store")
        .ok_or("missing store")?;
    *argv.get_mut(store + 1).ok_or("missing value")? = "house".into();
    assert_eq!(run(&argv)?.status.code(), Some(2));
    *argv.get_mut(store + 1).ok_or("missing value")? =
        root.path().join("absent").display().to_string();
    let missing = run(&argv)?;
    assert_eq!(missing.status.code(), Some(3));
    assert!(missing.stdout.is_empty());
    Ok(())
}
