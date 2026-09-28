//! The gardener precheck process contract. GitHub is a fake `gh` script; no
//! live forge is contacted.
#![cfg(unix)]

use std::{
    error::Error,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use kitchen::{
    CredentialId, HouseId,
    contracts::{ExternalRef, Repository, Text},
    scheduling::PrecheckOutcome,
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
    std::fs::write(&path, script)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// The argument vector the schedule records, built by the library.
fn scheduled_argv(root: &Path, gh: PathBuf) -> TestResult<Vec<String>> {
    let token = root.join("token");
    std::fs::write(&token, "sanitized-fixture-token")?;
    let args = gardener::PrecheckArgs {
        kitchen: env!("CARGO_BIN_EXE_kitchen").into(),
        house: HouseId::new("sample")?,
        repository: Repository::new("sample/project")?,
        requester: ExternalRef::new("sample-bot")?,
        credential: CredentialId::new("read")?,
        credential_file: token,
        gh,
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
    Ok(())
}
