//! Shared fixtures for Kitchen integration tests. Every store lives in a
//! temporary directory outside the repository.

#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::{cell::Cell, time::Duration};

use kitchen::{
    BackendId, EffectName, HolderId, HouseId, TaskId,
    contracts::{
        Clock, CommitId, EvidenceRevision, Fence, Grant, HouseGrants, LeaseTtl, Operation,
        Permission, Provenance, RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Timestamp,
        Workspace,
    },
    state::{EffectPlan, HouseStore, StoreOptions},
};
use tempfile::TempDir;

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub const HOUSE: &str = "origin89";
pub const OTHER_HOUSE: &str = "crabnebula";

pub fn house() -> TestResult<HouseId> {
    Ok(HouseId::new(HOUSE)?)
}

pub fn other_house() -> TestResult<HouseId> {
    Ok(HouseId::new(OTHER_HOUSE)?)
}

pub fn task_id(value: &str) -> TestResult<TaskId> {
    Ok(TaskId::new(value)?)
}

pub fn holder(value: &str) -> TestResult<HolderId> {
    Ok(HolderId::new(value)?)
}

pub fn backend_id() -> TestResult<BackendId> {
    Ok(BackendId::new("fake")?)
}

pub fn effect(value: &str) -> TestResult<EffectName> {
    Ok(EffectName::new(value)?)
}

/// Worker-lifecycle permissions granted house-wide.
pub const WORKER_PERMISSIONS: [Permission; 4] = [
    Permission::LaunchWorker,
    Permission::MessageWorker,
    Permission::CancelWorker,
    Permission::ReleaseResource,
];

pub fn grants_for(house: HouseId, permissions: &[Permission]) -> HouseGrants {
    HouseGrants::new(house, permissions.iter().copied().map(Grant::house))
}

pub fn grants() -> TestResult<HouseGrants> {
    Ok(grants_for(house()?, &WORKER_PERMISSIONS))
}

pub fn commit(fill: char) -> TestResult<CommitId> {
    Ok(CommitId::new(&fill.to_string().repeat(40))?)
}

pub fn spec_with(id: &str, retry: RetryPolicy, permissions: &[Permission]) -> TestResult<TaskSpec> {
    let authority =
        TaskAuthority::delegate(&grants()?, permissions.iter().copied().map(Grant::house))?;
    Ok(TaskSpec {
        id: task_id(id)?,
        role: Role::StationCook,
        repository: None,
        authority,
        retry,
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
    })
}

pub fn spec(id: &str) -> TestResult<TaskSpec> {
    spec_with(
        id,
        RetryPolicy::new(3, Duration::from_secs(3600))?,
        &WORKER_PERMISSIONS,
    )
}

pub fn ttl(seconds: u64) -> TestResult<LeaseTtl> {
    Ok(LeaseTtl::new(Duration::from_secs(seconds))?)
}

pub const fn at(seconds: u64) -> Timestamp {
    Timestamp::from_unix_millis(seconds * 1000)
}

pub fn launch() -> TestResult<Operation> {
    Ok(Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new("Implement the task described in the issue.")?,
    })
}

pub fn plan(
    task: &TaskId,
    fence: Fence,
    name: &str,
    operation: Operation,
) -> TestResult<EffectPlan> {
    Ok(EffectPlan {
        task: task.clone(),
        fence,
        name: effect(name)?,
        decided_at: EvidenceRevision::INITIAL,
        operation,
    })
}

/// A store in a fresh temporary directory. Keep `dir` alive for the test.
pub struct Fixture {
    pub dir: TempDir,
    pub store: HouseStore,
}

impl Fixture {
    pub fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let store =
            HouseStore::initialize(dir.path().join("house"), house()?, StoreOptions::default())?;
        Ok(Self { dir, store })
    }

    /// Another handle on the same directory, as a separate process would open it.
    pub fn reopen(&self) -> TestResult<HouseStore> {
        Ok(HouseStore::open(
            self.dir.path().join("house"),
            house()?,
            StoreOptions::default(),
        )?)
    }

    pub fn state_path(&self) -> std::path::PathBuf {
        self.dir.path().join("house").join("state.json")
    }
}

/// A clock tests advance explicitly.
pub struct ManualClock(Cell<u64>);

impl ManualClock {
    pub const fn starting_at(seconds: u64) -> Self {
        Self(Cell::new(seconds * 1000))
    }

    pub fn advance(&self, seconds: u64) {
        self.0.set(self.0.get() + seconds * 1000);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.get())
    }
}
