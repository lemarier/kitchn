//! The `kitchn run` process contract, in disposable roots with a house from
//! guided `house init` and a fake `gh`. Simulated: no GitHub account, token,
//! or worker backend is used.
#![cfg(unix)]

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use kitchen::{
    BackendId, CredentialId, EffectName, HolderId, HouseId,
    contracts::{
        AttemptOutcome, AttemptStart, BranchName, CapabilityRequirements, Claimant, Clock,
        CommitId, EvidenceRevision, Grant, HouseGrants, LeaseTtl, Operation, Permission,
        Provenance, Repository, RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Timestamp,
        Workspace, fake::FakeBackend,
    },
    house::HouseConfig,
    state::{EffectPlan, EffectState, HouseStore, StoreOptions, TaskState, run_effect},
    workflows::pickup::{IssueRef, issue_task_id},
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

struct Fixed(Timestamp);

impl Clock for Fixed {
    fn now(&self) -> Timestamp {
        self.0
    }
}

fn gate_ready_task(house: &House) -> TestResult {
    let repository = Repository::new("acme/app")?;
    let house_id = HouseId::new("acme")?;
    let backend_id = BackendId::new("orca")?;
    let grant = Grant::repository(
        Permission::LaunchWorker,
        repository.clone(),
        backend_id.clone(),
        CredentialId::new("orca-local")?,
    );
    let grants = HouseGrants::new(house_id.clone(), [grant.clone()]);
    let backend = FakeBackend::fully_capable(backend_id, house_id);
    let store = house.store()?;
    let task = issue_task_id(&IssueRef {
        repository: repository.clone(),
        number: kitchen::contracts::IssueNumber::new(7)?,
    })?;
    let claimant = Claimant::scheduled(HolderId::new("pickup")?);
    let at = now()?;
    store.create_task(
        TaskSpec {
            id: task.clone(),
            role: Role::StationCook,
            repository: Some(repository),
            authority: TaskAuthority::delegate(&grants, [grant])?,
            retry: RetryPolicy::new(1, Duration::from_secs(60))?,
            provenance: Provenance {
                kitchen: CommitId::new(KITCHEN)?,
                house_guidance: CommitId::new(KITCHEN)?,
                repository_instructions: None,
            },
            resources: BTreeSet::new(),
            requires: CapabilityRequirements::new(),
            agent: None,
            work_type: None,
        },
        &claimant,
        at,
    )?;
    let fence = store
        .claim(
            &task,
            &claimant,
            LeaseTtl::new(Duration::from_secs(60))?,
            at,
        )?
        .fence();
    let AttemptStart::Started(attempt) = store.start_attempt(&task, fence, at)? else {
        return Err("pickup attempt did not start".into());
    };
    let launched = run_effect(
        &store,
        &backend,
        &grants,
        EffectPlan {
            task: task.clone(),
            fence,
            name: EffectName::new("launch")?,
            decided_at: EvidenceRevision::INITIAL,
            effect: Operation::LaunchWorker {
                role: Role::StationCook,
                workspace: Workspace::Isolated,
                brief: Text::new("Implement issue 7")?,
                branch: Some(BranchName::new("kitchen/issue-7")?),
                agent: None,
            }
            .into(),
            consent: None,
            basis: None,
        },
        &Fixed(at),
    )?;
    assert!(matches!(launched.state(), EffectState::Applied { .. }));
    store.finish_attempt(&task, fence, attempt, AttemptOutcome::Succeeded, at)?;
    assert!(matches!(
        store.task(&task)?.state(),
        TaskState::Settled { .. }
    ));

    let config_path = house.registry().join("houses/acme.json");
    let mut config: HouseConfig = serde_json::from_slice(&fs::read(&config_path)?)?;
    let merge = Grant::repository(
        Permission::Merge,
        Repository::new("acme/app")?,
        BackendId::new("github")?,
        CredentialId::new("github")?,
    );
    config.policy_limits.insert(merge.clone());
    config.grants.insert(merge);
    config.required_reviewers = ["reviewer".to_owned()].into();
    fs::write(config_path, serde_json::to_vec(&config)?)?;
    Ok(())
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
    fs::write(house.checkout.join("reviewed.txt"), "reviewed")?;
    git(&house.checkout, &["add", "reviewed.txt"])?;
    git(
        &house.checkout,
        &[
            "-c",
            "user.name=Author",
            "-c",
            "user.email=author@example.com",
            "commit",
            "--quiet",
            "-m",
            "Reviewed",
        ],
    )?;
    let sha_output = run(Command::new("git")
        .arg("-C")
        .arg(&house.checkout)
        .args(["rev-parse", "HEAD"]))?;
    let sha = text(&sha_output.stdout);
    let sha = sha.trim();
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
        fixtures.join("pulls"),
        serde_json::to_vec(&serde_json::json!([{
            "number": 12, "state": "open",
            "head": {"sha": sha, "ref": "review-branch", "repo": {"full_name": "acme/app"}},
            "base": {"ref": "main"}, "user": {"login": "author"}
        }]))?,
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
        "#!/bin/sh\ncase \" $* \" in\n  *\" config get user \"*) echo octo-cat;;\n  *\" user \"*) cat '{}/user';;\n  *\"pulls/12/reviews\"*) cat '{}/reviews';;\n  *\"pulls/12/commits\"*) cat '{}/commits';;\n  *\"branches/main\"*) cat '{}/branch';;\n  *\"pulls?state=open\"*) cat '{}/pulls';;\n  *\"pulls/12\"*) cat '{}/pr';;\n  *) exit 1;;\nesac\n",
        fixtures.display(),
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
    fs::write(house.checkout.join("untracked"), "unreviewed")?;
    let explicit = house.kitchen(&[
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
    assert_eq!(explicit.status.code(), Some(1));
    assert!(text(&explicit.stderr).contains("already recorded"));
    Ok(())
}

#[test]
fn gate_inference_refuses_dirty_stale_missing_and_ambiguous_heads() -> TestResult {
    let house = House::new()?;
    git(
        &house.checkout,
        &["checkout", "--quiet", "-b", "review-branch"],
    )?;
    fs::write(house.checkout.join("reviewed.txt"), "reviewed")?;
    git(&house.checkout, &["add", "reviewed.txt"])?;
    git(
        &house.checkout,
        &[
            "-c",
            "user.name=Author",
            "-c",
            "user.email=author@example.com",
            "commit",
            "--quiet",
            "-m",
            "Reviewed",
        ],
    )?;
    let sha_output = run(Command::new("git")
        .arg("-C")
        .arg(&house.checkout)
        .args(["rev-parse", "HEAD"]))?;
    let sha = text(&sha_output.stdout);
    let sha = sha.trim();
    let fixture = house.home.join("pulls.json");
    let pr = |number, head: &str| {
        serde_json::json!({
            "number": number, "state": "open",
            "head": {"sha": head, "ref": "review-branch", "repo": {"full_name": "acme/app"}},
            "base": {"ref": "main"}, "user": {"login": "author"}
        })
    };
    let gh = Path::new(&house.path.split(':').next().ok_or("bin")?).join("gh");
    fs::write(
        &gh,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" config get user \"*) echo octo-cat;;\n  *\" user \"*) echo '{{\"login\":\"octo-cat\"}}';;\n  *\"pulls?state=open\"*) cat '{}';;\n  *\"pulls/12\"*) cat '{}';;\n  *) exit 1;;\nesac\n",
            fixture.display(),
            house.home.join("pr.json").display()
        ),
    )?;
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;
    let registry = house.registry().display().to_string();
    let command = || {
        house.kitchen(&[
            "gate",
            "attest",
            "--registry",
            &registry,
            "--house",
            "acme",
            "--review-id",
            "11",
        ])
    };

    fs::write(house.checkout.join("untracked"), "dirty")?;
    let dirty = command()?;
    assert_eq!(dirty.status.code(), Some(1));
    assert!(text(&dirty.stderr).contains("dirty checkout"));
    fs::remove_file(house.checkout.join("untracked"))?;

    fs::write(&fixture, "[]")?;
    let missing = command()?;
    assert_eq!(missing.status.code(), Some(2));
    assert!(text(&missing.stderr).contains("--pull-request"));

    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!([pr(12, sha), pr(13, sha)]))?,
    )?;
    let ambiguous = command()?;
    assert_eq!(ambiguous.status.code(), Some(2));
    assert!(text(&ambiguous.stderr).contains("--pull-request"));

    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!([pr(12, KITCHEN)]))?,
    )?;
    fs::write(
        house.home.join("pr.json"),
        serde_json::to_vec(&serde_json::json!({
            "number": 12, "state": "open", "draft": false, "merged": false,
            "head": {"sha": KITCHEN, "ref": "review-branch", "repo": {"full_name": "acme/app"}},
            "base": {"sha": sha, "ref": "main"}, "mergeable": true
        }))?,
    )?;
    let stale = command()?;
    assert_eq!(stale.status.code(), Some(1));
    assert!(text(&stale.stderr).contains("live forge head"));
    Ok(())
}

#[test]
fn gate_inference_reports_detached_checkout_with_moved_pr_head() -> TestResult {
    let house = House::new()?;
    fs::write(house.checkout.join("reviewed.txt"), "reviewed")?;
    git(&house.checkout, &["add", "reviewed.txt"])?;
    git(
        &house.checkout,
        &[
            "-c",
            "user.name=Author",
            "-c",
            "user.email=author@example.com",
            "commit",
            "--quiet",
            "-m",
            "Reviewed",
        ],
    )?;
    git(&house.checkout, &["checkout", "--quiet", "--detach"])?;

    let moved_head = "a".repeat(40);
    let pr = serde_json::json!({
        "number": 12, "state": "open",
        "head": {"sha": moved_head, "ref": "review-branch", "repo": {"full_name": "acme/app"}},
        "base": {"ref": "main"}, "user": {"login": "author"}
    });
    let list = house.home.join("pulls.json");
    let detail = house.home.join("pr.json");
    fs::write(&list, serde_json::to_vec(&serde_json::json!([pr]))?)?;
    let mut detailed = pr;
    detailed["draft"] = serde_json::json!(false);
    detailed["merged"] = serde_json::json!(false);
    detailed["mergeable"] = serde_json::json!(true);
    detailed["base"]["sha"] = serde_json::json!(KITCHEN);
    fs::write(&detail, serde_json::to_vec(&detailed)?)?;
    let gh = Path::new(&house.path.split(':').next().ok_or("bin")?).join("gh");
    fs::write(
        &gh,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" config get user \"*) echo octo-cat;;\n  *\" user \"*) echo '{{\"login\":\"octo-cat\"}}';;\n  *\"pulls?state=open\"*) cat '{}';;\n  *\"pulls/12\"*) cat '{}';;\n  *) exit 1;;\nesac\n",
            list.display(),
            detail.display()
        ),
    )?;
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;

    let registry = house.registry().display().to_string();
    for arguments in [
        vec!["gate", "attest", "--review-id", "11"],
        vec![
            "gate",
            "review",
            "--verdict",
            "approve",
            "--body-file",
            "unused.txt",
        ],
    ] {
        let mut command = arguments;
        command.extend(["--registry", &registry, "--house", "acme"]);
        let result = house.kitchen(&command)?;
        assert_eq!(result.status.code(), Some(1));
        let error = text(&result.stderr);
        assert!(
            error.contains("matches no open pull request head"),
            "{error}"
        );
        assert!(error.contains("may be stale"), "{error}");
        assert!(error.contains("--pull-request"), "{error}");
        assert!(error.contains("--head"), "{error}");
    }
    Ok(())
}

#[test]
fn cli_attestation_is_consumed_by_gate_for_one_exact_head_merge() -> TestResult {
    let house = House::new()?;
    gate_ready_task(&house)?;
    let head = "d".repeat(40);
    let base = "e".repeat(40);
    let body = format!(
        "```kitchen-attestation\nhead={head}\nbase={base}\nsemantic=clean\nread_only=true\nacceptance=complete\nhardware=complete\nrisk=none\n```"
    );
    let fixtures = house.home.join("gate-forge");
    fs::create_dir(&fixtures)?;
    let put = |name: &str, value: serde_json::Value| -> TestResult {
        fs::write(fixtures.join(name), serde_json::to_vec(&value)?)?;
        Ok(())
    };
    let pr = serde_json::json!({
        "number": 12, "state": "open", "draft": false, "merged": false,
        "head": {"sha": head, "ref": "kitchen/issue-7", "repo": {"full_name": "acme/app"}},
        "base": {"sha": base, "ref": "main", "repo": {"full_name": "acme/app"}},
        "mergeable": true, "mergeable_state": "clean", "user": {"login": "octo-cat"}
    });
    put("pr", pr.clone())?;
    let mut merged = pr;
    merged["state"] = serde_json::json!("closed");
    merged["merged"] = serde_json::json!(true);
    merged["merge_commit_sha"] = serde_json::json!("9".repeat(40));
    put("pr-merged", merged)?;
    put("user", serde_json::json!({"login": "octo-cat"}))?;
    put("repo", serde_json::json!({"default_branch": "main"}))?;
    put(
        "branch",
        serde_json::json!({"name": "main", "commit": {"sha": base}}),
    )?;
    put(
        "compare",
        serde_json::json!({"behind_by": 0, "ahead_by": 1}),
    )?;
    put(
        "checks",
        serde_json::json!({"check_runs": [{
            "name": "build", "head_sha": head, "status": "completed", "conclusion": "success"
        }]}),
    )?;
    put("statuses", serde_json::json!([]))?;
    put(
        "protection",
        serde_json::json!({"contexts": ["build"], "checks": []}),
    )?;
    put(
        "reviews",
        serde_json::json!([{
            "id": 11, "user": {"login": "reviewer"}, "commit_id": head,
            "state": "APPROVED", "body": body, "submitted_at": "1970-01-01T00:00:00Z"
        }]),
    )?;
    put(
        "commits",
        serde_json::json!([{
            "sha": head, "author": {"login": "octo-cat"}, "committer": {"login": "octo-cat"}
        }]),
    )?;
    put(
        "commit",
        serde_json::json!({
            "sha": head, "commit": {"committer": {"date": "1970-01-01T00:00:00Z"}}
        }),
    )?;
    put("timeline", serde_json::json!([]))?;
    put(
        "closing",
        serde_json::json!({"data": {"repository": {"issue": {
            "closedByPullRequestsReferences": {"nodes": [{
                "number": 12, "repository": {"nameWithOwner": "acme/app"}
            }], "pageInfo": {"hasNextPage": false, "endCursor": null}}
        }}}}),
    )?;
    put(
        "merge-state",
        serde_json::json!({"data": {"repository": {"pullRequest": {
            "headRefOid": head, "mergeStateStatus": "CLEAN"
        }}}}),
    )?;
    put(
        "threads",
        serde_json::json!({"data": {"repository": {"pullRequest": {
            "reviewThreads": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}
        }}}}),
    )?;
    let script = r#"#!/bin/sh
dir='@FIXTURES@'
printf '%s\n' "$*" >> "$dir/calls"
case " $* " in
  *" config get user "*) echo octo-cat ;;
  *" graphql "*)
    query=$(cat)
    case "$query" in
      *closedByPullRequestsReferences*) cat "$dir/closing" ;;
      *mergeStateStatus*) cat "$dir/merge-state" ;;
      *reviewThreads*) cat "$dir/threads" ;;
      *) exit 1 ;;
    esac ;;
  *" --method PUT repos/acme/app/pulls/12/merge "*)
    cat >> "$dir/merge-requests"
    echo >> "$dir/merge-requests"
    cp "$dir/pr-merged" "$dir/pr"
    printf 'HTTP/2 200\r\n\r\n{"merged":true}\n' ;;
  *" user "*) cat "$dir/user" ;;
  *" repos/acme/app/pulls/12/reviews"*) cat "$dir/reviews" ;;
  *" repos/acme/app/pulls/12/commits"*) cat "$dir/commits" ;;
  *" repos/acme/app/pulls/12 "*) cat "$dir/pr" ;;
  *" repos/acme/app/branches/main "*) cat "$dir/branch" ;;
  *" repos/acme/app/compare/"*) cat "$dir/compare" ;;
  *" repos/acme/app/commits/"*"/check-runs"*) cat "$dir/checks" ;;
  *" repos/acme/app/commits/"*"/statuses"*) cat "$dir/statuses" ;;
  *" repos/acme/app/commits/"*) cat "$dir/commit" ;;
  *" repos/acme/app/branches/main/protection/required_status_checks "*) cat "$dir/protection" ;;
  *" repos/acme/app/issues/7/timeline"*) cat "$dir/timeline" ;;
  *" repos/acme/app "*) cat "$dir/repo" ;;
  *) exit 1 ;;
esac
"#;
    let gh = Path::new(house.path.split(':').next().ok_or("bin")?).join("gh");
    fs::write(
        &gh,
        script.replace("@FIXTURES@", &fixtures.display().to_string()),
    )?;
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))?;

    let registry = house.registry().display().to_string();
    let attested = house.kitchen(&[
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
    assert_eq!(
        attested.status.code(),
        Some(0),
        "{}\n{}",
        text(&attested.stderr),
        fs::read_to_string(fixtures.join("calls"))?
    );
    let gated = house.pass("gate", &[])?;
    assert_eq!(
        gated.status.code(),
        Some(0),
        "{}\n{}",
        text(&gated.stderr),
        fs::read_to_string(fixtures.join("calls"))?
    );
    assert!(
        text(&gated.stdout).contains("Merge, Merged"),
        "{}\n{}",
        text(&gated.stdout),
        fs::read_to_string(fixtures.join("calls"))?
    );
    let requests = fs::read_to_string(fixtures.join("merge-requests"))?;
    let requests: Vec<serde_json::Value> = requests
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        requests,
        [serde_json::json!({"sha": head, "merge_method": "squash"})]
    );
    let gate_task = house
        .store()?
        .tasks()?
        .into_iter()
        .find(|record| record.spec().role == Role::Expediter)
        .ok_or("missing gate task")?;
    assert!(matches!(
        gate_task.state(),
        TaskState::Settled {
            settlement: kitchen::contracts::Settlement::Succeeded,
            ..
        }
    ));
    let again = house.pass("gate", &[])?;
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout).trim(), "idle");
    assert_eq!(
        fs::read_to_string(fixtures.join("merge-requests"))?,
        requests
            .iter()
            .map(|request| format!("{request}\n"))
            .collect::<String>()
    );
    Ok(())
}
