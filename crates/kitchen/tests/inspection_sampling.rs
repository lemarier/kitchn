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
    state::{
        HouseStore, Inventory, MAX_MARKERS, MarkerRecording, MarkerSchema, Presence,
        RetentionPolicy, StateError, StoreOptions, WorkItem,
    },
    trust::{
        Attribution, EvidenceMode, Finding, Ledger, Measurement, Observation, PullRequestEvidence,
        StationScope,
    },
    workflows::inspector::{FollowUpRoute, InspectionPlan, SampleResult},
    workflows::sampling::{
        GrantEpoch, MAX_DAYS, Outcome, OwnerReport, Rate, RateRaise, RateSchedule, RecordedFinding,
        SamplingDecision, SamplingError, SamplingPolicy, ScopeRecord, SelectionKey, compact,
        select,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
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
        audit_horizon_days: NonZeroU16::new(90).ok_or("zero")?,
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

/// A house store with the repository's selection key and the fixture
/// scope's grant epoch, first seen at time zero like [`record`]'s grant.
struct Sampler {
    f: Fixture,
    key: SelectionKey,
    epoch: GrantEpoch,
}

fn sampler() -> TestResult<Sampler> {
    let f = Fixture::new()?;
    let recorder = scheduled("sampling")?;
    let key = SelectionKey::establish(&f.store, &project()?, &recorder, at(0))?;
    let epoch = GrantEpoch::establish(&f.store, &scope()?, &recorder, at(0))?;
    Ok(Sampler { f, key, epoch })
}

impl Sampler {
    fn select(
        &self,
        policy: &SamplingPolicy,
        merged: &MergeSubject,
        record: &ScopeRecord,
        budget: &BudgetAssessment,
        now: Timestamp,
    ) -> Result<SamplingDecision, SamplingError> {
        select(policy, &self.key, merged, record, &self.epoch, budget, now)
    }
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
    s: &Sampler,
    policy: &SamplingPolicy,
    record: &ScopeRecord,
    now: Timestamp,
    count: u64,
) -> TestResult<usize> {
    let mut picked = 0;
    for number in 1..=count {
        let decision = s.select(policy, &merge(number)?, record, &open_budget(), now)?;
        if decision.record.outcome == Outcome::Selected {
            picked += 1;
        }
    }
    Ok(picked)
}

#[test]
fn a_new_grant_samples_at_the_initial_rate() -> TestResult {
    let s = sampler()?;
    let fresh = record([])?;
    assert_eq!(rate_at(&fresh, at(0))?, 500);
    let decision = s.select(&policy()?, &merge(1)?, &fresh, &open_budget(), at(0))?;
    assert_eq!(decision.record.rate, rate(500)?);
    assert_eq!(decision.record.inputs.clean_merges, 0);
    let expected = if decision.record.draw < 500 {
        Outcome::Selected
    } else {
        Outcome::Skipped
    };
    assert_eq!(decision.record.outcome, expected);
    // Draws spread over 0..1000, so about half of a new grant's merges are
    // picked. The key is random, so the bounds are six standard deviations
    // wide.
    let half = picked(&s, &policy()?, &fresh, at(0), 1000)?;
    assert!((400..=600).contains(&half), "picked {half} of 1000");
    assert_eq!(picked(&s, &flat(1000)?, &fresh, at(0), 50)?, 50);
    Ok(())
}

#[test]
fn a_long_clean_record_sits_at_the_floor_and_still_samples() -> TestResult {
    let s = sampler()?;
    let mature = record(1..=60)?;
    assert_eq!(rate_at(&mature, day(120))?, 20);
    let picks = picked(&s, &policy()?, &mature, day(120), 2000)?;
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
fn a_rerun_with_a_later_clock_and_changed_rates_is_already_recorded() -> TestResult {
    let finding = source("https://example.invalid/revert/1")?;
    let mut history = record(1..=60)?;
    history.findings.push(RecordedFinding {
        source: finding.clone(),
        at: day(100),
    });
    let first = RateRaise::of(&policy()?, &history, &finding, day(110))?;
    // Later clean deliveries and a later clock change at, from, and to.
    history.clean.extend((101..=105).map(|n| at(n * DAY + 1)));
    let rerun = RateRaise::of(&policy()?, &history, &finding, day(120))?;
    assert_ne!(
        (first.at, first.from, first.to),
        (rerun.at, rerun.from, rerun.to)
    );

    let f = Fixture::new()?;
    let recorder = scheduled("sampling")?;
    assert!(matches!(
        first.record(&f.store, &recorder, day(110))?,
        MarkerRecording::Recorded(_)
    ));
    let MarkerRecording::AlreadyRecorded(kept) = rerun.record(&f.store, &recorder, day(120))?
    else {
        return Err("a recomputed raise for the same finding and scope was recorded again".into());
    };
    // The first report's values stand.
    let schema = MarkerSchema::new("inspection-sampling.rate-raise", NonZeroU32::MIN)?;
    assert_eq!(kept.fact().decode::<RateRaise>(&schema)?, first);
    Ok(())
}

#[test]
fn one_finding_attributed_to_two_scopes_records_both_raises() -> TestResult {
    let finding = source("https://example.invalid/revert/1")?;
    let raise_for = |work_type: &str| -> TestResult<RateRaise> {
        let mut history = record(1..=60)?;
        history.scope = scope_of(work_type)?;
        history.findings.push(RecordedFinding {
            source: finding.clone(),
            at: day(100),
        });
        Ok(RateRaise::of(&policy()?, &history, &finding, day(110))?)
    };
    let (code, docs) = (raise_for("implementation")?, raise_for("documentation")?);
    assert_ne!(code.scope, docs.scope);

    let f = Fixture::new()?;
    let recorder = scheduled("sampling")?;
    let MarkerRecording::Recorded(recorded) = code.record(&f.store, &recorder, day(110))? else {
        return Err("the first raise was not recorded".into());
    };
    // The subject is a fixed-width digest, the same on every host, so a store
    // moved between architectures still finds the raise.
    let kitchen::state::MarkerSubject::Observation(subject) = &recorded.key().subject else {
        return Err("a raise's subject is an observation".into());
    };
    assert_eq!(
        subject.as_str(),
        "finding:51f047dca398f6ca5833b9589ca614b9dd75b93dabcf4fd909c654a18d780928"
    );
    assert!(matches!(
        docs.record(&f.store, &recorder, day(110))?,
        MarkerRecording::Recorded(_)
    ));
    assert!(matches!(
        docs.record(&f.store, &recorder, day(111))?,
        MarkerRecording::AlreadyRecorded(_)
    ));
    Ok(())
}

#[test]
fn budget_exhaustion_defers_a_pick_and_is_reported_without_lowering_the_rate() -> TestResult {
    let s = sampler()?;
    let always = flat(1000)?;
    let fresh = record([])?;
    let deferred = s.select(
        &always,
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
    deferred.replay(&always, &s.key)?;

    // The floor is the same with and without budget.
    let mature = record(1..=60)?;
    let (mut with_budget, mut without) = (Vec::new(), Vec::new());
    for number in 1..=200 {
        let merged = merge(number)?;
        let open = s.select(&policy()?, &merged, &mature, &open_budget(), day(120))?;
        let spent = s.select(
            &policy()?,
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
        s.select(&always, &merge(1)?, &fresh, &budget(None, false), at(0)),
        Err(SamplingError::IncompleteBudget)
    ));
    assert!(matches!(
        s.select(&always, &merge(1)?, &fresh, &open_budget(), day(1000)),
        Err(SamplingError::StaleBudget)
    ));
    // A skipped merge spends nothing, so exhaustion does not matter. The
    // draw is random per house key, so a rare head may still be selected;
    // that one alone needs budget evidence and is refused for lacking it.
    let rare = flat(1)?;
    let mut skipped = 0;
    for number in 1..=50 {
        match s.select(&rare, &merge(number)?, &fresh, &budget(None, false), at(0)) {
            Ok(decision) => {
                assert_eq!(decision.record.outcome, Outcome::Skipped);
                assert!(decision.report().is_none());
                skipped += 1;
            }
            Err(SamplingError::IncompleteBudget) => {}
            Err(other) => return Err(format!("unexpected refusal: {other:?}").into()),
        }
    }
    // At a 1 in 1000 rate, 50 heads are all selected with probability 1e-150.
    assert!(skipped > 0);
    Ok(())
}

#[test]
fn selection_is_recorded_and_replays_from_its_inputs() -> TestResult {
    let s = sampler()?;
    let f = &s.f;
    let recorder = scheduled("sampling")?;
    let mut history = record(1..=20)?;
    history.findings.push(RecordedFinding {
        source: source("fixture:finding")?,
        at: day(3),
    });
    let first = s.select(&policy()?, &merge(7)?, &history, &open_budget(), day(40))?;
    let again = s.select(&policy()?, &merge(7)?, &history, &open_budget(), day(40))?;
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
    loaded.replay(&policy()?, &s.key)?;
    assert_eq!(SamplingDecision::load(&f.store, &merge(8)?)?, None);

    // A later decision for the same merge cannot replace the recorded one.
    let later = s.select(&policy()?, &merge(7)?, &history, &open_budget(), day(90))?;
    assert_ne!(later.record, first.record);
    assert!(matches!(
        later.record(&f.store, &recorder, day(90)),
        Err(kitchen::Error::State(StateError::MarkerConflict))
    ));

    // A tampered or re-policied decision does not replay.
    let mut tampered = loaded.clone();
    tampered.record.draw = (tampered.record.draw + 1) % 1000;
    assert!(matches!(
        tampered.replay(&policy()?, &s.key),
        Err(SamplingError::NotReproducible)
    ));
    let mut outcome = loaded.clone();
    outcome.record.outcome = match loaded.record.outcome {
        Outcome::Skipped => Outcome::Selected,
        _ => Outcome::Skipped,
    };
    assert!(matches!(
        outcome.replay(&policy()?, &s.key),
        Err(SamplingError::NotReproducible)
    ));
    let mut revised = policy()?;
    revised.revision = NonZeroU32::new(2).ok_or("zero")?;
    assert!(matches!(
        loaded.replay(&revised, &s.key),
        Err(SamplingError::NotReproducible)
    ));
    let mut backwards = loaded.clone();
    backwards.record.inputs.clean_since = day(50);
    assert!(matches!(
        backwards.replay(&policy()?, &s.key),
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
    let s = sampler()?;
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
            s.select(&invalid, &merge(1)?, &fresh, &open_budget(), at(0)),
            Err(SamplingError::InvalidPolicy)
        ));
    }

    // A work type's own schedule overrides the default.
    let mut per_type = policy()?;
    per_type
        .work_types
        .insert(WorkType::new("implementation")?, flat(1000)?.default);
    let decision = s.select(&per_type, &merge(1)?, &fresh, &open_budget(), at(0))?;
    assert_eq!(decision.record.rate, rate(1000)?);

    let elsewhere = ScopeRecord {
        scope: StationScope {
            project: Repository::new("example/other")?,
            ..scope()?
        },
        ..record([])?
    };
    assert!(matches!(
        s.select(&policy()?, &merge(1)?, &elsewhere, &open_budget(), at(0)),
        Err(SamplingError::InvalidInput)
    ));
    // A decision before the scope's epoch has no grant to judge.
    let late = Fixture::new()?;
    let epoch = GrantEpoch::establish(&late.store, &scope()?, &scheduled("sampling")?, day(5))?;
    let future = ScopeRecord {
        granted_at: day(5),
        ..record([])?
    };
    assert!(matches!(
        select(
            &policy()?,
            &s.key,
            &merge(1)?,
            &future,
            &epoch,
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

    let recorder = scheduled("sampling")?;
    let epoch = |work_type: &str| -> TestResult<GrantEpoch> {
        Ok(GrantEpoch::establish(
            &f.store,
            &scope_of(work_type)?,
            &recorder,
            at(0),
        )?)
    };
    let implementation = ScopeRecord::from_ledger(&ledger, &epoch("implementation")?)?;
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
    let firmware = ScopeRecord::from_ledger(&ledger, &epoch("firmware")?)?;
    assert!(firmware.clean.is_empty());
    assert_eq!(firmware.findings.len(), 1);
    let idle = ScopeRecord::from_ledger(&ledger, &epoch("docs")?)?;
    assert!(idle.clean.is_empty() && idle.findings.is_empty());

    // Another house's epoch cannot read this ledger.
    let foreign = Fixture::new()?;
    let other = HouseStore::initialize(
        foreign.dir.path().join("other"),
        other_house()?,
        StoreOptions::default(),
    )?;
    let foreign_epoch = GrantEpoch::establish(&other, &scope()?, &recorder, at(0))?;
    assert!(matches!(
        ScopeRecord::from_ledger(&ledger, &foreign_epoch),
        Err(SamplingError::HouseMismatch)
    ));
    Ok(())
}

/// Deliver one clean change, then reserve one inspection sample of it at
/// time 6, under an inspector claim taken at time 0. Returns the ledger, the
/// inspection's plan, and the claim's fence.
fn reserved_sample(f: &Fixture) -> TestResult<(Ledger, InspectionPlan, kitchen::contracts::Fence)> {
    let ledger = Ledger::initialize(f.dir.path().join("trust"), house()?)?;
    deliver(
        f,
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
    Ok((ledger, plan, fence))
}

fn confirmed() -> TestResult<SampleResult> {
    Ok(SampleResult::Confirmed {
        finding: revert("fixture:inspection-finding")?,
        route: FollowUpRoute::Issue,
    })
}

#[test]
fn a_confirmed_inspection_sample_is_a_finding_for_the_delivering_scope() -> TestResult {
    let s = sampler()?;
    let f = &s.f;
    let (ledger, plan, fence) = reserved_sample(f)?;
    // Before the result arrives the delivery is clean.
    assert_eq!(
        ScopeRecord::from_ledger(&ledger, &s.epoch)?.findings,
        Vec::new()
    );
    ledger.finish_sample(
        &f.store,
        &plan.id,
        fence,
        1,
        confirmed()?,
        &ManualClock::starting_at(7),
    )?;

    let record = ScopeRecord::from_ledger(&ledger, &s.epoch)?;
    assert_eq!(
        record.findings,
        vec![RecordedFinding {
            source: source("fixture:inspection-finding")?,
            at: at(7),
        }]
    );
    assert_eq!(rate_at(&record, at(10))?, 800);
    let firmware = GrantEpoch::establish(
        &f.store,
        &scope_of("firmware")?,
        &scheduled("sampling")?,
        at(0),
    )?;
    assert!(
        ScopeRecord::from_ledger(&ledger, &firmware)?
            .findings
            .is_empty()
    );
    Ok(())
}

#[test]
fn a_late_confirmation_raises_the_rate_for_a_full_window_from_its_arrival() -> TestResult {
    let s = sampler()?;
    let f = &s.f;
    let (ledger, plan, _) = reserved_sample(f)?;
    // The result arrives 40 days after its reservation, past the 30-day
    // finding window, under a fresh claim of the inspector task.
    let fence = f
        .store
        .take_over(
            &task_id("inspector")?,
            &scheduled("reviewer")?,
            ttl(600)?,
            day(40),
        )?
        .fence();
    ledger.finish_sample(
        &f.store,
        &plan.id,
        fence,
        1,
        confirmed()?,
        &ManualClock::starting_at(40 * DAY),
    )?;
    let record = ScopeRecord::from_ledger(&ledger, &s.epoch)?;
    assert_eq!(record.findings[0].at, day(40));
    // Dated at its reservation, the finding would already be outside the
    // window and the rate would stay below the raised rate.
    assert_eq!(rate_at(&record, day(40))?, 800);
    let finding = source("fixture:inspection-finding")?;
    let raise = RateRaise::of(&policy()?, &record, &finding, day(40))?;
    assert_eq!(raise.finding_at, day(40));
    assert!(raise.from < raise.to, "{raise:?}");
    assert_eq!(raise.to, rate(800)?);
    // The window runs from the confirmation: raised until day 70, then back
    // to the initial rate, and the raise is no longer reportable.
    assert_eq!(rate_at(&record, day(69))?, 800);
    assert_eq!(rate_at(&record, day(70))?, 500);
    assert!(matches!(
        RateRaise::of(&policy()?, &record, &finding, day(70)),
        Err(SamplingError::ExpiredFinding)
    ));

    // A result recorded before results carried their time counts from its
    // reservation.
    let path = f.dir.path().join("trust").join("ledger.json");
    let mut doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let sample = &mut doc["inspections"][0]["samples"][0];
    assert_eq!(sample["finishedAt"], serde_json::json!(40 * DAY * 1000));
    sample
        .as_object_mut()
        .ok_or("sample is not an object")?
        .remove("finishedAt");
    fs::write(&path, serde_json::to_vec(&doc)?)?;
    let legacy = Ledger::open(f.dir.path().join("trust"), house()?)?;
    assert_eq!(
        ScopeRecord::from_ledger(&legacy, &s.epoch)?.findings[0].at,
        at(6)
    );
    Ok(())
}

#[test]
fn deliveries_before_the_house_first_saw_the_grant_do_not_lower_the_rate() -> TestResult {
    let f = Fixture::new()?;
    let recorder = scheduled("sampling")?;
    let ledger = Ledger::initialize(f.dir.path().join("trust"), house()?)?;
    deliver(
        &f,
        &ledger,
        "before",
        "implementation",
        EvidenceMode::Live,
        Vec::new(),
    )?;
    // Sampling first sees the grant on day 61, after that delivery.
    let epoch = GrantEpoch::establish(&f.store, &scope()?, &recorder, day(61))?;
    assert_eq!(epoch.at(), day(61));
    let observed = ScopeRecord::from_ledger(&ledger, &epoch)?;
    assert_eq!(observed.granted_at, day(61));
    assert!(observed.clean.is_empty());
    assert_eq!(rate_at(&observed, day(61))?, 500);

    // The epoch never moves, even when a later call, another process, or a
    // restart asks again.
    let again = GrantEpoch::establish(&f.reopen()?, &scope()?, &recorder, day(90))?;
    assert_eq!(again, epoch);

    // A record that claims an older grant, so that 60 earlier clean
    // deliveries would count and the rate would sit at the floor, is refused.
    let key = SelectionKey::establish(&f.store, &project()?, &recorder, day(61))?;
    let backdated = record(1..=60)?;
    assert_eq!(rate_at(&backdated, day(120))?, 20);
    assert!(matches!(
        select(
            &policy()?,
            &key,
            &merge(1)?,
            &backdated,
            &epoch,
            &open_budget(),
            day(120)
        ),
        Err(SamplingError::UnboundRecord)
    ));
    // The same deliveries, dated before the epoch, count for nothing in a
    // bound record: the rate is still the one for a new grant on day 62.
    let bound = ScopeRecord {
        granted_at: epoch.at(),
        ..record(1..=60)?
    };
    let decision = select(
        &policy()?,
        &key,
        &merge(1)?,
        &bound,
        &epoch,
        &open_budget(),
        day(62),
    )?;
    assert_eq!(decision.record.inputs.clean_merges, 0);
    assert_eq!(decision.record.rate, rate(500)?);

    // Another scope's epoch does not bind this scope's record.
    let docs = GrantEpoch::establish(&f.store, &scope_of("docs")?, &recorder, day(61))?;
    assert!(matches!(
        select(
            &policy()?,
            &key,
            &merge(1)?,
            &bound,
            &docs,
            &open_budget(),
            day(62)
        ),
        Err(SamplingError::UnboundRecord)
    ));
    Ok(())
}

/// The recorded key's hexadecimal text, read from the store file.
fn stored_key(f: &Fixture) -> TestResult<String> {
    let state: serde_json::Value = serde_json::from_str(&fs::read_to_string(f.state_path())?)?;
    let markers = state["markers"].as_array().ok_or("no markers")?;
    let marker = markers
        .iter()
        .find(|marker| marker["fact"]["schema"] == "inspection-sampling.selection-key/1")
        .ok_or("no key marker")?;
    let payload: serde_json::Value =
        serde_json::from_str(marker["fact"]["payload"].as_str().ok_or("no payload")?)?;
    Ok(payload["key"].as_str().ok_or("no key")?.to_owned())
}

#[test]
fn an_author_cannot_precompute_draws_for_candidate_heads() -> TestResult {
    let house_a = sampler()?;
    let house_b = sampler()?;
    let always = flat(1000)?;
    let fresh = record([])?;
    // One pull request, 300 heads its author could push. Anyone without the
    // house key, modelled by a second house's key, computes unrelated draws.
    let mut agree = 0;
    for n in 0..300u64 {
        let candidate = MergeSubject {
            head: CommitId::new(&format!("{:040x}", 0xabc0_0000 + n))?,
            ..merge(7)?
        };
        let a = house_a.select(&always, &candidate, &fresh, &open_budget(), at(0))?;
        let b = house_b.select(&always, &candidate, &fresh, &open_budget(), at(0))?;
        if a.record.draw == b.record.draw {
            agree += 1;
        }
    }
    // Independent draws agree about once in 1000 tries.
    assert!(agree <= 10, "{agree} of 300 draws agreed without the key");

    // The key is stable for the house: another handle reads the same key and
    // replays the same decisions; the other key does not.
    let decision = house_a.select(&always, &merge(7)?, &fresh, &open_budget(), at(0))?;
    let reread = SelectionKey::load(&house_a.f.reopen()?, &project()?)?.ok_or("no key")?;
    assert_eq!(reread, house_a.key);
    decision.replay(&always, &reread)?;
    let mut mismatched = 0;
    for number in 1..=50 {
        let decision = house_a.select(&always, &merge(number)?, &fresh, &open_budget(), at(0))?;
        if matches!(
            decision.replay(&always, &house_b.key),
            Err(SamplingError::NotReproducible)
        ) {
            mismatched += 1;
        }
    }
    assert!(mismatched >= 45, "only {mismatched} of 50 failed to replay");
    let again = SelectionKey::establish(
        &house_a.f.store,
        &project()?,
        &scheduled("sampling")?,
        day(1),
    )?;
    assert_eq!(again, house_a.key);

    // The key is never printed.
    let text = stored_key(&house_a.f)?;
    assert_eq!(text.len(), 64);
    assert!(text.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!format!("{:?}", house_a.key).contains(&text));
    assert!(!format!("{decision:?}").contains(&text));

    // A key for another repository cannot select or replay this merge.
    let elsewhere = SelectionKey::establish(
        &house_a.f.store,
        &Repository::new("example/other")?,
        &scheduled("sampling")?,
        at(0),
    )?;
    assert_ne!(elsewhere, house_a.key);
    assert!(matches!(
        select(
            &always,
            &elsewhere,
            &merge(7)?,
            &fresh,
            &house_a.epoch,
            &open_budget(),
            at(0)
        ),
        Err(SamplingError::UnboundRecord)
    ));
    assert!(matches!(
        decision.replay(&always, &elsewhere),
        Err(SamplingError::UnboundRecord)
    ));

    // A damaged key is refused, not replaced with a fresh one.
    let path = house_a.f.state_path();
    let damaged = fs::read_to_string(&path)?.replace(&text, &"z".repeat(64));
    fs::write(&path, damaged)?;
    assert!(matches!(
        SelectionKey::establish(
            &house_a.f.reopen()?,
            &project()?,
            &scheduled("sampling")?,
            day(2)
        ),
        Err(kitchen::Error::Sampling(SamplingError::MalformedRecord))
    ));
    Ok(())
}

#[test]
fn compaction_frees_a_full_store_and_keeps_decisions_for_the_audit_horizon() -> TestResult {
    let s = sampler()?;
    let recorder = scheduled("sampling")?;
    let always = flat(1000)?;
    let history = record(1..=5)?;
    let decide = |number: u64, now: Timestamp| -> TestResult<SamplingDecision> {
        Ok(s.select(&always, &merge(number)?, &history, &open_budget(), now)?)
    };
    let old = decide(1, day(1))?;
    old.record(&s.f.store, &recorder, day(1))?;
    let recent = decide(2, day(50))?;
    recent.record(&s.f.store, &recorder, day(50))?;
    // Another scope's decision gets the same horizon.
    let docs_epoch = GrantEpoch::establish(&s.f.store, &scope_of("docs")?, &recorder, at(0))?;
    let docs_record = ScopeRecord {
        scope: scope_of("docs")?,
        ..record([])?
    };
    select(
        &always,
        &s.key,
        &merge(3)?,
        &docs_record,
        &docs_epoch,
        &open_budget(),
        day(1),
    )?
    .record(&s.f.store, &recorder, day(1))?;
    // Raises for a finding long past its window and a recent one.
    let mut found = record(1..=5)?;
    for (name, when) in [
        ("fixture:old-finding", day(2)),
        ("fixture:new-finding", day(80)),
    ] {
        found.findings.push(RecordedFinding {
            source: source(name)?,
            at: when,
        });
    }
    RateRaise::of(&policy()?, &found, &source("fixture:old-finding")?, day(3))?.record(
        &s.f.store,
        &recorder,
        day(3),
    )?;
    RateRaise::of(&policy()?, &found, &source("fixture:new-finding")?, day(81))?.record(
        &s.f.store,
        &recorder,
        day(81),
    )?;

    // Fill the store with copies of the old decision, one pull request each.
    let path = s.f.state_path();
    let mut state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let markers = state["markers"].as_array().ok_or("no markers")?.clone();
    let template = markers
        .iter()
        .find(|marker| marker["key"]["item"]["number"] == 1)
        .ok_or("no decision")?
        .clone();
    let copies = MAX_MARKERS - markers.len();
    let mut filled = markers;
    filled.extend((0..copies).map(|n| {
        let mut copy = template.clone();
        copy["key"]["item"]["number"] = (1000 + n).into();
        copy
    }));
    state["markers"] = filled.into();
    fs::write(&path, serde_json::to_vec(&state)?)?;
    let store = s.f.reopen()?;
    assert_eq!(store.capacity()?.markers.used, MAX_MARKERS);
    let next = decide(4, day(91))?;
    assert!(matches!(
        next.record(&store, &recorder, day(91)),
        Err(kitchen::Error::State(StateError::CapacityExceeded { .. }))
    ));

    // The store's retention pass leaves sampling markers to their owner,
    // even when their merged pull requests are observed closed.
    let mut inventory = Inventory::new();
    for number in [1, 2, 3] {
        inventory.observe(
            WorkItem::PullRequest {
                repository: project()?,
                number: NonZeroU64::new(number).ok_or("zero")?,
            },
            Presence::Gone,
        );
    }
    let preview = store.preview_retention(&RetentionPolicy::default(), &inventory, day(91))?;
    assert!(preview.markers.is_empty(), "{:?}", preview.markers);

    // On day 90 the day-1 decisions are 89 days old, inside the 90-day
    // horizon: all stay. Only the raise past its finding window goes.
    let compaction = compact(&store, &policy()?, &recorder, day(90))?;
    assert_eq!((compaction.decisions, compaction.raises), (0, 1));
    SamplingDecision::load(&store, &merge(1)?)?
        .ok_or("retired inside the horizon")?
        .replay(&always, &s.key)?;

    // On day 91 they reach the horizon and are retired.
    let compaction = compact(&store, &policy()?, &recorder, day(91))?;
    assert_eq!((compaction.decisions, compaction.raises), (copies + 2, 0));
    // The day-50 decision, the recent raise, the key, and both epochs remain.
    assert_eq!(store.capacity()?.markers.used, 5);
    // 41 days old, past the finding window but inside the horizon: it still
    // replays.
    let kept = SamplingDecision::load(&store, &merge(2)?)?.ok_or("retired")?;
    assert_eq!(kept, recent);
    kept.replay(&always, &s.key)?;
    // Past the horizon a decision has expired: it can no longer be loaded,
    // so why merges 1 and 3 were picked cannot be shown any more.
    assert_eq!(SamplingDecision::load(&store, &merge(1)?)?, None);
    assert_eq!(SamplingDecision::load(&store, &merge(3)?)?, None);
    assert_eq!(
        SelectionKey::load(&store, &project()?)?.as_ref(),
        Some(&s.key)
    );
    assert_eq!(
        GrantEpoch::establish(&store, &scope()?, &recorder, day(91))?,
        s.epoch
    );
    assert!(matches!(
        next.record(&store, &recorder, day(91))?,
        MarkerRecording::Recorded(_)
    ));
    // A second pass has nothing left to retire.
    assert_eq!(
        compact(&store, &policy()?, &recorder, day(91))?,
        kitchen::workflows::sampling::Compaction::default()
    );
    // An invalid policy retires nothing.
    let mut inverted = policy()?;
    inverted.default.floor = rate(600)?;
    assert!(matches!(
        compact(&store, &inverted, &recorder, day(400)),
        Err(kitchen::Error::Sampling(SamplingError::InvalidPolicy))
    ));
    assert_eq!(store.capacity()?.markers.used, 6);
    Ok(())
}

#[test]
fn the_audit_horizon_defaults_to_ninety_days_and_covers_every_finding_window() -> TestResult {
    let mut parsed: serde_json::Value = serde_json::to_value(policy()?)?;
    let removed = parsed
        .as_object_mut()
        .ok_or("not an object")?
        .remove("auditHorizonDays");
    assert_eq!(removed, Some(90.into()));
    let defaulted: SamplingPolicy = serde_json::from_value(parsed)?;
    assert_eq!(defaulted.audit_horizon_days.get(), 90);
    defaulted.validate()?;

    // A horizon equal to the finding window is the shortest accepted.
    let mut shortest = policy()?;
    shortest.audit_horizon_days = NonZeroU16::new(30).ok_or("zero")?;
    shortest.validate()?;

    let mut short = policy()?;
    short.audit_horizon_days = NonZeroU16::new(29).ok_or("zero")?;
    let mut long_window = schedule()?;
    long_window.finding_window_days = NonZeroU16::new(120).ok_or("zero")?;
    let mut per_type = policy()?;
    per_type
        .work_types
        .insert(WorkType::new("docs")?, long_window);
    let mut too_long = policy()?;
    too_long.audit_horizon_days = NonZeroU16::new(MAX_DAYS + 1).ok_or("zero")?;
    for invalid in [short, per_type, too_long] {
        assert!(matches!(
            invalid.validate(),
            Err(SamplingError::InvalidPolicy)
        ));
    }
    Ok(())
}
