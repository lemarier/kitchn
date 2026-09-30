//! Backend host facts for scheduled ticks: a trigger printed for an
//! Orca-bound house stores them in the house's private runtime configuration
//! and stays `kitchn tick --registry --house`. Simulated: `orca` and `gh` are
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
    let trigger = house.trigger(&flags)?;
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
    assert!(stdout.contains("kitchn tick trigger"), "{stdout}");
    assert_eq!(house.orca_calls(), before);
    Ok(())
}

/// A fresh house with its runtime stored, and the cron line that ticks it.
fn stored_house() -> TestResult<(House, String)> {
    let house = House::new()?;
    let flags = house.orca_flags();
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let printed = text(&house.trigger(&flags)?.stdout);
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
    assert_eq!(house.trigger(&flags)?.status.code(), Some(0));
    // One flag overlays the stored set.
    let again = house.trigger(&["--orca-run", "run-2"])?;
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert!(fs::read_to_string(house.runtime_file())?.contains("run-2"));
    assert!(!house.runtime_file().with_extension("json.tmp").exists());

    // A relative path is refused and the stored file is kept.
    let relative = house.trigger(&["--orca", "orca"])?;
    assert_eq!(
        relative.status.code(),
        Some(2),
        "{}",
        text(&relative.stderr)
    );
    assert!(fs::read_to_string(house.runtime_file())?.contains("run-2"));

    // A first store needs every Orca fact.
    let fresh = House::new()?;
    let partial = fresh.trigger(&["--orca-run", "run-1"])?;
    assert_eq!(partial.status.code(), Some(2), "{}", text(&partial.stderr));
    assert!(!fresh.runtime_file().exists());
    Ok(())
}
