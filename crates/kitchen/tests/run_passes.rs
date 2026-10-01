//! Scheduled runner passes against the in-memory fake backend and a fake
//! forge that answers GitHub reads from a table. Simulated evidence only:
//! no live Orca, backend, or GitHub was contacted.

mod common;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    fmt::Write as _,
    num::NonZeroU32,
    rc::Rc,
    time::Duration,
};

use common::{ManualClock, TestResult, WORKER_PERMISSIONS, backend_id, commit, credential, house};
use kitchen::{
    CredentialId, HolderId, WorkflowId,
    contracts::{
        BranchName, Capability, CapabilitySet, CheckoutFact, CheckoutReport, Clock, Effect,
        Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict, ExternalRef, Grant, LeaseTtl,
        MailMessage, MessageKind, Operation, PostingBudget, Repository, ResourceRef, ReviewVerdict,
        Settlement, Text, WorkerOutcome, WorkerState, fake::FakeBackend,
    },
    house::HouseConfig,
    integrations::github::{
        CredentialRef, GitHubClient, GitHubExecutor, GitHubMutationTransport, GitHubReadTransport,
        HouseScope, IntegrationError, MutationRequest, ReadLimits, ReadRequest, TokenScope,
    },
    scheduling::IntervalMinutes,
    state::{
        ConsumerState, EffectOutcome, EffectState, HouseStore, MailSender, MarkerFact, MarkerKey,
        MarkerSchema, MarkerSubject, PostKind, ReportedOutcome, RiskAction, RiskDecision, RunState,
        StateError, TaskState, WorkItem, WorkerPost,
    },
    workflows::{
        coordination::{Supervision, current_worker},
        gate::Verdict,
        pickup::{IssueRef, PinnedInstructions, issue_task_id},
        repair::{HandOver, RepairDecision, Skip},
        run::{
            CoordinateAction, CoordinatePass, ForgeReview, GateAction, GateAttestation, GatePass,
            GateResult, GateReviewInput, NotMerged, Outcome, PASS_LEASE, Pass, PickupAction,
            PickupLabels, PickupPass, PickupSettings, RepairAction, RepairPass, RepairSettings,
            ReportReason, RunError, TASK_LEASE, TickPasses, Unroutable, Wait, attest_gate_review,
            pass_repository, post_gate_review, record_gate_attestation, run_claimant,
        },
        tick::{
            self, Pass as TickPass, PassFailure, PassOutcome, PassSchedule, PassTick, TickDecision,
            TickPolicy,
        },
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const REPO: &str = "origin89hq/firmware";

fn repo() -> TestResult<Repository> {
    Ok(Repository::new(REPO)?)
}

/// The shared house, granted worker lifecycle on the fake backend. House
/// policy scopes launches to a repository.
fn house_config() -> TestResult<HouseConfig> {
    house_config_with(Some(2))
}

/// [`house_config`] with `fix_rounds` repair and review-fix rounds.
fn house_config_with(fix_rounds: Option<u8>) -> TestResult<HouseConfig> {
    let mut config = common::house_with_fix_rounds(fix_rounds)?;
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
/// endpoint is unavailable, never empty. An endpoint's queued answers come
/// first, one per read. Clones share their state, as the executor a gate
/// pass builds shares the house's forge.
#[derive(Clone)]
struct Forge {
    responses: Rc<RefCell<BTreeMap<String, Value>>>,
    queued: Rc<RefCell<BTreeMap<String, VecDeque<Value>>>>,
    reads: Rc<RefCell<Vec<String>>>,
    read_accesses: Rc<RefCell<Vec<Option<TokenScope>>>>,
    /// Submitted writes: endpoint and body.
    writes: Rc<RefCell<Vec<(String, Value)>>>,
    /// While set, a write is received and its answer lost: nothing applies
    /// and the caller cannot tell.
    lose_writes: Rc<Cell<bool>>,
    /// The next review applies but its response is lost.
    lose_review_reply: Rc<Cell<bool>>,
    /// A thread reply applies, then its response is lost once.
    lose_thread_reply: Rc<Cell<bool>>,
}

impl Forge {
    fn new() -> Self {
        Self {
            responses: Rc::new(RefCell::new(BTreeMap::new())),
            queued: Rc::new(RefCell::new(BTreeMap::new())),
            reads: Rc::new(RefCell::new(Vec::new())),
            read_accesses: Rc::new(RefCell::new(Vec::new())),
            writes: Rc::new(RefCell::new(Vec::new())),
            lose_writes: Rc::new(Cell::new(false)),
            lose_review_reply: Rc::new(Cell::new(false)),
            lose_thread_reply: Rc::new(Cell::new(false)),
        }
    }

    /// Answer the next reads of `endpoint` with `values`, in order, before
    /// its table entry.
    fn queue(&self, endpoint: &str, values: Vec<Value>) {
        self.queued
            .borrow_mut()
            .insert(endpoint.to_owned(), values.into());
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
            let text = query.to_string();
            let kind = if text.contains("closedByPullRequestsReferences") {
                "closing"
            } else if text.contains("mergeStateStatus") {
                "merge-state"
            } else if text.contains("reviewThreads") {
                "threads"
            } else if text.contains("node(id:") {
                "node"
            } else {
                "other"
            };
            let subject = if kind == "node" {
                query.pointer("/variables/id").cloned().unwrap_or_default()
            } else {
                number
            };
            return format!("graphql:{kind}#{subject}");
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
        self.read_accesses
            .borrow_mut()
            .push(request.access().cloned());
        let queued = self
            .queued
            .borrow_mut()
            .get_mut(&key)
            .and_then(VecDeque::pop_front);
        let value = match queued {
            Some(value) => value,
            None => self
                .responses
                .borrow()
                .get(&key)
                .cloned()
                .ok_or(IntegrationError::Unavailable)?,
        };
        serde_json::to_vec(&value).map_err(|_| IntegrationError::Unknown)
    }
}

/// Only a squash merge at the pull request's current head applies: the
/// pull request then reads as merged and closed.
impl GitHubMutationTransport for Forge {
    fn submit(
        &self,
        credential: &CredentialRef,
        request: &MutationRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, kitchen::contracts::EffectFailure> {
        use kitchen::contracts::{EffectFailure, NotAppliedReason};
        let endpoint = request.endpoint().to_owned();
        self.writes
            .borrow_mut()
            .push((endpoint.clone(), request.body().clone()));
        if self.lose_writes.get() {
            return Err(EffectFailure::Uncertain(
                kitchen::contracts::UncertainReason::ResponseLost,
            ));
        }
        if endpoint == "graphql" {
            let query = request.body()["query"].as_str().unwrap_or_default();
            let input = &request.body()["variables"]["input"];
            let (thread, resolve) = if query.contains("addPullRequestReviewThreadReply") {
                (input["pullRequestReviewThreadId"].as_str(), false)
            } else if query.contains("resolveReviewThread") {
                (input["threadId"].as_str(), true)
            } else {
                (None, false)
            };
            let thread = thread.ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            let node_key = format!("graphql:node#{}", json!(thread));
            let mut responses = self.responses.borrow_mut();
            let node = responses
                .get_mut(&node_key)
                .and_then(|value| value.pointer_mut("/data/node"))
                .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            if resolve {
                node["isResolved"] = json!(true);
                node["resolvedBy"] = json!({"login":"kitchen-bot"});
                if let Some(list) = responses.get_mut("graphql:threads#12").and_then(|value| {
                    value.pointer_mut("/data/repository/pullRequest/reviewThreads/nodes/0")
                }) {
                    list["isResolved"] = json!(true);
                }
            } else {
                node["comments"]["nodes"].as_array_mut()
                    .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?
                    .push(json!({"id":"PRRC_reply","body":input["body"],"url":"https://github.com/origin89hq/firmware/pull/12#discussion_r2","author":{"login":"kitchen-bot"}}));
                if self.lose_thread_reply.replace(false) {
                    return Err(EffectFailure::Uncertain(
                        kitchen::contracts::UncertainReason::ResponseLost,
                    ));
                }
            }
            if resolve {
                return serde_json::to_vec(&json!({"data":{"resolveReviewThread":{
                    "clientMutationId":input["clientMutationId"],
                    "thread":{"id":thread,"isResolved":true}
                }}}))
                .map_err(|_| {
                    EffectFailure::Uncertain(kitchen::contracts::UncertainReason::ResponseLost)
                });
            }
            return Ok(b"{}".to_vec());
        }
        if endpoint.ends_with("/reviews") {
            let mut responses = self.responses.borrow_mut();
            let entries = responses
                .get_mut(&endpoint)
                .and_then(Value::as_array_mut)
                .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            entries.push(json!({"id": 19, "user": {"login": credential.requester().as_str()},
                "commit_id": request.body()["commit_id"],
                "state": if request.body()["event"] == "APPROVE" { "APPROVED" } else { "CHANGES_REQUESTED" },
                "body": request.body()["body"], "submitted_at": "1970-01-01T00:00:00Z"}));
            if self.lose_review_reply.replace(false) {
                return Err(EffectFailure::Uncertain(
                    kitchen::contracts::UncertainReason::ResponseLost,
                ));
            }
            return Ok(json!({"id":19}).to_string().into_bytes());
        }
        let pull = endpoint
            .strip_suffix("/merge")
            .map(str::to_owned)
            .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let mut responses = self.responses.borrow_mut();
        let pr = responses
            .get_mut(&pull)
            .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        if pr["head"]["sha"] != request.body()["sha"] {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        pr["merged"] = json!(true);
        pr["state"] = json!("closed");
        pr["merge_commit_sha"] = json!("9".repeat(40));
        Ok(b"{\"merged\":true}".to_vec())
    }
}

fn client(forge: Forge) -> TestResult<GitHubClient<Forge>> {
    let requester = ExternalRef::new("kitchen-bot")?;
    let scope = HouseScope::new(
        house()?,
        [repo()?],
        requester.clone(),
        CredentialRef::new(house()?, CredentialId::new("forge")?, requester),
        PostingBudget::new(3)?,
        [
            kitchen::contracts::Permission::Merge,
            kitchen::contracts::Permission::PostComment,
            kitchen::contracts::Permission::ResolveReviewThread,
        ],
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
            tick: None,
        }
        .run()
    }

    fn coordinate_on(
        &self,
        backend: &FakeBackend,
        take_over: bool,
    ) -> kitchen::Result<Outcome<CoordinateAction>> {
        let executor = GitHubExecutor::new(
            kitchen::BackendId::new("github")?,
            self.forge.scope().clone(),
            self.forge.transport().clone(),
            ReadLimits::default(),
        );
        CoordinatePass {
            store: self.store(),
            house: &self.config,
            backend,
            forge: &self.forge,
            forge_executor: Some(&executor),
            clock: &self.clock,
            take_over,
            tick: None,
        }
        .run()
    }

    fn coordinate(&self) -> kitchen::Result<Outcome<CoordinateAction>> {
        self.coordinate_on(&self.backend, false)
    }

    fn repair(&self) -> kitchen::Result<Outcome<RepairAction>> {
        self.repair_with(false)
    }

    fn follow_up(&self) -> kitchen::Result<Outcome<kitchen::workflows::run::FollowUpAction>> {
        RepairPass {
            store: self.store(),
            house: &self.config,
            backend: &self.backend,
            forge: &self.forge,
            clock: &self.clock,
            repository: &self.settings.repository,
            settings: &RepairSettings {
                instructions: self.settings.instructions.clone(),
                report_path: self.settings.report_path.clone(),
            },
            take_over: false,
            tick: None,
        }
        .run_follow_up()
    }

    fn repair_with(&self, take_over: bool) -> kitchen::Result<Outcome<RepairAction>> {
        RepairPass {
            store: self.store(),
            house: &self.config,
            backend: &self.backend,
            forge: &self.forge,
            clock: &self.clock,
            repository: &self.settings.repository,
            settings: &RepairSettings {
                instructions: self.settings.instructions.clone(),
                report_path: self.settings.report_path.clone(),
            },
            take_over,
            tick: None,
        }
        .run()
    }

    fn gate(&self) -> kitchen::Result<Outcome<kitchen::workflows::run::GateAction>> {
        GatePass {
            store: self.store(),
            house: &self.config,
            forge: &self.forge,
            forge_backend: &kitchen::BackendId::new("github").map_err(kitchen::Error::from)?,
            provenance: &self.settings.instructions.provenance,
            clock: &self.clock,
            repository: &self.settings.repository,
            authors: &["kitchen-bot".to_owned()],
            take_over: false,
            tick: None,
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

    /// The fence of task `number`'s current claim.
    fn claim_fence(&self, number: u64) -> TestResult<kitchen::contracts::Fence> {
        match self.store().task(&self.task(number)?)?.state() {
            TaskState::Claimed { lease } => Ok(lease.fence()),
            TaskState::Open | TaskState::Settled { .. } => Err("not claimed".into()),
        }
    }

    /// Begin a message effect to task `number`'s worker at `fence`, as a
    /// process holding that fence would before submitting it.
    fn message_worker_at(
        &self,
        number: u64,
        fence: kitchen::contracts::Fence,
    ) -> TestResult<kitchen::Result<kitchen::state::EffectStart>> {
        let plan = common::plan(
            &self.task(number)?,
            fence,
            "nudge",
            kitchen::contracts::Operation::MessageWorker {
                worker: self.worker(number)?,
                body: Text::new("Status?")?,
            },
        )?;
        Ok(self.store().begin_effect(
            plan,
            &self.config.authority()?,
            &self.backend,
            self.clock.now(),
        ))
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
        let worker = self.launch_and_finish_unlinked()?;
        self.store()
            .link_pull_request(&self.task(7)?, self.claim_fence(7)?, pr(12)?)?;
        Ok(worker)
    }

    fn launch_and_finish_unlinked(&self) -> TestResult<ResourceRef> {
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

/// A worker's checkout with nothing uncommitted or unpushed, as it states it.
const CLEAN_AND_PUSHED: CheckoutReport = CheckoutReport {
    clean: CheckoutFact::Yes,
    pushed: CheckoutFact::Yes,
};

/// A successful report whose worker states a clean, pushed checkout.
fn report(worker: &ResourceRef, id: &str) -> TestResult<MailMessage> {
    report_with(worker, id, CLEAN_AND_PUSHED)
}

/// A successful report stating `checkout`.
fn report_with(
    worker: &ResourceRef,
    id: &str,
    checkout: CheckoutReport,
) -> TestResult<MailMessage> {
    Ok(MailMessage {
        id: ExternalRef::new(id)?,
        kind: MessageKind::WorkerDone,
        worker: Some(worker.clone()),
        outcome: Some(WorkerOutcome::Succeeded),
        subject: None,
        body: Some(Text::new("Done; the firmware builds.")?),
        checkout,
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
fn pickup_launches_one_writer_per_repository_even_with_capacity_for_two() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.settings.capacity = 2;
    open_issues(
        kitchen.forge(),
        vec![issue_json(7, &["ready"]), issue_json(8, &["ready"])],
    );
    ready_issue(kitchen.forge(), 7, ACCEPTANCE);
    ready_issue(kitchen.forge(), 8, ACCEPTANCE);
    // Nothing tells whether 7 and 8 touch the same files: one writer only.
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { task, .. }] if *task == kitchen.task(7)?
    ));
    assert_eq!(kitchen.store().tasks()?.len(), 1);
    // While 7 is unsettled, 8 waits.
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    // Once 7 settled, the next pass takes 8.
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.backend.post(vec![report(&worker, "done-7")?])?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    acted(kitchen.coordinate()?)?;
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { task, .. }] if *task == kitchen.task(8)?
    ));
    Ok(())
}

#[test]
fn pickup_relaunches_one_of_two_waiting_tasks_per_pass() -> TestResult {
    let kitchen = Kitchen::new()?;
    open_issues(
        kitchen.forge(),
        vec![issue_json(7, &["ready"]), issue_json(8, &["ready"])],
    );
    ready_issue(kitchen.forge(), 7, ACCEPTANCE);
    ready_issue(kitchen.forge(), 8, ACCEPTANCE);
    // Two tasks were claimed without a launch, as an earlier runner could.
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    for number in [7, 8] {
        kitchen::workflows::pickup::claim_issue(
            kitchen.store(),
            &template,
            &IssueRef {
                repository: repo()?,
                number: kitchen::contracts::IssueNumber::new(number)?,
            },
            &run_claimant()?,
            kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
            kitchen.clock.now(),
        )?;
    }
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { .. }]
    ));
    // The other waits while the first has an open attempt.
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn an_old_pickup_resuming_after_a_takeover_can_neither_claim_nor_launch() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    // The old pass took its lease and claimed issue 7 under it, then
    // stalled before launching.
    let consumer = Pass::Pickup.consumer(&repo()?)?;
    let old_pass = kitchen.store().acquire_consumer(
        &consumer,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        kitchen.clock.now(),
    )?;
    let old = run_claimant()?.under(consumer.clone(), old_pass.fence());
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    let issue = |number| -> TestResult<IssueRef> {
        Ok(IssueRef {
            repository: repo()?,
            number: kitchen::contracts::IssueNumber::new(number)?,
        })
    };
    let ttl = kitchen::contracts::LeaseTtl::new(TASK_LEASE)?;
    kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &issue(7)?,
        &old,
        ttl,
        kitchen.clock.now(),
    )?;
    let old_fence = kitchen.claim_fence(7)?;
    kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
    // The task claim is still live; a takeover moves it and launches once.
    let actions = acted(kitchen.pickup(true)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { attempt, .. }] if attempt.get() == 1
    ));
    assert_ne!(kitchen.claim_fence(7)?, old_fence);
    // The old process resumes: it can claim nothing new and submit nothing
    // for the task it held.
    let effects = kitchen.backend.effects_performed();
    let claim = kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &issue(8)?,
        &old,
        ttl,
        kitchen.clock.now(),
    );
    assert!(matches!(
        claim,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert!(matches!(
        kitchen.message_worker_at(7, old_fence)?,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert_eq!(kitchen.backend.effects_performed(), effects);
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn pickup_launches_a_task_whose_attempt_started_without_a_launch() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    // An earlier pass claimed issue 7 and started its attempt, then stopped
    // before submitting the launch.
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &IssueRef {
            repository: repo()?,
            number: kitchen::contracts::IssueNumber::new(7)?,
        },
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
        kitchen.clock.now(),
    )?;
    kitchen.store().start_attempt(
        &kitchen.task(7)?,
        kitchen.claim_fence(7)?,
        kitchen.clock.now(),
    )?;
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { .. }]
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    // The launch is recorded now: the next pass leaves it to supervision.
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn pickup_retry_uses_a_fresh_branch_for_the_next_attempt() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    let first = acted(kitchen.pickup(false)?)?;
    assert!(
        matches!(first.as_slice(), [PickupAction::Launched { attempt, .. }] if attempt.get() == 1)
    );
    let task = kitchen.task(7)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no pickup worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    let mut failed = report(&worker, "failed-pickup")?;
    failed.outcome = Some(WorkerOutcome::Failed);
    kitchen.backend.post(vec![failed])?;
    acted(kitchen.coordinate()?)?;
    let second = acted(kitchen.pickup(false)?)?;
    assert!(
        matches!(second.as_slice(), [PickupAction::Launched { attempt, .. }] if attempt.get() == 2)
    );
    let branches: Vec<_> = kitchen
        .store()
        .task(&task)?
        .effects()
        .iter()
        .filter_map(|effect| match effect.request().effect() {
            Effect::Worker(Operation::LaunchWorker { branch, .. }) => {
                branch.as_ref().map(BranchName::as_str)
            }
            _ => None,
        })
        .map(str::to_owned)
        .collect();
    assert_eq!(branches, ["kitchen/issue-7", "kitchen/issue-7-attempt-2"]);
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
        tick: None,
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

/// A checked push marker has the same durable shape the push boundary writes.
fn checked_push_marker(kitchen: &Kitchen, head: &kitchen::contracts::CommitId) -> TestResult {
    push_marker_for(
        kitchen,
        &kitchen.task(7)?,
        MarkerFact::workflow(
            MarkerSchema::new("worker-push-head", NonZeroU32::MIN)?,
            head,
        )?,
    )
}

fn push_marker(kitchen: &Kitchen, fact: MarkerFact) -> TestResult {
    push_marker_for(kitchen, &kitchen.task(7)?, fact)
}

fn checked_follow_up_push(
    kitchen: &Kitchen,
    task: &kitchen::TaskId,
    head: &kitchen::contracts::CommitId,
) -> TestResult {
    push_marker_for(
        kitchen,
        task,
        MarkerFact::workflow(
            MarkerSchema::new("worker-push-head", NonZeroU32::MIN)?,
            head,
        )?,
    )
}

fn push_marker_for(kitchen: &Kitchen, task: &kitchen::TaskId, fact: MarkerFact) -> TestResult {
    let mut name = String::from("branch-");
    for byte in Sha256::digest(b"kitchen/issue-7").iter().take(16) {
        write!(name, "{byte:02x}")?;
    }
    kitchen.store().record_marker_unless(
        MarkerKey {
            workflow: WorkflowId::new("worker-push")?,
            item: WorkItem::Task { task: task.clone() },
            subject: MarkerSubject::Observation(ExternalRef::new(&name)?),
        },
        fact,
        &person_session()?,
        kitchen.clock.now(),
        |_| Ok(None::<()>),
    )?;
    Ok(())
}

#[test]
fn bad_merged_recovery_marker_does_not_block_mailbox_or_other_task() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    push_marker(
        &kitchen,
        MarkerFact::workflow(
            MarkerSchema::new("wrong-push-head", NonZeroU32::MIN)?,
            &commit('d')?,
        )?,
    )?;
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &IssueRef {
            repository: repo()?,
            number: kitchen::contracts::IssueNumber::new(8)?,
        },
        &run_claimant()?,
        LeaseTtl::new(TASK_LEASE)?,
        kitchen.clock.now(),
    )?;
    let stranger = stray(&worker, "stray-after-bad-marker")?;
    kitchen.backend.post(vec![stranger])?;
    let actions = acted(kitchen.coordinate()?)?;
    let task = kitchen.task(7)?;
    assert!(actions.iter().any(|action| matches!(action,
        CoordinateAction::RecoveryFailed { task: failed, reason }
            if failed == &task && reason.contains("marker"))));
    assert!(actions.iter().any(|action| matches!(action,
        CoordinateAction::Unroutable { message, .. }
            if message.as_str() == "stray-after-bad-marker")));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(8)?,
        outcome: Supervision::AwaitingLaunch,
    }));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn full_evidence_log_does_not_abort_merged_recovery() -> TestResult {
    let kitchen = Kitchen::new()?;
    lost_report_with_pull_request(&kitchen)?;
    let endpoint = format!("repos/{REPO}/pulls/12");
    let mut response = kitchen
        .forge()
        .responses
        .borrow()
        .get(&endpoint)
        .cloned()
        .ok_or("missing PR fixture")?;
    response["state"] = json!("closed");
    response["merged"] = json!(true);
    response["merge_commit_sha"] = json!(commit('f')?.as_str());
    kitchen.forge().set(&endpoint, response);
    let task = kitchen.task(7)?;
    let fence = kitchen.claim_fence(7)?;
    for index in 0..kitchen::state::MAX_EVIDENCE_PER_REVISION {
        kitchen.store().record_evidence(
            &task,
            fence,
            Evidence {
                kind: EvidenceKind::WorkerReport(CLEAN_AND_PUSHED),
                verdict: EvidenceVerdict::Pass,
                subject: EvidenceSubject {
                    head: commit('d')?,
                    base: None,
                },
                source: ExternalRef::new(&format!("prior-{index}"))?,
                observed_at: kitchen.clock.now(),
            },
            kitchen.clock.now(),
        )?;
    }
    let actions = acted(kitchen.coordinate()?)?;
    assert!(
        actions.iter().any(|action| matches!(action,
        CoordinateAction::RecoveryFailed { task: failed, reason }
            if failed == &task && reason.contains("evidence items per revision"))),
        "{actions:?}"
    );
    assert!(!matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    Ok(())
}

fn lost_report_with_pull_request(kitchen: &Kitchen) -> TestResult {
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    checked_push_marker(kitchen, &commit('d')?)?;
    pull_request(kitchen.forge(), 7, 12, true)?;
    Ok(())
}

#[test]
fn coordinate_settles_a_checked_merged_pr_without_a_worker_report() -> TestResult {
    let kitchen = Kitchen::new()?;
    lost_report_with_pull_request(&kitchen)?;
    let merge = commit('f')?;
    let endpoint = format!("repos/{REPO}/pulls/12");
    let mut response = kitchen
        .forge()
        .responses
        .borrow()
        .get(&endpoint)
        .cloned()
        .ok_or("missing PR fixture")?;
    response["state"] = json!("closed");
    response["merged"] = json!(true);
    response["merge_commit_sha"] = json!(merge.as_str());
    kitchen.forge().set(&endpoint, response);
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(7)?,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    let record = kitchen.store().task(&kitchen.task(7)?)?;
    assert!(matches!(
        record.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    let pushed = commit('d')?;
    assert!(record.evidence().items().iter().any(|evidence| {
        evidence.kind == EvidenceKind::ForgeMerge(merge.clone()) && evidence.subject.head == pushed
    }));
    Ok(())
}

#[test]
fn coordinate_refuses_a_merged_pr_at_another_head() -> TestResult {
    let kitchen = Kitchen::new()?;
    lost_report_with_pull_request(&kitchen)?;
    let endpoint = format!("repos/{REPO}/pulls/12");
    let mut response = kitchen
        .forge()
        .responses
        .borrow()
        .get(&endpoint)
        .cloned()
        .ok_or("missing PR fixture")?;
    response["state"] = json!("closed");
    response["merged"] = json!(true);
    response["merge_commit_sha"] = json!(commit('f')?.as_str());
    response["head"]["sha"] = json!(commit('e')?.as_str());
    kitchen.forge().set(&endpoint, response);
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(7)?,
        outcome: Supervision::Escalate(
            kitchen::workflows::coordination::Escalation::MissingEvidence
        )
    }));
    assert!(!matches!(
        kitchen.store().task(&kitchen.task(7)?)?.state(),
        TaskState::Settled { .. }
    ));
    Ok(())
}

#[test]
fn coordinate_keeps_an_open_pr_with_a_lost_report() -> TestResult {
    let kitchen = Kitchen::new()?;
    lost_report_with_pull_request(&kitchen)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(7)?,
        outcome: Supervision::Escalate(
            kitchen::workflows::coordination::Escalation::MissingEvidence
        )
    }));
    assert!(!matches!(
        kitchen.store().task(&kitchen.task(7)?)?.state(),
        TaskState::Settled { .. }
    ));
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
fn coordinate_holds_a_successful_report_until_a_pr_is_linked() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish_unlinked()?;
    delivery_comparison(&kitchen, 1)?;
    let task = kitchen.task(7)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.iter().any(|action| matches!(action,
        CoordinateAction::AwaitingDelivery { task: held, .. } if held == &task)));
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Claimed { .. }
    ));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    kitchen
        .store()
        .link_pull_request(&task, kitchen.claim_fence(7)?, pr(12)?)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    Ok(())
}

#[test]
fn a_delivery_hold_acknowledges_the_batch_and_escalates_once() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish_unlinked()?;
    delivery_comparison(&kitchen, 1)?;
    acted(kitchen.coordinate()?)?;
    // Another launched task can ask a question while the first awaits delivery.
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    let claimed = kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &IssueRef {
            repository: repo()?,
            number: pr(8)?,
        },
        &run_claimant()?,
        LeaseTtl::new(TASK_LEASE)?,
        kitchen.clock.now(),
    )?;
    assert!(matches!(
        claimed,
        kitchen::workflows::pickup::ClaimOutcome::Claimed(_)
    ));
    let (_, launched) = old_process_calls(&kitchen, 8, kitchen.claim_fence(8)?)?;
    assert!(matches!(
        launched?,
        kitchen::workflows::coordination::LaunchOutcome::Accepted { .. }
    ));
    let question = ExternalRef::new("question-8")?;
    kitchen.backend.post(vec![MailMessage {
        id: question.clone(),
        kind: MessageKind::Question,
        worker: Some(kitchen.worker(8)?),
        outcome: None,
        subject: None,
        body: Some(Text::new("Which bus?")?),
        checkout: CheckoutReport::default(),
    }])?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Question {
        task: kitchen.task(8)?,
        message: question
    }));
    kitchen.clock.advance(60 * 60 + 1);
    acted(kitchen.coordinate_on(&kitchen.backend, true)?)?;
    let questions = kitchen.store().open_questions(8)?;
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].task, kitchen.task(7)?);
    assert!(
        questions[0]
            .body
            .as_str()
            .contains("no pull request is linked")
    );
    acted(kitchen.coordinate()?)?;
    assert_eq!(kitchen.store().open_questions(8)?.len(), 1);
    Ok(())
}

fn delivery_comparison(kitchen: &Kitchen, ahead_by: u64) -> TestResult {
    kitchen
        .forge()
        .set(&format!("repos/{REPO}"), json!({"default_branch": "main"}));
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/main"),
        json!({"name": "main", "commit": {"sha": commit('c')?.as_str()}}),
    );
    kitchen.forge().set(
        &format!(
            "repos/{REPO}/compare/{}...{}",
            commit('c')?.as_str(),
            commit('d')?.as_str()
        ),
        json!({"behind_by": 0, "ahead_by": ahead_by}),
    );
    Ok(())
}

#[test]
fn coordinate_does_not_hold_an_unlinked_branch_without_new_commits() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish_unlinked()?;
    delivery_comparison(&kitchen, 0)?;
    let task = kitchen.task(7)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::AwaitingDelivery { .. }))
    );
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: task.clone(),
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    Ok(())
}

#[test]
fn coordinate_does_not_hold_a_branch_at_its_base_commit() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.launch_and_finish_unlinked()?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}"), json!({"default_branch": "main"}));
    for branch in ["main", "kitchen/issue-7"] {
        kitchen.forge().set(
            &format!("repos/{REPO}/branches/{branch}"),
            json!({"name": branch, "commit": {"sha": commit('c')?.as_str()}}),
        );
    }
    let task = kitchen.task(7)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::AwaitingDelivery { .. }))
    );
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
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
fn coordinate_moves_a_launched_task_off_the_pickup_pass_once() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let task = kitchen.task(7)?;
    let launched_at = kitchen.claim_fence(7)?;
    // The pickup pass ended, so its claim can no longer act.
    assert!(matches!(
        kitchen.message_worker_at(7, launched_at)?,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    kitchen
        .backend
        .set_worker_state(&kitchen.worker(7)?, WorkerState::Ready);
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Moved { task: task.clone() }));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: task.clone(),
        outcome: Supervision::Running(WorkerState::Ready),
    }));
    let moved = kitchen.store().task(&task)?;
    assert!(matches!(
        moved.attempts().last().map(|attempt| attempt.state()),
        Some(kitchen::state::AttemptState::Running)
    ));
    // Later passes keep the same claim.
    let owned_at = kitchen.claim_fence(7)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(!actions.contains(&CoordinateAction::Moved { task }));
    assert_eq!(kitchen.claim_fence(7)?, owned_at);
    assert!(kitchen.message_worker_at(7, owned_at)?.is_ok());
    Ok(())
}

#[test]
fn coordinate_leaves_a_task_to_a_pickup_pass_still_running() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    // A pickup pass holds its lease and has claimed issue 7 under it.
    let consumer = Pass::Pickup.consumer(&repo()?)?;
    let pass = kitchen.store().acquire_consumer(
        &consumer,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        kitchen.clock.now(),
    )?;
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    kitchen::workflows::pickup::claim_issue(
        kitchen.store(),
        &template,
        &IssueRef {
            repository: repo()?,
            number: kitchen::contracts::IssueNumber::new(7)?,
        },
        &run_claimant()?.under(consumer, pass.fence()),
        kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
        kitchen.clock.now(),
    )?;
    let claimed_at = kitchen.claim_fence(7)?;
    assert!(matches!(kitchen.coordinate()?, Outcome::Idle));
    assert_eq!(kitchen.claim_fence(7)?, claimed_at);
    Ok(())
}

#[test]
fn an_old_coordinator_resuming_after_a_takeover_is_refused() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    acted(kitchen.coordinate()?)?;
    kitchen
        .backend
        .set_worker_state(&kitchen.worker(7)?, WorkerState::Ready);
    // The old coordination pass holds its lease and the task's claim, then
    // stalls past its lease while the task claim stays live.
    let old_fence = kitchen.claim_fence(7)?;
    let consumer = Pass::Coordinate.consumer(&repo()?)?;
    let old_pass = kitchen.store().acquire_consumer(
        &consumer,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        kitchen.clock.now(),
    )?;
    kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
    let actions = acted(kitchen.coordinate_on(&kitchen.backend, true)?)?;
    let task = kitchen.task(7)?;
    assert!(actions.contains(&CoordinateAction::Moved { task: task.clone() }));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task,
        outcome: Supervision::Running(WorkerState::Ready),
    }));
    let new_fence = kitchen.claim_fence(7)?;
    assert_ne!(new_fence, old_fence);
    // The old process resumes: its lease, claim, and effects are refused.
    let effects = kitchen.backend.effects_performed();
    assert!(
        kitchen
            .store()
            .renew_consumer(
                &consumer,
                old_pass.fence(),
                kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
                kitchen.clock.now(),
            )
            .is_err()
    );
    assert!(matches!(
        kitchen.store().renew(
            &kitchen.task(7)?,
            old_fence,
            kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
            kitchen.clock.now(),
        ),
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert!(matches!(
        kitchen.message_worker_at(7, old_fence)?,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert_eq!(kitchen.backend.effects_performed(), effects);
    // The new claim continues the same attempt and still acts.
    assert!(
        kitchen
            .store()
            .continue_attempt(&kitchen.task(7)?, new_fence, kitchen.clock.now())?
            .is_some_and(|attempt| attempt.get() == 1)
    );
    let fresh = kitchen.message_worker_at(7, new_fence)?;
    assert!(fresh.is_ok(), "{fresh:?}");
    Ok(())
}

#[test]
fn transfer_keeps_a_lost_or_unobservable_worker_interrupted_until_live() -> TestResult {
    use kitchen::state::AttemptState;
    for uncertain in [
        WorkerState::Missing,
        WorkerState::Unknown,
        WorkerState::Starting,
    ] {
        let kitchen = Kitchen::new()?;
        kitchen.ready_seven();
        acted(kitchen.pickup(false)?)?;
        acted(kitchen.coordinate()?)?;
        let task = kitchen.task(7)?;
        let worker = kitchen.worker(7)?;
        kitchen.backend.set_worker_state(&worker, uncertain);
        let old_fence = kitchen.claim_fence(7)?;
        let consumer = Pass::Coordinate.consumer(&repo()?)?;
        kitchen.store().acquire_consumer(
            &consumer,
            &run_claimant()?,
            kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
            kitchen.clock.now(),
        )?;
        kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
        let actions = acted(kitchen.coordinate_on(&kitchen.backend, true)?)?;
        assert!(actions.contains(&CoordinateAction::Moved { task: task.clone() }));
        assert_ne!(kitchen.claim_fence(7)?, old_fence);
        assert!(matches!(
            kitchen
                .store()
                .task(&task)?
                .attempts()
                .last()
                .ok_or("no attempt")?
                .state(),
            AttemptState::Interrupted { .. }
        ));
        kitchen
            .backend
            .set_worker_state(&worker, WorkerState::Ready);
        acted(kitchen.coordinate()?)?;
        assert_eq!(
            kitchen
                .store()
                .task(&task)?
                .attempts()
                .last()
                .ok_or("no attempt")?
                .state(),
            AttemptState::Running
        );
    }
    Ok(())
}

/// Supervise and launch for `number` as the process holding `fence`.
fn old_process_calls(
    kitchen: &Kitchen,
    number: u64,
    fence: kitchen::contracts::Fence,
) -> TestResult<(
    kitchen::Result<Supervision>,
    kitchen::Result<kitchen::workflows::coordination::LaunchOutcome>,
)> {
    use kitchen::workflows::{
        coordination::{
            Context, Standing, SupervisionInput, SupervisionPolicy, launch_worker, supervise,
        },
        pickup::{Base, WorkerBrief, work_branch},
    };
    let grants = kitchen.config.authority()?;
    let ctx = Context {
        store: kitchen.store(),
        backend: &kitchen.backend,
        grants: &grants,
        clock: &kitchen.clock,
        consent: &Standing,
    };
    let task = kitchen.task(number)?;
    let policy = SupervisionPolicy {
        readiness_deadline: Duration::from_secs(600),
        question_deadline: Duration::from_secs(600),
        idle_deadline: Duration::from_secs(600),
        claim_ttl: kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
    };
    let brief = WorkerBrief {
        issue: IssueRef {
            repository: repo()?,
            number: kitchen::contracts::IssueNumber::new(number)?,
        },
        branch: work_branch(&format!("kitchen/issue-{number}"))?,
        base: Base::DefaultBranch,
        instructions: kitchen.settings.instructions.clone(),
        acceptance: vec![Text::new("The firmware builds with the new driver.")?],
        budget: kitchen.config.follow_up_budget(),
        report_path: kitchen.settings.report_path.clone(),
    };
    Ok((
        supervise(&ctx, &task, fence, &policy, &SupervisionInput::default()),
        launch_worker(
            &ctx,
            &task,
            fence,
            kitchen::contracts::Workspace::Isolated,
            &brief,
        ),
    ))
}

#[test]
fn an_old_process_after_a_takeover_is_stale_and_its_uncertain_launch_is_reconciled_once()
-> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    // The old pickup launches, but the response is lost: the launch effect
    // is recorded as uncertain and the worker exists on the backend.
    kitchen
        .backend
        .inject(kitchen::contracts::fake::ExecuteFault::ApplyThenLoseResponse);
    acted(kitchen.pickup(false)?)?;
    let old_fence = kitchen.claim_fence(7)?;
    let unresolved = |kitchen: &Kitchen| -> TestResult<bool> {
        Ok(kitchen
            .store()
            .task(&kitchen.task(7)?)?
            .effects()
            .iter()
            .any(|effect| {
                matches!(
                    effect.state(),
                    kitchen::state::EffectState::Intended
                        | kitchen::state::EffectState::Uncertain { .. }
                )
            }))
    };
    assert!(unresolved(&kitchen)?);
    assert_eq!(kitchen.backend.effects_performed(), 1);
    // A new coordinator takes the task over and reconciles before acting.
    let actions = acted(kitchen.coordinate()?)?;
    let task = kitchen.task(7)?;
    assert!(actions.contains(&CoordinateAction::Moved { task: task.clone() }));
    assert_ne!(kitchen.claim_fence(7)?, old_fence);
    assert!(!unresolved(&kitchen)?);
    assert!(kitchen.worker(7).is_ok());
    // The old process resumes: both calls say it lost the task, with no
    // answer that would have it continue, and nothing is launched again.
    let (supervised, launched) = old_process_calls(&kitchen, 7, old_fence)?;
    assert!(
        matches!(
            supervised,
            Err(kitchen::Error::State(StateError::StaleFence { .. }))
        ),
        "{supervised:?}"
    );
    assert!(
        matches!(
            launched,
            Err(kitchen::Error::State(StateError::StaleFence { .. }))
        ),
        "{launched:?}"
    );
    assert_eq!(kitchen.backend.effects_performed(), 1);
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    // The current owner sees the same accepted attempt and worker.
    kitchen
        .backend
        .set_worker_state(&kitchen.worker(7)?, WorkerState::Ready);
    acted(kitchen.coordinate()?)?;
    let (_, current) = old_process_calls(&kitchen, 7, kitchen.claim_fence(7)?)?;
    let expected_worker = kitchen.worker(7)?;
    assert!(
        matches!(
            &current,
            Ok(kitchen::workflows::coordination::LaunchOutcome::Accepted { attempt, worker })
                if attempt.get() == 1 && worker == &expected_worker
        ),
        "{current:?}"
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
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
                checkout: CLEAN_AND_PUSHED,
            },
            subject: None,
            body: Text::new("Done.")?,
        },
        kitchen.clock.now(),
    )?;
    kitchen
        .store()
        .link_pull_request(&task, kitchen.claim_fence(7)?, pr(12)?)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: task.clone(),
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    // The checkout the worker stated is kept on its report evidence.
    let record = kitchen.store().task(&task)?;
    assert!(
        record
            .evidence()
            .items()
            .iter()
            .any(|evidence| evidence.kind == EvidenceKind::WorkerReport(CLEAN_AND_PUSHED)),
        "{:?}",
        record.evidence()
    );
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

/// Sanitized `reviewThreads` and `node(id)` shapes from the read-only
/// `gh api graphql` check on lemarier/kitchn PR #275 for layer 1.
fn follow_up_thread(kitchen: &Kitchen) -> TestResult {
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    kitchen.forge().set("graphql:threads#12", json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
        "nodes":[{"id":"PRRT_fixture","isResolved":false,"isOutdated":false,"path":"src/lib.rs","line":10,"originalLine":10,
            "comments":{"nodes":[{"id":"PRRC_original","body":"Check the boundary.","author":{"login":"coderabbitai"}}],
                "pageInfo":{"hasNextPage":false,"endCursor":"cursor"}}}],
        "pageInfo":{"hasNextPage":false,"endCursor":"cursor"}
    }}}}}));
    kitchen.forge().set("graphql:node#\"PRRT_fixture\"", json!({"data":{"node":{
        "id":"PRRT_fixture","isResolved":false,"resolvedBy":null,
        "pullRequest":{"number":12,"headRefOid":commit('d')?.as_str(),"repository":{"nameWithOwner":REPO}},
        "comments":{"nodes":[{"id":"PRRC_original","body":"Check the boundary.","url":"https://github.com/origin89hq/firmware/pull/12#discussion_r1","author":{"login":"coderabbitai"}}],
            "pageInfo":{"hasNextPage":false,"endCursor":"cursor"}}
    }}}));
    Ok(())
}

fn grant_follow_up(kitchen: &mut Kitchen) -> TestResult {
    for permission in [
        kitchen::contracts::Permission::PostComment,
        kitchen::contracts::Permission::ResolveReviewThread,
    ] {
        let grant = Grant::repository(
            permission,
            repo()?,
            kitchen::BackendId::new("github")?,
            CredentialId::new("forge")?,
        );
        kitchen.config.policy_limits.insert(grant.clone());
        kitchen.config.grants.insert(grant);
    }
    Ok(())
}

#[test]
fn follow_up_bot_thread_launches_and_posts_fixed_resolution() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(kitchen.config.clone())?;
    grant_follow_up(&mut kitchen)?;
    green_pull_request(&kitchen)?;
    seed_attestation(&kitchen)?;
    follow_up_thread(&kitchen)?;
    let actions = acted(kitchen.follow_up()?)?;
    assert!(matches!(
        actions.as_slice(),
        [kitchen::workflows::run::FollowUpAction::Launched { round: 1, .. }]
    ));
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('f')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name":"kitchen/issue-7","commit":{"sha":commit('f')?.as_str()}}),
    );
    set_pull_request(&kitchen, "/head/sha", json!(commit('f')?.as_str()))?;
    kitchen
        .forge()
        .responses
        .borrow_mut()
        .get_mut("graphql:node#\"PRRT_fixture\"")
        .ok_or("missing thread node")?["data"]["node"]["pullRequest"]["headRefOid"] =
        json!(commit('f')?.as_str());
    let mut message = report(&worker, "done-follow-up")?;
    message.body = Some(Text::new(&json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed with a boundary regression test."}]}).to_string())?);
    kitchen.backend.post(vec![message])?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.iter().any(|action| matches!(action, CoordinateAction::Supervised { task: found, outcome: Supervision::Settled(Settlement::Succeeded) } if found == &task)), "{actions:?}");
    assert_eq!(
        kitchen
            .forge()
            .writes
            .borrow()
            .iter()
            .filter(|(endpoint, _)| endpoint == "graphql")
            .count(),
        2
    );
    assert_eq!(
        kitchen.forge().responses.borrow()["graphql:node#\"PRRT_fixture\""]["data"]["node"]["isResolved"],
        json!(true)
    );
    green_at(&kitchen, 'f')?;
    assert_eq!(
        one_verdict(kitchen.gate()?)?.result,
        GateResult::ReportOnly(ReportReason::Unattested)
    );
    Ok(())
}

#[test]
fn follow_up_declined_thread_stays_open_and_reaches_house_mailbox() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('d')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let mut message = report(&worker, "done-declined")?;
    message.body = Some(Text::new(&json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"declined","reply":"The existing range check covers this case."}]}).to_string())?);
    kitchen.backend.post(vec![message])?;
    acted(kitchen.coordinate()?)?;
    assert_eq!(
        kitchen
            .forge()
            .writes
            .borrow()
            .iter()
            .filter(|(endpoint, _)| endpoint == "graphql")
            .count(),
        1
    );
    assert_eq!(
        kitchen.forge().responses.borrow()["graphql:node#\"PRRT_fixture\""]["data"]["node"]["isResolved"],
        json!(false)
    );
    assert!(
        kitchen
            .store()
            .open_questions(512)?
            .iter()
            .any(|question| question.task == task)
    );
    Ok(())
}

#[test]
fn follow_up_refuses_missing_duplicate_unknown_and_stale_dispositions() -> TestResult {
    for body in [
        json!({"sourceHead":commit('d')?.as_str(),"dispositions":[]}),
        json!({"sourceHead":commit('d')?.as_str(),"dispositions":[
            {"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."},
            {"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]}),
        json!({"sourceHead":commit('d')?.as_str(),"dispositions":[
            {"thread":"PRRT_unknown","verdict":"fixed","reply":"Fixed."}]}),
        json!({"sourceHead":commit('a')?.as_str(),"dispositions":[
            {"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]}),
    ] {
        let mut kitchen = settled_with_pull_request(true)?;
        grant_follow_up(&mut kitchen)?;
        follow_up_thread(&kitchen)?;
        acted(kitchen.follow_up()?)?;
        let task = round_task(1)?;
        let worker = current_worker(&kitchen.store().task(&task)?)
            .ok_or("no follow-up worker")?
            .worker;
        kitchen
            .backend
            .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
        let mut message = report(&worker, "invalid-follow-up")?;
        message.body = Some(Text::new(&body.to_string())?);
        kitchen.backend.post(vec![message])?;
        assert!(
            matches!(
                kitchen.coordinate(),
                Err(kitchen::Error::Run(RunError::DispositionInvalid))
            ),
            "{body}"
        );
        assert!(kitchen.forge().writes.borrow().is_empty());
        assert!(!matches!(
            kitchen.store().task(&task)?.state(),
            TaskState::Settled { .. }
        ));
    }
    Ok(())
}

#[test]
fn follow_up_refuses_a_moved_pull_request_head_before_effects() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('d')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    set_pull_request(&kitchen, "/head/sha", json!(commit('f')?.as_str()))?;
    let mut message = report(&worker, "moved-follow-up")?;
    message.body = Some(Text::new(
        &json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]})
        .to_string(),
    )?);
    kitchen.backend.post(vec![message])?;
    assert!(matches!(
        kitchen.coordinate(),
        Err(kitchen::Error::Run(RunError::AttestationStaleHead))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn follow_up_skips_a_new_reviewer_comment_after_launch() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('d')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.forge().responses.borrow_mut().get_mut("graphql:threads#12").ok_or("missing threads")?["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"]
        .as_array_mut().ok_or("missing comments")?
        .push(json!({"id":"PRRC_new","body":"The boundary is still wrong.","author":{"login":"coderabbitai"}}));
    let mut message = report(&worker, "stale-thread")?;
    message.body = Some(Text::new(
        &json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]})
        .to_string(),
    )?);
    kitchen.backend.post(vec![message])?;
    acted(kitchen.coordinate()?)?;
    assert!(kitchen.forge().writes.borrow().is_empty());
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    Ok(())
}

#[test]
fn follow_up_skips_an_edited_reviewer_comment_after_launch() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('d')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    {
        let forge = kitchen.forge();
        let mut responses = forge.responses.borrow_mut();
        let thread = &mut responses
            .get_mut("graphql:threads#12")
            .ok_or("missing threads")?["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
            [0];
        thread["comments"]["nodes"][0]["body"] = json!("The boundary also needs a timeout.");
        thread["line"] = json!(11);
    }
    let mut message = report(&worker, "edited-thread")?;
    message.body = Some(Text::new(
        &json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]})
        .to_string(),
    )?);
    kitchen.backend.post(vec![message])?;
    acted(kitchen.coordinate()?)?;
    assert!(kitchen.forge().writes.borrow().is_empty());
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    assert!(matches!(kitchen.follow_up()?, Outcome::Acted(_)));
    Ok(())
}

#[test]
fn follow_up_requires_the_rounds_checked_push_before_replying() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let mut message = report(&worker, "unverified-push")?;
    message.body = Some(Text::new(
        &json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]})
        .to_string(),
    )?);
    kitchen.backend.post(vec![message])?;
    assert!(matches!(
        kitchen.coordinate(),
        Err(kitchen::Error::Run(RunError::FollowUpPushMissing))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn follow_up_waits_for_worker_settlement_before_external_effects() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('f')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Ready);
    let mut message = report(&worker, "worker-still-active")?;
    message.body = Some(Text::new(
        &json!({"sourceHead":commit('d')?.as_str(),
            "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed."}]})
        .to_string(),
    )?);
    kitchen.backend.post(vec![message])?;
    assert!(matches!(
        kitchen.coordinate(),
        Err(kitchen::Error::Run(RunError::FollowUpPushMissing))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn follow_up_change_request_without_thread_uses_an_empty_disposition_set() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    follow_up_thread(&kitchen)?;
    kitchen.forge().set(
        "graphql:threads#12",
        json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
            "nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}
        }}}}}),
    );
    kitchen.forge().set(&format!("repos/{REPO}/pulls/12/reviews"), json!([{
        "id":41,"user":{"login":"coderabbitai"},"commit_id":commit('d')?.as_str(),
        "state":"CHANGES_REQUESTED","body":"Add a boundary test.","submitted_at":"1970-01-01T00:00:00Z"
    }]));
    let actions = acted(kitchen.follow_up()?)?;
    assert!(matches!(
        actions.as_slice(),
        [kitchen::workflows::run::FollowUpAction::Launched { .. }]
    ));
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('d')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    let mut message = report(&worker, "review-only")?;
    message.body = Some(Text::new(
        &json!({"sourceHead":commit('d')?.as_str(),"dispositions":[]}).to_string(),
    )?);
    kitchen.backend.post(vec![message])?;
    acted(kitchen.coordinate()?)?;
    assert!(matches!(kitchen.follow_up()?, Outcome::Idle));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn follow_up_waits_for_another_repository_writer() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    follow_up_thread(&kitchen)?;
    open_issues(kitchen.forge(), vec![issue_json(8, &["ready"])]);
    ready_issue(kitchen.forge(), 8, ACCEPTANCE);
    acted(kitchen.pickup(false)?)?;
    let actions = acted(kitchen.follow_up()?)?;
    assert!(matches!(
        actions.as_slice(),
        [kitchen::workflows::run::FollowUpAction::Waiting {
            reason: Wait::WriterOpen,
            ..
        }]
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    Ok(())
}

#[test]
fn follow_up_exhausted_budget_asks_the_house_owner_once() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = house_config_with(Some(0))?;
    follow_up_thread(&kitchen)?;
    let first = acted(kitchen.follow_up()?)?;
    assert!(matches!(
        first.as_slice(),
        [kitchen::workflows::run::FollowUpAction::Exhausted { .. }]
    ));
    let questions = kitchen.store().open_questions(512)?;
    assert_eq!(questions.len(), 1);
    kitchen.clock.advance(TASK_LEASE.as_secs() + 1);
    let second = acted(kitchen.follow_up()?)?;
    assert!(matches!(
        second.as_slice(),
        [kitchen::workflows::run::FollowUpAction::Exhausted { .. }]
    ));
    assert_eq!(kitchen.store().open_questions(512)?.len(), 1);
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn follow_up_reconciles_a_lost_reply_after_coordinator_restart() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    acted(kitchen.follow_up()?)?;
    let task = round_task(1)?;
    checked_follow_up_push(&kitchen, &task, &commit('f')?)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no follow-up worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name":"kitchen/issue-7","commit":{"sha":commit('f')?.as_str()}}),
    );
    set_pull_request(&kitchen, "/head/sha", json!(commit('f')?.as_str()))?;
    kitchen
        .forge()
        .responses
        .borrow_mut()
        .get_mut("graphql:node#\"PRRT_fixture\"")
        .ok_or("missing thread node")?["data"]["node"]["pullRequest"]["headRefOid"] =
        json!(commit('f')?.as_str());
    kitchen.forge().lose_thread_reply.set(true);
    let mut message = report(&worker, "lost-follow-up")?;
    message.body = Some(Text::new(&json!({"sourceHead":commit('d')?.as_str(),
        "dispositions":[{"thread":"PRRT_fixture","verdict":"fixed","reply":"Fixed with a regression test."}]}).to_string())?);
    kitchen.backend.post(vec![message])?;
    assert!(matches!(
        kitchen.coordinate(),
        Err(kitchen::Error::Run(RunError::FollowUpEffectUncertain))
    ));
    assert!(!matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    let resumed = acted(kitchen.coordinate()?)?;
    assert!(resumed.iter().any(|action| matches!(action, CoordinateAction::Supervised { task: found, outcome: Supervision::Settled(Settlement::Succeeded) } if found == &task)));
    let writes = kitchen.forge().writes.borrow();
    assert_eq!(
        writes
            .iter()
            .filter(|(_, body)| body["query"]
                .as_str()
                .is_some_and(|query| query.contains("addPullRequestReviewThreadReply")))
            .count(),
        1
    );
    assert_eq!(
        writes
            .iter()
            .filter(|(_, body)| body["query"]
                .as_str()
                .is_some_and(|query| query.contains("resolveReviewThread")))
            .count(),
        1
    );
    Ok(())
}

fn pr(number: u64) -> TestResult<kitchen::contracts::IssueNumber> {
    Ok(kitchen::contracts::IssueNumber::new(number)?)
}

/// Repair round `round` of pull request 12.
fn round_task(round: u8) -> TestResult<kitchen::TaskId> {
    Ok(kitchen::workflows::repair::repair_task_id(
        &repo()?,
        pr(12)?,
        round,
    )?)
}

/// The brief of the task's latest launch.
fn launch_brief(kitchen: &Kitchen, task: &kitchen::TaskId) -> TestResult<String> {
    let record = kitchen.store().task(task)?;
    record
        .effects()
        .iter()
        .rev()
        .find_map(|effect| match effect.request().effect() {
            kitchen::contracts::Effect::Worker(kitchen::contracts::Operation::LaunchWorker {
                brief,
                ..
            }) => Some(brief.as_str().to_owned()),
            _ => None,
        })
        .ok_or_else(|| "no launch".into())
}

#[test]
fn repair_launches_one_writer_for_a_conflict_within_the_budget() -> TestResult {
    let kitchen = settled_with_pull_request(false)?;
    kitchen.forge().set(
        &format!("repos/{REPO}/pulls/12/reviews"),
        json!([{"id": 31, "user": {"login": "safety-reviewer"}, "commit_id": commit('d')?.as_str(),
            "state": "CHANGES_REQUESTED", "body": "The merge drops the watchdog reset.",
            "submitted_at": "1970-01-01T00:00:00Z"}]),
    );
    let actions = acted(kitchen.repair()?)?;
    let task = round_task(1)?;
    assert!(
        matches!(
            actions.as_slice(),
            [RepairAction::Launched { pull_request, task: launched, round: 1, attempt, .. }]
                if pull_request.get() == 12 && *launched == task && attempt.get() == 1
        ),
        "{actions:?}"
    );
    // The pickup writer and one repair writer.
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    let record = kitchen.store().task(&task)?;
    assert_eq!(
        record.created_by().holder.as_str(),
        kitchen::workflows::run::RUN_HOLDER
    );
    let brief = launch_brief(&kitchen, &task)?;
    assert!(brief.contains("repair round 1 of 2"), "{brief}");
    assert!(
        brief.contains("existing branch `kitchen/issue-7`"),
        "{brief}"
    );
    assert!(
        brief.contains("> The merge drops the watchdog reset."),
        "{brief}"
    );
    // The round's writer is running: the next pass launches nothing, and
    // pickup counts it as the repository's writer.
    let again = acted(kitchen.repair()?)?;
    assert!(
        matches!(
            again.as_slice(),
            [RepairAction::Decided {
                decision: RepairDecision::Skip(Skip::WriterActive),
                ..
            }]
        ),
        "{again:?}"
    );
    kitchen.ready_seven();
    open_issues(kitchen.forge(), vec![issue_json(8, &["ready"])]);
    ready_issue(kitchen.forge(), 8, ACCEPTANCE);
    assert!(!matches!(
        kitchen.pickup(false)?,
        Outcome::Acted(actions) if actions.iter().any(|action| matches!(action, PickupAction::Launched { .. }))
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    // Once coordination settles the round from its report, the clean pull
    // request needs nothing more.
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no repair worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen
        .backend
        .post(vec![report(&worker, "done-repair")?])?;
    acted(kitchen.coordinate()?)?;
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    pull_request(kitchen.forge(), 7, 12, true)?;
    let healthy = acted(kitchen.repair()?)?;
    assert!(matches!(
        healthy.as_slice(),
        [RepairAction::Decided {
            decision: RepairDecision::Skip(Skip::Healthy),
            ..
        }]
    ));
    Ok(())
}

#[test]
fn a_repair_round_waiting_for_its_next_attempt_blocks_no_writer() -> TestResult {
    let kitchen = settled_with_pull_request(false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    acted(kitchen.repair()?)?;
    let task = round_task(1)?;
    // The repair writer fails; its round waits for a next attempt.
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no repair worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    let mut failed = report(&worker, "failed-repair")?;
    failed.outcome = Some(WorkerOutcome::Failed);
    kitchen.backend.post(vec![failed])?;
    acted(kitchen.coordinate()?)?;
    assert!(!matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    // A person closes the pull request, so no repair pass relaunches the
    // round. Pickup still takes the next ready issue.
    let mut closed = kitchen
        .forge()
        .responses
        .borrow()
        .get(&format!("repos/{REPO}/pulls/12"))
        .cloned()
        .ok_or("no pull request")?;
    closed["state"] = json!("closed");
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12"), closed);
    open_issues(kitchen.forge(), vec![issue_json(8, &["ready"])]);
    ready_issue(kitchen.forge(), 8, ACCEPTANCE);
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(
        matches!(actions.as_slice(), [PickupAction::Launched { .. }]),
        "{actions:?}"
    );
    Ok(())
}

/// Round 1 of pull request 12 launched a repair writer, which failed: the
/// round waits for its next attempt.
fn failed_repair_round() -> TestResult<Kitchen> {
    let kitchen = settled_with_pull_request(false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    acted(kitchen.repair()?)?;
    let task = round_task(1)?;
    let worker = current_worker(&kitchen.store().task(&task)?)
        .ok_or("no repair worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    let mut failed = report(&worker, "failed-repair")?;
    failed.outcome = Some(WorkerOutcome::Failed);
    kitchen.backend.post(vec![failed])?;
    acted(kitchen.coordinate()?)?;
    assert!(!matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    Ok(kitchen)
}

#[test]
fn a_failed_repair_round_is_retried_only_through_the_repair_decision() -> TestResult {
    let decided = |decision| -> TestResult<[RepairAction; 1]> {
        Ok([RepairAction::Decided {
            pull_request: pr(12)?,
            task: issue_task_id(&IssueRef {
                repository: repo()?,
                number: pr(7)?,
            })?,
            decision,
        }])
    };
    // The failed writer is the branch's latest, and it reported no clean,
    // pushed checkout at the head: its work may be unpushed.
    let kitchen = failed_repair_round()?;
    assert_eq!(
        acted(kitchen.repair()?)?,
        decided(RepairDecision::HandOver(HandOver::WorktreeUnknown))?
    );
    // Someone resolves the conflict: the round needs no writer.
    pull_request(kitchen.forge(), 7, 12, true)?;
    assert_eq!(
        acted(kitchen.repair()?)?,
        decided(RepairDecision::Skip(Skip::Healthy))?
    );
    // The backend loses track of the failed writer: it is not known to have
    // stopped.
    let kitchen = failed_repair_round()?;
    let worker = current_worker(&kitchen.store().task(&round_task(1)?)?)
        .ok_or("no repair worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Missing);
    assert_eq!(
        acted(kitchen.repair()?)?,
        decided(RepairDecision::Skip(Skip::WriterActive))?
    );
    // None of these launched another writer or started another attempt.
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    assert_eq!(kitchen.store().task(&round_task(1)?)?.attempts().len(), 1);
    Ok(())
}

#[test]
fn a_repair_round_whose_launch_was_refused_is_launched_by_the_next_pass() -> TestResult {
    let kitchen = settled_with_pull_request(false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    // The backend refuses the launch: the round has no writer and waits.
    kitchen
        .backend
        .inject(kitchen::contracts::fake::ExecuteFault::Reject);
    let refused = acted(kitchen.repair()?)?;
    assert!(
        matches!(refused.as_slice(), [RepairAction::NotLaunched { .. }]),
        "{refused:?}"
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    // The pickup worker is still the branch's latest writer, and its report
    // stands: the next pass launches the round's second attempt.
    let retried = acted(kitchen.repair()?)?;
    assert!(
        matches!(
            retried.as_slice(),
            [RepairAction::Launched { task, round: 1, attempt, .. }]
                if *task == round_task(1)? && attempt.get() == 2
        ),
        "{retried:?}"
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    Ok(())
}

/// A person's interactive session.
fn person_session() -> TestResult<kitchen::contracts::Claimant> {
    Ok(kitchen::contracts::Claimant {
        holder: HolderId::new("session-dana")?,
        trigger: kitchen::contracts::Trigger::Interactive,
        consumer: None,
    })
}

/// A person holds `pr` round 1 of pull request `number` interactively, as
/// `kitchn pr` claims it: no worker launch is recorded.
fn person_holds_round(
    kitchen: &Kitchen,
    number: u64,
) -> TestResult<(kitchen::TaskId, kitchen::contracts::Fence)> {
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    let task = kitchen::workflows::repair::repair_task_id(&repo()?, pr(number)?, 1)?;
    let work_type = kitchen::selection::WorkType::fix();
    let spec = kitchen::contracts::TaskSpec {
        id: task.clone(),
        role: kitchen::contracts::Role::StationCook,
        repository: Some(repo()?),
        authority: template.authority.clone(),
        retry: template.retry,
        provenance: template.provenance.clone(),
        resources: std::collections::BTreeSet::new(),
        requires: kitchen::contracts::CapabilityRequirements::new(),
        agent: None,
        work_type: Some(work_type),
    };
    let person = person_session()?;
    kitchen
        .store()
        .create_task(spec, &person, kitchen.clock.now())?;
    let lease = kitchen.store().claim(
        &task,
        &person,
        kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
        kitchen.clock.now(),
    )?;
    Ok((task, lease.fence()))
}

#[test]
fn repair_waits_while_a_person_holds_a_round_of_another_pull_request() -> TestResult {
    let kitchen = settled_with_pull_request(false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    // A person repairs pull request 40 of the same repository through
    // `kitchn pr`; nothing tells whether it touches pull request 12's files.
    let (held, fence) = person_holds_round(&kitchen, 40)?;
    let actions = acted(kitchen.repair()?)?;
    assert_eq!(
        actions,
        [RepairAction::Waiting {
            pull_request: pr(12)?,
            task: round_task(1)?,
            wait: Wait::WriterOpen,
        }]
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    assert!(matches!(
        kitchen.store().task(&round_task(1)?),
        Err(kitchen::Error::State(StateError::TaskNotFound(_)))
    ));
    // Once the person hands the round back, the scheduled writer launches.
    kitchen
        .store()
        .relinquish(&held, fence, kitchen.clock.now())?;
    let actions = acted(kitchen.repair()?)?;
    assert!(
        matches!(
            actions.as_slice(),
            [RepairAction::Launched { round: 1, .. }]
        ),
        "{actions:?}"
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    Ok(())
}

#[test]
fn pickup_picks_nothing_while_a_person_holds_a_pull_request_round() -> TestResult {
    let kitchen = Kitchen::new()?;
    let (held, fence) = person_holds_round(&kitchen, 40)?;
    kitchen.ready_seven();
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 0);
    assert!(matches!(
        kitchen.store().task(&kitchen.task(7)?),
        Err(kitchen::Error::State(StateError::TaskNotFound(_)))
    ));
    kitchen
        .store()
        .relinquish(&held, fence, kitchen.clock.now())?;
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(
        matches!(actions.as_slice(), [PickupAction::Launched { task, .. }] if *task == kitchen.task(7)?),
        "{actions:?}"
    );
    Ok(())
}

#[test]
fn pickup_relaunches_nothing_while_a_person_works_another_issue() -> TestResult {
    let kitchen = Kitchen::new()?;
    open_issues(
        kitchen.forge(),
        vec![issue_json(7, &["ready"]), issue_json(8, &["ready"])],
    );
    ready_issue(kitchen.forge(), 7, ACCEPTANCE);
    ready_issue(kitchen.forge(), 8, ACCEPTANCE);
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::Backend,
        kitchen.settings.instructions.provenance.clone(),
    )?;
    let ttl = kitchen::contracts::LeaseTtl::new(TASK_LEASE)?;
    let claim = |number, claimant: &kitchen::contracts::Claimant| -> TestResult<_> {
        Ok(kitchen::workflows::pickup::claim_issue(
            kitchen.store(),
            &template,
            &IssueRef {
                repository: repo()?,
                number: kitchen::contracts::IssueNumber::new(number)?,
            },
            claimant,
            ttl,
            kitchen.clock.now(),
        )?)
    };
    // The runner claimed 7 without a launch; a person works 8 through
    // `kitchn work`, with no launch recorded.
    claim(7, &run_claimant()?)?;
    let person = claim(8, &person_session()?)?;
    let kitchen::workflows::pickup::ClaimOutcome::Claimed(lease) = person else {
        return Err(format!("the person did not claim 8: {person:?}").into());
    };
    assert!(matches!(kitchen.pickup(false)?, Outcome::Idle));
    assert_eq!(kitchen.backend.launched_agents().len(), 0);
    kitchen
        .store()
        .relinquish(&kitchen.task(8)?, lease.fence(), kitchen.clock.now())?;
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(
        matches!(actions.as_slice(), [PickupAction::Launched { task, .. }] if *task == kitchen.task(7)?),
        "{actions:?}"
    );
    Ok(())
}

#[test]
fn repair_launches_nothing_once_the_house_budget_is_spent() -> TestResult {
    let mut kitchen = settled_with_pull_request(false)?;
    kitchen.config = house_config_with(Some(1))?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    acted(kitchen.repair()?)?;
    let worker = current_worker(&kitchen.store().task(&round_task(1)?)?)
        .ok_or("no repair worker")?
        .worker;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen
        .backend
        .post(vec![report(&worker, "done-repair")?])?;
    acted(kitchen.coordinate()?)?;
    // The pull request conflicts again after its one round.
    let actions = acted(kitchen.repair()?)?;
    assert_eq!(
        actions,
        [RepairAction::Decided {
            pull_request: pr(12)?,
            task: kitchen.task(7)?,
            decision: RepairDecision::HandOver(HandOver::BudgetExhausted),
        }]
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    assert!(matches!(
        kitchen.store().task(&round_task(2)?),
        Err(kitchen::Error::State(StateError::TaskNotFound(_)))
    ));
    Ok(())
}

#[test]
fn repair_hands_over_when_the_head_is_not_the_reported_one() -> TestResult {
    let kitchen = settled_with_pull_request(false)?;
    // Someone pushed after the worker's report: its checkout is not known
    // to hold nothing unpushed.
    let mut moved = kitchen
        .forge()
        .responses
        .borrow()
        .get(&format!("repos/{REPO}/pulls/12"))
        .cloned()
        .ok_or("no pull request")?;
    moved["head"]["sha"] = json!(commit('f')?.as_str());
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12"), moved);
    let actions = acted(kitchen.repair()?)?;
    assert_eq!(
        actions,
        [RepairAction::Decided {
            pull_request: pr(12)?,
            task: kitchen.task(7)?,
            decision: RepairDecision::HandOver(HandOver::WorktreeUnknown),
        }]
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

/// Issue 7 settled on a report stating `checkout`, with pull request 12
/// conflicting at the reported head.
fn settled_stating(checkout: CheckoutReport) -> TestResult<Kitchen> {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen
        .backend
        .post(vec![report_with(&worker, "done-7", checkout)?])?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    acted(kitchen.coordinate()?)?;
    assert!(matches!(
        kitchen.store().task(&kitchen.task(7)?)?.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    pull_request(kitchen.forge(), 7, 12, false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/issues/7/timeline"), json!([]));
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    Ok(kitchen)
}

#[test]
fn repair_hands_over_unless_the_report_states_a_clean_pushed_checkout() -> TestResult {
    let stated = |clean, pushed| CheckoutReport { clean, pushed };
    for checkout in [
        // Uncommitted work left in the checkout.
        stated(CheckoutFact::No, CheckoutFact::Yes),
        // Commits not pushed, or the checkout ahead of the remote.
        stated(CheckoutFact::Yes, CheckoutFact::No),
        // A report that says nothing about its checkout, as Orca's
        // worker_done and every report before this field.
        CheckoutReport::default(),
        stated(CheckoutFact::Yes, CheckoutFact::Unknown),
    ] {
        let kitchen = settled_stating(checkout)?;
        let actions = acted(kitchen.repair()?)?;
        assert_eq!(
            actions,
            [RepairAction::Decided {
                pull_request: pr(12)?,
                task: kitchen.task(7)?,
                decision: RepairDecision::HandOver(HandOver::WorktreeUnknown),
            }],
            "{checkout:?}"
        );
        assert_eq!(kitchen.backend.launched_agents().len(), 1, "{checkout:?}");
    }
    // Stated clean and pushed at the head, the repair writer launches.
    let kitchen = settled_stating(CLEAN_AND_PUSHED)?;
    let actions = acted(kitchen.repair()?)?;
    assert!(
        matches!(
            actions.as_slice(),
            [RepairAction::Launched { round: 1, .. }]
        ),
        "{actions:?}"
    );
    Ok(())
}

#[test]
fn checked_push_survives_orcas_empty_completion_and_allows_follow_up() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.backend.post(vec![report_with(
        &worker,
        "done-7",
        CheckoutReport::default(),
    )?])?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    kitchen.store().record_evidence(
        &kitchen.task(7)?,
        kitchen.claim_fence(7)?,
        Evidence {
            kind: EvidenceKind::WorkerReport(CLEAN_AND_PUSHED),
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: commit('d')?,
                base: None,
            },
            source: ExternalRef::new("checked-push-1")?,
            observed_at: kitchen.clock.now(),
        },
        kitchen.clock.now(),
    )?;
    acted(kitchen.coordinate()?)?;
    pull_request(kitchen.forge(), 7, 12, false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/issues/7/timeline"), json!([]));
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    grant_follow_up(&mut kitchen)?;
    follow_up_thread(&kitchen)?;
    let follow_up = acted(kitchen.follow_up()?)?;
    assert!(
        matches!(
            follow_up.as_slice(),
            [kitchen::workflows::run::FollowUpAction::Launched { round: 1, .. }]
        ),
        "{follow_up:?}"
    );
    Ok(())
}

#[test]
fn owner_preservation_is_bound_to_the_settled_pr_head() -> TestResult {
    let kitchen = settled_stating(CheckoutReport::default())?;
    assert!(matches!(
        kitchen.store().record_owner_preservation(
            &kitchen.task(7)?,
            pr(12)?,
            commit('e')?,
            ExternalRef::new("owner-preserved-person")?,
            kitchen.clock.now(),
        ),
        Err(kitchen::Error::State(StateError::PreservationMismatch))
    ));
    kitchen.store().record_owner_preservation(
        &kitchen.task(7)?,
        pr(12)?,
        commit('d')?,
        ExternalRef::new("owner-preserved-person")?,
        kitchen.clock.now(),
    )?;
    let actions = acted(kitchen.repair()?)?;
    assert!(
        matches!(
            actions.as_slice(),
            [RepairAction::Launched { round: 1, .. }]
        ),
        "{actions:?}"
    );
    Ok(())
}

#[test]
fn reports_recorded_before_the_checkout_field_read_as_unknown() -> TestResult {
    assert_eq!(
        serde_json::from_str::<EvidenceKind>(r#""worker-report""#)?,
        EvidenceKind::WorkerReport(CheckoutReport::default())
    );
    let current = EvidenceKind::WorkerReport(CLEAN_AND_PUSHED);
    assert_eq!(
        serde_json::from_str::<EvidenceKind>(&serde_json::to_string(&current)?)?,
        current
    );
    assert_eq!(
        serde_json::from_str::<EvidenceKind>(r#""check""#)?,
        EvidenceKind::Check
    );
    assert_eq!(
        serde_json::from_str::<PostKind>(r#"{"type":"report","outcome":"succeeded"}"#)?,
        PostKind::Report {
            outcome: ReportedOutcome::Succeeded,
            checkout: CheckoutReport::default(),
        }
    );
    // A statement Kitchen does not know is refused, not read as a yes.
    assert!(
        serde_json::from_str::<EvidenceKind>(
            r#"{"worker-report":{"clean":"mostly","pushed":"yes"}}"#
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn repair_takeover_mid_pass_launches_the_claimed_round_once() -> TestResult {
    let kitchen = settled_with_pull_request(false)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    // A repair pass took its lease, created and claimed round 1 under it,
    // then died before launching.
    let consumer = Pass::Repair.consumer(&repo()?)?;
    let now = kitchen.clock.now();
    let lease = kitchen.store().acquire_consumer(
        &consumer,
        &run_claimant()?,
        kitchen::contracts::LeaseTtl::new(PASS_LEASE)?,
        now,
    )?;
    let dead = run_claimant()?.under(consumer, lease.fence());
    let template = kitchen::workflows::run::task_template(
        &kitchen.config,
        kitchen::workflows::coordination::MailboxRoute::select(
            <FakeBackend as kitchen::contracts::EffectExecutor>::descriptor(&kitchen.backend),
        ),
        kitchen.settings.instructions.provenance.clone(),
    )?;
    let task = round_task(1)?;
    let fix = kitchen::selection::WorkType::fix();
    kitchen.store().create_task(
        kitchen::contracts::TaskSpec {
            id: task.clone(),
            role: kitchen::contracts::Role::StationCook,
            repository: Some(repo()?),
            authority: template.authority.clone(),
            retry: template.retry,
            provenance: template.provenance.clone(),
            resources: std::collections::BTreeSet::new(),
            requires: template.requires.clone(),
            agent: kitchen::workflows::pickup::resolve_agent(
                template.agents.as_ref(),
                kitchen::contracts::Role::StationCook,
                &fix,
                &repo()?,
            ),
            work_type: Some(fix),
        },
        &dead,
        now,
    )?;
    let old = kitchen
        .store()
        .claim(
            &task,
            &dead,
            kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
            now,
        )?
        .fence();
    assert!(matches!(kitchen.repair()?, Outcome::Busy));
    kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
    assert!(matches!(kitchen.repair()?, Outcome::OwnerUncertain { .. }));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    let actions = acted(kitchen.repair_with(true)?)?;
    assert!(
        matches!(
            actions.as_slice(),
            [RepairAction::Launched { task: launched, round: 1, attempt, .. }]
                if *launched == task && attempt.get() == 1
        ),
        "{actions:?}"
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
    // The dead pass's claim can no longer act on the round.
    assert!(matches!(
        kitchen
            .store()
            .start_attempt(&task, old, kitchen.clock.now()),
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    // Nothing launches twice.
    let again = acted(kitchen.repair()?)?;
    assert!(
        !again
            .iter()
            .any(|action| matches!(action, RepairAction::Launched { .. }))
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 2);
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

/// The forge shows pull request 12 ready at head `d` on base `e`
/// ([`green_at`]).
fn green_pull_request(kitchen: &Kitchen) -> TestResult {
    green_at(kitchen, 'd')
}

/// Change one field of pull request 12 as the forge shows it.
fn set_pull_request(kitchen: &Kitchen, pointer: &str, value: Value) -> TestResult {
    let mut responses = kitchen.forge().responses.borrow_mut();
    let field = responses
        .get_mut(&format!("repos/{REPO}/pulls/12"))
        .and_then(|pull_request| pull_request.pointer_mut(pointer))
        .ok_or("no such pull request field")?;
    *field = value;
    Ok(())
}

/// Pull request 12 as the forge shows it now.
fn pull_request_now(kitchen: &Kitchen) -> TestResult<Value> {
    Ok(kitchen
        .forge()
        .responses
        .borrow()
        .get(&format!("repos/{REPO}/pulls/12"))
        .cloned()
        .ok_or("no pull request")?)
}

/// The forge shows pull request 12 ready at head `at` on base `e`
/// ([`green_on`]).
fn green_at(kitchen: &Kitchen, at: char) -> TestResult {
    green_on(kitchen, at, 'e')
}

/// The forge shows pull request 12 ready at head `at` on a base branch
/// whose tip is `base`: checks green, the required reviewer approved the
/// head, nothing outstanding.
fn green_on(kitchen: &Kitchen, at: char, base: char) -> TestResult {
    let forge = kitchen.forge();
    let head = commit(at)?;
    let head = head.as_str();
    set_pull_request(kitchen, "/base/sha", json!(commit(base)?.as_str()))?;
    forge.set(
        &format!("repos/{REPO}/branches/main"),
        json!({"name": "main", "commit": {"sha": commit(base)?.as_str()}}),
    );
    forge.set(&format!("repos/{REPO}"), json!({"default_branch": "main"}));
    forge.set(
        "graphql:merge-state#12",
        json!({"data": {"repository": {"pullRequest": {"headRefOid": head, "mergeStateStatus": "CLEAN"}}}}),
    );
    forge.set(
        &format!("repos/{REPO}/compare/{}...{head}", commit(base)?.as_str()),
        json!({"behind_by": 0, "ahead_by": 1}),
    );
    forge.set(
        &format!("repos/{REPO}/commits/{head}/check-runs"),
        json!({"check_runs": [{"name": "build", "head_sha": head, "status": "completed", "conclusion": "success"}]}),
    );
    forge.set(&format!("repos/{REPO}/commits/{head}/statuses"), json!([]));
    forge.set(
        &format!("repos/{REPO}/branches/main/protection/required_status_checks"),
        json!({"contexts": ["build"], "checks": []}),
    );
    forge.set(
        &format!("repos/{REPO}/pulls/12/reviews"),
        json!([{"id": 11, "user": {"login": "safety-reviewer"}, "commit_id": head,
            "state": "APPROVED", "body": review_block(at, base)?,
            "submitted_at": "1970-01-01T00:00:00Z"}]),
    );
    forge.set(
        "graphql:threads#12",
        json!({"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": [],
            "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}}),
    );
    forge.set(
        &format!("repos/{REPO}/commits/{head}"),
        json!({"sha": head, "commit": {"committer": {"date": "1970-01-01T00:00:00Z"}}}),
    );
    forge.set(&format!("repos/{REPO}/issues/12/timeline"), json!([]));
    // One commit, which the forge attributes to the house's own login.
    commits(
        kitchen,
        json!([commit_json(at, Some("kitchen-bot"), Some("kitchen-bot"))?]),
    );
    Ok(())
}

/// A pull request commit as the forge lists it: `author` and `committer`
/// are the logins it links, or `None` where it links no account.
fn review_block(head: char, base: char) -> TestResult<String> {
    Ok(format!(
        "```kitchen-attestation\nhead={}\nbase={}\nsemantic=clean\nread_only=true\nacceptance=complete\nhardware=complete\nrisk=none\n```",
        commit(head)?,
        commit(base)?
    ))
}

fn commit_json(sha: char, author: Option<&str>, committer: Option<&str>) -> TestResult<Value> {
    let account = |login: Option<&str>| login.map_or(Value::Null, |login| json!({"login": login}));
    Ok(
        json!({"sha": commit(sha)?.as_str(), "author": account(author),
        "committer": account(committer)}),
    )
}

const COMMITS: &str = "repos/origin89hq/firmware/pulls/12/commits";

/// The forge lists `list` as the commits of pull request 12.
fn commits(kitchen: &Kitchen, list: Value) {
    kitchen.forge().set(COMMITS, list);
}

/// The house with a standing merge grant on the repository at the forge.
fn with_merge_grant(mut config: HouseConfig) -> TestResult<HouseConfig> {
    let merge = Grant::repository(
        kitchen::contracts::Permission::Merge,
        repo()?,
        kitchen::BackendId::new("github")?,
        CredentialId::new("forge")?,
    );
    config.policy_limits.insert(merge.clone());
    config.grants.insert(merge);
    Ok(config)
}

/// An attestation of pull request 12 at `head` on base `e`, resting on
/// forge review `id` by `reviewer`.
fn attestation(head: char, reviewer: &str, id: u64) -> TestResult<GateAttestation> {
    attestation_on(head, 'e', reviewer, id)
}

/// An attestation of pull request 12 at `head` on base `base`, resting on
/// forge review `id` by `reviewer`.
fn attestation_on(head: char, base: char, reviewer: &str, id: u64) -> TestResult<GateAttestation> {
    Ok(GateAttestation {
        house: house()?,
        repository: repo()?,
        pull_request: pr(12)?,
        head: commit(head)?,
        base: commit(base)?,
        forge_review: ForgeReview {
            id: std::num::NonZeroU64::new(id).ok_or("review id")?,
            reviewer: reviewer.to_owned(),
        },
        review: kitchen::workflows::gate::SemanticReview::Clean,
        read_only: true,
        acceptance_met: true,
        hardware_complete: true,
        risk_classes: Vec::new(),
    })
}

/// Record `attestation` as `recorder`. The pull request's branch and author
/// are read from the forge.
fn record_as(
    kitchen: &Kitchen,
    attestation: &GateAttestation,
    recorder: &kitchen::contracts::Claimant,
) -> kitchen::Result<()> {
    record_gate_attestation(
        kitchen.store(),
        &kitchen.forge,
        attestation,
        recorder,
        kitchen.clock.now(),
    )
}

/// Record an independent reviewer's attestation of pull request 12 at `head`.
fn attest(kitchen: &Kitchen, head: char) -> TestResult {
    let recorded = attest_gate_review(
        kitchen.store(),
        &kitchen.forge,
        &repo()?,
        pr(12)?,
        std::num::NonZeroU64::new(11).ok_or("review id")?,
        kitchen.clock.now(),
    )?;
    assert_eq!(recorded.head, commit(head)?);
    Ok(())
}

/// Seed a valid review marker while testing later gate reads that are
/// intentionally unavailable or over their bound.
fn seed_attestation(kitchen: &Kitchen) -> TestResult {
    record_as(
        kitchen,
        &attestation('d', "safety-reviewer", 11)?,
        &common::scheduled("safety-reviewer")?,
    )?;
    Ok(())
}

/// The attestation recorded for pull request 12 at head `d` on base `e`.
fn recorded_at_d(
    kitchen: &Kitchen,
) -> TestResult<Option<kitchen::workflows::run::RecordedAttestation>> {
    Ok(kitchen::workflows::run::gate_attestation(
        kitchen.store(),
        &repo()?,
        pr(12)?,
        &commit('d')?,
        &commit('e')?,
    )?)
}

/// The gate tasks of the house.
fn gate_tasks(kitchen: &Kitchen) -> TestResult<Vec<kitchen::state::TaskRecord>> {
    Ok(kitchen
        .store()
        .tasks()?
        .into_iter()
        .filter(|record| record.spec().role == kitchen::contracts::Role::Expediter)
        .collect())
}

fn one_verdict(outcome: Outcome<GateAction>) -> TestResult<GateAction> {
    let mut actions = acted(outcome)?;
    match (actions.pop(), actions.is_empty()) {
        (Some(action), true) => Ok(action),
        _ => Err("one verdict expected".into()),
    }
}

fn merges(kitchen: &Kitchen) -> Vec<(String, Value)> {
    kitchen.forge().writes.borrow().clone()
}

fn verdict_markers(kitchen: &Kitchen) -> TestResult<usize> {
    Ok(kitchen
        .store()
        .markers(&kitchen::WorkflowId::new(
            kitchen::workflows::gate::GATE_WORKFLOW,
        )?)?
        .len())
}

#[test]
fn gate_reports_a_verdict_without_merging_on_unattested_evidence() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.pull_request.get(), 12);
    assert_eq!(action.head, commit('d')?);
    assert_ne!(action.verdict, Verdict::Merge);
    assert_eq!(
        action.result,
        GateResult::ReportOnly(ReportReason::Unattested)
    );
    // Nothing was recorded or written.
    assert_eq!(verdict_markers(&kitchen)?, 0);
    assert!(merges(&kitchen).is_empty());
    Ok(())
}

#[test]
fn gate_merges_an_attested_pull_request_at_its_exact_head() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    attest_gate_review(
        kitchen.store(),
        &kitchen.forge,
        &repo()?,
        pr(12)?,
        std::num::NonZeroU64::new(11).ok_or("review id")?,
        kitchen.clock.now(),
    )?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.verdict, Verdict::Merge, "{action:?}");
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    let writes = merges(&kitchen);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, format!("repos/{REPO}/pulls/12/merge"));
    assert_eq!(writes[0].1["sha"], commit('d')?.as_str());
    assert_eq!(writes[0].1["merge_method"], "squash");
    assert_eq!(verdict_markers(&kitchen)?, 1);
    // The gate task settled with the merge; the pull request is closed, so
    // the next pass has nothing to judge.
    let gate_task = kitchen
        .store()
        .tasks()?
        .into_iter()
        .find(|record| record.spec().role == kitchen::contracts::Role::Expediter);
    assert!(matches!(
        gate_task.as_ref().map(|record| record.state()),
        Some(TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        })
    ));
    assert!(matches!(kitchen.gate()?, Outcome::Idle));
    assert_eq!(merges(&kitchen).len(), 1);
    Ok(())
}

#[test]
fn gate_refuses_review_claims_changed_after_attestation() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    attest(&kitchen, 'd')?;
    kitchen.forge().set(
        &format!("repos/{REPO}/pulls/12/reviews"),
        json!([{"id":11,"user":{"login":"safety-reviewer"},
            "commit_id":commit('d')?.as_str(),"state":"APPROVED",
            "body":review_block('d','e')?.replace("semantic=clean", "semantic=findings")}]),
    );
    assert_reported(
        &kitchen,
        ReportReason::ReviewUnverified,
        "edited review claims",
    )
}

#[test]
fn gate_refuses_a_recorded_principal_other_than_the_review_author() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    record_as(
        &kitchen,
        &attestation('d', "safety-reviewer", 11)?,
        &common::scheduled("unrelated-handle")?,
    )?;
    assert_reported(
        &kitchen,
        ReportReason::ReviewUnverified,
        "different recorder",
    )
}

#[test]
fn reviewer_entrypoint_refuses_moved_self_reviewed_and_duplicate_subjects() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    let repository = repo()?;
    let pull_number = pr(12)?;
    let review_id = std::num::NonZeroU64::new(11).ok_or("review id")?;
    let record = || {
        attest_gate_review(
            kitchen.store(),
            &kitchen.forge,
            &repository,
            pull_number,
            review_id,
            kitchen.clock.now(),
        )
    };
    kitchen.forge().set(
        &format!("repos/{REPO}/pulls/12/reviews"),
        json!([{"id":11,"user":{"login":"safety-reviewer"},"commit_id":commit('d')?.as_str(),
            "state":"COMMENTED","body":review_block('d','e')?}]),
    );
    assert!(matches!(
        record(),
        Err(kitchen::Error::Run(RunError::AttestationReviewUnverified))
    ));
    kitchen.forge().set(
        &format!("repos/{REPO}/pulls/12/reviews"),
        json!([{"id":11,"user":{"login":"safety-reviewer"},"commit_id":commit('d')?.as_str(),
            "state":"APPROVED","body":review_block('f','e')?}]),
    );
    assert!(matches!(
        record(),
        Err(kitchen::Error::Run(RunError::AttestationStaleHead))
    ));
    approved_by(&kitchen, "safety-reviewer")?;
    commits(
        &kitchen,
        json!([commit_json(
            'd',
            Some("kitchen-bot"),
            Some("safety-reviewer")
        )?]),
    );
    assert!(matches!(
        record(),
        Err(kitchen::Error::Run(RunError::AttestationNotIndependent))
    ));
    commits(
        &kitchen,
        json!([commit_json('d', Some("kitchen-bot"), Some("kitchen-bot"))?]),
    );
    let base = commit('f')?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/main"),
        json!({"name":"main","commit":{"sha":base.as_str()}}),
    );
    assert!(matches!(
        record(),
        Err(kitchen::Error::Run(RunError::AttestationStaleBase))
    ));
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/main"),
        json!({"name":"main","commit":{"sha":commit('e')?.as_str()}}),
    );
    // The PR object's base SHA may lag the live ref used by the gate.
    set_pull_request(&kitchen, "/base/sha", json!(commit('c')?.as_str()))?;
    record()?;
    assert!(matches!(
        record(),
        Err(kitchen::Error::Run(RunError::AttestationRecorded))
    ));
    Ok(())
}

/// An outside recorder attests review 11, which the forge shows approved at
/// head `d` by `reviewer`.
fn attest_review_by(kitchen: &Kitchen, reviewer: &str) -> TestResult {
    approved_by(kitchen, reviewer)?;
    record_as(
        kitchen,
        &attestation('d', reviewer, 11)?,
        &common::scheduled("reviewer")?,
    )?;
    Ok(())
}

/// The gate only reports pull request 12, for `reason`: no verdict is
/// recorded and nothing merges.
fn assert_reported(kitchen: &Kitchen, reason: ReportReason, case: &str) -> TestResult {
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::ReportOnly(reason), "{case}");
    assert!(merges(kitchen).is_empty(), "{case}");
    assert_eq!(verdict_markers(kitchen)?, 0, "{case}");
    Ok(())
}

#[test]
fn gate_merges_nothing_when_the_reviewer_is_a_commit_author_or_committer() -> TestResult {
    // A scheduled worker pushed with Dana's own credentials, so the house
    // records name only the runner and the worker, and the bot opened the
    // pull request. Dana then approves the head. The forge attributes a
    // commit to her, as its author or as its committer.
    for (case, list) in [
        (
            "author and committer",
            json!([commit_json('d', Some("dana"), Some("dana"))?]),
        ),
        (
            "committer of an earlier commit",
            json!([
                commit_json('c', Some("kitchen-bot"), Some("Dana"))?,
                commit_json('d', Some("kitchen-bot"), Some("kitchen-bot"))?,
            ]),
        ),
        (
            "author of the head",
            json!([
                commit_json('c', Some("kitchen-bot"), Some("kitchen-bot"))?,
                commit_json('d', Some("DANA"), Some("web-flow"))?,
            ]),
        ),
    ] {
        let mut kitchen = settled_with_pull_request(true)?;
        kitchen.config = with_merge_grant(house_config()?)?;
        green_pull_request(&kitchen)?;
        commits(&kitchen, list);
        attest_review_by(&kitchen, "dana")?;
        assert_reported(&kitchen, ReportReason::NotIndependent, case)?;
    }
    Ok(())
}

#[test]
fn gate_merges_for_a_reviewer_no_commit_names() -> TestResult {
    // Dana's commits are on the branch; another login reviews it.
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    commits(
        &kitchen,
        json!([
            commit_json('c', Some("dana"), Some("dana"))?,
            commit_json('d', Some("kitchen-bot"), Some("web-flow"))?,
        ]),
    );
    attest(&kitchen, 'd')?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    assert_eq!(merges(&kitchen).len(), 1);
    assert!(
        kitchen
            .forge()
            .reads
            .borrow()
            .iter()
            .any(|read| read == COMMITS)
    );
    Ok(())
}

#[test]
fn gate_only_reports_a_commit_the_forge_links_to_no_account() -> TestResult {
    let unlinked = json!({});
    for (case, list) in [
        (
            "no author",
            json!([commit_json('d', None, Some("kitchen-bot"))?]),
        ),
        (
            "no committer on an earlier commit",
            json!([
                commit_json('c', Some("kitchen-bot"), None)?,
                commit_json('d', Some("kitchen-bot"), Some("kitchen-bot"))?,
            ]),
        ),
        (
            "an empty account",
            json!([{"sha": commit('d')?.as_str(), "author": unlinked,
                "committer": {"login": "kitchen-bot"}}]),
        ),
        ("no account fields", json!([{"sha": commit('d')?.as_str()}])),
    ] {
        let mut kitchen = settled_with_pull_request(true)?;
        kitchen.config = with_merge_grant(house_config()?)?;
        green_pull_request(&kitchen)?;
        commits(&kitchen, list);
        seed_attestation(&kitchen)?;
        assert_reported(&kitchen, ReportReason::WriterIdentityUnknown, case)?;
    }
    Ok(())
}

/// `count` commits by the house's login, the last one head `d`.
fn commit_page(count: usize) -> TestResult<Vec<Value>> {
    let mut page = vec![commit_json('c', Some("kitchen-bot"), Some("kitchen-bot"))?; count];
    if let Some(last) = page.last_mut() {
        *last = commit_json('d', Some("kitchen-bot"), Some("kitchen-bot"))?;
    }
    Ok(page)
}

#[test]
fn gate_reads_commits_up_to_the_bound_and_only_reports_beyond_it() -> TestResult {
    use kitchen::integrations::github::MAX_PULL_REQUEST_COMMITS;
    // Exactly the bound: a full first page, then an empty one.
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    kitchen.forge().queue(
        COMMITS,
        vec![json!(commit_page(MAX_PULL_REQUEST_COMMITS)?), json!([])],
    );
    seed_attestation(&kitchen)?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");

    // One more, on a second page: the writers are not read, whoever the
    // extra commit names.
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    kitchen.forge().queue(
        COMMITS,
        vec![
            json!(commit_page(MAX_PULL_REQUEST_COMMITS)?),
            json!(commit_page(1)?),
        ],
    );
    seed_attestation(&kitchen)?;
    assert_reported(&kitchen, ReportReason::CommitsOverBound, "one over")
}

#[test]
fn gate_stops_when_the_commits_cannot_be_read_or_lack_the_head() -> TestResult {
    for (case, list) in [
        ("unavailable", None),
        // The head moved under the read: the list is of another branch tip.
        (
            "without the head",
            Some(json!([commit_json(
                'c',
                Some("kitchen-bot"),
                Some("kitchen-bot")
            )?])),
        ),
        ("empty", Some(json!([]))),
    ] {
        let mut kitchen = settled_with_pull_request(true)?;
        kitchen.config = with_merge_grant(house_config()?)?;
        green_pull_request(&kitchen)?;
        match list {
            Some(list) => commits(&kitchen, list),
            None => {
                kitchen.forge().responses.borrow_mut().remove(COMMITS);
            }
        }
        seed_attestation(&kitchen)?;
        assert!(kitchen.gate().is_err(), "{case}");
        assert!(merges(&kitchen).is_empty(), "{case}");
        assert_eq!(verdict_markers(&kitchen)?, 0, "{case}");
        // The failed pass handed its lease on; with the commits readable
        // the next one merges.
        commits(
            &kitchen,
            json!([commit_json('d', Some("kitchen-bot"), Some("kitchen-bot"))?]),
        );
        let action = one_verdict(kitchen.gate()?)?;
        assert_eq!(action.result, GateResult::Merged, "{case}: {action:?}");
    }
    Ok(())
}

/// The next gate pass finds and judges pull request 12 at its head now; the
/// read just before the merge, and every later one, shows head `next`.
fn move_head_before_the_merge(kitchen: &Kitchen, next: char) -> TestResult {
    let judged = pull_request_now(kitchen)?;
    kitchen.forge().queue(
        &format!("repos/{REPO}/pulls/12"),
        vec![judged.clone(), judged],
    );
    set_pull_request(kitchen, "/head/sha", json!(commit(next)?.as_str()))
}

#[test]
fn gate_does_not_merge_when_the_head_moves_after_the_verdict() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    attest(&kitchen, 'd')?;
    // The pull request is read at head d to find it and to judge it; the
    // read just before the merge shows a new head.
    move_head_before_the_merge(&kitchen, 'f')?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.head, commit('d')?);
    assert_eq!(action.verdict, Verdict::Merge);
    assert_eq!(action.result, GateResult::NotMerged(NotMerged::Moved));
    assert!(merges(&kitchen).is_empty());
    // The verdict is recorded; its intent was never submitted and is
    // recorded as not applied, so it blocks nothing.
    assert_eq!(verdict_markers(&kitchen)?, 1);
    let gate_task = kitchen
        .store()
        .tasks()?
        .into_iter()
        .find(|record| record.spec().role == kitchen::contracts::Role::Expediter)
        .ok_or("no gate task")?;
    assert_eq!(gate_task.unresolved_effects().count(), 0);
    assert!(matches!(gate_task.state(), TaskState::Open));
    // At the new head the old attestation does not apply.
    let next = one_verdict(kitchen.gate()?)?;
    assert_eq!(next.head, commit('f')?);
    assert_eq!(
        next.result,
        GateResult::ReportOnly(ReportReason::Unattested)
    );
    assert!(merges(&kitchen).is_empty());
    Ok(())
}

#[test]
fn gate_passes_that_do_not_merge_continue_one_attempt() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    // Each pass relinquishes its task with an attempt open. A new subject
    // gets a new generation, so no task inherits another head's clock.
    let mut head = 'd';
    for (index, next) in ['1', '2', '3', '4'].into_iter().enumerate() {
        green_at(&kitchen, head)?;
        attest(&kitchen, head)?;
        move_head_before_the_merge(&kitchen, next)?;
        let action = one_verdict(kitchen.gate()?)?;
        assert_eq!(
            action.result,
            GateResult::NotMerged(NotMerged::Moved),
            "{head}: {action:?}"
        );
        let tasks = gate_tasks(&kitchen)?;
        assert_eq!(tasks.len(), index + 1, "{head}");
        assert_eq!(
            tasks
                .iter()
                .filter(|task| matches!(task.state(), TaskState::Open))
                .count(),
            1,
            "{head}"
        );
        assert!(tasks.iter().all(|task| task.attempts().len() == 1));
        head = next;
    }
    // The head then holds still: its fresh task merges it.
    green_at(&kitchen, head)?;
    attest(&kitchen, head)?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    let writes = merges(&kitchen);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].1["sha"], commit(head)?.as_str());
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 5);
    assert!(tasks.iter().all(|task| task.attempts().len() == 1));
    assert_eq!(
        tasks
            .iter()
            .filter(|task| matches!(
                task.state(),
                TaskState::Settled {
                    settlement: Settlement::Succeeded,
                    ..
                }
            ))
            .count(),
        1
    );
    Ok(())
}

/// The house pins newer guidance and gains a grant, as houses do over
/// time: a gate task created now has another specification.
fn change_gate_specification(kitchen: &mut Kitchen) -> TestResult {
    let push = Grant::repository(
        kitchen::contracts::Permission::PushBranch,
        repo()?,
        kitchen::BackendId::new("github")?,
        CredentialId::new("forge")?,
    );
    kitchen.config.policy_limits.insert(push.clone());
    kitchen.config.grants.insert(push);
    kitchen.settings.instructions.provenance.house_guidance = commit('c')?;
    Ok(())
}

/// A house with a merge grant whose gate pass recorded a merge verdict for
/// pull request 12 and left its gate task open: the pull request could not
/// be read again before the merge (`Unread`), or its head moved to `f`
/// (`Moved`), where it is green and attested again.
fn gate_task_left_open(outcome: NotMerged) -> TestResult<Kitchen> {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    attest(&kitchen, 'd')?;
    let endpoint = format!("repos/{REPO}/pulls/12");
    let at_d = pull_request_now(&kitchen)?;
    match outcome {
        NotMerged::Unread => {
            kitchen
                .forge()
                .queue(&endpoint, vec![at_d.clone(), at_d.clone()]);
            kitchen.forge().responses.borrow_mut().remove(&endpoint);
        }
        NotMerged::Moved => move_head_before_the_merge(&kitchen, 'f')?,
        _ => return Err("not an outcome this fixture makes".into()),
    }
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::NotMerged(outcome), "{action:?}");
    match outcome {
        NotMerged::Unread => kitchen.forge().set(&endpoint, at_d),
        _ => {
            green_at(&kitchen, 'f')?;
            attest(&kitchen, 'f')?;
        }
    }
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 1);
    assert!(matches!(tasks[0].state(), TaskState::Open));
    assert!(merges(&kitchen).is_empty());
    Ok(kitchen)
}

#[test]
fn gate_merges_under_a_new_task_after_the_specification_changes() -> TestResult {
    for outcome in [NotMerged::Unread, NotMerged::Moved] {
        let mut kitchen = gate_task_left_open(outcome)?;
        let earlier = gate_tasks(&kitchen)?[0].spec().id.clone();
        change_gate_specification(&mut kitchen)?;
        let action = one_verdict(kitchen.gate()?)?;
        assert_eq!(action.result, GateResult::Merged, "{outcome:?}: {action:?}");
        assert_eq!(merges(&kitchen).len(), 1, "{outcome:?}");
        // The earlier task was settled, and the merge ran under a task
        // with the current specification.
        let earlier = kitchen.store().task(&earlier)?;
        assert!(
            matches!(
                earlier.state(),
                TaskState::Settled {
                    settlement: Settlement::Cancelled,
                    ..
                }
            ),
            "{outcome:?}: {earlier:?}"
        );
        assert_eq!(earlier.unresolved_effects().count(), 0, "{outcome:?}");
        let tasks = gate_tasks(&kitchen)?;
        let current: Vec<_> = tasks
            .iter()
            .filter(|record| record.spec().id != earlier.spec().id)
            .collect();
        assert!(
            matches!(
                current.as_slice(),
                [record] if matches!(
                    record.state(),
                    TaskState::Settled { settlement: Settlement::Succeeded, .. }
                ) && record.spec().provenance.house_guidance == commit('c')?
            ),
            "{outcome:?}: {current:?}"
        );
    }
    Ok(())
}

#[test]
fn gate_merges_after_the_house_returns_to_an_earlier_specification() -> TestResult {
    // The first specification's task is open after an unread pass.
    let mut kitchen = gate_task_left_open(NotMerged::Unread)?;
    let first = gate_tasks(&kitchen)?[0].spec().id.clone();
    let (config, provenance) = (
        kitchen.config.clone(),
        kitchen.settings.instructions.provenance.clone(),
    );
    // Under a second specification the head moves before the merge.
    change_gate_specification(&mut kitchen)?;
    move_head_before_the_merge(&kitchen, 'f')?;
    let moved = one_verdict(kitchen.gate()?)?;
    assert_eq!(moved.result, GateResult::NotMerged(NotMerged::Moved));
    assert!(matches!(
        kitchen.store().task(&first)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    // The house returns to the first specification, whose task settled: a
    // new generation of it merges.
    kitchen.config = config;
    kitchen.settings.instructions.provenance = provenance;
    green_at(&kitchen, 'f')?;
    attest(&kitchen, 'f')?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 3);
    let merged: Vec<_> = tasks
        .iter()
        .filter(|record| {
            matches!(
                record.state(),
                TaskState::Settled {
                    settlement: Settlement::Succeeded,
                    ..
                }
            )
        })
        .collect();
    assert!(
        matches!(
            merged.as_slice(),
            [record] if record.spec().id != first
                && record.spec().provenance == kitchen.store().task(&first)?.spec().provenance
        ),
        "{merged:?}"
    );
    assert!(
        tasks
            .iter()
            .all(|record| matches!(record.state(), TaskState::Settled { .. }))
    );
    assert_eq!(merges(&kitchen).len(), 1);
    Ok(())
}

#[test]
fn gate_replaces_a_task_whose_ownership_history_is_nearly_full() -> TestResult {
    let kitchen = gate_task_left_open(NotMerged::Unread)?;
    let worn = wear_gate_task(&kitchen)?;
    // The next pass settles it while its history still has room, and
    // merges under the next generation.
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    assert!(matches!(
        kitchen.store().task(&worn)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    assert_eq!(gate_tasks(&kitchen)?.len(), 2);
    assert_eq!(merges(&kitchen).len(), 1);
    Ok(())
}

fn wear_gate_task(kitchen: &Kitchen) -> TestResult<kitchen::TaskId> {
    let worn = gate_tasks(kitchen)?[0].spec().id.clone();
    // Many passes held the task and gave it back without merging.
    let ttl = kitchen::contracts::LeaseTtl::new(TASK_LEASE)?;
    while kitchen.store().task(&worn)?.ownership().len() + 6
        <= kitchen::state::MAX_OWNERSHIP_HISTORY
    {
        let lease = kitchen
            .store()
            .claim(&worn, &run_claimant()?, ttl, kitchen.clock.now())?;
        kitchen
            .store()
            .relinquish(&worn, lease.fence(), kitchen.clock.now())?;
    }
    Ok(worn)
}

#[test]
fn replacement_gate_task_inherits_the_subject_deadline() -> TestResult {
    let kitchen = gate_task_left_open(NotMerged::Unread)?;
    let worn = wear_gate_task(&kitchen)?;
    kitchen.clock.advance(retry_deadline(&kitchen)? - 1);
    let endpoint = format!("repos/{REPO}/pulls/12");
    let at_d = pull_request_now(&kitchen)?;
    kitchen
        .forge()
        .queue(&endpoint, vec![at_d.clone(), at_d.clone()]);
    kitchen.forge().responses.borrow_mut().remove(&endpoint);
    let within = one_verdict(kitchen.gate()?)?;
    assert_eq!(within.result, GateResult::NotMerged(NotMerged::Unread));
    assert_eq!(gate_tasks(&kitchen)?.len(), 2);
    assert!(matches!(
        kitchen.store().task(&worn)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    kitchen.forge().set(&endpoint, at_d);
    kitchen.clock.advance(2);
    let after = one_verdict(kitchen.gate()?)?;
    assert_eq!(after.result, GateResult::NotMerged(NotMerged::Exhausted));
    assert!(matches!(
        gate_tasks(&kitchen)?
            .into_iter()
            .find(|record| record.spec().id != worn)
            .ok_or("missing replacement")?
            .state(),
        TaskState::Settled {
            settlement: Settlement::Exhausted,
            ..
        }
    ));
    assert!(merges(&kitchen).is_empty());
    Ok(())
}

#[test]
fn worn_gate_task_exhausts_the_subject_after_a_restart() -> TestResult {
    let mut kitchen = gate_task_left_open(NotMerged::Unread)?;
    let worn = wear_gate_task(&kitchen)?;
    kitchen.fixture.store = kitchen.fixture.reopen()?;
    kitchen.clock.advance(retry_deadline(&kitchen)? + 1);
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        action.result,
        GateResult::ReportOnly(ReportReason::ExhaustedForSubject),
        "{action:?}"
    );
    assert!(matches!(
        kitchen.store().task(&worn)?.state(),
        TaskState::Settled {
            settlement: Settlement::Exhausted,
            ..
        }
    ));
    assert_eq!(gate_tasks(&kitchen)?.len(), 1);
    assert!(merges(&kitchen).is_empty());
    let again = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        again.result,
        GateResult::ReportOnly(ReportReason::ExhaustedForSubject)
    );
    green_and_attested_on(&kitchen, 'f', 'e')?;
    let fresh = one_verdict(kitchen.gate()?)?;
    assert_eq!(fresh.result, GateResult::Merged, "{fresh:?}");
    assert_eq!(gate_tasks(&kitchen)?.len(), 2);
    Ok(())
}

#[test]
fn a_legacy_task_spanning_heads_keeps_the_first_attempt_budget() -> TestResult {
    let kitchen = gate_task_left_open(NotMerged::Moved)?;
    let task = gate_tasks(&kitchen)?[0].spec().id.clone();
    let first = gate_tasks(&kitchen)?[0].attempts()[0].started_at();
    // Model an old gate task: its single attempt began at d, but its latest
    // evidence now names f and no subject budget marker existed then.
    let lease = kitchen.store().claim(
        &task,
        &run_claimant()?,
        LeaseTtl::new(TASK_LEASE)?,
        kitchen.clock.now(),
    )?;
    kitchen.store().record_evidence(
        &task,
        lease.fence(),
        Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: commit('f')?,
                base: Some(commit('e')?),
            },
            source: ExternalRef::new("legacy-check")?,
            observed_at: kitchen.clock.now(),
        },
        kitchen.clock.now(),
    )?;
    kitchen
        .store()
        .relinquish(&task, lease.fence(), kitchen.clock.now())?;
    let old_markers = kitchen
        .store()
        .markers(&WorkflowId::new("merge-gate-budget")?)?;
    assert_eq!(old_markers.len(), 1);
    let retired = kitchen.store().retire_markers(
        &old_markers
            .into_iter()
            .map(|marker| (marker.key().clone(), marker.fact().clone()))
            .collect::<Vec<_>>(),
    )?;
    assert_eq!(retired.len(), 1);

    kitchen.clock.advance(retry_deadline(&kitchen)? + 1);
    let b = one_verdict(kitchen.gate()?)?;
    assert_eq!(b.head, commit('f')?);
    assert_eq!(b.result, GateResult::NotMerged(NotMerged::Exhausted));
    assert_eq!(gate_tasks(&kitchen)?.len(), 1);
    assert_eq!(gate_tasks(&kitchen)?[0].attempts()[0].started_at(), first);
    assert!(merges(&kitchen).is_empty());

    // The original attestation at d still exists; restore the forge view
    // without recording the same exact subject again.
    set_pull_request(&kitchen, "/head/sha", json!(commit('d')?.as_str()))?;
    green_on(&kitchen, 'd', 'e')?;
    let returned = one_verdict(kitchen.gate()?)?;
    assert_eq!(returned.head, commit('d')?);
    assert_eq!(
        returned.result,
        GateResult::ReportOnly(ReportReason::ExhaustedForSubject)
    );
    assert!(merges(&kitchen).is_empty());
    Ok(())
}

#[test]
fn a_new_head_gets_a_fresh_budget_after_the_old_head_expires() -> TestResult {
    let kitchen = gate_task_left_open(NotMerged::Unread)?;
    let old = gate_tasks(&kitchen)?[0].spec().id.clone();
    kitchen.clock.advance(retry_deadline(&kitchen)? + 1);
    green_and_attested_on(&kitchen, 'f', 'e')?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    assert!(matches!(
        kitchen.store().task(&old)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    assert_eq!(gate_tasks(&kitchen)?.len(), 2);
    assert_eq!(merges(&kitchen).len(), 1);
    Ok(())
}

#[test]
fn gate_keeps_one_task_while_the_specification_is_unchanged() -> TestResult {
    // The same revisions and grants: the open task is adopted, not replaced.
    let kitchen = gate_task_left_open(NotMerged::Unread)?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].attempts().len(), 1);
    Ok(())
}

#[test]
fn gate_creates_no_new_task_while_the_earlier_one_stands() -> TestResult {
    let mut kitchen = gate_task_left_open(NotMerged::Moved)?;
    change_gate_specification(&mut kitchen)?;
    let earlier = gate_tasks(&kitchen)?[0].spec().id.clone();
    let ttl = kitchen::contracts::LeaseTtl::new(TASK_LEASE)?;
    // Another holder has the earlier task: it may be reconciling a merge.
    let other = kitchen.store().claim(
        &earlier,
        &common::scheduled("other-gate")?,
        ttl,
        kitchen.clock.now(),
    )?;
    let held = one_verdict(kitchen.gate()?)?;
    assert_eq!(held.result, GateResult::ReportOnly(ReportReason::TaskHeld));
    kitchen
        .store()
        .relinquish(&earlier, other.fence(), kitchen.clock.now())?;
    // A pass that died holding it left an expired claim: uncertain until a
    // takeover.
    kitchen
        .store()
        .claim(&earlier, &run_claimant()?, ttl, kitchen.clock.now())?;
    kitchen.clock.advance(TASK_LEASE.as_secs() + 1);
    let uncertain = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        uncertain.result,
        GateResult::ReportOnly(ReportReason::TaskUncertain)
    );
    assert_eq!(gate_tasks(&kitchen)?.len(), 1);
    assert!(merges(&kitchen).is_empty());
    Ok(())
}

#[test]
fn gate_reports_an_earlier_task_whose_ownership_history_is_full() -> TestResult {
    let mut kitchen = gate_task_left_open(NotMerged::Moved)?;
    change_gate_specification(&mut kitchen)?;
    let earlier = gate_tasks(&kitchen)?[0].spec().id.clone();
    let ttl = kitchen::contracts::LeaseTtl::new(TASK_LEASE)?;
    while kitchen.store().task(&earlier)?.ownership().len() + 2
        <= kitchen::state::MAX_OWNERSHIP_HISTORY
    {
        let lease = kitchen
            .store()
            .claim(&earlier, &run_claimant()?, ttl, kitchen.clock.now())?;
        kitchen
            .store()
            .relinquish(&earlier, lease.fence(), kitchen.clock.now())?;
    }
    // It cannot change hands again, so the pass reports it instead of
    // failing on the claim, and creates no task beside it.
    let held = one_verdict(kitchen.gate()?)?;
    assert_eq!(held.result, GateResult::ReportOnly(ReportReason::TaskHeld));
    assert!(matches!(
        kitchen.store().task(&earlier)?.state(),
        TaskState::Open
    ));
    assert_eq!(gate_tasks(&kitchen)?.len(), 1);
    assert!(merges(&kitchen).is_empty());
    Ok(())
}

/// Pull request 12 is ready at head `head` on a base branch whose tip is
/// `base`, and an independent reviewer's attestation of exactly that is
/// recorded.
fn green_and_attested_on(kitchen: &Kitchen, head: char, base: char) -> TestResult {
    set_pull_request(kitchen, "/head/sha", json!(commit(head)?.as_str()))?;
    green_on(kitchen, head, base)?;
    attest(kitchen, head)?;
    Ok(())
}

/// A house with a merge grant whose gate pass sent the merge of pull
/// request 12 at head `d` and lost the answer. The forge shows the pull
/// request open at `d`, which proves nothing either way. The owner then
/// handed the effect over and decided the gate task may only settle
/// unsuccessfully.
fn merge_sent_and_waived() -> TestResult<Kitchen> {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    attest(&kitchen, 'd')?;
    kitchen.forge().lose_writes.set(true);
    let action = one_verdict(kitchen.gate()?)?;
    kitchen.forge().lose_writes.set(false);
    assert_eq!(
        action.result,
        GateResult::NotMerged(NotMerged::Uncertain),
        "{action:?}"
    );
    assert_eq!(merges(&kitchen).len(), 1);
    let (store, now) = (kitchen.store(), kitchen.clock.now());
    let record = gate_tasks(&kitchen)?.pop().ok_or("no gate task")?;
    let task = record.spec().id.clone();
    let sent = record.effects().last().ok_or("no merge intent")?;
    let fence = store
        .claim(
            &task,
            &common::scheduled("owner")?,
            kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
            now,
        )?
        .fence();
    store.record_effect_outcome(&task, fence, sent.seq(), EffectOutcome::Unresolvable, now)?;
    let waived = store.accept_risk(
        &task,
        fence,
        sent.seq(),
        RiskDecision {
            effect: sent.request().key().clone(),
            decided_by: HolderId::new("owner")?,
            revision: record.evidence().revision(),
            action: RiskAction::SettleUnsuccessfully,
        },
        now,
    )?;
    assert!(matches!(waived.state(), EffectState::Waived { .. }));
    store.relinquish(&task, fence, now)?;
    // The decision covers the effect: the task reports nothing unresolved.
    assert_eq!(store.task(&task)?.unresolved_effects().count(), 0);
    Ok(kitchen)
}

/// The gate pass only reports that it is reconciling: no gate task beyond
/// the first exists, and no write beyond the first merge was sent.
fn assert_barred(kitchen: &Kitchen, case: &str) -> TestResult {
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        action.result,
        GateResult::NotMerged(NotMerged::Reconciling),
        "{case}: {action:?}"
    );
    assert_eq!(gate_tasks(kitchen)?.len(), 1, "{case}");
    assert_eq!(merges(kitchen).len(), 1, "{case}");
    Ok(())
}

#[test]
fn gate_sends_no_merge_while_an_earlier_one_has_no_proven_outcome() -> TestResult {
    let mut kitchen = merge_sent_and_waived()?;
    let earlier = gate_tasks(&kitchen)?[0].spec().id.clone();
    // Under the same task, the waived merge still bars another.
    assert_barred(&kitchen, "same task")?;
    // The base branch moves, the pull request is attested on the new tip,
    // and the house's specification changes: the verdict is a merge at a
    // subject the earlier one never covered, under another gate task.
    green_and_attested_on(&kitchen, 'd', 'a')?;
    change_gate_specification(&mut kitchen)?;
    assert_barred(&kitchen, "new task")?;
    // The earlier task was not cancelled away, and its merge is still
    // waived, not resolved.
    let record = kitchen.store().task(&earlier)?;
    assert!(matches!(record.state(), TaskState::Open), "{record:?}");
    assert!(matches!(
        record.effects().last().map(|effect| effect.state()),
        Some(EffectState::Waived { .. })
    ));
    // The owner settles the earlier task, as the decision allows. Its
    // merge is no more proven than before.
    let now = kitchen.clock.now();
    let fence = kitchen
        .store()
        .claim(
            &earlier,
            &common::scheduled("owner")?,
            kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
            now,
        )?
        .fence();
    kitchen.store().settle_cancelled(&earlier, fence, now)?;
    assert_barred(&kitchen, "settled task")?;
    Ok(())
}

#[test]
fn gate_merges_once_the_forge_proves_the_earlier_merge_absent() -> TestResult {
    for settle_first in [false, true] {
        let mut kitchen = merge_sent_and_waived()?;
        let earlier = gate_tasks(&kitchen)?[0].spec().id.clone();
        change_gate_specification(&mut kitchen)?;
        if settle_first {
            let now = kitchen.clock.now();
            let fence = kitchen
                .store()
                .claim(
                    &earlier,
                    &common::scheduled("owner")?,
                    kitchen::contracts::LeaseTtl::new(TASK_LEASE)?,
                    now,
                )?
                .fence();
            kitchen.store().settle_cancelled(&earlier, fence, now)?;
        }
        // The head moves: the request sent for head d can no longer merge,
        // which the lookup reads as proof of absence.
        green_and_attested_on(&kitchen, 'f', 'a')?;
        let action = one_verdict(kitchen.gate()?)?;
        assert_eq!(
            action.result,
            GateResult::Merged,
            "{settle_first}: {action:?}"
        );
        let writes = merges(&kitchen);
        assert_eq!(writes.len(), 2, "{settle_first}");
        assert_eq!(writes[1].1["sha"], commit('f')?.as_str(), "{settle_first}");
        let record = kitchen.store().task(&earlier)?;
        assert!(
            matches!(
                record.state(),
                TaskState::Settled {
                    settlement: Settlement::Cancelled,
                    ..
                }
            ),
            "{settle_first}: {record:?}"
        );
        assert!(
            matches!(
                record.effects().last().map(|effect| effect.state()),
                Some(EffectState::NotApplied { .. })
            ),
            "{settle_first}: {record:?}"
        );
        assert_eq!(gate_tasks(&kitchen)?.len(), 2, "{settle_first}");
    }
    Ok(())
}

#[test]
fn gate_never_repeats_a_merge_the_forge_shows_landed() -> TestResult {
    let mut kitchen = merge_sent_and_waived()?;
    green_and_attested_on(&kitchen, 'd', 'a')?;
    change_gate_specification(&mut kitchen)?;
    // The lost request lands after all. The pull request is closed, so the
    // pass has nothing to judge and sends nothing.
    set_pull_request(&kitchen, "/merged", json!(true))?;
    set_pull_request(&kitchen, "/state", json!("closed"))?;
    assert!(matches!(kitchen.gate()?, Outcome::Idle));
    assert_eq!(gate_tasks(&kitchen)?.len(), 1);
    assert_eq!(merges(&kitchen).len(), 1);
    Ok(())
}

/// The retry deadline of the house's one gate task, in seconds from its
/// first attempt.
fn retry_deadline(kitchen: &Kitchen) -> TestResult<u64> {
    let tasks = gate_tasks(kitchen)?;
    let task = tasks.first().ok_or("no gate task")?;
    Ok(task.spec().retry.max_elapsed().as_secs())
}

#[test]
fn gate_ends_a_continued_attempt_past_the_retry_deadline() -> TestResult {
    // The gate task's attempt was interrupted by a pass that did not merge.
    // The pull request is still at the task's original head.
    let kitchen = gate_task_left_open(NotMerged::Unread)?;
    kitchen.clock.advance(retry_deadline(&kitchen)? + 1);
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.head, commit('d')?);
    assert_eq!(
        action.result,
        GateResult::NotMerged(NotMerged::Exhausted),
        "{action:?}"
    );
    assert!(merges(&kitchen).is_empty());
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].attempts().len(), 1);
    assert!(
        matches!(
            tasks[0].state(),
            TaskState::Settled {
                settlement: Settlement::Exhausted,
                ..
            }
        ),
        "{:?}",
        tasks[0].state()
    );
    // No second intent was recorded after the deadline.
    assert_eq!(tasks[0].effects().len(), 1);
    // On the same head and base no new task retries the same evidence.
    let again = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        again.result,
        GateResult::ReportOnly(ReportReason::ExhaustedForSubject),
        "{again:?}"
    );
    assert_eq!(gate_tasks(&kitchen)?.len(), 1);
    assert!(merges(&kitchen).is_empty());
    // A moved base is new evidence, and so is a new head: a new generation
    // of the task merges.
    green_and_attested_on(&kitchen, 'd', 'a')?;
    move_head_before_the_merge(&kitchen, '1')?;
    let moved = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        moved.result,
        GateResult::NotMerged(NotMerged::Moved),
        "{moved:?}"
    );
    assert_eq!(gate_tasks(&kitchen)?.len(), 2);
    green_and_attested_on(&kitchen, '1', 'a')?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    let writes = merges(&kitchen);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].1["sha"], commit('1')?.as_str());
    assert_eq!(gate_tasks(&kitchen)?.len(), 3);
    Ok(())
}

#[test]
fn gate_continues_an_attempt_up_to_the_retry_deadline() -> TestResult {
    // Exactly at the deadline the budget is not yet spent.
    let kitchen = gate_task_left_open(NotMerged::Unread)?;
    kitchen.clock.advance(retry_deadline(&kitchen)?);
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].attempts().len(), 1);
    Ok(())
}

#[test]
fn gate_reconciles_an_unresolved_merge_past_the_retry_deadline() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    attest(&kitchen, 'd')?;
    kitchen.forge().lose_writes.set(true);
    let lost = one_verdict(kitchen.gate()?)?;
    kitchen.forge().lose_writes.set(false);
    assert_eq!(lost.result, GateResult::NotMerged(NotMerged::Uncertain));
    kitchen.clock.advance(retry_deadline(&kitchen)? + 1);
    // Past the deadline the sent merge is still looked up, and the task is
    // neither ended nor replaced while its outcome is unknown.
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        action.result,
        GateResult::NotMerged(NotMerged::Reconciling),
        "{action:?}"
    );
    let tasks = gate_tasks(&kitchen)?;
    assert_eq!(tasks.len(), 1);
    assert!(matches!(tasks[0].state(), TaskState::Open));
    assert_eq!(merges(&kitchen).len(), 1);
    Ok(())
}

#[test]
fn gate_without_a_merge_grant_reports_and_merges_nothing() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    attest(&kitchen, 'd')?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        action.result,
        GateResult::ReportOnly(ReportReason::NoMergeGrant),
        "{action:?}"
    );
    assert!(merges(&kitchen).is_empty());
    assert_eq!(verdict_markers(&kitchen)?, 0);
    Ok(())
}

#[test]
fn gate_attestations_must_be_independent_and_are_never_rewritten() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    let recorder = common::scheduled("reviewer")?;
    let record = |attestation: &GateAttestation| record_as(&kitchen, attestation, &recorder);
    let recorded = || recorded_at_d(&kitchen);
    // The pull request's author, as the forge names it, is refused as its
    // reviewer. So is any reviewer while the forge names no author.
    assert!(matches!(
        record(&attestation('d', "Kitchen-Bot", 11)?),
        Err(kitchen::Error::Run(RunError::AttestationNotIndependent))
    ));
    set_pull_request(&kitchen, "/user", Value::Null)?;
    assert!(matches!(
        record(&attestation('d', "safety-reviewer", 11)?),
        Err(kitchen::Error::Run(RunError::AttestationNotIndependent))
    ));
    // A pull request the forge cannot show is not attested.
    let shown = pull_request_now(&kitchen)?;
    let endpoint = format!("repos/{REPO}/pulls/12");
    kitchen.forge().responses.borrow_mut().remove(&endpoint);
    assert!(record(&attestation('d', "safety-reviewer", 11)?).is_err());
    assert!(recorded()?.is_none());
    kitchen.forge().set(&endpoint, shown);
    // The forge named another author when the attestation was recorded.
    // The gate reads the author again and catches the reviewer.
    set_pull_request(&kitchen, "/user", json!({"login": "someone-else"}))?;
    record(&attestation('d', "kitchen-bot", 11)?)?;
    assert_eq!(
        recorded()?.map(|found| found.recorded_by),
        Some(recorder.holder.clone())
    );
    set_pull_request(&kitchen, "/user", json!({"login": "kitchen-bot"}))?;
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(
        action.result,
        GateResult::ReportOnly(ReportReason::NotIndependent)
    );
    assert!(merges(&kitchen).is_empty());
    // Neither the same attestation nor a different one can replace it.
    set_pull_request(&kitchen, "/user", json!({"login": "someone-else"}))?;
    assert!(matches!(
        record(&attestation('d', "kitchen-bot", 11)?),
        Err(kitchen::Error::Run(RunError::AttestationRecorded))
    ));
    assert!(matches!(
        record(&attestation('d', "safety-reviewer", 11)?),
        Err(kitchen::Error::Run(RunError::AttestationRecorded))
    ));
    // Another house's attestation is refused before anything is read.
    let reads = kitchen.forge().reads();
    let mut foreign = attestation('f', "safety-reviewer", 11)?;
    foreign.house = common::other_house()?;
    assert!(matches!(record(&foreign), Err(kitchen::Error::Contract(_))));
    assert_eq!(kitchen.forge().reads(), reads);
    Ok(())
}

#[test]
fn a_branch_writer_cannot_record_an_attestation_for_another_reviewer() -> TestResult {
    let kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    // A person repairs the pull request through `kitchn pr`.
    let (_, _) = person_holds_round(&kitchen, 12)?;
    let worker = kitchen.worker(7)?;
    // The branch's worker, the runner that held its task, and the person
    // holding its round each claim the real reviewer's approval. The branch
    // they wrote is the one the forge shows for the pull request.
    for recorder in [
        common::scheduled(worker.handle.as_str())?,
        run_claimant()?,
        person_session()?,
    ] {
        let recorded = record_as(
            &kitchen,
            &attestation('d', "safety-reviewer", 11)?,
            &recorder,
        );
        assert!(
            matches!(
                recorded,
                Err(kitchen::Error::Run(RunError::AttestationByWriter))
            ),
            "{recorder:?}: {recorded:?}"
        );
    }
    assert!(recorded_at_d(&kitchen)?.is_none());
    Ok(())
}

#[test]
fn gate_refuses_an_attestation_a_branch_writer_recorded_against_another_branch() -> TestResult {
    // The pickup task's worker and the runner wrote `kitchen/issue-7`. Each
    // records the real reviewer's approval while the pull request read
    // names another head branch, so the record-time check finds none of
    // their tasks on it.
    let worker_handle = |kitchen: &Kitchen| -> TestResult<kitchen::contracts::Claimant> {
        common::scheduled(kitchen.worker(7)?.handle.as_str())
    };
    for recorder in [worker_handle, |_: &Kitchen| Ok(run_claimant()?)] {
        let mut kitchen = settled_with_pull_request(true)?;
        kitchen.config = with_merge_grant(house_config()?)?;
        green_pull_request(&kitchen)?;
        let recorder = recorder(&kitchen)?;
        set_pull_request(&kitchen, "/head/ref", json!("kitchen/issue-8"))?;
        record_as(
            &kitchen,
            &attestation('d', "safety-reviewer", 11)?,
            &recorder,
        )?;
        set_pull_request(&kitchen, "/head/ref", json!("kitchen/issue-7"))?;
        // The gate checks the recorder against the branch it found the pull
        // request on, and the writer's acceptance, hardware and risk facts
        // are not used.
        assert_reported(&kitchen, ReportReason::AttestedByWriter, "wrong branch")?;
    }
    Ok(())
}

/// The forge shows review 11 of pull request 12 approved at head `d` by
/// `login`.
fn approved_by(kitchen: &Kitchen, login: &str) -> TestResult {
    kitchen.forge().set(
        &format!("repos/{REPO}/pulls/12/reviews"),
        json!([{"id": 11, "user": {"login": login}, "commit_id": commit('d')?.as_str(),
            "state": "APPROVED", "body": review_block('d', 'e')?,
            "submitted_at": "1970-01-01T00:00:00Z"}]),
    );
    Ok(())
}

/// An outside recorder attests review 11 by `reviewer`; the gate then
/// reports the pull request for an unknown writer identity and neither
/// records a verdict nor merges.
fn assert_writer_unknown(kitchen: &Kitchen, reviewer: &str) -> TestResult {
    attest_review_by(kitchen, reviewer)?;
    assert_reported(kitchen, ReportReason::WriterIdentityUnknown, reviewer)?;
    // The records alone refuse it: the commits were not read.
    assert!(
        !kitchen
            .forge()
            .reads
            .borrow()
            .iter()
            .any(|read| read == COMMITS)
    );
    Ok(())
}

#[test]
fn gate_merges_nothing_when_a_person_wrote_the_branch_under_a_session_name() -> TestResult {
    // Dana repaired the pull request as `session-dana`, pushing through the
    // house's forge login, and handed the round back. She then approves as
    // the forge login `dana`: no name in the records matches it, and every
    // commit is attributed to the house's login. Any other reviewer is
    // refused too, since Dana's login is unknown.
    for reviewer in ["dana", "safety-reviewer"] {
        let mut kitchen = settled_with_pull_request(true)?;
        kitchen.config = with_merge_grant(house_config()?)?;
        green_pull_request(&kitchen)?;
        let (round, fence) = person_holds_round(&kitchen, 12)?;
        kitchen
            .store()
            .relinquish(&round, fence, kitchen.clock.now())?;
        assert_writer_unknown(&kitchen, reviewer)?;
    }
    Ok(())
}

#[test]
fn gate_merges_nothing_when_a_person_took_the_branch_workers_terminal() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.config = with_merge_grant(house_config()?)?;
    kitchen.ready_seven();
    assert!(matches!(kitchen.pickup(false)?, Outcome::Acted(_)));
    let worker = kitchen.worker(7)?;
    // A person takes the worker's terminal over; supervision records it.
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::UserTakeover);
    acted(kitchen.coordinate()?)?;
    // The worker then settles and reports, and its pull request is green.
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.backend.post(vec![report(&worker, "done-7")?])?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    acted(kitchen.coordinate()?)?;
    pull_request(kitchen.forge(), 7, 12, true)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/issues/7/timeline"), json!([]));
    green_pull_request(&kitchen)?;
    assert_writer_unknown(&kitchen, "safety-reviewer")
}

#[test]
fn gate_merges_nothing_on_an_attestation_the_forge_does_not_show() -> TestResult {
    let head = commit('d')?;
    let review = |id: u64, login: &str, at: &kitchen::contracts::CommitId, state: &str| {
        json!([{"id": id, "user": {"login": login}, "commit_id": at.as_str(),
            "state": state, "submitted_at": "1970-01-01T00:00:00Z"}])
    };
    for (claimed, id, forge) in [
        // Someone outside the records claims the reviewer's approval with a
        // review the forge does not have.
        (
            "safety-reviewer",
            99,
            review(11, "safety-reviewer", &head, "APPROVED"),
        ),
        // The review exists, but another login wrote it.
        (
            "safety-reviewer",
            11,
            review(11, "kitchen-helper", &head, "APPROVED"),
        ),
        // The approval is on an earlier head.
        (
            "safety-reviewer",
            11,
            review(11, "safety-reviewer", &commit('c')?, "APPROVED"),
        ),
        // The review only commented.
        (
            "safety-reviewer",
            11,
            review(11, "safety-reviewer", &head, "COMMENTED"),
        ),
    ] {
        let mut kitchen = settled_with_pull_request(true)?;
        kitchen.config = with_merge_grant(house_config()?)?;
        green_pull_request(&kitchen)?;
        kitchen
            .forge()
            .set(&format!("repos/{REPO}/pulls/12/reviews"), forge.clone());
        record_as(
            &kitchen,
            &attestation('d', claimed, id)?,
            &common::scheduled("reviewer")?,
        )?;
        let action = one_verdict(kitchen.gate()?)?;
        assert_eq!(
            action.result,
            GateResult::ReportOnly(ReportReason::ReviewUnverified),
            "{forge}"
        );
        assert!(merges(&kitchen).is_empty(), "{forge}");
        assert_eq!(verdict_markers(&kitchen)?, 0, "{forge}");
    }
    Ok(())
}

#[test]
fn passes_run_for_a_house_with_a_merge_grant_without_delegating_it() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.config = with_merge_grant(house_config()?)?;
    kitchen.ready_seven();
    let actions = acted(kitchen.pickup(false)?)?;
    assert!(matches!(
        actions.as_slice(),
        [PickupAction::Launched { .. }]
    ));
    let brief = launch_brief(&kitchen, &kitchen.task(7)?)?;
    let authority = brief
        .lines()
        .find(|line| line.starts_with("Authority:"))
        .ok_or("no authority line")?;
    assert!(authority.contains("launch-worker"), "{authority}");
    assert!(!authority.contains("merge"), "{authority}");
    assert!(matches!(
        kitchen.coordinate()?,
        Outcome::Acted(_) | Outcome::Idle
    ));
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

/// A message from a worker no stored task launched, modelled on `like`.
fn stray(like: &ResourceRef, id: &str) -> TestResult<MailMessage> {
    Ok(MailMessage {
        worker: Some(ResourceRef {
            handle: ExternalRef::new("worker-of-no-task")?,
            ..like.clone()
        }),
        kind: MessageKind::Escalation,
        ..report(like, id)?
    })
}

#[test]
fn coordinate_acknowledges_a_stray_worker_message_and_reads_the_next_batch() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    let message = stray(&worker, "stray")?;
    let stranger = message.worker.clone().ok_or("no worker")?;
    kitchen.backend.post(vec![message])?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.backend.post(vec![report(&worker, "done-7")?])?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Unroutable {
        message: ExternalRef::new("stray")?,
        reason: Unroutable::UnknownWorker { worker: stranger },
    }));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: kitchen.task(7)?,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn coordinate_records_unreadable_rows_and_a_message_without_a_worker() -> TestResult {
    let kitchen = Kitchen::new()?;
    let anonymous = MailMessage {
        worker: None,
        kind: MessageKind::Other,
        ..report(
            &ResourceRef {
                kind: kitchen::contracts::ResourceKind::Worker,
                backend: backend_id()?,
                handle: ExternalRef::new("unnamed")?,
            },
            "anonymous",
        )?
    };
    kitchen.backend.post_with_unreadable(vec![anonymous], 2)?;
    // Run adoption redelivers the batch under a new id.
    let actions = acted(kitchen.coordinate()?)?;
    assert!(matches!(
        actions.as_slice(),
        [
            CoordinateAction::Unreadable { rows: 2, .. },
            CoordinateAction::Unroutable {
                message,
                reason: Unroutable::NoWorker,
            },
        ] if message.as_str() == "anonymous"
    ));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn coordinate_keeps_a_stray_message_while_its_batch_waits_for_an_owner() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen
        .backend
        .post(vec![stray(&worker, "stray")?, report(&worker, "done-7")?])?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    let task = kitchen.task(7)?;
    kitchen.clock.advance(TASK_LEASE.as_secs() + 1);
    // The report waits for whoever continues task 7, so nothing in its
    // batch is acknowledged or reported as dropped yet.
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
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::Unroutable { .. }))
    );
    assert!(actions.contains(&CoordinateAction::Supervised {
        task,
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn coordinate_supervises_a_task_it_adopted_after_a_failed_pass() -> TestResult {
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.backend.post(vec![report(&worker, "done-7")?])?;
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    // The forge does not show the pushed branch yet: the pass fails after
    // moving the task and relinquishes its lease.
    assert!(kitchen.coordinate().is_err());
    assert!(matches!(
        kitchen.consumer(Pass::Coordinate)?,
        Some(ConsumerState::Relinquished { .. })
    ));
    // A move interrupted between its relinquish and its claim leaves the
    // task open for the next coordinator to adopt.
    let task = kitchen.task(7)?;
    kitchen
        .store()
        .relinquish(&task, kitchen.claim_fence(7)?, kitchen.clock.now())?;
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/kitchen/issue-7"),
        json!({"name": "kitchen/issue-7", "commit": {"sha": commit('d')?.as_str()}}),
    );
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Adopted { task: task.clone() }));
    assert!(actions.contains(&CoordinateAction::Supervised {
        task: task.clone(),
        outcome: Supervision::Settled(Settlement::Succeeded),
    }));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::Unacknowledged { .. }))
    );
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Settled { .. }
    ));
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

/// What runs while a launch is paused.
type During<'a> = Box<dyn FnOnce(&ResourceRef) + 'a>;

/// A backend that pauses each launch after the fake started the worker and
/// before the launch returns, so the receipt is not stored yet. It runs
/// `during` with the new worker there, then returns the receipt, or loses
/// the response when `lose_response` is set.
struct PausedLaunch<'a> {
    inner: &'a FakeBackend,
    during: RefCell<Option<During<'a>>>,
    lose_response: bool,
}

impl kitchen::contracts::EffectExecutor for PausedLaunch<'_> {
    fn descriptor(&self) -> &kitchen::contracts::BackendDescriptor {
        self.inner.descriptor()
    }

    fn execute(
        &self,
        request: &kitchen::contracts::EffectRequest,
    ) -> Result<kitchen::contracts::Receipt, kitchen::contracts::EffectFailure> {
        let receipt = self.inner.execute(request)?;
        let worker = receipt
            .created()
            .iter()
            .find(|resource| resource.kind == kitchen::contracts::ResourceKind::Worker);
        if let (Some(worker), Some(during)) = (worker, self.during.borrow_mut().take()) {
            during(worker);
        }
        if self.lose_response {
            return Err(kitchen::contracts::EffectFailure::Uncertain(
                kitchen::contracts::UncertainReason::ResponseLost,
            ));
        }
        Ok(receipt)
    }

    fn lookup(
        &self,
        request: &kitchen::contracts::EffectRequest,
    ) -> Result<kitchen::contracts::Lookup, kitchen::contracts::BackendUnavailable> {
        self.inner.lookup(request)
    }
}

impl kitchen::contracts::WorkerBackend for PausedLaunch<'_> {
    fn observe_worker(
        &self,
        worker: &ResourceRef,
    ) -> Result<WorkerState, kitchen::contracts::BackendUnavailable> {
        self.inner.observe_worker(worker)
    }
}

impl Kitchen {
    /// Run a pickup pass whose launch pauses as [`PausedLaunch`] describes.
    fn pickup_paused<'a>(
        &'a self,
        lose_response: bool,
        during: impl FnOnce(&ResourceRef) + 'a,
    ) -> kitchen::Result<Outcome<PickupAction>> {
        let backend = PausedLaunch {
            inner: &self.backend,
            during: RefCell::new(Some(Box::new(during))),
            lose_response,
        };
        PickupPass {
            store: self.store(),
            house: &self.config,
            backend: &backend,
            forge: &self.forge,
            clock: &self.clock,
            settings: &self.settings,
            take_over: false,
            tick: None,
        }
        .run()
    }
}

fn question(worker: &ResourceRef, id: &str) -> TestResult<MailMessage> {
    Ok(MailMessage {
        kind: MessageKind::Question,
        outcome: None,
        ..report(worker, id)?
    })
}

#[test]
fn coordinate_keeps_a_message_from_a_worker_whose_launch_is_still_in_flight() -> TestResult {
    use kitchen::contracts::CoordinatorMailbox;
    let kitchen = Kitchen::new()?;
    kitchen.ready_seven();
    let during = RefCell::new(None);
    acted(kitchen.pickup_paused(false, |worker| {
        // The worker asks before the pickup pass stores its receipt.
        let asked = question(worker, "early")
            .and_then(|message| Ok(kitchen.backend.post(vec![message])?));
        *during.borrow_mut() = Some((asked, kitchen.coordinate()));
    })?)?;
    let (asked, first) = during.into_inner().ok_or("the launch did not pause")?;
    asked?;
    let first = acted(first?)?;
    assert!(
        matches!(first.as_slice(), [CoordinateAction::Unacknowledged { .. }]),
        "{first:?}"
    );
    // Once the launch is stored, the message reaches the task.
    let task = kitchen.task(7)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Question {
        task,
        message: ExternalRef::new("early")?,
    }));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::Unroutable { .. }))
    );
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

/// Launch issue 7 with a lost response while its worker asks a question:
/// the launch is uncertain and the question waits in the mailbox.
fn launch_uncertain_with_question(kitchen: &Kitchen) -> TestResult {
    kitchen.ready_seven();
    let asked = RefCell::new(None);
    acted(kitchen.pickup_paused(true, |worker| {
        *asked.borrow_mut() = Some(
            question(worker, "early")
                .and_then(|message| Ok(kitchen.backend.post(vec![message])?)),
        );
    })?)?;
    asked.into_inner().ok_or("the launch did not pause")??;
    assert!(current_worker(&kitchen.store().task(&kitchen.task(7)?)?).is_none());
    Ok(())
}

#[test]
fn coordinate_reconciles_an_uncertain_launch_before_routing_its_worker() -> TestResult {
    let kitchen = Kitchen::new()?;
    launch_uncertain_with_question(&kitchen)?;
    let task = kitchen.task(7)?;
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Moved { task: task.clone() }));
    assert!(actions.contains(&CoordinateAction::Question {
        task,
        message: ExternalRef::new("early")?,
    }));
    assert!(kitchen.worker(7).is_ok());
    use kitchen::contracts::CoordinatorMailbox;
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

#[test]
fn coordinate_keeps_the_message_while_the_launch_cannot_be_reconciled() -> TestResult {
    use kitchen::contracts::CoordinatorMailbox;
    let kitchen = Kitchen::new()?;
    launch_uncertain_with_question(&kitchen)?;
    let task = kitchen.task(7)?;
    // Every lookup this pass makes fails, routing's and supervision's, so
    // the launch stays uncertain. The count only needs to exceed them; a
    // successful lookup would route the question and fail the asserts.
    kitchen.backend.fail_lookups(8);
    let actions = acted(kitchen.coordinate()?)?;
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, CoordinateAction::Unacknowledged { .. })),
        "{actions:?}"
    );
    assert!(!actions.iter().any(|action| matches!(
        action,
        CoordinateAction::Unroutable { .. } | CoordinateAction::Question { .. }
    )));
    assert!(kitchen.backend.next_delivery()?.is_some());
    // The backend answers again: the next pass routes the question.
    kitchen.backend.fail_lookups(0);
    let actions = acted(kitchen.coordinate()?)?;
    assert!(actions.contains(&CoordinateAction::Question {
        task,
        message: ExternalRef::new("early")?,
    }));
    assert_eq!(kitchen.backend.next_delivery(), Ok(None));
    Ok(())
}

// The house tick running the scheduled passes.

impl kitchen::contracts::CoordinatorMailbox for PausedLaunch<'_> {
    fn adopt_run(&self) -> Result<(), kitchen::contracts::MailboxError> {
        self.inner.adopt_run()
    }

    fn next_delivery(
        &self,
    ) -> Result<Option<kitchen::contracts::Delivery>, kitchen::contracts::MailboxError> {
        self.inner.next_delivery()
    }

    fn acknowledge(
        &self,
        delivery: &ExternalRef,
    ) -> Result<Option<kitchen::contracts::Delivery>, kitchen::contracts::MailboxError> {
        self.inner.acknowledge(delivery)
    }

    fn await_delivery(
        &self,
        wait: Duration,
    ) -> Result<Option<kitchen::contracts::Delivery>, kitchen::contracts::MailboxError> {
        self.inner.await_delivery(wait)
    }
}

/// A clock that moves one second at every reading, so each step of a pass
/// sees a later time than the step before.
struct Stepping<'a>(&'a ManualClock);

impl Clock for Stepping<'_> {
    fn now(&self) -> kitchen::contracts::Timestamp {
        self.0.advance(1);
        self.0.now()
    }
}

impl Kitchen {
    /// Schedule `passes` on the house tick every 15 minutes.
    fn schedule(&mut self, passes: &[TickPass]) -> TestResult {
        let mut scheduled = BTreeMap::new();
        for pass in passes {
            scheduled.insert(
                *pass,
                PassSchedule {
                    every_minutes: IntervalMinutes::new(15)?,
                },
            );
        }
        self.config.tick = Some(TickPolicy { passes: scheduled });
        Ok(())
    }

    /// One house tick as `holder`, running due passes on `backend`.
    fn tick_on(
        &self,
        backend: Option<&dyn kitchen::contracts::CoordinatorMailbox>,
        clock: &dyn Clock,
        holder: &str,
    ) -> TestResult<Vec<PassTick>> {
        let authors = ["kitchen-bot".to_owned()];
        let repair = RepairSettings {
            instructions: self.settings.instructions.clone(),
            report_path: self.settings.report_path.clone(),
        };
        let forge_backend = kitchen::BackendId::new("github").map_err(kitchen::Error::from)?;
        let mut passes = TickPasses {
            store: self.store(),
            house: &self.config,
            backend,
            forge: &self.forge,
            forge_executor: None,
            clock,
            repository: &self.settings.repository,
            pickup: Some(&self.settings),
            repair: Some(&repair),
            provenance: Some(&self.settings.instructions.provenance),
            forge_backend: &forge_backend,
            authors: &authors,
        };
        Ok(tick::tick(
            self.store(),
            &self.config,
            &HolderId::new(holder)?,
            &mut passes,
            clock,
        )?
        .passes)
    }

    fn tick(&self, holder: &str) -> TestResult<Vec<PassTick>> {
        self.tick_on(Some(&self.backend), &self.clock, holder)
    }
}

/// The only pass's decision.
fn decided(ticks: Vec<PassTick>) -> TestResult<TickDecision> {
    match <[PassTick; 1]>::try_from(ticks) {
        Ok([only]) => Ok(only.decision),
        Err(ticks) => Err(format!("expected one pass, got {ticks:?}").into()),
    }
}

fn ran(outcome: PassOutcome) -> impl Fn(&TickDecision) -> bool {
    move |decision| matches!(decision, TickDecision::Ran { outcome: ran, .. } if *ran == outcome)
}

#[test]
fn a_tick_runs_a_due_pickup_pass_and_records_its_task() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Pickup])?;
    kitchen.ready_seven();

    let decision = decided(kitchen.tick("tick-a")?)?;
    assert!(ran(PassOutcome::Done)(&decision), "{decision:?}");
    let (task, worker) = (kitchen.task(7)?, kitchen.worker(7)?);
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    let runs = kitchen.store().runs()?;
    let [run] = runs.as_slice() else {
        return Err(format!("expected one run, got {runs:?}").into());
    };
    assert_eq!(run.tasks, [task]);
    assert!(
        matches!(
            &run.state,
            RunState::Ended { outcome: PassOutcome::Done, backend_runs, .. }
                if *backend_runs == [worker.handle]
        ),
        "{run:?}"
    );
    // The pass released its workflow lease like `kitchn run` does.
    assert_eq!(kitchen.consumer(Pass::Pickup)?, Some(ConsumerState::Idle));

    assert!(matches!(
        decided(kitchen.tick("tick-b")?)?,
        TickDecision::NotDue { .. }
    ));
    // Due again, pickup finds the issue claimed and records no task.
    kitchen.clock.advance(15 * 60);
    assert!(ran(PassOutcome::Idle)(&decided(kitchen.tick("tick-c")?)?));
    let runs = kitchen.store().runs()?;
    assert!(matches!(runs.as_slice(), [_, idle] if idle.tasks.is_empty()));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn a_crash_mid_pass_leaves_the_tick_run_uncertain_and_blocks_the_pass() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Pickup])?;
    kitchen.ready_seven();
    // The process dies after the backend started the worker and before
    // the launch receipt is stored.
    let dying = PausedLaunch {
        inner: &kitchen.backend,
        during: RefCell::new(Some(Box::new(|_| {
            std::panic::resume_unwind(Box::new("the tick process died"))
        }))),
        lose_response: false,
    };
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        kitchen.tick_on(Some(&dying), &kitchen.clock, "tick-crashed")
    }));
    assert!(crashed.is_err());
    let task = kitchen.task(7)?;
    let runs = kitchen.store().runs()?;
    assert!(
        matches!(runs.as_slice(), [run] if run.state == RunState::Running && run.tasks == [task.clone()]),
        "{runs:?}"
    );
    assert_eq!(kitchen.store().task(&task)?.unresolved_effects().count(), 1);

    // While its tick lease lasts, the pass is busy.
    assert!(matches!(
        decided(kitchen.tick("tick-b")?)?,
        TickDecision::Busy { .. }
    ));
    // Once it lapses the run is uncertain, and it blocks the pass on every
    // later tick; nothing is launched again.
    kitchen.clock.advance(tick::PASS_LEASE.as_secs() + 1);
    for newly in [true, false] {
        let ticks = kitchen.tick("tick-c")?;
        assert!(
            matches!(
                ticks.as_slice(),
                [PassTick {
                    decision: TickDecision::Blocked { unresolved_effects: 1, .. },
                    newly_uncertain,
                    ..
                }] if *newly_uncertain == newly
            ),
            "{ticks:?}"
        );
    }
    assert!(matches!(
        kitchen.store().runs()?.as_slice(),
        [run] if matches!(run.state, RunState::Uncertain { .. })
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn a_tick_and_kitchn_run_never_run_the_same_pass_at_once() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Pickup])?;
    kitchen.ready_seven();
    let consumer = Pass::Pickup.consumer(&repo()?)?;
    let ttl = kitchen::contracts::LeaseTtl::new(PASS_LEASE)?;

    // `kitchn run pickup` is mid-pass: the tick records a busy run.
    let running =
        kitchen
            .store()
            .acquire_consumer(&consumer, &run_claimant()?, ttl, kitchen.clock.now())?;
    let busy = PassOutcome::Failed {
        reason: PassFailure::Busy,
    };
    assert!(ran(busy)(&decided(kitchen.tick("tick-a")?)?));
    // It died without a release: the tick never takes over.
    kitchen.clock.advance(PASS_LEASE.as_secs() + 1);
    let uncertain = PassOutcome::Failed {
        reason: PassFailure::OwnerUncertain,
    };
    assert!(ran(uncertain)(&decided(kitchen.tick("tick-b")?)?));
    assert_eq!(kitchen.backend.launched_agents().len(), 0);

    // A person takes it over and ends it; then a tick mid-pass holds the
    // lease against `kitchn run`.
    let taken = kitchen.store().take_over_consumer(
        &consumer,
        &run_claimant()?,
        ttl,
        kitchen.clock.now(),
    )?;
    assert_ne!(taken.fence(), running.fence());
    kitchen
        .store()
        .release_consumer(&consumer, taken.fence(), kitchen.clock.now())?;
    kitchen.clock.advance(15 * 60);
    let during = RefCell::new(None);
    let launching = PausedLaunch {
        inner: &kitchen.backend,
        during: RefCell::new(Some(Box::new(|_| {
            *during.borrow_mut() = Some(kitchen.pickup(false));
        }))),
        lose_response: false,
    };
    let decision = decided(kitchen.tick_on(Some(&launching), &kitchen.clock, "tick-c")?)?;
    assert!(ran(PassOutcome::Done)(&decision), "{decision:?}");
    assert!(matches!(
        during
            .borrow_mut()
            .take()
            .ok_or("the launch did not pause")??,
        Outcome::Busy
    ));
    assert_eq!(kitchen.backend.launched_agents().len(), 1);
    Ok(())
}

#[test]
fn a_tick_pass_renews_its_run_while_it_acts() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Pickup])?;
    kitchen.ready_seven();
    let clock = Stepping(&kitchen.clock);
    let seen = RefCell::new(None);
    let launching = PausedLaunch {
        inner: &kitchen.backend,
        during: RefCell::new(Some(Box::new(|_| {
            *seen.borrow_mut() = Some(kitchen.tick_on(Some(&kitchen.backend), &clock, "tick-b"));
        }))),
        lose_response: false,
    };
    let decision = decided(kitchen.tick_on(Some(&launching), &clock, "tick-a")?)?;
    assert!(ran(PassOutcome::Done)(&decision), "{decision:?}");
    let started = kitchen.store().runs()?.first().ok_or("no run")?.started_at;
    // A second tick during the launch saw the lease renewed past its first
    // expiry.
    let seen = seen.borrow_mut().take().ok_or("the launch did not pause")?;
    match decided(seen?)? {
        TickDecision::Busy { expires_at, .. } => {
            assert!(expires_at > started.saturating_add(tick::PASS_LEASE));
        }
        other => return Err(format!("expected busy, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_tick_without_a_backend_refuses_the_passes_that_need_one() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Pickup, TickPass::Coordinate, TickPass::Gate])?;
    kitchen.ready_seven();
    let ticks = kitchen.tick_on(None, &kitchen.clock, "tick-a")?;
    let refused = PassOutcome::Failed {
        reason: PassFailure::Refused,
    };
    let decisions: Vec<(TickPass, bool)> = ticks
        .iter()
        .map(|tick| (tick.pass, ran(refused)(&tick.decision)))
        .collect();
    assert_eq!(
        decisions,
        [
            (TickPass::Pickup, true),
            (TickPass::Coordinate, true),
            (TickPass::Gate, false)
        ]
    );
    // The gate needs no backend; with no settled task it is idle.
    assert!(
        ticks
            .last()
            .is_some_and(|tick| ran(PassOutcome::Idle)(&tick.decision))
    );
    assert_eq!(kitchen.backend.launched_agents().len(), 0);
    assert!(kitchen.store().tasks()?.is_empty());
    Ok(())
}

#[test]
fn a_tick_coordination_pass_records_the_tasks_it_continues() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Pickup, TickPass::Coordinate])?;
    kitchen.ready_seven();
    let ticks = kitchen.tick("tick-a")?;
    assert!(
        ticks
            .iter()
            .all(|tick| ran(PassOutcome::Done)(&tick.decision)),
        "{ticks:?}"
    );
    let task = kitchen.task(7)?;
    let runs = kitchen.store().runs()?;
    let coordinate = runs
        .iter()
        .find(|run| run.pass == TickPass::Coordinate)
        .ok_or("no coordination run")?;
    // Coordination moved the task from the ended pickup pass to its own claim.
    assert_eq!(coordinate.tasks, std::slice::from_ref(&task));
    assert!(matches!(
        kitchen.store().task(&task)?.state(),
        TaskState::Claimed { lease } if lease.consumer().is_none()
    ));
    Ok(())
}

/// A clock whose process dies at the first reading after coordination
/// claimed `task` under its pass lease.
struct DiesAfterAdoption<'a> {
    clock: &'a ManualClock,
    store: &'a HouseStore,
    task: kitchen::TaskId,
}

impl Clock for DiesAfterAdoption<'_> {
    fn now(&self) -> kitchen::contracts::Timestamp {
        let adopted = self.store.task(&self.task).is_ok_and(|record| {
            matches!(
                record.state(),
                TaskState::Claimed { lease }
                    if lease.consumer().is_some_and(|bound| bound.consumer.as_str() == "run-coordinate")
            )
        });
        if adopted {
            std::panic::resume_unwind(Box::new("the tick process died"));
        }
        self.clock.now()
    }
}

#[test]
fn a_tick_coordination_pass_records_a_handed_over_task_before_claiming_it() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    kitchen.schedule(&[TickPass::Coordinate])?;
    kitchen.ready_seven();
    acted(kitchen.pickup(false)?)?;
    let worker = kitchen.worker(7)?;
    kitchen
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
    kitchen.backend.post(vec![report(&worker, "done-7")?])?;
    kitchen
        .store()
        .link_pull_request(&kitchen.task(7)?, kitchen.claim_fence(7)?, pr(12)?)?;
    // A coordination pass fails and relinquishes its lease; its task is
    // handed over open, relinquished by the scheduled runner.
    assert!(kitchen.coordinate().is_err());
    let task = kitchen.task(7)?;
    kitchen
        .store()
        .relinquish(&task, kitchen.claim_fence(7)?, kitchen.clock.now())?;

    // The tick's coordination pass adopts the task, and its process dies
    // before anything else happens.
    let dying = DiesAfterAdoption {
        clock: &kitchen.clock,
        store: kitchen.store(),
        task: task.clone(),
    };
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        kitchen.tick_on(Some(&kitchen.backend), &dying, "tick-crashed")
    }));
    assert!(crashed.is_err());
    assert!(matches!(
        kitchen.store().task(&task)?.ownership().last(),
        Some(kitchen::state::OwnershipEvent::Adopted { .. })
    ));
    let runs = kitchen.store().runs()?;
    assert!(
        matches!(runs.as_slice(), [run] if run.state == RunState::Running && run.tasks == [task.clone()]),
        "{runs:?}"
    );

    // Once its tick lease lapses the run is uncertain and still names the
    // task it adopted.
    kitchen.clock.advance(tick::PASS_LEASE.as_secs() + 1);
    assert!(matches!(
        decided(kitchen.tick("tick-b")?)?,
        TickDecision::Blocked { .. }
    ));
    let runs = kitchen.store().runs()?;
    assert!(
        matches!(runs.as_slice(), [run] if matches!(run.state, RunState::Uncertain { .. }) && run.tasks == [task.clone()]),
        "{runs:?}"
    );
    Ok(())
}

/// A clock on which each stretch of forge reads between two readings takes
/// `stride` seconds, as slow forge calls would.
struct SlowForge<'a> {
    clock: &'a ManualClock,
    forge: &'a Forge,
    seen: std::cell::Cell<usize>,
    stride: u64,
}

impl Clock for SlowForge<'_> {
    fn now(&self) -> kitchen::contracts::Timestamp {
        let reads = self.forge.reads();
        if reads > self.seen.replace(reads) {
            self.clock.advance(self.stride);
        }
        self.clock.now()
    }
}

impl Kitchen {
    /// Launch, report, and settle issue `number`, with pull request
    /// `100 + number` open on its branch.
    fn settle_with_pull_request(&self, number: u64) -> TestResult {
        open_issues(self.forge(), vec![issue_json(number, &["ready"])]);
        ready_issue(self.forge(), number, ACCEPTANCE);
        acted(self.pickup(false)?)?;
        let worker = self.worker(number)?;
        self.backend
            .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Succeeded));
        self.backend
            .post(vec![report(&worker, &format!("done-{number}"))?])?;
        self.forge().set(
            &format!("repos/{REPO}/branches/kitchen/issue-{number}"),
            json!({"name": format!("kitchen/issue-{number}"), "commit": {"sha": commit('d')?.as_str()}}),
        );
        self.store().link_pull_request(
            &self.task(number)?,
            self.claim_fence(number)?,
            pr(100 + number)?,
        )?;
        acted(self.coordinate()?)?;
        let task = self.task(number)?;
        if !matches!(self.store().task(&task)?.state(), TaskState::Settled { .. }) {
            return Err(format!("task {number} did not settle").into());
        }
        open_issues(self.forge(), Vec::new());
        pull_request(self.forge(), number, 100 + number, true)?;
        self.forge()
            .set(&format!("repos/{REPO}/issues/{number}/timeline"), json!([]));
        Ok(())
    }
}

#[test]
fn tick_repair_and_gate_passes_renew_while_slow_forge_reads_outlast_their_leases() -> TestResult {
    let mut kitchen = Kitchen::new()?;
    let issues = [7, 8, 9, 10, 11];
    for number in issues {
        kitchen.settle_with_pull_request(number)?;
    }
    kitchen.forge().set(
        &format!("repos/{REPO}/branches/main"),
        json!({"name": "main", "commit": {"sha": commit('e')?.as_str()}}),
    );
    kitchen.schedule(&[TickPass::Repair, TickPass::Gate])?;
    // Each forge lookup or evaluation takes 14 minutes: under the 15-minute
    // pass lease, but five of them outlast the tick run's first hour.
    let stride = PASS_LEASE.as_secs() - 60;
    let slow = SlowForge {
        clock: &kitchen.clock,
        forge: kitchen.forge(),
        seen: std::cell::Cell::new(kitchen.forge().reads()),
        stride,
    };
    let ticks = kitchen.tick_on(Some(&kitchen.backend), &slow, "tick-a")?;
    assert!(
        ticks
            .iter()
            .all(|tick| ran(PassOutcome::Done)(&tick.decision)),
        "{ticks:?}"
    );
    let mut tasks = Vec::with_capacity(issues.len());
    for number in issues {
        tasks.push(kitchen.task(number)?);
    }
    for pass in [TickPass::Repair, TickPass::Gate] {
        let runs = kitchen.store().runs()?;
        let run = runs
            .iter()
            .find(|run| run.pass == pass)
            .ok_or("no run for the pass")?;
        let RunState::Ended { ended_at, .. } = run.state else {
            return Err(format!("{pass:?} did not end: {run:?}").into());
        };
        // The pass outlasted both the tick run's and its own first lease.
        assert!(
            ended_at > run.started_at.saturating_add(tick::PASS_LEASE),
            "{run:?}"
        );
        let expected = match pass {
            TickPass::Repair => issues.len(),
            TickPass::Gate => kitchen::workflows::run::MAX_GATE_PULL_REQUESTS,
            TickPass::Pickup | TickPass::Coordinate | TickPass::FollowUp => 0,
        };
        assert_eq!(run.tasks.len(), expected, "{run:?}");
        assert!(run.tasks.iter().all(|task| tasks.contains(task)));
    }
    assert_eq!(kitchen.consumer(Pass::Repair)?, Some(ConsumerState::Idle));
    assert_eq!(kitchen.consumer(Pass::Gate)?, Some(ConsumerState::Idle));
    Ok(())
}

#[test]
fn a_repair_pass_stops_when_a_forge_read_outlasts_its_lease() -> TestResult {
    let kitchen = Kitchen::new()?;
    for number in [7, 8] {
        kitchen.settle_with_pull_request(number)?;
    }
    // One lookup takes longer than the pass lease: the renewal before the
    // next item is refused and the pass assesses nothing.
    let slow = SlowForge {
        clock: &kitchen.clock,
        forge: kitchen.forge(),
        seen: std::cell::Cell::new(kitchen.forge().reads()),
        stride: PASS_LEASE.as_secs() + 60,
    };
    let stopped = RepairPass {
        store: kitchen.store(),
        house: &kitchen.config,
        backend: &kitchen.backend,
        forge: &kitchen.forge,
        clock: &slow,
        repository: &kitchen.settings.repository,
        settings: &RepairSettings {
            instructions: kitchen.settings.instructions.clone(),
            report_path: kitchen.settings.report_path.clone(),
        },
        take_over: false,
        tick: None,
    }
    .run();
    assert!(
        matches!(
            stopped,
            Err(kitchen::Error::State(StateError::LeaseExpired { .. }))
        ),
        "{stopped:?}"
    );
    // The failed pass relinquished its lease, so the next start adopts it
    // and assesses both pull requests.
    assert!(matches!(
        kitchen.consumer(Pass::Repair)?,
        Some(ConsumerState::Relinquished { .. })
    ));
    assert_eq!(acted(kitchen.repair()?)?.len(), 2);
    Ok(())
}

/// A separate forge identity for the expediter, with review write authority.
fn review_fixture(
    kitchen: &mut Kitchen,
    login: &str,
) -> TestResult<(GitHubClient<Forge>, GitHubExecutor<Forge>)> {
    let grant = Grant::repository(
        kitchen::contracts::Permission::ReviewPullRequest,
        repo()?,
        kitchen::BackendId::new("github")?,
        CredentialId::new("forge")?,
    );
    kitchen.config.grants.insert(grant.clone());
    kitchen.config.policy_limits.insert(grant);
    let requester = ExternalRef::new(login)?;
    let scope = HouseScope::new(
        house()?,
        [repo()?],
        requester.clone(),
        CredentialRef::new(house()?, CredentialId::new("forge")?, requester),
        PostingBudget::new(3)?,
        [kitchen::contracts::Permission::ReviewPullRequest],
    )?;
    let forge = GitHubClient::new(
        scope.clone(),
        kitchen.forge().clone(),
        ReadLimits::default(),
    );
    let executor = GitHubExecutor::new(
        kitchen::BackendId::new("github")?,
        scope,
        kitchen.forge().clone(),
        ReadLimits::default(),
    );
    Ok((forge, executor))
}

fn review_input(verdict: ReviewVerdict) -> TestResult<GateReviewInput> {
    Ok(GateReviewInput {
        repository: repo()?,
        pull_request: pr(12)?,
        head: commit('d')?,
        verdict,
        findings: "The committed diff is clean.".to_owned(),
        semantic: Some(kitchen::workflows::gate::SemanticReview::Clean),
        acceptance: Some(true),
        hardware: Some(true),
        risk: Some(Vec::new()),
        attest: true,
    })
}

#[test]
fn posted_review_round_trips_through_attestation_and_scheduled_gate() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    kitchen.config = with_merge_grant(house_config()?)?;
    green_pull_request(&kitchen)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let review = post_gate_review(
        kitchen.store(),
        &kitchen.config,
        &forge,
        &executor,
        &kitchen.settings.instructions.provenance,
        &kitchen.clock,
        &review_input(ReviewVerdict::Approve)?,
    )?;
    assert_eq!(review.id.get(), 19);
    assert!(review.attested);
    let posted = kitchen.forge().writes.borrow();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0, format!("repos/{REPO}/pulls/12/reviews"));
    assert!(
        posted[0].1["body"]
            .as_str()
            .ok_or("body")?
            .contains("```kitchen-attestation")
    );
    drop(posted);
    assert_eq!(
        recorded_at_d(&kitchen)?
            .ok_or("attestation")?
            .attestation
            .forge_review
            .id
            .get(),
        19
    );
    let action = one_verdict(kitchen.gate()?)?;
    assert_eq!(action.result, GateResult::Merged, "{action:?}");
    assert_eq!(kitchen.forge().writes.borrow().len(), 2);
    Ok(())
}

#[test]
fn stale_head_and_request_changes_claims_never_post() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let mut input = review_input(ReviewVerdict::Approve)?;
    input.head = commit('c')?;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input
        ),
        Err(kitchen::Error::Run(RunError::AttestationStaleHead))
    ));
    let mut input = review_input(ReviewVerdict::RequestChanges)?;
    input.attest = false;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input
        ),
        Err(kitchen::Error::Run(RunError::ReviewClaimsWithoutApproval))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn uncertain_review_post_is_found_by_marker_without_a_second_post() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    kitchen.forge().lose_review_reply.set(true);
    let mut input = review_input(ReviewVerdict::Approve)?;
    input.attest = false;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input
        ),
        Err(kitchen::Error::Run(RunError::ReviewUncertain))
    ));
    let other_backend = GitHubExecutor::new(
        kitchen::BackendId::new("other-github")?,
        executor.scope().clone(),
        kitchen.forge().clone(),
        ReadLimits::default(),
    );
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &other_backend,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input,
        ),
        Err(kitchen::Error::State(StateError::TaskConflict(_)))
    ));
    assert_eq!(kitchen.forge().writes.borrow().len(), 1);
    let mut changed = kitchen.settings.instructions.provenance.clone();
    changed.repository_instructions = Some(commit('f')?);
    let recovered = post_gate_review(
        kitchen.store(),
        &kitchen.config,
        &forge,
        &executor,
        &changed,
        &kitchen.clock,
        &input,
    )?;
    assert_eq!(recovered.id.get(), 19);
    assert_eq!(kitchen.forge().writes.borrow().len(), 1);
    assert!(kitchen.store().tasks()?.iter().any(|task| {
        task.spec().id.as_str().starts_with("gate-review-")
            && matches!(task.state(), TaskState::Settled { .. })
    }));
    Ok(())
}

#[test]
fn refused_review_attempt_can_retry_and_settle_from_later_effect() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    let reviews_path = format!("repos/{REPO}/pulls/12/reviews");
    kitchen.forge().set(&reviews_path, json!([]));
    // The first effect cannot inspect existing reviews, so it is not applied.
    kitchen.forge().queue(&reviews_path, vec![json!({})]);
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let mut input = review_input(ReviewVerdict::Approve)?;
    input.attest = false;
    let post = || {
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input,
        )
    };
    assert!(matches!(
        post(),
        Err(kitchen::Error::Run(RunError::ReviewPostRefused))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    let review = post()?;
    assert_eq!(review.id.get(), 19);
    assert!(!review.attested);
    assert_eq!(kitchen.forge().writes.borrow().len(), 1);
    let task = kitchen
        .store()
        .tasks()?
        .into_iter()
        .find(|task| task.spec().id.as_str().starts_with("gate-review-"))
        .ok_or("review task")?;
    assert!(matches!(
        task.effects()[0].state(),
        EffectState::NotApplied { .. }
    ));
    assert!(matches!(
        task.effects()[1].state(),
        EffectState::Applied { .. }
    ));
    assert!(matches!(
        task.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    assert_eq!(post()?.id.get(), 19);
    assert_eq!(kitchen.forge().writes.borrow().len(), 1);
    Ok(())
}

#[test]
fn three_refused_review_attempts_remain_refused_after_exhaustion() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    let reviews_path = format!("repos/{REPO}/pulls/12/reviews");
    kitchen.forge().set(&reviews_path, json!([]));
    kitchen
        .forge()
        .queue(&reviews_path, vec![json!({}), json!({}), json!({})]);
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let mut input = review_input(ReviewVerdict::Approve)?;
    input.attest = false;
    for _ in 0..5 {
        assert!(matches!(
            post_gate_review(
                kitchen.store(),
                &kitchen.config,
                &forge,
                &executor,
                &kitchen.settings.instructions.provenance,
                &kitchen.clock,
                &input,
            ),
            Err(kitchen::Error::Run(RunError::ReviewPostRefused))
        ));
    }
    assert!(kitchen.forge().writes.borrow().is_empty());
    let task = kitchen
        .store()
        .tasks()?
        .into_iter()
        .find(|task| task.spec().id.as_str().starts_with("gate-review-"))
        .ok_or("review task")?;
    assert_eq!(task.attempts().len(), 3);
    assert_eq!(task.effects().len(), 3);
    assert!(
        task.effects()
            .iter()
            .all(|effect| matches!(effect.state(), EffectState::NotApplied { .. }))
    );
    assert!(matches!(
        task.state(),
        TaskState::Settled {
            settlement: Settlement::Exhausted,
            ..
        }
    ));
    Ok(())
}

#[test]
fn review_by_pull_request_author_creates_no_intent_or_post() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    let (forge, executor) = review_fixture(&mut kitchen, "kitchen-bot")?;
    let input = review_input(ReviewVerdict::Approve)?;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input
        ),
        Err(kitchen::Error::Run(RunError::AttestationNotIndependent))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    assert!(
        !kitchen.store().tasks()?.iter().any(|task| task
            .spec()
            .id
            .as_str()
            .starts_with("gate-review-"))
    );
    assert!(recorded_at_d(&kitchen)?.is_none());
    // A retry is refused before creating intent too.
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &input
        ),
        Err(kitchen::Error::Run(RunError::AttestationNotIndependent))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn review_by_branch_committer_creates_no_intent_or_post() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    commits(
        &kitchen,
        json!([commit_json(
            'd',
            Some("other-author"),
            Some("safety-reviewer")
        )?]),
    );
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &review_input(ReviewVerdict::Approve)?,
        ),
        Err(kitchen::Error::Run(RunError::AttestationNotIndependent))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    assert!(
        !kitchen.store().tasks()?.iter().any(|task| task
            .spec()
            .id
            .as_str()
            .starts_with("gate-review-"))
    );
    Ok(())
}

#[test]
fn review_with_unknown_commit_writer_creates_no_intent_or_post() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    commits(
        &kitchen,
        json!([commit_json('d', None, Some("kitchen-bot"))?]),
    );
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &review_input(ReviewVerdict::Approve)?,
        ),
        Err(kitchen::Error::Run(RunError::AttestationWritersUnknown))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    assert!(
        !kitchen.store().tasks()?.iter().any(|task| task
            .spec()
            .id
            .as_str()
            .starts_with("gate-review-"))
    );
    Ok(())
}

#[test]
fn request_changes_posts_one_unattested_review_without_claim_flags() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let mut input = review_input(ReviewVerdict::RequestChanges)?;
    input.semantic = None;
    input.acceptance = None;
    input.hardware = None;
    input.risk = None;
    input.attest = false;
    let review = post_gate_review(
        kitchen.store(),
        &kitchen.config,
        &forge,
        &executor,
        &kitchen.settings.instructions.provenance,
        &kitchen.clock,
        &input,
    )?;
    assert_eq!(review.id.get(), 19);
    assert!(!review.attested);
    assert_eq!(
        kitchen.forge().writes.borrow()[0].1["event"],
        "REQUEST_CHANGES"
    );
    assert!(recorded_at_d(&kitchen)?.is_none());
    Ok(())
}

#[test]
fn base_moving_between_review_checks_is_refused_without_posting() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    let base_path = format!("repos/{REPO}/branches/main");
    kitchen.forge().queue(
        &base_path,
        vec![json!({"name":"main", "commit":{"sha":commit('e')?.as_str()}})],
    );
    kitchen.forge().set(
        &base_path,
        json!({"name":"main", "commit":{"sha":commit('f')?.as_str()}}),
    );
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    assert!(matches!(
        post_gate_review(
            kitchen.store(),
            &kitchen.config,
            &forge,
            &executor,
            &kitchen.settings.instructions.provenance,
            &kitchen.clock,
            &review_input(ReviewVerdict::Approve)?
        ),
        Err(kitchen::Error::Run(RunError::AttestationStaleBase))
    ));
    assert!(kitchen.forge().writes.borrow().is_empty());
    Ok(())
}

#[test]
fn review_base_tip_preserves_branch_slashes_and_encodes_reserved_bytes() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    set_pull_request(&kitchen, "/base/ref", json!("release/v1#final"))?;
    let base_path = format!("repos/{REPO}/branches/release/v1%23final");
    kitchen.forge().set(
        &base_path,
        json!({"name":"release/v1#final", "commit":{"sha":commit('e')?.as_str()}}),
    );
    kitchen
        .forge()
        .set(&format!("repos/{REPO}/pulls/12/reviews"), json!([]));
    let (forge, executor) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let mut input = review_input(ReviewVerdict::RequestChanges)?;
    input.semantic = None;
    input.acceptance = None;
    input.hardware = None;
    input.risk = None;
    input.attest = false;
    let review = post_gate_review(
        kitchen.store(),
        &kitchen.config,
        &forge,
        &executor,
        &kitchen.settings.instructions.provenance,
        &kitchen.clock,
        &input,
    )?;
    assert_eq!(review.id.get(), 19);
    assert_eq!(
        kitchen
            .forge()
            .reads
            .borrow()
            .iter()
            .filter(|path| *path == &base_path)
            .count(),
        2,
    );
    assert_eq!(kitchen.forge().writes.borrow().len(), 1);
    Ok(())
}

#[test]
fn gate_review_reads_carry_the_app_token_scope() -> TestResult {
    let mut kitchen = settled_with_pull_request(true)?;
    green_pull_request(&kitchen)?;
    let (forge, _) = review_fixture(&mut kitchen, "safety-reviewer")?;
    let scoped = forge.with_read_access(TokenScope::for_review(repo()?))?;
    let start = kitchen.forge().read_accesses.borrow().len();
    assert!(matches!(
        scoped.pull_request(&house()?, &repo()?, pr(12)?),
        kitchen::integrations::github::Observation::Known(_)
    ));
    assert!(matches!(
        scoped.branch_tip(&house()?, &repo()?, &BranchName::new("main")?),
        kitchen::integrations::github::Observation::Known(_)
    ));
    let accesses = kitchen.forge().read_accesses.borrow();
    assert_eq!(accesses.len() - start, 2);
    for access in &accesses[start..] {
        let scope = access.as_ref().ok_or("missing app token scope")?;
        assert_eq!(scope.repository(), &repo()?);
        assert_eq!(
            scope.permissions().collect::<Vec<_>>(),
            vec![
                (
                    kitchen::integrations::github::AppPermission::Contents,
                    kitchen::integrations::github::Access::Read
                ),
                (
                    kitchen::integrations::github::AppPermission::PullRequests,
                    kitchen::integrations::github::Access::Write
                ),
            ]
        );
    }
    Ok(())
}
