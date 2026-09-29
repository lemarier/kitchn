//! A workflow's declared capability requirements are enforced when its
//! schedule is installed (#81): through the state store before any intent is
//! persisted, and by the Orca adapter before any Orca call.
//!
//! Executors are the in-memory fake and the simulated Orca runtime
//! (`orca_sim`); none of this is live runtime evidence.

mod common;
mod orca_sim;

use std::{collections::BTreeSet, time::Duration};

use common::{
    Fixture, ManualClock, TestResult, at, backend_id, creator, grant, grants_for, house, plan,
    scheduled, spec, task_id, ttl,
};
use kitchen::{
    BackendId, ConsumerId, CredentialId, Error, TaskId,
    adapters::orca::{OrcaBackend, OrcaConfig, OrcaError},
    contracts::{
        AttemptNumber, BranchName, Capability, CapabilitySet, ContractError, Effect,
        EffectExecutor, EffectFailure, EffectRequest, ExternalRef, Fence, HouseGrants,
        IdempotencyKey, NotAppliedReason, Permission, ResourceKind, ScheduleEffect, Support,
        TaskAuthority, Text, fake::FakeBackend,
    },
    scheduling::{AgentFamily, CronExpr, Recurrence, ScheduleSpec, Timezone, WorkflowName},
    selection::{AgentSelection, ResolvedSelection},
    state::{EffectState, run_effect},
    workflows::budget::{self, TickArgs},
};
use orca_sim::SimOrca;

fn agent() -> ResolvedSelection {
    ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Claude))
}

/// The budget tick's schedule, which declares
/// [`budget::REQUIRED_CAPABILITIES`].
fn budget_tick() -> TestResult<ScheduleSpec> {
    Ok(budget::install(
        Recurrence::Cron(CronExpr::new("15 * * * *")?),
        Timezone::new("UTC")?,
        agent(),
        &TickArgs {
            kitchen: "/opt/kitchen/bin/kitchn".into(),
            registry: "/var/kitchen/registry".into(),
            house: house()?,
            store: "/var/kitchen/store".into(),
            orca: "/opt/orca/bin/orca".into(),
            backend: BackendId::new("orca-local")?,
            credential: CredentialId::new("orca-host-session")?,
            runtime_dir: "/var/kitchen/orca".into(),
            report: None,
        },
    )?)
}

/// A schedule whose workflow declares no requirements.
fn plain_schedule() -> TestResult<ScheduleSpec> {
    Ok(ScheduleSpec::new(
        WorkflowName::new("pickup")?,
        ConsumerId::new("pickup")?,
        Recurrence::Hourly,
        Timezone::new("UTC")?,
        Text::new("Run pickup.")?,
        agent(),
    ))
}

fn install(schedule: ScheduleSpec) -> Effect {
    ScheduleEffect::InstallDisabled {
        schedule: schedule.into(),
    }
    .into()
}

/// A schedule executor declaring `capabilities` on the fixture's backend.
fn scheduler(capabilities: CapabilitySet) -> TestResult<FakeBackend> {
    Ok(FakeBackend::new(backend_id()?, house()?, capabilities))
}

/// Schedule management with lookup and idempotent requests.
fn manages_schedules() -> CapabilitySet {
    CapabilitySet::supporting([
        Capability::ScheduleManage,
        Capability::EffectLookup,
        Capability::EffectIdempotentRequests,
    ])
}

/// A claimed, running house-level task that may manage schedules.
fn schedule_task(fixture: &Fixture) -> TestResult<(HouseGrants, TaskId, Fence)> {
    let grants = grants_for(house()?, &[Permission::ManageSchedule])?;
    let mut work = spec("install")?;
    work.authority = TaskAuthority::delegate(&grants, vec![grant(Permission::ManageSchedule)?])?;
    let task = task_id("install")?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("installer")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((grants, task, fence))
}

/// Run `effect` on `executor` through the store.
fn run_install(
    executor: &FakeBackend,
    effect: Effect,
) -> TestResult<(Fixture, TaskId, Result<EffectState, Error>)> {
    let fixture = Fixture::new()?;
    let (grants, task, fence) = schedule_task(&fixture)?;
    let result = run_effect(
        &fixture.store,
        executor,
        &grants,
        plan(&task, fence, "install", effect)?,
        &ManualClock::starting_at(1),
    )
    .map(|record| record.state().clone());
    Ok((fixture, task, result))
}

#[test]
fn a_backend_missing_required_capabilities_is_refused_before_intent() -> TestResult {
    let executor =
        scheduler(manages_schedules().with(Capability::SchedulePrecheck, Support::Partial))?;
    let (fixture, task, result) = run_install(&executor, install(budget_tick()?))?;
    assert!(
        matches!(
            &result,
            Err(Error::Contract(ContractError::UnsupportedCapabilities { missing, partial }))
                if missing == &[Capability::ScheduleSingleConsumer, Capability::ScheduleRunTimeout]
                    && partial == &[Capability::SchedulePrecheck]
        ),
        "{result:?}"
    );
    assert!(
        fixture.store.task(&task)?.effects().is_empty(),
        "no intent persisted"
    );
    assert_eq!(executor.execute_calls(), 0, "nothing reached the executor");
    Ok(())
}

#[test]
fn partial_support_alone_is_refused_and_named() -> TestResult {
    let executor = scheduler(
        manages_schedules()
            .with(Capability::ScheduleSingleConsumer, Support::Supported)
            .with(Capability::ScheduleRunTimeout, Support::Supported)
            .with(Capability::SchedulePrecheck, Support::Partial),
    )?;
    let (fixture, task, result) = run_install(&executor, install(budget_tick()?))?;
    assert!(
        matches!(
            &result,
            Err(Error::Contract(ContractError::UnsupportedCapabilities { missing, partial }))
                if missing.is_empty() && partial == &[Capability::SchedulePrecheck]
        ),
        "{result:?}"
    );
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(executor.execute_calls(), 0);
    Ok(())
}

#[test]
fn a_backend_supporting_every_requirement_installs_the_schedule() -> TestResult {
    let executor = scheduler(CapabilitySet::supporting(
        [
            Capability::EffectLookup,
            Capability::EffectIdempotentRequests,
        ]
        .into_iter()
        .chain(budget::REQUIRED_CAPABILITIES),
    ))?;
    let (_fixture, _task, result) = run_install(&executor, install(budget_tick()?))?;
    let EffectState::Applied { receipt, .. } = result? else {
        return Err("schedule not installed".into());
    };
    assert!(matches!(receipt.created(), [resource] if resource.kind == ResourceKind::Schedule));
    assert_eq!(executor.effects_performed(), 1);
    Ok(())
}

#[test]
fn a_schedule_without_declared_requirements_needs_only_schedule_management() -> TestResult {
    let executor = scheduler(manages_schedules())?;
    let (_fixture, _task, result) = run_install(&executor, install(plain_schedule()?))?;
    assert!(matches!(result?, EffectState::Applied { .. }));
    Ok(())
}

fn orca_config(sim: &SimOrca) -> TestResult<OrcaConfig> {
    Ok(OrcaConfig {
        backend: BackendId::new("orca-local")?,
        house: house()?,
        credential: CredentialId::new("orca-host-session")?,
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

#[test]
fn orca_refuses_an_install_whose_workflow_needs_what_orca_lacks() -> TestResult {
    let sim = SimOrca::default();
    let backend = OrcaBackend::connect(orca_config(&sim)?, &sim)?;
    let refused = Err(OrcaError::Contract(
        ContractError::UnsupportedCapabilities {
            missing: vec![
                Capability::ScheduleSingleConsumer,
                Capability::ScheduleRunTimeout,
            ],
            partial: vec![Capability::SchedulePrecheck],
        },
    ));
    assert_eq!(backend.install_schedule(&budget_tick()?), refused);
    // Executed as an effect, the refusal is definite: nothing was applied.
    let request = EffectRequest::new(
        house()?,
        BackendId::new("orca-local")?,
        CredentialId::new("orca-host-session")?,
        task_id("install")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("install-1")?),
        install(budget_tick()?),
    );
    assert_eq!(
        backend.execute(&request),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert!(sim.calls_to(&["automations"]).is_empty(), "no Orca call");

    // A requirement Orca fully supports is no obstacle.
    let manageable = plain_schedule()?.requiring([Capability::ScheduleManage]);
    let installed = backend.install_schedule(&manageable)?;
    assert_eq!(installed.kind, ResourceKind::Schedule);
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    Ok(())
}

#[test]
fn schedule_requirements_persist_and_older_specs_decode_without_them() -> TestResult {
    let tick = budget_tick()?;
    assert_eq!(
        tick.requires(),
        &BTreeSet::from(budget::REQUIRED_CAPABILITIES)
    );
    let json = serde_json::to_value(&tick)?;
    assert_eq!(
        json.get("requires"),
        Some(&serde_json::json!([
            "schedule.manage",
            "schedule.precheck",
            "schedule.single_consumer",
            "schedule.run_timeout",
        ]))
    );
    assert_eq!(serde_json::from_value::<ScheduleSpec>(json.clone())?, tick);

    // A spec stored before requirements were recorded has none, and an
    // empty set is not written.
    let plain = serde_json::to_value(plain_schedule()?)?;
    assert_eq!(plain.get("requires"), None);
    assert!(
        serde_json::from_value::<ScheduleSpec>(plain)?
            .requires()
            .is_empty()
    );

    // An unknown capability name is rejected, not dropped.
    let mut unknown = json;
    if let Some(requires) = unknown.get_mut("requires") {
        *requires = serde_json::json!(["schedule.teleport"]);
    }
    assert!(serde_json::from_value::<ScheduleSpec>(unknown).is_err());
    Ok(())
}
