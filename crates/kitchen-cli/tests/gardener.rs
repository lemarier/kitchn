//! The gardener precheck and report process contracts. GitHub is a fake `gh` script; no
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
    BackendId, CredentialId, HolderId, HouseId,
    adoption::HouseRegistry,
    contracts::{
        Claimant, ExternalRef, Grant, IssueNumber, Permission, Repository, Text, Timestamp,
    },
    house::HouseConfig,
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
        kitchen: env!("CARGO_BIN_EXE_kitchn").into(),
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
        env!("CARGO_BIN_EXE_kitchn").into(),
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

/// A fake `gh` for `gardener report-stale` that keeps GitHub's side in files
/// under `root`: `updated` is issue 3's last update and `state` its state,
/// `posted` holds the report comment once a POST applied, and `mode` makes a
/// POST `deny` (403, nothing applied) or `lose` its response (applied, but
/// `gh` fails). `reread` set to `fail` fails issue reads once a report is
/// posted. `posts` counts POSTs, and `comments` holds other comments on the
/// issue. Nothing here is a live forge.
fn fake_forge(root: &Path) -> TestResult<PathBuf> {
    let dir = root.join("forge");
    std::fs::create_dir_all(&dir)?;
    for (name, value) in [
        ("updated", "2000-01-01T00:00:00Z"),
        ("state", "open"),
        ("mode", "ok"),
        ("reread", "ok"),
        ("posts", ""),
        ("comments", ""),
    ] {
        std::fs::write(dir.join(name), value)?;
    }
    let d = dir.display();
    let script = format!(
        r#"#!/bin/sh
for arg in "$@"; do [ "$arg" != sanitized-fixture-token ] || exit 9; done
if [ "$4" = user ]; then printf '%s' '{{"login":"sample-bot"}}'; exit 0; fi
if [ "$6" = POST ]; then
  echo post >> '{d}/posts'
  mode=$(/bin/cat '{d}/mode')
  if [ "$mode" = deny ]; then printf 'HTTP/2 403 Forbidden\r\n\r\n{{}}'; exit 1; fi
  /bin/cat | /usr/bin/sed 's/^{{//; s/}}$//' > '{d}/posted'
  echo 2999-01-01T00:00:00Z > '{d}/updated'
  if [ "$mode" = lose ]; then exit 1; fi
  printf 'HTTP/2 201 Created\r\n\r\n{{}}'; exit 0
fi
case "$6" in
  *issues/3/comments*)
    printf '['
    sep=''
    if [ -s '{d}/comments' ]; then /bin/cat '{d}/comments'; sep=','; fi
    if [ -s '{d}/posted' ]; then
      printf '%s{{"id":77,"user":{{"login":"sample-bot"}},"html_url":"https://github.com/sample/project/issues/3#issuecomment-77",%s}}' "$sep" "$(/bin/cat '{d}/posted')"
    fi
    printf ']' ;;
  *issues/3*)
    if [ -s '{d}/posted' ] && [ "$(/bin/cat '{d}/reread')" = fail ]; then exit 1; fi
    printf '{{"repository_url":"https://api.github.com/repos/sample/project","id":3,"number":3,"title":"issue","state":"%s","assignees":[],"labels":[],"updated_at":"%s","closed_at":null}}' "$(/bin/cat '{d}/state')" "$(/bin/cat '{d}/updated')" ;;
  *) exit 1 ;;
esac
"#
    );
    let path = dir.join("gh");
    executable::write_executable(&path, script)?;
    Ok(path)
}

fn forge_set(root: &Path, name: &str, value: &str) -> TestResult {
    Ok(std::fs::write(root.join("forge").join(name), value)?)
}

fn posts(root: &Path) -> TestResult<usize> {
    Ok(std::fs::read_to_string(root.join("forge").join("posts"))?
        .lines()
        .count())
}

/// A house registry for `sample` granting comments on `sample/project` when
/// `grant` is set.
fn registry(root: &Path, grant: bool) -> TestResult<PathBuf> {
    registry_at(root, grant, None)
}

/// As [`registry`], in its own directory when `guidance` names another house
/// guidance revision.
fn registry_at(root: &Path, grant: bool, guidance: Option<&str>) -> TestResult<PathBuf> {
    let mut house: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/origin89.json"
    ))?;
    if let Some(guidance) = guidance {
        house.guidance = guidance.parse()?;
    }
    house.house = HouseId::new("sample")?;
    house.repositories = [Repository::new("sample/project")?].into();
    house.posting_destinations = house.repositories.clone();
    if grant {
        let grant = Grant::repository(
            Permission::PostComment,
            Repository::new("sample/project")?,
            BackendId::new("github")?,
            CredentialId::new("read")?,
        );
        house.policy_limits.insert(grant.clone());
        house.grants.insert(grant);
    }
    // The registry refuses a path through a symlink, such as macOS's /var.
    let name = match (grant, guidance) {
        (true, None) => "registry".to_owned(),
        (false, None) => "registry-ungranted".to_owned(),
        (_, Some(guidance)) => format!("registry-{guidance}"),
    };
    let path = root.canonicalize()?.join(name);
    if !path.exists() {
        HouseRegistry::new(&path)?.initialize(&house)?;
    }
    Ok(path)
}

fn report_argv(root: &Path, registry: &Path, repository: &str) -> TestResult<Vec<String>> {
    let token = root.join("token");
    std::fs::write(&token, "sanitized-fixture-token")?;
    house_store(root)?;
    let path = |path: &Path| path.to_str().map(str::to_owned).ok_or("non-UTF-8 path");
    Ok(vec![
        env!("CARGO_BIN_EXE_kitchn").into(),
        "gardener".into(),
        "report-stale".into(),
        "--registry".into(),
        path(registry)?,
        "--house".into(),
        "sample".into(),
        "--store".into(),
        path(&root.join("house"))?,
        "--repository".into(),
        repository.into(),
        "--issue".into(),
        "3".into(),
        "--body".into(),
        "No activity for 30 days; is this still needed?".into(),
        "--github-backend".into(),
        "github".into(),
        "--requester".into(),
        "sample-bot".into(),
        "--credential".into(),
        "read".into(),
        "--credential-file".into(),
        path(&token)?,
        "--gh".into(),
        path(&root.join("forge").join("gh"))?,
    ])
}

fn report(root: &Path) -> TestResult<Output> {
    run(&report_argv(
        root,
        &registry(root, true)?,
        "sample/project",
    )?)
}

/// The precheck's answer for stale issue 3 as `updated` after the report.
fn precheck_after(root: &Path, updated: &str) -> TestResult<PrecheckOutcome> {
    let current = issue(3, "open", updated, &[]);
    let gh = fake_gh(root, &format!("[{current}]"), &format!("[{current}]"))?;
    Ok(outcome(&run(&scheduled_argv(root, gh)?)?))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_posted_report_keeps_the_next_runs_idle_and_is_never_posted_twice() -> TestResult {
    let root = tempfile::tempdir()?;
    fake_forge(root.path())?;
    assert_eq!(
        precheck_after(root.path(), "2000-01-01T00:00:00Z")?,
        PrecheckOutcome::Actionable
    );

    let reported = report(root.path())?;
    assert_eq!(
        reported.status.code(),
        Some(0),
        "{}",
        text(&reported.stderr)
    );
    assert_eq!(
        text(&reported.stdout),
        "recorded https://github.com/sample/project/issues/3#issuecomment-77\n"
    );
    assert_eq!(posts(root.path())?, 1);
    assert_eq!(
        precheck_after(root.path(), "2999-01-01T00:00:00Z")?,
        PrecheckOutcome::Idle
    );

    // The same run again, as after a restart: nothing is posted.
    let again = report(root.path())?;
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout), "already handled\n");
    assert_eq!(posts(root.path())?, 1);

    // Someone else answers afterwards: that wakes the schedule again.
    assert_eq!(
        precheck_after(root.path(), "2999-01-02T00:00:00Z")?,
        PrecheckOutcome::Actionable
    );
    Ok(())
}

#[test]
fn a_failed_report_records_nothing_even_beside_a_requester_comment() -> TestResult {
    let root = tempfile::tempdir()?;
    fake_forge(root.path())?;
    // The house identity already commented on the issue, unrelated to any
    // report; GitHub refuses the report itself.
    forge_set(
        root.path(),
        "comments",
        r#"{"id":88,"user":{"login":"sample-bot"},"html_url":"https://github.com/sample/project/issues/3#issuecomment-88","body":"unrelated"}"#,
    )?;
    forge_set(root.path(), "mode", "deny")?;
    let refused = report(root.path())?;
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused.stderr));
    assert_eq!(
        text(&refused.stdout),
        "not posted; nothing recorded, run again later\n"
    );
    assert_eq!(
        precheck_after(root.path(), "2000-01-01T00:00:00Z")?,
        PrecheckOutcome::Actionable
    );

    // A later run posts the report and records it, even after the house
    // guidance moved to another revision in between.
    forge_set(root.path(), "mode", "ok")?;
    let resynced = registry_at(
        root.path(),
        true,
        Some("cccccccccccccccccccccccccccccccccccccccc"),
    )?;
    let reported = run(&report_argv(root.path(), &resynced, "sample/project")?)?;
    assert_eq!(
        reported.status.code(),
        Some(0),
        "{}",
        text(&reported.stderr)
    );
    assert_eq!(posts(root.path())?, 2);
    assert_eq!(
        precheck_after(root.path(), "2999-01-01T00:00:00Z")?,
        PrecheckOutcome::Idle
    );
    Ok(())
}

#[test]
fn a_lost_post_response_is_looked_up_on_restart_not_posted_again() -> TestResult {
    let root = tempfile::tempdir()?;
    fake_forge(root.path())?;
    // The comment lands but `gh` fails: the outcome is uncertain.
    forge_set(root.path(), "mode", "lose")?;
    let uncertain = report(root.path())?;
    assert_eq!(
        uncertain.status.code(),
        Some(1),
        "{}",
        text(&uncertain.stderr)
    );
    assert_eq!(
        precheck_after(root.path(), "2999-01-01T00:00:00Z")?,
        PrecheckOutcome::Actionable
    );

    forge_set(root.path(), "mode", "ok")?;
    let recovered = report(root.path())?;
    assert_eq!(
        recovered.status.code(),
        Some(0),
        "{}",
        text(&recovered.stderr)
    );
    assert_eq!(posts(root.path())?, 1);
    assert_eq!(
        precheck_after(root.path(), "2999-01-01T00:00:00Z")?,
        PrecheckOutcome::Idle
    );
    Ok(())
}

#[test]
fn a_run_interrupted_after_the_post_records_it_on_restart() -> TestResult {
    let root = tempfile::tempdir()?;
    fake_forge(root.path())?;
    // The post applies, then the issue cannot be read again: no marker.
    forge_set(root.path(), "reread", "fail")?;
    let interrupted = report(root.path())?;
    assert_eq!(interrupted.status.code(), Some(3));
    assert_eq!(posts(root.path())?, 1);
    assert_eq!(
        precheck_after(root.path(), "2999-01-01T00:00:00Z")?,
        PrecheckOutcome::Actionable
    );

    forge_set(root.path(), "reread", "ok")?;
    let recovered = report(root.path())?;
    assert_eq!(
        recovered.status.code(),
        Some(0),
        "{}",
        text(&recovered.stderr)
    );
    assert_eq!(posts(root.path())?, 1);
    assert_eq!(
        precheck_after(root.path(), "2999-01-01T00:00:00Z")?,
        PrecheckOutcome::Idle
    );
    Ok(())
}

#[test]
fn report_stale_refuses_without_posting() -> TestResult {
    let root = tempfile::tempdir()?;
    fake_forge(root.path())?;

    // A closed issue needs no stale report.
    forge_set(root.path(), "state", "closed")?;
    let closed = report(root.path())?;
    assert_eq!(closed.status.code(), Some(3));
    assert_eq!(text(&closed.stderr), "error: decision scope mismatch\n");
    forge_set(root.path(), "state", "open")?;

    // A repository outside the house's posting destinations.
    let outside = run(&report_argv(
        root.path(),
        &registry(root.path(), true)?,
        "sample/other",
    )?)?;
    assert_eq!(outside.status.code(), Some(3));
    assert!(
        text(&outside.stderr).contains("not permitted"),
        "{}",
        text(&outside.stderr)
    );

    // A house without the comment grant.
    let ungranted = run(&report_argv(
        root.path(),
        &registry(root.path(), false)?,
        "sample/project",
    )?)?;
    assert_eq!(ungranted.status.code(), Some(3));
    assert!(
        text(&ungranted.stderr).contains("post-comment"),
        "{}",
        text(&ungranted.stderr)
    );
    assert!(!text(&ungranted.stderr).contains("sanitized-fixture-token"));

    assert_eq!(posts(root.path())?, 0);
    assert_eq!(
        precheck_after(root.path(), "2000-01-01T00:00:00Z")?,
        PrecheckOutcome::Actionable
    );
    Ok(())
}

#[test]
fn report_stale_rejects_invalid_arguments_before_reading() -> TestResult {
    let root = tempfile::tempdir()?;
    fake_forge(root.path())?;
    let registry = registry(root.path(), true)?;
    for (flag, value) in [("--store", "house"), ("--issue", "0"), ("--body", "")] {
        let mut argv = report_argv(root.path(), &registry, "sample/project")?;
        let index = argv
            .iter()
            .position(|arg| arg == flag)
            .ok_or("missing flag")?;
        *argv.get_mut(index + 1).ok_or("missing value")? = value.into();
        assert_eq!(run(&argv)?.status.code(), Some(2), "{flag}");
    }
    assert_eq!(posts(root.path())?, 0);
    Ok(())
}
