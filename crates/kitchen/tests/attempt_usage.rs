//! Attempt usage records: backend-reported tokens, model, and cost, and
//! human time derived from recorded replies and interactive claims.
//! Simulated with temporary stores and fake backend descriptors; no live
//! backend reports usage here.

mod common;

use std::{fs, time::Duration};

use common::{Fixture, TestResult, at, creator, interactive, scheduled, ttl};
use kitchen::{
    Error, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, BackendDescriptor, Capability, CapabilitySet, ContractError,
        ExternalRef, FailureClass, Fence, IssueNumber, Repository, Role, Support,
    },
    scheduling::AgentFamily,
    selection::{AgentModel, WorkType},
    state::{
        AttemptUsage, Cost, CostBasis, HouseStore, HumanTime, Inventory,
        MAX_HUMAN_REPLIES_PER_ATTEMPT, MIN_TASK_WINDOW, RetentionPolicy, StateError, StoreOptions,
        TaskRetirement, TokenCounts, UsageError, UsageReport, UsdMicros,
    },
};

const DAY: u64 = 24 * 60 * 60;

fn repo() -> TestResult<Repository> {
    Ok(Repository::new("origin89hq/km43")?)
}

/// A backend declaring usage attribution with `support`.
fn reporting(support: Support) -> TestResult<BackendDescriptor> {
    let mut descriptor = common::descriptor_with([])?;
    descriptor.capabilities = CapabilitySet::default().with(Capability::UsageAttribution, support);
    Ok(descriptor)
}

fn full_report() -> TestResult<UsageReport> {
    Ok(UsageReport {
        source: ExternalRef::new("orca-run:42")?,
        agent: Some(AgentFamily::Claude),
        model: Some(AgentModel::new("claude-opus-5-5")?),
        tokens: TokenCounts {
            input: Some(1_200),
            output: Some(800),
            cache_read: Some(40_000),
            cache_write: Some(2_000),
        },
        cost: Some(Cost {
            amount: UsdMicros(310_000),
            basis: CostBasis::Reported,
        }),
    })
}

/// Create a repository task with a work type and claim it.
fn claimed(fixture: &Fixture, id: &str, when: u64) -> TestResult<(TaskId, Fence)> {
    let mut spec = common::spec(id)?;
    spec.repository = Some(repo()?);
    spec.work_type = Some(WorkType::implementation());
    let id = spec.id.clone();
    fixture.store.create_task(spec, &creator()?, at(when))?;
    let fence = fixture
        .store
        .claim(&id, &scheduled("worker")?, ttl(3600)?, at(when))?
        .fence();
    Ok((id, fence))
}

/// A task whose single attempt ran from `0` to `10` and succeeded.
fn settled(fixture: &Fixture, id: &str) -> TestResult<(TaskId, Fence)> {
    let (id, fence) = claimed(fixture, id, 0)?;
    fixture.store.start_attempt(&id, fence, at(0))?;
    fixture.store.finish_attempt(
        &id,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(10),
    )?;
    Ok((id, fence))
}

fn only_entry(store: &HouseStore) -> TestResult<kitchen::state::AttemptUsageEntry> {
    let mut entries = store.attempt_usage()?;
    if entries.len() != 1 {
        return Err(format!("expected one entry, found {}", entries.len()).into());
    }
    Ok(entries.remove(0))
}

#[test]
fn a_reported_attempt_is_linked_to_its_task_station_work_type_and_pull_request() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = settled(&fixture, "usage-known")?;
    let backend = reporting(Support::Supported)?;

    // The report arrives after the attempt settled the task.
    fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &backend,
        full_report()?,
        at(20),
    )?;
    fixture
        .store
        .link_pull_request(&id, fence, IssueNumber::new(194)?)?;

    let entry = only_entry(&fixture.reopen()?)?;
    assert_eq!(entry.task, id);
    assert_eq!(entry.attempt, AttemptNumber::FIRST);
    assert_eq!(entry.station, Role::StationCook);
    assert_eq!(entry.work_type, Some(WorkType::implementation()));
    assert_eq!(entry.repository, Some(repo()?));
    assert_eq!(entry.pull_request, Some(IssueNumber::new(194)?));
    assert!(entry.ended);
    assert_eq!(
        entry.usage,
        AttemptUsage::Reported {
            backend: backend.backend.clone(),
            report: full_report()?,
            at: at(20),
        }
    );
    let AttemptUsage::Reported { report, .. } = &entry.usage else {
        return Err("expected a report".into());
    };
    assert_eq!(report.tokens.total(), Some(44_000));

    // The same report again changes nothing; a different one conflicts.
    fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &backend,
        full_report()?,
        at(30),
    )?;
    let mut other = full_report()?;
    other.tokens.output = Some(801);
    let conflict = fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &backend,
        other,
        at(30),
    );
    assert!(matches!(
        conflict,
        Err(Error::Usage(UsageError::AlreadyReported(number))) if number == AttemptNumber::FIRST
    ));
    assert_eq!(only_entry(&fixture.store)?.usage, entry.usage);
    Ok(())
}

#[test]
fn an_attempt_without_a_report_settles_as_explicitly_not_reported() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = settled(&fixture, "usage-unknown")?;

    let entry = only_entry(&fixture.reopen()?)?;
    assert!(entry.ended);
    assert_eq!(entry.usage, AttemptUsage::NotReported);

    // A backend that does not declare usage attribution cannot report.
    let silent = common::descriptor_with([Capability::WorkerStatusAndOutcome])?;
    let refused = fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &silent,
        full_report()?,
        at(20),
    );
    assert!(matches!(
        refused,
        Err(Error::Contract(ContractError::UnsupportedCapabilities { ref missing, .. }))
            if missing == &[Capability::UsageAttribution]
    ));

    // A report that knows nothing is not a zero-cost report.
    let empty = UsageReport {
        source: ExternalRef::new("orca-run:43")?,
        agent: None,
        model: None,
        tokens: TokenCounts::default(),
        cost: None,
    };
    let refused = fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &reporting(Support::Supported)?,
        empty,
        at(20),
    );
    assert!(matches!(
        refused,
        Err(Error::Usage(UsageError::EmptyReport))
    ));
    assert_eq!(only_entry(&fixture.store)?.usage, AttemptUsage::NotReported);
    Ok(())
}

#[test]
fn a_partial_report_keeps_what_the_backend_did_not_give_unknown() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = settled(&fixture, "usage-partial")?;
    // Orca declares usage attribution as partial: it reports total tokens
    // for automation runs and no split, model, or cost.
    let partial = UsageReport {
        source: ExternalRef::new("orca-run:44")?,
        agent: None,
        model: None,
        tokens: TokenCounts {
            output: Some(512),
            ..TokenCounts::default()
        },
        cost: None,
    };
    fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &reporting(Support::Partial)?,
        partial.clone(),
        at(20),
    )?;

    let entry = only_entry(&fixture.reopen()?)?;
    let AttemptUsage::Reported { report, .. } = entry.usage else {
        return Err("expected a report".into());
    };
    assert_eq!(report, partial);
    assert_eq!(report.tokens.input, None);
    assert_eq!(report.cost, None);
    // An unknown kind makes the total unknown, not the sum of the rest.
    assert_eq!(report.tokens.total(), None);
    Ok(())
}

#[test]
fn usage_refuses_another_house_a_stale_owner_and_a_missing_attempt() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = claimed(&fixture, "usage-refused", 0)?;
    fixture.store.start_attempt(&id, fence, at(0))?;

    let mut foreign = reporting(Support::Supported)?;
    foreign.house = common::other_house()?;
    let refused = fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &foreign,
        full_report()?,
        at(1),
    );
    assert!(matches!(
        refused,
        Err(Error::Contract(ContractError::CrossHouse { .. }))
    ));

    // After the claim changes hands, the old owner's fence is stale.
    let backend = reporting(Support::Supported)?;
    fixture.store.relinquish(&id, fence, at(1))?;
    let current = fixture
        .store
        .claim(&id, &scheduled("next")?, ttl(3600)?, at(2))?
        .fence();
    let refused = fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &backend,
        full_report()?,
        at(3),
    );
    assert!(matches!(
        refused,
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    let fence = current;

    let second = AttemptNumber::new(2).ok_or("attempt")?;
    let refused =
        fixture
            .store
            .record_attempt_usage(&id, fence, second, &backend, full_report()?, at(1));
    assert!(matches!(
        refused,
        Err(Error::State(StateError::AttemptNotFound(number))) if number == second
    ));

    // Another house's store holds nothing of this one's usage.
    let other = HouseStore::initialize(
        fixture.dir.path().join("other"),
        common::other_house()?,
        StoreOptions::default(),
    )?;
    assert!(other.attempt_usage()?.is_empty());
    Ok(())
}

#[test]
fn human_time_comes_from_recorded_replies_and_interactive_sessions() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let (id, scheduled_fence) = claimed(&fixture, "usage-human", 0)?;
    store.start_attempt(&id, scheduled_fence, at(0))?;
    // A person answered a question under the unattended claim after 300s.
    let first = ExternalRef::new("question-1")?;
    store.record_human_reply(&id, scheduled_fence, &first, at(100), at(400))?;
    // Recording it again changes nothing.
    store.record_human_reply(&id, scheduled_fence, &first, at(100), at(900))?;
    store.relinquish(&id, scheduled_fence, at(1000))?;

    // A person adopts the task to review and rework it for 600s, and
    // answers a question inside that session.
    let session = store
        .claim(&id, &interactive("david")?, ttl(3600)?, at(1600))?
        .fence();
    store.continue_attempt(&id, session, at(1600))?;
    let second = ExternalRef::new("question-2")?;
    store.record_human_reply(&id, session, &second, at(1700), at(1800))?;
    store.finish_attempt(
        &id,
        session,
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(2200),
    )?;

    let entry = only_entry(&fixture.reopen()?)?;
    assert_eq!(
        entry.human,
        HumanTime {
            replies: Duration::from_secs(300),
            sessions: Duration::from_secs(600),
            complete: true,
        }
    );
    assert_eq!(entry.human.total(), Duration::from_secs(900));
    Ok(())
}

#[test]
fn a_reply_needs_a_running_attempt_and_an_earlier_question() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = claimed(&fixture, "usage-reply", 0)?;
    let question = ExternalRef::new("question-1")?;

    let refused = fixture
        .store
        .record_human_reply(&id, fence, &question, at(5), at(10));
    assert!(matches!(
        refused,
        Err(Error::State(StateError::NoRunningAttempt))
    ));

    fixture.store.start_attempt(&id, fence, at(0))?;
    let refused = fixture
        .store
        .record_human_reply(&id, fence, &question, at(20), at(10));
    assert!(matches!(
        refused,
        Err(Error::Usage(UsageError::ReplyBeforeQuestion))
    ));

    // The per-attempt bound holds; the next distinct question is refused.
    for index in 0..MAX_HUMAN_REPLIES_PER_ATTEMPT {
        let question = ExternalRef::new(&format!("question-{index}"))?;
        fixture
            .store
            .record_human_reply(&id, fence, &question, at(1), at(2))?;
    }
    let refused = fixture.store.record_human_reply(
        &id,
        fence,
        &ExternalRef::new("question-last")?,
        at(1),
        at(2),
    );
    assert!(matches!(
        refused,
        Err(Error::Usage(UsageError::TooManyReplies))
    ));
    Ok(())
}

#[test]
fn an_open_session_makes_human_time_a_lower_bound() -> TestResult {
    let fixture = Fixture::new()?;
    let mut spec = common::spec("usage-open")?;
    spec.repository = Some(repo()?);
    let id = spec.id.clone();
    fixture.store.create_task(spec, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&id, &interactive("david")?, ttl(3600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&id, fence, at(0))?;

    let entry = only_entry(&fixture.store)?;
    assert!(!entry.ended);
    assert_eq!(entry.usage, AttemptUsage::NotReported);
    assert_eq!(entry.human.sessions, Duration::ZERO);
    assert!(!entry.human.complete);
    Ok(())
}

#[test]
fn session_time_splits_at_the_next_attempt() -> TestResult {
    let fixture = Fixture::new()?;
    let mut spec = common::spec("usage-rounds")?;
    spec.repository = Some(repo()?);
    let id = spec.id.clone();
    fixture.store.create_task(spec, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&id, &interactive("david")?, ttl(3600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&id, fence, at(0))?;
    fixture.store.finish_attempt(
        &id,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Failed(FailureClass::Retryable),
        at(500),
    )?;
    // Review between the attempts counts toward the first one.
    fixture.store.start_attempt(&id, fence, at(800))?;
    fixture.store.finish_attempt(
        &id,
        fence,
        AttemptNumber::new(2).ok_or("attempt")?,
        AttemptOutcome::Succeeded,
        at(1000),
    )?;

    let sessions: Vec<Duration> = fixture
        .store
        .attempt_usage()?
        .into_iter()
        .map(|entry| entry.human.sessions)
        .collect();
    assert_eq!(
        sessions,
        [Duration::from_secs(800), Duration::from_secs(200)]
    );
    Ok(())
}

#[test]
fn a_pull_request_link_needs_a_repository_and_stays_fixed() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = settled(&fixture, "usage-pr")?;
    let first = IssueNumber::new(7)?;
    fixture.store.link_pull_request(&id, fence, first)?;
    fixture.store.link_pull_request(&id, fence, first)?;
    let refused = fixture
        .store
        .link_pull_request(&id, fence, IssueNumber::new(8)?);
    assert!(matches!(
        refused,
        Err(Error::Usage(UsageError::PullRequestConflict(linked))) if linked == first
    ));

    let house_level = common::spec("usage-house")?;
    let house_id = house_level.id.clone();
    fixture.store.create_task(house_level, &creator()?, at(0))?;
    let house_fence = fixture
        .store
        .claim(&house_id, &scheduled("worker")?, ttl(60)?, at(0))?
        .fence();
    let refused = fixture
        .store
        .link_pull_request(&house_id, house_fence, first);
    assert!(matches!(
        refused,
        Err(Error::Usage(UsageError::NoRepository))
    ));
    Ok(())
}

#[test]
fn usage_retires_with_its_task_after_the_retention_window() -> TestResult {
    let fixture = Fixture::new()?;
    // A schedule budget window task, the family retention retires by age.
    let id = TaskId::new("budget-1000")?;
    fixture
        .store
        .create_task(common::spec(id.as_str())?, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&id, &scheduled("tick")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&id, fence, at(0))?;
    fixture.store.finish_attempt(
        &id,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(1),
    )?;
    fixture.store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &reporting(Support::Supported)?,
        full_report()?,
        at(2),
    )?;
    let policy = RetentionPolicy::default();

    // Inside the window the record stays with its task.
    let early = fixture
        .store
        .retain(&policy, &Inventory::new(), &creator()?, at(DAY))?;
    assert!(early.tasks.is_empty());
    assert_eq!(fixture.store.attempt_usage()?.len(), 1);

    // After it, the preview names the usage the pass removes.
    let later = at(MIN_TASK_WINDOW.as_secs() + DAY);
    let preview = fixture
        .store
        .preview_retention(&policy, &Inventory::new(), later)?;
    assert_eq!(preview.tasks.len(), 1);
    assert_eq!(preview.tasks[0].reason, TaskRetirement::WindowEnded);
    assert_eq!(preview.tasks[0].reported_usage, 1);
    let report = fixture
        .store
        .retain(&policy, &Inventory::new(), &creator()?, later)?;
    assert_eq!(report.tasks, preview.tasks);
    assert!(fixture.store.attempt_usage()?.is_empty());
    Ok(())
}

#[test]
fn a_store_written_before_usage_records_reads_as_not_reported() -> TestResult {
    let fixture = Fixture::new()?;
    let (id, fence) = settled(&fixture, "usage-legacy")?;
    // An attempt without usage is written exactly as a Kitchen from before
    // usage records wrote it, so either version reads the other's store.
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.state_path())?)?;
    let task = &state["tasks"][id.as_str()];
    assert_eq!(task.get("pullRequest"), None);
    let attempt = task["attempts"][0].as_object().ok_or("attempt")?;
    let mut keys: Vec<&str> = attempt.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["fence", "number", "startedAt", "state"]);

    let store = fixture.reopen()?;
    let entry = only_entry(&store)?;
    assert_eq!(entry.usage, AttemptUsage::NotReported);
    assert_eq!(entry.pull_request, None);
    assert!(entry.human.complete);
    // A late report can still be recorded against the legacy attempt.
    store.record_attempt_usage(
        &id,
        fence,
        AttemptNumber::FIRST,
        &reporting(Support::Supported)?,
        full_report()?,
        at(20),
    )?;
    assert!(matches!(
        only_entry(&store)?.usage,
        AttemptUsage::Reported { .. }
    ));
    Ok(())
}
