//! Event-started work: the typed intake, durable deduplication, repository
//! binding, event-delivery capability, and the polling fallback's shared
//! consumer. Simulated with temporary stores, sanitized envelope fixtures in
//! `tests/fixtures/events/`, and the fake backend; no live delivery.

use crate::common;

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use common::{
    Fixture, ManualClock, TestResult, at, backend_id, commit, grants, holder, house, launch,
    other_house, plan, scheduled, spec, ttl,
};
use kitchen::{
    BackendId, ConsumerId, Error, ErrorClass, HouseId, TaskId, WorkflowId,
    contracts::{
        Authorization, BackendDescriptor, Capability, CapabilitySet, Claimant, Consent,
        ContractError, EventOrigin, EvidenceRevision, EvidenceSubject, ExternalRef, Fence,
        Repository, Support, TaskSpec, Timestamp, Trigger, fake::FakeBackend,
    },
    events::{
        Admission, EventError, EventIntake, EventRoute, ForgeEvent, ForgeEventKind,
        MAX_EVENT_BYTES, PolledWork, RevisionSource, RevisionState, WorkOrder,
    },
    house::HouseConfig,
    integrations::github::IntegrationError,
    state::{
        EffectState, HouseStore, IssueRevision, MarkerSubject, StateError, TaskState, WorkItem,
        run_effect,
    },
};

const ISSUE_LABELED: &[u8] = include_bytes!("../fixtures/events/issue_labeled.json");
const PULL_REQUEST_PUSHED: &[u8] = include_bytes!("../fixtures/events/pull_request_pushed.json");

const BOUND: &str = "lemarier/kitchen";
const UNBOUND: &str = "lemarier/elsewhere";

fn repository(name: &str) -> TestResult<Repository> {
    Ok(Repository::new(name)?)
}

fn source_id() -> TestResult<BackendId> {
    Ok(BackendId::new("github")?)
}

fn consumer_id() -> TestResult<ConsumerId> {
    Ok(ConsumerId::new("pickup-kitchen")?)
}

fn house_config() -> TestResult<HouseConfig> {
    Ok(HouseConfig {
        schema: 1,
        house: house()?,
        kitchen: commit('a')?,
        guidance: commit('b')?,
        repositories: BTreeSet::from([repository(BOUND)?]),
        posting_destinations: BTreeSet::new(),
        required_reviewers: BTreeSet::new(),
        required_checks: BTreeSet::new(),
        policy_limits: BTreeSet::new(),
        grants: BTreeSet::new(),
        agents: None,
        stack_tool: None,
        schedules: None,
        merge_readiness: std::collections::BTreeMap::new(),
        disk_pressure: None,
        follow_up: None,
        backend: None,
        graduation: std::collections::BTreeMap::new(),
        tick: None,
    })
}

fn source(capabilities: CapabilitySet) -> TestResult<BackendDescriptor> {
    Ok(BackendDescriptor {
        backend: source_id()?,
        house: house()?,
        capabilities,
        worker_selection: None,
    })
}

fn delivering() -> TestResult<BackendDescriptor> {
    source(CapabilitySet::supporting([Capability::EventDelivery]))
}

fn route() -> TestResult<EventRoute> {
    Ok(EventRoute {
        workflow: WorkflowId::new("pickup")?,
        consumer: consumer_id()?,
        kinds: BTreeSet::from([
            ForgeEventKind::IssueLabeled,
            ForgeEventKind::PullRequestPushed,
        ]),
    })
}

fn origin(house: HouseId, event: &str) -> TestResult<EventOrigin> {
    Ok(EventOrigin {
        house,
        source: source_id()?,
        event: ExternalRef::new(event)?,
    })
}

fn issue(repo: &str, number: u64) -> TestResult<WorkItem> {
    Ok(WorkItem::Issue {
        repository: repository(repo)?,
        number: NonZeroU64::new(number).ok_or("zero issue number")?,
    })
}

fn pull_request(number: u64) -> TestResult<WorkItem> {
    Ok(WorkItem::PullRequest {
        repository: repository(BOUND)?,
        number: NonZeroU64::new(number).ok_or("zero pull request number")?,
    })
}

fn issue_revision(updated_seconds: u64) -> MarkerSubject {
    MarkerSubject::Issue(IssueRevision {
        updated_at: at(updated_seconds),
        last_comment: None,
    })
}

fn head(fill: char) -> TestResult<MarkerSubject> {
    Ok(MarkerSubject::Git(EvidenceSubject {
        head: commit(fill)?,
        base: None,
    }))
}

/// An issue label event about revision `updated_seconds`.
fn labeled(event: &str, number: u64, updated_seconds: u64) -> TestResult<ForgeEvent> {
    Ok(ForgeEvent::new(
        origin(house()?, event)?,
        ForgeEventKind::IssueLabeled,
        issue(BOUND, number)?,
        issue_revision(updated_seconds),
        at(updated_seconds),
    )?)
}

/// A pull-request push event for head `fill`, delivered at `seconds`.
fn pushed(event: &str, fill: char, seconds: u64) -> TestResult<ForgeEvent> {
    Ok(ForgeEvent::new(
        origin(house()?, event)?,
        ForgeEventKind::PullRequestPushed,
        pull_request(60)?,
        head(fill)?,
        at(seconds),
    )?)
}

/// The receiver acting on `event` under the consumer lease `fence`.
fn receiver(event: &ForgeEvent, fence: Fence) -> TestResult<Claimant> {
    Ok(Claimant::event(holder("receiver")?, event.origin().clone()).under(consumer_id()?, fence))
}

/// Acquire the route's consumer lease for `claimant` at `seconds`.
fn acquire(store: &HouseStore, claimant: &Claimant, seconds: u64) -> TestResult<Fence> {
    Ok(store
        .acquire_consumer(&consumer_id()?, claimant, ttl(600)?, at(seconds))?
        .fence())
}

/// The event receiver's consumer fence.
fn receiver_fence(store: &HouseStore, seconds: u64) -> TestResult<Fence> {
    acquire(store, &scheduled("receiver")?, seconds)
}

/// A task for the order, as a workflow would plan it.
fn planned(order: &WorkOrder) -> kitchen::Result<TaskSpec> {
    let mut task = spec(order.task.as_str()).map_err(|_| Error::from(EventError::PlanMismatch))?;
    task.repository = Some(order.repository.clone());
    Ok(task)
}

fn admit(
    intake: &EventIntake<'_>,
    event: &ForgeEvent,
    claimant: &Claimant,
    seconds: u64,
) -> kitchen::Result<Admission> {
    intake.admit_event(event, claimant, planned, at(seconds))
}

fn task_of(admission: &Admission) -> TestResult<TaskId> {
    match admission {
        Admission::Admitted(task) | Admission::Duplicate(task) | Admission::Stale(task) => {
            Ok(task.clone())
        }
        Admission::Superseded | Admission::Ignored => Err("admission names no task".into()),
    }
}

/// A forge whose every offered revision is still current, for tests about
/// other admission rules.
struct Latest;

impl RevisionSource for Latest {
    fn revision_state(&self, _: &WorkItem, _: &MarkerSubject) -> kitchen::Result<RevisionState> {
        Ok(RevisionState::Current)
    }
}

/// A forge reporting one current revision per item; `None` fails the read.
#[derive(Default)]
struct Forge {
    current: Mutex<BTreeMap<WorkItem, MarkerSubject>>,
    reads: AtomicUsize,
}

impl Forge {
    fn moves_to(&self, event: &ForgeEvent) {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(event.item().clone(), event.subject().clone());
    }
}

impl RevisionSource for Forge {
    fn revision_state(
        &self,
        item: &WorkItem,
        subject: &MarkerSubject,
    ) -> kitchen::Result<RevisionState> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match current.get(item) {
            Some(current) if current == subject => Ok(RevisionState::Current),
            Some(_) => Ok(RevisionState::Superseded),
            None => Err(IntegrationError::Unavailable.into()),
        }
    }
}

/// No task and no marker exist.
fn assert_untouched(store: &HouseStore) -> TestResult {
    assert!(store.tasks()?.is_empty());
    assert!(store.markers(&route()?.workflow)?.is_empty());
    Ok(())
}

#[test]
fn fixtures_parse_into_validated_events() -> TestResult {
    let labeled = ForgeEvent::parse(ISSUE_LABELED)?;
    assert_eq!(
        labeled.origin(),
        &origin(house()?, "72d3162e-cc78-11e3-81ab-4c9367dc0958")?
    );
    assert_eq!(labeled.kind(), ForgeEventKind::IssueLabeled);
    assert_eq!(labeled.item(), &issue(BOUND, 41)?);
    assert_eq!(labeled.repository(), &repository(BOUND)?);
    assert_eq!(
        labeled.subject(),
        &MarkerSubject::Issue(IssueRevision {
            updated_at: Timestamp::from_unix_millis(1_759_060_800_000),
            last_comment: Some(ExternalRef::new("IC_kwDOA1")?),
        })
    );
    assert_eq!(
        labeled.occurred_at(),
        Timestamp::from_unix_millis(1_759_060_800_000)
    );

    let pushed = ForgeEvent::parse(PULL_REQUEST_PUSHED)?;
    assert_eq!(pushed.kind(), ForgeEventKind::PullRequestPushed);
    assert_eq!(pushed.item(), &pull_request(60)?);
    assert_eq!(
        pushed.subject(),
        &MarkerSubject::Git(EvidenceSubject {
            head: commit('1')?,
            base: Some(commit('2')?),
        })
    );

    // The envelope round-trips through its own serialization.
    let encoded = serde_json::to_vec(&pushed)?;
    assert_eq!(ForgeEvent::parse(&encoded)?, pushed);
    Ok(())
}

#[test]
fn malformed_and_inconsistent_envelopes_are_rejected() -> TestResult {
    let fixture: serde_json::Value = serde_json::from_slice(ISSUE_LABELED)?;
    let variant = |edit: &dyn Fn(&mut serde_json::Value)| -> TestResult<Vec<u8>> {
        let mut value = fixture.clone();
        edit(&mut value);
        Ok(serde_json::to_vec(&value)?)
    };
    let cases: [(Vec<u8>, EventError); 7] = [
        (b"{not json".to_vec(), EventError::Malformed),
        (
            variant(&|value| value["unexpected"] = serde_json::json!(true))?,
            EventError::Malformed,
        ),
        (
            variant(&|value| value["origin"]["event"] = serde_json::json!("has space"))?,
            EventError::Malformed,
        ),
        (
            variant(&|value| value["kind"] = serde_json::json!("pull-request-pushed"))?,
            EventError::KindMismatch,
        ),
        (
            variant(&|value| {
                value["subject"] = serde_json::json!({
                    "type": "git",
                    "revision": { "head": "1111111111111111111111111111111111111111" }
                });
            })?,
            EventError::SubjectMismatch,
        ),
        (
            variant(&|value| {
                value["subject"] = serde_json::json!({"type": "observation", "revision": "d"});
            })?,
            EventError::SubjectMismatch,
        ),
        (
            vec![b' '; MAX_EVENT_BYTES + 1],
            EventError::TooLarge {
                max: MAX_EVENT_BYTES,
            },
        ),
    ];
    for (encoded, expected) in cases {
        assert_eq!(ForgeEvent::parse(&encoded), Err(expected));
    }
    // Whitespace padding up to the bound is still accepted.
    let mut padded = ISSUE_LABELED.to_vec();
    padded.resize(MAX_EVENT_BYTES, b' ');
    assert!(ForgeEvent::parse(&padded).is_ok());
    Ok(())
}

#[test]
fn only_issues_and_pull_requests_start_event_work() -> TestResult {
    let resource = WorkItem::Resource {
        resource: kitchen::contracts::ResourceRef {
            backend: backend_id()?,
            kind: kitchen::contracts::ResourceKind::Worktree,
            handle: ExternalRef::new("wt-1")?,
        },
    };
    assert_eq!(
        PolledWork::new(resource, issue_revision(1), at(1)),
        Err(EventError::UnsupportedItem)
    );
    assert_eq!(
        PolledWork::new(pull_request(60)?, issue_revision(1), at(1)),
        Err(EventError::SubjectMismatch)
    );
    Ok(())
}

#[test]
fn intake_requires_full_event_delivery_on_the_house_source() -> TestResult {
    let fixture = Fixture::new()?;
    let config = house_config()?;

    let none = source(CapabilitySet::new())?;
    let refused = EventIntake::new(&fixture.store, &config, &none, &Latest, route()?);
    let Err(Error::Contract(ContractError::UnsupportedCapabilities { missing, partial })) = refused
    else {
        return Err("a backend without event delivery was accepted".into());
    };
    assert_eq!(missing, vec![Capability::EventDelivery]);
    assert!(partial.is_empty());

    let partial_support =
        source(CapabilitySet::new().with(Capability::EventDelivery, Support::Partial))?;
    let Err(Error::Contract(ContractError::UnsupportedCapabilities { missing, partial })) =
        EventIntake::new(&fixture.store, &config, &partial_support, &Latest, route()?)
    else {
        return Err("partial event delivery was accepted".into());
    };
    assert!(missing.is_empty());
    assert_eq!(partial, vec![Capability::EventDelivery]);

    let mut foreign = delivering()?;
    foreign.house = other_house()?;
    assert!(matches!(
        EventIntake::new(&fixture.store, &config, &foreign, &Latest, route()?),
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));

    let mut empty = route()?;
    empty.kinds.clear();
    assert!(matches!(
        EventIntake::new(&fixture.store, &config, &delivering()?, &Latest, empty),
        Err(Error::Event(EventError::EmptyRoute))
    ));

    let source = delivering()?;
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    assert_eq!(intake.route(), &route()?);
    Ok(())
}

#[test]
fn redelivered_event_produces_one_task() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let event = ForgeEvent::parse(ISSUE_LABELED)?;
    let claimant = receiver(&event, fence)?;

    let first = admit(&intake, &event, &claimant, 1)?;
    let Admission::Admitted(task) = &first else {
        return Err(format!("expected a new task, got {first:?}").into());
    };
    assert!(task.as_str().starts_with("work-"));
    assert_eq!(
        admit(&intake, &event, &claimant, 2)?,
        Admission::Duplicate(task.clone())
    );
    assert_eq!(
        admit(&intake, &event, &claimant, 3)?,
        Admission::Duplicate(task.clone())
    );

    let tasks = fixture.store.tasks()?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].spec().repository, Some(repository(BOUND)?));
    assert_eq!(tasks[0].created_by(), &claimant);
    assert_eq!(fixture.store.markers(&route()?.workflow)?.len(), 1);
    Ok(())
}

#[test]
fn late_event_for_a_superseded_head_starts_no_work() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source, forge) = (house_config()?, delivering()?, Forge::default());
    let intake = EventIntake::new(&fixture.store, &config, &source, &forge, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let older = pushed("delivery-1", 'c', 10)?;
    let newer = pushed("delivery-2", 'd', 20)?;

    // The newer head arrives first, then the older one, then redeliveries.
    // Neither order says which head is newer; the forge's current head does.
    forge.moves_to(&newer);
    let admitted = admit(&intake, &newer, &receiver(&newer, fence)?, 21)?;
    let newer_task = task_of(&admitted)?;
    assert_eq!(admitted, Admission::Admitted(newer_task.clone()));
    let markers = fixture.store.markers(&route()?.workflow)?.len();
    assert_eq!(
        admit(&intake, &older, &receiver(&older, fence)?, 22)?,
        Admission::Superseded
    );
    assert_eq!(
        admit(&intake, &newer, &receiver(&newer, fence)?, 23)?,
        Admission::Duplicate(newer_task.clone())
    );
    assert_eq!(
        admit(&intake, &older, &receiver(&older, fence)?, 24)?,
        Admission::Superseded
    );
    let tasks = fixture.store.tasks()?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].spec().id, newer_task);
    assert_eq!(fixture.store.markers(&route()?.workflow)?.len(), markers);

    // A poll that observed the older head before the push is refused too.
    let polled = PolledWork::new(pull_request(60)?, head('c')?, at(19))?;
    let poller = scheduled("poller")?.under(consumer_id()?, fence);
    assert_eq!(
        intake.admit_polled(&polled, &poller, planned, at(25))?,
        Admission::Superseded
    );
    assert_eq!(fixture.store.tasks()?.len(), 1);
    Ok(())
}

#[test]
fn revisions_admitted_while_current_are_each_work() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source, forge) = (house_config()?, delivering()?, Forge::default());
    let intake = EventIntake::new(&fixture.store, &config, &source, &forge, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;

    // Pushes and issue edits delivered while each is current start work of
    // their own, and a redelivery after the item moved on starts nothing.
    let mut tasks = BTreeSet::new();
    for event in [
        pushed("delivery-1", 'c', 10)?,
        pushed("delivery-2", 'd', 20)?,
        labeled("delivery-3", 7, 30)?,
        labeled("delivery-4", 7, 40)?,
    ] {
        forge.moves_to(&event);
        let admitted = admit(&intake, &event, &receiver(&event, fence)?, 41)?;
        assert_eq!(admitted, Admission::Admitted(task_of(&admitted)?));
        tasks.insert(task_of(&admitted)?);
    }
    assert_eq!(tasks.len(), 4);
    let first = labeled("delivery-3", 7, 30)?;
    assert_eq!(
        admit(&intake, &first, &receiver(&first, fence)?, 42)?,
        Admission::Superseded
    );
    assert_eq!(fixture.store.tasks()?.len(), 4);
    Ok(())
}

#[test]
fn unreadable_current_revision_writes_nothing_until_a_retry() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source, forge) = (house_config()?, delivering()?, Forge::default());
    let intake = EventIntake::new(&fixture.store, &config, &source, &forge, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let event = pushed("delivery-1", 'c', 10)?;

    assert!(matches!(
        admit(&intake, &event, &receiver(&event, fence)?, 11),
        Err(Error::Integration(IntegrationError::Unavailable))
    ));
    assert_untouched(&fixture.store)?;
    forge.moves_to(&event);
    let admitted = admit(&intake, &event, &receiver(&event, fence)?, 12)?;
    assert_eq!(admitted, Admission::Admitted(task_of(&admitted)?));

    // Ignored kinds and refused claimants never read the forge.
    let reads = forge.reads.load(Ordering::Relaxed);
    let stranger = Claimant::event(holder("receiver")?, origin(house()?, "other")?)
        .under(consumer_id()?, fence);
    assert!(admit(&intake, &event, &stranger, 13).is_err());
    assert_eq!(forge.reads.load(Ordering::Relaxed), reads);
    Ok(())
}

/// A skewed, future, or hostile event time cannot make later real events
/// stale or merge distinct revisions: only the store's receipt order counts.
#[test]
fn provider_event_times_never_order_admissions() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let future = ForgeEvent::new(
        origin(house()?, "delivery-future")?,
        ForgeEventKind::PullRequestPushed,
        pull_request(60)?,
        head('c')?,
        Timestamp::from_unix_millis(u64::MAX),
    )?;
    let future_task = task_of(&admit(&intake, &future, &receiver(&future, fence)?, 10)?)?;

    // Real pushes after it, one claiming the epoch, are each new work.
    let mut tasks = vec![future_task.clone()];
    for (event, fill, seconds) in [("delivery-2", 'd', 11), ("delivery-3", 'e', 0)] {
        let pushed = pushed(event, fill, seconds)?;
        let admitted = admit(&intake, &pushed, &receiver(&pushed, fence)?, 12)?;
        assert_eq!(admitted, Admission::Admitted(task_of(&admitted)?));
        tasks.push(task_of(&admitted)?);
    }
    assert_eq!(fixture.store.tasks()?.len(), 3);
    tasks.sort();
    tasks.dedup();
    assert_eq!(tasks.len(), 3);

    // A redelivery of the same revision with a different claimed time is the
    // same work, not a new or stale one.
    let rewound = ForgeEvent::new(
        origin(house()?, "delivery-future")?,
        ForgeEventKind::PullRequestPushed,
        pull_request(60)?,
        head('c')?,
        at(1),
    )?;
    assert_eq!(
        admit(&intake, &rewound, &receiver(&rewound, fence)?, 13)?,
        Admission::Duplicate(future_task)
    );
    assert_eq!(fixture.store.tasks()?.len(), 3);
    Ok(())
}

/// Racing deliveries of two revisions under the same live consumer fence
/// each admit their own work once; the store's transaction orders the two
/// markers. Each round is an independent race.
#[test]
fn racing_deliveries_admit_each_revision_once() -> TestResult {
    let (config, source) = (house_config()?, delivering()?);
    let older = pushed("delivery-1", 'c', 10)?;
    let newer = pushed("delivery-2", 'd', 20)?;
    let (older_head, newer_head) = (head('c')?, head('d')?);
    for round in 0..40 {
        let fixture = Fixture::new()?;
        let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
        let fence = receiver_fence(&fixture.store, 0)?;
        let (older_claim, newer_claim) = (receiver(&older, fence)?, receiver(&newer, fence)?);
        let start = std::sync::Barrier::new(2);
        let (from_older, from_newer) = std::thread::scope(|scope| {
            let old = scope.spawn(|| {
                start.wait();
                admit(&intake, &older, &older_claim, 21)
            });
            let new = scope.spawn(|| {
                start.wait();
                admit(&intake, &newer, &newer_claim, 22)
            });
            (old.join(), new.join())
        });
        let (from_older, from_newer) = (
            from_older.map_err(|_| "older delivery panicked")??,
            from_newer.map_err(|_| "newer delivery panicked")??,
        );
        for admitted in [&from_older, &from_newer] {
            assert!(
                matches!(admitted, Admission::Admitted(_)),
                "round {round}: {admitted:?}"
            );
        }
        let mut recorded: Vec<MarkerSubject> = fixture
            .store
            .markers(&route()?.workflow)?
            .iter()
            .map(|marker| marker.key().subject.clone())
            .collect();
        recorded.sort_by_key(|subject| subject == &newer_head);
        assert_eq!(
            recorded,
            [older_head.clone(), newer_head.clone()],
            "round {round}"
        );
        assert_eq!(fixture.store.tasks()?.len(), 2, "round {round}");
    }
    Ok(())
}

#[test]
fn restart_between_receipt_and_claim_resumes_the_same_task() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let event = ForgeEvent::parse(PULL_REQUEST_PUSHED)?;
    let task = {
        let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
        let fence = receiver_fence(&fixture.store, 0)?;
        task_of(&admit(&intake, &event, &receiver(&event, fence)?, 1)?)?
    };

    // A new process opens the store; the source redelivers because the first
    // receipt was never acknowledged. The consumer lease expired meanwhile,
    // so the old fence is refused until someone takes the scope over.
    let reopened = fixture.reopen()?;
    let intake = EventIntake::new(&reopened, &config, &source, &Latest, route()?)?;
    let expired = receiver(
        &event,
        reopened
            .consumer(&consumer_id()?)?
            .and_then(|record| record.lease().map(kitchen::state::Lease::fence))
            .ok_or("no consumer lease")?,
    )?;
    assert!(matches!(
        admit(&intake, &event, &expired, 700),
        Err(Error::State(StateError::LeaseExpired { .. }))
    ));
    let fence = reopened
        .take_over_consumer(
            &consumer_id()?,
            &scheduled("receiver-2")?,
            ttl(600)?,
            at(700),
        )?
        .fence();
    let claimant = receiver(&event, fence)?;
    assert_eq!(
        admit(&intake, &event, &claimant, 701)?,
        Admission::Duplicate(task.clone())
    );
    assert!(matches!(reopened.task(&task)?.state(), TaskState::Open));

    // Exactly one claimant gets the task, under the event trigger.
    let lease = reopened.claim(&task, &claimant, ttl(600)?, at(702))?;
    assert_eq!(lease.trigger(), &Trigger::Event(event.origin().clone()));
    assert!(matches!(
        reopened.claim(&task, &receiver(&event, fence)?, ttl(600)?, at(703)),
        Err(Error::State(StateError::ClaimHeld { .. }))
    ));
    assert_eq!(reopened.tasks()?.len(), 1);
    Ok(())
}

#[test]
fn interrupted_admission_is_finished_by_the_next_delivery() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let event = ForgeEvent::parse(ISSUE_LABELED)?;
    let claimant = receiver(&event, fence)?;

    // Planning fails after the work key was recorded, as a crash would.
    let failed = intake.admit_event(
        &event,
        &claimant,
        |_| Err(Error::from(StateError::MarkerPayloadInvalid)),
        at(1),
    );
    assert!(failed.is_err());
    assert!(fixture.store.tasks()?.is_empty());
    assert_eq!(fixture.store.markers(&route()?.workflow)?.len(), 1);

    let finished = admit(&intake, &event, &claimant, 2)?;
    let task = task_of(&finished)?;
    assert_eq!(finished, Admission::Admitted(task.clone()));
    assert_eq!(
        admit(&intake, &event, &claimant, 3)?,
        Admission::Duplicate(task)
    );
    assert_eq!(fixture.store.tasks()?.len(), 1);
    Ok(())
}

#[test]
fn recovery_of_an_older_event_is_stale_once_a_newer_event_was_admitted() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let older = pushed("delivery-1", 'c', 10)?;
    let newer = pushed("delivery-2", 'd', 20)?;

    // The older event records its marker, then planning fails.
    let failed = intake.admit_event(
        &older,
        &receiver(&older, fence)?,
        |_| Err(Error::from(StateError::MarkerPayloadInvalid)),
        at(11),
    );
    assert!(failed.is_err());
    assert!(fixture.store.tasks()?.is_empty());

    let newer_task = task_of(&admit(&intake, &newer, &receiver(&newer, fence)?, 21)?)?;

    // Redelivering the older event must not finish its admission.
    assert_eq!(
        admit(&intake, &older, &receiver(&older, fence)?, 22)?,
        Admission::Stale(newer_task)
    );
    assert_eq!(fixture.store.tasks()?.len(), 1);
    Ok(())
}

#[test]
fn a_poll_received_later_makes_an_interrupted_event_stale() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    // The event carries a later provider time than the poll; only receipt
    // order counts.
    let event = pushed("delivery-1", 'c', 50)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let failed = intake.admit_event(
        &event,
        &receiver(&event, fence)?,
        |_| Err(Error::from(StateError::MarkerPayloadInvalid)),
        at(1),
    );
    assert!(failed.is_err());
    assert!(fixture.store.tasks()?.is_empty());

    // The receiver stops; the fallback tick polls a newer head.
    fixture
        .store
        .release_consumer(&consumer_id()?, fence, at(2))?;
    let tick = scheduled("fallback-tick")?;
    let tick_fence = acquire(&fixture.store, &tick, 3)?;
    let polled = PolledWork::new(pull_request(60)?, head('d')?, at(4))?;
    let polled_task = task_of(&intake.admit_polled(
        &polled,
        &tick.clone().under(consumer_id()?, tick_fence),
        planned,
        at(4),
    )?)?;
    fixture
        .store
        .release_consumer(&consumer_id()?, tick_fence, at(5))?;

    // Redelivering the interrupted event does not finish its admission.
    let fence = receiver_fence(&fixture.store, 6)?;
    assert_eq!(
        admit(&intake, &event, &receiver(&event, fence)?, 7)?,
        Admission::Stale(polled_task)
    );
    assert_eq!(fixture.store.tasks()?.len(), 1);
    Ok(())
}

#[test]
fn foreign_events_are_refused_before_any_state_changes() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;

    // Delivered for another house.
    let cross_house = ForgeEvent::new(
        origin(other_house()?, "delivery-x")?,
        ForgeEventKind::IssueLabeled,
        issue(BOUND, 1)?,
        issue_revision(1),
        at(1),
    )?;
    let refused = admit(&intake, &cross_house, &receiver(&cross_house, fence)?, 1);
    let Err(error) = refused else {
        return Err("a cross-house event was admitted".into());
    };
    assert!(matches!(
        error,
        Error::Contract(ContractError::CrossHouse { .. })
    ));
    assert_eq!(error.class(), ErrorClass::Refused);

    // For a repository the house is not bound to.
    let unbound = ForgeEvent::new(
        origin(house()?, "delivery-y")?,
        ForgeEventKind::IssueLabeled,
        issue(UNBOUND, 1)?,
        issue_revision(1),
        at(1),
    )?;
    let Err(error) = admit(&intake, &unbound, &receiver(&unbound, fence)?, 1) else {
        return Err("an unbound repository was admitted".into());
    };
    assert!(matches!(
        &error,
        Error::Event(EventError::RepositoryNotBound(repo)) if repo == &repository(UNBOUND)?
    ));
    assert_eq!(error.class(), ErrorClass::Refused);
    let polled = PolledWork::new(issue(UNBOUND, 1)?, issue_revision(1), at(1))?;
    assert!(matches!(
        intake.admit_polled(
            &polled,
            &scheduled("tick")?.under(consumer_id()?, fence),
            planned,
            at(1)
        ),
        Err(Error::Event(EventError::RepositoryNotBound(_)))
    ));

    // From another source namespace.
    let mut other_source = origin(house()?, "delivery-z")?;
    other_source.source = BackendId::new("gitlab")?;
    let spoofed = ForgeEvent::new(
        other_source,
        ForgeEventKind::IssueLabeled,
        issue(BOUND, 1)?,
        issue_revision(1),
        at(1),
    )?;
    assert!(matches!(
        admit(&intake, &spoofed, &receiver(&spoofed, fence)?, 1),
        Err(Error::Event(EventError::UnknownSource))
    ));
    assert_untouched(&fixture.store)
}

#[test]
fn claimants_must_act_under_the_event_trigger_and_route_consumer() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let event = labeled("delivery-1", 1, 1)?;
    let other_event = labeled("delivery-2", 1, 1)?;

    let wrong_triggers = [
        Claimant::interactive(holder("person")?).under(consumer_id()?, fence),
        scheduled("tick")?.under(consumer_id()?, fence),
        receiver(&other_event, fence)?,
    ];
    for claimant in wrong_triggers {
        assert!(matches!(
            admit(&intake, &event, &claimant, 2),
            Err(Error::Event(EventError::TriggerMismatch))
        ));
    }
    let without_consumer = Claimant::event(holder("receiver")?, event.origin().clone());
    let other_consumer = Claimant::event(holder("receiver")?, event.origin().clone())
        .under(ConsumerId::new("triage-kitchen")?, fence);
    for claimant in [without_consumer, other_consumer] {
        assert!(matches!(
            admit(&intake, &event, &claimant, 2),
            Err(Error::Event(EventError::ConsumerRequired))
        ));
    }
    let polled = PolledWork::new(issue(BOUND, 1)?, issue_revision(1), at(1))?;
    assert!(matches!(
        intake.admit_polled(&polled, &receiver(&event, fence)?, planned, at(2)),
        Err(Error::Event(EventError::TriggerMismatch))
    ));

    // A routed kind the workflow does not start from is ignored.
    let commented = ForgeEvent::new(
        origin(house()?, "delivery-3")?,
        ForgeEventKind::IssueCommented,
        issue(BOUND, 1)?,
        issue_revision(1),
        at(1),
    )?;
    assert_eq!(
        admit(&intake, &commented, &receiver(&commented, fence)?, 2)?,
        Admission::Ignored
    );
    assert_untouched(&fixture.store)
}

#[test]
fn plan_must_target_the_work_order() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let event = labeled("delivery-1", 1, 1)?;
    let claimant = receiver(&event, fence)?;

    let wrong_id = intake.admit_event(
        &event,
        &claimant,
        |order| {
            let mut task = planned(order)?;
            task.id = TaskId::new("chosen-by-caller")?;
            Ok(task)
        },
        at(2),
    );
    assert!(matches!(
        wrong_id,
        Err(Error::Event(EventError::PlanMismatch))
    ));
    let house_level = intake.admit_event(
        &event,
        &claimant,
        |order| {
            let mut task = planned(order)?;
            task.repository = None;
            Ok(task)
        },
        at(2),
    );
    assert!(matches!(
        house_level,
        Err(Error::Event(EventError::PlanMismatch))
    ));
    assert!(fixture.store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn event_and_fallback_schedule_share_work_and_one_consumer() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let event = pushed("delivery-1", 'e', 10)?;

    // The receiver holds the scope; the fallback tick cannot become a
    // second consumer while that lease is live.
    let first_fence = receiver_fence(&fixture.store, 0)?;
    let tick = scheduled("fallback-tick")?;
    assert!(matches!(
        fixture
            .store
            .acquire_consumer(&consumer_id()?, &tick, ttl(600)?, at(5)),
        Err(Error::State(StateError::ClaimHeld { .. }))
    ));
    let task = task_of(&admit(
        &intake,
        &event,
        &receiver(&event, first_fence)?,
        11,
    )?)?;

    // The receiver stops; the tick takes the scope and polls the same head.
    fixture
        .store
        .release_consumer(&consumer_id()?, first_fence, at(12))?;
    let tick_fence = acquire(&fixture.store, &tick, 13)?;
    let ticking = tick.clone().under(consumer_id()?, tick_fence);
    let same_head = PolledWork::new(pull_request(60)?, head('e')?, at(14))?;
    assert_eq!(
        intake.admit_polled(&same_head, &ticking, planned, at(14))?,
        Admission::Duplicate(task.clone())
    );

    // The superseded receiver can no longer admit anything, new or already
    // admitted.
    let late = pushed("delivery-2", 'f', 15)?;
    for offered in [&late, &event] {
        assert!(matches!(
            admit(&intake, offered, &receiver(offered, first_fence)?, 16),
            Err(Error::State(StateError::StaleFence { presented })) if presented == first_fence
        ));
    }
    assert_eq!(fixture.store.tasks()?.len(), 1);

    // The poll sees a new head first; the event for it arrives later.
    let polled = PolledWork::new(pull_request(60)?, head('f')?, at(17))?;
    let polled_task = task_of(&intake.admit_polled(&polled, &ticking, planned, at(17))?)?;
    fixture
        .store
        .release_consumer(&consumer_id()?, tick_fence, at(18))?;
    let fence = receiver_fence(&fixture.store, 19)?;
    assert_eq!(
        admit(&intake, &late, &receiver(&late, fence)?, 20)?,
        Admission::Duplicate(polled_task.clone())
    );
    assert_eq!(fixture.store.tasks()?.len(), 2);

    // A poll's observation time does not order events: an event that
    // occurred before the poll ran, about a revision the poll never saw, is
    // still new work.
    let missed = pushed("delivery-3", 'b', 16)?;
    let missed_task = task_of(&admit(&intake, &missed, &receiver(&missed, fence)?, 21)?)?;
    assert_ne!(missed_task, polled_task);
    assert_eq!(fixture.store.tasks()?.len(), 3);

    // Neither do event times: an event that claims to predate an admitted
    // one is new work for a revision the store has not received.
    let old = pushed("delivery-0", 'a', 1)?;
    let old_task = task_of(&admit(&intake, &old, &receiver(&old, fence)?, 22)?)?;
    assert_ne!(old_task, missed_task);
    assert_eq!(fixture.store.tasks()?.len(), 4);
    Ok(())
}

#[test]
fn event_work_uses_standing_grants_and_refuses_consent() -> TestResult {
    let fixture = Fixture::new()?;
    let (config, source) = (house_config()?, delivering()?);
    let intake = EventIntake::new(&fixture.store, &config, &source, &Latest, route()?)?;
    let fence = receiver_fence(&fixture.store, 0)?;
    let event = labeled("delivery-1", 1, 1)?;
    let claimant = receiver(&event, fence)?;
    let task = task_of(&admit(&intake, &event, &claimant, 1)?)?;
    let claim = fixture
        .store
        .claim(&task, &claimant, ttl(600)?, at(2))?
        .fence();
    fixture.store.start_attempt(&task, claim, at(2))?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(3);

    // A consent, even one naming exactly this effect, is not accepted.
    let mut consented = plan(&task, claim, "launch", launch()?)?;
    consented.consent = Some(Consent {
        id: ExternalRef::new("approval-1")?,
        given_by: holder("person")?,
        house: house()?,
        task: task.clone(),
        effect: launch()?.into(),
        revision: EvidenceRevision::INITIAL,
    });
    assert!(matches!(
        run_effect(&fixture.store, &backend, &grants()?, consented, &clock),
        Err(Error::Contract(ContractError::ConsentNotAccepted))
    ));
    assert_eq!(backend.effects_performed(), 0);

    let launched = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, claim, "launch", launch()?)?,
        &clock,
    )?;
    assert!(matches!(launched.state(), EffectState::Applied { .. }));
    assert_eq!(launched.authorization(), &Authorization::Standing);
    Ok(())
}

#[test]
fn event_trigger_serializes_beside_the_unit_triggers() -> TestResult {
    // Persisted scheduled and interactive triggers keep their form.
    assert_eq!(serde_json::to_string(&Trigger::Scheduled)?, "\"scheduled\"");
    assert_eq!(
        serde_json::from_str::<Trigger>("\"interactive\"")?,
        Trigger::Interactive
    );
    let event = Trigger::Event(origin(house()?, "delivery-1")?);
    let encoded = serde_json::to_value(&event)?;
    assert_eq!(
        encoded,
        serde_json::json!({
            "event": { "house": "origin89", "source": "github", "event": "delivery-1" }
        })
    );
    assert_eq!(serde_json::from_value::<Trigger>(encoded)?, event);
    assert!(
        serde_json::from_value::<Trigger>(serde_json::json!({
            "event": { "house": "origin89", "source": "github", "event": "d", "extra": 1 }
        }))
        .is_err()
    );
    assert!(event.is_unattended());
    assert!(Trigger::Scheduled.is_unattended());
    assert!(!Trigger::Interactive.is_unattended());
    assert_eq!(event.to_string(), "event origin89/github/delivery-1");
    Ok(())
}
