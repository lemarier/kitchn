//! The `kitchn trust` process contract against a temporary house store and
//! trust ledger. Observations are simulated fixtures; no runtime, forge, or
//! model is contacted.

use std::{
    collections::BTreeSet,
    error::Error,
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::Duration,
};

use kitchen::{
    HolderId, HouseId, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, CapabilityRequirements, Claimant, CommitId, ExternalRef,
        HouseGrants, LeaseTtl, Provenance, Repository, RetryPolicy, Role, TaskAuthority, TaskSpec,
        Timestamp,
    },
    selection::WorkType,
    state::{HouseStore, StoreOptions},
    trust::{
        ARCHIVE_FILE, Attribution, EvidenceMode, Ledger, Measurement, Observation, StationScope,
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Kitchen {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Kitchen {
    /// A house store and trust ledger holding one settled task's observation.
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let store = HouseStore::initialize(root.join("store"), house()?, StoreOptions::default())?;
        let ledger = Ledger::initialize(root.join("trust"), house()?)?;
        record_settled(&store, &ledger)?;
        Ok(Self { _dir: dir, root })
    }

    fn ledger(&self) -> PathBuf {
        self.root.join("trust")
    }

    fn run(&self, command: &str, extra: &[&str]) -> TestResult<Output> {
        let ledger = self.ledger();
        let ledger = ledger.to_str().ok_or("non-UTF-8 path")?;
        let mut args = vec!["trust", command, "--house", "origin89", "--ledger", ledger];
        args.extend_from_slice(extra);
        run(&args)
    }
}

fn run(args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .args(args)
        .output()?)
}

fn house() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn at(millis: u64) -> Timestamp {
    Timestamp::from_unix_millis(millis)
}

fn record_settled(store: &HouseStore, ledger: &Ledger) -> TestResult {
    let task = TaskId::new("settled")?;
    let repository = Repository::new("example/project")?;
    let work_type = WorkType::new("implementation")?;
    let tick = Claimant::scheduled(HolderId::new("pickup")?);
    store.create_task(
        TaskSpec {
            id: task.clone(),
            role: Role::StationCook,
            repository: Some(repository.clone()),
            authority: TaskAuthority::delegate(&HouseGrants::new(house()?, []), [])?,
            retry: RetryPolicy::new(1, Duration::from_secs(60))?,
            provenance: Provenance {
                kitchen: CommitId::new(&"a".repeat(40))?,
                house_guidance: CommitId::new(&"b".repeat(40))?,
                repository_instructions: None,
            },
            resources: BTreeSet::new(),
            requires: CapabilityRequirements::new(),
            agent: None,
            work_type: Some(work_type.clone()),
        },
        &tick,
        at(0),
    )?;
    let fence = store
        .claim(&task, &tick, LeaseTtl::new(Duration::from_secs(60))?, at(1))?
        .fence();
    store.start_attempt(&task, fence, at(2))?;
    store.finish_attempt(
        &task,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(3),
    )?;
    let observation = Observation::collect(
        store,
        &task,
        ExternalRef::new("fixture:settled")?,
        Attribution {
            scope: StationScope {
                station: Role::StationCook,
                project: repository,
                work_type,
            },
            agent: Measurement::Missing,
            model: Measurement::Missing,
            tokens: Measurement::Missing,
        },
        EvidenceMode::Simulated,
        at(4),
    )?;
    ledger.record(store, observation)?;
    Ok(())
}

fn stdout(output: &Output) -> TestResult<String> {
    Ok(String::from_utf8(output.stdout.clone())?)
}

#[test]
fn capacity_reports_entries_and_limits_as_json() -> TestResult {
    let kitchen = Kitchen::new()?;
    let output = kitchen.run("capacity", &["--json"])?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)?)?;
    assert_eq!(report["capacity"]["entries"], 1);
    assert_eq!(report["capacity"]["maxEntries"], 4096);
    assert_eq!(report["capacity"]["maxBytes"], 8 * 1024 * 1024);
    assert_eq!(report["archivals"], serde_json::json!([]));
    Ok(())
}

#[test]
fn archive_previews_by_default_and_applies_with_apply() -> TestResult {
    let kitchen = Kitchen::new()?;
    let snapshot = kitchen.ledger().join("ledger.json");
    let before = fs::read(&snapshot)?;

    let preview = kitchen.run("archive", &[])?;
    assert_eq!(preview.status.code(), Some(0), "{preview:?}");
    let text = stdout(&preview)?;
    assert!(
        text.contains("Would archive 1 observation stream(s) (1 revision(s)), 0 binding(s)"),
        "{text}"
    );
    assert!(
        text.contains("stream fixture:settled (task settled)"),
        "{text}"
    );
    assert!(text.contains("rerun with --apply"), "{text}");
    assert_eq!(fs::read(&snapshot)?, before, "a preview writes nothing");
    assert!(!kitchen.ledger().join(ARCHIVE_FILE).exists());

    let applied = kitchen.run("archive", &["--apply", "--json"])?;
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    let report: serde_json::Value = serde_json::from_str(&stdout(&applied)?)?;
    assert_eq!(report["applied"], true);
    assert_eq!(report["archive"]["streams"][0]["id"], "fixture:settled");
    let digest = report["archive"]["archival"]["digest"]
        .as_str()
        .ok_or("digest")?;
    assert_eq!(digest.len(), 64);
    assert_eq!(report["capacity"]["entries"], 1, "the summary alone");
    assert!(kitchen.ledger().join(ARCHIVE_FILE).exists());

    let capacity = kitchen.run("capacity", &[])?;
    let text = stdout(&capacity)?;
    assert!(
        text.contains("Archived: 1 record(s) in 1 archival(s)"),
        "{text}"
    );
    assert!(text.contains(digest), "{text}");

    let again = kitchen.run("archive", &["--apply"])?;
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert!(
        stdout(&again)?.contains("Archived 0 observation stream(s)"),
        "nothing left to archive"
    );
    Ok(())
}

#[test]
fn invalid_paths_and_foreign_houses_are_refused() -> TestResult {
    let kitchen = Kitchen::new()?;
    let relative = run(&[
        "trust", "archive", "--house", "origin89", "--ledger", "trust", "--apply",
    ])?;
    assert_eq!(relative.status.code(), Some(2), "{relative:?}");

    let missing = kitchen.root.join("absent");
    let missing = run(&[
        "trust",
        "capacity",
        "--house",
        "origin89",
        "--ledger",
        missing.to_str().ok_or("path")?,
    ])?;
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");

    let ledger = kitchen.ledger();
    let foreign = run(&[
        "trust",
        "archive",
        "--house",
        "crabnebula",
        "--ledger",
        ledger.to_str().ok_or("path")?,
        "--apply",
    ])?;
    assert_ne!(foreign.status.code(), Some(0), "{foreign:?}");
    assert!(!kitchen.ledger().join(ARCHIVE_FILE).exists());
    Ok(())
}

#[test]
fn a_ledger_near_its_limit_exits_nonzero() -> TestResult {
    let kitchen = Kitchen::new()?;
    let snapshot = kitchen.ledger().join("ledger.json");
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&snapshot)?)?;
    let observations = document["observations"]
        .as_array_mut()
        .ok_or("observations")?;
    let template = observations.first().cloned().ok_or("observation")?;
    // 80 % of 4096 entries.
    for index in 1..3277 {
        let mut copy = template.clone();
        copy["id"] = serde_json::json!(format!("fixture:bulk-{index}"));
        copy["task"] = serde_json::json!(format!("bulk-{index}"));
        observations.push(copy);
    }
    fs::write(&snapshot, serde_json::to_vec(&document)?)?;
    let near = kitchen.run("capacity", &[])?;
    assert_eq!(near.status.code(), Some(1), "{near:?}");
    assert!(stdout(&near)?.contains("Near the limit"));
    assert_eq!(kitchen.run("archive", &["--apply"])?.status.code(), Some(0));
    assert_eq!(kitchen.run("capacity", &[])?.status.code(), Some(0));
    Ok(())
}
