//! A workflow's declared capability requirements are enforced when its
//! schedule is installed, activated, or tried (#81): through the state store
//! before any intent is persisted, and by the Orca adapter before any Orca
//! change. A schedule whose requirements were never recorded is not started.
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
        IdempotencyKey, NotAppliedReason, Permission, ResourceKind, ResourceRef, ScheduleEffect,
        Support, TaskAuthority, Text, fake::FakeBackend,
    },
    scheduling::{
        AgentFamily, CronExpr, Recurrence, ScheduleField, ScheduleSpec, ScheduleState, Timezone,
        WorkflowName,
    },
    selection::{AgentSelection, ResolvedSelection},
    state::{EffectState, StateError, run_effect},
    workflows::budget::{self, TickArgs},
};
use orca_sim::{SimAutomation, SimOrca};

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

/// A schedule on the fixture's fake backend, such as one installed before
/// Kitchen recorded requirements.
fn fake_schedule(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Schedule,
        backend: backend_id()?,
        handle: ExternalRef::new(handle)?,
    })
}

/// A claimed, running task that may manage, activate, and trial the
/// schedules in `given`.
fn activation_task(
    fixture: &Fixture,
    id: &str,
    given: &[ResourceRef],
) -> TestResult<(HouseGrants, TaskId, Fence)> {
    let permissions = [
        Permission::ManageSchedule,
        Permission::ActivateSchedule,
        Permission::TrialSchedule,
    ];
    let grants = grants_for(house()?, &permissions)?;
    let mut work = spec(id)?;
    work.authority = TaskAuthority::delegate(
        &grants,
        permissions
            .into_iter()
            .map(grant)
            .collect::<TestResult<Vec<_>>>()?,
    )?;
    work.resources = given.iter().cloned().collect();
    let task = task_id(id)?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled(id)?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((grants, task, fence))
}

fn run_on(
    fixture: &Fixture,
    executor: &FakeBackend,
    (grants, task, fence): &(HouseGrants, TaskId, Fence),
    name: &str,
    effect: impl Into<Effect>,
) -> TestResult<Result<EffectState, Error>> {
    Ok(run_effect(
        &fixture.store,
        executor,
        grants,
        plan(task, *fence, name, effect)?,
        &ManualClock::starting_at(1),
    )
    .map(|record| record.state().clone()))
}

fn activate(schedule: &ResourceRef) -> ScheduleEffect {
    ScheduleEffect::SetState {
        schedule: schedule.clone(),
        state: ScheduleState::Active,
    }
}

fn trial(schedule: &ResourceRef) -> ScheduleEffect {
    ScheduleEffect::Trial {
        schedule: schedule.clone(),
    }
}

/// Supports every capability the budget tick declares.
fn full_scheduler() -> TestResult<FakeBackend> {
    scheduler(CapabilitySet::supporting(
        [
            Capability::EffectLookup,
            Capability::EffectIdempotentRequests,
        ]
        .into_iter()
        .chain(budget::REQUIRED_CAPABILITIES),
    ))
}

#[test]
fn a_schedule_installed_without_recorded_requirements_is_not_started() -> TestResult {
    let fixture = Fixture::new()?;
    let executor = full_scheduler()?;
    let legacy = fake_schedule("legacy-1")?;
    let task = activation_task(&fixture, "activate", std::slice::from_ref(&legacy))?;
    for (name, effect) in [("activate", activate(&legacy)), ("trial", trial(&legacy))] {
        let result = run_on(&fixture, &executor, &task, name, effect)?;
        assert!(
            matches!(
                result,
                Err(Error::State(StateError::ScheduleRequirementsUnknown))
            ),
            "{name}: {result:?}"
        );
    }
    assert!(fixture.store.task(&task.1)?.effects().is_empty());
    assert_eq!(executor.execute_calls(), 0);

    // Pausing starts nothing, so it needs no requirements.
    let paused = run_on(
        &fixture,
        &executor,
        &task,
        "pause",
        ScheduleEffect::SetState {
            schedule: legacy,
            state: ScheduleState::Paused,
        },
    )?;
    assert!(matches!(paused?, EffectState::Applied { .. }));
    Ok(())
}

#[test]
fn activation_and_trial_recheck_the_requirements_recorded_at_install() -> TestResult {
    let fixture = Fixture::new()?;
    let installer =
        scheduler(manages_schedules().with(Capability::SchedulePrecheck, Support::Supported))?;
    let installing = activation_task(&fixture, "install", &[])?;
    let installed = run_on(
        &fixture,
        &installer,
        &installing,
        "install",
        install(plain_schedule()?.requiring([Capability::SchedulePrecheck])),
    )??;
    let EffectState::Applied { receipt, .. } = installed else {
        return Err("schedule not installed".into());
    };
    let schedule = receipt.created().first().cloned().ok_or("a schedule")?;

    // Another task given the schedule, on a backend that now supports its
    // precheck only in part, starts nothing.
    let task = activation_task(&fixture, "activate", std::slice::from_ref(&schedule))?;
    let degraded =
        scheduler(manages_schedules().with(Capability::SchedulePrecheck, Support::Partial))?;
    for (name, effect) in [
        ("activate", activate(&schedule)),
        ("trial", trial(&schedule)),
    ] {
        let result = run_on(&fixture, &degraded, &task, name, effect)?;
        assert!(
            matches!(
                &result,
                Err(Error::Contract(ContractError::UnsupportedCapabilities { missing, partial }))
                    if missing.is_empty() && partial == &[Capability::SchedulePrecheck]
            ),
            "{name}: {result:?}"
        );
    }
    assert!(fixture.store.task(&task.1)?.effects().is_empty());
    assert_eq!(degraded.execute_calls(), 0);

    // The installing backend supports it.
    for (name, effect) in [
        ("trial", trial(&schedule)),
        ("activate", activate(&schedule)),
    ] {
        let result = run_on(&fixture, &installer, &task, name, effect)?;
        assert!(matches!(result?, EffectState::Applied { .. }), "{name}");
    }

    // A removed schedule's requirements are forgotten.
    run_on(
        &fixture,
        &installer,
        &task,
        "remove",
        ScheduleEffect::Remove {
            schedule: schedule.clone(),
        },
    )??;
    let again = run_on(
        &fixture,
        &installer,
        &task,
        "activate-again",
        activate(&schedule),
    )?;
    assert!(
        matches!(
            again,
            Err(Error::State(StateError::ScheduleRequirementsUnknown))
        ),
        "{again:?}"
    );
    Ok(())
}

#[test]
fn orca_does_not_start_a_schedule_whose_requirements_it_cannot_establish() -> TestResult {
    let sim = SimOrca::default();
    let legacy = |id: &str, name: &str, enabled: bool| SimAutomation {
        id: id.to_owned(),
        name: name.to_owned(),
        enabled,
        ..SimAutomation::default()
    };
    {
        let mut state = sim.state();
        // Installed before requirements were recorded in the name.
        state
            .automations
            .push(legacy("legacy", "kitchen:origin89:gardener", false));
        // Recorded with a requirement Orca lacks, as if Orca's support shrank.
        state.automations.push(legacy(
            "shrunk",
            "kitchen:origin89:budget:requires=schedule.manage,schedule.run_timeout",
            false,
        ));
        // Recorded with a capability this Kitchen does not know.
        state.automations.push(legacy(
            "unknown",
            "kitchen:origin89:triage:requires=schedule.teleport",
            false,
        ));
    }
    let backend = OrcaBackend::connect(orca_config(&sim)?, &sim)?;
    let orca = |id: &str| -> TestResult<ResourceRef> {
        Ok(ResourceRef {
            kind: ResourceKind::Schedule,
            backend: BackendId::new("orca-local")?,
            handle: ExternalRef::new(id)?,
        })
    };
    for id in ["legacy", "unknown"] {
        let schedule = orca(id)?;
        assert_eq!(
            backend.set_schedule_state(&schedule, ScheduleState::Active),
            Err(OrcaError::ScheduleRequirementsUnknown),
            "{id}"
        );
        assert_eq!(
            backend.trial_schedule(&schedule),
            Err(OrcaError::ScheduleRequirementsUnknown),
            "{id}"
        );
    }
    let shrunk = orca("shrunk")?;
    let lacking = Err(OrcaError::Contract(
        ContractError::UnsupportedCapabilities {
            missing: vec![Capability::ScheduleRunTimeout],
            partial: Vec::new(),
        },
    ));
    assert_eq!(
        backend.set_schedule_state(&shrunk, ScheduleState::Active),
        lacking
    );
    assert_eq!(backend.trial_schedule(&shrunk), lacking);
    // Executed as effects, the refusals are definite.
    for (key, effect) in [
        ("activate-legacy", activate(&orca("legacy")?)),
        ("trial-legacy", trial(&orca("legacy")?)),
        ("activate-shrunk", activate(&shrunk)),
    ] {
        let request = EffectRequest::new(
            house()?,
            BackendId::new("orca-local")?,
            CredentialId::new("orca-host-session")?,
            task_id("activate")?,
            AttemptNumber::FIRST,
            IdempotencyKey::from_ref(ExternalRef::new(key)?),
            effect.into(),
        );
        assert_eq!(
            backend.execute(&request),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            "{key}"
        );
    }
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(sim.calls_to(&["automations", "run"]).is_empty());

    // Pausing starts nothing and stays available.
    backend.set_schedule_state(&orca("legacy")?, ScheduleState::Paused)?;

    // A schedule this adapter installs records its requirements and can
    // be tried and activated.
    let installed =
        backend.install_schedule(&plain_schedule()?.requiring([Capability::ScheduleManage]))?;
    let creates = sim.calls_to(&["automations", "create"]);
    let create = creates.first().ok_or("one create")?;
    assert!(
        create
            .iter()
            .any(|arg| arg == "--name=kitchen:origin89:pickup:requires=schedule.manage"),
        "{create:?}"
    );
    backend.trial_schedule(&installed)?;
    backend.set_schedule_state(&installed, ScheduleState::Active)?;
    Ok(())
}

#[test]
fn orca_does_not_reuse_a_matching_schedule_without_recorded_requirements() -> TestResult {
    let sim = SimOrca::default();
    let backend = OrcaBackend::connect(orca_config(&sim)?, &sim)?;
    backend.install_schedule(&plain_schedule()?)?;
    // Rename it to the pre-requirements form, as an older install left it.
    {
        let mut state = sim.state();
        let automation = state.automations.first_mut().ok_or("installed")?;
        assert_eq!(automation.name, "kitchen:origin89:pickup:requires=");
        automation.name = "kitchen:origin89:pickup".to_owned();
    }
    assert_eq!(
        backend.install_schedule(&plain_schedule()?),
        Err(OrcaError::ScheduleDiffers {
            fields: vec![ScheduleField::Requirements]
        })
    );
    assert_eq!(sim.calls_to(&["automations", "create"]).len(), 1);
    Ok(())
}
