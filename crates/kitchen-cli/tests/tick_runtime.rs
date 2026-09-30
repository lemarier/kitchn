//! Backend host facts for scheduled ticks: `tick configure` stores them in
//! the house's private runtime configuration, and the trigger it prints
//! stays `kitchn tick --registry --house` and stores nothing. Simulated: `orca` and `gh` are
//! fake scripts, so no live Orca, account, or trigger is used and none of
//! this is live runtime evidence.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use kitchen::contracts::CommitId;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";
const TOKEN: &str = "fixture-token-4c1d";

/// Answers the status probe, the account listing, and an empty mailbox, and
/// logs every call.
const FAKE_ORCA: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
case "$1 $2" in
  "status --json")
    printf '{"id":"fake","ok":true,"result":{"runtime":{"state":"ready","reachable":true,"appVersion":"1.4.212","capabilities":["orchestration.contract.v1","orchestration.worker-stop-verdict.v1"]}}}' ;;
  "account list")
    printf '{"id":"fake","ok":true,"result":{"claude":{"accounts":[]},"codex":{"accounts":[]}}}' ;;
  "orchestration check")
    printf '{"id":"fake","ok":true,"result":{"messages":[]}}' ;;
  *) exit 1 ;;
esac
"#;

/// Answers its login and an empty list for everything else.
const FAKE_GH: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/gh-calls"
case "$*" in
  "config get user"*) echo octo-cat ;;
  "api --hostname github.com user") echo '{"login":"octo-cat"}' ;;
  *) echo '[]' ;;
esac
"#;

/// Like [`FAKE_GH`], but the open issue list holds issue 7, labeled
/// `agent-ready`; a pickup that finds it inspects it, which the log shows.
const FAKE_GH_WITH_ISSUE: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/gh-calls"
case "$*" in
  "config get user"*) echo octo-cat ;;
  "api --hostname github.com user") echo '{"login":"octo-cat"}' ;;
  *"repos/acme/app/issues?state=open"*) echo '[{"repository_url":"https://api.github.com/repos/acme/app","id":1007,"number":7,"title":"Do it","state":"open","assignees":[],"labels":[{"name":"agent-ready","color":"0e8a16"}],"updated_at":"2026-01-01T00:00:00Z"}]' ;;
  *) echo '[]' ;;
esac
"#;

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn script(path: &Path, body: &str) -> TestResult {
    fs::write(path, body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// A house `acme` serving `acme/app` whose tick runs pickup and coordinate,
/// with a fake `orca` and `gh`.
struct House {
    _temp: tempfile::TempDir,
    checkout: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    path: String,
}

impl House {
    fn new() -> TestResult<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (checkout, home, bin) = (root.join("app"), root.join("home"), root.join("bin"));
        for directory in [&checkout, &home, &bin, &root.join("runtime")] {
            fs::create_dir_all(directory)?;
        }
        for args in [
            &["init", "--quiet"][..],
            &["remote", "add", "origin", "git@github.com:acme/app.git"],
        ] {
            let status = Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(&checkout)
                .args(args)
                .status()?;
            assert!(status.success());
        }
        script(&bin.join("gh"), FAKE_GH)?;
        script(&bin.join("orca"), FAKE_ORCA)?;
        let bundle = kitchen::house::default_guidance(&"acme".parse()?, &CommitId::new(KITCHEN)?)?;
        let bundle_path = root.join("acme-bundle.json");
        fs::write(&bundle_path, serde_json::to_vec(&bundle)?)?;
        let house = Self {
            path: format!("{}:/usr/bin:/bin", bin.display()),
            _temp: temp,
            checkout,
            home,
            bin,
        };
        let init = house.kitchen(&[
            "house",
            "init",
            "--house",
            "acme",
            "--required-checks",
            "none",
            "--bundle",
            &bundle_path.display().to_string(),
            "--yes",
        ])?;
        assert_eq!(init.status.code(), Some(0), "{}", text(&init.stderr));
        let token = house.registry().join("private/acme/credentials/github");
        if let Some(directory) = token.parent() {
            fs::create_dir_all(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        fs::write(&token, TOKEN)?;
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
        let config = house.registry().join("houses/acme.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&config)?)?;
        value["tick"] = serde_json::json!({"passes": {
            "pickup": {"everyMinutes": 5},
            "coordinate": {"everyMinutes": 5},
        }});
        fs::write(&config, serde_json::to_vec_pretty(&value)?)?;
        Ok(house)
    }

    fn registry(&self) -> PathBuf {
        self.home.join(".kitchn")
    }

    fn runtime_file(&self) -> PathBuf {
        self.registry().join("private/acme/runtime.json")
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.checkout)
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .stdin(Stdio::null());
        command
    }

    fn kitchen(&self, args: &[&str]) -> TestResult<Output> {
        Ok(self
            .command(env!("CARGO_BIN_EXE_kitchn"))
            .args(args)
            .output()?)
    }

    /// `kitchn tick configure` for `acme`, with `extra` flags.
    fn configure(&self, extra: &[&str]) -> TestResult<Output> {
        let registry = self.registry().display().to_string();
        let mut args = vec![
            "tick",
            "configure",
            "--registry",
            &registry,
            "--house",
            "acme",
        ];
        args.extend_from_slice(extra);
        self.kitchen(&args)
    }

    /// `kitchn tick trigger cron` for `acme`, with `extra` flags.
    fn trigger(&self, extra: &[&str]) -> TestResult<Output> {
        let registry = self.registry().display().to_string();
        let mut args = vec![
            "tick",
            "trigger",
            "cron",
            "--kitchn",
            env!("CARGO_BIN_EXE_kitchn"),
            "--registry",
            &registry,
            "--house",
            "acme",
        ];
        args.extend_from_slice(extra);
        self.kitchen(&args)
    }

    /// The flags that describe this house's fake Orca.
    fn orca_flags(&self) -> Vec<String> {
        let runtime = self.bin.parent().map(|root| root.join("runtime"));
        vec![
            "--orca".into(),
            self.bin.join("orca").display().to_string(),
            "--runtime-dir".into(),
            runtime
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
            "--orca-run".into(),
            "run-1".into(),
            "--orca-coordinator".into(),
            "term-1".into(),
            "--orca-repo".into(),
            "id:app".into(),
        ]
    }

    /// Run the printed cron line's command the way cron would: the fields
    /// after the schedule, unquoted, with this house's environment.
    fn fire(&self, printed: &str) -> TestResult<Output> {
        let words = shell_words(printed.trim().splitn(6, ' ').nth(5).unwrap_or_default())?;
        let (program, args) = words.split_first().ok_or("empty trigger")?;
        Ok(self.command(program).args(args).output()?)
    }

    fn orca_calls(&self) -> String {
        fs::read_to_string(self.bin.join("calls")).unwrap_or_default()
    }
}

/// Split a line of single-quoted words, as the printed cron line is.
fn shell_words(line: &str) -> TestResult<Vec<String>> {
    let mut words = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        let inner = rest.strip_prefix('\'').ok_or("word is not single-quoted")?;
        let (word, tail) = inner.split_once('\'').ok_or("unterminated quote")?;
        words.push(word.to_owned());
        rest = tail.trim_start();
    }
    Ok(words)
}

#[test]
fn a_printed_trigger_runs_pickup_and_coordinate_from_the_stored_runtime() -> TestResult {
    let house = House::new()?;
    let flags = house.orca_flags();
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let configured = house.configure(&flags)?;
    assert_eq!(
        configured.status.code(),
        Some(0),
        "{}",
        text(&configured.stderr)
    );
    assert_eq!(
        text(&configured.stdout).trim(),
        "stored the runtime configuration of house acme"
    );
    let trigger = house.trigger(&[])?;
    assert_eq!(trigger.status.code(), Some(0), "{}", text(&trigger.stderr));
    let printed = text(&trigger.stdout);
    // The line carries no backend flags and no credential.
    assert!(!printed.contains("--orca"), "{printed}");
    assert!(!printed.contains("runtime"), "{printed}");
    assert!(!printed.contains(TOKEN), "{printed}");
    assert!(printed.contains("'--house' 'acme'"), "{printed}");

    let fired = house.fire(&printed)?;
    let stdout = text(&fired.stdout);
    assert_eq!(
        fired.status.code(),
        Some(0),
        "{stdout}{}\nORCA {}\nGH {}",
        text(&fired.stderr),
        house.orca_calls(),
        fs::read_to_string(house.bin.join("gh-calls")).unwrap_or_default()
    );
    assert!(stdout.contains("pickup: run"), "{stdout}");
    assert!(stdout.contains("coordinate: run"), "{stdout}");
    assert!(!stdout.contains("failed"), "{stdout}");
    // The backend that ran was the stored one: its Run and coordinator.
    assert!(house.orca_calls().contains("--terminal=term-1 --run=run-1"));
    assert_eq!(stdout.matches(": idle").count(), 2, "{stdout}");

    let stored = fs::read_to_string(house.runtime_file())?;
    assert!(!stored.contains(TOKEN), "{stored}");
    assert_eq!(
        fs::metadata(house.runtime_file())?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn a_tick_without_stored_runtime_names_what_is_missing() -> TestResult {
    let house = House::new()?;
    let before = house.orca_calls();
    let trigger = house.trigger(&[])?;
    assert_eq!(trigger.status.code(), Some(0), "{}", text(&trigger.stderr));
    assert!(!house.runtime_file().exists());
    let fired = house.fire(&text(&trigger.stdout))?;
    let stdout = text(&fired.stdout);
    assert_eq!(fired.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("--orca, --runtime-dir"), "{stdout}");
    assert!(stdout.contains("kitchn tick configure"), "{stdout}");
    assert_eq!(house.orca_calls(), before);
    Ok(())
}

/// A fresh house with its runtime stored, and the cron line that ticks it.
fn stored_house() -> TestResult<(House, String)> {
    let house = House::new()?;
    let flags = house.orca_flags();
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    assert_eq!(house.configure(&flags)?.status.code(), Some(0));
    let printed = text(&house.trigger(&[])?.stdout);
    Ok((house, printed))
}

#[test]
fn invalid_runtime_config_is_refused_before_any_backend_call() -> TestResult {
    // A pass that fails is not due again for its interval, so each damage
    // gets its own house.
    type Damage = fn(&House, &str) -> String;
    let damages: [(&str, Damage); 4] = [
        ("truncated", |_, _| "{".to_owned()),
        ("a credential field", |_, original| {
            original.replacen('{', "{\"token\": \"x\",", 1)
        }),
        ("a relative executable", |house, original| {
            original.replace(&house.bin.join("orca").display().to_string(), "orca")
        }),
        ("another house", |_, original| {
            original.replace("\"house\": \"acme\"", "\"house\": \"other\"")
        }),
    ];
    for (what, damage) in damages {
        let (house, printed) = stored_house()?;
        let before = house.orca_calls();
        let original = fs::read_to_string(house.runtime_file())?;
        fs::write(house.runtime_file(), damage(&house, &original))?;
        let fired = house.fire(&printed)?;
        let combined = format!("{}{}", text(&fired.stdout), text(&fired.stderr));
        assert_eq!(fired.status.code(), Some(1), "{what}: {combined}");
        assert!(
            combined.contains("runtime configuration is invalid"),
            "{what}: {combined}"
        );
        assert!(!combined.contains("token"), "{what}: {combined}");
        assert_eq!(house.orca_calls(), before, "{what}");
    }
    // Shared with other users, the file is refused whole.
    let (house, printed) = stored_house()?;
    let before = house.orca_calls();
    fs::set_permissions(house.runtime_file(), fs::Permissions::from_mode(0o644))?;
    let shared = house.fire(&printed)?;
    let combined = format!("{}{}", text(&shared.stdout), text(&shared.stderr));
    assert_eq!(shared.status.code(), Some(1), "{combined}");
    assert!(combined.contains("only its owner"), "{combined}");
    assert_eq!(house.orca_calls(), before);
    Ok(())
}

#[test]
fn storing_replaces_a_valid_configuration_and_refuses_partial_or_relative_input() -> TestResult {
    let house = House::new()?;
    let flags = house.orca_flags();
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    assert_eq!(house.configure(&flags)?.status.code(), Some(0));
    // The same flags again change nothing.
    let same = house.configure(&flags)?;
    assert_eq!(
        text(&same.stdout).trim(),
        "unchanged the runtime configuration of house acme"
    );
    // One flag overlays the stored set.
    let again = house.configure(&["--orca-run", "run-2"])?;
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert!(text(&again.stdout).starts_with("updated"));
    assert!(fs::read_to_string(house.runtime_file())?.contains("run-2"));
    let leftovers = fs::read_dir(house.registry().join("private/acme"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0);

    // A relative path is refused and the stored file is kept.
    let relative = house.configure(&["--orca", "orca"])?;
    assert_eq!(
        relative.status.code(),
        Some(2),
        "{}",
        text(&relative.stderr)
    );
    assert!(fs::read_to_string(house.runtime_file())?.contains("run-2"));

    // A first store needs every Orca fact.
    let fresh = House::new()?;
    let partial = fresh.configure(&["--orca-run", "run-1"])?;
    assert_eq!(partial.status.code(), Some(2), "{}", text(&partial.stderr));
    assert!(!fresh.runtime_file().exists());
    // Nothing to store is refused rather than reported as stored.
    let empty = fresh.configure(&[])?;
    assert_eq!(empty.status.code(), Some(2), "{}", text(&empty.stderr));
    assert!(!fresh.runtime_file().exists());
    Ok(())
}

/// `kitchn run <pass>` for `acme`, with `extra` flags.
fn run_pass(house: &House, pass: &str, extra: &[&str]) -> TestResult<Output> {
    let registry = house.registry().display().to_string();
    let mut args = vec!["run", pass, "--registry", &registry, "--house", "acme"];
    args.extend_from_slice(extra);
    house.kitchen(&args)
}

#[test]
fn a_flag_that_disagrees_with_the_stored_runtime_is_refused_before_connecting() -> TestResult {
    let elsewhere = "/somewhere/else";
    let cases: [(&str, &str); 6] = [
        ("--orca", elsewhere),
        ("--runtime-dir", elsewhere),
        ("--orca-run", "run-2"),
        ("--orca-coordinator", "term-2"),
        ("--orca-repo", "id:other"),
        ("--repository", "acme/other"),
    ];
    for (flag, value) in cases {
        let house = House::new()?;
        let mut flags = house.orca_flags();
        flags.extend(["--repository".to_owned(), "acme/app".to_owned()]);
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        assert_eq!(house.configure(&flags)?.status.code(), Some(0));
        let printed = text(&house.trigger(&[])?.stdout);
        let stored = fs::read(house.runtime_file())?;
        let before = house.orca_calls();
        // The scheduled tick and a direct pass refuse alike.
        let direct = run_pass(&house, "coordinate", &[flag, value])?;
        let direct_text = format!("{}{}", text(&direct.stdout), text(&direct.stderr));
        assert_eq!(direct.status.code(), Some(2), "{flag}: {direct_text}");
        assert!(direct_text.contains(flag), "{flag}: {direct_text}");
        assert!(direct_text.contains("disagrees"), "{flag}: {direct_text}");
        let words = shell_words(printed.trim().splitn(6, ' ').nth(5).unwrap_or_default())?;
        let (program, args) = words.split_first().ok_or("empty trigger")?;
        let tick = house
            .command(program)
            .args(args)
            .args([flag, value])
            .output()?;
        let tick_text = format!("{}{}", text(&tick.stdout), text(&tick.stderr));
        assert!(tick_text.contains(flag), "{flag}: {tick_text}");
        assert_eq!(house.orca_calls(), before, "{flag}: contacted a backend");
        assert_eq!(fs::read(house.runtime_file())?, stored, "{flag}: rewrote");
    }
    Ok(())
}

#[test]
fn a_flag_that_agrees_with_the_stored_runtime_is_accepted() -> TestResult {
    let (house, _) = stored_house()?;
    let agreeing = run_pass(
        &house,
        "coordinate",
        &["--orca-run", "run-1", "--orca-repo", "id:app"],
    )?;
    assert_eq!(
        agreeing.status.code(),
        Some(0),
        "{}",
        text(&agreeing.stderr)
    );
    assert!(house.orca_calls().contains("--run=run-1"));
    Ok(())
}

#[test]
fn a_trigger_only_prints_and_never_writes() -> TestResult {
    let (house, _) = stored_house()?;
    let stored = fs::read(house.runtime_file())?;
    // A valid trigger leaves the stored file byte for byte.
    let printed = house.trigger(&["--every-minutes", "10"])?;
    assert_eq!(printed.status.code(), Some(0), "{}", text(&printed.stderr));
    assert_eq!(fs::read(house.runtime_file())?, stored);
    // Backend, repository, and pickup flags are not the trigger's: it refuses
    // them, so a differing value can neither be stored nor printed.
    for flag in [
        ["--orca-run", "run-2"],
        ["--repository", "acme/other"],
        ["--capacity", "9"],
        ["--curl", "/usr/bin/curl"],
    ] {
        let refused = house.trigger(&flag)?;
        assert_eq!(refused.status.code(), Some(2), "{flag:?}");
        assert!(text(&refused.stdout).is_empty(), "{flag:?}");
        assert_eq!(fs::read(house.runtime_file())?, stored, "{flag:?}");
    }
    // A trigger that cannot print writes nothing, for a house with none too.
    let interval = house.trigger(&["--every-minutes", "7"])?;
    assert_eq!(
        interval.status.code(),
        Some(2),
        "{}",
        text(&interval.stderr)
    );
    assert_eq!(fs::read(house.runtime_file())?, stored);
    let fresh = House::new()?;
    let none = fresh.trigger(&["--every-minutes", "7"])?;
    assert_eq!(none.status.code(), Some(2), "{}", text(&none.stderr));
    let valid = fresh.trigger(&[])?;
    assert_eq!(valid.status.code(), Some(0), "{}", text(&valid.stderr));
    assert!(!fresh.runtime_file().exists());
    Ok(())
}

#[test]
fn a_failed_configure_leaves_the_stored_runtime_unchanged() -> TestResult {
    let (house, _) = stored_house()?;
    let stored = fs::read(house.runtime_file())?;
    for bad in [
        ["--capacity", "0"],
        ["--capacity", "65"],
        ["--report-path", "../out.md"],
        ["--ready-label", ""],
        ["--orca", "orca"],
        ["--repository", "acme/other"],
    ] {
        let refused = house.configure(&[bad[0], bad[1], "--orca-run", "run-2"])?;
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{bad:?}: {}",
            text(&refused.stderr)
        );
        assert_eq!(fs::read(house.runtime_file())?, stored, "{bad:?}");
    }
    // A valid configure still replaces it.
    let valid = house.configure(&["--orca-run", "run-2"])?;
    assert_eq!(valid.status.code(), Some(0), "{}", text(&valid.stderr));
    assert!(fs::read_to_string(house.runtime_file())?.contains("run-2"));
    Ok(())
}

#[test]
fn nondefault_pickup_settings_travel_through_a_printed_trigger() -> TestResult {
    let house = House::new()?;
    let mut flags = house.orca_flags();
    flags.extend(
        [
            "--ready-label",
            "agent-ready",
            "--needs-spec-label",
            "spec-me",
            "--human-label",
            "people-only",
            "--capacity",
            "3",
            "--branch-prefix",
            "bot",
            "--report-path",
            "reports/out.md",
        ]
        .map(str::to_owned),
    );
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let configured = house.configure(&flags)?;
    assert_eq!(
        configured.status.code(),
        Some(0),
        "{}",
        text(&configured.stderr)
    );
    let trigger = house.trigger(&[])?;
    assert_eq!(trigger.status.code(), Some(0), "{}", text(&trigger.stderr));
    let printed = text(&trigger.stdout);
    // The line still carries no settings.
    assert!(!printed.contains("agent-ready"), "{printed}");
    assert!(!printed.contains("--capacity"), "{printed}");

    let stored = fs::read_to_string(house.runtime_file())?;
    for expected in [
        "\"readyLabel\": \"agent-ready\"",
        "\"needsSpecLabel\": \"spec-me\"",
        "\"humanLabel\": \"people-only\"",
        "\"capacity\": 3",
        "\"branchPrefix\": \"bot\"",
        "\"reportPath\": \"reports/out.md\"",
    ] {
        assert!(stored.contains(expected), "{expected}: {stored}");
    }

    // The stored ready label finds issue 7, which the default label would
    // not, so the pickup inspects it.
    script(&house.bin.join("gh"), FAKE_GH_WITH_ISSUE)?;
    let fired = house.fire(&printed)?;
    let gh = fs::read_to_string(house.bin.join("gh-calls")).unwrap_or_default();
    assert!(
        gh.contains("issues/7"),
        "{gh}\n{}{}",
        text(&fired.stdout),
        text(&fired.stderr)
    );

    let defaults = House::new()?;
    let mut orca = defaults.orca_flags();
    orca.extend(["--repository".to_owned(), "acme/app".to_owned()]);
    let orca: Vec<&str> = orca.iter().map(String::as_str).collect();
    assert_eq!(defaults.configure(&orca)?.status.code(), Some(0));
    let printed_defaults = text(&defaults.trigger(&[])?.stdout);
    script(&defaults.bin.join("gh"), FAKE_GH_WITH_ISSUE)?;
    defaults.fire(&printed_defaults)?;
    let gh = fs::read_to_string(defaults.bin.join("gh-calls")).unwrap_or_default();
    assert!(!gh.contains("issues/7"), "{gh}");

    // A later configure keeps the stored settings it was not given.
    let again = house.configure(&["--capacity", "2"])?;
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    let stored = fs::read_to_string(house.runtime_file())?;
    assert!(stored.contains("\"capacity\": 2"), "{stored}");
    assert!(
        stored.contains("\"readyLabel\": \"agent-ready\""),
        "{stored}"
    );

    // A pickup flag that disagrees is refused before any call.
    let calls = house.orca_calls();
    let refused = run_pass(&house, "pickup", &["--ready-label", "ready"])?;
    let combined = format!("{}{}", text(&refused.stdout), text(&refused.stderr));
    assert_eq!(refused.status.code(), Some(2), "{combined}");
    assert!(combined.contains("--ready-label"), "{combined}");
    assert_eq!(house.orca_calls(), calls);
    Ok(())
}

#[test]
fn run_repair_reads_the_stored_branch_prefix_and_report_path() -> TestResult {
    let house = House::new()?;
    let mut flags = house.orca_flags();
    flags.extend(["--branch-prefix", "bot", "--report-path", "reports/out.md"].map(str::to_owned));
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    assert_eq!(house.configure(&flags)?.status.code(), Some(0));
    // A flag naming another value than the stored one is refused before any
    // backend call, the built-in defaults included.
    let calls = house.orca_calls();
    for (flag, value) in [
        ("--branch-prefix", "kitchen"),
        ("--report-path", "kitchen-report.md"),
    ] {
        let refused = run_pass(&house, "repair", &[flag, value])?;
        let combined = format!("{}{}", text(&refused.stdout), text(&refused.stderr));
        assert_eq!(refused.status.code(), Some(2), "{flag}: {combined}");
        assert!(combined.contains(flag), "{flag}: {combined}");
        assert!(combined.contains("disagrees"), "{flag}: {combined}");
        assert_eq!(house.orca_calls(), calls, "{flag}: contacted a backend");
    }
    // Without the flags, or with the stored values, the pass runs.
    for extra in [
        &[][..],
        &["--branch-prefix", "bot", "--report-path", "reports/out.md"],
    ] {
        let ran = run_pass(&house, "repair", extra)?;
        assert_eq!(
            ran.status.code(),
            Some(0),
            "{extra:?}: {}{}",
            text(&ran.stdout),
            text(&ran.stderr)
        );
        assert_eq!(text(&ran.stdout).trim(), "idle", "{extra:?}");
    }

    // A house with nothing stored takes the flags, and refuses a report
    // path outside the workspace as `configure` does.
    let fresh = House::new()?;
    let orca = fresh.orca_flags();
    let mut given: Vec<&str> = orca.iter().map(String::as_str).collect();
    given.extend(["--branch-prefix", "bot", "--report-path", "reports/out.md"]);
    let ran = run_pass(&fresh, "repair", &given)?;
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    let calls = fresh.orca_calls();
    let mut outside: Vec<&str> = orca.iter().map(String::as_str).collect();
    outside.extend(["--report-path", "../out.md"]);
    let refused = run_pass(&fresh, "repair", &outside)?;
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused.stderr));
    assert_eq!(fresh.orca_calls(), calls);
    Ok(())
}
