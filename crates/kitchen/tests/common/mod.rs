//! Shared fixtures for Kitchen integration tests. Every store lives in a
//! temporary directory outside the repository.

#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::{cell::Cell, time::Duration};

#[cfg(unix)]
pub mod executable;

use kitchen::{
    BackendId, CredentialId, EffectName, HolderId, HouseId, TaskId,
    contracts::{
        BackendDescriptor, Capability, CapabilitySet, Claimant, Clock, CommitId, Effect,
        EvidenceRevision, Fence, Grant, HouseGrants, LeaseTtl, Operation, Permission, Provenance,
        RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Timestamp, Workspace, fake::FakeBackend,
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

pub fn credential() -> TestResult<CredentialId> {
    Ok(CredentialId::new("origin89-orca")?)
}

/// A scheduled claimant, such as one pickup tick.
pub fn scheduled(value: &str) -> TestResult<Claimant> {
    Ok(Claimant::scheduled(holder(value)?))
}

/// An interactive claimant: a session with a person present.
pub fn interactive(value: &str) -> TestResult<Claimant> {
    Ok(Claimant::interactive(holder(value)?))
}

/// The scheduled tick that creates test tasks.
pub fn creator() -> TestResult<Claimant> {
    scheduled("pickup")
}

pub fn grant(permission: Permission) -> TestResult<Grant> {
    Ok(Grant::house(permission, backend_id()?, credential()?))
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

pub fn grants_for(house: HouseId, permissions: &[Permission]) -> TestResult<HouseGrants> {
    let grants = permissions
        .iter()
        .map(|permission| grant(*permission))
        .collect::<TestResult<Vec<_>>>()?;
    Ok(HouseGrants::new(house, grants))
}

pub fn grants() -> TestResult<HouseGrants> {
    grants_for(house()?, &WORKER_PERMISSIONS)
}

pub fn commit(fill: char) -> TestResult<CommitId> {
    Ok(CommitId::new(&fill.to_string().repeat(40))?)
}

pub fn spec_with(id: &str, retry: RetryPolicy, permissions: &[Permission]) -> TestResult<TaskSpec> {
    let requested = permissions
        .iter()
        .map(|permission| grant(*permission))
        .collect::<TestResult<Vec<_>>>()?;
    let authority = TaskAuthority::delegate(&grants()?, requested)?;
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
        resources: std::collections::BTreeSet::new(),
        requires: kitchen::contracts::CapabilityRequirements::new(),
        agent: None,
        work_type: None,
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
        branch: None,
        pinned: None,
        agent: None,
    })
}

pub fn plan(
    task: &TaskId,
    fence: Fence,
    name: &str,
    action: impl Into<Effect>,
) -> TestResult<EffectPlan> {
    Ok(EffectPlan {
        task: task.clone(),
        fence,
        name: effect(name)?,
        decided_at: EvidenceRevision::INITIAL,
        effect: action.into(),
        consent: None,
        basis: None,
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

/// A fake executor declaring exactly `capabilities`.
pub fn executor_with(
    capabilities: impl IntoIterator<Item = Capability>,
) -> TestResult<FakeBackend> {
    Ok(FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting(capabilities),
    ))
}

/// A backend descriptor declaring exactly `capabilities`.
pub fn descriptor_with(
    capabilities: impl IntoIterator<Item = Capability>,
) -> TestResult<BackendDescriptor> {
    Ok(BackendDescriptor {
        backend: backend_id()?,
        house: house()?,
        worker_selection: None,
        capabilities: CapabilitySet::supporting(capabilities),
    })
}

/// A worker backend with lookup but without provider-side idempotency.
pub fn refusing() -> TestResult<FakeBackend> {
    executor_with([
        Capability::WorkerLaunchIsolated,
        Capability::WorkerMessaging,
        Capability::WorkerCancel,
        Capability::ResourceRelease,
        Capability::WorkerStatusAndOutcome,
        Capability::EffectLookup,
    ])
}

/// A backend declaring every capability, including idempotent requests.
pub fn idempotent() -> TestResult<FakeBackend> {
    executor_with(Capability::ALL)
}

/// Roger's reference for the one Ask a [`AckRoger`] acknowledges.
pub const ROGER_ASK: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
/// Requester identity the house's Roger scope uses.
pub const ROGER_REQUESTER: &str = "kitchen-roger";

/// A Roger stand-in that acknowledges every Ask as [`ROGER_ASK`]. It never
/// answers; [`roger_client`] reads sanitized [`roger_answer`] fixtures.
pub struct AckRoger(BackendDescriptor);

impl AckRoger {
    pub fn new() -> TestResult<Self> {
        Ok(Self(BackendDescriptor {
            backend: BackendId::new("roger")?,
            house: house()?,
            worker_selection: None,
            capabilities: CapabilitySet::supporting(Capability::ALL),
        }))
    }
}

impl kitchen::contracts::EffectExecutor for AckRoger {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.0
    }
    fn execute(
        &self,
        _: &kitchen::contracts::EffectRequest,
    ) -> Result<kitchen::contracts::Receipt, kitchen::contracts::EffectFailure> {
        kitchen::contracts::ExternalRef::new(ROGER_ASK)
            .ok()
            .and_then(|ask| kitchen::contracts::Receipt::new(ask, vec![], vec![]).ok())
            .ok_or(kitchen::contracts::EffectFailure::NotApplied(
                kitchen::contracts::NotAppliedReason::Rejected,
            ))
    }
    fn lookup(
        &self,
        _: &kitchen::contracts::EffectRequest,
    ) -> Result<kitchen::contracts::Lookup, kitchen::contracts::BackendUnavailable> {
        Ok(kitchen::contracts::Lookup::Unknown)
    }
}

/// The house's Roger scope for `repository`.
pub fn roger_scope(
    repository: &kitchen::contracts::Repository,
) -> TestResult<kitchen::integrations::github::HouseScope> {
    let requester = kitchen::contracts::ExternalRef::new(ROGER_REQUESTER)?;
    Ok(kitchen::integrations::github::HouseScope::new(
        house()?,
        [repository.clone()],
        requester.clone(),
        kitchen::integrations::github::CredentialRef::new(
            house()?,
            CredentialId::new("roger-credential")?,
            requester,
        ),
        kitchen::integrations::github::PostingBudget::new(3)?,
        [Permission::AskHuman],
    )?)
}

/// Roger as the house's Roger client reads it: every `get` returns `reply`
/// and records the Ask id it was asked for.
pub struct RogerReply {
    reply: Result<Vec<u8>, kitchen::integrations::github::IntegrationError>,
    asked: RogerReads,
}

/// Ask ids a [`RogerReply`] was read for, in order.
pub type RogerReads = std::rc::Rc<std::cell::RefCell<Vec<kitchen::contracts::ExternalRef>>>;

impl kitchen::integrations::roger::RogerReadTransport for RogerReply {
    fn get(
        &self,
        _: &kitchen::integrations::github::CredentialRef,
        ask: &kitchen::contracts::ExternalRef,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, kitchen::integrations::github::IntegrationError> {
        self.asked.borrow_mut().push(ask.clone());
        self.reply.clone()
    }
}

/// The house's Roger client for `repository`, reading `reply` from Roger,
/// and the Ask ids it reads.
pub fn roger_client(
    repository: &kitchen::contracts::Repository,
    reply: Result<Vec<u8>, kitchen::integrations::github::IntegrationError>,
) -> TestResult<(
    kitchen::integrations::roger::RogerClient<RogerReply>,
    RogerReads,
)> {
    let asked = RogerReads::default();
    Ok((
        kitchen::integrations::roger::RogerClient::new(
            roger_scope(repository)?,
            RogerReply {
                reply,
                asked: asked.clone(),
            },
            kitchen::integrations::github::ReadLimits::default(),
        ),
        asked,
    ))
}

/// A running task that may ask a person about `repository`, with its
/// evidence at `head` and `base`. Returns the task, its fence, the
/// evidence revision, and the house grants.
pub fn asking_task(
    fixture: &Fixture,
    id: &str,
    repository: &kitchen::contracts::Repository,
    head: &CommitId,
    base: &CommitId,
) -> TestResult<(TaskId, Fence, EvidenceRevision, HouseGrants)> {
    let ask = Grant::repository(
        Permission::AskHuman,
        repository.clone(),
        BackendId::new("roger")?,
        CredentialId::new("roger-credential")?,
    );
    let grants = HouseGrants::new(house()?, [ask.clone()]);
    let mut work = spec(id)?;
    work.repository = Some(repository.clone());
    work.authority = TaskAuthority::delegate(&grants, [ask])?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let task = task_id(id)?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("owner")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let revision = fixture.store.record_evidence(
        &task,
        fence,
        kitchen::contracts::Evidence {
            kind: kitchen::contracts::EvidenceKind::Check,
            verdict: kitchen::contracts::EvidenceVerdict::Pass,
            subject: kitchen::contracts::EvidenceSubject {
                head: head.clone(),
                base: Some(base.clone()),
            },
            source: kitchen::contracts::ExternalRef::new("ci-1")?,
            observed_at: at(1),
        },
        at(1),
    )?;
    Ok((task, fence, revision, grants))
}

/// Persist `ask` as the task's Roger effect and have Roger acknowledge it.
pub fn persist_ask(
    fixture: &Fixture,
    task: &TaskId,
    fence: Fence,
    grants: &HouseGrants,
    ask: kitchen::contracts::RogerAsk,
) -> TestResult<kitchen::state::EffectRecord> {
    let revision = ask.binding.revision;
    let effect = kitchen::contracts::RogerEffect {
        requester: kitchen::contracts::ExternalRef::new(ROGER_REQUESTER)?,
        ask,
        posting_budget: kitchen::integrations::github::PostingBudget::new(3)?,
    };
    let mut plan = plan(task, fence, "readiness-ask", Effect::Roger(effect))?;
    plan.decided_at = revision;
    Ok(kitchen::state::run_effect(
        &fixture.store,
        &AckRoger::new()?,
        grants,
        plan,
        &ManualClock::starting_at(2),
    )?)
}

/// A sanitized Roger reply to `ask`, as Roger reports the Ask it holds.
/// `approve` selects a passkey approval; otherwise the owner rejected it.
pub fn roger_answer(ask: &kitchen::contracts::RogerAsk, approve: bool) -> TestResult<Vec<u8>> {
    let binding = &ask.binding;
    let head = binding.head()?.as_str();
    let action = serde_json::json!({
        "verb": binding.action.as_str(),
        "target": binding.target.as_str(),
        "rev": head,
        "limits": binding.limits.as_str(),
    });
    let decision = if approve { "approve" } else { "reject" };
    Ok(serde_json::to_vec(&serde_json::json!({
        "id": ROGER_ASK,
        "requester": ROGER_REQUESTER,
        "repo": binding.repository.to_string(),
        "decisionKey": binding.decision_key()?,
        "kind": "approval",
        "action": action,
        "resume": {"task": binding.task.to_string(), "rev": head},
        "state": "answered",
        "supersededBy": null,
        "answer": {
            "decision": decision,
            "optionId": decision,
            "action": action,
            "input": null,
            "passkey": approve,
        },
    }))?)
}

/// A house config from the shared fixture whose follow-up policy allows
/// `fix_rounds` rounds, or whose policy is absent for `None`. The only way a
/// test obtains a budget, as in production.
pub fn house_with_fix_rounds(fix_rounds: Option<u8>) -> TestResult<kitchen::house::HouseConfig> {
    let mut json: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/house/origin89.json"))?;
    if let Some(fix_rounds) = fix_rounds {
        json["followUp"] = serde_json::json!({ "fixRounds": fix_rounds });
    }
    Ok(serde_json::from_value(json)?)
}
