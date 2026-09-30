//! The `kitchn run` process contract, in disposable roots with a house from
//! guided `house init` and a fake `gh`. Simulated: no GitHub account, token,
//! or worker backend is used.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use kitchen::{
    HouseId,
    contracts::{CommitId, LeaseTtl, Repository, Timestamp},
    state::{HouseStore, StoreOptions},
    workflows::run::{PASS_LEASE, Pass, run_claimant},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const KITCHEN: &str = "4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c";

fn run(command: &mut Command) -> TestResult<Output> {
    Ok(command.stdin(Stdio::null()).output()?)
}

fn git(path: &Path, args: &[&str]) -> TestResult {
    let output = run(Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(path)
        .args(args))?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", text(&output.stderr)).into());
    }
    Ok(())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A house `acme` serving `acme/app`, initialized with its store and a
/// forge binding whose token file is in place, and a `gh` that answers
/// nothing but its login.
struct House {
    _temp: tempfile::TempDir,
    checkout: PathBuf,
    home: PathBuf,
    path: String,
}

impl House {
    fn new() -> TestResult<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (checkout, home, bin) = (root.join("app"), root.join("home"), root.join("bin"));
        for directory in [&checkout, &home, &bin] {
            fs::create_dir_all(directory)?;
        }
        git(&checkout, &["init", "--quiet"])?;
        git(
            &checkout,
            &["remote", "add", "origin", "git@github.com:acme/app.git"],
        )?;
        let gh = bin.join("gh");
        fs::write(&gh, "#!/bin/sh\necho octo-cat\n")?;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;
        let bundle = kitchen::house::default_guidance(&"acme".parse()?, &CommitId::new(KITCHEN)?)?;
        let bundle_path = root.join("acme-bundle.json");
        fs::write(&bundle_path, serde_json::to_vec(&bundle)?)?;
        let house = Self {
            _temp: temp,
            checkout,
            home,
            path: format!("{}:/usr/bin:/bin", bin.display()),
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
        if init.status.code() != Some(0) {
            return Err(format!("house init: {}", text(&init.stderr)).into());
        }
        let token = house.registry().join("private/acme/credentials/github");
        if let Some(directory) = token.parent() {
            fs::create_dir_all(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        fs::write(&token, "fixture-token")?;
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
        Ok(house)
    }

    fn registry(&self) -> PathBuf {
        self.home.join(".kitchn")
    }

    fn kitchen(&self, args: &[&str]) -> TestResult<Output> {
        run(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .current_dir(&self.checkout)
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .args(args))
    }

    /// `kitchn run <pass> --registry <registry> --house acme <extra>`.
    fn pass(&self, pass: &str, extra: &[&str]) -> TestResult<Output> {
        let registry = self.registry().display().to_string();
        let mut args = vec!["run", pass, "--registry", &registry, "--house", "acme"];
        args.extend_from_slice(extra);
        self.kitchen(&args)
    }

    fn store(&self) -> TestResult<HouseStore> {
        Ok(HouseStore::open(
            self.registry().join("private/acme/store"),
            HouseId::new("acme")?,
            StoreOptions::default(),
        )?)
    }

    /// Take `pass`'s lease as another pass would, at `now`.
    fn hold(&self, pass: Pass, now: Timestamp, ttl: Duration) -> TestResult {
        self.store()?.acquire_consumer(
            &pass.consumer(&Repository::new("acme/app")?)?,
            &run_claimant()?,
            LeaseTtl::new(ttl)?,
            now,
        )?;
        Ok(())
    }
}

fn now() -> TestResult<Timestamp> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    Ok(Timestamp::from_unix_millis(u64::try_from(millis)?))
}

#[test]
fn gate_is_idle_without_settled_work_and_exits_zero() -> TestResult {
    let house = House::new()?;
    let output = house.pass("gate", &[])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout).trim(), "idle");
    Ok(())
}

#[test]
fn a_duplicate_start_exits_three_and_an_expired_lease_four_until_taken_over() -> TestResult {
    let house = House::new()?;
    house.hold(Pass::Gate, now()?, PASS_LEASE)?;
    let busy = house.pass("gate", &[])?;
    assert_eq!(busy.status.code(), Some(3), "{}", text(&busy.stderr));
    assert!(text(&busy.stdout).starts_with("busy"));

    let house = House::new()?;
    let an_hour_ago = Timestamp::from_unix_millis(now()?.as_unix_millis() - 3_600_000);
    house.hold(Pass::Gate, an_hour_ago, Duration::from_secs(60))?;
    let uncertain = house.pass("gate", &[])?;
    assert_eq!(
        uncertain.status.code(),
        Some(4),
        "{}",
        text(&uncertain.stderr)
    );
    assert!(text(&uncertain.stdout).contains("--take-over"));
    let taken = house.pass("gate", &["--take-over"])?;
    assert_eq!(taken.status.code(), Some(0), "{}", text(&taken.stderr));
    assert_eq!(text(&taken.stdout).trim(), "idle");
    Ok(())
}

#[test]
fn a_repository_outside_the_house_is_invalid_input() -> TestResult {
    let house = House::new()?;
    let output = house.pass("gate", &["--repository", "someone/else"])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("not one of the house's repositories"));
    Ok(())
}

#[test]
fn a_pass_needing_workers_names_the_missing_backend_arguments() -> TestResult {
    // Guided init binds the house to Orca, whose host facts the trigger
    // passes; without them nothing is contacted.
    let house = House::new()?;
    for pass in ["pickup", "coordinate", "repair"] {
        let output = house.pass(pass, &[])?;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{pass}: {}",
            text(&output.stderr)
        );
        assert!(
            text(&output.stderr).contains("--orca, --runtime-dir, --orca-run"),
            "{pass}: {}",
            text(&output.stderr)
        );
    }
    let relative = house.pass(
        "coordinate",
        &[
            "--orca",
            "orca",
            "--runtime-dir",
            "/tmp/unused",
            "--orca-run",
            "run",
            "--orca-coordinator",
            "term",
            "--orca-repo",
            "id:app",
        ],
    )?;
    assert_eq!(relative.status.code(), Some(2));
    assert!(text(&relative.stderr).contains("absolute --orca"));
    // Nothing ran, so no pass lease was taken.
    let store = house.store()?;
    let repository = Repository::new("acme/app")?;
    for pass in Pass::ALL {
        assert!(store.consumer(&pass.consumer(&repository)?)?.is_none());
    }
    Ok(())
}

#[test]
fn an_unknown_pass_is_invalid_input() -> TestResult {
    let house = House::new()?;
    let output = house.pass("tick", &[])?;
    assert_eq!(output.status.code(), Some(2));
    Ok(())
}

#[test]
fn gate_attest_accepts_only_pr_and_review_ids() -> TestResult {
    let house = House::new()?;
    let registry = house.registry().display().to_string();
    let missing = house.kitchen(&[
        "gate",
        "attest",
        "--registry",
        &registry,
        "--house",
        "acme",
        "--pull-request",
        "12",
    ])?;
    assert_eq!(missing.status.code(), Some(2));
    assert!(text(&missing.stderr).contains("--review-id"));

    let asserted = house.kitchen(&[
        "gate",
        "attest",
        "--registry",
        &registry,
        "--house",
        "acme",
        "--pull-request",
        "12",
        "--review-id",
        "11",
        "--recorder",
        "someone",
    ])?;
    assert_eq!(
        asserted.status.code(),
        Some(2),
        "{}",
        text(&asserted.stderr)
    );
    assert!(text(&asserted.stderr).contains("unexpected argument"));

    let invalid = house.kitchen(&[
        "gate",
        "attest",
        "--registry",
        &registry,
        "--house",
        "acme",
        "--pull-request",
        "0",
        "--review-id",
        "11",
    ])?;
    assert_eq!(invalid.status.code(), Some(2));
    Ok(())
}

#[test]
fn gate_attest_reads_the_forge_review_and_records_its_author() -> TestResult {
    let house = House::new()?;
    let sha = KITCHEN;
    let body = format!(
        "```kitchen-attestation\nhead={sha}\nbase={sha}\nsemantic=clean\nread_only=true\nacceptance=complete\nhardware=complete\nrisk=none\n```"
    );
    let fixtures = house.home.join("forge-fixtures");
    fs::create_dir_all(&fixtures)?;
    fs::write(fixtures.join("user"), r#"{"login":"octo-cat"}"#)?;
    fs::write(
        fixtures.join("pr"),
        serde_json::to_vec(&serde_json::json!({
            "number": 12, "state": "open", "draft": false, "merged": false,
            "head": {"sha": sha, "ref": "review-branch", "repo": {"full_name": "acme/app"}},
            "base": {"sha": sha, "ref": "main", "repo": {"full_name": "acme/app"}},
            "mergeable": true, "mergeable_state": "clean", "user": {"login": "author"}
        }))?,
    )?;
    fs::write(
        fixtures.join("branch"),
        serde_json::to_vec(&serde_json::json!({
            "name": "main", "commit": {"sha": sha}
        }))?,
    )?;
    fs::write(
        fixtures.join("reviews"),
        serde_json::to_vec(&serde_json::json!([{
            "id": 11, "user": {"login": "reviewer"}, "commit_id": sha,
            "state": "APPROVED", "body": body
        }]))?,
    )?;
    fs::write(
        fixtures.join("commits"),
        serde_json::to_vec(&serde_json::json!([{
            "sha": sha, "author": {"login": "author"}, "committer": {"login": "author"}
        }]))?,
    )?;
    let script = format!(
        "#!/bin/sh\ncase \" $* \" in\n  *\" config get user \"*) echo octo-cat;;\n  *\" user \"*) cat '{}/user';;\n  *\"pulls/12/reviews\"*) cat '{}/reviews';;\n  *\"pulls/12/commits\"*) cat '{}/commits';;\n  *\"branches/main\"*) cat '{}/branch';;\n  *\"pulls/12\"*) cat '{}/pr';;\n  *) exit 1;;\nesac\n",
        fixtures.display(),
        fixtures.display(),
        fixtures.display(),
        fixtures.display(),
        fixtures.display()
    );
    let gh = Path::new(&house.path.split(':').next().ok_or("bin")?).join("gh");
    fs::write(&gh, script)?;
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;
    let registry = house.registry().display().to_string();
    let output = house.kitchen(&[
        "gate",
        "attest",
        "--registry",
        &registry,
        "--house",
        "acme",
        "--pull-request",
        "12",
        "--review-id",
        "11",
    ])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let stored = kitchen::workflows::run::gate_attestation(
        &house.store()?,
        &Repository::new("acme/app")?,
        kitchen::contracts::IssueNumber::new(12)?,
        &CommitId::new(sha)?,
        &CommitId::new(sha)?,
    )?
    .ok_or("missing attestation")?;
    assert_eq!(stored.attestation.forge_review.reviewer, "reviewer");
    assert_eq!(stored.recorded_by.as_str(), "reviewer");
    Ok(())
}
