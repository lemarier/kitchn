//! Scheduled runner passes against the in-memory fake backend and a fake
//! forge that answers GitHub reads from a table. Simulated evidence only:
//! no live Orca, backend, or GitHub was contacted.

mod common;

use std::{cell::RefCell, collections::BTreeMap, time::Duration};

use common::{ManualClock, TestResult, WORKER_PERMISSIONS, backend_id, commit, credential, house};
use kitchen::{
    CredentialId,
    contracts::{
        BranchName, Capability, CapabilitySet, Clock, ExternalRef, Grant, MailMessage, MessageKind,
        PostingBudget, Repository, ResourceRef, Settlement, Text, WorkerOutcome, WorkerState,
        fake::FakeBackend,
    },
    house::HouseConfig,
    integrations::github::{
        CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError, ReadLimits,
        ReadRequest,
    },
    state::{
        ConsumerState, HouseStore, MailSender, PostKind, ReportedOutcome, TaskState, WorkerPost,
    },
    workflows::{
        coordination::{Supervision, current_worker},
        gate::Verdict,
        pickup::{IssueRef, PinnedInstructions, issue_task_id},
        repair::{HandOver, RepairDecision, Skip},
        run::{
            CoordinateAction, CoordinatePass, GatePass, Outcome, PASS_LEASE, Pass, PickupAction,
            PickupLabels, PickupPass, PickupSettings, RepairAction, RepairPass, RunError,
            TASK_LEASE, pass_repository, run_claimant,
        },
    },
};
use serde_json::{Value, json};

const REPO: &str = "origin89hq/firmware";

fn repo() -> TestResult<Repository> {
    Ok(Repository::new(REPO)?)
}

/// The shared house, granted worker lifecycle on the fake backend. House
/// policy scopes launches to a repository.
fn house_config() -> TestResult<HouseConfig> {
    let mut config = common::house_with_fix_rounds(Some(2))?;
    for permission in WORKER_PERMISSIONS {
        let grant = if permission == kitchen::contracts::Permission::LaunchWorker {
            Grant::repository(permission, repo()?, backend_id()?, credential()?)
        } else {
            Grant::house(permission, backend_id()?, credential()?)
        };
        config.policy_limits.insert(grant.clone());
        config.grants.insert(grant);
    }
    Ok(config)
}

/// A GitHub transport answering each endpoint from a table. A missing
/// endpoint is unavailable, never empty.
struct Forge {
    responses: RefCell<BTreeMap<String, Value>>,
    reads: RefCell<Vec<String>>,
}

impl Forge {
    fn new() -> Self {
        Self {
            responses: RefCell::new(BTreeMap::new()),
            reads: RefCell::new(Vec::new()),
        }
    }

    fn set(&self, endpoint: &str, value: Value) {
        self.responses
            .borrow_mut()
            .insert(endpoint.to_owned(), value);
    }

    fn reads(&self) -> usize {
        self.reads.borrow().len()
    }

    /// The table key of `request`: paging parameters are dropped (every
    /// answer is one page), and a GraphQL query is keyed by what it reads.
    fn key(request: &ReadRequest) -> String {
        if let Some(query) = request.graphql() {
            let number = query
                .pointer("/variables/number")
                .cloned()
                .unwrap_or_default();
            let kind = if query.to_string().contains("closedByPullRequestsReferences") {
                "closing"
            } else {
                "other"
            };
            return format!("graphql:{kind}#{number}");
        }
        let endpoint = request.endpoint();
        match endpoint.find("per_page=") {
            Some(at) => endpoint
                .get(..at)
                .unwrap_or(endpoint)
                .trim_end_matches(['?', '&'])
                .to_owned(),
            None => endpoint.to_owned(),
        }
    }
}

impl GitHubReadTransport for Forge {
    fn read(
        &self,
        _: &CredentialRef,
        request: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let key = Self::key(request);
        self.reads.borrow_mut().push(key.clone());
        let value = self
            .responses
            .borrow()
            .get(&key)
            .cloned()
            .ok_or(IntegrationError::Unavailable)?;
        serde_json::to_vec(&value).map_err(|_| IntegrationError::Unknown)
    }
}

fn client(forge: Forge) -> TestResult<GitHubClient<Forge>> {
    let requester = ExternalRef::new("kitchen-bot")?;
    let scope = HouseScope::new(
        house()?,
        [repo()?],
        requester.clone(),
        CredentialRef::new(house()?, CredentialId::new("forge")?, requester),
        PostingBudget::new(0)?,
        [],
    )?;
    Ok(GitHubClient::new(scope, forge, ReadLimits::default()))
}

fn issue_json(number: u64, labels: &[&str]) -> Value {
    json!({
        "repository_url": format!("https://api.github.com/repos/{REPO}"),
        "id": number,
        "number": number,
        "title": format!("Issue {number}"),
        "state": "open",
        "assignees": [],
        "labels": labels
            .iter()
            .map(|name| json!({"name": name, "color": "ffffff", "description": null}))
            .collect::<Vec<_>>(),
        "updated_at": "2026-09-01T00:00:00Z",
        "closed_at": null,
    })
}

const ACCEPTANCE: &str =
    "Add the driver.\n\n## Acceptance\n\n- The firmware builds with the new driver.\n";

/// Ready issue `number` with its detail, no blockers, and no linked work.
fn ready_issue(forge: &Forge, number: u64, body: &str) {
    forge.set(
        &format!("repos/{REPO}/issues/{number}"),
        json!({
            "number": number,
            "state": "open",
            "user": {"login": "lemarier"},
            "body": body,
            "created_at": "2026-09-01T00:00:00Z",
            "updated_at": "2026-09-01T00:00:00Z",
            "closed_at": null,
        }),
    );
    forge.set(
        &format!("repos/{REPO}/issues/{number}/dependencies/blocked_by"),
        json!([]),
    );
    forge.set(&format!("repos/{REPO}/issues/{number}/timeline"), json!([]));
    closing_prs(forge, number, &[]);
}

fn closing_prs(forge: &Forge, issue: u64, prs: &[u64]) {
    forge.set(
        &format!("graphql:closing#{issue}"),
        json!({"data": {"repository": {"issue": {"closedByPullRequestsReferences": {
            "nodes": prs
                .iter()
                .map(|number| json!({"number": number, "repository": {"nameWithOwner": REPO}}))
                .collect::<Vec<_>>(),
            "pageInfo": {"hasNextPage": false, "endCursor": null},
        }}}}}),
    );
}

fn open_issues(forge: &Forge, issues: Vec<Value>) {
    forge.set(
        &format!("repos/{REPO}/issues?state=open"),
        Value::Array(issues),
    );
}

/// The pull request of the task branch `kitchen/issue-<issue>`.
fn pull_request(forge: &Forge, issue: u64, number: u64, mergeable: bool) -> TestResult {
    closing_prs(forge, issue, &[number]);
    forge.set(
        &format!("repos/{REPO}/pulls/{number}"),
        json!({
            "number": number,
            "state": "open",
            "draft": false,
            "merged": false,
            "head": {"sha": commit('d')?.as_str(), "ref": format!("kitchen/issue-{issue}"), "repo": {"full_name": REPO}},
            "base": {"sha": commit('e')?.as_str(), "ref": "main", "repo": {"full_name": REPO}},
            "mergeable": mergeable,
            "mergeable_state": if mergeable { "clean" } else { "dirty" },
            "user": {"login": "kitchen-bot"},
        }),
    );
    Ok(())
}

fn settings() -> TestResult<PickupSettings> {
    Ok(PickupSettings {
        repository: repo()?,
        labels: PickupLabels {
            ready: "ready".to_owned(),
            needs_spec: "needs-spec".to_owned(),
            human_only: "human-only".to_owned(),
        },
        capacity: 1,
        branch_prefix: BranchName::new("kitchen")?,
        instructions: PinnedInstructions {
            house: house()?,
            provenance: kitchen::contracts::Provenance {
                kitchen: commit('a')?,
                house_guidance: commit('b')?,
                repository_instructions: None,
            },
            entrypoint: Text::new("snapshots/origin89/AGENTS.md")?,
        },
        report_path: Text::new("kitchen-report.md")?,
    })
}

/// One house with its store, fake backend, fake forge, and clock.
struct Kitchen {
    fixture: common::Fixture,
    config: HouseConfig,
    backend: FakeBackend,
    forge: GitHubClient<Forge>,
    clock: ManualClock,
    settings: PickupSettings,
}

impl Kitchen {
    fn new() -> TestResult<Self> {
        Self::with_backend(FakeBackend::fully_capable(backend_id()?, house()?))
    }

    fn with_backend(backend: FakeBackend) -> TestResult<Self> {
        let forge = Forge::new();
        open_issues(&forge, Vec::new());
        Ok(Self {
            fixture: common::Fixture::new()?,
            config: house_config()?,
            backend,
            forge: client(forge)?,
            clock: ManualClock::starting_at(1_000_000),
            settings: settings()?,
        })
    }

    fn store(&self) -> &HouseStore {
        &self.fixture.store
    }

    fn forge(&self) -> &Forge {
        self.forge.transport()
    }

    fn pickup(&self, take_over: bool) -> kitchen::Result<Outcome<PickupAction>> {
        PickupPass {
            store: self.store(),
            house: &self.config,
            backend: &self.backend,
            forge: &self.forge,
            clock: &self.clock,
            settings: &self.settings,
            take_over,
        }
        .run()
    }

    fn coordinate_on(
        &self,
        backend: &FakeBackend,
        take_over: bool,
    ) -> kitchen::Result<Outcome<CoordinateAction>> {
        CoordinatePass {
            store: self.store(),
            house: &self.config,
            backend,
            forge: &self.forge,
            clock: &self.clock,
            take_over,
        }
        .run()
    }

    fn coordinate(&self) -> kitchen::Result<Outcome<CoordinateAction>> {
        self.coordinate_on(&self.backend, false)
    }

    fn repair(&self) -> kitchen::Result<Outcome<RepairAction>> {
        RepairPass {
            store: self.store(),
            house: &self.config,
            backend: &self.backend,
            forge: &self.forge,
            clock: &self.clock,
            repository: &self.settings.repository,
            take_over: false,
        }
        .run()
    }

    fn gate(&self) -> kitchen::Result<Outcome<kitchen::workflows::run::GateAction>> {
        GatePass {
            store: self.store(),
            house: &self.config,
            forge: &self.forge,
            clock: &self.clock,
            repository: &self.settings.repository,
            authors: &["kitchen-bot".to_owned()],
            take_over: false,
        }
        .run()
    }

    /// Issue 7 is ready and has acceptance criteria.
    fn ready_seven(&self) {
        open_issues(self.forge(), vec![issue_json(7, &["ready"])]);
        ready_issue(self.forge(), 7, ACCEPTANCE);
    }

    fn task(&self, number: u64) -> TestResult<kitchen::TaskId> {
        Ok(issue_task_id(&IssueRef {
            repository: repo()?,
            number: kitchen::contracts::IssueNumber::new(number)?,
        })?)
    }

    fn worker(&self, number: u64) -> TestResult<ResourceRef> {
        let record = self.store().task(&self.task(number)?)?;
        Ok(current_worker(&record).ok_or("no worker")?.worker)
    }

    /// The consumer state of `pass`.
    fn consumer(&self, pass: Pass) -> TestResult<Option<ConsumerState>> {
        Ok(self
            .store()
            .consumer(&pass.consumer(&repo()?)?)?
            .map(|record| record.state().clone()))
    }

    /// Launch issue 7 and report it done on the backend's mailbox, with the
    /// branch tip on the forge.
    fn launch_and_finish(&self) -> TestResult<ResourceRef> {
        self.ready_seven();
        assert!(matches!(self.pickup(false)?, Outcome::Acted(_)));
        let worker = self.worker(7)?;
        self.backend
            .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
        self.backend.post(vec![report(&worker, "done-7")?])?;
        self.forge().set(
            &format!("repos/{REPO}/branches/kitchen/issue-7"),
            json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
        );
        Ok(worker)
    }
}

fn report(worker: &ResourceRef, id: &str) -> TestResult<MailMessage> {
    Ok(MailMessage {
        id: ExternalRef::new(id)?,
        kind: MessageKind::WorkerDone,
        worker: Some(worker.clone()),
        outcome: Some(WorkerOutcome::Succeeded),
        subject: None,
        body: Some(Text::new("Done; the firmware builds.")?),
    })
}

fn acted<A>(outcome: Outcome<A>) -> TestResult<Vec<A>> {
    match outcome {
        Outcome::Acted(actions) => Ok(actions),
        Outcome::Idle | Outcome::Busy | Outcome::OwnerUncertain { .. } => {
            Err("the pass did not act".into())
        }
    }
}

// Pickup.

#[test]
fn pickup_claims_a_ready_issue_launches_once_and_is_idle_after() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    let actions = acted(kitchen.pickup(false)?)?;
    let task = kitchen.task(7)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { task: launched, attempt, .. }]
            if *launched == task && attempt.get() == 1
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    let record = kitchen.store().task(&task)?;
    assert!(matches!(
        record.state(),
        TaskState::Claimed { lease } if lease.holder().as_str() == kitchen::workflows::run::RUN_HOLDER
    ));
    // The lease is released, and the next pass finds the issue claimed.
    assert_eq!(kitchen.consumer(Pass::Pickup)?, Some(ConsumerState::Idle));
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn pickup_idle_reads_only_and_launches_nothing() -> TestResult {
    let kitchen = Kitchen::new()?;
    open_issues(kitchen.forge(), vec![issue_json(3, &["enhancement"])]);
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.execute_calls(), 0);
    // Only the inventory was read; the unready issue cost nothing more.
    assert_eq!(kitchen.forge().reads(), 1);
    assert!(kitchen.store().tasks()?.is_empty());
    Ok(())
}

#[test]
fn pickup_leaves_unspecified_blocked_and_reserved_issues() -> TestResult {
    let kitchen = Kitchen::new()?;
    open_issues(
        kitchen.forge(),
        vec![
            issue_json(4, &["ready"]),
            issue_json(5, &["ready"]),
            issue_json(6, &["ready", "human-only"]),
        ],
    );
    // 4 states no acceptance criteria; 5 is blocked by an open issue.
    ready_issue(kitchen.forge(), 4, "Just do it.");
    ready_issue(kitchen.forge(), 5, ACCEPTANCE);
    kitchen.forge().set(
        &format!("repos/{REPO}/issues/5/dependencies/blocked_by"),
        json!([issue_json(2, &[])]),
    );
    ready_issue(kitchen.forge(), 6, ACCEPTANCE);
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn pickup_duplicate_start_is_refused_by_the_lease() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    let consumer = Pass::Pickup.consumer(&repo()?)?;
    kitchen.store().acquire_consumer(
        &consumer,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        kitchen.clock.now(),
    )?;
    assert!(matches!(kitchen.pickup(false)?, Outcome::Busy));
    assert_eq!(kitchen.forge().reads(), 0);
    assert_eq!(kitchen.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn pickup_restart_after_a_crash_between_claim_and_launch_launches_once() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    // A pass took its lease and claimed issue 7, then died before launching.
    let consumer = Pass::Pickup.consumer(&repo()?)?;
    let claimant = run_claimant()?;
    let now = kitchen.clock.now();
    kitchen.store().acquire_consumer(
        &consumer,
        &claimant,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        now,
    )?;
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    let issue = IssueRef {
        repository: repo()?,
        number: kitchen::contracts::IssueNumber::new(7)?,
    };
    kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &issue,
        &claimant,
        kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
        now,
    )?;
    // While the dead pass's lease is live, a new start is refused; once it
    // expired, ownership is uncertain until a start takes it over.
    assert!(matches!(kitchen.pickup(false)?, Outcome::Busy));
    kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
    assert!(matches!(
        kitchen.pickup(false)?,
        Outcome::OwnerUncertain { .. }
    ));
    assert_eq!(kitchen.backend.execute_calls(), 0);
    let actions = acted(kitchen.pickup(true)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { attempt, .. }] if attempt.get() == 1
    ));
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn pickup_after_a_lost_launch_response_never_launches_twice() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    kitchen
        .backend
        .inject(kitchen::contracts::fake::ExecuteFault::ApplyThenLoseResponse);
    let first = acted(kitchen.pickup(false)?)?;
    assert!(matches!(
        first.as_slice(),
        [PickupAction::NotLaunched {
            outcome: kitchen::workflows::coordination::LaunchOutcome::Uncertain,
            ..
        }]
    ));
    // The attempt stays open with its intent recorded: later pickup passes
    // do not launch again, and coordination reconciles the launch.
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert!(matches!(kitchen.coordinate()?, Outcome::Acted(_)));
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.effects_performed(), 1);
    assert!(kitchen.worker(7).is_ok());
    Ok(())
}

#[test]
fn pickup_refuses_before_its_lease_what_it_cannot_run() -> TestResult {
    let kitchen = Kitchen::new()?;
    let mut settings = settings()?;
    settings.repository = Repository::new("someone/else")?;
    let outside = PickupPass {
        store: kitchen.store(),
        house: &kitchen.config,
        backend: &kitchen.backend,
        forge: &kitchen.forge,
        clock: &kitchen.clock,
        settings: &settings,
        take_over: false,
    }
    .run();
    assert!(matches!(
        outside,
        Err(kitchen::Error::Run(RunError::RepositoryOutsideHouse))
    ));
    // A backend that cannot report status cannot be supervised (#81).
    let limited = FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting([Capability::WorkerLaunchIsolated]),
    );
    let limited = Kitchen::with_backend(limited)?;
    limited.ready_seven();
    assert!(matches!(
        limited.pickup(false),
        Err(kitchen::Error::Contract(_))
    ));
    assert_eq!(limited.consumer(Pass::Pickup)?, None);
    assert_eq!(limited.forge().reads(), 0);
    Ok(())
}

#[test]
fn pickup_failed_read_relinquishes_the_lease_for_the_next_start() -> TestResult {
    let kitchen = Kitchen::new()?;
    open_issues(kitchen.forge(), vec![issue_json(7, &["ready"])]);
    // Issue 7's detail is missing: the read fails and stops the pass.
    assert!(kitchen.pickup(false).is_err());
    assert!(matches!(
        kitchen.consumer(Pass::Pickup)?,
        Some(ConsumerState::Relinquished { .. })
    ));
    // The next start adopts the relinquished lease without a takeover.
    ready_issue(kitchen.forge(), 7, ACCEPTANCE);
    assert!(matches!(kitchen.pickup(false)?, Outcome::Acted(_)));
    Ok(())
}

#[test]
fn pass_repository_defaults_to_the_only_one_and_refuses_others() -> TestResult {
    let config = house_config()?;
    assert_eq!(pass_repository(&config, None)?, repo()?);
    assert_eq!(pass_repository(&config, Some(repo()?))?, repo()?);
    assert_eq!(
        pass_repository(&config, Some(Repository::new("someone/else")?)),
        Err(RunError::RepositoryOutsideHouse)
    );
    let mut two = config;
    two.repositories
        .insert(Repository::new("origin89hq/other")?);
    assert_eq!(
        pass_repository(&two, None),
        Err(RunError::RepositoryAmbiguous)
    );
    assert_eq!("coordinate".parse::<Pass>()?, Pass::Coordinate);
    assert_eq!("tick".parse::<Pass>(), Err(RunError::UnknownPass));
    Ok(())
}

// Coordination.

#[test]
fn coordinate_is_idle_without_scheduled_tasks() -> TestResult {
    let kitchen = Kitchen::new()?;
    assert!(matches!(kitchen.coordinate()?, Outcome::Idle));
    assert_eq!(kitchen.forge().reads(), 0);
    assert_eq!(kitchen.backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn coordinate_settles_a_reported_task_and_acknowledges_the_report() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish()?;
    let actions = acted(kitchen.coordinate()?)?;
    let task = kitchen.task(7)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: task.clone(),
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    assert!(matches!(kitchen.coordinate()?, Outcome::Idle));
    Ok(())
}

#[test]
fn coordinate_keeps_a_report_until_the_backend_shows_the_worker_settled() -> TestResult {
    let kitchen = Kitchen::new()?;
    let worker = kitchen.launch_and_finish()?;
    // The report arrived before the backend shows the worker finished.
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Ready);
    let actions = acted(kitchen.coordinate()?)?;
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::Unacknowledged { .. }))
    );
    use kitchen::contracts::CoordinatorMailbox;
    assert!(kitchen.backend.next_delivery()?.is_some());
    // Once it does, the next pass settles the task from the same report.
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(7)?,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn coordinate_after_a_restart_reads_the_redelivered_report_once() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish()?;
    // The previous coordinator instance read the batch and died before
    // acknowledging it; a restarted instance adopts the run.
    use kitchen::contracts::CoordinatorMailbox;
    assert!(kitchen.backend.next_delivery()?.is_some());
    let restarted = kitchen.backend.restarted();
    let actions = acted(kitchen.coordinate_on(&restarted, false)?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(7)?,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    assert_eq!(restarted.next_delivery(), Ok(None));
    assert!(matches!(
        kitchen.backend.next_delivery(),
        Err(kitchen::contracts::MailboxError::Fenced)
    ));
    Ok(())
}

#[test]
fn coordinate_duplicate_start_is_refused_and_an_expired_owner_needs_a_takeover() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish()?;
    let consumer = Pass::Coordinate.consumer(&repo()?)?;
    kitchen.store().acquire_consumer(
        &consumer,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        kitchen.clock.now(),
    )?;
    assert!(matches!(kitchen.coordinate()?, Outcome::Busy));
    kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
    assert!(matches!(
        kitchen.coordinate()?,
        Outcome::OwnerUncertain { .. }
    ));
    assert!(matches!(
        kitchen.store().task(&kitchen.task(7)?)?.state(),
        TaskState::Claimed { .. }
    ));
    assert!(matches!(
        kitchen.coordinate_on(&kitchen.backend, true)?,
        Outcome::Acted(_)
    ));
    Ok(())
}

#[test]
fn coordinate_leaves_an_expired_task_claim_until_a_takeover() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish()?;
    let task = kitchen.task(7)?;
    kitchen.clock.advance(TASK_LEASE.as_secs() + 1);
    // The task's report stays in the mailbox for whoever continues it.
    let actions = acted(kitchen.coordinate()?)?;
    assert!(matches!(
        actions.as_slice(),
        [
            CoordinateAction::Uncertain { task: uncertain, .. },
            CoordinateAction::Unacknowledged { .. },
        ] if *uncertain == task
    ));
    let actions = acted(kitchen.coordinate_on(&kitchen.backend, true)?)?;
    assert!(actions.contains(&CoordinateAction::TakenOver { task: task.clone() }));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    Ok(())
}

#[test]
fn coordinate_on_the_house_route_reports_a_question_for_a_person() -> TestResult {
    // A backend without worker deliveries: workers post to the house store.
    let capabilities =
        CapabilitySet::supporting(Capability::ALL.into_iter().filter(|capability| {
            !matches!(
                capability,
                Capability::WorkerDeliveries | Capability::RunTransfer
            )
        }));
    let kitchen = Kitchen::with_backend(FakeBackend::new(backend_id()?, house()?, capabilities))?;
    kitchen.ready_seven();
    assert!(matches!(kitchen.pickup(false)?, Outcome::Acted(_)));
    let task = kitchen.task(7)?;
    let fence = match kitchen.store().task(&task)?.state() {
        TaskState::Claimed { lease } => lease.fence(),
        TaskState::Open | TaskState::Settled { .. } => return Err("not claimed".into()),
    };
    let question = kitchen.store().post_mail(
        &MailSender::new(task.clone(), fence.get()),
        WorkerPost {
            kind: PostKind::Question,
            subject: None,
            body: Text::new("Which bus does the driver use?")?,
        },
        kitchen.clock.now(),
    )?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Question {
        task: task.clone(),
        message: question,
    }));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: task.clone(),
        outcome: Supervision::Running(WorkerState::Starting),
    }));
    // The question stays open for `kitchn mailbox reply`.
    assert_eq!(kitchen.store().open_questions(8)?.len(), 1);
    // Its report through the house mailbox settles the task.
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen.store().post_mail(
        &MailSender::new(task.clone(), fence.get()),
        WorkerPost {
            kind: PostKind::Report {
                outcome: ReportedOutcome::Succeeded,
            },
            subject: None,
            body: Text::new("Done.")?,
        },
        kitchen.clock.now(),
    )?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    Ok(())
}

// Repair and the gate.

#[test]
fn repair_and_gate_are_idle_without_settled_work() -> TestResult {
    let kitchen = Kitchen::new()?;
    assert!(matches!(kitchen.repair()?, Outcome::Idle));
    assert!(matches!(kitchen.gate()?, Outcome::Idle));
    assert_eq!(kitchen.forge().reads(), 0);
    Ok(())
}

/// Issue 7 launched, reported, and settled, with pull request 12 open on
/// its branch.
fn settled_with_pull_request(mergeable: bool) -> TestResult<Kitchen> {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish()?;
    acted(kitchen.coordinate()?)?;
    pull_request(kitchen.forge(), 7, 12, mergeable)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/issues/7/timeline"), json!([]));
    Ok(kitchen)
}

#[test]
fn repair_hands_a_conflict_over_and_leaves_a_clean_pull_request() -> TestResult {
    let conflicting = settled_with_pull_request(false)?;
    let task = conflicting.task(7)?;
    let actions = acted(conflicting.repair()?)?;
    // The writer's checkout is not observed from a scheduled pass.
    assert_eq!(
        actions,
        [RepairAction::Decided {
            pull_request: kitchen::contracts::IssueNumber::new(12)?,
            task: task.clone(),
            decision: RepairDecision::HandOver(HandOver::WorktreeUnknown),
        }]
    );
    let clean = settled_with_pull_request(true)?;
    let actions = acted(clean.repair()?)?;
    assert_eq!(
        actions,
        [RepairAction::Decided {
            pull_request: kitchen::contracts::IssueNumber::new(12)?,
            task,
            decision: RepairDecision::Skip(Skip::Healthy),
        }]
    );
    assert_eq!(clean.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn repair_stops_on_an_unreadable_pull_request_lookup() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish()?;
    acted(kitchen.coordinate()?)?;
    // The closing references of issue 7 can no longer be read.
    kitchen
        .forge()
        .responses
        .borrow_mut()
        .remove("graphql:closing#7");
    assert!(kitchen.repair().is_err());
    assert!(matches!(
        kitchen.consumer(Pass::Repair)?,
        Some(ConsumerState::Relinquished { .. })
    ));
    Ok(())
}

#[test]
fn gate_reports_a_verdict_without_merging_on_unattested_evidence() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/main"),
        json!({"name": "main", "commit": {"sha": commit('e')?.as_str()}}),
    );
    let actions = acted(kitchen.gate()?)?;
    let [action] = actions.as_slice() else {
        return Err("one verdict expected".into());
    };
    assert_eq!(action.pull_request.get(), 12);
    assert_eq!(action.head, commit('d')?);
    assert_ne!(action.verdict, Verdict::Merge);
    // Nothing was recorded: the gate pass writes no marker.
    assert!(
        kitchen
            .store()
            .markers(&kitchen::WorkflowId::new(
                kitchen::workflows::gate::GATE_WORKFLOW
            )?)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn gate_duplicate_start_is_refused_by_the_lease() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    let reads = kitchen.forge().reads();
    kitchen.store().acquire_consumer(
        &Pass::Gate.consumer(&repo()?)?,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        kitchen.clock.now(),
    )?;
    assert!(matches!(kitchen.gate()?, Outcome::Busy));
    assert_eq!(kitchen.forge().reads(), reads);
    // The lease is per pass kind: repair still runs.
    assert!(matches!(kitchen.repair()?, Outcome::Acted(_)));
    Ok(())
}

#[test]
fn coordinate_acknowledges_a_late_message_from_a_settled_task() -> TestResult {
    let kitchen = Kitchen::new()?;
    let worker = kitchen.launch_and_finish()?;
    acted(kitchen.coordinate()?)?;
    // The worker repeats its report after the task settled; it must not
    // hold up the mailbox.
    kitchen
        .backend
        .post(vec![report(&worker, "done-7-again")?])?;
    let actions = acted(kitchen.coordinate_on(&kitchen.backend, false)?)?;
    assert_eq!(
        actions,
        [CoordinateAction::Stale {
            task: kitchen.task(7)?,
            message: ExternalRef::new("done-7-again")?,
        }]
    );
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn a_fork_pull_request_on_the_task_branch_name_is_not_kitchen_work() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    let mut fork = kitchen
        .forge()
        .responses
        .borrow()
        .get(&format!("repos/{REPO}/pulls/12"))
        .cloned()
        .ok_or("no pull request")?;
    fork["head"]["repo"] = json!({"full_name": "someone/firmware"});
    kitchen.forge().set(&format!("repos/{REPO}/pulls/12"), fork);
    assert!(matches!(kitchen.repair()?, Outcome::Idle));
    assert!(matches!(kitchen.gate()?, Outcome::Idle));
    Ok(())
}
