//! Dishwasher ownership and preservation tests. Simulated evidence only: the
//! backend is the in-memory fake with an inventory wrapper, and every
//! worktree is a disposable Git repository in a temporary directory.

mod common;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use common::{
    Fixture, ManualClock, TestResult, WORKER_PERMISSIONS, backend_id, commit, credential, grants,
    grants_for, house, interactive, launch, plan, scheduled, spec, ttl,
};
use kitchen::{
    Error, ErrorClass, TaskId,
    contracts::{
        AttemptOutcome, AttemptStart, BackendDescriptor, BackendUnavailable, Capability,
        CapabilitySet, Claimant, Clock, CommitId, Consent, Effect, EffectExecutor, EffectFailure,
        EffectRequest, EvidenceRevision, ExternalRef, Grant, HouseGrants, Liveness, Lookup,
        Operation, Permission, Provenance, Receipt, ResourceKind, ResourceObservation, ResourceRef,
        WorkerBackend, WorkerOutcome, WorkerState,
        fake::{ExecuteFault, FakeBackend},
    },
    state::{
        EffectState, HouseStore, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem,
        run_effect,
    },
    workflows::cleanup::{
        ApplyOptions, ApplyReport, ApprovalOutcome, ApprovalResult, BuildOutcome, BuildReport,
        CACHEDIR_SIGNATURE, CleanupError, ConsentSource, Decision, EXTERNAL_CACHE_SUGGESTIONS,
        Exclusion, GitLimits, GitReadError, InspectionTrigger, Inspector, NoConsent, Ownership,
        Precheck, Preview, ReleaseOutcome, Step, apply, approve, inspect, inspect_worktree,
        reclaim_build_output,
    },
};

// ---------------------------------------------------------------------------
// Git fixtures

/// Run Git for a fixture, isolated from the user's configuration.
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
        .env_remove("GIT_INDEX_FILE")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// A bare origin, a main checkout with one pushed commit, and linked worktrees.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let origin = dir.path().join("origin.git");
        let main = dir.path().join("main");
        fs::create_dir_all(&origin)?;
        git(&origin, &["init", "--bare", "--quiet"])?;
        fs::create_dir_all(&main)?;
        git(&main, &["init", "--quiet"])?;
        git(&main, &["remote", "add", "origin", path_str(&origin)?])?;
        fs::write(main.join("README.md"), "kitchen\n")?;
        fs::write(main.join(".gitignore"), "target/\n")?;
        git(&main, &["add", "README.md", ".gitignore"])?;
        git(&main, &["commit", "--quiet", "-m", "initial"])?;
        git(&main, &["push", "--quiet", "origin", "main"])?;
        Ok(Self { dir })
    }

    fn main(&self) -> PathBuf {
        self.dir.path().join("main")
    }

    /// A linked worktree on a new branch whose commit is pushed.
    fn pushed_worktree(&self, name: &str) -> TestResult<PathBuf> {
        let path = self.dir.path().join(name);
        git(
            &self.main(),
            &["worktree", "add", "--quiet", "-b", name, path_str(&path)?],
        )?;
        fs::write(path.join(format!("{name}.txt")), "work\n")?;
        git(&path, &["add", "."])?;
        git(&path, &["commit", "--quiet", "-m", name])?;
        git(&path, &["push", "--quiet", "origin", name])?;
        Ok(path)
    }
}

fn path_str(path: &Path) -> TestResult<&str> {
    path.to_str().ok_or_else(|| "non-UTF-8 path".into())
}

fn head(path: &Path) -> TestResult<CommitId> {
    Ok(CommitId::new(git(path, &["rev-parse", "HEAD"])?.trim())?)
}

/// Create a tagged build directory holding `bytes` of output.
fn build_dir(dir: &Path, bytes: usize) -> TestResult<PathBuf> {
    let target = dir.join("target");
    fs::create_dir_all(target.join("debug"))?;
    fs::write(
        target.join("CACHEDIR.TAG"),
        [CACHEDIR_SIGNATURE, b"\n"].concat(),
    )?;
    fs::write(target.join("debug").join("output.rlib"), vec![7_u8; bytes])?;
    Ok(target)
}

/// Ignore `patterns` in every worktree of the repository through
/// `.git/info/exclude`, which `git status` honors without reporting.
fn exclude(repo: &Repo, patterns: &str) -> TestResult {
    let file = repo.main().join(".git").join("info").join("exclude");
    let mut current = fs::read_to_string(&file).unwrap_or_default();
    current.push_str(patterns);
    fs::write(file, current)?;
    Ok(())
}

/// The ignored paths the worktree entry of `preview` lists as keeping it.
fn ignored_files(preview: &Preview, resource: &ResourceRef) -> TestResult<Vec<String>> {
    let json = serde_json::to_value(preview.entry(resource).ok_or("resource not previewed")?)?;
    Ok(serde_json::from_value(
        json["worktree"]["ignoredFiles"].clone(),
    )?)
}

/// Commit locally without pushing.
fn commit_locally(path: &Path, name: &str) -> TestResult {
    fs::write(path.join(name), "unpushed\n")?;
    git(path, &["add", name])?;
    git(path, &["commit", "--quiet", "-m", name])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Backend: the fake plus inventoried worktrees and scripted changes

/// A scripted change to the extra inventory.
type Change = dyn Fn(&mut Vec<ResourceObservation>);

/// The fake backend, with worktrees in its inventory. Releasing an extra
/// resource removes it, as a backend that reclaimed it would.
struct Inventory {
    fake: FakeBackend,
    extra: RefCell<Vec<ResourceObservation>>,
    calls: Cell<usize>,
    /// The inventory call count after which `change` applies once.
    change_after: Cell<Option<usize>>,
    change: RefCell<Option<Box<Change>>>,
    outage: Cell<bool>,
}

impl Inventory {
    fn new(fake: FakeBackend) -> Self {
        Self {
            fake,
            extra: RefCell::new(Vec::new()),
            calls: Cell::new(0),
            change_after: Cell::new(None),
            change: RefCell::new(None),
            outage: Cell::new(false),
        }
    }

    fn add(&self, resource: ResourceRef, owner: Option<ExternalRef>, liveness: Liveness) {
        self.extra.borrow_mut().push(ResourceObservation {
            resource,
            owner,
            liveness,
        });
    }

    /// Apply `change` once, after `calls` more inventory calls have been made.
    fn change_after(&self, calls: usize, change: impl Fn(&mut Vec<ResourceObservation>) + 'static) {
        self.change_after.set(Some(self.calls.get() + calls));
        *self.change.borrow_mut() = Some(Box::new(change));
    }
}

impl EffectExecutor for Inventory {
    fn descriptor(&self) -> &BackendDescriptor {
        self.fake.descriptor()
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let result = self.fake.execute(request);
        if let Effect::Worker(Operation::ReleaseResource { resource }) = request.effect() {
            // A lost response still means the release happened.
            let applied = result.is_ok()
                || self
                    .fake
                    .lookup(request)
                    .is_ok_and(|lookup| matches!(lookup, Lookup::Applied(_)));
            if applied {
                self.extra
                    .borrow_mut()
                    .retain(|observation| &observation.resource != resource);
            }
        }
        result
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.fake.lookup(request)
    }
}

impl WorkerBackend for Inventory {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.fake.observe_worker(worker)
    }

    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        if self.outage.get() {
            return Err(BackendUnavailable::Timeout);
        }
        let calls = self.calls.get() + 1;
        self.calls.set(calls);
        if self.change_after.get() == Some(calls - 1) {
            self.change_after.set(None);
            if let Some(change) = self.change.borrow_mut().take() {
                change(&mut self.extra.borrow_mut());
            }
        }
        let mut all = self.fake.inventory()?;
        all.extend(self.extra.borrow().iter().cloned());
        Ok(all)
    }
}

// ---------------------------------------------------------------------------
// Harness

struct Owned {
    task: TaskId,
    worker: ResourceRef,
    worktree: ResourceRef,
    key: ExternalRef,
}

struct Harness {
    fixture: Fixture,
    backend: Inventory,
    paths: BTreeMap<ResourceRef, PathBuf>,
    merged: BTreeMap<ResourceRef, CommitId>,
    limits: GitLimits,
    clock: ManualClock,
    repo: Repo,
}

const PREVIEW_AGE: Duration = Duration::from_secs(3600);

impl Harness {
    fn new() -> TestResult<Self> {
        Self::with_backend(FakeBackend::fully_capable(backend_id()?, house()?))
    }

    fn with_backend(fake: FakeBackend) -> TestResult<Self> {
        Ok(Self {
            fixture: Fixture::new()?,
            backend: Inventory::new(fake),
            paths: BTreeMap::new(),
            merged: BTreeMap::new(),
            limits: GitLimits::default(),
            clock: ManualClock::starting_at(1_000),
            repo: Repo::new()?,
        })
    }

    fn store(&self) -> &HouseStore {
        &self.fixture.store
    }

    fn inspector(&self) -> Inspector<'_> {
        Inspector {
            store: self.store(),
            backend: &self.backend,
            worktrees: &self.paths,
            merged_heads: &self.merged,
            git: &self.limits,
        }
    }

    /// A task that launched a worker in an isolated worktree, whose worker
    /// reported `outcome`. With `settle`, the task settles too.
    fn owner(&mut self, name: &str, settle: bool) -> TestResult<Owned> {
        let task = TaskId::new(name)?;
        let store = &self.fixture.store;
        store.create_task(spec(name)?, &scheduled("pickup")?, self.clock.now())?;
        let fence = store
            .claim(&task, &scheduled("pickup")?, ttl(600)?, self.clock.now())?
            .fence();
        let AttemptStart::Started(attempt) = store.start_attempt(&task, fence, self.clock.now())?
        else {
            return Err("attempt did not start".into());
        };
        let launched = run_effect(
            store,
            &self.backend,
            &grants()?,
            plan(&task, fence, "launch", launch()?)?,
            &self.clock,
        )?;
        let EffectState::Applied { receipt, .. } = launched.state() else {
            return Err("launch not applied".into());
        };
        let find = |kind| {
            receipt
                .created()
                .iter()
                .find(|resource| resource.kind == kind)
                .cloned()
                .ok_or("missing created resource")
        };
        let worker = find(ResourceKind::Worker)?;
        let worktree = find(ResourceKind::Worktree)?;
        let key = ExternalRef::new(launched.request().key().as_str())?;
        self.backend
            .fake
            .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
        if settle {
            store.finish_attempt(
                &task,
                fence,
                attempt,
                AttemptOutcome::Succeeded,
                self.clock.now(),
            )?;
        }
        let path = self.repo.pushed_worktree(name)?;
        self.backend
            .add(worktree.clone(), Some(key.clone()), Liveness::Exited);
        self.paths.insert(worktree.clone(), path);
        Ok(Owned {
            task,
            worker,
            worktree,
            key,
        })
    }

    fn path(&self, resource: &ResourceRef) -> TestResult<&Path> {
        Ok(self.paths.get(resource).ok_or("no path")?.as_path())
    }

    fn inspect(&self) -> TestResult<Preview> {
        Ok(inspect(
            &self.inspector(),
            InspectionTrigger::Schedule,
            self.clock.now(),
        )?)
    }

    /// A person reviews the preview and approves every step it would take.
    fn approve_all(&self) -> TestResult<Vec<ApprovalResult>> {
        let preview = self.inspect()?;
        let mut digests = Vec::new();
        for entry in &preview.entries {
            if entry.eligible() {
                digests.push(entry.observation.clone());
            }
            if let Some(build) = entry
                .build_output
                .as_ref()
                .filter(|_| entry.build_output_eligible())
            {
                digests.push(build.observation.clone());
            }
        }
        Ok(approve(
            &self.inspector(),
            &interactive("david")?,
            &digests,
            &self.clock,
        )?)
    }

    /// The dishwasher's approval markers.
    fn markers(&self) -> TestResult<Vec<kitchen::state::WorkflowMarker>> {
        Ok(self
            .store()
            .markers(&kitchen::WorkflowId::new("dishwasher")?)?)
    }

    fn reclaim(&self) -> TestResult<BuildReport> {
        Ok(reclaim_build_output(
            &self.inspector(),
            InspectionTrigger::DiskPressure,
            PREVIEW_AGE,
            &self.clock,
        )?)
    }

    fn apply_as(
        &self,
        grants: &HouseGrants,
        claimant: &Claimant,
        consents: &dyn ConsentSource,
    ) -> Result<ApplyReport, Error> {
        apply(
            &self.inspector(),
            grants,
            claimant,
            consents,
            &options().map_err(|_| Error::Cleanup(CleanupError::Encoding))?,
            &self.clock,
        )
    }

    fn apply(&self) -> TestResult<ApplyReport> {
        Ok(self.apply_as(&grants()?, &scheduled("dishwasher")?, &NoConsent)?)
    }
}

fn release_grant() -> TestResult<Grant> {
    Ok(Grant::house(
        Permission::ReleaseResource,
        backend_id()?,
        credential()?,
    ))
}

fn options() -> TestResult<ApplyOptions> {
    Ok(ApplyOptions {
        release: release_grant()?,
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
        lease: ttl(300)?,
        max_approval_age: PREVIEW_AGE,
        max_releases: 16,
    })
}

fn reasons(preview: &Preview, resource: &ResourceRef) -> TestResult<Vec<Exclusion>> {
    match &preview
        .entry(resource)
        .ok_or("resource not previewed")?
        .decision
    {
        Decision::Release => Ok(Vec::new()),
        Decision::Retain { reasons } => Ok(reasons.clone()),
    }
}

fn outcome(report: &ApplyReport, resource: &ResourceRef) -> TestResult<ReleaseOutcome> {
    Ok(report
        .results
        .iter()
        .chain(&report.recovered)
        .find(|result| &result.resource == resource)
        .ok_or("resource not in report")?
        .outcome)
}

// ---------------------------------------------------------------------------
// Preview: eligibility and exclusions

#[test]
fn a_settled_clean_pushed_task_is_eligible_with_its_evidence() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let preview = harness.inspect()?;
    assert_eq!(preview.precheck(), Precheck::Actionable);
    for resource in [&owned.worker, &owned.worktree] {
        let entry = preview.entry(resource).ok_or("missing")?;
        assert_eq!(entry.decision, Decision::Release, "{resource:?}");
        let Ownership::Task(owner) = &entry.ownership else {
            return Err("owner not attributed".into());
        };
        assert_eq!(owner.task, owned.task);
        assert_eq!(owner.attempt.get(), 1);
        assert_eq!(owner.key.as_str(), owned.key.as_str());
    }
    // The preview is reviewable: identity, ownership, evidence, and decision.
    let json = serde_json::to_value(&preview)?;
    let worktree = json["entries"]
        .as_array()
        .ok_or("entries")?
        .iter()
        .find(|entry| entry["resource"]["kind"] == "worktree")
        .ok_or("worktree entry")?;
    assert_eq!(worktree["ownership"]["task"], "task-1");
    assert_eq!(worktree["worktree"]["type"], "read");
    assert_eq!(worktree["worktree"]["state"]["unpushedCommits"], false);
    assert_eq!(worktree["decision"]["type"], "release");
    Ok(())
}

#[test]
fn unknown_and_legacy_resources_are_retained() -> TestResult {
    let harness = Harness::new()?;
    let legacy = ResourceRef {
        kind: ResourceKind::Terminal,
        backend: backend_id()?,
        handle: ExternalRef::new("legacy-terminal")?,
    };
    let orphan = ResourceRef {
        kind: ResourceKind::Worktree,
        backend: backend_id()?,
        handle: ExternalRef::new("orphan-worktree")?,
    };
    harness.backend.add(legacy.clone(), None, Liveness::Exited);
    // An owner record that names no Kitchen task is not ownership evidence.
    harness.backend.add(
        orphan.clone(),
        Some(ExternalRef::new("forgotten")?),
        Liveness::Exited,
    );
    let preview = harness.inspect()?;
    assert_eq!(preview.precheck(), Precheck::Idle);
    assert_eq!(reasons(&preview, &legacy)?, [Exclusion::UnknownOwner]);
    assert_eq!(
        reasons(&preview, &orphan)?,
        [Exclusion::UnknownOwner, Exclusion::WorktreeUnlocated]
    );
    Ok(())
}

#[test]
fn an_unsettled_owner_and_live_worker_retain_the_whole_task() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", false)?;
    harness
        .backend
        .fake
        .set_worker_state(&owned.worker, WorkerState::Ready);
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worker)?,
        [
            Exclusion::OwnerActive,
            Exclusion::InUse,
            Exclusion::WorkerNotSettled
        ]
    );
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::OwnerActive, Exclusion::SiblingInUse]
    );
    Ok(())
}

#[test]
fn a_user_takeover_retains_every_resource_of_the_task() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness
        .backend
        .fake
        .set_worker_state(&owned.worker, WorkerState::UserTakeover);
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worker)?,
        [Exclusion::InUse, Exclusion::UserTakeover]
    );
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::UserTakeover, Exclusion::SiblingInUse]
    );
    Ok(())
}

#[test]
fn dirty_worktrees_are_retained() -> TestResult {
    let mut harness = Harness::new()?;
    let tracked = harness.owner("task-1", true)?;
    let untracked = harness.owner("task-2", true)?;
    fs::write(
        harness.path(&tracked.worktree)?.join("README.md"),
        "edited\n",
    )?;
    fs::write(
        harness.path(&untracked.worktree)?.join("notes.txt"),
        "draft\n",
    )?;
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &tracked.worktree)?,
        [Exclusion::TrackedChanges]
    );
    assert_eq!(
        reasons(&preview, &untracked.worktree)?,
        [Exclusion::UntrackedFiles]
    );
    // The workers themselves are settled and independent of the checkout.
    assert_eq!(reasons(&preview, &tracked.worker)?, []);
    Ok(())
}

#[test]
fn ignored_local_files_keep_the_worktree_and_are_listed() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    exclude(&harness.repo, ".env\n.claude/\n")?;
    fs::write(path.join(".env"), "TOKEN=secret\n")?;
    fs::create_dir_all(path.join(".claude"))?;
    fs::write(path.join(".claude").join("notes.md"), "plan\n")?;
    // `git status` reports neither, so the worktree looks clean.
    let state = inspect_worktree(&path, &GitLimits::default())?;
    assert_eq!((state.tracked_changes, state.untracked_files), (0, 0));

    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::IgnoredFiles]
    );
    assert_eq!(
        ignored_files(&preview, &owned.worktree)?,
        [".claude/", ".env"]
    );
    // The settled worker does not depend on the checkout.
    assert_eq!(reasons(&preview, &owned.worker)?, []);
    Ok(())
}

#[test]
fn proven_build_output_does_not_keep_the_worktree_but_other_ignored_files_do() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    // A tagged, Git-ignored, untracked directory is regenerable.
    build_dir(&path, 1024)?;
    let preview = harness.inspect()?;
    assert_eq!(reasons(&preview, &owned.worktree)?, []);
    assert_eq!(ignored_files(&preview, &owned.worktree)?, [] as [&str; 0]);

    // An ignored file beside it is not, and only that file is listed.
    exclude(&harness.repo, ".env\n")?;
    fs::write(path.join(".env"), "TOKEN=secret\n")?;
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::IgnoredFiles]
    );
    assert_eq!(ignored_files(&preview, &owned.worktree)?, [".env"]);
    // The build output can still be reclaimed: it is not the work.
    let entry = preview.entry(&owned.worktree).ok_or("worktree")?;
    assert!(entry.build_output_eligible());
    Ok(())
}

#[test]
fn ignored_directories_without_a_cache_tag_are_not_build_output() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    exclude(&harness.repo, "node_modules/\n")?;
    fs::create_dir_all(path.join("node_modules").join("pkg"))?;
    fs::write(path.join("node_modules").join("pkg").join("index.js"), "x")?;
    // `target/` is ignored by the repository but carries no cache tag.
    fs::create_dir_all(path.join("target"))?;
    fs::write(path.join("target").join("notes"), "kept by hand\n")?;
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::IgnoredFiles]
    );
    assert_eq!(
        ignored_files(&preview, &owned.worktree)?,
        ["node_modules/", "target/"]
    );
    Ok(())
}

#[test]
fn files_ignored_by_a_global_excludes_file_keep_the_worktree() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    let global = harness.repo.dir.path().join("global-ignore");
    fs::write(&global, "secret.txt\n")?;
    git(
        &harness.repo.main(),
        &["config", "core.excludesFile", path_str(&global)?],
    )?;
    fs::write(path.join("secret.txt"), "keep me\n")?;
    let state = inspect_worktree(&path, &GitLimits::default())?;
    assert_eq!((state.tracked_changes, state.untracked_files), (0, 0));
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::IgnoredFiles]
    );
    assert_eq!(ignored_files(&preview, &owned.worktree)?, ["secret.txt"]);
    Ok(())
}

#[test]
fn an_ignored_nested_repository_with_unpushed_commits_keeps_the_worktree() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    exclude(&harness.repo, "scratch-repo/\n")?;
    let nested = path.join("scratch-repo");
    fs::create_dir_all(&nested)?;
    git(&nested, &["init", "--quiet"])?;
    commit_locally(&nested, "only-here.txt")?;
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::IgnoredFiles]
    );
    assert_eq!(ignored_files(&preview, &owned.worktree)?, ["scratch-repo/"]);
    Ok(())
}

#[test]
fn edits_hidden_by_index_flags_keep_the_worktree() -> TestResult {
    let mut harness = Harness::new()?;
    let assumed = harness.owner("task-1", true)?;
    let skipped = harness.owner("task-2", true)?;
    let plain = harness.owner("task-3", true)?;
    for (owned, flag) in [
        (&assumed, "--assume-unchanged"),
        (&skipped, "--skip-worktree"),
    ] {
        let path = harness.path(&owned.worktree)?.to_path_buf();
        git(&path, &["update-index", flag, "README.md"])?;
        fs::write(path.join("README.md"), "edited where status cannot see\n")?;
        let state = inspect_worktree(&path, &GitLimits::default())?;
        assert_eq!(state.tracked_changes, 0, "status reports the edit: {flag}");
        assert_eq!(state.hidden_tracked, 1);
    }
    let preview = harness.inspect()?;
    for owned in [&assumed, &skipped] {
        assert_eq!(
            reasons(&preview, &owned.worktree)?,
            [Exclusion::HiddenTrackedFiles]
        );
    }
    assert_eq!(reasons(&preview, &plain.worktree)?, []);
    Ok(())
}

#[test]
fn too_many_ignored_paths_make_the_worktree_unreadable() -> TestResult {
    let repo = Repo::new()?;
    let path = repo.pushed_worktree("many")?;
    // Ignored files inside a tracked directory are listed one by one.
    fs::create_dir_all(path.join("logs"))?;
    fs::write(path.join("logs").join("keep.txt"), "tracked\n")?;
    git(&path, &["add", "logs"])?;
    git(&path, &["commit", "--quiet", "-m", "logs"])?;
    git(&path, &["push", "--quiet", "origin", "many"])?;
    exclude(&repo, "*.log\n")?;
    for index in 0..300 {
        fs::write(path.join("logs").join(format!("run-{index}.log")), "x")?;
    }
    assert_eq!(
        inspect_worktree(&path, &GitLimits::default()),
        Err(GitReadError::OutputTooLarge)
    );
    Ok(())
}

#[test]
fn unpushed_commits_need_the_merged_head_to_be_preserved() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    commit_locally(&path, "local.txt")?;
    assert_eq!(
        reasons(&harness.inspect()?, &owned.worktree)?,
        [Exclusion::UnpreservedCommits]
    );
    // A squash merge of a different head does not preserve this one.
    harness.merged.insert(owned.worktree.clone(), commit('c')?);
    assert_eq!(
        reasons(&harness.inspect()?, &owned.worktree)?,
        [Exclusion::UnpreservedCommits]
    );
    // The merged pull request's head is exactly the local head.
    harness.merged.insert(owned.worktree.clone(), head(&path)?);
    assert_eq!(reasons(&harness.inspect()?, &owned.worktree)?, []);
    // A merged pull request alone never discards later local changes.
    fs::write(path.join("after-merge.txt"), "new\n")?;
    assert_eq!(
        reasons(&harness.inspect()?, &owned.worktree)?,
        [Exclusion::UntrackedFiles]
    );
    Ok(())
}

#[test]
fn main_checkouts_locked_unreadable_and_unlocated_worktrees_are_retained() -> TestResult {
    let mut harness = Harness::new()?;
    let main = harness.owner("task-1", true)?;
    let locked = harness.owner("task-2", true)?;
    let unreadable = harness.owner("task-3", true)?;
    let unlocated = harness.owner("task-4", true)?;
    harness
        .paths
        .insert(main.worktree.clone(), harness.repo.main());
    git(
        &harness.repo.main(),
        &[
            "worktree",
            "lock",
            path_str(harness.path(&locked.worktree)?)?,
        ],
    )?;
    let subdirectory = harness.path(&unreadable.worktree)?.join("nested");
    fs::create_dir_all(&subdirectory)?;
    harness
        .paths
        .insert(unreadable.worktree.clone(), subdirectory);
    harness.paths.remove(&unlocated.worktree);
    let preview = harness.inspect()?;
    assert_eq!(
        reasons(&preview, &main.worktree)?,
        [Exclusion::MainCheckout]
    );
    assert_eq!(
        reasons(&preview, &locked.worktree)?,
        [Exclusion::WorktreeLocked]
    );
    assert_eq!(
        reasons(&preview, &unreadable.worktree)?,
        [Exclusion::WorktreeUnreadable]
    );
    assert_eq!(
        reasons(&preview, &unlocated.worktree)?,
        [Exclusion::WorktreeUnlocated]
    );
    Ok(())
}

#[test]
fn a_reused_identifier_with_another_owner_is_retained() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    // The backend now reports the same handle owned by another request.
    harness.backend.extra.borrow_mut().clear();
    harness.backend.add(
        owned.worktree.clone(),
        Some(ExternalRef::new("another-request")?),
        Liveness::Exited,
    );
    assert_eq!(
        reasons(&harness.inspect()?, &owned.worktree)?,
        [Exclusion::BackendOwnerMismatch]
    );
    Ok(())
}

#[test]
fn a_resource_given_to_an_active_task_is_retained() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let mut repair = spec("repair-1")?;
    repair.resources.insert(owned.worktree.clone());
    harness
        .store()
        .create_task(repair, &scheduled("pickup")?, harness.clock.now())?;
    assert_eq!(
        reasons(&harness.inspect()?, &owned.worktree)?,
        [Exclusion::SharedWithTask]
    );
    Ok(())
}

#[test]
fn disk_pressure_starts_an_inspection_but_changes_no_decision() -> TestResult {
    let mut harness = Harness::new()?;
    harness.owner("task-1", true)?;
    let dirty = harness.owner("task-2", true)?;
    fs::write(harness.path(&dirty.worktree)?.join("wip.txt"), "wip\n")?;
    let scheduled_run = harness.inspect()?;
    let pressured = inspect(
        &harness.inspector(),
        InspectionTrigger::DiskPressure,
        harness.clock.now(),
    )?;
    assert_eq!(pressured.trigger, InspectionTrigger::DiskPressure);
    assert_eq!(pressured.entries, scheduled_run.entries);
    Ok(())
}

#[test]
fn an_empty_inventory_is_idle() -> TestResult {
    let harness = Harness::new()?;
    let preview = harness.inspect()?;
    assert_eq!(preview.precheck(), Precheck::Idle);
    assert!(preview.entries.is_empty());
    Ok(())
}

#[test]
fn backend_failures_are_errors_not_idle() -> TestResult {
    let harness = Harness::new()?;
    harness.backend.outage.set(true);
    let error = inspect(
        &harness.inspector(),
        InspectionTrigger::Schedule,
        harness.clock.now(),
    )
    .err()
    .ok_or("outage reported as a preview")?;
    assert!(matches!(
        error,
        Error::Cleanup(CleanupError::Backend(BackendUnavailable::Timeout))
    ));
    assert_eq!(error.class(), ErrorClass::Execution);

    // A backend without inventory is refused by capability, before any read.
    let limited = Harness::with_backend(FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting([Capability::WorkerStatusAndOutcome]),
    ))?;
    let error = inspect(
        &limited.inspector(),
        InspectionTrigger::Schedule,
        limited.clock.now(),
    )
    .err()
    .ok_or("missing capability accepted")?;
    assert_eq!(error.class(), ErrorClass::Refused);
    assert_eq!(limited.backend.calls.get(), 0);
    Ok(())
}

#[test]
fn a_duplicated_inventory_entry_is_an_error() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness
        .backend
        .add(owned.worktree, Some(owned.key), Liveness::Exited);
    let error = harness.inspect().err().ok_or("duplicate accepted")?;
    assert!(error.to_string().contains("more than once"), "{error}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Apply: grant, preview-first, revalidation, recovery, repetition

#[test]
fn the_first_run_is_preview_only_and_repeating_is_harmless() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let before = harness.backend.fake.effects_performed();

    // Nobody has approved anything, so nothing runs and nothing is written.
    let first = harness.apply()?;
    assert_eq!(outcome(&first, &owned.worker)?, ReleaseOutcome::NotApproved);
    assert_eq!(
        outcome(&first, &owned.worktree)?,
        ReleaseOutcome::NotApproved
    );
    assert_eq!(harness.backend.fake.effects_performed(), before);
    assert!(harness.markers()?.is_empty());

    harness.approve_all()?;
    harness.clock.advance(60);
    let second = harness.apply()?;
    assert_eq!(outcome(&second, &owned.worker)?, ReleaseOutcome::Released);
    assert_eq!(outcome(&second, &owned.worktree)?, ReleaseOutcome::Released);
    assert_eq!(harness.backend.fake.effects_performed(), before + 2);
    // The worktree's release reports the space measured before it.
    let freed = second
        .results
        .iter()
        .find(|result| result.resource == owned.worktree)
        .and_then(|result| result.freed)
        .ok_or("no freed space reported")?;
    assert!(freed.complete && freed.bytes > 0);
    // Each release ran as a settled dishwasher task given that resource.
    for result in &second.results {
        let task = harness.store().task(result.task.as_ref().ok_or("task")?)?;
        assert!(task.spec().resources.contains(&result.resource));
        assert_eq!(task.effects().len(), 1);
    }

    harness.clock.advance(60);
    let third = harness.apply()?;
    assert!(third.results.is_empty() && third.recovered.is_empty());
    assert_eq!(third.preview.precheck(), Precheck::Idle);
    assert_eq!(harness.backend.fake.effects_performed(), before + 2);
    Ok(())
}

#[test]
fn a_scheduled_run_cannot_approve_its_own_preview() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let before = harness.backend.fake.effects_performed();
    // The same standing claimant inspects and applies, tick after tick.
    for _ in 0..3 {
        let report = harness.apply()?;
        assert_eq!(
            outcome(&report, &owned.worker)?,
            ReleaseOutcome::NotApproved
        );
        assert_eq!(
            outcome(&report, &owned.worktree)?,
            ReleaseOutcome::NotApproved
        );
        harness.clock.advance(60);
    }
    assert_eq!(harness.backend.fake.effects_performed(), before);
    assert!(harness.markers()?.is_empty(), "apply records no approval");
    // Build output under disk pressure is gated the same way.
    build_dir(harness.path(&owned.worktree)?, 1024)?;
    let reclaimed = harness.reclaim()?;
    assert_eq!(
        reclaimed
            .results
            .iter()
            .map(|r| r.outcome)
            .collect::<Vec<_>>(),
        [BuildOutcome::NotApproved]
    );
    assert!(harness.path(&owned.worktree)?.join("target").is_dir());
    Ok(())
}

#[test]
fn only_a_person_can_record_an_approval() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let digest = harness
        .inspect()?
        .entry(&owned.worktree)
        .ok_or("worktree")?
        .observation
        .clone();
    let error = approve(
        &harness.inspector(),
        &scheduled("dishwasher")?,
        std::slice::from_ref(&digest),
        &harness.clock,
    )
    .err()
    .ok_or("a scheduled claimant approved")?;
    assert!(matches!(
        error,
        Error::Cleanup(CleanupError::ApprovalNeedsPerson)
    ));
    assert_eq!(error.class(), ErrorClass::Refused);
    assert!(harness.markers()?.is_empty());
    let report = harness.apply()?;
    assert_eq!(
        outcome(&report, &owned.worktree)?,
        ReleaseOutcome::NotApproved
    );
    Ok(())
}

#[test]
fn a_marker_a_person_did_not_record_approves_nothing() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let worker = harness.inspect()?;
    let worker_digest = worker
        .entry(&owned.worker)
        .ok_or("worker")?
        .observation
        .clone();
    let worktree_digest = worker
        .entry(&owned.worktree)
        .ok_or("worktree")?
        .observation
        .clone();
    let key = |resource: &ResourceRef, digest: &ExternalRef| -> TestResult<MarkerKey> {
        Ok(MarkerKey {
            workflow: kitchen::WorkflowId::new("dishwasher")?,
            item: WorkItem::Resource {
                resource: resource.clone(),
            },
            subject: MarkerSubject::Observation(digest.clone()),
        })
    };
    let schema = MarkerSchema::new("cleanup.approval", NonZeroU32::MIN)?;
    let approval = |step: &str| {
        MarkerFact::workflow(
            schema.clone(),
            &serde_json::json!({
                "step": step,
                "owner": "task-1",
                "approvedAt": harness.clock.now().as_unix_millis(),
            }),
        )
    };
    // A well-formed approval recorded by a scheduled claimant, as if the
    // automation had written it for itself.
    harness.store().record_marker(
        key(&owned.worker, &worker_digest)?,
        approval("release")?,
        &scheduled("dishwasher")?,
        harness.clock.now(),
    )?;
    // A person's marker, but for a different step than the key's evidence.
    harness.store().record_marker(
        key(&owned.worktree, &worktree_digest)?,
        approval("build-output")?,
        &interactive("david")?,
        harness.clock.now(),
    )?;
    let before = harness.backend.fake.effects_performed();
    harness.clock.advance(60);
    let report = harness.apply()?;
    assert_eq!(
        outcome(&report, &owned.worker)?,
        ReleaseOutcome::NotApproved
    );
    assert_eq!(
        outcome(&report, &owned.worktree)?,
        ReleaseOutcome::NotApproved
    );
    assert_eq!(harness.backend.fake.effects_performed(), before);
    Ok(())
}

#[test]
fn an_approval_names_exact_current_evidence() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let dirty = harness.owner("task-2", true)?;
    fs::write(harness.path(&dirty.worktree)?.join("wip.txt"), "wip\n")?;
    let preview = harness.inspect()?;
    let release = preview
        .entry(&owned.worktree)
        .ok_or("worktree")?
        .observation
        .clone();
    let retained = preview
        .entry(&dirty.worktree)
        .ok_or("dirty")?
        .observation
        .clone();
    let unknown = ExternalRef::new("sha256:0000")?;
    let results = approve(
        &harness.inspector(),
        &interactive("david")?,
        &[release.clone(), retained, unknown.clone()],
        &harness.clock,
    )?;
    assert_eq!(
        results
            .iter()
            .map(|r| r.outcome.clone())
            .collect::<Vec<_>>(),
        [
            ApprovalOutcome::Approved {
                resource: owned.worktree.clone(),
                step: Step::Release
            },
            ApprovalOutcome::NotCurrent,
            ApprovalOutcome::NotCurrent,
        ]
    );
    // Only the approved step is recorded, by the person who approved it.
    let markers = harness.markers()?;
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].recorded_by(), &interactive("david")?);

    // Approving again renews the one marker rather than adding another.
    harness.clock.advance(10);
    approve(
        &harness.inspector(),
        &interactive("david")?,
        &[release],
        &harness.clock,
    )?;
    let markers = harness.markers()?;
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].history().len(), 1);

    // Evidence that changes after the approval needs a new one.
    fs::write(harness.path(&owned.worktree)?.join("resumed.txt"), "wip\n")?;
    let report = harness.apply()?;
    assert!(
        report
            .results
            .iter()
            .all(|result| result.resource != owned.worktree)
    );
    assert_eq!(
        reasons(&report.preview, &owned.worktree)?,
        [Exclusion::UntrackedFiles]
    );
    Ok(())
}

#[test]
fn changed_evidence_after_the_approval_is_not_released() -> TestResult {
    let mut harness = Harness::new()?;
    let dirty = harness.owner("task-1", true)?;
    let moved = harness.owner("task-2", true)?;
    harness.approve_all()?;
    let before = harness.backend.fake.effects_performed();
    // Someone resumes work in one worktree and pushes a new commit in another.
    fs::write(harness.path(&dirty.worktree)?.join("resumed.txt"), "wip\n")?;
    let path = harness.path(&moved.worktree)?.to_path_buf();
    commit_locally(&path, "more.txt")?;
    git(&path, &["push", "--quiet", "origin", "task-2"])?;
    harness.clock.advance(60);
    let report = harness.apply()?;
    assert_eq!(
        reasons(&report.preview, &dirty.worktree)?,
        [Exclusion::UntrackedFiles]
    );
    assert!(
        report
            .results
            .iter()
            .all(|result| result.resource != dirty.worktree)
    );
    // New evidence needs its own approval first.
    assert_eq!(
        outcome(&report, &moved.worktree)?,
        ReleaseOutcome::NotApproved
    );
    // Only the two untouched workers were released.
    assert_eq!(harness.backend.fake.effects_performed(), before + 2);
    Ok(())
}

#[test]
fn an_expired_approval_is_not_applied_until_a_person_renews_it() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(PREVIEW_AGE.as_secs() + 1);
    let before = harness.backend.fake.effects_performed();
    let stale = harness.apply()?;
    assert_eq!(
        outcome(&stale, &owned.worktree)?,
        ReleaseOutcome::ApprovalExpired
    );
    assert_eq!(harness.backend.fake.effects_performed(), before);
    // Applying again does not renew it.
    let again = harness.apply()?;
    assert_eq!(
        outcome(&again, &owned.worktree)?,
        ReleaseOutcome::ApprovalExpired
    );
    harness.approve_all()?;
    harness.clock.advance(1);
    let fresh = harness.apply()?;
    assert_eq!(outcome(&fresh, &owned.worktree)?, ReleaseOutcome::Released);
    Ok(())
}

#[test]
fn an_ignored_file_keeps_its_worktree_even_with_an_approval() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    exclude(&harness.repo, ".env\n")?;
    fs::write(path.join(".env"), "TOKEN=secret\n")?;
    let approved = harness.approve_all()?;
    // Only the worker is offered; the worktree is retained and never approved.
    assert_eq!(approved.len(), 1);
    harness.clock.advance(60);
    let report = harness.apply()?;
    assert_eq!(outcome(&report, &owned.worker)?, ReleaseOutcome::Released);
    assert!(
        report
            .results
            .iter()
            .all(|result| result.resource != owned.worktree)
    );
    assert!(path.join(".env").is_file());
    assert!(
        harness
            .backend
            .extra
            .borrow()
            .iter()
            .any(|observation| observation.resource == owned.worktree)
    );
    Ok(())
}

#[test]
fn a_full_marker_table_stops_new_approvals_but_not_recovery() -> TestResult {
    let mut harness = Harness::new()?;
    let first = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    harness
        .backend
        .fake
        .inject(ExecuteFault::TimeoutWithoutApplying);
    let interrupted = harness.apply()?;
    assert_eq!(
        outcome(&interrupted, &first.worker)?,
        ReleaseOutcome::Uncertain
    );
    // Unrelated workflows fill the house's shared marker table.
    let path = harness.fixture.state_path();
    let mut state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let template = state["markers"][0].clone();
    if let Some(list) = state["markers"].as_array_mut() {
        for number in 0..(kitchen::state::MAX_MARKERS - list.len()) {
            let mut marker = template.clone();
            marker["key"]["item"]["resource"]["handle"] = format!("filler-{number}").into();
            list.push(marker);
        }
    }
    fs::write(&path, serde_json::to_vec(&state)?)?;
    // Another resource becomes eligible while the earlier release is unresolved.
    let second = harness.owner("task-2", true)?;
    harness.clock.advance(60);

    // Approving it fails closed with the capacity error and records nothing.
    let error = harness
        .approve_all()
        .err()
        .ok_or("approved into a full table")?;
    assert!(error.to_string().contains("limit reached"), "{error}");
    // Apply still reconciles the interrupted release and reports the new
    // resource as not approved instead of failing.
    let report = harness.apply()?;
    assert_eq!(outcome(&report, &first.worker)?, ReleaseOutcome::Released);
    assert_eq!(
        outcome(&report, &second.worker)?,
        ReleaseOutcome::NotApproved
    );
    assert!(harness.markers()?.iter().all(|marker| !matches!(
        marker.key().item,
        WorkItem::Resource { ref resource } if resource == &second.worker
    )));
    Ok(())
}

#[test]
fn an_owner_change_just_before_the_effect_refuses_the_release() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    // After apply's first inventory read, the backend reassigns the worktree
    // while the worker is being released.
    let worktree = owned.worktree.clone();
    harness.backend.change_after(1, move |extra| {
        for observation in extra.iter_mut() {
            if observation.resource == worktree {
                observation.owner = ExternalRef::new("reassigned").ok();
            }
        }
    });
    let before = harness.backend.fake.effects_performed();
    let report = harness.apply()?;
    let changed: Vec<_> = report
        .results
        .iter()
        .filter(|result| result.outcome == ReleaseOutcome::Changed)
        .collect();
    assert_eq!(changed.len(), 1, "{:?}", report.results);
    let task = harness.store().task(
        changed
            .first()
            .and_then(|result| result.task.as_ref())
            .ok_or("task")?,
    )?;
    assert!(task.effects().is_empty(), "no effect was attempted");
    assert!(
        harness
            .backend
            .extra
            .borrow()
            .iter()
            .any(|o| o.resource == owned.worktree)
    );
    // Whatever else was released, the reassigned worktree was not.
    assert!(harness.backend.fake.effects_performed() <= before + 1);
    Ok(())
}

#[test]
fn release_needs_the_separate_grant() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    let before = harness.backend.fake.effects_performed();
    // The house grants worker lifecycle permissions but not release.
    let without = grants_for(
        house()?,
        &WORKER_PERMISSIONS
            .into_iter()
            .filter(|permission| *permission != Permission::ReleaseResource)
            .collect::<Vec<_>>(),
    )?;
    let error = harness
        .apply_as(&without, &scheduled("dishwasher")?, &NoConsent)
        .err()
        .ok_or("release without a grant")?;
    assert_eq!(error.class(), ErrorClass::Refused);
    // A grant for another backend is invalid input.
    let mut foreign = options()?;
    foreign.release.destination = kitchen::BackendId::new("elsewhere")?;
    let error = apply(
        &harness.inspector(),
        &grants()?,
        &scheduled("dishwasher")?,
        &NoConsent,
        &foreign,
        &harness.clock,
    )
    .err()
    .ok_or("foreign grant accepted")?;
    assert!(matches!(error, Error::Cleanup(CleanupError::GrantMismatch)));
    assert_eq!(harness.backend.fake.effects_performed(), before);
    assert!(
        harness
            .backend
            .extra
            .borrow()
            .iter()
            .any(|o| o.resource == owned.worktree)
    );
    Ok(())
}

#[test]
fn an_interrupted_release_is_reconciled_not_repeated() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    let before = harness.backend.fake.effects_performed();
    // The first release is applied but its response is lost.
    harness
        .backend
        .fake
        .inject(ExecuteFault::ApplyThenLoseResponse);
    let partial = harness.apply()?;
    let uncertain = partial
        .results
        .iter()
        .find(|result| result.outcome == ReleaseOutcome::Uncertain)
        .ok_or("no uncertain release")?;
    let task = uncertain.task.clone().ok_or("task")?;
    let resource = uncertain.resource.clone();
    assert_eq!(harness.backend.fake.effects_performed(), before + 2);

    // Another session holds the interrupted task: the next run leaves it.
    let other = scheduled("other-dishwasher")?;
    let lease = harness
        .store()
        .claim(&task, &other, ttl(30)?, harness.clock.now())?;
    harness.clock.advance(1);
    let held = harness.apply()?;
    assert_eq!(outcome(&held, &resource)?, ReleaseOutcome::HeldElsewhere);

    // Once that claim expires, the next run reconciles by lookup.
    harness.clock.advance(60);
    assert!(!lease.is_live(harness.clock.now()));
    let recovered = harness.apply()?;
    assert_eq!(outcome(&recovered, &resource)?, ReleaseOutcome::Released);
    assert_eq!(harness.backend.fake.effects_performed(), before + 2);
    assert!(matches!(
        harness.store().task(&task)?.state(),
        kitchen::state::TaskState::Settled { .. }
    ));
    let _ = owned;
    Ok(())
}

/// A person approving every release they are asked about.
struct Approve(kitchen::HouseId);

impl ConsentSource for Approve {
    fn consent(
        &self,
        task: &TaskId,
        effect: &Effect,
        revision: EvidenceRevision,
    ) -> Option<Consent> {
        Some(Consent {
            id: ExternalRef::new(&format!("consent-{task}")).ok()?,
            given_by: kitchen::HolderId::new("david").ok()?,
            house: self.0.clone(),
            task: task.clone(),
            effect: effect.clone(),
            revision,
        })
    }
}

#[test]
fn interactive_release_needs_consent_for_each_resource() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    let before = harness.backend.fake.effects_performed();
    let session = interactive("session")?;
    let refused = harness.apply_as(&grants()?, &session, &NoConsent)?;
    assert_eq!(
        outcome(&refused, &owned.worktree)?,
        ReleaseOutcome::ConsentMissing
    );
    assert_eq!(harness.backend.fake.effects_performed(), before);
    let approved = harness.apply_as(&grants()?, &session, &Approve(house()?))?;
    assert_eq!(
        outcome(&approved, &owned.worktree)?,
        ReleaseOutcome::Released
    );
    assert_eq!(outcome(&approved, &owned.worker)?, ReleaseOutcome::Released);
    Ok(())
}

// ---------------------------------------------------------------------------
// The Git reader's bounds

#[cfg(unix)]
#[test]
fn a_hung_git_call_is_killed_at_its_deadline() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("git");
    fs::write(&script, "#!/bin/sh\nsleep 30\n")?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let limits = GitLimits {
        program: script,
        call_timeout: Duration::from_millis(200),
        max_output_bytes: 1024,
    };
    let started = std::time::Instant::now();
    assert_eq!(
        inspect_worktree(dir.path(), &limits),
        Err(GitReadError::Timeout)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    Ok(())
}

#[test]
fn oversized_git_output_is_refused() -> TestResult {
    let repo = Repo::new()?;
    let path = repo.pushed_worktree("big")?;
    for index in 0..64 {
        fs::write(path.join(format!("untracked-{index}.txt")), "x")?;
    }
    let limits = GitLimits {
        max_output_bytes: 256,
        ..GitLimits::default()
    };
    assert_eq!(
        inspect_worktree(&path, &limits),
        Err(GitReadError::OutputTooLarge)
    );
    let state = inspect_worktree(&path, &GitLimits::default())?;
    assert_eq!(state.untracked_files, 64);
    assert!(state.linked && !state.locked && !state.unpushed_commits);
    Ok(())
}

#[test]
fn a_repository_without_remotes_has_unpushed_commits() -> TestResult {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet"])?;
    fs::write(dir.path().join("a.txt"), "a")?;
    git(dir.path(), &["add", "."])?;
    git(dir.path(), &["commit", "--quiet", "-m", "a"])?;
    let state = inspect_worktree(dir.path(), &GitLimits::default())?;
    assert!(state.unpushed_commits && !state.linked);
    assert_eq!(state.head, head(dir.path())?);
    Ok(())
}

#[test]
fn a_directory_that_is_not_a_repository_is_unreadable() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert_eq!(
        inspect_worktree(dir.path(), &GitLimits::default()),
        Err(GitReadError::Failed)
    );
    Ok(())
}

#[test]
fn an_unresolved_release_blocks_any_new_release_of_that_resource() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    // The worker's release times out and its outcome cannot be looked up.
    harness
        .backend
        .fake
        .inject(ExecuteFault::TimeoutWithoutApplying);
    harness.backend.fake.fail_lookups(usize::MAX);
    let first = harness.apply()?;
    assert_eq!(outcome(&first, &owned.worker)?, ReleaseOutcome::Uncertain);
    let calls = harness.backend.fake.execute_calls();

    // The approval expires and a person renews it, which plans a new release
    // task.
    harness.clock.advance(PREVIEW_AGE.as_secs() + 1);
    let expired = harness.apply()?;
    assert_eq!(
        outcome(&expired, &owned.worker)?,
        ReleaseOutcome::ApprovalExpired
    );
    harness.approve_all()?;
    harness.clock.advance(60);
    let blocked = harness.apply()?;
    let result = blocked
        .results
        .iter()
        .find(|result| result.resource == owned.worker)
        .ok_or("worker not planned")?;
    assert_eq!(result.outcome, ReleaseOutcome::Uncertain);
    assert_eq!(
        harness.backend.fake.execute_calls(),
        calls,
        "no second release while the first is unresolved"
    );

    // Once the backend proves the first release never applied, the old task
    // settles and the new one releases.
    harness.backend.fake.fail_lookups(0);
    harness.clock.advance(60);
    let resolved = harness.apply()?;
    let old = resolved
        .recovered
        .iter()
        .find(|result| result.resource == owned.worker)
        .ok_or("old task not recovered")?;
    assert_eq!(
        old.outcome,
        ReleaseOutcome::NotApplied(kitchen::contracts::NotAppliedReason::ConfirmedAbsent)
    );
    let new = resolved
        .results
        .iter()
        .find(|result| result.resource == owned.worker)
        .ok_or("new task not driven")?;
    assert_eq!(new.outcome, ReleaseOutcome::Released);
    assert_ne!(old.task, new.task);
    Ok(())
}

#[test]
fn a_release_proven_absent_is_retried_within_its_task() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    harness
        .backend
        .fake
        .inject(ExecuteFault::TimeoutWithoutApplying);
    let first = harness.apply()?;
    assert_eq!(outcome(&first, &owned.worker)?, ReleaseOutcome::Uncertain);
    harness.clock.advance(60);
    // The lookup proves absence, so the same task revalidates and releases.
    let second = harness.apply()?;
    let result = second
        .results
        .iter()
        .find(|result| result.resource == owned.worker)
        .ok_or("worker")?;
    assert_eq!(result.outcome, ReleaseOutcome::Released);
    let task = harness.store().task(result.task.as_ref().ok_or("task")?)?;
    assert_eq!(task.effects().len(), 2, "one absent, one applied");
    assert_eq!(task.attempts().len(), 2);
    Ok(())
}

// ---------------------------------------------------------------------------
// Build output under disk pressure

#[test]
fn build_output_of_settled_workers_is_reclaimed_without_touching_work() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    // Unfinished work keeps the worktree but not its build output.
    commit_locally(&path, "local.txt")?;
    fs::write(path.join("notes.txt"), "draft\n")?;
    let target = build_dir(&path, 64 * 1024)?;
    let preview = harness.inspect()?;
    let entry = preview.entry(&owned.worktree).ok_or("worktree")?;
    assert_eq!(
        reasons(&preview, &owned.worktree)?,
        [Exclusion::UntrackedFiles, Exclusion::UnpreservedCommits]
    );
    let build = entry.build_output.as_ref().ok_or("no build output")?;
    assert_eq!(build.decision, Decision::Release);
    assert_eq!(build.directories.len(), 1);
    assert!(build.usage().bytes >= 64 * 1024);
    assert_eq!(preview.precheck(), Precheck::Actionable);

    // The first run under disk pressure only previews: nobody approved it.
    let first = harness.reclaim()?;
    assert_eq!(
        first.results.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        [BuildOutcome::NotApproved]
    );
    assert!(target.is_dir());

    // A person approves the build output; the worktree itself stays retained
    // and is never offered for release.
    let steps: Vec<Step> = harness
        .approve_all()?
        .into_iter()
        .filter_map(|result| match result.outcome {
            ApprovalOutcome::Approved { resource, step } if resource == owned.worktree => {
                Some(step)
            }
            ApprovalOutcome::Approved { .. } | ApprovalOutcome::NotCurrent => None,
        })
        .collect();
    assert_eq!(steps, [Step::BuildOutput]);
    harness.clock.advance(60);
    let second = harness.reclaim()?;
    let result = second.results.first().ok_or("no result")?;
    assert_eq!(result.outcome, BuildOutcome::Removed);
    assert_eq!(result.directory, "target");
    assert!(second.freed().bytes >= 64 * 1024);
    assert!(!target.exists());
    // Work and the worktree are untouched, and nothing went to the backend.
    assert!(path.join("notes.txt").is_file() && path.join("local.txt").is_file());
    assert_eq!(harness.backend.fake.execute_calls(), 1, "only the launch");

    harness.clock.advance(60);
    assert!(harness.reclaim()?.results.is_empty());
    Ok(())
}

#[test]
fn build_output_is_kept_while_its_owner_is_in_use_taken_over_or_unknown() -> TestResult {
    let mut harness = Harness::new()?;
    let running = harness.owner("task-1", true)?;
    let taken = harness.owner("task-2", true)?;
    for owned in [&running, &taken] {
        build_dir(harness.path(&owned.worktree)?, 1024)?;
    }
    harness
        .backend
        .fake
        .set_worker_state(&running.worker, WorkerState::Ready);
    harness
        .backend
        .fake
        .set_worker_state(&taken.worker, WorkerState::UserTakeover);
    let orphan = ResourceRef {
        kind: ResourceKind::Worktree,
        backend: backend_id()?,
        handle: ExternalRef::new("orphan")?,
    };
    let orphan_path = harness.repo.pushed_worktree("orphan")?;
    build_dir(&orphan_path, 1024)?;
    harness.backend.add(orphan.clone(), None, Liveness::Exited);
    harness.paths.insert(orphan.clone(), orphan_path.clone());

    let preview = harness.inspect()?;
    let build_reasons = |resource: &ResourceRef| -> TestResult<Vec<Exclusion>> {
        let entry = preview.entry(resource).ok_or("missing")?;
        match &entry
            .build_output
            .as_ref()
            .ok_or("no build output")?
            .decision
        {
            Decision::Release => Ok(Vec::new()),
            Decision::Retain { reasons } => Ok(reasons.clone()),
        }
    };
    assert_eq!(build_reasons(&running.worktree)?, [Exclusion::SiblingInUse]);
    assert_eq!(
        build_reasons(&taken.worktree)?,
        [Exclusion::UserTakeover, Exclusion::SiblingInUse]
    );
    assert_eq!(build_reasons(&orphan)?, [Exclusion::UnknownOwner]);
    harness.approve_all()?;
    harness.clock.advance(60);
    assert!(harness.reclaim()?.results.is_empty());
    assert!(orphan_path.join("target").is_dir());
    Ok(())
}

#[test]
fn only_ignored_untracked_tagged_directories_are_build_output() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    // Ignored but untagged.
    fs::create_dir_all(path.join("target"))?;
    fs::write(path.join("target").join("file"), "x")?;
    // Tagged but not ignored.
    fs::create_dir_all(path.join("cache"))?;
    fs::write(path.join("cache").join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE)?;
    git(&path, &["add", "cache"])?;
    git(&path, &["commit", "--quiet", "-m", "tracked cache"])?;
    git(&path, &["push", "--quiet", "origin", "task-1"])?;
    let entry_has_build = |harness: &Harness| -> TestResult<bool> {
        Ok(harness
            .inspect()?
            .entry(&owned.worktree)
            .ok_or("worktree")?
            .build_output
            .is_some())
    };
    assert!(!entry_has_build(&harness)?);
    // Tagged and ignored, but holding a force-added tracked file.
    fs::write(path.join("target").join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE)?;
    git(&path, &["add", "--force", "target/file"])?;
    git(&path, &["commit", "--quiet", "-m", "tracked output"])?;
    git(&path, &["push", "--quiet", "origin", "task-1"])?;
    assert!(!entry_has_build(&harness)?);
    #[cfg(unix)]
    {
        // A symlink to a tagged directory elsewhere is never followed.
        git(&path, &["rm", "--quiet", "-r", "--cached", "target"])?;
        git(&path, &["commit", "--quiet", "-m", "untrack"])?;
        git(&path, &["push", "--quiet", "origin", "task-1"])?;
        fs::remove_dir_all(path.join("target"))?;
        let outside = tempfile::tempdir()?;
        let elsewhere = build_dir(outside.path(), 1024)?;
        std::os::unix::fs::symlink(&elsewhere, path.join("target"))?;
        assert!(!entry_has_build(&harness)?);
        harness.paths.insert(owned.worktree.clone(), path.clone());
        harness.approve_all()?;
        harness.clock.advance(60);
        assert!(harness.reclaim()?.results.is_empty());
        assert!(elsewhere.join("CACHEDIR.TAG").is_file());
    }
    Ok(())
}

#[test]
fn a_directory_named_like_pathspec_magic_is_checked_literally() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let path = harness.path(&owned.worktree)?.to_path_buf();
    // As a pathspec, `:(top)target/` names the ignored top-level `target/`,
    // not this directory, which is neither ignored nor free of tracked files.
    let odd = path.join(":(top)target");
    fs::create_dir_all(&odd)?;
    fs::write(odd.join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE)?;
    fs::write(odd.join("kept.txt"), "tracked\n")?;
    git(
        &path,
        &["--literal-pathspecs", "add", "--", ":(top)target/kept.txt"],
    )?;
    git(
        &path,
        &["commit", "--quiet", "-m", "tracked file in odd dir"],
    )?;
    git(&path, &["push", "--quiet", "origin", "task-1"])?;
    let preview = harness.inspect()?;
    let entry = preview.entry(&owned.worktree).ok_or("worktree")?;
    assert!(
        entry.build_output.is_none(),
        "a directory holding tracked files is not build output"
    );
    Ok(())
}

#[test]
fn build_output_whose_owner_changes_before_removal_is_kept() -> TestResult {
    let mut harness = Harness::new()?;
    let owned = harness.owner("task-1", true)?;
    let target = build_dir(harness.path(&owned.worktree)?, 1024)?;
    harness.approve_all()?;
    harness.clock.advance(60);
    // Between reclaim's inspection and its revalidation, the backend
    // reports the worktree under another owner.
    let worktree = owned.worktree.clone();
    harness.backend.change_after(1, move |extra| {
        for observation in extra.iter_mut() {
            if observation.resource == worktree {
                observation.owner = ExternalRef::new("reassigned").ok();
            }
        }
    });
    let report = harness.reclaim()?;
    assert_eq!(
        report.results.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        [BuildOutcome::Changed]
    );
    assert!(target.is_dir());
    Ok(())
}

#[test]
fn disk_pressure_suggests_commands_for_external_caches_and_runs_none() -> TestResult {
    let harness = Harness::new()?;
    let pressured = inspect(
        &harness.inspector(),
        InspectionTrigger::DiskPressure,
        harness.clock.now(),
    )?;
    assert_eq!(pressured.suggestions, EXTERNAL_CACHE_SUGGESTIONS);
    assert!(harness.inspect()?.suggestions.is_empty());
    let json = serde_json::to_value(&pressured)?;
    assert_eq!(json["suggestions"][0]["command"], "cargo cache --autoclean");
    Ok(())
}
