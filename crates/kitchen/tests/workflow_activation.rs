//! A workflow's declared capability requirements are enforced when its
//! schedule is installed, activated, or tried (#81): through the state store
//! before any intent is persisted, and by the Orca adapter before any Orca
//! change. Activation and trial carry Kitchen's recorded requirements; a
//! schedule whose requirements were never recorded, or whose Orca name
//! records others, is not started. Pausing it still works.
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

/// The budget tick as a binary from before requirements were recorded
/// stored it: without `requires`.
fn stored_before_requirements() -> TestResult<ScheduleSpec> {
    let mut json = serde_json::to_value(budget_tick()?)?;
    json.as_object_mut()
        .ok_or("a spec object")?
        .remove("requires")
        .ok_or("requires written")?;
    Ok(serde_json::from_value(json)?)
}

#[test]
fn schedule_requirements_persist_and_older_specs_decode_without_them() -> TestResult {
    let tick = budget_tick()?;
    assert_eq!(
        tick.requires(),
        Some(&BTreeSet::from(budget::REQUIRED_CAPABILITIES))
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

    // An empty set is recorded as such, while a spec stored before
    // requirements were recorded has none, not an empty set.
    let plain = serde_json::to_value(plain_schedule()?)?;
    assert_eq!(plain.get("requires"), Some(&serde_json::json!([])));
    assert_eq!(
        serde_json::from_value::<ScheduleSpec>(plain)?.requires(),
        Some(&BTreeSet::new())
    );
    assert_eq!(stored_before_requirements()?.requires(), None);

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

/// The requirement set an activation or trial carries from Kitchen's record.
fn requiring<const N: usize>(capabilities: [Capability; N]) -> Option<BTreeSet<Capability>> {
    Some(BTreeSet::from(capabilities))
}

fn activate(schedule: &ResourceRef, requires: Option<BTreeSet<Capability>>) -> ScheduleEffect {
    ScheduleEffect::SetState {
        schedule: schedule.clone(),
        state: ScheduleState::Active,
        requires,
    }
}

fn trial(schedule: &ResourceRef, requires: Option<BTreeSet<Capability>>) -> ScheduleEffect {
    ScheduleEffect::Trial {
        schedule: schedule.clone(),
        requires,
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
    let requires = requiring(budget::REQUIRED_CAPABILITIES);
    for (name, effect) in [
        ("activate", activate(&legacy, requires.clone())),
        ("trial", trial(&legacy, requires)),
    ] {
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
            requires: None,
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
    let recorded = requiring([Capability::SchedulePrecheck]);
    // The effect must carry exactly the recorded set: none, fewer, or other
    // requirements are refused before any intent.
    for (name, effect, mismatch) in [
        ("activate-unstated", activate(&schedule, None), false),
        ("trial-unstated", trial(&schedule, None), false),
        ("activate-fewer", activate(&schedule, requiring([])), true),
        (
            "trial-other",
            trial(&schedule, requiring([Capability::ScheduleRunTimeout])),
            true,
        ),
    ] {
        let result = run_on(&fixture, &installer, &task, name, effect)?;
        let refused = if mismatch {
            matches!(
                result,
                Err(Error::State(StateError::ScheduleRequirementsMismatch))
            )
        } else {
            matches!(
                result,
                Err(Error::State(StateError::ScheduleRequirementsUnknown))
            )
        };
        assert!(refused, "{name}: {result:?}");
    }
    assert!(fixture.store.task(&task.1)?.effects().is_empty());
    assert_eq!(installer.execute_calls(), 1, "only the install ran");

    let degraded =
        scheduler(manages_schedules().with(Capability::SchedulePrecheck, Support::Partial))?;
    for (name, effect) in [
        ("activate", activate(&schedule, recorded.clone())),
        ("trial", trial(&schedule, recorded.clone())),
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
        ("trial", trial(&schedule, recorded.clone())),
        ("activate", activate(&schedule, recorded.clone())),
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
        activate(&schedule, recorded),
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

fn orca_ref(id: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Schedule,
        backend: BackendId::new("orca-local")?,
        handle: ExternalRef::new(id)?,
    })
}

fn orca_request(key: &str, effect: ScheduleEffect) -> TestResult<EffectRequest> {
    Ok(EffectRequest::new(
        house()?,
        BackendId::new("orca-local")?,
        CredentialId::new("orca-host-session")?,
        task_id("activate")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new(key)?),
        effect.into(),
    ))
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
    let kitchen = requiring(budget::REQUIRED_CAPABILITIES);
    for id in ["legacy", "unknown"] {
        let schedule = orca_ref(id)?;
        assert_eq!(
            backend.set_schedule_state(&schedule, ScheduleState::Active, kitchen.as_ref()),
            Err(OrcaError::ScheduleRequirementsUnknown),
            "{id}"
        );
        assert_eq!(
            backend.trial_schedule(&schedule, kitchen.as_ref()),
            Err(OrcaError::ScheduleRequirementsUnknown),
            "{id}"
        );
    }
    let shrunk = orca_ref("shrunk")?;
    let recorded = requiring([Capability::ScheduleManage, Capability::ScheduleRunTimeout]);
    // Without Kitchen's requirements the name alone starts nothing.
    assert_eq!(
        backend.set_schedule_state(&shrunk, ScheduleState::Active, None),
        Err(OrcaError::ScheduleRequirementsUnknown)
    );
    assert_eq!(
        backend.trial_schedule(&shrunk, None),
        Err(OrcaError::ScheduleRequirementsUnknown)
    );
    let lacking = Err(OrcaError::Contract(
        ContractError::UnsupportedCapabilities {
            missing: vec![Capability::ScheduleRunTimeout],
            partial: Vec::new(),
        },
    ));
    assert_eq!(
        backend.set_schedule_state(&shrunk, ScheduleState::Active, recorded.as_ref()),
        lacking
    );
    assert_eq!(backend.trial_schedule(&shrunk, recorded.as_ref()), lacking);
    // Executed as effects, the refusals are definite.
    for (key, effect) in [
        (
            "activate-legacy",
            activate(&orca_ref("legacy")?, kitchen.clone()),
        ),
        ("trial-legacy", trial(&orca_ref("legacy")?, kitchen)),
        ("activate-shrunk", activate(&shrunk, recorded)),
        ("trial-unstated", trial(&shrunk, None)),
    ] {
        assert_eq!(
            backend.execute(&orca_request(key, effect)?),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            "{key}"
        );
    }
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(sim.calls_to(&["automations", "run"]).is_empty());

    // A legacy schedule can still be paused: that starts nothing.
    backend.set_schedule_state(&orca_ref("legacy")?, ScheduleState::Paused, None)?;
    assert_eq!(sim.calls_to(&["automations", "edit"]).len(), 1);

    // A schedule this adapter installs records its requirements and can
    // be tried and activated with Kitchen's matching set.
    let manage = requiring([Capability::ScheduleManage]);
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
    backend.trial_schedule(&installed, manage.as_ref())?;
    backend.set_schedule_state(&installed, ScheduleState::Active, manage.as_ref())?;
    Ok(())
}

#[test]
fn orca_does_not_start_a_renamed_schedule() -> TestResult {
    let sim = SimOrca::default();
    let backend = OrcaBackend::connect(orca_config(&sim)?, &sim)?;
    let kitchen = requiring([Capability::ScheduleManage]);
    let installed =
        backend.install_schedule(&plain_schedule()?.requiring([Capability::ScheduleManage]))?;
    // Renamed in Orca to record no, fewer, or other requirements than
    // Kitchen recorded at install. Orca supports every name below, so only
    // the comparison with Kitchen's set can refuse them.
    for renamed in [
        "kitchen:origin89:pickup:requires=",
        "kitchen:origin89:pickup:requires=effect.lookup",
        "kitchen:origin89:pickup:requires=schedule.manage,effect.lookup",
    ] {
        sim.state().automations.first_mut().ok_or("installed")?.name = renamed.to_owned();
        assert_eq!(
            backend.set_schedule_state(&installed, ScheduleState::Active, kitchen.as_ref()),
            Err(OrcaError::ScheduleRequirementsMismatch),
            "{renamed}"
        );
        assert_eq!(
            backend.trial_schedule(&installed, kitchen.as_ref()),
            Err(OrcaError::ScheduleRequirementsMismatch),
            "{renamed}"
        );
        for (key, effect) in [
            ("activate", activate(&installed, kitchen.clone())),
            ("trial", trial(&installed, kitchen.clone())),
        ] {
            assert_eq!(
                backend.execute(&orca_request(key, effect)?),
                Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
                "{renamed}: {key}"
            );
        }
    }
    assert!(sim.calls_to(&["automations", "edit"]).is_empty());
    assert!(sim.calls_to(&["automations", "run"]).is_empty());

    // With the recorded name restored, the same calls go through.
    sim.state().automations.first_mut().ok_or("installed")?.name =
        "kitchen:origin89:pickup:requires=schedule.manage".to_owned();
    backend.trial_schedule(&installed, kitchen.as_ref())?;
    backend.set_schedule_state(&installed, ScheduleState::Active, kitchen.as_ref())?;
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

#[test]
fn a_stored_spec_without_recorded_requirements_is_not_installed() -> TestResult {
    // A backend supporting everything the tick needs today still refuses
    // it: the stored spec cannot show what its workflow requires.
    let executor = full_scheduler()?;
    let (fixture, task, result) = run_install(&executor, install(stored_before_requirements()?))?;
    assert!(
        matches!(
            result,
            Err(Error::State(StateError::ScheduleRequirementsUnknown))
        ),
        "{result:?}"
    );
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(executor.execute_calls(), 0);

    let sim = SimOrca::default();
    let backend = OrcaBackend::connect(orca_config(&sim)?, &sim)?;
    let mut plain = serde_json::to_value(plain_schedule()?)?;
    plain
        .as_object_mut()
        .ok_or("a spec object")?
        .remove("requires");
    let legacy_plain: ScheduleSpec = serde_json::from_value(plain)?;
    assert_eq!(
        backend.install_schedule(&legacy_plain),
        Err(OrcaError::ScheduleRequirementsUnknown)
    );
    assert!(sim.calls_to(&["automations"]).is_empty(), "no Orca call");
    Ok(())
}
