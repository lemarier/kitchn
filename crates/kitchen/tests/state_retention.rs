//! The house store's retention policy: what a pass removes, what it keeps,
//! and that dedupe still holds afterwards. Simulated with temporary stores
//! and a fake backend; forge and backend observations are supplied directly.

mod common;

use std::{
    fs,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{Fixture, TestResult, at, creator, grants, launch, plan, scheduled, ttl};
use kitchen::{
    Error, TaskId, WorkflowId,
    contracts::{
        AttemptNumber, AttemptOutcome, EvidenceSubject, ExternalRef, FailureClass, IssueNumber,
        Permission, PostingBudget, Receipt, Repository, ResourceKind, ResourceRef,
    },
    integrations::github::{
        CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError, ReadLimits,
        ReadRequest,
    },
    state::{
        CAPACITY_WARNING_PERCENT, EffectOutcome, EffectStart, Inventory, MAX_MARKERS,
        MIN_TASK_WINDOW, MarkerFact, MarkerKey, MarkerRecording, MarkerRetirement, MarkerSubject,
        Presence, RetentionPolicy, RetentionSubjects, StateError, TableUsage, TaskRetirement,
        WorkItem,
    },
    workflows::{
        pickup::{IssueRef, issue_task_id},
        repair::repair_task_id,
    },
};

const DAY: u64 = 24 * 60 * 60;
/// Just past the default settled-task window, counted from time zero.
const LATER: u64 = MIN_TASK_WINDOW.as_secs() + DAY;

fn repo() -> TestResult<Repository> {
    Ok(Repository::new("origin89hq/km43")?)
}

fn number(value: u64) -> TestResult<NonZeroU64> {
    Ok(NonZeroU64::new(value).ok_or("zero")?)
}

fn pull_request(value: u64) -> TestResult<WorkItem> {
    Ok(WorkItem::PullRequest {
        repository: repo()?,
        number: number(value)?,
    })
}

fn issue(value: u64) -> TestResult<WorkItem> {
    Ok(WorkItem::Issue {
        repository: repo()?,
        number: number(value)?,
    })
}

fn key(workflow: &str, item: WorkItem, head: char) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(workflow)?,
        item,
        subject: MarkerSubject::Git(EvidenceSubject {
            head: common::commit(head)?,
            base: None,
        }),
    })
}

fn fact(schema: &str) -> TestResult<MarkerFact> {
    Ok(MarkerFact::workflow(schema.parse()?, &"fact")?)
}

fn record(fixture: &Fixture, key: &MarkerKey, fact: MarkerFact, when: u64) -> TestResult {
    fixture
        .store
        .record_marker(key.clone(), fact, &scheduled("recorder")?, at(when))?;
    Ok(())
}

fn worktree(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Worktree,
        backend: common::backend_id()?,
        handle: ExternalRef::new(handle)?,
    })
}

/// Create, claim, and settle `id` for `repository` at `settled` seconds,
/// after one launch that created `created`, if any.
fn settled_task(
    fixture: &Fixture,
    id: &TaskId,
    created: Option<ResourceRef>,
    outcome: AttemptOutcome,
    settled: u64,
) -> TestResult {
    let mut spec = common::spec(id.as_str())?;
    spec.repository = Some(repo()?);
    let store = &fixture.store;
    store.create_task(spec, &creator()?, at(0))?;
    let fence = store
        .claim(id, &scheduled("worker")?, ttl(3600)?, at(0))?
        .fence();
    store.start_attempt(id, fence, at(0))?;
    if let Some(resource) = created {
        let EffectStart::Execute(intent) = store.begin_effect(
            plan(id, fence, "launch", launch()?)?,
            &grants()?,
            &common::refusing()?,
            at(1),
        )?
        else {
            return Err("expected a new effect".into());
        };
        let receipt = Receipt::new(ExternalRef::new("launch-1")?, vec![resource], Vec::new())?;
        store.record_effect_outcome(
            id,
            fence,
            intent.seq(),
            EffectOutcome::Applied(receipt),
            at(2),
        )?;
    }
    store.finish_attempt(id, fence, AttemptNumber::FIRST, outcome, at(settled))?;
    Ok(())
}

fn issue_task(value: u64) -> TestResult<TaskId> {
    Ok(issue_task_id(&IssueRef {
        repository: repo()?,
        number: IssueNumber::new(value)?,
    })?)
}

fn closed(items: &[WorkItem]) -> Inventory {
    let mut inventory = Inventory::new();
    for item in items {
        inventory.observe(item.clone(), Presence::Gone);
    }
    inventory
}

#[test]
fn markers_of_a_closed_item_retire_and_others_stay() -> TestResult {
    let fixture = Fixture::new()?;
    let closed_pr = key("merge-gate", pull_request(1)?, 'a')?;
    let open_pr = key("merge-gate", pull_request(2)?, 'a')?;
    let unobserved = key("merge-gate", pull_request(3)?, 'a')?;
    let closed_budget = key("merge-gate-budget", pull_request(1)?, 'a')?;
    let open_budget = key("merge-gate-budget", pull_request(2)?, 'a')?;
    for marker in [&closed_pr, &open_pr, &unobserved] {
        record(&fixture, marker, fact("gate.verdict/1")?, 1)?;
    }
    for marker in [&closed_budget, &open_budget] {
        record(&fixture, marker, fact("gate.subject-budget/1")?, 1)?;
    }
    let mut inventory = closed(&[pull_request(1)?]);
    inventory.observe(pull_request(2)?, Presence::Present);
    let policy = RetentionPolicy::default();

    // A preview reports the same removal and writes nothing.
    let preview = fixture
        .store
        .preview_retention(&policy, &inventory, at(2))?;
    assert!(!preview.applied);
    assert_eq!(preview.markers.len(), 2);
    assert!(fixture.store.marker(&closed_pr)?.is_some());

    let report = fixture
        .store
        .retain(&policy, &inventory, &creator()?, at(2))?;
    assert!(report.applied);
    assert_eq!(report.markers, preview.markers);
    assert!(report.markers.iter().any(|marker| marker.key == closed_pr));
    assert!(
        report
            .markers
            .iter()
            .any(|marker| marker.key == closed_budget)
    );
    assert!(
        report
            .markers
            .iter()
            .all(|marker| marker.reason == MarkerRetirement::ItemGone)
    );
    assert_eq!(fixture.store.marker(&closed_pr)?, None);
    assert_eq!(fixture.store.marker(&closed_budget)?, None);
    // An open or unobserved pull request keeps every head's verdict, which
    // the gate's fix budget counts.
    assert!(fixture.store.marker(&open_pr)?.is_some());
    assert!(fixture.store.marker(&open_budget)?.is_some());
    assert!(fixture.store.marker(&unobserved)?.is_some());
    // A second pass finds nothing more.
    assert!(
        !fixture
            .store
            .retain(&policy, &inventory, &creator()?, at(3))?
            .applied
    );
    Ok(())
}

#[test]
fn only_the_newest_subject_of_a_latest_subject_marker_stays() -> TestResult {
    let fixture = Fixture::new()?;
    let older = key("ready-report", pull_request(4)?, 'a')?;
    let newer = key("ready-report", pull_request(4)?, 'b')?;
    let other = key("ready-report", pull_request(5)?, 'a')?;
    record(&fixture, &older, fact("ready-report/1")?, 1)?;
    record(&fixture, &newer, fact("ready-report/1")?, 2)?;
    record(&fixture, &other, fact("ready-report/1")?, 1)?;

    let report = fixture.store.retain(
        &RetentionPolicy::default(),
        &Inventory::new(),
        &creator()?,
        at(3),
    )?;
    assert_eq!(report.markers.len(), 1);
    assert_eq!(report.markers[0].key, older);
    assert_eq!(report.markers[0].reason, MarkerRetirement::Superseded);
    // Dedupe after retention: the current head is still reported once.
    assert!(matches!(
        fixture.store.record_marker(
            newer.clone(),
            fact("ready-report/1")?,
            &scheduled("again")?,
            at(4)
        )?,
        MarkerRecording::AlreadyRecorded(_)
    ));
    // Once the pull request closes, its newest subject retires too.
    let report = fixture.store.retain(
        &RetentionPolicy::default(),
        &closed(&[pull_request(4)?]),
        &creator()?,
        at(5),
    )?;
    assert_eq!(report.markers.len(), 1);
    assert_eq!(report.markers[0].key, newer);
    assert!(fixture.store.marker(&other)?.is_some());
    Ok(())
}

#[test]
fn facts_dedupe_needs_are_kept_even_when_their_item_is_gone() -> TestResult {
    let fixture = Fixture::new()?;
    let gone = issue(6)?;
    let kept = [
        (
            key("triage", gone.clone(), 'a')?,
            MarkerFact::QuestionAsked {
                question: ExternalRef::new("ask-1")?,
            },
        ),
        (
            key("deliberation", gone.clone(), 'b')?,
            fact("deliberation.entry/1")?,
        ),
        (
            key("schedule-budget", gone.clone(), 'c')?,
            fact("schedule-budget-undeliverable/1")?,
        ),
        (
            key("intake", gone.clone(), 'd')?,
            fact("intake.reservation/1")?,
        ),
        (key("intake", gone.clone(), 'e')?, fact("intake.counted/1")?),
        // A schema without a stated rule is kept.
        (key("future", gone.clone(), 'f')?, fact("future.fact/1")?),
    ];
    for (marker, value) in &kept {
        record(&fixture, marker, value.clone(), 1)?;
    }
    let report = fixture.store.retain(
        &RetentionPolicy::default(),
        &closed(&[gone]),
        &creator()?,
        at(2),
    )?;
    assert!(report.markers.is_empty(), "{report:?}");
    for (marker, _) in &kept {
        assert!(fixture.store.marker(marker)?.is_some());
    }
    Ok(())
}

#[test]
fn an_unsettled_task_keeps_its_items_markers() -> TestResult {
    let fixture = Fixture::new()?;
    let marker = key("gardener", issue(7)?, 'a')?;
    record(&fixture, &marker, fact("gardener.stale-handled/1")?, 1)?;
    let task = issue_task(7)?;
    let mut spec = common::spec(task.as_str())?;
    spec.repository = Some(repo()?);
    fixture.store.create_task(spec, &creator()?, at(0))?;
    fixture
        .store
        .claim(&task, &scheduled("worker")?, ttl(60)?, at(0))?;

    let report = fixture.store.retain(
        &RetentionPolicy::default(),
        &closed(&[issue(7)?]),
        &creator()?,
        at(LATER),
    )?;
    assert!(report.markers.is_empty() && report.tasks.is_empty());
    assert!(fixture.store.marker(&marker)?.is_some());
    Ok(())
}

#[test]
fn a_settled_repair_task_retires_after_its_window_once_its_pull_request_and_worktree_are_gone()
-> TestResult {
    let fixture = Fixture::new()?;
    let task = repair_task_id(&repo()?, IssueNumber::new(8)?, 1)?;
    let resource = worktree("wt-8")?;
    settled_task(
        &fixture,
        &task,
        Some(resource.clone()),
        AttemptOutcome::Succeeded,
        3,
    )?;
    let policy = RetentionPolicy::default();
    let mut inventory = closed(&[pull_request(8)?]);

    // The backend was not listed: the worktree may still exist.
    assert!(
        fixture
            .store
            .retain(&policy, &inventory, &creator()?, at(LATER))?
            .tasks
            .is_empty()
    );
    // Still listed.
    inventory.list_backend(common::backend_id()?, [resource]);
    assert!(
        fixture
            .store
            .retain(&policy, &inventory, &creator()?, at(LATER))?
            .tasks
            .is_empty()
    );
    // Gone from a complete listing, but not yet settled for the window.
    inventory.list_backend(common::backend_id()?, []);
    assert!(
        fixture
            .store
            .retain(&policy, &inventory, &creator()?, at(DAY))?
            .tasks
            .is_empty()
    );
    // An open pull request could be repaired again under the same id.
    let mut open = inventory.clone();
    open.observe(pull_request(8)?, Presence::Present);
    assert!(
        fixture
            .store
            .retain(&policy, &open, &creator()?, at(LATER))?
            .tasks
            .is_empty()
    );

    let report = fixture
        .store
        .retain(&policy, &inventory, &creator()?, at(LATER))?;
    assert_eq!(report.tasks.len(), 1);
    assert_eq!(report.tasks[0].task, task);
    assert_eq!(report.tasks[0].reason, TaskRetirement::ItemGone);
    assert!(matches!(
        fixture.store.task(&task),
        Err(Error::State(StateError::TaskNotFound(_)))
    ));
    Ok(())
}

#[test]
fn a_settled_issue_task_is_kept_after_its_issue_closes() -> TestResult {
    let fixture = Fixture::new()?;
    let task = issue_task(11)?;
    settled_task(&fixture, &task, None, AttemptOutcome::Succeeded, 3)?;
    // A pull request may still be open and its gate verdicts may name this
    // task's effects, so closing the issue alone never removes it.
    let report = fixture.store.retain(
        &RetentionPolicy::default(),
        &closed(&[issue(11)?]),
        &creator()?,
        at(LATER),
    )?;
    assert!(report.tasks.is_empty());
    assert!(fixture.store.task(&task).is_ok());
    Ok(())
}

#[test]
fn a_failed_write_no_person_acknowledged_keeps_its_task() -> TestResult {
    let fixture = Fixture::new()?;
    let task = repair_task_id(&repo()?, IssueNumber::new(9)?, 1)?;
    let resource = worktree("wt-9")?;
    settled_task(
        &fixture,
        &task,
        Some(resource),
        AttemptOutcome::Failed(FailureClass::Permanent),
        3,
    )?;
    let mut inventory = closed(&[pull_request(9)?]);
    inventory.list_backend(common::backend_id()?, []);
    let report = fixture.store.retain(
        &RetentionPolicy::default(),
        &inventory,
        &creator()?,
        at(LATER),
    )?;
    assert!(report.tasks.is_empty());
    assert!(fixture.store.task(&task).is_ok());
    Ok(())
}

#[test]
fn repair_rounds_of_a_pull_request_retire_together() -> TestResult {
    let fixture = Fixture::new()?;
    let pr = IssueNumber::new(10)?;
    let first = repair_task_id(&repo()?, pr, 1)?;
    let second = repair_task_id(&repo()?, pr, 2)?;
    settled_task(&fixture, &first, None, AttemptOutcome::Succeeded, 3)?;
    settled_task(&fixture, &second, None, AttemptOutcome::Succeeded, 2 * DAY)?;
    let policy = RetentionPolicy::default();
    let inventory = closed(&[pull_request(10)?]);

    // The second round has not been settled for the window, so the first
    // stays too: rounds count from the first missing one.
    assert!(
        fixture
            .store
            .retain(&policy, &inventory, &creator()?, at(LATER))?
            .tasks
            .is_empty()
    );
    let report = fixture
        .store
        .retain(&policy, &inventory, &creator()?, at(LATER + 2 * DAY))?;
    let mut retired: Vec<TaskId> = report.tasks.into_iter().map(|task| task.task).collect();
    retired.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(retired, expected);
    Ok(())
}

#[test]
fn budget_windows_retire_after_the_window_and_unknown_tasks_stay() -> TestResult {
    let fixture = Fixture::new()?;
    let window = TaskId::new("budget-1000")?;
    let other = TaskId::new("decompose-abc")?;
    for id in [&window, &other] {
        let spec = common::spec(id.as_str())?;
        fixture.store.create_task(spec, &creator()?, at(0))?;
        let fence = fixture
            .store
            .claim(id, &scheduled("tick")?, ttl(60)?, at(0))?
            .fence();
        fixture.store.start_attempt(id, fence, at(0))?;
        fixture.store.finish_attempt(
            id,
            fence,
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(1),
        )?;
    }
    let policy = RetentionPolicy::default();
    assert!(
        fixture
            .store
            .retain(&policy, &Inventory::new(), &creator()?, at(DAY))?
            .tasks
            .is_empty()
    );
    let report = fixture
        .store
        .retain(&policy, &Inventory::new(), &creator()?, at(LATER))?;
    assert_eq!(report.tasks.len(), 1);
    assert_eq!(report.tasks[0].task, window);
    assert_eq!(report.tasks[0].reason, TaskRetirement::WindowEnded);
    assert!(fixture.store.task(&other).is_ok());
    Ok(())
}

#[test]
fn a_window_shorter_than_the_longest_budget_window_is_refused() -> TestResult {
    assert!(matches!(
        RetentionPolicy::new(MIN_TASK_WINDOW - Duration::from_secs(1)),
        Err(StateError::RetentionWindowTooShort)
    ));
    assert_eq!(
        RetentionPolicy::new(MIN_TASK_WINDOW)?.task_window(),
        MIN_TASK_WINDOW
    );
    assert_eq!(
        MIN_TASK_WINDOW.as_secs(),
        u64::from(kitchen::scheduling::MAX_WINDOW_HOURS) * 60 * 60
    );
    Ok(())
}

#[test]
fn a_full_marker_table_accepts_new_work_after_retention() -> TestResult {
    let fixture = Fixture::new()?;
    let template = key("merge-gate", pull_request(1)?, 'a')?;
    record(&fixture, &template, fact("gate.verdict/1")?, 1)?;
    // Fill the table by copying the one valid marker, one pull request each.
    let path = fixture.state_path();
    let mut state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let marker = state["markers"][0].clone();
    let markers: Vec<_> = (1..=MAX_MARKERS)
        .map(|value| {
            let mut copy = marker.clone();
            copy["key"]["item"]["number"] = value.into();
            copy
        })
        .collect();
    state["markers"] = markers.into();
    fs::write(&path, serde_json::to_vec(&state)?)?;
    let store = fixture.reopen()?;

    let capacity = store.capacity()?;
    assert_eq!(capacity.markers.used, MAX_MARKERS);
    assert!(capacity.near_limit());
    let extra = key("ready-report", pull_request(99_999)?, 'a')?;
    assert!(matches!(
        store.record_marker(
            extra.clone(),
            fact("ready-report/1")?,
            &scheduled("ready")?,
            at(2)
        ),
        Err(Error::State(StateError::CapacityExceeded { .. }))
    ));
    // The subjects to observe include every pull request a verdict names.
    assert_eq!(store.retention_subjects()?.items.len(), MAX_MARKERS);

    let report = store.retain(
        &RetentionPolicy::default(),
        &closed(&[pull_request(1)?, pull_request(2)?]),
        &creator()?,
        at(3),
    )?;
    assert_eq!(report.markers.len(), 2);
    assert!(matches!(
        store.record_marker(extra, fact("ready-report/1")?, &scheduled("ready")?, at(4))?,
        MarkerRecording::Recorded(_)
    ));
    assert_eq!(store.capacity()?.markers.used, MAX_MARKERS - 1);
    Ok(())
}

#[test]
fn capacity_warns_from_the_threshold() {
    let limit = 1000;
    let threshold = limit * CAPACITY_WARNING_PERCENT / 100;
    assert!(
        !TableUsage {
            used: threshold - 1,
            limit
        }
        .near_limit()
    );
    assert!(
        TableUsage {
            used: threshold,
            limit
        }
        .near_limit()
    );
    assert!(!TableUsage { used: 0, limit }.near_limit());
}

#[test]
fn capacity_counts_markers_by_workflow_and_settled_tasks() -> TestResult {
    let fixture = Fixture::new()?;
    record(
        &fixture,
        &key("merge-gate", pull_request(1)?, 'a')?,
        fact("gate.verdict/1")?,
        1,
    )?;
    record(
        &fixture,
        &key("ready-report", pull_request(1)?, 'a')?,
        fact("ready-report/1")?,
        1,
    )?;
    record(
        &fixture,
        &key("ready-report", pull_request(2)?, 'a')?,
        fact("ready-report/1")?,
        1,
    )?;
    settled_task(
        &fixture,
        &issue_task(1)?,
        None,
        AttemptOutcome::Succeeded,
        1,
    )?;
    let capacity = fixture.store.capacity()?;
    assert_eq!(capacity.markers.used, 3);
    assert_eq!(capacity.tasks.used, 1);
    assert_eq!(capacity.settled_tasks, 1);
    assert_eq!(
        capacity
            .markers_by_workflow
            .get(&WorkflowId::new("ready-report")?),
        Some(&2)
    );
    assert!(!capacity.near_limit());
    Ok(())
}

/// A forge that answers each endpoint suffix with a fixed body, or fails.
struct Forge(Vec<(&'static str, Option<serde_json::Value>)>);

impl GitHubReadTransport for Forge {
    fn read(
        &self,
        _: &CredentialRef,
        request: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let body = self
            .0
            .iter()
            .find(|(suffix, _)| request.endpoint().ends_with(suffix))
            .and_then(|(_, body)| body.as_ref())
            .ok_or(IntegrationError::Unavailable)?;
        serde_json::to_vec(body).map_err(|_| IntegrationError::Unknown)
    }
}

fn issue_json(value: u64, state: &str) -> serde_json::Value {
    serde_json::json!({"repository_url":"https://api.github.com/repos/origin89hq/km43","id":value,"number":value,"title":"issue","state":state,"assignees":[],"labels":[],"updated_at":"2026-01-01T00:00:00Z","closed_at":null})
}

fn pull_json(value: u64, state: &str) -> serde_json::Value {
    serde_json::json!({"number":value,"state":state,"draft":false,"merged":state == "closed","head":{"sha":"a".repeat(40),"ref":"topic","repo":{"full_name":"origin89hq/km43"}},"base":{"sha":"b".repeat(40),"ref":"main"},"mergeable":null,"user":{"login":"author"}})
}

fn forge_client<T: GitHubReadTransport>(forge: T) -> TestResult<GitHubClient<T>> {
    let requester = ExternalRef::new("kitchen-bot")?;
    let scope = HouseScope::new(
        common::house()?,
        [repo()?],
        requester.clone(),
        CredentialRef::new(common::house()?, common::credential()?, requester),
        PostingBudget::new(0)?,
        std::iter::empty::<Permission>(),
    )?;
    Ok(GitHubClient::new(scope, forge, ReadLimits::default()))
}

#[test]
fn only_complete_forge_answers_let_markers_retire() -> TestResult {
    let fixture = Fixture::new()?;
    let items = [
        issue(1)?,
        issue(2)?,
        issue(3)?,
        pull_request(4)?,
        pull_request(5)?,
    ];
    for item in &items {
        record(
            &fixture,
            &key("triage", item.clone(), 'a')?,
            fact("triage.resolution/2")?,
            1,
        )?;
    }
    let client = forge_client(Forge(vec![
        ("/issues/1", Some(issue_json(1, "closed"))),
        ("/issues/2", Some(issue_json(2, "open"))),
        // Issue 3 is unavailable.
        ("/pulls/4", Some(pull_json(4, "closed"))),
        ("/pulls/5", Some(pull_json(5, "archived"))),
    ]))?;
    let subjects = fixture.store.retention_subjects()?;
    assert_eq!(subjects.items, items.iter().cloned().collect());

    // The lookup bound is honored: only issues 1 and 2 are read.
    let mut bounded = Inventory::new();
    assert_eq!(
        bounded.observe_forge(&client, &common::house()?, &subjects, 2),
        2
    );
    let report = fixture
        .store
        .preview_retention(&RetentionPolicy::default(), &bounded, at(2))?;
    assert_eq!(report.markers.len(), 1);
    assert_eq!(report.markers[0].key.item, issue(1)?);

    let mut inventory = Inventory::new();
    // Open, closed, and merged answers count; the unavailable issue and the
    // unrecognized pull-request state do not.
    assert_eq!(
        inventory.observe_forge(&client, &common::house()?, &subjects, 100),
        3
    );
    let report =
        fixture
            .store
            .retain(&RetentionPolicy::default(), &inventory, &creator()?, at(2))?;
    let retired: Vec<WorkItem> = report
        .markers
        .into_iter()
        .map(|marker| marker.key.item)
        .collect();
    assert_eq!(retired, vec![issue(1)?, pull_request(4)?]);
    Ok(())
}

/// A [`Forge`] that also records each endpoint's last two segments, such as
/// `issues/1`, in the order they were read.
struct RecordingForge {
    forge: Forge,
    reads: Arc<Mutex<Vec<String>>>,
}

impl GitHubReadTransport for RecordingForge {
    fn read(
        &self,
        credential: &CredentialRef,
        request: &ReadRequest,
        timeout: Duration,
        limit: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let mut segments = request.endpoint().rsplit('/');
        let number = segments.next().unwrap_or_default();
        let kind = segments.next().unwrap_or_default();
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{kind}/{number}"));
        self.forge.read(credential, request, timeout, limit)
    }
}

fn take_reads(reads: &Mutex<Vec<String>>) -> Vec<String> {
    std::mem::take(
        &mut *reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[test]
fn bounded_passes_take_turns_until_a_closed_pull_request_retires() -> TestResult {
    let fixture = Fixture::new()?;
    // Five open issues that stay subjects, and one closed pull request that
    // sorts after them.
    for value in 1..=5 {
        record(
            &fixture,
            &key("triage", issue(value)?, 'a')?,
            fact("triage.resolution/2")?,
            1,
        )?;
    }
    let verdict = key("merge-gate", pull_request(6)?, 'a')?;
    record(&fixture, &verdict, fact("gate.verdict/1")?, 1)?;
    let reads = Arc::new(Mutex::new(Vec::new()));
    let client = forge_client(RecordingForge {
        forge: Forge(vec![
            ("/issues/1", Some(issue_json(1, "open"))),
            ("/issues/2", Some(issue_json(2, "open"))),
            // Issue 3 is unavailable; the cursor still moves past it.
            ("/issues/4", Some(issue_json(4, "open"))),
            ("/issues/5", Some(issue_json(5, "open"))),
            ("/pulls/6", Some(pull_json(6, "closed"))),
        ]),
        reads: Arc::clone(&reads),
    })?;
    let policy = RetentionPolicy::default();
    let limit = 2;

    // A preview looks up the same first items and leaves the cursor alone,
    // so previews cannot shift which items the applied passes cover.
    for _ in 0..2 {
        let subjects = fixture.store.retention_subjects()?;
        let mut inventory = Inventory::new();
        inventory.observe_forge(&client, &common::house()?, &subjects, limit);
        fixture
            .store
            .preview_retention(&policy, &inventory, at(2))?;
        assert_eq!(take_reads(&reads), ["issues/1", "issues/2"]);
        assert_eq!(fixture.store.retention_subjects()?.cursor, None);
    }

    // Six subjects at two lookups per pass: the pull request is reached,
    // and its verdict retired, by the third applied pass.
    let expected = [
        (["issues/1", "issues/2"], true),
        (["issues/3", "issues/4"], true),
        (["issues/5", "pulls/6"], false),
    ];
    for (pass, (looked_up, verdict_stays)) in expected.into_iter().enumerate() {
        let subjects = fixture.store.retention_subjects()?;
        let mut inventory = Inventory::new();
        inventory.observe_forge(&client, &common::house()?, &subjects, limit);
        fixture
            .store
            .retain(&policy, &inventory, &creator()?, at(2))?;
        assert_eq!(take_reads(&reads), looked_up, "pass {pass}");
        assert_eq!(
            fixture.store.marker(&verdict)?.is_some(),
            verdict_stays,
            "pass {pass}"
        );
    }

    // The cursor names the retired pull request, no longer a subject; the
    // next pass wraps around to the first issue.
    let subjects = fixture.store.retention_subjects()?;
    assert_eq!(subjects.cursor, Some(pull_request(6)?));
    assert!(!subjects.items.contains(&pull_request(6)?));
    let mut inventory = Inventory::new();
    inventory.observe_forge(&client, &common::house()?, &subjects, limit);
    fixture
        .store
        .retain(&policy, &inventory, &creator()?, at(2))?;
    assert_eq!(take_reads(&reads), ["issues/1", "issues/2"]);
    assert_eq!(fixture.store.retention_subjects()?.cursor, Some(issue(2)?));

    // A pass without lookups keeps the cursor.
    fixture
        .store
        .retain(&policy, &Inventory::new(), &creator()?, at(2))?;
    assert_eq!(fixture.store.retention_subjects()?.cursor, Some(issue(2)?));
    Ok(())
}

#[test]
fn lookup_order_resumes_after_the_cursor_and_wraps() -> TestResult {
    let mut subjects = RetentionSubjects {
        items: [issue(1)?, issue(3)?, pull_request(4)?].into(),
        ..RetentionSubjects::default()
    };
    let order = |subjects: &RetentionSubjects| -> Vec<WorkItem> {
        subjects.lookup_order().cloned().collect()
    };
    // No cursor yet: from the start.
    assert_eq!(order(&subjects), [issue(1)?, issue(3)?, pull_request(4)?]);
    // A present cursor comes last.
    subjects.cursor = Some(issue(3)?);
    assert_eq!(order(&subjects), [pull_request(4)?, issue(1)?, issue(3)?]);
    // A vanished cursor resumes at the next item that remains.
    subjects.cursor = Some(issue(2)?);
    assert_eq!(order(&subjects), [issue(3)?, pull_request(4)?, issue(1)?]);
    // A cursor past every item wraps to the first.
    subjects.cursor = Some(pull_request(9)?);
    assert_eq!(order(&subjects), [issue(1)?, issue(3)?, pull_request(4)?]);
    // No items: nothing to look up.
    subjects.items.clear();
    assert!(order(&subjects).is_empty());
    Ok(())
}

#[test]
fn held_follow_ups_retire_once_their_task_settled_or_is_gone() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let held = |item: WorkItem, subject: &str| -> TestResult<MarkerKey> {
        Ok(MarkerKey {
            workflow: WorkflowId::new("held-follow-up")?,
            item,
            subject: MarkerSubject::Observation(ExternalRef::new(subject)?),
        })
    };
    let task = |id: &str| -> TestResult<WorkItem> {
        Ok(WorkItem::Task {
            task: common::task_id(id)?,
        })
    };
    let live = common::task_id("follow-up-live")?;
    store.create_task(common::spec(live.as_str())?, &creator()?, at(0))?;
    store.claim(&live, &scheduled("worker")?, ttl(3600)?, at(0))?;
    let settled = common::task_id("follow-up-settled")?;
    settled_task(&fixture, &settled, None, AttemptOutcome::Succeeded, 5)?;

    let on_live = held(task("follow-up-live")?, "a")?;
    let on_settled = held(task("follow-up-settled")?, "a")?;
    let on_missing = held(task("follow-up-retired")?, "a")?;
    // The rule is about tasks: the same schema on an issue is kept.
    let on_issue = held(issue(7)?, "a")?;
    for key in [&on_live, &on_settled, &on_missing, &on_issue] {
        record(&fixture, key, fact("coordination.held-follow-up/1")?, 1)?;
    }

    let report = store.retain(
        &RetentionPolicy::default(),
        &Inventory::new(),
        &creator()?,
        at(6),
    )?;
    let mut retired: Vec<(MarkerKey, MarkerRetirement)> = report
        .markers
        .into_iter()
        .map(|retired| (retired.key, retired.reason))
        .collect();
    retired.sort_by(|left, right| left.0.cmp(&right.0));
    let mut expected = vec![
        (on_settled.clone(), MarkerRetirement::TaskSettled),
        (on_missing.clone(), MarkerRetirement::TaskSettled),
    ];
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(retired, expected);
    assert!(store.marker(&on_live)?.is_some());
    assert!(store.marker(&on_issue)?.is_some());
    assert_eq!(store.marker(&on_settled)?, None);
    Ok(())
}
