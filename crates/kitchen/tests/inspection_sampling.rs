//! Offline fixtures: no live usage, GitHub, inspector, or model is exercised.
//! "Live" below is the evidence mode the fixture declares, not a live run.
mod common;
use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, holder, house, other_house, scheduled,
    task_id, ttl,
};
use kitchen::{
    contracts::{
        AttemptNumber, AttemptOutcome, CommitId, Evidence, EvidenceKind, EvidenceSubject,
        EvidenceVerdict, ExternalRef, IssueNumber, Repository, Role, TaskSpec, Text, Timestamp,
    },
    house::MergeSubject,
    scheduling::{
        AgentFamily, BudgetAssessment, Exhausted, ScheduleLimit, TokenUsage, UsageWindow,
        WindowUsage,
    },
    selection::{AgentModel, AgentSelection, ResolvedSelection, WorkType},
    state::{MarkerRecording, StateError},
    trust::{
        Attribution, EvidenceMode, Finding, Ledger, Measurement, Observation, PullRequestEvidence,
        StationScope,
    },
    workflows::inspector::{FollowUpRoute, InspectionPlan, SampleResult},
    workflows::sampling::{
        Outcome, OwnerReport, Rate, RateRaise, RateSchedule, RecordedFinding, SamplingDecision,
        SamplingError, SamplingPolicy, ScopeRecord, select,
    },
};
use std::{
    collections::BTreeMap,
    num::{NonZeroU16, NonZeroU32},
};

const DAY: u64 = 86_400;

fn rate(per_mille: u16) -> TestResult<Rate> {
    Ok(Rate::new(per_mille)?)
}

/// Initial 500‰, floor 20‰, 800‰ after a finding; matures after 90 days
/// and 50 clean deliveries; findings stay recent for 30 days.
fn schedule() -> TestResult<RateSchedule> {
    Ok(RateSchedule {
        initial: rate(500)?,
        floor: rate(20)?,
        after_finding: rate(800)?,
        mature_after_days: NonZeroU16::new(90).ok_or("zero")?,
        mature_after_merges: NonZeroU32::new(50).ok_or("zero")?,
        finding_window_days: NonZeroU16::new(30).ok_or("zero")?,
    })
}

fn policy() -> TestResult<SamplingPolicy> {
    Ok(SamplingPolicy {
        revision: NonZeroU32::MIN,
        default: schedule()?,
        work_types: BTreeMap::new(),
    })
}

/// A policy whose every rate is `per_mille`.
fn flat(per_mille: u16) -> TestResult<SamplingPolicy> {
    let mut policy = policy()?;
    policy.default.initial = rate(per_mille)?;
    policy.default.floor = rate(per_mille)?;
    policy.default.after_finding = rate(per_mille)?;
    Ok(policy)
}

fn project() -> TestResult<Repository> {
    Ok(Repository::new("example/project")?)
}

fn scope_of(work_type: &str) -> TestResult<StationScope> {
    Ok(StationScope {
        station: Role::StationCook,
        project: project()?,
        work_type: WorkType::new(work_type)?,
    })
}

fn scope() -> TestResult<StationScope> {
    scope_of("implementation")
}

fn source(value: &str) -> TestResult<ExternalRef> {
    Ok(ExternalRef::new(value)?)
}

/// Pull request `number` merged at a head derived from it.
fn merge(number: u64) -> TestResult<MergeSubject> {
    Ok(MergeSubject {
        repository: project()?,
        number: IssueNumber::new(number)?,
        head: CommitId::new(&format!("{number:040x}"))?,
        base: commit('b')?,
    })
}

/// A record granted at day 0 with clean deliveries on the given days.
fn record(clean_days: impl IntoIterator<Item = u64>) -> TestResult<ScopeRecord> {
    Ok(ScopeRecord {
        scope: scope()?,
        granted_at: at(0),
        clean: clean_days.into_iter().map(|day| at(day * DAY)).collect(),
        findings: Vec::new(),
    })
}

fn day(n: u64) -> Timestamp {
    at(n * DAY)
}

fn budget(exhausted: Option<Exhausted>, complete: bool) -> BudgetAssessment {
    BudgetAssessment {
        window: UsageWindow {
            start: at(0),
            end: day(1000),
        },
        house: WindowUsage {
            runs: 3,
            tokens: TokenUsage::default(),
            complete,
        },
        house_exhausted: exhausted,
        schedules: Vec::new(),
    }
}

fn open_budget() -> BudgetAssessment {
    budget(None, true)
}

fn exhausted() -> Exhausted {
    Exhausted {
        limit: ScheduleLimit::HouseRuns,
        used: 40,
        allowed: 40,
    }
}

fn rate_at(record: &ScopeRecord, now: Timestamp) -> TestResult<u16> {
    let schedule = schedule()?;
    Ok(schedule.rate(&record.inputs(&schedule, now)?).per_mille())
}

/// How many of pull requests 1..=count `policy` picks for `record` at `now`.
fn picked(
    policy: &SamplingPolicy,
    record: &ScopeRecord,
    now: Timestamp,
    count: u64,
) -> TestResult<usize> {
    let mut picked = 0;
    for number in 1..=count {
        let decision = select(
            policy,
            &house()?,
            &merge(number)?,
            record,
            &open_budget(),
            now,
        )?;
        if decision.record.outcome == Outcome::Selected {
            picked += 1;
        }
    }
    Ok(picked)
}

#[test]
fn a_new_grant_samples_at_the_initial_rate() -> TestResult {
    let fresh = record([])?;
    assert_eq!(rate_at(&fresh, at(0))?, 500);
    let decision = select(
        &policy()?,
        &house()?,
        &merge(1)?,
        &fresh,
        &open_budget(),
        at(0),
    )?;
    assert_eq!(decision.record.rate, rate(500)?);
    assert_eq!(decision.record.inputs.clean_merges, 0);
    let expected = if decision.record.draw < 500 {
        Outcome::Selected
    } else {
        Outcome::Skipped
    };
    assert_eq!(decision.record.outcome, expected);
    // Draws spread over 0..1000, so about half of a new grant's merges are
    // picked; the fixed digests make this count exact across runs.
    let half = picked(&policy()?, &fresh, at(0), 1000)?;
    assert!((400..=600).contains(&half), "picked {half} of 1000");
    assert_eq!(picked(&flat(1000)?, &fresh, at(0), 50)?, 50);
    Ok(())
}

#[test]
fn a_long_clean_record_sits_at_the_floor_and_still_samples() -> TestResult {
    let mature = record(1..=60)?;
    assert_eq!(rate_at(&mature, day(120))?, 20);
    let picks = picked(&policy()?, &mature, day(120), 2000)?;
    assert!((1..=100).contains(&picks), "picked {picks} of 2000 at 2%");
    // Both maturities are required: an old grant with few clean deliveries
    // stays high, and so does a young grant with many.
    assert_eq!(rate_at(&record(1..=10)?, day(400))?, 404);
    let busy_young = ScopeRecord {
        clean: (0..60).map(|n| at(n * 60)).collect(),
        ..record([])?
    };
    assert_eq!(rate_at(&busy_young, day(45))?, 260);
    // Deliveries before the rate is judged count; later ones do not.
    assert_eq!(rate_at(&record(1..=60)?, day(10))?, 447);
    Ok(())
}

#[test]
fn a_confirmed_finding_raises_the_rate_and_restarts_the_clean_record() -> TestResult {
    let mut history = record(1..=60)?;
    history.findings.push(RecordedFinding {
        source: source("https://example.invalid/revert/1")?,
        at: day(100),
    });
    assert_eq!(rate_at(&history, day(99))?, 20);
    assert_eq!(rate_at(&history, day(110))?, 800);
    let raise = RateRaise::of(
        &policy()?,
        &history,
        &source("https://example.invalid/revert/1")?,
        day(110),
    )?;
    assert_eq!((raise.from, raise.to), (rate(20)?, rate(800)?));
    assert_eq!(raise.scope, scope()?);
    // After the window the rate falls from the initial rate again, not
    // straight back to the floor.
    assert_eq!(rate_at(&history, day(131))?, 500);
    history.clean.extend((101..=160).map(|n| at(n * DAY + 1)));
    assert_eq!(rate_at(&history, day(200))?, 20);

    let unknown = RateRaise::of(&policy()?, &history, &source("fixture:other")?, day(110));
    assert!(matches!(unknown, Err(SamplingError::UnknownFinding)));
    let early = RateRaise::of(
        &policy()?,
        &history,
        &source("https://example.invalid/revert/1")?,
        day(99),
    );
    assert!(matches!(early, Err(SamplingError::UnknownFinding)));

    let f = Fixture::new()?;
    let recorder = scheduled("sampling")?;
    assert!(matches!(
        raise.record(&f.store, &recorder, day(110))?,
        MarkerRecording::Recorded(_)
    ));
    assert!(matches!(
        raise.record(&f.store, &recorder, day(111))?,
        MarkerRecording::AlreadyRecorded(_)
    ));
    Ok(())
}

#[test]
fn budget_exhaustion_defers_a_pick_and_is_reported_without_lowering_the_rate() -> TestResult {
    let always = flat(1000)?;
    let fresh = record([])?;
    let deferred = select(
        &always,
        &house()?,
        &merge(1)?,
        &fresh,
        &budget(Some(exhausted()), true),
        at(0),
    )?;
    assert_eq!(
        deferred.record.outcome,
        Outcome::BudgetExhausted {
            exhausted: exhausted()
        }
    );
    assert_eq!(deferred.record.rate, rate(1000)?);
    match deferred.report() {
        Some(OwnerReport::BudgetExhausted {
            merge: left,
            rate: kept,
            exhausted: limit,
            ..
        }) => {
            assert_eq!(left, merge(1)?);
            assert_eq!(kept, rate(1000)?);
            assert_eq!(limit, exhausted());
        }
        other => return Err(format!("expected a budget report, got {other:?}").into()),
    }
    deferred.replay(&always)?;

    // The floor is the same with and without budget.
    let mature = record(1..=60)?;
    let (mut with_budget, mut without) = (Vec::new(), Vec::new());
    for number in 1..=200 {
        let merged = merge(number)?;
        let open = select(
            &policy()?,
            &house()?,
            &merged,
            &mature,
            &open_budget(),
            day(120),
        )?;
        let spent = select(
            &policy()?,
            &house()?,
            &merged,
            &mature,
            &budget(Some(exhausted()), true),
            day(120),
        )?;
        with_budget.push(open.record.rate);
        without.push(spent.record.rate);
        assert_eq!(
            open.record.outcome == Outcome::Selected,
            spent.report().is_some()
        );
    }
    assert_eq!(with_budget, without);
    let floor = rate(20)?;
    assert!(without.iter().all(|kept| *kept == floor));

    // Missing or stale budget evidence is not budget.
    assert!(matches!(
        select(
            &always,
            &house()?,
            &merge(1)?,
            &fresh,
            &budget(None, false),
            at(0)
        ),
        Err(SamplingError::IncompleteBudget)
    ));
    assert!(matches!(
        select(
            &always,
            &house()?,
            &merge(1)?,
            &fresh,
            &open_budget(),
            day(1000)
        ),
        Err(SamplingError::StaleBudget)
    ));
    // A skipped merge spends nothing, so exhaustion does not matter.
    let rare = flat(1)?;
    let skipped = (1..=50)
        .map(|number| {
            select(
                &rare,
                &house()?,
                &merge(number)?,
                &fresh,
                &budget(None, false),
                at(0),
            )
            .map_err(Into::into)
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert!(
        skipped
            .iter()
            .all(|d| d.record.outcome == Outcome::Skipped && d.report().is_none())
    );
    Ok(())
}

#[test]
fn selection_is_recorded_and_replays_from_its_inputs() -> TestResult {
    let f = Fixture::new()?;
    let recorder = scheduled("sampling")?;
    let mut history = record(1..=20)?;
    history.findings.push(RecordedFinding {
        source: source("fixture:finding")?,
        at: day(3),
    });
    let first = select(
        &policy()?,
        &house()?,
        &merge(7)?,
        &history,
        &open_budget(),
        day(40),
    )?;
    let again = select(
        &policy()?,
        &house()?,
        &merge(7)?,
        &history,
        &open_budget(),
        day(40),
    )?;
    assert_eq!(first, again);
    assert_eq!(first.record.inputs.clean_since, day(3));
    assert_eq!(first.record.inputs.clean_merges, 18);
    assert_eq!(first.record.inputs.recent_findings, 0);

    assert!(matches!(
        first.record(&f.store, &recorder, day(40))?,
        MarkerRecording::Recorded(_)
    ));
    assert!(matches!(
        first.record(&f.store, &recorder, day(41))?,
        MarkerRecording::AlreadyRecorded(_)
    ));
    let loaded = SamplingDecision::load(&f.store, &merge(7)?)?.ok_or("decision not recorded")?;
    assert_eq!(loaded, first);
    loaded.replay(&policy()?)?;
    assert_eq!(SamplingDecision::load(&f.store, &merge(8)?)?, None);

    // A later decision for the same merge cannot replace the recorded one.
    let later = select(
        &policy()?,
        &house()?,
        &merge(7)?,
        &history,
        &open_budget(),
        day(90),
    )?;
    assert_ne!(later.record, first.record);
    assert!(matches!(
        later.record(&f.store, &recorder, day(90)),
        Err(kitchen::Error::State(StateError::MarkerConflict))
    ));

    // A tampered or re-policied decision does not replay.
    let mut tampered = loaded.clone();
    tampered.record.draw = (tampered.record.draw + 1) % 1000;
    assert!(matches!(
        tampered.replay(&policy()?),
        Err(SamplingError::NotReproducible)
    ));
    let mut outcome = loaded.clone();
    outcome.record.outcome = match loaded.record.outcome {
        Outcome::Skipped => Outcome::Selected,
        _ => Outcome::Skipped,
    };
    assert!(matches!(
        outcome.replay(&policy()?),
        Err(SamplingError::NotReproducible)
    ));
    let mut revised = policy()?;
    revised.revision = NonZeroU32::new(2).ok_or("zero")?;
    assert!(matches!(
        loaded.replay(&revised),
        Err(SamplingError::NotReproducible)
    ));
    let mut backwards = loaded.clone();
    backwards.record.inputs.clean_since = day(50);
    assert!(matches!(
        backwards.replay(&policy()?),
        Err(SamplingError::InvalidInput)
    ));

    // Another house's decision is refused by this store.
    let mut foreign = first;
    foreign.house = other_house()?;
    assert!(matches!(
        foreign.record(&f.store, &recorder, day(40)),
        Err(kitchen::Error::Sampling(SamplingError::HouseMismatch))
    ));
    Ok(())
}

#[test]
fn policies_never_reach_zero_and_inputs_must_agree() -> TestResult {
    assert!(matches!(Rate::new(0), Err(SamplingError::InvalidPolicy)));
    assert!(matches!(Rate::new(1001), Err(SamplingError::InvalidPolicy)));
    assert_eq!(Rate::new(1)?.per_mille(), 1);
    let mut json = serde_json::to_value(policy()?)?;
    assert_eq!(
        serde_json::from_value::<SamplingPolicy>(json.clone())?,
        policy()?
    );
    json["default"]["floor"] = 0.into();
    assert!(serde_json::from_value::<SamplingPolicy>(json).is_err());

    let fresh = record([])?;
    let mut inverted = policy()?;
    inverted.default.floor = rate(600)?;
    let mut ignores_findings = policy()?;
    ignores_findings.default.after_finding = rate(100)?;
    let mut endless = policy()?;
    endless.default.finding_window_days = NonZeroU16::new(4000).ok_or("zero")?;
    let mut crowded = policy()?;
    for n in 0..65 {
        crowded
            .work_types
            .insert(WorkType::new(&format!("type-{n}"))?, schedule()?);
    }
    for invalid in [inverted, ignores_findings, endless, crowded] {
        assert!(matches!(
            select(
                &invalid,
                &house()?,
                &merge(1)?,
                &fresh,
                &open_budget(),
                at(0)
            ),
            Err(SamplingError::InvalidPolicy)
        ));
    }

    // A work type's own schedule overrides the default.
    let mut per_type = policy()?;
    per_type
        .work_types
        .insert(WorkType::new("implementation")?, flat(1000)?.default);
    let decision = select(
        &per_type,
        &house()?,
        &merge(1)?,
        &fresh,
        &open_budget(),
        at(0),
    )?;
    assert_eq!(decision.record.rate, rate(1000)?);

    let elsewhere = ScopeRecord {
        scope: StationScope {
            project: Repository::new("example/other")?,
            ..scope()?
        },
        ..record([])?
    };
    assert!(matches!(
        select(
            &policy()?,
            &house()?,
            &merge(1)?,
            &elsewhere,
            &open_budget(),
            at(0)
        ),
        Err(SamplingError::InvalidInput)
    ));
    let future = ScopeRecord {
        granted_at: day(5),
        ..record([])?
    };
    assert!(matches!(
        select(
            &policy()?,
            &house()?,
            &merge(1)?,
            &future,
            &open_budget(),
            day(4)
        ),
        Err(SamplingError::InvalidInput)
    ));
    Ok(())
}

// Trust-ledger fixtures, as in `trust_ledger.rs`.
const MODEL: &str = "fixture-model-v1";
const ATTRIBUTED_MODEL: &str = "claude:fixture-model-v1";

fn measured<T>(value: T) -> TestResult<Measurement<T>> {
    Ok(Measurement::Observed {
        value,
        samples: NonZeroU32::MIN,
        source: source("fixture:source")?,
    })
}

fn pr_subject() -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit('a')?,
        base: Some(commit('b')?),
    })
}

/// Settle `name` under `work_type`, bind it, and record its observation in
/// `ledger` with the given mode and reverts.
fn deliver(
    f: &Fixture,
    ledger: &Ledger,
    name: &str,
    work_type: &str,
    mode: EvidenceMode,
    reverts: Vec<Finding>,
) -> TestResult {
    let mut task: TaskSpec = common::spec(name)?;
    task.agent = Some(ResolvedSelection::owner(AgentSelection {
        agent: AgentFamily::Claude,
        model: Some(AgentModel::new(MODEL)?),
        effort: None,
    }));
    task.work_type = Some(WorkType::new(work_type)?);
    task.repository = Some(project()?);
    let id = task_id(name)?;
    f.store.create_task(task, &creator()?, at(0))?;
    let lease = f.store.claim(&id, &scheduled("owner")?, ttl(60)?, at(1))?;
    f.store.start_attempt(&id, lease.fence(), at(2))?;
    f.store.finish_attempt(
        &id,
        lease.fence(),
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(3),
    )?;
    ledger.bind_task(
        f.store.task(&id)?.spec(),
        source(&format!("fixture:bind:{name}"))?,
    )?;
    let attribution = Attribution {
        scope: scope_of(work_type)?,
        agent: measured(holder("worker")?)?,
        model: measured(Text::new(ATTRIBUTED_MODEL)?)?,
        tokens: measured(120)?,
    };
    let mut observation = Observation::collect(
        &f.store,
        &id,
        source(&format!("fixture:stream:{name}"))?,
        attribution,
        mode,
        at(4),
    )?;
    observation.pull_request = measured(PullRequestEvidence {
        house: house()?,
        task: id,
        repository: project()?,
        source: source(&format!("https://example.invalid/pr/{name}"))?,
        subject: pr_subject()?,
        first_pass: measured(true)?,
        findings: measured(Vec::new())?,
        reverts: measured(reverts)?,
        regressions: measured(Vec::new())?,
        checks: measured(vec![Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: pr_subject()?,
            source: source("fixture:passing-check")?,
            observed_at: at(4),
        }])?,
    })?;
    ledger.record(&f.store, observation)?;
    Ok(())
}

fn revert(name: &str) -> TestResult<Finding> {
    Ok(Finding {
        source: source(name)?,
        subject: pr_subject()?,
        consequence: Text::new("Reverted after merge.")?,
    })
}

#[test]
fn the_ledger_record_counts_live_evidence_per_station_and_work_type() -> TestResult {
    let f = Fixture::new()?;
    let ledger = Ledger::initialize(f.dir.path().join("trust"), house()?)?;
    deliver(
        &f,
        &ledger,
        "clean",
        "implementation",
        EvidenceMode::Live,
        Vec::new(),
    )?;
    deliver(
        &f,
        &ledger,
        "simulated",
        "implementation",
        EvidenceMode::Simulated,
        Vec::new(),
    )?;
    deliver(
        &f,
        &ledger,
        "reverted",
        "implementation",
        EvidenceMode::Live,
        vec![revert("https://example.invalid/revert/impl")?],
    )?;
    deliver(
        &f,
        &ledger,
        "simulated-revert",
        "implementation",
        EvidenceMode::Simulated,
        vec![revert("https://example.invalid/revert/sim")?],
    )?;
    deliver(
        &f,
        &ledger,
        "firmware",
        "firmware",
        EvidenceMode::Live,
        vec![revert("https://example.invalid/revert/fw")?],
    )?;

    let implementation = ScopeRecord::from_ledger(&ledger, &scope()?, at(0))?;
    assert_eq!(implementation.clean, vec![at(4)]);
    assert_eq!(
        implementation.findings,
        vec![RecordedFinding {
            source: source("https://example.invalid/revert/impl")?,
            at: at(4),
        }]
    );
    // The revert raises this scope's rate and is reportable.
    let raise = RateRaise::of(
        &policy()?,
        &implementation,
        &source("https://example.invalid/revert/impl")?,
        at(10),
    )?;
    assert_eq!((raise.from, raise.to), (rate(500)?, rate(800)?));

    // Another work type keeps its own record.
    let firmware = ScopeRecord::from_ledger(&ledger, &scope_of("firmware")?, at(0))?;
    assert!(firmware.clean.is_empty());
    assert_eq!(firmware.findings.len(), 1);
    let idle = ScopeRecord::from_ledger(&ledger, &scope_of("docs")?, at(0))?;
    assert!(idle.clean.is_empty() && idle.findings.is_empty());

    // Deliveries before the grant are not part of its clean record.
    let later = ScopeRecord::from_ledger(&ledger, &scope()?, at(5))?;
    assert!(later.clean.is_empty());
    Ok(())
}

#[test]
fn a_confirmed_inspection_sample_is_a_finding_for_the_delivering_scope() -> TestResult {
    let f = Fixture::new()?;
    let ledger = Ledger::initialize(f.dir.path().join("trust"), house()?)?;
    deliver(
        &f,
        &ledger,
        "clean",
        "implementation",
        EvidenceMode::Live,
        Vec::new(),
    )?;
    let mut inspector = common::spec("inspector")?;
    inspector.role = Role::Inspector;
    inspector.repository = Some(project()?);
    f.store.create_task(inspector, &creator()?, at(0))?;
    let fence = f
        .store
        .claim(
            &task_id("inspector")?,
            &scheduled("reviewer")?,
            ttl(600)?,
            at(0),
        )?
        .fence();
    let plan = InspectionPlan {
        id: source("fixture:inspection")?,
        house: house()?,
        observation: source("fixture:stream:clean")?,
        question: Text::new("Does the merged parser reject duplicate keys?")?,
        inspector: holder("reviewer")?,
        task: task_id("inspector")?,
        independent: true,
        max_samples: 1,
        max_tokens: 100,
        deadline: at(60),
    };
    ledger.start_inspection(&f.store, plan.clone(), fence, &ManualClock::starting_at(5))?;
    ledger.reserve_sample(
        &f.store,
        &plan.id,
        fence,
        1,
        50,
        &ManualClock::starting_at(6),
    )?;
    // Before the result arrives the delivery is clean.
    assert_eq!(
        ScopeRecord::from_ledger(&ledger, &scope()?, at(0))?.findings,
        Vec::new()
    );
    ledger.finish_sample(
        &f.store,
        &plan.id,
        fence,
        1,
        SampleResult::Confirmed {
            finding: revert("fixture:inspection-finding")?,
            route: FollowUpRoute::Issue,
        },
        &ManualClock::starting_at(7),
    )?;

    let record = ScopeRecord::from_ledger(&ledger, &scope()?, at(0))?;
    assert_eq!(
        record.findings,
        vec![RecordedFinding {
            source: source("fixture:inspection-finding")?,
            at: at(6),
        }]
    );
    assert_eq!(rate_at(&record, at(10))?, 800);
    assert!(
        ScopeRecord::from_ledger(&ledger, &scope_of("firmware")?, at(0))?
            .findings
            .is_empty()
    );
    Ok(())
}
