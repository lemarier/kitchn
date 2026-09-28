//! Portable schedule definitions and install reconciliation.

use std::time::Duration;

use kitchen::{
    BackendId, ConsumerId,
    contracts::{ExternalRef, ResourceKind, ResourceRef, Text},
    scheduling::{
        AgentFamily, CronExpr, GraceMinutes, InstallPlan, InstalledSchedule, MAX_PRECHECK_ARGS,
        ObservedScheduleState, Precheck, PrecheckOutcome, PrecheckTimeout, Recurrence, RunOutcome,
        RunVerdict, ScheduleError, ScheduleRun, ScheduleSpec, ScheduleWorkspace, TimeOfDay,
        Timezone, WorkflowName, plan_install, run_verdict,
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn schedule(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Schedule,
        backend: BackendId::new("orca-local")?,
        handle: ExternalRef::new(handle)?,
    })
}

fn installed(
    handle: &str,
    name: &str,
    state: ObservedScheduleState,
) -> TestResult<InstalledSchedule> {
    Ok(InstalledSchedule {
        resource: schedule(handle)?,
        consumer: ConsumerId::new(name)?,
        state,
    })
}

#[test]
fn cron_expressions_need_five_plain_fields() {
    for valid in ["* * * * *", "17,37,57 * * * *", "*/5 9-17 * * 1-5"] {
        assert!(CronExpr::new(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "* * * *",
        "* * * * * *",
        "*  * * * *",
        "@hourly",
        "* * * * mon",
        "* * * * *; rm -rf /",
    ] {
        assert_eq!(
            CronExpr::new(invalid),
            Err(ScheduleError::Cron),
            "{invalid}"
        );
    }
    assert_eq!(
        CronExpr::new(&format!("{} * * * *", "1,".repeat(70))),
        Err(ScheduleError::Cron)
    );
}

#[test]
fn times_zones_and_bounds_are_validated() {
    assert_eq!(
        TimeOfDay::new(23, 59).map(|t| t.to_string()),
        Ok("23:59".to_owned())
    );
    assert_eq!(TimeOfDay::new(24, 0), Err(ScheduleError::TimeOfDay));
    assert_eq!(TimeOfDay::new(0, 60), Err(ScheduleError::TimeOfDay));
    for valid in [
        "UTC",
        "America/Toronto",
        "America/Argentina/Buenos_Aires",
        "Etc/GMT+5",
    ] {
        assert!(Timezone::new(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "Toronto",
        "America/",
        "/UTC",
        "America/New York",
        "../etc/passwd",
    ] {
        assert_eq!(
            Timezone::new(invalid),
            Err(ScheduleError::Timezone),
            "{invalid}"
        );
    }
    assert!(GraceMinutes::new(1440).is_ok());
    assert_eq!(
        GraceMinutes::new(1441),
        Err(ScheduleError::Grace { max: 1440 })
    );
    assert!(PrecheckTimeout::new(Duration::from_secs(1)).is_ok());
    assert!(PrecheckTimeout::new(Duration::from_millis(999)).is_err());
    assert!(PrecheckTimeout::new(Duration::from_secs(301)).is_err());
    assert_eq!(
        PrecheckTimeout::new(Duration::from_millis(1500)).map(PrecheckTimeout::whole_seconds),
        Ok(2),
        "rounding never shortens the bound"
    );
}

#[test]
fn prechecks_are_bounded_argument_vectors_with_typed_results() -> TestResult {
    let timeout = PrecheckTimeout::new(Duration::from_secs(30))?;
    assert_eq!(
        Precheck::new(Vec::new(), timeout),
        Err(ScheduleError::PrecheckArgs {
            max: MAX_PRECHECK_ARGS
        })
    );
    let too_many = vec![Text::new("a")?; MAX_PRECHECK_ARGS + 1];
    assert!(Precheck::new(too_many, timeout).is_err());
    let exact = vec![Text::new("a")?; MAX_PRECHECK_ARGS];
    assert_eq!(
        Precheck::new(exact, timeout)?.argv().len(),
        MAX_PRECHECK_ARGS
    );

    assert_eq!(
        PrecheckOutcome::from_exit_code(Some(0)),
        PrecheckOutcome::Actionable
    );
    assert_eq!(
        PrecheckOutcome::from_exit_code(Some(1)),
        PrecheckOutcome::Idle
    );
    assert_eq!(
        PrecheckOutcome::from_exit_code(Some(2)),
        PrecheckOutcome::Error
    );
    assert_eq!(
        PrecheckOutcome::from_exit_code(None),
        PrecheckOutcome::Error,
        "a killed precheck is an error, not idle"
    );
    Ok(())
}

#[test]
fn session_reuse_needs_an_existing_workspace() -> TestResult {
    let spec = ScheduleSpec::new(
        WorkflowName::new("triage")?,
        ConsumerId::new("triage-home")?,
        Recurrence::Weekdays(TimeOfDay::new(9, 0)?),
        Timezone::new("UTC")?,
        Text::new("Triage needs-spec issues.")?,
        AgentFamily::Codex,
    );
    assert_eq!(
        spec.clone().with_session_reuse(),
        Err(ScheduleError::ReuseNeedsExistingWorkspace)
    );
    let workspace = ResourceRef {
        kind: ResourceKind::Worktree,
        ..schedule("wt-1")?
    };
    let reusing = spec
        .with_workspace(ScheduleWorkspace::Existing(workspace))
        .with_session_reuse()?;
    assert!(reusing.reuse_session());
    let fresh = reusing.with_workspace(ScheduleWorkspace::NewPerRun);
    assert!(
        !fresh.reuse_session(),
        "a fresh workspace cannot keep reuse"
    );
    Ok(())
}

#[test]
fn install_plans_never_add_a_second_consumer() -> TestResult {
    let name = ConsumerId::new("pickup")?;
    assert_eq!(plan_install(&name, &[]), InstallPlan::Create);
    let one = installed("a1", "pickup", ObservedScheduleState::Paused)?;
    let other = installed("a2", "triage", ObservedScheduleState::Active)?;
    assert_eq!(
        plan_install(&name, &[other.clone(), one.clone()]),
        InstallPlan::Installed(schedule("a1")?)
    );
    let second = installed("a0", "pickup", ObservedScheduleState::Active)?;
    assert_eq!(
        plan_install(&name, &[one.clone(), other, second]),
        InstallPlan::Duplicates(vec![schedule("a0")?, schedule("a1")?])
    );
    let gone = installed("a9", "pickup", ObservedScheduleState::Missing)?;
    assert_eq!(
        plan_install(&name, &[gone, one]),
        InstallPlan::Installed(schedule("a1")?),
        "a missing entry is not a consumer"
    );
    Ok(())
}

#[test]
fn persisted_specs_round_trip_and_revalidate() -> TestResult {
    let spec = ScheduleSpec::new(
        WorkflowName::new("gardener")?,
        ConsumerId::new("gardener-home")?,
        Recurrence::Weekly(kitchen::scheduling::Weekday::Monday, TimeOfDay::new(9, 30)?),
        Timezone::new("America/Toronto")?,
        Text::new("Garden the issues.")?,
        AgentFamily::Claude,
    )
    .with_precheck(Precheck::new(
        vec![Text::new("kitchen")?, Text::new("precheck")?],
        PrecheckTimeout::new(Duration::from_secs(45))?,
    )?);
    let json = serde_json::to_value(&spec)?;
    assert_eq!(
        json["recurrence"],
        serde_json::json!({"type": "weekly", "at": ["monday", "09:30"]})
    );
    assert_eq!(json["precheck"]["timeout"], 45_000);
    let back: ScheduleSpec = serde_json::from_value(json.clone())?;
    assert_eq!(back, spec);

    let mut reuse_without_workspace = json.clone();
    reuse_without_workspace["reuseSession"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<ScheduleSpec>(reuse_without_workspace).is_err());
    let mut bad_zone = json.clone();
    bad_zone["timezone"] = serde_json::json!("Not A Zone");
    assert!(serde_json::from_value::<ScheduleSpec>(bad_zone).is_err());
    let mut bad_time = json.clone();
    bad_time["recurrence"] = serde_json::json!({"type": "daily", "at": "24:00"});
    assert!(serde_json::from_value::<ScheduleSpec>(bad_time).is_err());
    assert_eq!(json["workflow"], "gardener");
    assert_eq!(json["consumer"], "gardener-home");
    let mut bad_workflow = json.clone();
    bad_workflow["workflow"] = serde_json::json!("../gardener");
    assert!(serde_json::from_value::<ScheduleSpec>(bad_workflow).is_err());
    let mut extra = json;
    extra["shell"] = serde_json::json!("rm -rf /");
    assert!(
        serde_json::from_value::<ScheduleSpec>(extra).is_err(),
        "unknown fields are rejected"
    );
    Ok(())
}

#[test]
fn an_agent_that_never_became_ready_is_a_launch_failure() {
    use kitchen::contracts::Timestamp;
    let deadline = Duration::from_secs(600);
    let due = Timestamp::from_unix_millis(1_000_000);
    let reported = ScheduleRun {
        outcome: RunOutcome::LaunchReported,
        scheduled_for: Some(due),
    };
    let within = due.saturating_add(Duration::from_secs(60));
    let past = due.saturating_add(Duration::from_secs(601));
    assert_eq!(
        run_verdict(&reported, false, within, deadline),
        RunVerdict::Pending
    );
    assert_eq!(
        run_verdict(&reported, false, past, deadline),
        RunVerdict::LaunchFailed,
        "Orca's completed launch without readiness is a failed launch"
    );
    assert_eq!(
        run_verdict(&reported, true, past, deadline),
        RunVerdict::Started
    );
    let undated = ScheduleRun {
        outcome: RunOutcome::LaunchReported,
        scheduled_for: None,
    };
    assert_eq!(
        run_verdict(&undated, false, past, deadline),
        RunVerdict::Pending
    );
    for (outcome, verdict) in [
        (RunOutcome::PrecheckIdle, RunVerdict::Idle),
        (RunOutcome::PrecheckFailed, RunVerdict::PrecheckFailed),
        (RunOutcome::LaunchFailed, RunVerdict::LaunchFailed),
        (RunOutcome::Skipped, RunVerdict::Skipped),
        (RunOutcome::Unknown, RunVerdict::Unknown),
    ] {
        let run = ScheduleRun {
            outcome,
            scheduled_for: Some(due),
        };
        assert_eq!(
            run_verdict(&run, true, past, deadline),
            verdict,
            "{outcome:?}"
        );
    }
}
