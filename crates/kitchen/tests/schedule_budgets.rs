//! Schedule intervals and usage budgets per house (#40).
//!
//! Policy decisions run on constructed observations. Adapter behavior runs
//! against the simulated Orca runtime (`orca_sim`); none of this is live
//! runtime evidence.

mod common;
mod orca_sim;

use std::{collections::BTreeSet, num::NonZeroU32, time::Duration};

use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, house, other_house, scheduled, task_id,
    ttl,
};
use kitchen::{
    BackendId, ConsumerId, CredentialId, EffectName, HouseId,
    adapters::orca::{OrcaBackend, OrcaConfig, OrcaError},
    adoption::{HouseRegistry, InstructionBundle},
    contracts::{
        BranchName, Capability, CapabilityRequirements, CapabilitySet, Effect, EffectExecutor,
        EvidenceRevision, ExternalRef, Grant, HouseGrants, Permission, Provenance, ResourceKind,
        ResourceRef, RetryPolicy, Role, ScheduleEffect, TaskAuthority, TaskSpec, Text, Timestamp,
    },
    house::{
        AccessStatus, DoctorCode, DoctorEvidence, HouseConfig, HouseError, RepositoryConfig,
        RepositoryLabel, Workflow, doctor,
    },
    scheduling::{
        AgentFamily, Budget, BudgetError, CronExpr, InstalledSchedule, IntervalMinutes, JudgedRun,
        ObservedScheduleState, Readiness, Recurrence, RunOutcome, RunVerdict, ScheduleEvidence,
        ScheduleLimit, ScheduleLimits, ScheduleObservation, SchedulePolicy, ScheduleRun,
        ScheduleSpec, ScheduleState, ScheduleUsage, Timezone, TokenUsage, WindowHours,
        WorkflowName,
    },
    state::{EffectPlan, EffectState, run_effect},
    trust::Measurement,
};
use orca_sim::SimOrca;
use serde_json::json;

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;

fn budget(runs: u32, tokens: Option<u64>) -> TestResult<Budget> {
    Ok(Budget {
        runs: NonZeroU32::new(runs).ok_or("zero runs")?,
        tokens: tokens.map(|tokens| tokens.try_into()).transpose()?,
    })
}

/// A daily window, a 60-minute house minimum, 10 house runs, and 4 runs per
/// schedule by default; tokens capped at 1000 per house and 400 per schedule.
fn policy() -> TestResult<SchedulePolicy> {
    Ok(SchedulePolicy {
        window_hours: WindowHours::new(24)?,
        min_interval_minutes: IntervalMinutes::new(60)?,
        house_budget: budget(10, Some(1000))?,
        schedule_budget: budget(4, Some(400))?,
        schedules: std::collections::BTreeMap::new(),
        idle: kitchen::scheduling::IdlePolicy::default(),
    })
}

fn consumer(name: &str) -> TestResult<ConsumerId> {
    Ok(ConsumerId::new(name)?)
}

fn orca_id() -> TestResult<BackendId> {
    Ok(BackendId::new("orca-local")?)
}

fn schedule_ref(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Schedule,
        backend: orca_id()?,
        handle: ExternalRef::new(handle)?,
    })
}

fn spec(name: &str, cron: &str) -> TestResult<ScheduleSpec> {
    Ok(ScheduleSpec::new(
        WorkflowName::new("pickup")?,
        consumer(name)?,
        Recurrence::Cron(CronExpr::new(cron)?),
        Timezone::new("America/Toronto")?,
        Text::new("Run Kitchen pickup.")?,
        AgentFamily::Claude,
    ))
}

fn installed(name: &str, state: ObservedScheduleState) -> TestResult<InstalledSchedule> {
    Ok(InstalledSchedule {
        resource: schedule_ref(name)?,
        consumer: consumer(name)?,
        state,
    })
}

fn tokens(value: u64) -> TestResult<Measurement<u64>> {
    Ok(Measurement::Observed {
        value,
        samples: NonZeroU32::MIN,
        source: ExternalRef::new("orca-run:test")?,
    })
}

/// A run due at `due_ms` with `verdict` and reported `usage`.
fn run(due_ms: u64, verdict: RunVerdict, usage: Measurement<u64>) -> JudgedRun {
    let outcome = match verdict {
        RunVerdict::Idle => RunOutcome::PrecheckIdle,
        RunVerdict::PrecheckFailed => RunOutcome::PrecheckFailed,
        RunVerdict::Skipped => RunOutcome::Skipped,
        RunVerdict::LaunchFailed => RunOutcome::LaunchFailed,
        RunVerdict::Pending | RunVerdict::Started => RunOutcome::LaunchReported,
        RunVerdict::Unknown => RunOutcome::Unknown,
    };
    JudgedRun {
        run: ScheduleRun {
            outcome,
            scheduled_for: Some(Timestamp::from_unix_millis(due_ms)),
            created_at: None,
            usage,
        },
        verdict,
    }
}

fn usage(
    name: &str,
    state: ObservedScheduleState,
    runs: Vec<JudgedRun>,
) -> TestResult<ScheduleUsage> {
    Ok(ScheduleUsage {
        consumer: consumer(name)?,
        schedule: schedule_ref(name)?,
        observation: ScheduleObservation {
            state,
            recent_runs: runs,
        },
    })
}

fn evidence(
    house: HouseId,
    observed_at_ms: u64,
    schedules: Vec<ScheduleUsage>,
) -> ScheduleEvidence {
    ScheduleEvidence {
        house,
        observed_at: Timestamp::from_unix_millis(observed_at_ms),
        schedules,
    }
}

/// `count` started runs with `each` tokens, an hour apart from `start_ms`.
fn started(start_ms: u64, count: u64, each: u64) -> TestResult<Vec<JudgedRun>> {
    (0..count)
        .map(|index| {
            Ok(run(
                start_ms + index * HOUR_MS,
                RunVerdict::Started,
                tokens(each)?,
            ))
        })
        .collect()
}

#[test]
fn an_interval_below_the_minimum_is_refused_naming_the_limit() -> TestResult {
    let mut policy = policy()?;
    // Every 20 minutes breaks the 60-minute house minimum.
    assert_eq!(
        policy.check_install(&spec("pickup", "17,37,57 * * * *")?, &[]),
        Err(BudgetError::IntervalTooShort {
            consumer: consumer("pickup")?,
            limit: ScheduleLimit::HouseMinInterval,
            actual_minutes: 20,
            required_minutes: 60,
        })
    );
    // Exactly the minimum is allowed.
    policy.check_install(&spec("pickup", "17 * * * *")?, &[])?;
    // A schedule's own, stricter minimum is named instead.
    policy.schedules.insert(
        consumer("pickup")?,
        ScheduleLimits {
            min_interval_minutes: Some(IntervalMinutes::new(180)?),
            budget: None,
        },
    );
    let refused = policy.check_install(&spec("pickup", "17 * * * *")?, &[]);
    assert!(
        matches!(
            &refused,
            Err(BudgetError::IntervalTooShort {
                limit: ScheduleLimit::ScheduleMinInterval,
                actual_minutes: 60,
                required_minutes: 180,
                ..
            })
        ),
        "{refused:?}"
    );
    let message = refused.err().ok_or("refused")?.to_string();
    assert!(message.contains("schedule minimum interval"), "{message}");
    // Another schedule still gets the house minimum.
    policy.check_install(&spec("gardener", "17 * * * *")?, &[])?;
    Ok(())
}

#[test]
fn a_schedule_that_would_overcommit_the_house_budget_is_refused() -> TestResult {
    let policy = policy()?;
    let hourly = |name: &str| spec(name, "0 * * * *");
    // Two default allocations of 4 runs fit in 10; a third would not.
    let two = [
        installed("pickup", ObservedScheduleState::Active)?,
        installed("gate", ObservedScheduleState::Paused)?,
    ];
    assert_eq!(
        policy.check_install(&hourly("triage")?, &two),
        Err(BudgetError::Overcommitted {
            consumer: consumer("triage")?,
            limit: ScheduleLimit::HouseRuns,
            allocated: 12,
            allowed: 10,
        })
    );
    // Updating an installed schedule does not count it twice, and a schedule
    // the backend reports missing holds no allocation.
    policy.check_install(&hourly("pickup")?, &two)?;
    let with_missing = [
        installed("pickup", ObservedScheduleState::Active)?,
        installed("gate", ObservedScheduleState::Missing)?,
    ];
    policy.check_install(&hourly("triage")?, &with_missing)?;

    // Tokens: 400 + 400 + 300 > 1000 even though runs fit.
    let mut tokens = self::policy()?;
    tokens.house_budget = budget(20, Some(1000))?;
    tokens.schedules.insert(
        consumer("triage")?,
        ScheduleLimits {
            min_interval_minutes: None,
            budget: Some(budget(2, Some(300))?),
        },
    );
    assert_eq!(
        tokens.check_install(&hourly("triage")?, &two),
        Err(BudgetError::Overcommitted {
            consumer: consumer("triage")?,
            limit: ScheduleLimit::HouseTokens,
            allocated: 1100,
            allowed: 1000,
        })
    );
    Ok(())
}

#[test]
fn schedule_entries_cannot_relax_house_limits() -> TestResult {
    let relaxed_interval = {
        let mut policy = policy()?;
        policy.schedules.insert(
            consumer("pickup")?,
            ScheduleLimits {
                min_interval_minutes: Some(IntervalMinutes::new(30)?),
                budget: None,
            },
        );
        policy
    };
    assert_eq!(
        relaxed_interval.validate(),
        Err(BudgetError::Relaxation {
            limit: ScheduleLimit::HouseMinInterval
        })
    );
    let mut larger = policy()?;
    larger.schedule_budget = budget(11, Some(400))?;
    assert_eq!(
        larger.validate(),
        Err(BudgetError::Relaxation {
            limit: ScheduleLimit::HouseRuns
        })
    );
    // A schedule without a token limit under a house token limit is uncapped.
    let mut uncapped = policy()?;
    uncapped.schedule_budget = budget(4, None)?;
    assert_eq!(
        uncapped.validate(),
        Err(BudgetError::Relaxation {
            limit: ScheduleLimit::HouseTokens
        })
    );
    // An invalid policy refuses every install rather than allowing it.
    assert!(
        relaxed_interval
            .check_install(&spec("gate", "0 0 * * *")?, &[])
            .is_err()
    );
    Ok(())
}

#[test]
fn house_configuration_carries_and_validates_the_schedule_policy() -> TestResult {
    let mut config: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    let plain: HouseConfig = serde_json::from_value(config.clone())?;
    assert_eq!(plain.schedules, None, "older configurations still load");
    config["schedules"] = json!({
        "windowHours": 24,
        "minIntervalMinutes": 60,
        "houseBudget": {"runs": 10, "tokens": 1000},
        "scheduleBudget": {"runs": 4, "tokens": 400},
        "schedules": {"pickup": {"minIntervalMinutes": 120}},
    });
    let with_policy: HouseConfig = serde_json::from_value(config.clone())?;
    with_policy.validate()?;
    let policy = with_policy.schedules.as_ref().ok_or("policy")?;
    assert_eq!(policy.idle, kitchen::scheduling::IdlePolicy::default());
    assert_eq!(
        policy
            .schedules
            .get(&consumer("pickup")?)
            .and_then(|limits| limits.min_interval_minutes),
        Some(IntervalMinutes::new(120)?)
    );
    let round_trip: HouseConfig = serde_json::from_value(serde_json::to_value(&with_policy)?)?;
    assert_eq!(round_trip, with_policy);

    config["schedules"]["schedules"]["pickup"]["minIntervalMinutes"] = json!(5);
    let relaxed: HouseConfig = serde_json::from_value(config.clone())?;
    assert!(matches!(
        relaxed.validate(),
        Err(HouseError::PolicyRelaxation)
    ));
    config["schedules"]["windowHours"] = json!(0);
    assert!(serde_json::from_value::<HouseConfig>(config.clone()).is_err());
    config["schedules"]["windowHours"] = json!(24);
    config["schedules"]["unknown"] = json!(1);
    assert!(serde_json::from_value::<HouseConfig>(config).is_err());
    Ok(())
}

#[test]
fn budget_exhaustion_mid_window_pauses_once_and_reports_once() -> TestResult {
    let fixture = Fixture::new()?;
    let policy = policy()?;
    let house = house()?;
    let day = 100 * DAY_MS;
    let noon = day + 12 * HOUR_MS;
    // Three runs so far: within the 4-run budget.
    let three = evidence(
        house.clone(),
        noon,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            started(day, 3, 10)?,
        )?],
    );
    assert!(
        policy
            .plan_exhaustion(&house, &three, |_| false)?
            .is_empty()
    );
    policy.check_activation(&house, &three, &consumer("pickup")?)?;

    // The fourth run mid-window exhausts it.
    let four = evidence(
        house.clone(),
        noon,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            started(day, 4, 10)?,
        )?],
    );
    let reported =
        |key: &kitchen::state::MarkerKey| fixture.store.marker(key).ok().flatten().is_some();
    let [exhausted] = policy
        .plan_exhaustion(&house, &four, reported)?
        .try_into()
        .map_err(|_| "one")?;
    assert_eq!(exhausted.exhausted.limit, ScheduleLimit::ScheduleRuns);
    assert_eq!(
        (exhausted.exhausted.used, exhausted.exhausted.allowed),
        (4, 4)
    );
    assert_eq!(
        exhausted.pause_effect(),
        Some(Effect::Schedule(ScheduleEffect::SetState {
            schedule: schedule_ref("pickup")?,
            state: ScheduleState::Paused,
        }))
    );
    assert!(exhausted.report().contains("schedule run budget"));
    assert_eq!(
        policy.check_activation(&house, &four, &consumer("pickup")?),
        Err(BudgetError::Exhausted {
            consumer: consumer("pickup")?,
            limit: ScheduleLimit::ScheduleRuns,
            used: 4,
            allowed: 4,
        })
    );

    // After an interruption between the pause and the report, the paused
    // schedule is reported without being paused again.
    let paused = evidence(
        house.clone(),
        noon,
        vec![usage(
            "pickup",
            ObservedScheduleState::Paused,
            started(day, 4, 10)?,
        )?],
    );
    let [again] = policy
        .plan_exhaustion(&house, &paused, reported)?
        .try_into()
        .map_err(|_| "one")?;
    assert_eq!(again.pause_effect(), None);
    assert_eq!(again.marker_key()?, exhausted.marker_key()?);

    // Once the report is recorded, later passes in the window stay quiet.
    fixture.store.record_marker(
        exhausted.marker_key()?,
        exhausted.marker_fact()?,
        &scheduled("budget-pass")?,
        at(1),
    )?;
    assert!(
        policy
            .plan_exhaustion(&house, &paused, reported)?
            .is_empty()
    );

    // A schedule re-activated in the same window is paused again on every
    // pass, but its owner is not told twice.
    let [repause] = policy
        .plan_exhaustion(&house, &four, reported)?
        .try_into()
        .map_err(|_| "a pause without a second report")?;
    assert!(repause.pause);
    assert!(!repause.report_due);
    assert_eq!(repause.pause_effect(), exhausted.pause_effect());
    Ok(())
}

#[test]
fn a_token_budget_or_the_house_budget_can_exhaust_first() -> TestResult {
    let policy = policy()?;
    let house = house()?;
    let day = 10 * DAY_MS;
    // Two runs of 250 tokens reach the 400-token schedule budget.
    let heavy = evidence(
        house.clone(),
        day + 5 * HOUR_MS,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            started(day, 2, 250)?,
        )?],
    );
    let [only] = policy
        .plan_exhaustion(&house, &heavy, |_| false)?
        .try_into()
        .map_err(|_| "one")?;
    assert_eq!(only.exhausted.limit, ScheduleLimit::ScheduleTokens);
    assert_eq!((only.exhausted.used, only.exhausted.allowed), (500, 400));

    // Three runs on each of three schedules stay within their own budgets,
    // and one more on a since-removed schedule brings the house to its
    // 10-run budget: active and unknown schedules are paused, a paused one
    // is only reported, and a missing one is left alone.
    let busy = evidence(
        house.clone(),
        day + 5 * HOUR_MS,
        vec![
            usage("pickup", ObservedScheduleState::Active, started(day, 3, 1)?)?,
            usage("gate", ObservedScheduleState::Unknown, started(day, 3, 1)?)?,
            usage("triage", ObservedScheduleState::Paused, started(day, 3, 1)?)?,
            usage("gone", ObservedScheduleState::Missing, started(day, 1, 1)?)?,
        ],
    );
    let plan = policy.plan_exhaustion(&house, &busy, |_| false)?;
    let summary: Vec<(String, ScheduleLimit, bool)> = plan
        .iter()
        .map(|item| (item.consumer.to_string(), item.exhausted.limit, item.pause))
        .collect();
    assert_eq!(
        summary,
        [
            ("pickup".to_owned(), ScheduleLimit::HouseRuns, true),
            ("gate".to_owned(), ScheduleLimit::HouseRuns, true),
            ("triage".to_owned(), ScheduleLimit::HouseRuns, false),
        ]
    );
    Ok(())
}

/// A started trial: no due time, recorded at `created_ms` when known.
fn trial(created_ms: Option<u64>) -> TestResult<JudgedRun> {
    let mut judged = run(0, RunVerdict::Started, tokens(10)?);
    judged.run.scheduled_for = None;
    judged.run.created_at = created_ms.map(Timestamp::from_unix_millis);
    Ok(judged)
}

#[test]
fn a_run_with_no_due_time_counts_in_the_window_it_was_recorded_in() -> TestResult {
    let policy = policy()?;
    let house = house()?;
    let day = 100 * DAY_MS;
    let today_runs = |trials: Vec<JudgedRun>| -> TestResult<u32> {
        let today = evidence(
            house.clone(),
            day + HOUR_MS,
            vec![usage("pickup", ObservedScheduleState::Paused, trials)?],
        );
        let assessment = policy.assess(&house, &today)?;
        let [pickup] = assessment.schedules.as_slice() else {
            return Err("one schedule".into());
        };
        Ok(pickup.usage.runs)
    };
    // Four trials recorded yesterday belong to yesterday's window.
    let yesterday: Vec<JudgedRun> = (0..4)
        .map(|index| trial(Some(day - (index + 1) * HOUR_MS)))
        .collect::<TestResult<_>>()?;
    assert_eq!(today_runs(yesterday.clone())?, 0);
    // A trial recorded today counts, alongside yesterday's ignored ones.
    let mut mixed = yesterday;
    mixed.push(trial(Some(day + 30 * 60 * 1000))?);
    assert_eq!(today_runs(mixed)?, 1);
    // With no timestamp at all the run cannot be placed, so it counts.
    assert_eq!(today_runs(vec![trial(None)?])?, 1);
    Ok(())
}

#[test]
fn a_trial_recorded_before_the_window_shows_the_observation_reached_back() -> TestResult {
    let policy = policy()?;
    let house = house()?;
    let day = 100 * DAY_MS;
    // The cap of retained runs, all trials, the oldest recorded yesterday.
    let cap = u64::try_from(kitchen::scheduling::MAX_SCHEDULE_RUNS)?;
    let trials: Vec<JudgedRun> = (0..cap)
        .map(|index| trial(Some(day + HOUR_MS - index * 2 * HOUR_MS / cap)))
        .collect::<TestResult<_>>()?;
    let today = evidence(
        house.clone(),
        day + 2 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Paused, trials)?],
    );
    let assessment = policy.assess(&house, &today)?;
    let [pickup] = assessment.schedules.as_slice() else {
        return Err("one schedule".into());
    };
    assert!(pickup.usage.complete);
    Ok(())
}

#[test]
fn window_rollover_starts_a_fresh_budget_without_resuming() -> TestResult {
    let policy = policy()?;
    let house = house()?;
    let day = 100 * DAY_MS;
    // Four runs yesterday exhausted yesterday's window.
    let runs = started(day - 6 * HOUR_MS, 4, 10)?;
    let yesterday = evidence(
        house.clone(),
        day - HOUR_MS,
        vec![usage(
            "pickup",
            ObservedScheduleState::Paused,
            runs.clone(),
        )?],
    );
    assert_eq!(
        policy.plan_exhaustion(&house, &yesterday, |_| false)?.len(),
        1
    );
    // After midnight UTC they belong to the previous window: nothing is
    // exhausted, the schedule is not resumed automatically, and the owner
    // may activate it again.
    let today = evidence(
        house.clone(),
        day + HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Paused, runs)?],
    );
    let assessment = policy.assess(&house, &today)?;
    let [pickup] = assessment.schedules.as_slice() else {
        return Err("one schedule".into());
    };
    assert_eq!(pickup.usage.runs, 0);
    assert!(
        pickup.usage.complete,
        "earlier runs show the window's start"
    );
    assert_eq!(pickup.exhausted, None);
    assert_eq!(assessment.window.start, Timestamp::from_unix_millis(day));
    assert!(
        policy
            .plan_exhaustion(&house, &today, |_| false)?
            .is_empty()
    );
    policy.check_activation(&house, &today, &consumer("pickup")?)?;
    Ok(())
}

#[test]
fn missing_usage_is_unknown_not_zero() -> TestResult {
    let policy = policy()?;
    let house = house()?;
    let day = 5 * DAY_MS;
    let runs = vec![
        run(day, RunVerdict::Started, tokens(150)?),
        run(day + HOUR_MS, RunVerdict::Started, Measurement::Unavailable),
        run(day + 2 * HOUR_MS, RunVerdict::Pending, Measurement::Missing),
        // Idle and skipped runs started no agent: missing usage is no cost.
        run(day + 3 * HOUR_MS, RunVerdict::Idle, Measurement::Missing),
        run(
            day + 4 * HOUR_MS,
            RunVerdict::Skipped,
            Measurement::Unavailable,
        ),
    ];
    let observed = evidence(
        house.clone(),
        day + 5 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Active, runs)?],
    );
    let assessment = policy.assess(&house, &observed)?;
    let [pickup] = assessment.schedules.as_slice() else {
        return Err("one schedule".into());
    };
    assert_eq!(pickup.usage.runs, 3);
    assert_eq!(
        pickup.usage.tokens,
        TokenUsage::Unknown {
            known_tokens: 150,
            unknown_runs: 2
        }
    );
    assert_eq!(assessment.house.tokens, pickup.usage.tokens);
    // Unknown usage proves neither exhaustion nor headroom: no pause, and
    // doctor reports the token budget as unenforceable.
    assert_eq!(pickup.exhausted, None);
    let unenforceable = policy.unenforceable_token_budgets(&house, &observed)?;
    assert_eq!(unenforceable.len(), 1);
    // Reported tokens still count toward exhaustion alongside unknown runs.
    let heavy = evidence(
        house.clone(),
        day + 5 * HOUR_MS,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            vec![
                run(day, RunVerdict::Started, tokens(450)?),
                run(day + HOUR_MS, RunVerdict::Started, Measurement::Missing),
            ],
        )?],
    );
    let [exhausted] = policy
        .plan_exhaustion(&house, &heavy, |_| false)?
        .try_into()
        .map_err(|_| "one")?;
    assert_eq!(exhausted.exhausted.limit, ScheduleLimit::ScheduleTokens);
    Ok(())
}

#[test]
fn one_house_limits_do_not_apply_to_another_house() -> TestResult {
    let tight = policy()?;
    let mut roomy = policy()?;
    roomy.house_budget = budget(50, Some(10_000))?;
    roomy.schedule_budget = budget(20, Some(2_000))?;
    let day = 3 * DAY_MS;
    let runs = || started(day, 4, 10);
    let origin = evidence(
        house()?,
        day + 6 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Active, runs()?)?],
    );
    let crab = evidence(
        other_house()?,
        day + 6 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Active, runs()?)?],
    );
    assert_eq!(
        tight.plan_exhaustion(&house()?, &origin, |_| false)?.len(),
        1
    );
    // The same usage in the other house, under its own policy, is fine.
    assert!(
        roomy
            .plan_exhaustion(&other_house()?, &crab, |_| false)?
            .is_empty()
    );
    // One house's policy refuses another house's evidence outright.
    assert_eq!(
        tight.plan_exhaustion(&house()?, &crab, |_| false).err(),
        Some(BudgetError::HouseMismatch)
    );
    assert_eq!(
        tight.check_activation(&house()?, &crab, &consumer("pickup")?),
        Err(BudgetError::HouseMismatch)
    );
    Ok(())
}

#[test]
fn a_truncated_observation_is_a_lower_bound() -> TestResult {
    let mut policy = policy()?;
    policy.house_budget = budget(500, None)?;
    policy.schedule_budget = budget(200, None)?;
    let house = house()?;
    let day = 7 * DAY_MS;
    // A full listing that ends inside the window cannot show its start.
    let minute = 60_000;
    let runs: Vec<JudgedRun> = (0..u64::try_from(kitchen::scheduling::MAX_SCHEDULE_RUNS)?)
        .map(|index| {
            run(
                day + HOUR_MS + index * minute,
                RunVerdict::Idle,
                Measurement::Missing,
            )
        })
        .collect();
    let observed = evidence(
        house.clone(),
        day + 3 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Active, runs)?],
    );
    let assessment = policy.assess(&house, &observed)?;
    assert!(
        !assessment
            .schedules
            .iter()
            .all(|schedule| schedule.usage.complete)
    );
    assert!(!assessment.house.complete);
    Ok(())
}

/// The observation cap of runs due a minute apart inside the window that
/// starts at `day_ms`: `agents` started an agent, the rest were idle.
fn capped_window(day_ms: u64, agents: usize) -> Vec<JudgedRun> {
    (0..kitchen::scheduling::MAX_SCHEDULE_RUNS)
        .map(|index| {
            let verdict = if index < agents {
                RunVerdict::Started
            } else {
                RunVerdict::Idle
            };
            run(
                day_ms + HOUR_MS + u64::try_from(index).unwrap_or(0) * 60_000,
                verdict,
                Measurement::Missing,
            )
        })
        .collect()
}

fn pickup_at(day_ms: u64, runs: Vec<JudgedRun>) -> TestResult<ScheduleEvidence> {
    Ok(evidence(
        house()?,
        day_ms + 3 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Active, runs)?],
    ))
}

#[test]
fn a_run_budget_is_enforced_when_the_window_holds_more_runs_than_are_retained() -> TestResult {
    let house = house()?;
    let who = consumer("pickup")?;
    let day = 9 * DAY_MS;
    let mut policy = policy()?;
    policy.house_budget = budget(500, None)?;
    policy.schedule_budget = budget(100, None)?;

    // 100 agent runs fill the retention cap: the budget is reached.
    let full = pickup_at(day, capped_window(day, 100))?;
    let reached = BudgetError::Exhausted {
        consumer: who.clone(),
        limit: ScheduleLimit::ScheduleRuns,
        used: 100,
        allowed: 100,
    };
    assert_eq!(policy.check_activation(&house, &full, &who), Err(reached));
    let [pause] = policy
        .plan_exhaustion(&house, &full, |_| false)?
        .try_into()
        .map_err(|_| "one pause")?;
    assert!(pause.pause);

    // 99 agent runs in a window whose start is not visible may hide more:
    // activation fails closed instead of counting 99 as the whole window.
    let hidden = pickup_at(day, capped_window(day, 99))?;
    let incomplete = BudgetError::IncompleteEvidence {
        consumer: who.clone(),
        observed: 99,
        allowed: 100,
    };
    assert_eq!(
        policy.check_activation(&house, &hidden, &who),
        Err(incomplete)
    );

    // A budget above the retained runs can never be proven exhausted, so an
    // incomplete window is refused too.
    policy.schedule_budget = budget(101, None)?;
    assert_eq!(
        policy.check_activation(&house, &full, &who),
        Err(BudgetError::IncompleteEvidence {
            consumer: who.clone(),
            observed: 100,
            allowed: 101,
        })
    );

    // The same 99 runs in a window whose start the listing reaches, or in a
    // listing shorter than the cap, are the whole window.
    policy.schedule_budget = budget(100, None)?;
    let mut reaching = capped_window(day, 99);
    reaching[kitchen::scheduling::MAX_SCHEDULE_RUNS - 1] =
        run(day - HOUR_MS, RunVerdict::Started, Measurement::Missing);
    policy.check_activation(&house, &pickup_at(day, reaching)?, &who)?;
    let mut short = capped_window(day, 99);
    short.pop();
    policy.check_activation(&house, &pickup_at(day, short)?, &who)?;
    Ok(())
}

#[test]
fn another_schedules_incomplete_window_blocks_activation_under_the_house_budget() -> TestResult {
    let house = house()?;
    let day = 9 * DAY_MS;
    let mut policy = policy()?;
    policy.house_budget = budget(200, None)?;
    policy.schedule_budget = budget(150, None)?;
    let observed = evidence(
        house.clone(),
        day + 3 * HOUR_MS,
        vec![
            usage(
                "pickup",
                ObservedScheduleState::Paused,
                started(day, 2, 10)?,
            )?,
            usage(
                "gate",
                ObservedScheduleState::Active,
                capped_window(day, 60),
            )?,
        ],
    );
    assert_eq!(
        policy.check_activation(&house, &observed, &consumer("pickup")?),
        Err(BudgetError::IncompleteEvidence {
            consumer: consumer("pickup")?,
            observed: 62,
            allowed: 200,
        })
    );
    Ok(())
}

#[test]
fn evidence_that_repeats_a_schedule_or_is_oversized_is_refused() -> TestResult {
    let policy = policy()?;
    let day = 4 * DAY_MS;
    // Listing a schedule twice would count its runs twice against the house.
    let repeated = evidence(
        house()?,
        day,
        vec![
            usage("pickup", ObservedScheduleState::Active, started(day, 1, 1)?)?,
            usage("pickup", ObservedScheduleState::Active, started(day, 1, 1)?)?,
        ],
    );
    assert_eq!(
        policy.assess(&house()?, &repeated).err(),
        Some(BudgetError::InvalidEvidence)
    );
    let cap = u64::try_from(kitchen::scheduling::MAX_SCHEDULE_RUNS)?;
    let oversized = evidence(
        house()?,
        day,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            started(day, cap + 1, 1)?,
        )?],
    );
    assert_eq!(
        policy.assess(&house()?, &oversized).err(),
        Some(BudgetError::InvalidEvidence)
    );
    let at_cap = evidence(
        house()?,
        day,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            started(day, cap, 1)?,
        )?],
    );
    policy.assess(&house()?, &at_cap)?;
    Ok(())
}

#[test]
fn mostly_idle_schedules_are_recommendations_with_counts_and_usage() -> TestResult {
    let policy = policy()?;
    let day = 9 * DAY_MS;
    let mut idle_runs: Vec<JudgedRun> = (0..9)
        .map(|index| {
            run(
                day + index * HOUR_MS,
                RunVerdict::Idle,
                Measurement::Missing,
            )
        })
        .collect();
    idle_runs.push(run(
        day + 9 * HOUR_MS,
        RunVerdict::Started,
        Measurement::Unavailable,
    ));
    let mut busy: Vec<JudgedRun> = started(day, 5, 20)?;
    busy.extend((0..5).map(|index| {
        run(
            day + index * HOUR_MS,
            RunVerdict::Idle,
            Measurement::Missing,
        )
    }));
    let few: Vec<JudgedRun> = (0..3)
        .map(|index| {
            run(
                day + index * HOUR_MS,
                RunVerdict::Idle,
                Measurement::Missing,
            )
        })
        .collect();
    let observed = evidence(
        house()?,
        day + 12 * HOUR_MS,
        vec![
            usage("gardener", ObservedScheduleState::Active, idle_runs)?,
            usage("pickup", ObservedScheduleState::Active, busy)?,
            usage("triage", ObservedScheduleState::Active, few)?,
        ],
    );
    let idle = policy.idle_schedules(&observed);
    let [gardener] = idle.as_slice() else {
        return Err(format!("expected only the gardener: {idle:?}").into());
    };
    assert_eq!(gardener.consumer, consumer("gardener")?);
    assert_eq!((gardener.idle_runs, gardener.runs), (9, 10));
    assert_eq!(
        gardener.tokens,
        TokenUsage::Unknown {
            known_tokens: 0,
            unknown_runs: 1
        }
    );
    Ok(())
}

fn registry_with(
    schedules: Option<SchedulePolicy>,
) -> TestResult<(
    tempfile::TempDir,
    HouseRegistry,
    RepositoryConfig,
    DoctorEvidence,
)> {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let mut house: HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    house.schedules = schedules;
    let repository = RepositoryConfig {
        schema: 1,
        house: house.house.clone(),
        repository: house.repositories.first().ok_or("empty fixture")?.clone(),
        workflows: BTreeSet::from([Workflow::Pickup]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    };
    registry.initialize(&house)?;
    let bundle: InstructionBundle =
        serde_json::from_str(include_str!("fixtures/house/origin89-bundle.json"))?;
    registry.sync(&house.house, &bundle)?;
    let labels = doctor(&registry, &repository, None)?
        .labels
        .into_iter()
        .map(|item| RepositoryLabel {
            name: item.requirement.name,
            color: item.requirement.color,
            description: item.requirement.description,
        })
        .collect();
    let evidence = DoctorEvidence {
        house: house.house.clone(),
        repository: repository.repository.clone(),
        capabilities: CapabilitySet::supporting(Capability::ALL),
        labels: Some(labels),
        access: AccessStatus::Available,
        agent_models: None,
        schedules: None,
    };
    Ok((temp, registry, repository, evidence))
}

#[test]
fn doctor_reports_idle_schedules_as_recommendations_and_unenforceable_budgets() -> TestResult {
    let (_temp, registry, repository, mut doctor_evidence) = registry_with(Some(policy()?))?;
    // Without schedule evidence the budget check is a named gap.
    let unobserved = doctor(&registry, &repository, Some(&doctor_evidence))?;
    assert_eq!(
        unobserved
            .findings
            .iter()
            .map(|finding| finding.code)
            .collect::<Vec<_>>(),
        [DoctorCode::ScheduleBudget]
    );

    let day = 9 * DAY_MS;
    let idle_runs: Vec<JudgedRun> = (0..10)
        .map(|index| {
            run(
                day + index * HOUR_MS,
                RunVerdict::Idle,
                Measurement::Missing,
            )
        })
        .collect();
    doctor_evidence.schedules = Some(evidence(
        house()?,
        day + 12 * HOUR_MS,
        vec![usage("gardener", ObservedScheduleState::Active, idle_runs)?],
    ));
    let idle = doctor(&registry, &repository, Some(&doctor_evidence))?;
    assert!(idle.healthy(), "a recommendation is not a setup gap");
    let [recommendation] = idle.recommendations.as_slice() else {
        return Err("one recommendation".into());
    };
    assert_eq!(recommendation.code, DoctorCode::IdleSchedule);
    assert!(
        recommendation
            .message
            .contains("idle on 10 of its 10 recent runs")
            && recommendation.message.contains("0 tokens"),
        "{}",
        recommendation.message
    );
    assert!(idle.human_readable().contains("Recommendation:"));

    // Usage unknown for most runs makes the token budget unenforceable.
    let unknown: Vec<JudgedRun> = (0..3)
        .map(|index| {
            run(
                day + index * HOUR_MS,
                RunVerdict::Started,
                Measurement::Unavailable,
            )
        })
        .collect();
    doctor_evidence.schedules = Some(evidence(
        house()?,
        day + 12 * HOUR_MS,
        vec![usage("pickup", ObservedScheduleState::Active, unknown)?],
    ));
    let report = doctor(&registry, &repository, Some(&doctor_evidence))?;
    let [finding] = report.findings.as_slice() else {
        return Err("one finding".into());
    };
    assert_eq!(finding.code, DoctorCode::ScheduleBudget);
    assert!(
        finding.message.contains("token budget cannot be enforced")
            && finding.message.contains("unknown for 3 runs"),
        "{}",
        finding.message
    );

    // Evidence about another house is refused, not silently judged.
    doctor_evidence.schedules = Some(evidence(other_house()?, day, Vec::new()));
    assert!(matches!(
        doctor(&registry, &repository, Some(&doctor_evidence)),
        Err(HouseError::HouseSelection)
    ));
    Ok(())
}

#[test]
fn doctor_reports_a_run_budget_it_cannot_verify() -> TestResult {
    let (_temp, registry, repository, mut doctor_evidence) = registry_with(Some(policy()?))?;
    let day = 9 * DAY_MS;
    // Three agent runs among a full retained history that stops inside the
    // window: the budget of four is not shown exhausted, nor to hold.
    doctor_evidence.schedules = Some(evidence(
        house()?,
        day + 12 * HOUR_MS,
        vec![usage(
            "pickup",
            ObservedScheduleState::Active,
            capped_window(day, 3),
        )?],
    ));
    let report = doctor(&registry, &repository, Some(&doctor_evidence))?;
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.message.contains("run budget cannot be verified"))
        .ok_or("a finding for the unverifiable run budget")?;
    assert_eq!(finding.code, DoctorCode::ScheduleBudget);
    assert!(
        finding.message.contains("3 observed agent runs"),
        "{}",
        finding.message
    );

    // Once the budget is reached the exhaustion is proven; no finding.
    doctor_evidence.schedules = Some(evidence(
        house()?,
        day + 12 * HOUR_MS,
        vec![usage(
            "pickup",
            ObservedScheduleState::Paused,
            capped_window(day, 4),
        )?],
    ));
    let proven = doctor(&registry, &repository, Some(&doctor_evidence))?;
    assert!(
        !proven
            .findings
            .iter()
            .any(|finding| finding.message.contains("cannot be verified")),
        "{:?}",
        proven.findings
    );
    Ok(())
}

#[test]
fn doctor_reports_schedules_without_a_policy() -> TestResult {
    let (_temp, registry, repository, mut doctor_evidence) = registry_with(None)?;
    assert!(doctor(&registry, &repository, Some(&doctor_evidence))?.healthy());
    // An observation with no schedules needs no policy.
    doctor_evidence.schedules = Some(evidence(house()?, DAY_MS, Vec::new()));
    assert!(doctor(&registry, &repository, Some(&doctor_evidence))?.healthy());
    doctor_evidence.schedules = Some(evidence(
        house()?,
        DAY_MS,
        vec![usage("pickup", ObservedScheduleState::Active, Vec::new())?],
    ));
    let report = doctor(&registry, &repository, Some(&doctor_evidence))?;
    assert_eq!(
        report
            .findings
            .iter()
            .map(|finding| finding.code)
            .collect::<Vec<_>>(),
        [DoctorCode::ScheduleBudget]
    );
    Ok(())
}

// Adapter behavior against the simulated Orca runtime.

fn credential() -> TestResult<CredentialId> {
    Ok(CredentialId::new("orca-host-session")?)
}

fn orca_config(sim: &SimOrca) -> TestResult<OrcaConfig> {
    Ok(OrcaConfig {
        backend: orca_id()?,
        house: house()?,
        credential: credential()?,
        run: ExternalRef::new("run_sim")?,
        coordinator: ExternalRef::new("term_coordinator")?,
        repo: ExternalRef::new("id:repo-1")?,
        base_branch: Some(ExternalRef::new("main")?),
        branch_prefix: Some(BranchName::new("lemarier")?),
        agent: AgentFamily::Claude,
        call_timeout: Duration::from_secs(5),
        launch_timeout: Duration::from_secs(60),
        runtime_dir: sim.runtime_dir()?,
        reservation_timeout: Duration::from_secs(10),
    })
}

fn connect(sim: &SimOrca) -> TestResult<OrcaBackend<&SimOrca>> {
    Ok(OrcaBackend::connect(orca_config(sim)?, sim)?.with_schedule_policy(policy()?))
}

#[test]
fn orca_refuses_an_install_that_breaks_a_limit_before_creating_anything() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let refused = backend.install_schedule(&spec("pickup", "*/20 * * * *")?);
    assert!(
        matches!(
            &refused,
            Err(OrcaError::ScheduleLimit(BudgetError::IntervalTooShort {
                limit: ScheduleLimit::HouseMinInterval,
                ..
            }))
        ),
        "{refused:?}"
    );
    assert!(sim.calls_to(&["automations", "create"]).is_empty());
    // As an effect it is not applied, so it is never retried as uncertain.
    let effect = kitchen::contracts::EffectRequest::new(
        house()?,
        orca_id()?,
        credential()?,
        task_id("task-1")?,
        kitchen::contracts::AttemptNumber::FIRST,
        kitchen::contracts::IdempotencyKey::from_ref(ExternalRef::new("install-1")?),
        Effect::Schedule(ScheduleEffect::InstallDisabled {
            schedule: spec("pickup", "*/20 * * * *")?,
        }),
    );
    assert!(matches!(
        backend.execute(&effect),
        Err(kitchen::contracts::EffectFailure::NotApplied(_))
    ));

    // Within limits it installs; a third default allocation would overcommit.
    backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    backend.install_schedule(&spec("gate", "30 * * * *")?)?;
    let overcommit = backend.install_schedule(&spec("triage", "0 */2 * * *")?);
    assert!(
        matches!(
            overcommit,
            Err(OrcaError::ScheduleLimit(BudgetError::Overcommitted {
                limit: ScheduleLimit::HouseRuns,
                ..
            }))
        ),
        "{overcommit:?}"
    );
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 2);
    Ok(())
}

fn noon_on_day_twenty() -> Timestamp {
    Timestamp::from_unix_millis(20 * DAY_MS + 12 * HOUR_MS)
}

fn noon_on_day_twenty_one() -> Timestamp {
    Timestamp::from_unix_millis(21 * DAY_MS + 12 * HOUR_MS)
}

fn enabled(sim: &SimOrca, handle: &str) -> bool {
    sim.state()
        .automations
        .iter()
        .any(|automation| automation.id == handle && automation.enabled)
}

#[test]
fn orca_refuses_activating_an_exhausted_schedule_before_editing_it() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    let day = 20 * DAY_MS;
    let full = |count: u64| {
        (0..count)
            .map(|index| {
                json!({"id": format!("run-{index}"), "status": "completed",
                    "scheduledFor": day + index * 60_000})
            })
            .collect::<Vec<_>>()
    };
    sim.state().runs = full(4);

    let refused = backend.set_schedule_state(&installed, ScheduleState::Active);
    assert_eq!(
        refused,
        Err(OrcaError::ScheduleLimit(BudgetError::Exhausted {
            consumer: consumer("pickup")?,
            limit: ScheduleLimit::ScheduleRuns,
            used: 4,
            allowed: 4,
        }))
    );
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(!enabled(&sim, installed.handle.as_str()));

    // As an effect the refusal is a definite rejection, never retried.
    let activate = kitchen::contracts::EffectRequest::new(
        house()?,
        orca_id()?,
        credential()?,
        task_id("task-1")?,
        kitchen::contracts::AttemptNumber::FIRST,
        kitchen::contracts::IdempotencyKey::from_ref(ExternalRef::new("activate-1")?),
        Effect::Schedule(ScheduleEffect::SetState {
            schedule: installed.clone(),
            state: ScheduleState::Active,
        }),
    );
    assert_eq!(
        backend.execute(&activate),
        Err(kitchen::contracts::EffectFailure::NotApplied(
            kitchen::contracts::NotAppliedReason::Rejected
        ))
    );
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());

    // A run history that may hide runs is refused too.
    sim.state().runs = full(u64::try_from(kitchen::scheduling::MAX_SCHEDULE_RUNS)?)
        .into_iter()
        .map(|mut run| {
            run["status"] = json!("skipped_precheck");
            run["precheckResult"] = json!({"exitCode": 1, "timedOut": false, "error": null});
            run
        })
        .collect();
    assert!(matches!(
        backend.set_schedule_state(&installed, ScheduleState::Active),
        Err(OrcaError::ScheduleLimit(BudgetError::IncompleteEvidence {
            observed: 0,
            ..
        }))
    ));
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());

    // Headroom, or the next window, activates it.
    sim.state().runs = full(3);
    backend.set_schedule_state(&installed, ScheduleState::Active)?;
    assert!(enabled(&sim, installed.handle.as_str()));
    backend.set_schedule_state(&installed, ScheduleState::Paused)?;
    sim.state().runs = full(4);
    let next = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    next.set_schedule_state(&installed, ScheduleState::Active)?;
    assert!(enabled(&sim, installed.handle.as_str()));

    // Pausing is never refused.
    backend.set_schedule_state(&installed, ScheduleState::Paused)?;
    Ok(())
}

#[test]
fn orca_run_usage_is_observed_only_when_reported() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    sim.state().runs = vec![
        json!({"id": "run-1", "status": "completed", "scheduledFor": 3000,
            "usage": {"status": "known", "totalTokens": 3_067_427, "inputTokens": 64}}),
        json!({"id": "run-2", "status": "completed", "scheduledFor": 2000,
            "usage": {"status": "unavailable", "unavailableReason": "no_matching_session",
                "totalTokens": null}}),
        json!({"id": "run-3", "status": "completed", "scheduledFor": 1000, "usage": null}),
        json!({"status": "completed", "scheduledFor": 500,
            "usage": {"status": "known", "totalTokens": 10}}),
    ];
    let observation = backend.inspect_schedule(
        &installed,
        &Readiness::new(&[], at(0), Duration::from_secs(60)),
    )?;
    let usage: Vec<&Measurement<u64>> = observation
        .recent_runs
        .iter()
        .map(|judged| &judged.run.usage)
        .collect();
    assert_eq!(
        usage,
        [
            &Measurement::Observed {
                value: 3_067_427,
                samples: NonZeroU32::MIN,
                source: ExternalRef::new("orca-run:run-1")?,
            },
            &Measurement::Unavailable,
            &Measurement::Missing,
            // Tokens without a run to attribute them to are not an observation.
            &Measurement::Unavailable,
        ]
    );
    Ok(())
}

fn schedule_task(fixture: &Fixture) -> TestResult<(kitchen::TaskId, kitchen::contracts::Fence)> {
    let task = task_id("budget-pass")?;
    let grants = house_grants()?;
    let requested = vec![Grant::house(
        Permission::ManageSchedule,
        orca_id()?,
        credential()?,
    )];
    let spec = TaskSpec {
        id: task.clone(),
        role: Role::Expediter,
        repository: None,
        authority: TaskAuthority::delegate(&grants, requested)?,
        retry: RetryPolicy::new(3, Duration::from_secs(3600))?,
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
        requires: CapabilityRequirements::new(),
        resources: BTreeSet::new(),
        agent: None,
    };
    fixture.store.create_task(spec, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("budget-pass")?, ttl(60)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

fn house_grants() -> TestResult<HouseGrants> {
    Ok(HouseGrants::new(
        house()?,
        [Grant::house(
            Permission::ManageSchedule,
            orca_id()?,
            credential()?,
        )],
    ))
}

#[test]
fn an_exhausted_schedule_is_paused_through_the_adapter() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    // The owner activated it; four agent runs today reach its budget.
    for automation in &mut sim.state().automations {
        if automation.id == installed.handle.as_str() {
            automation.enabled = true;
        }
    }
    let day = 20 * DAY_MS;
    sim.state().runs = (0..4)
        .map(|index| {
            json!({"id": format!("run-{index}"), "status": "completed",
            "scheduledFor": day + index * HOUR_MS,
            "usage": {"status": "unavailable"}})
        })
        .collect();
    let now = Timestamp::from_unix_millis(day + 6 * HOUR_MS);
    let observation = backend.inspect_schedule(
        &installed,
        &Readiness::new(&[], now, Duration::from_secs(600)),
    )?;
    assert_eq!(observation.state, ObservedScheduleState::Active);
    let observed = ScheduleEvidence {
        house: house()?,
        observed_at: now,
        schedules: vec![ScheduleUsage {
            consumer: consumer("pickup")?,
            schedule: installed.clone(),
            observation,
        }],
    };
    let policy = backend.schedule_policy().ok_or("policy")?;
    let [exhausted] = policy
        .plan_exhaustion(&house()?, &observed, |_| false)?
        .try_into()
        .map_err(|_| "one exhaustion")?;
    let (task, fence) = schedule_task(&fixture)?;
    let clock = ManualClock::starting_at(1);
    let record = run_effect(
        &fixture.store,
        &backend,
        &house_grants()?,
        EffectPlan {
            task,
            fence,
            name: EffectName::new("pause-pickup")?,
            decided_at: EvidenceRevision::INITIAL,
            effect: exhausted.pause_effect().ok_or("a pause")?,
            consent: None,
        },
        &clock,
    )?;
    assert!(matches!(record.state(), EffectState::Applied { .. }));
    let paused = backend.installed_schedules()?;
    assert_eq!(
        paused
            .iter()
            .map(|schedule| schedule.state)
            .collect::<Vec<_>>(),
        [ObservedScheduleState::Paused]
    );
    Ok(())
}
