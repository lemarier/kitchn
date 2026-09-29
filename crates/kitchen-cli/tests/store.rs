//! The `kitchn store` process contract against a temporary house store. No
//! forge is contacted: without --gh nothing that depends on an issue or pull
//! request is removed, so these runs cover only store-local retention.

use std::{
    error::Error,
    num::NonZeroU64,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use kitchen::{
    HolderId, HouseId, WorkflowId,
    contracts::{Claimant, CommitId, EvidenceSubject, Repository, Timestamp},
    state::{HouseStore, MarkerFact, MarkerKey, MarkerSubject, StoreOptions, WorkItem},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Kitchen {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Kitchen {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        HouseStore::initialize(root.join("store"), house()?, StoreOptions::default())?;
        Ok(Self { _dir: dir, root })
    }

    fn store(&self) -> TestResult<HouseStore> {
        Ok(HouseStore::open(
            self.root.join("store"),
            house()?,
            StoreOptions::default(),
        )?)
    }

    fn store_arg(&self) -> TestResult<String> {
        path_arg(&self.root.join("store"))
    }

    fn run(&self, args: &[&str]) -> TestResult<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(args)
            .output()?)
    }

    fn retain(&self, extra: &[&str]) -> TestResult<Output> {
        let store = self.store_arg()?;
        let mut args = vec!["store", "retain", "--house", "origin89", "--store", &store];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

fn house() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn path_arg(path: &Path) -> TestResult<String> {
    Ok(path.to_str().ok_or("non-UTF-8 path")?.to_owned())
}

fn ready_key(head: char) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new("ready-report")?,
        item: WorkItem::PullRequest {
            repository: Repository::new("origin89hq/km43")?,
            number: NonZeroU64::new(4).ok_or("zero")?,
        },
        subject: MarkerSubject::Git(EvidenceSubject {
            head: CommitId::new(&head.to_string().repeat(40))?,
            base: None,
        }),
    })
}

/// Two reports for the same pull request at an older and a newer head.
fn record_heads(kitchen: &Kitchen) -> TestResult {
    let store = kitchen.store()?;
    let recorder = Claimant::scheduled(HolderId::new("ready")?);
    for (head, at) in [('a', 1), ('b', 2)] {
        store.record_marker(
            ready_key(head)?,
            MarkerFact::workflow("ready-report/1".parse()?, &"delivered")?,
            &recorder,
            Timestamp::from_unix_millis(at),
        )?;
    }
    Ok(())
}

#[test]
fn capacity_reports_each_table() -> TestResult {
    let kitchen = Kitchen::new()?;
    record_heads(&kitchen)?;
    let store = kitchen.store_arg()?;
    let output = kitchen.run(&[
        "store", "capacity", "--house", "origin89", "--store", &store,
    ])?;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("Workflow markers: 2 of 4096"), "{text}");
    assert!(text.contains("ready-report: 2"), "{text}");

    let output = kitchen.run(&[
        "store", "capacity", "--house", "origin89", "--store", &store, "--json",
    ])?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["markers"]["used"], 2);
    assert_eq!(json["tasks"]["limit"], 4096);
    Ok(())
}

#[test]
fn retain_previews_by_default_and_removes_only_with_apply() -> TestResult {
    let kitchen = Kitchen::new()?;
    record_heads(&kitchen)?;

    let preview = kitchen.retain(&["--json"])?;
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&preview.stdout)?;
    assert_eq!(json["retention"]["applied"], false);
    assert_eq!(json["retention"]["markers"][0]["reason"], "superseded");
    assert_eq!(json["observed"], 0);
    assert!(kitchen.store()?.marker(&ready_key('a')?)?.is_some());

    let applied = kitchen.retain(&["--apply"])?;
    assert!(applied.status.success());
    let text = String::from_utf8(applied.stdout)?;
    assert!(text.starts_with("Removed 1 marker(s)"), "{text}");
    assert_eq!(kitchen.store()?.marker(&ready_key('a')?)?, None);
    assert!(kitchen.store()?.marker(&ready_key('b')?)?.is_some());
    Ok(())
}

#[test]
fn retain_refuses_invalid_arguments_before_any_write() -> TestResult {
    let kitchen = Kitchen::new()?;
    record_heads(&kitchen)?;
    for extra in [
        &["--window-days", "30", "--apply"][..],
        &["--max-lookups", "1001", "--apply"],
        &["--gh", "/usr/bin/gh", "--apply"],
        &["--registry", "registry", "--gh", "gh", "--apply"],
    ] {
        let output = kitchen.retain(extra)?;
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
    }
    let output = kitchen.run(&[
        "store", "capacity", "--house", "origin89", "--store", "store",
    ])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(kitchen.store()?.marker(&ready_key('a')?)?.is_some());
    Ok(())
}
