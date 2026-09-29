//! Schedule intervals and usage budgets per house (#40), and the budget
//! pass that applies them to live schedules (#106).
//!
//! Policy decisions run on constructed observations. Adapter behavior runs
//! against the simulated Orca runtime (`orca_sim`); none of this is live
//! runtime evidence.

mod common;
mod orca_sim;

use std::{cell::RefCell, collections::BTreeSet, num::NonZeroU32, time::Duration};

use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, house, other_house, scheduled, task_id,
    ttl,
};
use kitchen::selection::{AgentSelection, ResolvedSelection};
use kitchen::{
    BackendId, ConsumerId, CredentialId, EffectName, HouseId,
    adapters::orca::{OrcaBackend, OrcaConfig, OrcaError},
    adoption::{HouseRegistry, InstructionBundle},
    contracts::{
        BackendDescriptor, BackendUnavailable, BranchName, Capability, CapabilityRequirements,
        CapabilitySet, Clock, ContractError, Effect, EffectExecutor, EffectFailure, EffectRequest,
        EvidenceRevision, ExternalRef, GitHubAction, GitHubEffect, GitHubMutation, Grant,
        HouseGrants, IssueNumber, Lookup, Permission, PostingBudget, Provenance, Receipt,
        Repository, ResourceKind, ResourceRef, RetryPolicy, Role, ScheduleBackend, ScheduleEffect,
        TaskAuthority, TaskSpec, Text, Timestamp, UncertainReason,
        fake::{ExecuteFault, FakeBackend},
    },
    house::{
        AccessStatus, DoctorCode, DoctorEvidence, HouseConfig, HouseError, RepositoryConfig,
        RepositoryLabel, Workflow, doctor,
    },
    scheduling::{
        AgentFamily, Budget, BudgetError, BudgetExhaustion, CronExpr, Exhausted, InstalledSchedule,
        IntervalMinutes, JudgedRun, ObservedScheduleState, Readiness, Recurrence, RunOutcome,
        RunVerdict, ScheduleEvidence, ScheduleLimit, ScheduleLimits, ScheduleObservation,
        SchedulePolicy, ScheduleRun, ScheduleSpec, ScheduleState, ScheduleUsage, Timezone,
        TokenUsage, UndeliveredReport, WindowHours, WorkflowName,
    },
    state::{EffectPlan, EffectState, StateError, TaskState, run_effect},
    trust::Measurement,
    workflows::{
        Precheck, WorkflowError,
        budget::{
            self, BudgetPass, Delivery, PassAction, PassClaim, ReportChannel, Tick, TickArgs,
            TickCommand, TickReport,
        },
    },
};
use orca_sim::{Fault, SimOrca};
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
        ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Claude)),
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
            agent: None,
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
            requires: None,
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
fn the_budget_schedule_counts_toward_budgets_like_any_other() -> TestResult {
    let policy = policy()?;
    let house = house()?;
    let day = 100 * DAY_MS;
    let noon = day + 12 * HOUR_MS;
    // Two pickups of three runs and the tick's four: ten in all is the house
    // budget, and four is the tick's default schedule budget.
    let all = evidence(
        house.clone(),
        noon,
        vec![
            usage(
                "pickup",
                ObservedScheduleState::Active,
                started(day, 3, 10)?,
            )?,
            usage(
                "triage",
                ObservedScheduleState::Active,
                started(day, 3, 10)?,
            )?,
            usage(
                "budget",
                ObservedScheduleState::Active,
                started(day, 4, 10)?,
            )?,
        ],
    );
    let assessment = policy.assess(&house, &all)?;
    assert_eq!(assessment.house.runs, 10, "the tick's runs are the house's");
    assert_eq!(
        assessment.house_exhausted.map(|exhausted| exhausted.limit),
        Some(ScheduleLimit::HouseRuns)
    );
    let planned = policy.plan_exhaustion(&house, &all, |_| false)?;
    let consumers: Vec<String> = planned
        .iter()
        .map(|item| item.consumer.to_string())
        .collect();
    assert_eq!(consumers, ["pickup", "triage", "budget"]);
    assert!(matches!(
        policy.check_activation(&house, &all, &consumer("budget")?),
        Err(BudgetError::Exhausted { .. })
    ));

    // Within the house budget, the tick exhausts its own budget alone.
    let alone = evidence(
        house.clone(),
        noon,
        vec![
            usage(
                "pickup",
                ObservedScheduleState::Active,
                started(day, 1, 10)?,
            )?,
            usage(
                "budget",
                ObservedScheduleState::Active,
                started(day, 4, 10)?,
            )?,
        ],
    );
    let planned = policy.plan_exhaustion(&house, &alone, |_| false)?;
    let [only] = planned.as_slice() else {
        return Err(format!("expected the tick alone, got {planned:?}").into());
    };
    assert_eq!(only.consumer.as_str(), "budget");
    assert_eq!(only.exhausted.limit, ScheduleLimit::ScheduleRuns);
    Ok(())
}

/// Arguments for the house's budget schedule in these tests.
fn tick_args() -> TestResult<TickArgs> {
    Ok(TickArgs {
        kitchen: "/opt/kitchen/bin/kitchen".into(),
        registry: "/var/kitchen/registry".into(),
        house: house()?,
        store: "/var/kitchen/store".into(),
        orca: "/opt/orca/bin/orca".into(),
        backend: orca_id()?,
        credential: credential()?,
        runtime_dir: "/var/kitchen/orca".into(),
        report: None,
    })
}

fn budget_tick(cron: &str) -> TestResult<ScheduleSpec> {
    Ok(budget::install(
        Recurrence::Cron(CronExpr::new(cron)?),
        Timezone::new("America/Toronto")?,
        ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Claude)),
        &tick_args()?,
    )?)
}

#[test]
fn the_budget_schedule_holds_an_allocation_and_keeps_the_interval() -> TestResult {
    let policy = policy()?;
    // Two 4-run schedules leave 2 of the 10 house runs: the tick's default
    // allocation of 4 does not fit, as for any other schedule.
    let two = [
        installed("pickup", ObservedScheduleState::Paused)?,
        installed("triage", ObservedScheduleState::Paused)?,
    ];
    assert!(matches!(
        policy.check_install(&budget_tick("15 * * * *")?, &two),
        Err(BudgetError::Overcommitted { .. })
    ));
    policy.check_install(&budget_tick("15 * * * *")?, &two[..1])?;
    assert!(matches!(
        policy.check_install(&budget_tick("*/5 * * * *")?, &[]),
        Err(BudgetError::IntervalTooShort { .. })
    ));
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
        schema: 2,
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
        stack_tool: None,
        schedules: None,
        readiness: None,
        undelivered_budget_reports: Vec::new(),
        store_capacity: None,
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
fn doctor_reports_an_exhaustion_whose_owner_was_never_told() -> TestResult {
    let (_temp, registry, repository, mut doctor_evidence) = registry_with(Some(policy()?))?;
    let day = 9 * DAY_MS;
    doctor_evidence.schedules = Some(evidence(
        house()?,
        day + 12 * HOUR_MS,
        vec![usage(
            "gardener",
            ObservedScheduleState::Paused,
            started(day, 1, 10)?,
        )?],
    ));
    let quiet = doctor(&registry, &repository, Some(&doctor_evidence))?;
    assert!(quiet.healthy());

    let window = policy()?
        .window_hours
        .containing(Timestamp::from_unix_millis(day));
    doctor_evidence.undelivered_budget_reports = vec![UndeliveredReport {
        consumer: consumer("pickup")?,
        window,
        exhausted: Exhausted {
            limit: ScheduleLimit::ScheduleRuns,
            used: 4,
            allowed: 4,
        },
    }];
    let told = doctor(&registry, &repository, Some(&doctor_evidence))?;
    assert!(!told.healthy(), "an unreported pause is a setup gap");
    let [finding] = told
        .findings
        .iter()
        .filter(|finding| finding.code == DoctorCode::BudgetReport)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| "one budget report finding")?;
    assert!(finding.message.contains("pickup"), "{}", finding.message);
    assert!(finding.next_step.contains("--report-issue"));
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
            schedule: spec("pickup", "*/20 * * * *")?.into(),
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

fn one_pm_on_day_twenty() -> Timestamp {
    Timestamp::from_unix_millis(20 * DAY_MS + 13 * HOUR_MS)
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
            requires: Some(BTreeSet::new()),
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

    // With headroom, or in the next window, the budget allows it; Kitchen
    // defines no scheduled `pickup` workflow, so its requirements are unknown
    // and it is still not activated.
    sim.state().runs = full(3);
    assert_eq!(
        backend.set_schedule_state(&installed, ScheduleState::Active),
        Err(OrcaError::ScheduleRequirementsUnknown)
    );
    sim.state().runs = full(4);
    let next = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    assert_eq!(
        next.set_schedule_state(&installed, ScheduleState::Active),
        Err(OrcaError::ScheduleRequirementsUnknown)
    );
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(!enabled(&sim, installed.handle.as_str()));

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
        work_type: None,
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
            basis: None,
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

// --- The budget pass on live schedules (#106), against the simulator. ---

fn enable(sim: &SimOrca, schedule: &ResourceRef) {
    for automation in &mut sim.state().automations {
        if automation.id == schedule.handle.as_str() {
            automation.enabled = true;
        }
    }
}

/// `count` completed agent runs on day twenty, one per hour from midnight.
fn day_twenty_runs(count: u64) -> Vec<serde_json::Value> {
    runs_on_day(20, count)
}

/// `count` completed agent runs on `day`, one per hour from midnight.
fn runs_on_day(day: u64, count: u64) -> Vec<serde_json::Value> {
    let day = day * DAY_MS;
    (0..count)
        .map(|index| {
            json!({"id": format!("run-{index}"), "status": "completed",
                "scheduledFor": day + index * HOUR_MS})
        })
        .collect()
}

fn activations(sim: &SimOrca) -> usize {
    sim.calls_to(&["automations", "edit"])
        .iter()
        .filter(|call| call.iter().any(|arg| arg == "--enabled"))
        .count()
}

#[test]
fn the_budget_pass_pauses_an_exhausted_live_schedule_and_reports_once() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    // The owner activated it; four agent runs today reach its budget.
    enable(&sim, &installed);
    sim.state().runs = day_twenty_runs(4);
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &evidence)?,
        Precheck::Actionable
    );

    let (task, fence) = schedule_task(&fixture)?;
    let grants = house_grants()?;
    let claim = PassClaim {
        task: &task,
        fence,
        grants: &grants,
    };
    let clock = ManualClock::starting_at(1);
    let pass = budget::run(&fixture.store, &backend, claim, &policy, &evidence, &clock)?;
    let BudgetPass::Acted(actions) = pass else {
        return Err("an exhausted schedule is not idle".into());
    };
    let [PassAction::Report(exhausted)] = actions.as_slice() else {
        return Err(format!("expected one report, got {actions:?}").into());
    };
    assert!(!enabled(&sim, installed.handle.as_str()), "paused on Orca");
    assert_eq!(exhausted.exhausted.limit, ScheduleLimit::ScheduleRuns);
    assert!(exhausted.report().starts_with("Paused schedule pickup"));
    let edits = sim.calls_to(&["automations", "edit"]).len();

    // The same observation again reuses the recorded pause, and the report
    // stays due until its delivery is confirmed.
    let again = budget::run(&fixture.store, &backend, claim, &policy, &evidence, &clock)?;
    assert_eq!(again, BudgetPass::Acted(actions.clone()));
    assert_eq!(sim.calls_to(&["automations", "edit"]).len(), edits);
    let unreported = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &unreported)?,
        Precheck::Actionable
    );

    budget::confirm_reported(&fixture.store, &scheduled("budget-pass")?, exhausted, at(2))?;
    budget::confirm_reported(&fixture.store, &scheduled("budget-pass")?, exhausted, at(3))?;
    let settled = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &settled)?,
        Precheck::Idle
    );
    assert_eq!(
        budget::run(&fixture.store, &backend, claim, &policy, &settled, &clock)?,
        BudgetPass::Idle
    );
    assert_eq!(activations(&sim), 0, "the pass never activates a schedule");
    Ok(())
}

#[test]
fn a_budget_pass_with_no_exhausted_schedule_stays_idle() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    // Three of four runs used: within budget.
    sim.state().runs = day_twenty_runs(3);
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &evidence)?,
        Precheck::Idle
    );
    let (task, fence) = schedule_task(&fixture)?;
    let grants = house_grants()?;
    let claim = PassClaim {
        task: &task,
        fence,
        grants: &grants,
    };
    let clock = ManualClock::starting_at(1);
    assert_eq!(
        budget::run(&fixture.store, &backend, claim, &policy, &evidence, &clock)?,
        BudgetPass::Idle
    );
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(enabled(&sim, installed.handle.as_str()));
    // No schedules at all is idle as well.
    let empty = SimOrca::default();
    let none = connect(&empty)?.with_clock(noon_on_day_twenty);
    assert_eq!(
        budget::precheck(
            &fixture.store,
            &house()?,
            &policy,
            &none.schedule_evidence()?
        )?,
        Precheck::Idle
    );
    Ok(())
}

#[test]
fn a_pause_that_does_not_apply_is_not_reported_and_the_next_pass_recovers() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    sim.state().runs = day_twenty_runs(4);
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;
    // The owner removes the schedule after it was observed.
    sim.state().automations.clear();

    let (task, fence) = schedule_task(&fixture)?;
    let grants = house_grants()?;
    let claim = PassClaim {
        task: &task,
        fence,
        grants: &grants,
    };
    let clock = ManualClock::starting_at(1);
    let pass = budget::run(&fixture.store, &backend, claim, &policy, &evidence, &clock)?;
    let BudgetPass::Acted(actions) = pass else {
        return Err("the stale observation is still exhausted".into());
    };
    let [PassAction::PauseNotApplied { exhaustion, record }] = actions.as_slice() else {
        return Err(format!("expected an unapplied pause, got {actions:?}").into());
    };
    assert!(matches!(record.state(), EffectState::NotApplied { .. }));
    assert_eq!(
        fixture.store.marker(&exhaustion.marker_key()?)?,
        None,
        "nothing is recorded as reported"
    );
    // The next observation no longer lists the schedule: nothing to do.
    let next = backend.schedule_evidence()?;
    assert!(next.schedules.is_empty());
    assert_eq!(
        budget::run(&fixture.store, &backend, claim, &policy, &next, &clock)?,
        BudgetPass::Idle
    );
    Ok(())
}

#[test]
fn a_budget_pass_refuses_another_house_or_missing_authority() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    sim.state().runs = day_twenty_runs(4);
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;

    // Evidence observed for this house is never judged for another.
    assert!(matches!(
        budget::precheck(&fixture.store, &other_house()?, &policy, &evidence),
        Err(kitchen::Error::Budget(_))
    ));

    // Without a standing schedule grant the pause is refused before Orca.
    let (task, fence) = schedule_task(&fixture)?;
    let without = HouseGrants::new(house()?, []);
    let claim = PassClaim {
        task: &task,
        fence,
        grants: &without,
    };
    let clock = ManualClock::starting_at(1);
    let refused = budget::run(&fixture.store, &backend, claim, &policy, &evidence, &clock);
    assert!(matches!(
        refused,
        Err(kitchen::Error::Contract(
            kitchen::contracts::ContractError::AuthorityExpansion {
                permission: Permission::ManageSchedule,
                ..
            }
        ))
    ));
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(enabled(&sim, installed.handle.as_str()));
    Ok(())
}

#[test]
fn reordered_evidence_at_one_instant_pauses_each_schedule_under_its_own_effect() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let pickup = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    let gate = backend.install_schedule(&spec("gate", "30 * * * *")?)?;
    enable(&sim, &pickup);
    enable(&sim, &gate);
    sim.state().runs = day_twenty_runs(4);
    let policy = policy()?;
    let observed = backend.schedule_evidence()?;
    // Two observations taken in the same millisecond: the first lists only
    // pickup, the second lists gate first.
    let mut first = observed.clone();
    first.schedules.retain(|usage| usage.schedule == pickup);
    let mut second = observed;
    second.schedules.sort_by_key(|usage| usage.schedule != gate);
    assert_eq!(first.observed_at, second.observed_at);
    assert_eq!(
        second.schedules.first().map(|usage| &usage.schedule),
        Some(&gate)
    );

    let (task, fence) = schedule_task(&fixture)?;
    let grants = house_grants()?;
    let claim = PassClaim {
        task: &task,
        fence,
        grants: &grants,
    };
    let clock = ManualClock::starting_at(1);
    budget::run(&fixture.store, &backend, claim, &policy, &first, &clock)?;
    assert!(!enabled(&sim, pickup.handle.as_str()));
    assert!(enabled(&sim, gate.handle.as_str()));

    let pass = budget::run(&fixture.store, &backend, claim, &policy, &second, &clock)?;
    let BudgetPass::Acted(actions) = pass else {
        return Err("both schedules are exhausted".into());
    };
    let reported: Vec<&ResourceRef> = actions
        .iter()
        .map(|action| match action {
            PassAction::Report(exhaustion) => Ok(&exhaustion.schedule),
            other => Err(format!("expected reports, got {other:?}")),
        })
        .collect::<Result<_, _>>()?;
    assert_eq!(reported, [&gate, &pickup]);
    assert!(!enabled(&sim, gate.handle.as_str()), "gate paused too");
    assert!(!enabled(&sim, pickup.handle.as_str()));
    Ok(())
}

#[test]
fn a_schedule_reactivated_after_it_was_observed_paused_is_paused_before_reporting() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    // Exhausted and paused, with this window's report still due.
    sim.state().runs = day_twenty_runs(4);
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;
    let [usage] = evidence.schedules.as_slice() else {
        return Err("one schedule".into());
    };
    assert_eq!(usage.observation.state, ObservedScheduleState::Paused);
    // The owner re-activates it between the observation and the pass.
    enable(&sim, &installed);

    let (task, fence) = schedule_task(&fixture)?;
    let grants = house_grants()?;
    let claim = PassClaim {
        task: &task,
        fence,
        grants: &grants,
    };
    let clock = ManualClock::starting_at(1);
    let pass = budget::run(&fixture.store, &backend, claim, &policy, &evidence, &clock)?;
    let BudgetPass::Acted(actions) = pass else {
        return Err("an unreported exhaustion is not idle".into());
    };
    let [PassAction::Report(exhaustion)] = actions.as_slice() else {
        return Err(format!("expected one report, got {actions:?}").into());
    };
    assert!(
        !enabled(&sim, installed.handle.as_str()),
        "the report is true when it is delivered"
    );
    assert_eq!(exhaustion.schedule, installed);
    assert_eq!(activations(&sim), 0);
    Ok(())
}

// --- The budget tick (#134): claim, pass, report, against the simulator. ---

fn github_id() -> TestResult<BackendId> {
    Ok(BackendId::new("github")?)
}

fn report_repository() -> TestResult<Repository> {
    Ok(Repository::new("sample/ops")?)
}

fn comment_grant() -> TestResult<Grant> {
    Ok(Grant::repository(
        Permission::PostComment,
        report_repository()?,
        github_id()?,
        CredentialId::new("github-bot")?,
    ))
}

fn tick_grants() -> TestResult<HouseGrants> {
    Ok(HouseGrants::new(
        house()?,
        [
            Grant::house(Permission::ManageSchedule, orca_id()?, credential()?),
            comment_grant()?,
        ],
    ))
}

fn report_effect(exhaustion: &BudgetExhaustion) -> kitchen::Result<Effect> {
    Ok(Effect::GitHub(GitHubEffect {
        requester: ExternalRef::new("kitchen-bot")?,
        mutation: GitHubMutation {
            repository: Repository::new("sample/ops")?,
            action: GitHubAction::PostComment {
                issue: IssueNumber::new(7)?,
                body: Text::new(&exhaustion.report())?,
            },
        },
        posting_budget: PostingBudget::new(100)?,
    }))
}

fn reporter() -> TestResult<FakeBackend> {
    Ok(FakeBackend::fully_capable(github_id()?, house()?))
}

fn tick_with<'a>(
    fixture: &'a Fixture,
    backend: &'a OrcaBackend<&'a SimOrca>,
    reports: Option<ReportChannel<'a>>,
    grants: &'a HouseGrants,
    claimant: &'a kitchen::contracts::Claimant,
    clock: &'a ManualClock,
) -> TestResult<Tick<'a>> {
    let mut authority = vec![Grant::house(
        Permission::ManageSchedule,
        orca_id()?,
        credential()?,
    )];
    authority.push(comment_grant()?);
    Ok(Tick {
        store: &fixture.store,
        schedules: backend,
        reports,
        grants,
        authority,
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
        claimant,
        ttl: ttl(300)?,
        clock,
    })
}

fn exhausted_pickup(sim: &SimOrca, backend: &OrcaBackend<&SimOrca>) -> TestResult<ResourceRef> {
    let installed = backend.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(sim, &installed);
    sim.state().runs = day_twenty_runs(4);
    Ok(installed)
}

#[test]
fn a_budget_tick_pauses_posts_the_report_once_and_then_idles() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let policy = policy()?;
    let reporter = reporter()?;
    let channel = ReportChannel {
        executor: &reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(
        &fixture,
        &backend,
        Some(channel),
        &grants,
        &claimant,
        &clock,
    )?;

    let report = budget::tick(&tick, &policy, &backend.schedule_evidence()?)?;
    let [Delivery::Delivered(delivered)] = report.deliveries.as_slice() else {
        return Err(format!("expected one delivered report, got {report:?}").into());
    };
    assert_eq!(delivered.schedule, installed);
    assert!(!enabled(&sim, installed.handle.as_str()), "paused on Orca");
    assert_eq!(reporter.effects_performed(), 1, "one comment posted");
    assert!(fixture.store.marker(&delivered.marker_key()?)?.is_some());
    // The window's task is given back for the next tick.
    let task = fixture.store.task(&task_id(&format!(
        "budget-{}-24h",
        delivered.window.start.as_unix_millis()
    ))?)?;
    assert!(matches!(task.state(), TaskState::Open));

    // The next tick finds the schedule paused and reported: idle, no post.
    let next = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &next)?,
        Precheck::Idle
    );
    let idle = budget::tick(&tick, &policy, &next)?;
    assert_eq!(idle.pass, BudgetPass::Idle);
    assert!(idle.deliveries.is_empty());
    assert_eq!(reporter.effects_performed(), 1);
    assert_eq!(activations(&sim), 0, "the tick never activates a schedule");
    Ok(())
}

#[test]
fn an_undeliverable_report_is_recorded_once_per_window_and_the_precheck_goes_idle() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let policy = policy()?;
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(&fixture, &backend, None, &grants, &claimant, &clock)?;

    let report = budget::tick(&tick, &policy, &backend.schedule_evidence()?)?;
    let [Delivery::Undeliverable(exhaustion)] = report.deliveries.as_slice() else {
        return Err(format!("expected an undeliverable report, got {report:?}").into());
    };
    assert!(!enabled(&sim, installed.handle.as_str()), "paused anyway");
    assert_eq!(
        fixture.store.marker(&exhaustion.marker_key()?)?,
        None,
        "it was not reported"
    );
    assert!(
        fixture
            .store
            .marker(&exhaustion.undeliverable_key()?)?
            .is_some(),
        "the window's undeliverable report is recorded"
    );
    let undelivered = budget::undelivered_reports(&fixture.store)?;
    assert_eq!(undelivered.len(), 1);
    assert_eq!(
        undelivered.first().map(|r| &r.consumer),
        Some(&exhaustion.consumer)
    );

    // Later ticks in the window neither start an agent nor repeat the line.
    let evidence = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &evidence)?,
        Precheck::Idle
    );
    let again = budget::tick(&tick, &policy, &evidence)?;
    assert_eq!(again.pass, BudgetPass::Idle);
    assert!(again.deliveries.is_empty());
    assert_eq!(budget::undelivered_reports(&fixture.store)?.len(), 1);

    // The owner re-activating the schedule is still paused again, without
    // a second undeliverable report.
    enable(&sim, &installed);
    let an_hour_later = connect(&sim)?.with_clock(one_pm_on_day_twenty);
    let evidence = an_hour_later.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &evidence)?,
        Precheck::Actionable,
        "an active exhausted schedule still needs its pause"
    );
    let repaused = budget::tick(&tick, &policy, &evidence)?;
    assert!(repaused.deliveries.is_empty());
    assert!(!enabled(&sim, installed.handle.as_str()));

    // A new window is a new exhaustion: due again, and undeliverable again.
    let next_day = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    enable(&sim, &installed);
    sim.state().runs = (0..4)
        .map(|index| {
            json!({"id": format!("next-{index}"), "status": "completed",
                "scheduledFor": 21 * DAY_MS + index * HOUR_MS})
        })
        .collect();
    let evidence = next_day.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &evidence)?,
        Precheck::Actionable
    );
    let later = budget::tick(&tick, &policy, &evidence)?;
    assert!(matches!(
        later.deliveries.as_slice(),
        [Delivery::Undeliverable(_)]
    ));
    assert_eq!(budget::undelivered_reports(&fixture.store)?.len(), 2);
    Ok(())
}

/// Reports go through `inner` and note, at each post, whether the pickup and
/// the budget schedule were still enabled on Orca.
struct PostSpy<'a> {
    inner: &'a FakeBackend,
    sim: &'a SimOrca,
    watched: [String; 2],
    seen: RefCell<Vec<[bool; 2]>>,
}

impl EffectExecutor for PostSpy<'_> {
    fn descriptor(&self) -> &BackendDescriptor {
        self.inner.descriptor()
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let [first, second] = &self.watched;
        self.seen
            .borrow_mut()
            .push([enabled(self.sim, first), enabled(self.sim, second)]);
        self.inner.execute(request)
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(request)
    }
}

/// The budget schedule installed as any schedule is, and enabled beside an
/// exhausted pickup. Every schedule the sim lists shows the same four runs,
/// so both exhaust their default budget. Returns the pickup and the tick.
fn budget_beside_pickup(
    sim: &SimOrca,
    backend: &OrcaBackend<&SimOrca>,
) -> TestResult<(ResourceRef, ResourceRef)> {
    let pickup = exhausted_pickup(sim, backend)?;
    // Orca refuses to install the tick (it lacks the tick's required
    // capabilities), so its automation is placed in Orca directly.
    let own = direct_budget_automation(sim)?;
    Ok((pickup, own))
}

#[test]
fn at_exhaustion_the_tick_reports_then_pauses_every_exhausted_schedule_including_itself()
-> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let (pickup, own) = budget_beside_pickup(&sim, &backend)?;
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;
    let assessment = policy.assess(&house()?, &evidence)?;
    assert!(
        assessment
            .schedules
            .iter()
            .all(|schedule| schedule.exhausted.is_some()),
        "the tick's runs count like the pickup's"
    );

    let inner = reporter()?;
    let spy = PostSpy {
        inner: &inner,
        sim: &sim,
        watched: [
            pickup.handle.as_str().to_owned(),
            own.handle.as_str().to_owned(),
        ],
        seen: RefCell::new(Vec::new()),
    };
    let channel = ReportChannel {
        executor: &spy,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(
        &fixture,
        &backend,
        Some(channel),
        &grants,
        &claimant,
        &clock,
    )?;

    let report = budget::tick(&tick, &policy, &evidence)?;
    let BudgetPass::Acted(actions) = &report.pass else {
        return Err(format!("expected the pass to act, got {report:?}").into());
    };
    let acted: Vec<&str> = actions
        .iter()
        .map(|action| match action {
            PassAction::Report(exhaustion) | PassAction::Repaused(exhaustion) => {
                exhaustion.consumer.as_str()
            }
            PassAction::PauseNotApplied { exhaustion, .. } => exhaustion.consumer.as_str(),
        })
        .collect();
    assert_eq!(acted, ["pickup", "budget"], "the tick's own pause is last");
    assert!(
        report
            .deliveries
            .iter()
            .all(|delivery| matches!(delivery, Delivery::Delivered(_))),
        "{report:?}"
    );
    assert_eq!(report.deliveries.len(), 2);
    // The pickup is already paused when its report posts; the tick is still
    // running when its own report posts, and paused after it.
    assert_eq!(*spy.seen.borrow(), [[false, true], [false, true]]);
    assert_eq!(inner.effects_performed(), 2);
    assert!(!enabled(&sim, pickup.handle.as_str()));
    assert!(
        !enabled(&sim, own.handle.as_str()),
        "the tick paused itself"
    );
    assert_eq!(activations(&sim), 0, "the pass never unpauses anything");

    // Both are reported and paused: the next pass has nothing to do.
    let next = backend.schedule_evidence()?;
    assert_eq!(
        budget::precheck(&fixture.store, &house()?, &policy, &next)?,
        Precheck::Idle
    );
    Ok(())
}

#[test]
fn a_tick_whose_own_report_is_refused_still_pauses_itself() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    sim.state().runs = day_twenty_runs(4);
    let own = direct_budget_automation(&sim)?;
    let reporter = reporter()?;
    reporter.inject(ExecuteFault::Reject);
    let channel = ReportChannel {
        executor: &reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(
        &fixture,
        &backend,
        Some(channel),
        &grants,
        &claimant,
        &clock,
    )?;

    let report = budget::tick(&tick, &policy()?, &backend.schedule_evidence()?)?;
    let [Delivery::NotDelivered { record, .. }] = report.deliveries.as_slice() else {
        return Err(format!("expected a refused post, got {report:?}").into());
    };
    assert!(matches!(record.state(), EffectState::NotApplied { .. }));
    assert!(
        !enabled(&sim, own.handle.as_str()),
        "the tick stops spending even though its report did not post"
    );
    assert_eq!(activations(&sim), 0);
    Ok(())
}

#[test]
fn an_uncertain_report_is_looked_up_by_the_next_tick_and_never_posted_twice() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    exhausted_pickup(&sim, &backend)?;
    let policy = policy()?;
    let reporter = reporter()?;
    reporter.inject(ExecuteFault::ApplyThenLoseResponse);
    let channel = ReportChannel {
        executor: &reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(
        &fixture,
        &backend,
        Some(channel),
        &grants,
        &claimant,
        &clock,
    )?;

    let first = budget::tick(&tick, &policy, &backend.schedule_evidence()?)?;
    let [Delivery::NotDelivered { exhaustion, record }] = first.deliveries.as_slice() else {
        return Err(format!("expected an uncertain post, got {first:?}").into());
    };
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    assert_eq!(
        fixture.store.marker(&exhaustion.marker_key()?)?,
        None,
        "an uncertain post is never recorded as delivered"
    );

    // Another run lands before the next tick; the retried report is the same.
    sim.state().runs = day_twenty_runs(5);
    clock.advance(60);
    let second = budget::tick(&tick, &policy, &backend.schedule_evidence()?)?;
    assert!(
        matches!(second.deliveries.as_slice(), [Delivery::Delivered(_)]),
        "{second:?}"
    );
    assert_eq!(
        reporter.effects_performed(),
        1,
        "found by lookup, not reposted"
    );
    assert!(fixture.store.marker(&exhaustion.marker_key()?)?.is_some());
    Ok(())
}

#[test]
fn a_budget_tick_refuses_while_another_tick_holds_the_window() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let policy = policy()?;
    let grants = tick_grants()?;
    let clock = ManualClock::starting_at(1);
    let evidence = backend.schedule_evidence()?;
    let other = scheduled("budget-other")?;
    let holder = tick_with(&fixture, &backend, None, &grants, &other, &clock)?;
    // Another tick created and claimed this window's task and is still running.
    budget::tick(&holder, &policy, &evidence)?;
    let window = evidence_window(&fixture, &policy, &evidence)?;
    let task = task_id(&format!("budget-{window}-24h"))?;
    fixture.store.claim(&task, &other, ttl(300)?, clock.now())?;
    enable(&sim, &installed);
    let edits = sim.calls_to(&["automations", "edit"]).len();

    let claimant = scheduled("budget-tick")?;
    let tick = tick_with(&fixture, &backend, None, &grants, &claimant, &clock)?;
    let refused = budget::tick(&tick, &policy, &backend.schedule_evidence()?);
    assert!(
        matches!(
            refused,
            Err(kitchen::Error::State(StateError::ClaimHeld { .. }))
        ),
        "{refused:?}"
    );
    assert_eq!(sim.calls_to(&["automations", "edit"]).len(), edits);
    Ok(())
}

/// The start of the budget window `evidence` falls in, in Unix ms.
fn evidence_window(
    fixture: &Fixture,
    policy: &SchedulePolicy,
    evidence: &ScheduleEvidence,
) -> TestResult<u64> {
    let _ = fixture;
    let assessment = policy.assess(&house()?, evidence)?;
    Ok(assessment.window.start.as_unix_millis())
}

#[test]
fn a_tick_in_a_later_window_settles_the_earlier_windows_task() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let policy = policy()?;
    let reporter = reporter()?;
    let channel = ReportChannel {
        executor: &reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(
        &fixture,
        &backend,
        Some(channel),
        &grants,
        &claimant,
        &clock,
    )?;
    let first = budget::tick(&tick, &policy, &backend.schedule_evidence()?)?;
    let [Delivery::Delivered(earlier)] = first.deliveries.as_slice() else {
        return Err(format!("expected a delivered report, got {first:?}").into());
    };
    let earlier_task = task_id(&format!(
        "budget-{}-24h",
        earlier.window.start.as_unix_millis()
    ))?;

    // Next day: the owner re-activated it and it ran out again.
    let next_day = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    enable(&sim, &installed);
    sim.state().runs = (0..4)
        .map(|index| {
            json!({"id": format!("next-{index}"), "status": "completed",
                "scheduledFor": 21 * DAY_MS + index * HOUR_MS})
        })
        .collect();
    clock.advance(DAY_MS / 1000);
    let later = budget::tick(&tick, &policy, &next_day.schedule_evidence()?)?;
    assert_eq!(later.settled, std::slice::from_ref(&earlier_task));
    assert!(matches!(
        fixture.store.task(&earlier_task)?.state(),
        TaskState::Settled { .. }
    ));
    assert!(matches!(
        later.deliveries.as_slice(),
        [Delivery::Delivered(_)]
    ));
    assert!(!enabled(&sim, installed.handle.as_str()));
    assert_eq!(reporter.effects_performed(), 2, "one report per window");
    Ok(())
}

#[test]
fn orca_refuses_the_budget_schedule_naming_the_capabilities_it_lacks() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let args = tick_args()?;
    let precheck = args.argv(TickCommand::Precheck)?;
    let precheck: Vec<&str> = precheck.iter().map(Text::as_str).collect();
    assert_eq!(
        precheck.get(..3),
        Some(["/opt/kitchen/bin/kitchen", "budget", "precheck"].as_slice())
    );
    let tick = budget_tick("15 * * * *")?;
    assert_eq!(
        tick.requires(),
        Some(&BTreeSet::from(budget::REQUIRED_CAPABILITIES))
    );
    // Orca cannot prevent overlapping runs or enforce a run timeout, and
    // tells a failed precheck from an idle one only after the fact.
    assert_eq!(
        backend.install_schedule(&tick),
        Err(OrcaError::Contract(
            ContractError::UnsupportedCapabilities {
                missing: vec![
                    Capability::ScheduleSingleConsumer,
                    Capability::ScheduleRunTimeout,
                ],
                partial: vec![Capability::SchedulePrecheck],
            }
        ))
    );
    // Refused before anything is read, reserved, or created.
    assert!(sim.calls_to(&["automations"]).is_empty());

    // A relative path cannot be recorded in a schedule.
    let relative = TickArgs {
        store: "store".into(),
        ..args
    };
    assert!(matches!(
        relative.argv(TickCommand::Run),
        Err(kitchen::Error::Workflow(WorkflowError::IncompleteEvidence))
    ));
    Ok(())
}

/// An automation named for the house's budget schedule, created directly in
/// Orca with its own prompt and no precheck, enabled.
fn direct_budget_automation(sim: &SimOrca) -> TestResult<ResourceRef> {
    let flags = [
        ("name", format!("kitchen:{}:budget", house()?)),
        ("prompt", "Refactor the whole repository.".to_owned()),
        ("provider", "claude".to_owned()),
        ("timezone", "UTC".to_owned()),
        ("trigger", "15 * * * *".to_owned()),
    ];
    sim.state().automations.push(orca_sim::SimAutomation {
        id: "auto-direct".into(),
        name: format!("kitchen:{}:budget", house()?),
        enabled: true,
        flags: flags
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    });
    schedule_ref("auto-direct")
}

#[test]
fn an_orca_automation_named_budget_is_budgeted_like_any_schedule() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let direct = direct_budget_automation(&sim)?;
    sim.state().runs = day_twenty_runs(4);
    let policy = policy()?;
    let evidence = backend.schedule_evidence()?;
    let plan = policy.plan_exhaustion(&house()?, &evidence, |_| false)?;
    assert_eq!(
        plan.iter().map(|item| &item.schedule).collect::<Vec<_>>(),
        [&direct],
        "four runs exhaust its default budget like any schedule's"
    );

    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(&fixture, &backend, None, &grants, &claimant, &clock)?;
    budget::tick(&tick, &policy, &evidence)?;
    assert!(!enabled(&sim, direct.handle.as_str()), "paused by the pass");
    Ok(())
}

/// A tick over an exhausted pickup whose first report post meets `fault`,
/// then a second tick under `next_policy`. Returns both ticks' deliveries
/// and the posts the reporter performed.
fn report_retry(
    fault: ExecuteFault,
    next_policy: &SchedulePolicy,
) -> TestResult<(Vec<Delivery>, Vec<Delivery>, usize)> {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    exhausted_pickup(&sim, &backend)?;
    let reporter = reporter()?;
    reporter.inject(fault);
    let channel = ReportChannel {
        executor: &reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let clock = ManualClock::starting_at(1);
    let tick = tick_with(
        &fixture,
        &backend,
        Some(channel),
        &grants,
        &claimant,
        &clock,
    )?;
    let first = budget::tick(&tick, &policy()?, &backend.schedule_evidence()?)?;
    clock.advance(60);
    let second = budget::tick(&tick, next_policy, &backend.schedule_evidence()?)?;
    Ok((
        first.deliveries,
        second.deliveries,
        reporter.effects_performed(),
    ))
}

#[test]
fn a_lost_report_is_not_posted_again_when_the_exhausted_limit_changes() -> TestResult {
    // The owner lowers the schedule budget while the first post is uncertain.
    let lowered = SchedulePolicy {
        schedule_budget: budget(3, Some(400))?,
        ..policy()?
    };
    let (first, second, posts) = report_retry(ExecuteFault::ApplyThenLoseResponse, &lowered)?;
    assert!(matches!(first.as_slice(), [Delivery::NotDelivered { .. }]));
    assert!(
        matches!(second.as_slice(), [Delivery::Delivered(_)]),
        "{second:?}"
    );
    assert_eq!(posts, 1, "the lost post is found, not posted again");
    Ok(())
}

#[test]
fn a_refused_report_is_posted_by_the_next_tick() -> TestResult {
    let (first, second, posts) = report_retry(ExecuteFault::Reject, &policy()?)?;
    let [Delivery::NotDelivered { record, .. }] = first.as_slice() else {
        return Err(format!("expected a refused post, got {first:?}").into());
    };
    assert!(matches!(record.state(), EffectState::NotApplied { .. }));
    assert!(
        matches!(second.as_slice(), [Delivery::Delivered(_)]),
        "{second:?}"
    );
    assert_eq!(posts, 1);
    Ok(())
}

#[test]
fn a_windows_task_stays_retryable_for_the_longest_window() -> TestResult {
    use kitchen::scheduling::MAX_WINDOW_HOURS;
    let longest = Duration::from_secs(u64::from(MAX_WINDOW_HOURS) * 3600);
    assert_eq!(MAX_WINDOW_HOURS, 744);
    assert!(
        RetryPolicy::new(1, longest).is_ok(),
        "the contract admits it"
    );

    for hours in [24, MAX_WINDOW_HOURS] {
        let fixture = Fixture::new()?;
        let sim = SimOrca::default();
        let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
        exhausted_pickup(&sim, &backend)?;
        let policy = SchedulePolicy {
            window_hours: WindowHours::new(hours)?,
            ..policy()?
        };
        let grants = tick_grants()?;
        let claimant = scheduled("budget-tick")?;
        let clock = ManualClock::starting_at(1);
        let tick = tick_with(&fixture, &backend, None, &grants, &claimant, &clock)?;
        let evidence = backend.schedule_evidence()?;
        let window = policy.window_hours.containing(evidence.observed_at);
        budget::tick(&tick, &policy, &evidence)?;

        // The task is created at the first exhaustion in the window and must
        // be claimable until the window's last instant, however long it is.
        let task = fixture.store.task(&task_id(&format!(
            "budget-{}-{hours}h",
            window.start.as_unix_millis()
        ))?)?;
        assert_eq!(
            task.spec().retry.max_elapsed(),
            Duration::from_secs(u64::from(hours) * 3600),
            "{hours}h window"
        );
    }
    Ok(())
}

#[test]
fn a_window_task_from_an_earlier_retry_policy_is_continued_not_refused() -> TestResult {
    let policy = policy()?;
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;

    // The spec this release creates for the window.
    let fresh = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    exhausted_pickup(&sim, &backend)?;
    let evidence = backend.schedule_evidence()?;
    let window = policy.window_hours.containing(evidence.observed_at);
    let start = window.start.as_unix_millis();
    let sized = task_id(&format!("budget-{start}-24h"))?;
    let id = task_id(&format!("budget-{start}"))?;
    let clock = ManualClock::starting_at(1);
    budget::tick(
        &tick_with(&fresh, &backend, None, &grants, &claimant, &clock)?,
        &policy,
        &evidence,
    )?;
    let current = TaskSpec {
        id: id.clone(),
        ..fresh.store.task(&sized)?.spec().clone()
    };

    // An earlier release created the window's task under its unsized name
    // and another retry policy; the tick continues it and still acts.
    let earlier = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    exhausted_pickup(&sim, &backend)?;
    let old_retry = RetryPolicy::new(1, Duration::from_secs(30 * 24 * 3600))?;
    earlier.store.create_task(
        TaskSpec {
            retry: old_retry,
            ..current.clone()
        },
        &claimant,
        Timestamp::from_unix_millis(1),
    )?;
    let report = budget::tick(
        &tick_with(&earlier, &backend, None, &grants, &claimant, &clock)?,
        &policy,
        &backend.schedule_evidence()?,
    )?;
    assert!(matches!(report.pass, BudgetPass::Acted(_)), "{report:?}");
    assert_eq!(earlier.store.task(&id)?.spec().retry, old_retry);
    assert!(
        earlier.store.task(&sized).is_err(),
        "no second task for the window"
    );

    // Any other difference is still a conflict.
    let other = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    exhausted_pickup(&sim, &backend)?;
    other.store.create_task(
        TaskSpec {
            role: Role::SousChef,
            ..current
        },
        &claimant,
        Timestamp::from_unix_millis(1),
    )?;
    let refused = budget::tick(
        &tick_with(&other, &backend, None, &grants, &claimant, &clock)?,
        &policy,
        &backend.schedule_evidence()?,
    );
    assert!(
        matches!(
            refused,
            Err(kitchen::Error::State(StateError::TaskConflict(_)))
        ),
        "{refused:?}"
    );
    Ok(())
}

/// Day thirty-one starts both a 24 hour and a 744 hour window: the epoch-
/// aligned windows of the two lengths share a start there.
fn thirty_one_at_noon() -> Timestamp {
    Timestamp::from_unix_millis(31 * DAY_MS + 12 * HOUR_MS)
}

fn thirty_one_at_one() -> Timestamp {
    Timestamp::from_unix_millis(31 * DAY_MS + 13 * HOUR_MS)
}

fn thirty_one_at_two() -> Timestamp {
    Timestamp::from_unix_millis(31 * DAY_MS + 14 * HOUR_MS)
}

fn thirty_one_at_three() -> Timestamp {
    Timestamp::from_unix_millis(31 * DAY_MS + 15 * HOUR_MS)
}

/// One tick under a policy whose window is `hours` long.
fn tick_at_length(
    fixture: &Fixture,
    backend: &OrcaBackend<&SimOrca>,
    reporter: &FakeBackend,
    hours: u16,
    clock: &ManualClock,
) -> TestResult<TickReport> {
    let policy = SchedulePolicy {
        window_hours: WindowHours::new(hours)?,
        ..policy()?
    };
    let channel = ReportChannel {
        executor: reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let tick = tick_with(fixture, backend, Some(channel), &grants, &claimant, clock)?;
    Ok(budget::tick(&tick, &policy, &backend.schedule_evidence()?)?)
}

/// One tick under a policy whose window is `hours` long, on `store`, such as
/// a store reopened after a restart, over an observation already made. The
/// outer result is the test setup; the inner one is the tick's.
fn tick_on_store(
    fixture: &Fixture,
    store: &kitchen::state::HouseStore,
    backend: &OrcaBackend<&SimOrca>,
    reporter: &FakeBackend,
    hours: u16,
    clock: &ManualClock,
    evidence: &ScheduleEvidence,
) -> TestResult<kitchen::Result<TickReport>> {
    let policy = SchedulePolicy {
        window_hours: WindowHours::new(hours)?,
        ..policy()?
    };
    let channel = ReportChannel {
        executor: reporter,
        effect: &report_effect,
    };
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let tick = tick_with(fixture, backend, Some(channel), &grants, &claimant, clock)?;
    Ok(budget::tick(&Tick { store, ..tick }, &policy, evidence))
}

/// The budget tasks in the store, by id: id and retry elapsed budget in
/// hours.
fn budget_tasks(fixture: &Fixture) -> TestResult<Vec<(String, u64)>> {
    Ok(fixture
        .store
        .tasks()?
        .iter()
        .filter(|task| task.spec().id.as_str().starts_with("budget-"))
        .map(|task| {
            (
                task.spec().id.as_str().to_owned(),
                task.spec().retry.max_elapsed().as_secs() / 3600,
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn repaused(report: &TickReport) -> bool {
    matches!(&report.pass, BudgetPass::Acted(actions)
        if matches!(actions.as_slice(), [PassAction::Repaused(_)]))
}

#[test]
fn lengthening_and_shortening_the_window_mid_window_pauses_again_but_reports_once() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let at_noon = connect(&sim)?.with_clock(thirty_one_at_noon);
    let installed = at_noon.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    sim.state().runs = runs_on_day(31, 4);
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let start = 31 * DAY_MS;

    // 24 hours: exhausted, paused, and reported.
    let first = tick_at_length(&fixture, &at_noon, &reporter, 24, &clock)?;
    assert!(matches!(
        first.deliveries.as_slice(),
        [Delivery::Delivered(_)]
    ));
    assert_eq!(reporter.effects_performed(), 1);

    // The owner re-activates it, then the house lengthens the window to 744
    // hours. The new window starts at the same instant, but the 24 hour
    // task's retry budget ends with the 24 hour window.
    enable(&sim, &installed);
    let at_one = connect(&sim)?.with_clock(thirty_one_at_one);
    let longer = tick_at_length(&fixture, &at_one, &reporter, 744, &clock)?;
    assert!(repaused(&longer), "{longer:?}");
    assert!(longer.deliveries.is_empty(), "the owner is not told twice");
    assert!(!enabled(&sim, installed.handle.as_str()));
    assert_eq!(
        budget_tasks(&fixture)?,
        [
            (format!("budget-{start}-24h"), 24),
            (format!("budget-{start}-744h"), 744)
        ],
        "the 744 hour window has a task retryable for its whole length"
    );

    // Back to 24 hours: the 24 hour task is used again, not a third one.
    enable(&sim, &installed);
    let at_two = connect(&sim)?.with_clock(thirty_one_at_two);
    let shorter = tick_at_length(&fixture, &at_two, &reporter, 24, &clock)?;
    assert!(repaused(&shorter), "{shorter:?}");
    assert!(shorter.deliveries.is_empty());
    assert!(!enabled(&sim, installed.handle.as_str()));

    // And to 744 again: the same second task.
    enable(&sim, &installed);
    let at_three = connect(&sim)?.with_clock(thirty_one_at_three);
    let again = tick_at_length(&fixture, &at_three, &reporter, 744, &clock)?;
    assert!(repaused(&again), "{again:?}");
    assert_eq!(budget_tasks(&fixture)?.len(), 2);
    assert_eq!(reporter.effects_performed(), 1, "one report for the window");
    assert_eq!(activations(&sim), 0, "the tick never activates a schedule");
    Ok(())
}

#[test]
fn a_shorter_window_sharing_a_start_gets_its_own_task_and_no_second_report() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let at_noon = connect(&sim)?.with_clock(thirty_one_at_noon);
    let installed = at_noon.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    sim.state().runs = runs_on_day(31, 4);
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);

    let first = tick_at_length(&fixture, &at_noon, &reporter, 744, &clock)?;
    assert!(matches!(
        first.deliveries.as_slice(),
        [Delivery::Delivered(_)]
    ));

    enable(&sim, &installed);
    let at_one = connect(&sim)?.with_clock(thirty_one_at_one);
    let shorter = tick_at_length(&fixture, &at_one, &reporter, 24, &clock)?;
    assert!(repaused(&shorter), "{shorter:?}");
    assert!(shorter.deliveries.is_empty());
    assert_eq!(
        budget_tasks(&fixture)?,
        [
            (format!("budget-{}-24h", 31 * DAY_MS), 24),
            (format!("budget-{}-744h", 31 * DAY_MS), 744)
        ],
        "each length's task names its hours"
    );
    assert_eq!(reporter.effects_performed(), 1);
    Ok(())
}

#[test]
fn a_window_that_starts_elsewhere_does_not_report_the_same_exhaustion_again() -> TestResult {
    // Day twenty: the 24 hour window starts on day twenty, the 744 hour one
    // on day zero, so the two windows have different tasks and markers.
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);

    let first = tick_at_length(&fixture, &backend, &reporter, 24, &clock)?;
    assert!(matches!(
        first.deliveries.as_slice(),
        [Delivery::Delivered(_)]
    ));
    assert_eq!(reporter.effects_performed(), 1);

    // Paused and reported; the longer window is exhausted too, and idle.
    let longer_policy = SchedulePolicy {
        window_hours: WindowHours::new(744)?,
        ..policy()?
    };
    assert_eq!(
        budget::precheck(
            &fixture.store,
            &house()?,
            &longer_policy,
            &backend.schedule_evidence()?
        )?,
        Precheck::Idle
    );
    let longer = tick_at_length(&fixture, &backend, &reporter, 744, &clock)?;
    assert_eq!(longer.pass, BudgetPass::Idle, "{longer:?}");
    assert_eq!(reporter.effects_performed(), 1);

    // Re-activated, it is paused again under either length without a report.
    enable(&sim, &installed);
    let one_pm = connect(&sim)?.with_clock(one_pm_on_day_twenty);
    let paused = tick_at_length(&fixture, &one_pm, &reporter, 744, &clock)?;
    assert!(repaused(&paused), "{paused:?}");
    enable(&sim, &installed);
    let back = tick_at_length(&fixture, &one_pm, &reporter, 24, &clock)?;
    assert!(
        back.deliveries.is_empty(),
        "no second report: {:?}",
        back.deliveries
    );
    assert_eq!(reporter.effects_performed(), 1);

    // The next day's window is a new exhaustion and is reported.
    sim.state().runs = runs_on_day(21, 4);
    let next_day = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    enable(&sim, &installed);
    let next = tick_at_length(&fixture, &next_day, &reporter, 24, &clock)?;
    assert!(
        next.deliveries
            .iter()
            .any(|delivery| matches!(delivery, Delivery::Delivered(_))),
        "{next:?}"
    );
    assert_eq!(reporter.effects_performed(), 2);
    Ok(())
}

/// A window task as a release before #182 created it: the same spec with a
/// fixed 30 day retry budget.
fn earlier_release_task(
    fixture: &Fixture,
    window_start: u64,
    claimant: &kitchen::contracts::Claimant,
) -> TestResult {
    let grants = tick_grants()?;
    let TickSpecProbe { spec } = TickSpecProbe::of(&grants, window_start)?;
    fixture.store.create_task(
        TaskSpec {
            retry: RetryPolicy::new(16, Duration::from_secs(30 * 24 * 3600))?,
            ..spec
        },
        claimant,
        Timestamp::from_unix_millis(1),
    )?;
    Ok(())
}

struct TickSpecProbe {
    spec: TaskSpec,
}

impl TickSpecProbe {
    /// The spec this release gives the window that starts at `start`.
    fn of(grants: &HouseGrants, start: u64) -> TestResult<Self> {
        Ok(Self {
            spec: TaskSpec {
                id: task_id(&format!("budget-{start}"))?,
                role: Role::Expediter,
                repository: None,
                authority: TaskAuthority::delegate(
                    grants,
                    [
                        Grant::house(Permission::ManageSchedule, orca_id()?, credential()?),
                        comment_grant()?,
                    ],
                )?,
                retry: RetryPolicy::new(16, Duration::from_secs(24 * 3600))?,
                provenance: Provenance {
                    kitchen: commit('a')?,
                    house_guidance: commit('b')?,
                    repository_instructions: None,
                },
                requires: CapabilityRequirements::new(),
                resources: BTreeSet::new(),
                agent: None,
                work_type: None,
            },
        })
    }
}

#[test]
fn a_task_from_before_the_retry_bound_is_looked_up_before_a_longer_window_acts() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let at_noon = connect(&sim)?.with_clock(thirty_one_at_noon);
    let installed = at_noon.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    sim.state().runs = runs_on_day(31, 4);
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let start = 31 * DAY_MS;
    earlier_release_task(&fixture, start, &scheduled("budget-tick")?)?;

    // The earlier release's task is continued for the 24 hour window; its
    // report post is applied but the response is lost.
    reporter.inject(ExecuteFault::ApplyThenLoseResponse);
    let first = tick_at_length(&fixture, &at_noon, &reporter, 24, &clock)?;
    let [Delivery::NotDelivered { record, .. }] = first.deliveries.as_slice() else {
        return Err(format!("expected an uncertain post, got {first:?}").into());
    };
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    assert_eq!(budget_tasks(&fixture)?, [(format!("budget-{start}"), 720)]);

    // Its 30 days are shorter than a 744 hour window, so the longer window
    // gets its own task, which looks the post up and does not repeat it.
    let at_one = connect(&sim)?.with_clock(thirty_one_at_one);
    let longer = tick_at_length(&fixture, &at_one, &reporter, 744, &clock)?;
    assert!(
        matches!(longer.deliveries.as_slice(), [Delivery::Delivered(_)]),
        "{longer:?}"
    );
    assert_eq!(reporter.effects_performed(), 1, "found, not reposted");
    assert_eq!(
        budget_tasks(&fixture)?,
        [
            (format!("budget-{start}"), 720),
            (format!("budget-{start}-744h"), 744)
        ]
    );
    let idle = tick_at_length(&fixture, &at_one, &reporter, 744, &clock)?;
    assert_eq!(idle.pass, BudgetPass::Idle);
    assert_eq!(reporter.effects_performed(), 1);
    Ok(())
}

#[test]
fn a_window_whose_earlier_task_cannot_be_looked_up_is_refused_not_acted_on() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let at_noon = connect(&sim)?.with_clock(thirty_one_at_noon);
    let installed = at_noon.install_schedule(&spec("pickup", "0 * * * *")?)?;
    enable(&sim, &installed);
    sim.state().runs = runs_on_day(31, 4);
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    earlier_release_task(&fixture, 31 * DAY_MS, &scheduled("budget-tick")?)?;
    reporter.inject(ExecuteFault::ApplyThenLoseResponse);
    tick_at_length(&fixture, &at_noon, &reporter, 24, &clock)?;
    enable(&sim, &installed);
    let edits = sim.calls_to(&["automations", "edit"]).len();

    // The post cannot be looked up: nothing is paused or posted, and the
    // next tick that can look it up carries on.
    reporter.fail_lookups(1);
    let refused = tick_at_length(&fixture, &at_noon, &reporter, 744, &clock);
    assert!(refused.is_err(), "{refused:?}");
    assert!(enabled(&sim, installed.handle.as_str()), "not paused");
    assert_eq!(sim.calls_to(&["automations", "edit"]).len(), edits);
    assert_eq!(reporter.effects_performed(), 1);

    let at_one = connect(&sim)?.with_clock(thirty_one_at_one);
    let retried = tick_at_length(&fixture, &at_one, &reporter, 744, &clock)?;
    assert!(!enabled(&sim, installed.handle.as_str()), "{retried:?}");
    assert_eq!(reporter.effects_performed(), 1);
    Ok(())
}

/// The start of the window of `hours` that holds noon on day twenty: day
/// twenty for 24 hours, day zero for 744.
fn day_twenty_window_start(hours: u16) -> u64 {
    let length = u64::from(hours) * HOUR_MS;
    (20 * DAY_MS + 12 * HOUR_MS) / length * length
}

/// Which release created the task whose report post is lost.
#[derive(Clone, Copy)]
enum PostedBy {
    /// This release: `budget-<start>-<hours>h`.
    ThisRelease,
    /// An earlier release: `budget-<start>` with a 30 day retry budget,
    /// which is also what a 720 hour window's task looked like.
    EarlierRelease,
}

/// A report post that applied under a window of `first` hours but whose
/// response was lost, then a tick under `second` hours, whose window starts
/// elsewhere, on a store reopened as by a new process.
fn a_lost_report_across_a_length_change(first: u16, second: u16, by: PostedBy) -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    exhausted_pickup(&sim, &backend)?;
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let start = day_twenty_window_start(first);
    let earlier = task_id(&match by {
        PostedBy::ThisRelease => format!("budget-{start}-{first}h"),
        PostedBy::EarlierRelease => {
            earlier_release_task(&fixture, start, &scheduled("budget-tick")?)?;
            format!("budget-{start}")
        }
    })?;
    reporter.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = tick_at_length(&fixture, &backend, &reporter, first, &clock)?;
    let [Delivery::NotDelivered { record, .. }] = lost.deliveries.as_slice() else {
        return Err(format!("expected an uncertain post, got {lost:?}").into());
    };
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    assert_eq!(reporter.effects_performed(), 1, "the post applied");
    assert_eq!(
        fixture.store.task(&earlier)?.unresolved_effects().count(),
        1
    );

    let restarted = fixture.reopen()?;
    let one_pm = connect(&sim)?.with_clock(one_pm_on_day_twenty);
    let evidence = one_pm.schedule_evidence()?;
    let found = tick_on_store(
        &fixture, &restarted, &one_pm, &reporter, second, &clock, &evidence,
    )??;
    assert!(
        matches!(found.deliveries.as_slice(), [Delivery::Delivered(_)]),
        "{found:?}"
    );
    assert_eq!(reporter.effects_performed(), 1, "found, not posted again");
    assert_eq!(restarted.task(&earlier)?.unresolved_effects().count(), 0);
    assert!(
        budget_tasks(&fixture)?
            .iter()
            .any(|(id, _)| *id == format!("budget-{}-{second}h", day_twenty_window_start(second))),
        "the new window has its own task"
    );

    let idle = tick_on_store(
        &fixture, &restarted, &one_pm, &reporter, second, &clock, &evidence,
    )??;
    assert_eq!(idle.pass, BudgetPass::Idle, "{idle:?}");
    assert_eq!(reporter.effects_performed(), 1);
    Ok(())
}

#[test]
fn a_report_lost_before_the_window_lengthens_is_found_not_posted_again() -> TestResult {
    a_lost_report_across_a_length_change(24, 744, PostedBy::ThisRelease)
}

#[test]
fn a_report_lost_before_the_window_shortens_is_found_not_posted_again() -> TestResult {
    a_lost_report_across_a_length_change(744, 24, PostedBy::ThisRelease)
}

#[test]
fn a_report_an_earlier_release_lost_before_a_720_hour_window_shortens_is_not_posted_again()
-> TestResult {
    // The earlier task's window may be 720 hours long and still open, so its
    // post counts although the task could also be a day's.
    a_lost_report_across_a_length_change(720, 24, PostedBy::EarlierRelease)
}

#[test]
fn a_report_an_earlier_release_lost_before_the_window_lengthens_to_720_hours_is_not_posted_again()
-> TestResult {
    a_lost_report_across_a_length_change(24, 720, PostedBy::EarlierRelease)
}

/// Orca, except that the response to every effect it applies is lost.
struct LosesResponses<'a>(&'a OrcaBackend<&'a SimOrca>);

impl EffectExecutor for LosesResponses<'_> {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.0.execute(request)?;
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.0.lookup(request)
    }
}

/// A pause that applied under a window of `first` hours but whose response
/// was lost, then ticks under `second` hours on a reopened store: refused
/// while the pause cannot be looked up, then found applied and reported once.
fn a_lost_pause_across_a_length_change(first: u16, second: u16) -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let grants = tick_grants()?;
    let claimant = scheduled("budget-tick")?;
    let losing = LosesResponses(&backend);
    let tick = Tick {
        schedules: &losing,
        ..tick_with(&fixture, &backend, None, &grants, &claimant, &clock)?
    };
    let first_policy = SchedulePolicy {
        window_hours: WindowHours::new(first)?,
        ..policy()?
    };
    let lost = budget::tick(&tick, &first_policy, &backend.schedule_evidence()?)?;
    let BudgetPass::Acted(actions) = &lost.pass else {
        return Err(format!("expected a pause, got {lost:?}").into());
    };
    let [PassAction::PauseNotApplied { record, .. }] = actions.as_slice() else {
        return Err(format!("expected an uncertain pause, got {actions:?}").into());
    };
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    assert!(lost.deliveries.is_empty());
    assert!(
        !enabled(&sim, installed.handle.as_str()),
        "the pause applied"
    );
    let earlier = task_id(&format!(
        "budget-{}-{first}h",
        day_twenty_window_start(first)
    ))?;
    let edits = sim.calls_to(&["automations", "edit"]).len();

    // The house changes the window's length and the next tick runs in a new
    // process that cannot reach Orca to look the pause up: nothing happens.
    let restarted = fixture.reopen()?;
    let one_pm = connect(&sim)?.with_clock(one_pm_on_day_twenty);
    let evidence = one_pm.schedule_evidence()?;
    sim.fault(Fault::Spawn);
    let refused = tick_on_store(
        &fixture, &restarted, &one_pm, &reporter, second, &clock, &evidence,
    )?;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(sim.calls_to(&["automations", "edit"]).len(), edits);
    assert_eq!(reporter.effects_performed(), 0);
    assert_eq!(restarted.task(&earlier)?.unresolved_effects().count(), 1);

    // The lookup finds it applied; the new task reports once.
    let found = tick_on_store(
        &fixture, &restarted, &one_pm, &reporter, second, &clock, &evidence,
    )??;
    assert!(
        matches!(found.deliveries.as_slice(), [Delivery::Delivered(_)]),
        "{found:?}"
    );
    assert_eq!(restarted.task(&earlier)?.unresolved_effects().count(), 0);
    assert_eq!(reporter.effects_performed(), 1);
    assert_eq!(activations(&sim), 0);
    Ok(())
}

#[test]
fn a_pause_lost_before_the_window_lengthens_is_looked_up_first() -> TestResult {
    a_lost_pause_across_a_length_change(24, 744)
}

#[test]
fn a_pause_lost_before_the_window_shortens_is_looked_up_first() -> TestResult {
    a_lost_pause_across_a_length_change(744, 24)
}

#[test]
fn a_task_from_before_the_retry_bound_settles_once_a_later_window_starts() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let earlier = task_id(&format!("budget-{}", 20 * DAY_MS))?;
    earlier_release_task(&fixture, 20 * DAY_MS, &scheduled("budget-tick")?)?;
    let first = tick_at_length(&fixture, &backend, &reporter, 24, &clock)?;
    assert!(matches!(
        first.deliveries.as_slice(),
        [Delivery::Delivered(_)]
    ));

    // Its 30 day retry budget runs on, but the next day's window has begun
    // and nothing on it is unresolved.
    sim.state().runs = runs_on_day(21, 4);
    enable(&sim, &installed);
    let next_day = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    let later = tick_at_length(&fixture, &next_day, &reporter, 24, &clock)?;
    assert_eq!(later.settled, std::slice::from_ref(&earlier));
    assert_eq!(reporter.effects_performed(), 2);
    Ok(())
}

#[test]
fn a_shortened_window_settles_an_earlier_releases_task_and_a_restored_one_gets_a_sized_task()
-> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(&sim, &backend)?;
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let start = day_twenty_window_start(720);
    let longer = format!("budget-{start}");
    earlier_release_task(&fixture, start, &scheduled("budget-tick")?)?;
    let first = tick_at_length(&fixture, &backend, &reporter, 720, &clock)?;
    assert!(matches!(
        first.deliveries.as_slice(),
        [Delivery::Delivered(_)]
    ));

    // Shortened: the day's window starts after the earlier release's task,
    // which has nothing unresolved and settles.
    enable(&sim, &installed);
    let one_pm = connect(&sim)?.with_clock(one_pm_on_day_twenty);
    let shorter = tick_at_length(&fixture, &one_pm, &reporter, 24, &clock)?;
    assert!(repaused(&shorter), "{shorter:?}");
    assert_eq!(shorter.settled, [task_id(&longer)?]);

    // Restored: its window is still open, so it gets a sized task, is
    // paused again, and is not reported twice.
    enable(&sim, &installed);
    let restored = tick_at_length(&fixture, &one_pm, &reporter, 720, &clock)?;
    assert!(repaused(&restored), "{restored:?}");
    assert!(
        restored.settled.is_empty(),
        "the sized tasks are not settled"
    );
    assert_eq!(
        budget_tasks(&fixture)?,
        [
            (longer.clone(), 720),
            (format!("{longer}-720h"), 720),
            (format!("budget-{}-24h", 20 * DAY_MS), 24),
        ]
    );
    assert_eq!(reporter.effects_performed(), 1);
    assert!(!enabled(&sim, installed.handle.as_str()));
    Ok(())
}

/// An earlier release's task for day twenty's 24 hour window whose report
/// post applied but whose response was lost.
fn an_earlier_releases_lost_post(
    fixture: &Fixture,
    sim: &SimOrca,
    reporter: &FakeBackend,
    clock: &ManualClock,
) -> TestResult<(kitchen::TaskId, ResourceRef)> {
    let backend = connect(sim)?.with_clock(noon_on_day_twenty);
    let installed = exhausted_pickup(sim, &backend)?;
    let earlier = task_id(&format!("budget-{}", 20 * DAY_MS))?;
    earlier_release_task(fixture, 20 * DAY_MS, &scheduled("budget-tick")?)?;
    reporter.inject(ExecuteFault::ApplyThenLoseResponse);
    tick_at_length(fixture, &backend, reporter, 24, clock)?;
    assert_eq!(
        fixture.store.task(&earlier)?.unresolved_effects().count(),
        1
    );
    assert_eq!(reporter.effects_performed(), 1);
    Ok((earlier, installed))
}

#[test]
fn an_earlier_releases_unknown_post_holds_back_later_windows_until_it_is_found() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let (earlier, installed) = an_earlier_releases_lost_post(&fixture, &sim, &reporter, &clock)?;

    // Next day the post still cannot be looked up. The task could be a 720
    // hour window's, still open, so the day's window waits on it.
    reporter.fail_lookups(1);
    sim.state().runs = runs_on_day(21, 4);
    enable(&sim, &installed);
    let next_day = connect(&sim)?.with_clock(noon_on_day_twenty_one);
    let edits = sim.calls_to(&["automations", "edit"]).len();
    let refused = tick_at_length(&fixture, &next_day, &reporter, 24, &clock);
    assert!(refused.is_err(), "{refused:?}");
    assert!(enabled(&sim, installed.handle.as_str()), "not paused");
    assert_eq!(sim.calls_to(&["automations", "edit"]).len(), edits);
    assert_eq!(reporter.effects_performed(), 1);

    // Found applied with no report marker: its window may still be open, so
    // the post counts for the day's window, and the task settles.
    let later = tick_at_length(&fixture, &next_day, &reporter, 24, &clock)?;
    assert!(
        matches!(later.deliveries.as_slice(), [Delivery::Delivered(_)]),
        "{later:?}"
    );
    assert!(!enabled(&sim, installed.handle.as_str()));
    assert_eq!(reporter.effects_performed(), 1, "not posted again");
    assert_eq!(later.settled, std::slice::from_ref(&earlier));
    Ok(())
}

fn noon_on_day_fifty() -> Timestamp {
    Timestamp::from_unix_millis(50 * DAY_MS + 12 * HOUR_MS)
}

fn noon_on_day_fifty_one() -> Timestamp {
    Timestamp::from_unix_millis(51 * DAY_MS + 12 * HOUR_MS)
}

#[test]
fn an_earlier_releases_unknown_post_holds_back_no_window_after_the_longest() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let reporter = reporter()?;
    let clock = ManualClock::starting_at(1);
    let (earlier, installed) = an_earlier_releases_lost_post(&fixture, &sim, &reporter, &clock)?;
    reporter.fail_lookups(8);

    // Day fifty is inside the 744 hours from day twenty: refused.
    sim.state().runs = runs_on_day(50, 4);
    enable(&sim, &installed);
    let day_fifty = connect(&sim)?.with_clock(noon_on_day_fifty);
    let refused = tick_at_length(&fixture, &day_fifty, &reporter, 24, &clock);
    assert!(refused.is_err(), "{refused:?}");
    assert!(enabled(&sim, installed.handle.as_str()));

    // Day fifty-one starts after them: paused and reported, and the task
    // stays open with its post unknown.
    sim.state().runs = runs_on_day(51, 4);
    let day_fifty_one = connect(&sim)?.with_clock(noon_on_day_fifty_one);
    let acted = tick_at_length(&fixture, &day_fifty_one, &reporter, 24, &clock)?;
    assert!(
        matches!(acted.deliveries.as_slice(), [Delivery::Delivered(_)]),
        "{acted:?}"
    );
    assert!(!enabled(&sim, installed.handle.as_str()));
    assert_eq!(reporter.effects_performed(), 2);
    assert!(
        !acted.settled.contains(&earlier),
        "its post is still unknown"
    );
    assert_eq!(
        fixture.store.task(&earlier)?.unresolved_effects().count(),
        1
    );
    Ok(())
}
