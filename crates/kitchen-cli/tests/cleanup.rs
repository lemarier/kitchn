//! `kitchen cleanup preview` through the real binary. Simulated evidence: the
//! store and worktrees are disposable, and ownership comes from the fake
//! backend; nothing here reads a live orchestrator.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};

use kitchen::{
    BackendId, CredentialId, EffectName, HouseId, TaskId, WorkflowId,
    contracts::{
        AttemptOutcome, AttemptStart, CapabilityRequirements, Claimant, Clock, CommitId,
        EvidenceRevision, Grant, HouseGrants, LeaseTtl, Operation, Permission, Provenance,
        ResourceKind, RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Timestamp, Workspace,
        fake::FakeBackend,
    },
    state::{EffectPlan, EffectState, HouseStore, StoreOptions, run_effect},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn kitchen(args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args(args)
        .output()?)
}

fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Kitchen Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Kitchen Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?} failed").into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// A pushed linked worktree under `root`.
fn pushed_worktree(root: &Path) -> TestResult<PathBuf> {
    let origin = root.join("origin.git");
    let main = root.join("main");
    let worktree = root.join("task-1");
    fs::create_dir_all(&origin)?;
    fs::create_dir_all(&main)?;
    git(&origin, &["init", "--bare", "--quiet"])?;
    git(&main, &["init", "--quiet"])?;
    git(&main, &["remote", "add", "origin", text(&origin)?])?;
    fs::write(main.join("README.md"), "kitchen\n")?;
    git(&main, &["add", "."])?;
    git(&main, &["commit", "--quiet", "-m", "initial"])?;
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "task-1",
            text(&worktree)?,
        ],
    )?;
    git(&worktree, &["push", "--quiet", "origin", "task-1"])?;
    Ok(worktree)
}

fn text(path: &Path) -> TestResult<&str> {
    path.to_str().ok_or_else(|| "non-UTF-8 path".into())
}

fn commit(fill: char) -> TestResult<CommitId> {
    Ok(CommitId::new(&fill.to_string().repeat(40))?)
}

/// A store holding one settled task that launched a worker in a worktree,
/// returning the worker handle, worktree handle, and launch key.
fn settled_task(store: &HouseStore) -> TestResult<(String, String, String)> {
    let house = store.house().clone();
    let backend = FakeBackend::fully_capable(BackendId::new("orca")?, house.clone());
    let grant = Grant::house(
        Permission::LaunchWorker,
        BackendId::new("orca")?,
        CredentialId::new("orca-local")?,
    );
    let grants = HouseGrants::new(house, [grant.clone()]);
    let task = TaskId::new("task-1")?;
    let pickup = Claimant::scheduled(kitchen::HolderId::new("pickup")?);
    let now = Timestamp::from_unix_millis(1_000);
    store.create_task(
        TaskSpec {
            id: task.clone(),
            role: Role::StationCook,
            repository: None,
            authority: TaskAuthority::delegate(&grants, [grant])?,
            retry: RetryPolicy::new(1, Duration::from_secs(60))?,
            provenance: Provenance {
                kitchen: commit('a')?,
                house_guidance: commit('b')?,
                repository_instructions: None,
            },
            resources: BTreeSet::new(),
            requires: CapabilityRequirements::new(),
        },
        &pickup,
        now,
    )?;
    let fence = store
        .claim(&task, &pickup, LeaseTtl::new(Duration::from_secs(60))?, now)?
        .fence();
    let AttemptStart::Started(attempt) = store.start_attempt(&task, fence, now)? else {
        return Err("attempt".into());
    };
    let launched = run_effect(
        store,
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
                brief: Text::new("Implement it.")?,
                branch: None,
            }
            .into(),
            consent: None,
        },
        &Fixed(now),
    )?;
    let EffectState::Applied { receipt, .. } = launched.state() else {
        return Err("launch not applied".into());
    };
    let handle = |kind| {
        receipt
            .created()
            .iter()
            .find(|resource| resource.kind == kind)
            .map(|resource| resource.handle.to_string())
            .ok_or("missing resource")
    };
    let worker = handle(ResourceKind::Worker)?;
    let worktree = handle(ResourceKind::Worktree)?;
    store.finish_attempt(&task, fence, attempt, AttemptOutcome::Succeeded, now)?;
    Ok((worker, worktree, launched.request().key().to_string()))
}

/// A clock stopped at one instant.
struct Fixed(Timestamp);

impl Clock for Fixed {
    fn now(&self) -> Timestamp {
        self.0
    }
}

fn initialize(root: &Path) -> TestResult<HouseStore> {
    Ok(HouseStore::initialize(
        root.join("house"),
        HouseId::new("origin89")?,
        StoreOptions::default(),
    )?)
}

fn preview_args<'a>(store: &'a str, inventory: &'a str) -> Vec<&'a str> {
    vec![
        "cleanup",
        "preview",
        "--store",
        store,
        "--house",
        "origin89",
        "--inventory",
        inventory,
        "--holder",
        "david",
    ]
}

#[test]
fn a_preview_explains_releases_and_exclusions_and_records_markers() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let store = initialize(&root)?;
    let (worker, worktree, key) = settled_task(&store)?;
    let path = pushed_worktree(&root)?;
    let inventory = root.join("inventory.json");
    fs::write(
        &inventory,
        serde_json::to_vec(&serde_json::json!({
            "backend": "orca",
            "resources": [
                {"kind": "worker", "handle": worker, "owner": key, "liveness": "exited", "worker": "settled-succeeded"},
                {"kind": "worktree", "handle": worktree, "owner": key, "liveness": "exited", "path": path},
                {"kind": "terminal", "handle": "legacy-console", "liveness": "live"},
            ],
        }))?,
    )?;
    let store_dir = root.join("house");
    let mut args = preview_args(text(&store_dir)?, text(&inventory)?);
    let output = kitchen(&args)?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("2 to release, 1 retained. Nothing was released."),
        "{stdout}"
    );
    assert!(
        stdout.contains("[task task-1 attempt 1 settled succeeded] release"),
        "{stdout}"
    );
    assert!(
        stdout.contains("terminal legacy-console [owner unknown] retain: unknown-owner, in-use"),
        "{stdout}"
    );
    // One marker per eligible resource, recorded by the person who ran it.
    let markers = store.markers(&WorkflowId::new("dishwasher")?)?;
    assert_eq!(markers.len(), 2);

    args.push("--json");
    let output = kitchen(&args)?;
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["trigger"], "manual");
    assert_eq!(json["entries"][1]["worktree"]["type"], "read");
    assert_eq!(
        json["entries"][2]["decision"]["reasons"][0],
        "unknown-owner"
    );
    // Unchanged evidence does not record more markers.
    assert_eq!(store.markers(&WorkflowId::new("dishwasher")?)?.len(), 2);
    Ok(())
}

#[test]
fn invalid_snapshots_are_rejected_as_input_errors() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    initialize(&root)?;
    let store_dir = root.join("house");
    let cases = [
        serde_json::json!({"backend": "orca", "resources": [], "extra": true}),
        serde_json::json!({"backend": "orca", "resources": [
            {"kind": "worktree", "handle": "w", "liveness": "exited", "path": "relative/path"}
        ]}),
        serde_json::json!({"backend": "orca", "resources": [
            {"kind": "worker", "handle": "w", "liveness": "maybe"}
        ]}),
    ];
    for case in cases {
        let inventory = root.join("inventory.json");
        fs::write(&inventory, serde_json::to_vec(&case)?)?;
        let output = kitchen(&preview_args(text(&store_dir)?, text(&inventory)?))?;
        assert_eq!(output.status.code(), Some(2), "{case}");
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

#[test]
fn an_uninitialized_store_is_an_error_not_an_idle_preview() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let inventory = root.join("inventory.json");
    fs::write(&inventory, br#"{"backend": "orca", "resources": []}"#)?;
    let missing = root.join("missing");
    let output = kitchen(&preview_args(text(&missing)?, text(&inventory)?))?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.starts_with("error:"));
    Ok(())
}
