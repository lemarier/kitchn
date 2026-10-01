//! House agent-selection policy, its record on a task, and its launch
//! through the Orca adapter against the simulated Orca runtime (`orca_sim`).
//!
//! Every result here is simulated. No live agent or model was launched.

mod common;
mod orca_sim;

use std::{collections::BTreeSet, time::Duration};

use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, house, scheduled, task_id, ttl,
};
use kitchen::{
    BackendId, ConsumerId, CredentialId, Error, TaskId,
    adapters::orca::{OrcaBackend, OrcaConfig, OrcaError, WORKER_SELECTION},
    adoption::HouseRegistry,
    contracts::{
        AttemptNumber, AttemptOutcome, BranchName, Capability, CapabilityRequirements, Effect,
        EffectExecutor, EffectFailure, EffectRequest, EvidenceRevision, ExternalRef, FailureClass,
        Fence, Grant, HouseGrants, IdempotencyKey, NotAppliedReason, Operation, Permission,
        Provenance, Repository, RetryPolicy, Role, ScheduleBackend, TaskAuthority, TaskSpec, Text,
        Workspace,
    },
    contracts::{CapabilitySet, ContractError, Support, fake::FakeBackend},
    house::{
        AccessStatus, DoctorCode, DoctorEvidence, HouseConfig, HouseError, RepositoryConfig,
        Workflow, doctor,
    },
    scheduling::{AgentFamily, Recurrence, ScheduleSpec, Timezone, WorkflowName},
    selection::{
        AgentModel, AgentPolicy, AgentSelection, EffortLevel, EffortSupport, MAX_SELECTION_RULES,
        OfferedModels, ResolvedSelection, RuleMatch, SelectionError, SelectionGap,
        SelectionRequest, SelectionRule, SelectionSource, SelectionSupport, TaskGroup, WorkType,
    },
    state::{EffectPlan, EffectStart, EffectState, StateError, run_effect},
};
use orca_sim::SimOrca;
use serde_json::json;

fn selection(
    agent: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
) -> TestResult<AgentSelection> {
    Ok(AgentSelection {
        agent,
        model: model.map(AgentModel::new).transpose()?,
        effort: effort.map(EffortLevel::new).transpose()?,
    })
}

/// The house policy from the #5 owner requirement: frontier implementation,
/// a lighter second-validation reviewer, and cheap small fix rounds.
fn policy_json() -> serde_json::Value {
    json!({
        "default": {"agent": "codex", "model": "gpt-6-sol", "effort": "high"},
        "rules": [
            {"when": {"role": "inspector"}, "use": {"agent": "claude", "model": "sonnet"}},
            {"when": {"role": "expediter"}, "use": {"agent": "claude", "model": "opus", "effort": "high"}},
            {"when": {"workType": "fix"}, "use": {"agent": "claude", "model": "sonnet", "effort": "low"}},
            {"when": {"role": "station-cook", "workType": "fix"}, "use": {"agent": "codex", "model": "gpt-6-mini"}},
            {"when": {"repository": "origin89hq/firmware", "role": "station-cook"}, "use": {"agent": "codex", "model": "gpt-6-sol", "effort": "xhigh"}},
            {"when": {"taskGroup": "release-1"}, "use": {"agent": "claude", "model": "opus"}}
        ]
    })
}

fn policy() -> TestResult<AgentPolicy> {
    Ok(serde_json::from_value(policy_json())?)
}

fn firmware() -> TestResult<Repository> {
    Ok(Repository::new("origin89hq/firmware")?)
}

fn house_config(agents: serde_json::Value) -> TestResult<HouseConfig> {
    Ok(serde_json::from_value(json!({
        "schema": 1,
        "house": "origin89",
        "kitchen": "a".repeat(40),
        "guidance": "b".repeat(40),
        "repositories": ["origin89hq/firmware"],
        "postingDestinations": [],
        "requiredReviewers": [],
        "requiredChecks": [],
        "policyLimits": [],
        "grants": [],
        "agents": agents,
    }))?)
}

#[test]
fn the_house_default_applies_when_no_rule_matches() -> TestResult {
    let policy = policy()?;
    let resolved = policy.resolve(&SelectionRequest::new(Role::StationCook));
    assert_eq!(resolved.source, SelectionSource::HouseDefault);
    assert_eq!(
        resolved.selection,
        selection(AgentFamily::Codex, Some("gpt-6-sol"), Some("high"))?
    );
    assert_eq!(
        resolved.selection.attribution_model()?.as_str(),
        "codex:gpt-6-sol"
    );

    // With no model configured the record says the agent default, not a guess.
    let bare: AgentPolicy = serde_json::from_value(json!({"default": {"agent": "claude"}}))?;
    let resolved = bare.resolve(&SelectionRequest::new(Role::Inspector));
    assert_eq!(
        resolved.selection,
        AgentSelection::agent_default(AgentFamily::Claude)
    );
    assert_eq!(resolved.selection.attribution_model()?.as_str(), "claude");
    Ok(())
}

#[test]
fn role_work_type_repository_and_group_rules_override_in_order() -> TestResult {
    let policy = policy()?;
    let request = |role,
                   work_type: Option<&str>,
                   repository: Option<Repository>,
                   group: Option<&str>|
     -> TestResult<SelectionRequest> {
        Ok(SelectionRequest {
            role,
            work_type: work_type.map(WorkType::new).transpose()?,
            repository,
            task_group: group.map(TaskGroup::new).transpose()?,
        })
    };

    // A lighter second-validation reviewer than the implementer.
    let review = policy.resolve(&request(Role::Inspector, None, None, None)?);
    assert_eq!(review.source, SelectionSource::HouseRule);
    assert_eq!(
        review.selection,
        selection(AgentFamily::Claude, Some("sonnet"), None)?
    );

    // Role and work type together outrank work type alone.
    let fix = policy.resolve(&request(Role::StationCook, Some("fix"), None, None)?);
    assert_eq!(
        fix.selection,
        selection(AgentFamily::Codex, Some("gpt-6-mini"), None)?
    );
    let other_fix = policy.resolve(&request(Role::Commis, Some("fix"), None, None)?);
    assert_eq!(
        other_fix.selection,
        selection(AgentFamily::Claude, Some("sonnet"), Some("low"))?
    );

    // A repository rule outranks house-wide rules, even a more specific one.
    let firmware_fix = policy.resolve(&request(
        Role::StationCook,
        Some("fix"),
        Some(firmware()?),
        None,
    )?);
    assert_eq!(
        firmware_fix.source,
        SelectionSource::Repository {
            repository: firmware()?
        }
    );
    assert_eq!(
        firmware_fix.selection,
        selection(AgentFamily::Codex, Some("gpt-6-sol"), Some("xhigh"))?
    );
    // ...but only for the role it names; other roles fall through to house rules.
    let firmware_review = policy.resolve(&request(Role::Inspector, None, Some(firmware()?), None)?);
    assert_eq!(firmware_review.source, SelectionSource::HouseRule);

    // A task group outranks the repository.
    let release = policy.resolve(&request(
        Role::StationCook,
        None,
        Some(firmware()?),
        Some("release-1"),
    )?);
    assert_eq!(
        release.source,
        SelectionSource::TaskGroup {
            group: TaskGroup::new("release-1")?
        }
    );
    assert_eq!(
        release.selection,
        selection(AgentFamily::Claude, Some("opus"), None)?
    );
    // An unknown group falls through.
    let other = policy.resolve(&request(Role::StationCook, None, None, Some("release-2"))?);
    assert_eq!(other.source, SelectionSource::HouseDefault);
    Ok(())
}

#[test]
fn a_repository_binding_cannot_override_house_agent_policy() -> TestResult {
    let binding = json!({
        "schema": 1,
        "house": "origin89",
        "repository": "origin89hq/firmware",
        "workflows": [],
        "additionalReviewers": [],
        "additionalChecks": [],
        "agents": {"default": {"agent": "claude", "model": "opus"}},
    });
    assert!(serde_json::from_value::<RepositoryConfig>(binding).is_err());

    let house = house_config(policy_json())?;
    house.validate()?;
    assert_eq!(house.agents, Some(policy()?));
    // A house without an agents key keeps loading unchanged.
    let none: HouseConfig = serde_json::from_value(json!({
        "schema": 1, "house": "origin89", "kitchen": "a".repeat(40), "guidance": "b".repeat(40),
        "repositories": ["origin89hq/firmware"], "postingDestinations": [], "requiredReviewers": [],
        "requiredChecks": [], "policyLimits": [], "grants": [],
    }))?;
    assert_eq!(none.agents, None);
    Ok(())
}

#[test]
fn invalid_policies_are_rejected() -> TestResult {
    let default = json!({"agent": "codex"});
    let rules = |rules: serde_json::Value| json!({"default": default, "rules": rules});
    let cases = [
        (
            rules(json!([{"when": {"repository": "origin89hq/km43"}, "use": default}])),
            HouseError::PolicyRelaxation,
        ),
        (
            rules(json!([{"when": {}, "use": default}])),
            HouseError::InvalidInput,
        ),
        (
            rules(
                json!([{"when": {"taskGroup": "g", "repository": "origin89hq/firmware"}, "use": default}]),
            ),
            HouseError::InvalidInput,
        ),
        (
            rules(
                json!([{"when": {"role": "inspector"}, "use": default}, {"when": {"role": "inspector"}, "use": {"agent": "claude"}}]),
            ),
            HouseError::InvalidInput,
        ),
    ];
    for (agents, expected) in cases {
        let error = house_config(agents)?.validate().err();
        assert_eq!(
            error.map(|error| error.to_string()),
            Some(expected.to_string())
        );
    }
    let direct: AgentPolicy = serde_json::from_value(rules(json!([{"when": {}, "use": default}])))?;
    assert_eq!(
        direct.validate(&BTreeSet::new()),
        Err(SelectionError::UnscopedCatchAll)
    );

    // Malformed values never load: unknown family, a flag-like model id, an
    // unknown key, and an unknown role.
    for agents in [
        json!({"default": {"agent": "gpt"}}),
        json!({"default": {"agent": "codex", "model": "--dangerously"}}),
        json!({"default": {"agent": "codex", "reasoning": "high"}}),
        json!({"default": {"agent": "codex"}, "rules": [{"when": {"role": "chef"}, "use": {"agent": "codex"}}]}),
    ] {
        assert!(house_config(agents).is_err());
    }
    Ok(())
}

#[test]
fn rule_count_is_bounded() -> TestResult {
    let rule = |index: usize| -> TestResult<SelectionRule> {
        Ok(SelectionRule {
            when: RuleMatch {
                task_group: Some(TaskGroup::new(&format!("g{index}"))?),
                ..RuleMatch::default()
            },
            selection: AgentSelection::agent_default(AgentFamily::Claude),
        })
    };
    let mut policy = AgentPolicy {
        default: AgentSelection::agent_default(AgentFamily::Codex),
        rules: (0..MAX_SELECTION_RULES)
            .map(rule)
            .collect::<TestResult<_>>()?,
    };
    assert_eq!(policy.validate(&BTreeSet::new()), Ok(()));
    policy.rules.push(rule(MAX_SELECTION_RULES)?);
    assert_eq!(
        policy.validate(&BTreeSet::new()),
        Err(SelectionError::TooManyRules)
    );
    Ok(())
}

#[test]
fn unsupported_selections_name_every_gap() -> TestResult {
    let effort_only = selection(AgentFamily::Codex, None, Some("high"))?;
    let error = WORKER_SELECTION.check(&effort_only).err();
    assert_eq!(
        error,
        Some(SelectionError::Unsupported {
            agent: AgentFamily::Codex,
            gaps: vec![SelectionGap::EffortWithoutModel]
        })
    );
    assert!(
        error
            .map(|error| error.to_string())
            .is_some_and(|text| text.contains("agent.select_model")),
        "the message names the missing capability"
    );
    assert_eq!(
        WORKER_SELECTION.check(&selection(
            AgentFamily::Claude,
            Some("sonnet"),
            Some("low")
        )?),
        Ok(())
    );

    let nothing = kitchen::selection::SelectionSupport {
        families: &[AgentFamily::Claude],
        model: false,
        effort: kitchen::selection::EffortSupport::Unsupported,
    };
    assert_eq!(
        nothing.gaps(&selection(
            AgentFamily::Codex,
            Some("gpt-6-sol"),
            Some("high")
        )?),
        vec![
            SelectionGap::Family(AgentFamily::Codex),
            SelectionGap::Model,
            SelectionGap::Effort
        ]
    );
    assert_eq!(
        Error::from(SelectionError::Unsupported {
            agent: AgentFamily::Codex,
            gaps: vec![SelectionGap::Model]
        })
        .class(),
        kitchen::ErrorClass::Refused
    );
    Ok(())
}

#[test]
fn doctor_offered_models_flag_unknown_configured_models() -> TestResult {
    let policy = policy()?;
    let offered = vec![
        OfferedModels {
            agent: AgentFamily::Codex,
            models: BTreeSet::from([AgentModel::new("gpt-6-sol")?]),
        },
        OfferedModels {
            agent: AgentFamily::Claude,
            models: BTreeSet::from([AgentModel::new("sonnet")?, AgentModel::new("opus")?]),
        },
    ];
    let unknown = policy.unoffered_models(&offered);
    assert_eq!(
        unknown,
        vec![(AgentFamily::Codex, AgentModel::new("gpt-6-mini")?)]
    );
    // The same model id under another family is not offered.
    let claude_only = vec![OfferedModels {
        agent: AgentFamily::Claude,
        models: BTreeSet::from([AgentModel::new("gpt-6-sol")?]),
    }];
    assert!(
        policy
            .unoffered_models(&claude_only)
            .contains(&(AgentFamily::Codex, AgentModel::new("gpt-6-sol")?))
    );
    assert!(policy.unoffered_models(&[]).len() == policy.configured_models().len());
    Ok(())
}

#[test]
fn doctor_reports_configured_models_the_agents_do_not_offer() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let house = house_config(policy_json())?;
    registry.initialize(&house)?;
    let repository: RepositoryConfig = serde_json::from_value(json!({
        "schema": 2, "house": "origin89", "repository": "origin89hq/firmware",
        "workflows": [], "additionalReviewers": [], "additionalChecks": [],
    }))?;
    let model_findings = |evidence: Option<&DoctorEvidence>| -> TestResult<Vec<String>> {
        Ok(doctor(&registry, &repository, evidence)?
            .findings
            .into_iter()
            .filter(|finding| finding.code == DoctorCode::AgentModel)
            .map(|finding| finding.message)
            .collect())
    };

    // Unobserved models are a gap, not success.
    let [unchecked] = model_findings(None)?
        .try_into()
        .map_err(|_| "one finding")?;
    assert!(unchecked.contains("not checked"));
    assert!(unchecked.contains("codex:gpt-6-mini") && unchecked.contains("claude:sonnet"));

    let mut evidence = DoctorEvidence {
        house: house.house.clone(),
        repository: firmware()?,
        capabilities: CapabilitySet::new(),
        labels: None,
        access: AccessStatus::Unobserved,
        schedules: None,
        undelivered_budget_reports: Vec::new(),
        store_capacity: None,
        agent_models: Some(vec![
            OfferedModels {
                agent: AgentFamily::Codex,
                models: BTreeSet::from([AgentModel::new("gpt-6-sol")?]),
            },
            OfferedModels {
                agent: AgentFamily::Claude,
                models: BTreeSet::from([AgentModel::new("sonnet")?, AgentModel::new("opus")?]),
            },
        ]),
        stack_tool: None,
        readiness: None,
    };
    let [unknown] = model_findings(Some(&evidence))?
        .try_into()
        .map_err(|_| "one finding")?;
    assert!(
        unknown.contains("do not offer: codex:gpt-6-mini."),
        "{unknown}"
    );

    if let Some(offered) = evidence
        .agent_models
        .as_mut()
        .and_then(|offered| offered.first_mut())
    {
        offered.models.insert(AgentModel::new("gpt-6-mini")?);
    }
    assert!(model_findings(Some(&evidence))?.is_empty());

    // A house without an agent policy has nothing to check.
    let temp = tempfile::tempdir()?;
    let plain = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    let mut without = house.clone();
    without.agents = None;
    plain.initialize(&without)?;
    assert!(
        doctor(&plain, &repository, None)?
            .findings
            .iter()
            .all(|finding| finding.code != DoctorCode::AgentModel)
    );
    Ok(())
}

#[test]
fn doctor_reports_scheduled_workflows_the_schedule_backend_cannot_select_for() -> TestResult {
    let temp = tempfile::tempdir()?;
    let registry = HouseRegistry::new(temp.path().canonicalize()?.join("registry"))?;
    // Pickup falls to the model-naming default; gardener has a family-only
    // rule; triage (also the gardener role) names only an effort.
    let agents = json!({
        "default": {"agent": "codex", "model": "gpt-6-sol"},
        "rules": [
            {"when": {"role": "gardener"}, "use": {"agent": "claude"}},
            {"when": {"role": "gardener", "workType": "triage"}, "use": {"agent": "codex", "effort": "low"}}
        ]
    });
    let house = house_config(agents.clone())?;
    registry.initialize(&house)?;
    let repository: RepositoryConfig = serde_json::from_value(json!({
        "schema": 2, "house": "origin89", "repository": "origin89hq/firmware",
        "workflows": ["pickup", "gardener", "triage"], "additionalReviewers": [], "additionalChecks": [],
    }))?;
    let evidence = |support: Support| -> TestResult<DoctorEvidence> {
        Ok(DoctorEvidence {
            house: house.house.clone(),
            repository: firmware()?,
            capabilities: CapabilitySet::new().with(Capability::AgentSelectModel, support),
            labels: None,
            access: AccessStatus::Unobserved,
            schedules: None,
            undelivered_budget_reports: Vec::new(),
            store_capacity: None,
            agent_models: None,
            stack_tool: None,
            readiness: None,
        })
    };
    let schedule_findings = |evidence: Option<&DoctorEvidence>| -> TestResult<Vec<String>> {
        Ok(doctor(&registry, &repository, evidence)?
            .findings
            .into_iter()
            .filter(|finding| finding.code == DoctorCode::ScheduleAgent)
            .map(|finding| finding.message)
            .collect())
    };

    // Orca declares model selection partial: automations take only a provider.
    let partial = evidence(Support::Partial)?;
    let [pickup, triage] = schedule_findings(Some(&partial))?
        .try_into()
        .map_err(|_| "two findings")?;
    assert!(
        pickup.starts_with("Scheduled pickup resolves to codex model gpt-6-sol,"),
        "{pickup}"
    );
    assert!(
        triage.starts_with("Scheduled triage resolves to codex effort low,"),
        "{triage}"
    );
    // Unobserved capabilities are not success either.
    assert_eq!(schedule_findings(None)?.len(), 2);
    // A schedule backend that can select a model enforces the selection.
    assert!(schedule_findings(Some(&evidence(Support::Supported)?))?.is_empty());
    Ok(())
}

// --- Store and Orca adapter: record, launch, retry, and policy change. ---

fn orca_id() -> TestResult<BackendId> {
    Ok(BackendId::new("orca-local")?)
}

fn credential() -> TestResult<CredentialId> {
    Ok(CredentialId::new("orca-host-session")?)
}

fn connect(sim: &SimOrca) -> TestResult<OrcaBackend<&SimOrca>> {
    Ok(OrcaBackend::connect(
        OrcaConfig {
            backend: orca_id()?,
            house: house()?,
            credential: credential()?,
            run: ExternalRef::new("run_sim")?,
            coordinator: ExternalRef::new("term_coordinator")?,
            repo: ExternalRef::new("id:repo-1")?,
            base_branch: None,
            branch_prefix: Some(BranchName::new("lemarier")?),
            agent: AgentFamily::Claude,
            call_timeout: Duration::from_secs(5),
            launch_timeout: Duration::from_secs(60),
            runtime_dir: sim.runtime_dir()?,
            reservation_timeout: Duration::from_secs(10),
        },
        sim,
    )?)
}

fn grants() -> TestResult<HouseGrants> {
    Ok(HouseGrants::new(
        house()?,
        [Grant::house(
            Permission::LaunchWorker,
            orca_id()?,
            credential()?,
        )],
    ))
}

fn spec(id: &str, agent: Option<ResolvedSelection>) -> TestResult<TaskSpec> {
    Ok(TaskSpec {
        id: task_id(id)?,
        role: Role::StationCook,
        repository: None,
        authority: TaskAuthority::delegate(
            &grants()?,
            [Grant::house(
                Permission::LaunchWorker,
                orca_id()?,
                credential()?,
            )],
        )?,
        retry: RetryPolicy::new(3, Duration::from_secs(3600))?,
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
        resources: BTreeSet::new(),
        requires: CapabilityRequirements::new(),
        agent,
        work_type: None,
    })
}

fn launch(agent: Option<AgentSelection>) -> TestResult<Operation> {
    Ok(Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new("Fix the failing check.")?,
        branch: None,
        pinned: None,
        agent,
    })
}

fn plan(task: &TaskId, fence: Fence, name: &str, effect: Operation) -> TestResult<EffectPlan> {
    Ok(EffectPlan {
        task: task.clone(),
        fence,
        name: kitchen::EffectName::new(name)?,
        decided_at: EvidenceRevision::INITIAL,
        effect: Effect::from(effect),
        consent: None,
        basis: None,
    })
}

fn flag(call: &[String], name: &str) -> Option<String> {
    call.iter()
        .find_map(|arg| arg.strip_prefix(&format!("--{name}=")).map(str::to_owned))
}

/// `(agent, model, effort)` of every `worker-start` call so far.
fn starts(sim: &SimOrca) -> Vec<(Option<String>, Option<String>, Option<String>)> {
    sim.calls_to(&["orchestration", "worker-start"])
        .iter()
        .map(|call| {
            (
                flag(call, "agent"),
                flag(call, "model"),
                flag(call, "effort"),
            )
        })
        .collect()
}

fn owned(
    values: (&str, Option<&str>, Option<&str>),
) -> (Option<String>, Option<String>, Option<String>) {
    (
        Some(values.0.to_owned()),
        values.1.map(str::to_owned),
        values.2.map(str::to_owned),
    )
}

#[test]
fn a_retry_launches_with_the_recorded_selection_after_a_policy_change() -> TestResult {
    let fixture = Fixture::new()?;
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let clock = ManualClock::starting_at(1);
    let request = SelectionRequest {
        work_type: Some(WorkType::new("fix")?),
        ..SelectionRequest::new(Role::StationCook)
    };

    let before = policy()?.resolve(&request);
    let task = task_id("task-fix")?;
    fixture
        .store
        .create_task(spec("task-fix", Some(before.clone()))?, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let pinned = fixture.reopen()?.task(&task)?.spec().agent.clone();
    assert_eq!(
        pinned.as_ref(),
        Some(&before),
        "the resolved selection is durable"
    );

    let first = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(
            &task,
            fence,
            "launch",
            launch(Some(before.selection.clone()))?,
        )?,
        &clock,
    )?;
    assert!(matches!(first.state(), EffectState::Applied { .. }));
    fixture.store.finish_attempt(
        &task,
        fence,
        AttemptNumber::FIRST,
        AttemptOutcome::Failed(FailureClass::Retryable),
        at(1),
    )?;

    // The owner edits the house policy while the task is active.
    let mut changed = policy_json();
    if let Some(rules) = changed
        .get_mut("rules")
        .and_then(serde_json::Value::as_array_mut)
    {
        rules.retain(|rule| rule["when"]["workType"] != json!("fix"));
    }
    let after: AgentPolicy = serde_json::from_value(changed)?;
    let now_resolves = after.resolve(&request);
    assert_ne!(now_resolves.selection, before.selection);

    fixture.store.start_attempt(&task, fence, at(2))?;
    // Launching with the new policy's answer is refused before anything reaches Orca...
    let refused = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(
            &task,
            fence,
            "relaunch",
            launch(Some(now_resolves.selection.clone()))?,
        )?,
        &clock,
    );
    assert!(matches!(
        refused,
        Err(Error::State(StateError::AgentSelectionMismatch))
    ));
    assert_eq!(starts(&sim).len(), 1);
    // ...and so is a launch that drops the selection for the backend default.
    let dropped = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "relaunch", launch(None)?)?,
        &clock,
    );
    assert!(matches!(
        dropped,
        Err(Error::State(StateError::AgentSelectionMismatch))
    ));

    // The retry launches with the recorded selection.
    let recorded = fixture
        .store
        .task(&task)?
        .spec()
        .agent
        .clone()
        .map(|resolved| resolved.selection);
    let second = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "relaunch", launch(recorded)?)?,
        &clock,
    )?;
    assert!(matches!(second.state(), EffectState::Applied { .. }));
    let expected = owned(("codex", Some("gpt-6-mini"), None));
    assert_eq!(starts(&sim), vec![expected.clone(), expected]);

    // A task created after the change resolves under the new policy.
    fixture.store.create_task(
        spec("task-new", Some(now_resolves.clone()))?,
        &creator()?,
        at(3),
    )?;
    assert_eq!(
        fixture
            .store
            .task(&task_id("task-new")?)?
            .spec()
            .agent
            .as_ref(),
        Some(&now_resolves)
    );
    Ok(())
}

fn pickup_schedule(agent: ResolvedSelection) -> TestResult<ScheduleSpec> {
    Ok(ScheduleSpec::new(
        WorkflowName::new("pickup")?,
        ConsumerId::new("pickup")?,
        Recurrence::Hourly,
        Timezone::new("UTC")?,
        Text::new("Run Kitchen pickup.")?,
        agent,
    ))
}

#[test]
fn a_scheduled_trigger_stays_family_only_and_its_worker_carries_the_model() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let trigger = Workflow::Pickup.schedule_request()?;

    // A policy whose default names a model cannot install the trigger:
    // Orca automations would run the agent's default model instead.
    let unscoped: AgentPolicy =
        serde_json::from_value(json!({"default": {"agent": "codex", "model": "gpt-6-sol"}}))?;
    let refused = unscoped.resolve(&trigger);
    assert_eq!(
        backend.install_schedule(&pickup_schedule(refused)?),
        Err(OrcaError::Selection(SelectionError::Unsupported {
            agent: AgentFamily::Codex,
            gaps: vec![SelectionGap::Model],
        }))
    );
    assert!(sim.calls_to(&["automations", "create"]).is_empty());

    // A family-only rule for scheduled pickup installs the cheap trigger.
    let policy: AgentPolicy = serde_json::from_value(json!({
        "default": {"agent": "codex", "model": "gpt-6-sol"},
        "rules": [{"when": {"role": "sous-chef", "workType": "pickup"}, "use": {"agent": "codex"}}]
    }))?;
    let scheduled_pickup = policy.resolve(&trigger);
    assert_eq!(scheduled_pickup.source, SelectionSource::HouseRule);
    backend.install_schedule(&pickup_schedule(scheduled_pickup)?)?;
    let creates = sim.calls_to(&["automations", "create"]);
    let create = creates.first().ok_or("one create")?;
    assert_eq!(flag(create, "provider").as_deref(), Some("codex"));
    assert_eq!(flag(create, "model"), None);

    // The work the trigger hands off launches as a worker with the model the
    // same policy resolves for it, recorded on the task.
    let fixture = Fixture::new()?;
    let clock = ManualClock::starting_at(1);
    let work = policy.resolve(&SelectionRequest::new(Role::StationCook));
    let task = task_id("task-picked")?;
    fixture
        .store
        .create_task(spec("task-picked", Some(work.clone()))?, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("pickup")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let launched = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(
            &task,
            fence,
            "launch",
            launch(Some(work.selection.clone()))?,
        )?,
        &clock,
    )?;
    assert!(matches!(launched.state(), EffectState::Applied { .. }));
    assert_eq!(
        starts(&sim),
        vec![owned(("codex", Some("gpt-6-sol"), None))]
    );
    assert_eq!(
        fixture.store.task(&task)?.spec().agent.as_ref(),
        Some(&work)
    );
    Ok(())
}

#[test]
fn an_owner_change_is_a_new_task_not_an_edit() -> TestResult {
    let fixture = Fixture::new()?;
    let original = policy()?.resolve(&SelectionRequest::new(Role::StationCook));
    fixture
        .store
        .create_task(spec("task-a", Some(original))?, &creator()?, at(0))?;
    // Re-creating the same task id with another selection is a conflict.
    let owner = ResolvedSelection::owner(selection(AgentFamily::Claude, Some("opus"), None)?);
    let edited =
        fixture
            .store
            .create_task(spec("task-a", Some(owner.clone()))?, &creator()?, at(1));
    assert!(matches!(
        edited,
        Err(Error::State(StateError::TaskConflict(_)))
    ));
    fixture
        .store
        .create_task(spec("task-a-2", Some(owner.clone()))?, &creator()?, at(1))?;
    let stored = fixture
        .store
        .task(&task_id("task-a-2")?)?
        .spec()
        .agent
        .clone();
    assert_eq!(
        stored.map(|resolved| resolved.source),
        Some(SelectionSource::Owner)
    );
    Ok(())
}

#[test]
fn orca_passes_the_selection_and_refuses_what_it_cannot_provide() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    let request = |effect: Operation, key: &str| -> TestResult<EffectRequest> {
        Ok(EffectRequest::new(
            house()?,
            orca_id()?,
            credential()?,
            task_id("task-1")?,
            AttemptNumber::FIRST,
            IdempotencyKey::from_ref(ExternalRef::new(key)?),
            Effect::from(effect),
        ))
    };

    backend.execute(&request(
        launch(Some(selection(
            AgentFamily::Codex,
            Some("gpt-6-sol"),
            Some("xhigh"),
        )?))?,
        "k-1",
    )?)?;
    backend.execute(&request(
        launch(Some(AgentSelection::agent_default(AgentFamily::Codex)))?,
        "k-2",
    )?)?;
    backend.execute(&request(launch(None)?, "k-3")?)?;
    assert_eq!(
        starts(&sim),
        vec![
            owned(("codex", Some("gpt-6-sol"), Some("xhigh"))),
            owned(("codex", None, None)),
            owned(("claude", None, None)),
        ]
    );

    // Orca accepts --effort only with --model: refused before any Task, with the gap named.
    let tasks_before = sim.calls_to(&["orchestration", "task-create"]).len();
    let refused = backend.execute(&request(
        launch(Some(selection(AgentFamily::Claude, None, Some("high"))?))?,
        "k-4",
    )?);
    assert_eq!(
        refused.err(),
        Some(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
            Capability::AgentSelectModel
        )))
    );
    assert_eq!(
        sim.calls_to(&["orchestration", "task-create"]).len(),
        tasks_before
    );
    assert_eq!(starts(&sim).len(), 3);
    Ok(())
}

/// A started task holding `agent`, ready for `run_effect`.
fn started(
    fixture: &Fixture,
    id: &str,
    agent: Option<ResolvedSelection>,
) -> TestResult<(TaskId, Fence)> {
    let task = task_id(id)?;
    fixture
        .store
        .create_task(spec(id, agent)?, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("coordinator")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

fn fake(support: Option<SelectionSupport>) -> TestResult<FakeBackend> {
    let backend = FakeBackend::new(
        orca_id()?,
        house()?,
        CapabilitySet::supporting(Capability::ALL),
    );
    Ok(match support {
        Some(support) => backend.with_worker_selection(support),
        None => backend,
    })
}

fn missing<T>(result: Result<T, Error>) -> Option<Vec<Capability>> {
    match result {
        Err(Error::Contract(ContractError::UnsupportedCapabilities { missing, partial }))
            if partial.is_empty() =>
        {
            Some(missing)
        }
        _ => None,
    }
}

const CLAUDE_ONLY: SelectionSupport = SelectionSupport {
    families: &[AgentFamily::Claude],
    model: false,
    effort: EffortSupport::Unsupported,
};

#[test]
fn the_store_refuses_a_selection_the_executor_does_not_declare() -> TestResult {
    let fixture = Fixture::new()?;
    let clock = ManualClock::starting_at(1);
    let pinned = ResolvedSelection::owner(selection(
        AgentFamily::Codex,
        Some("gpt-6-sol"),
        Some("high"),
    )?);
    let (task, fence) = started(&fixture, "task-gate", Some(pinned.clone()))?;
    // Declaring nothing: family, model, and effort are all unprovided.
    let silent = fake(None)?;
    let refused = run_effect(
        &fixture.store,
        &silent,
        &grants()?,
        plan(
            &task,
            fence,
            "none",
            launch(Some(pinned.selection.clone()))?,
        )?,
        &clock,
    );
    assert_eq!(
        missing(refused),
        Some(vec![
            Capability::AgentSelectFamily,
            Capability::AgentSelectModel
        ])
    );
    // Refused before the executor was called, and before any intent was kept.
    assert_eq!(silent.execute_calls(), 0);
    assert!(silent.launched_agents().is_empty());

    // Declaring a family the selection does not use, and no model or effort.
    let claude_only = fake(Some(CLAUDE_ONLY))?;
    let refused = run_effect(
        &fixture.store,
        &claude_only,
        &grants()?,
        plan(
            &task,
            fence,
            "claude",
            launch(Some(pinned.selection.clone()))?,
        )?,
        &clock,
    );
    assert_eq!(
        missing(refused),
        Some(vec![
            Capability::AgentSelectFamily,
            Capability::AgentSelectModel
        ])
    );
    assert_eq!(claude_only.execute_calls(), 0);

    // The same name is still free: a capable executor launches exactly the
    // recorded selection.
    let capable = FakeBackend::fully_capable(orca_id()?, house()?);
    let applied = run_effect(
        &fixture.store,
        &capable,
        &grants()?,
        plan(
            &task,
            fence,
            "none",
            launch(Some(pinned.selection.clone()))?,
        )?,
        &clock,
    )?;
    assert!(matches!(applied.state(), EffectState::Applied { .. }));
    assert_eq!(capable.launched_agents(), vec![Some(pinned.selection)]);
    Ok(())
}

#[test]
fn begin_effect_checks_the_selection_against_the_executor_it_is_given() -> TestResult {
    let fixture = Fixture::new()?;
    let pinned = ResolvedSelection::owner(selection(AgentFamily::Codex, Some("gpt-6-sol"), None)?);
    let (task, fence) = started(&fixture, "task-bound", Some(pinned.clone()))?;
    let effects = |fixture: &Fixture| -> TestResult<usize> {
        Ok(fixture
            .store
            .tasks()?
            .iter()
            .map(|task| task.effects().len())
            .sum())
    };

    // The executor that will run the launch declares only Claude; its own
    // descriptor decides, so no intent is persisted for the Codex launch.
    let claude_only = fake(Some(CLAUDE_ONLY))?;
    let refused = fixture.store.begin_effect(
        plan(
            &task,
            fence,
            "launch",
            launch(Some(pinned.selection.clone()))?,
        )?,
        &grants()?,
        &claude_only,
        at(1),
    );
    assert_eq!(
        missing(refused),
        Some(vec![
            Capability::AgentSelectFamily,
            Capability::AgentSelectModel
        ])
    );
    assert_eq!(effects(&fixture)?, 0);
    assert_eq!(claude_only.execute_calls(), 0);

    // An executor that declares the selection gets an intent recorded under
    // its own namespace.
    let capable = FakeBackend::fully_capable(orca_id()?, house()?);
    let EffectStart::Execute(intent) = fixture.store.begin_effect(
        plan(&task, fence, "launch", launch(Some(pinned.selection))?)?,
        &grants()?,
        &capable,
        at(2),
    )?
    else {
        return Err("expected a new launch intent".into());
    };
    assert_eq!(intent.request().backend(), &capable.descriptor().backend);
    assert_eq!(effects(&fixture)?, 1);
    Ok(())
}

#[test]
fn the_store_names_each_gap_of_a_partly_supported_selection() -> TestResult {
    let fixture = Fixture::new()?;
    let clock = ManualClock::starting_at(1);
    let effort_only = selection(AgentFamily::Claude, None, Some("high"))?;
    let (task, fence) = started(
        &fixture,
        "task-effort",
        Some(ResolvedSelection::owner(effort_only.clone())),
    )?;
    // The fully capable fake, like Orca, accepts an effort only with a model.
    let backend = FakeBackend::fully_capable(orca_id()?, house()?);
    let refused = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch(Some(effort_only))?)?,
        &clock,
    );
    assert_eq!(missing(refused), Some(vec![Capability::AgentSelectModel]));
    assert_eq!(backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn a_task_without_a_selection_launches_on_an_executor_that_declares_none() -> TestResult {
    let fixture = Fixture::new()?;
    let clock = ManualClock::starting_at(1);
    let (task, fence) = started(&fixture, "task-plain", None)?;
    let backend = fake(None)?;
    let applied = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch(None)?)?,
        &clock,
    )?;
    assert!(matches!(applied.state(), EffectState::Applied { .. }));
    assert_eq!(backend.launched_agents(), vec![None]);

    // A launch may not add a selection the task never recorded.
    let extra = run_effect(
        &fixture.store,
        &FakeBackend::fully_capable(orca_id()?, house()?),
        &grants()?,
        plan(
            &task,
            fence,
            "second",
            launch(Some(AgentSelection::agent_default(AgentFamily::Codex)))?,
        )?,
        &clock,
    );
    assert!(matches!(
        extra,
        Err(Error::State(StateError::AgentSelectionMismatch))
    ));
    Ok(())
}

#[test]
fn the_fake_refuses_what_it_does_not_declare_and_records_what_it_launches() -> TestResult {
    let request = |agent: Option<AgentSelection>, key: &str| -> TestResult<EffectRequest> {
        Ok(EffectRequest::new(
            house()?,
            orca_id()?,
            credential()?,
            task_id("task-1")?,
            AttemptNumber::FIRST,
            IdempotencyKey::from_ref(ExternalRef::new(key)?),
            Effect::from(launch(agent)?),
        ))
    };
    let undeclared = fake(None)?;
    let asked = selection(AgentFamily::Codex, Some("gpt-6-sol"), None)?;
    assert_eq!(
        undeclared
            .execute(&request(Some(asked.clone()), "k-1")?)
            .err(),
        Some(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
            Capability::AgentSelectFamily
        )))
    );
    assert_eq!(undeclared.effects_performed(), 0);
    assert!(undeclared.launched_agents().is_empty());

    let capable = FakeBackend::fully_capable(orca_id()?, house()?);
    capable.execute(&request(Some(asked.clone()), "k-2")?)?;
    capable.execute(&request(None, "k-3")?)?;
    assert_eq!(capable.launched_agents(), vec![Some(asked), None]);
    Ok(())
}

#[test]
fn orca_declares_the_selection_its_adapter_enforces() -> TestResult {
    let sim = SimOrca::default();
    let backend = connect(&sim)?;
    assert_eq!(
        backend.descriptor().worker_selection,
        Some(WORKER_SELECTION)
    );
    let fixture = Fixture::new()?;
    let clock = ManualClock::starting_at(1);

    // The store admits what Orca declares and Orca launches it.
    let pinned = ResolvedSelection::owner(selection(
        AgentFamily::Claude,
        Some("sonnet"),
        Some("high"),
    )?);
    let (task, fence) = started(&fixture, "task-orca", Some(pinned.clone()))?;
    run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&task, fence, "launch", launch(Some(pinned.selection))?)?,
        &clock,
    )?;
    assert_eq!(
        starts(&sim),
        vec![owned(("claude", Some("sonnet"), Some("high")))]
    );

    // An effort without a model is refused by the store, before Orca sees it.
    let bare_effort = selection(AgentFamily::Claude, None, Some("high"))?;
    let (other, other_fence) = started(
        &fixture,
        "task-orca-2",
        Some(ResolvedSelection::owner(bare_effort.clone())),
    )?;
    let refused = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&other, other_fence, "launch", launch(Some(bare_effort))?)?,
        &clock,
    );
    assert_eq!(missing(refused), Some(vec![Capability::AgentSelectModel]));
    assert_eq!(starts(&sim).len(), 1);
    assert_eq!(sim.calls_to(&["orchestration", "task-create"]).len(), 1);
    Ok(())
}

#[test]
fn a_launch_naming_a_selection_the_task_never_recorded_is_refused() -> TestResult {
    let fixture = Fixture::new()?;
    let clock = ManualClock::starting_at(1);
    let backend = FakeBackend::fully_capable(orca_id()?, house()?);
    let (task, fence) = started(&fixture, "task-none", None)?;
    let refused = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(
            &task,
            fence,
            "launch",
            launch(Some(selection(
                AgentFamily::Codex,
                Some("gpt-6-sol"),
                None,
            )?))?,
        )?,
        &clock,
    );
    assert!(matches!(
        refused,
        Err(Error::State(StateError::AgentSelectionMismatch))
    ));
    assert_eq!(backend.execute_calls(), 0);

    // A recorded selection is not replaced by the backend default.
    let pinned = ResolvedSelection::owner(selection(AgentFamily::Codex, Some("gpt-6-sol"), None)?);
    let (pinned_task, pinned_fence) = started(&fixture, "task-some", Some(pinned))?;
    let dropped = run_effect(
        &fixture.store,
        &backend,
        &grants()?,
        plan(&pinned_task, pinned_fence, "launch", launch(None)?)?,
        &clock,
    );
    assert!(matches!(
        dropped,
        Err(Error::State(StateError::AgentSelectionMismatch))
    ));
    assert_eq!(backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn a_repository_rule_without_a_role_does_not_replace_the_reviewer_default() -> TestResult {
    let policy: AgentPolicy = serde_json::from_value(json!({
        "default": {"agent": "codex", "model": "gpt-6-sol", "effort": "high"},
        "rules": [
            {"when": {"role": "inspector"}, "use": {"agent": "claude", "model": "sonnet"}},
            {"when": {"repository": "origin89hq/firmware"}, "use": {"agent": "codex", "model": "gpt-6-sol", "effort": "xhigh"}},
            {"when": {"taskGroup": "release-1"}, "use": {"agent": "claude", "model": "opus"}}
        ]
    }))?;
    let repositories = BTreeSet::from([firmware()?]);
    policy.validate(&repositories)?;
    let in_firmware = |role| SelectionRequest {
        repository: Some(firmware().ok()).flatten(),
        ..SelectionRequest::new(role)
    };

    // The house chose a lighter reviewer; a repository-wide rule leaves it.
    let review = policy.resolve(&in_firmware(Role::Inspector));
    assert_eq!(review.source, SelectionSource::HouseRule);
    assert_eq!(
        review.selection,
        selection(AgentFamily::Claude, Some("sonnet"), None)?
    );
    // The same for a group rule without a role.
    let group_review = policy.resolve(&SelectionRequest {
        task_group: Some(TaskGroup::new("release-1")?),
        ..in_firmware(Role::Inspector)
    });
    assert_eq!(group_review.source, SelectionSource::HouseRule);

    // Roles the house did not single out still take the repository rule.
    let cook = policy.resolve(&in_firmware(Role::StationCook));
    assert_eq!(
        cook.source,
        SelectionSource::Repository {
            repository: firmware()?
        }
    );

    // A repository rule that names the role does override the house rule.
    let named: AgentPolicy = serde_json::from_value(json!({
        "default": {"agent": "codex"},
        "rules": [
            {"when": {"role": "inspector"}, "use": {"agent": "claude", "model": "sonnet"}},
            {"when": {"repository": "origin89hq/firmware", "role": "inspector"}, "use": {"agent": "codex", "model": "gpt-6-sol"}}
        ]
    }))?;
    let review = named.resolve(&in_firmware(Role::Inspector));
    assert_eq!(
        review.selection,
        selection(AgentFamily::Codex, Some("gpt-6-sol"), None)?
    );
    Ok(())
}

#[test]
fn a_repository_work_type_rule_applies_unless_the_house_names_role_and_work_type() -> TestResult {
    let policy: AgentPolicy = serde_json::from_value(json!({
        "default": {"agent": "codex", "model": "gpt-6-sol", "effort": "high"},
        "rules": [
            {"when": {"role": "inspector"}, "use": {"agent": "claude", "model": "sonnet"}},
            {"when": {"role": "expediter", "workType": "docs"}, "use": {"agent": "claude", "model": "opus"}},
            {"when": {"repository": "origin89hq/firmware", "workType": "docs"}, "use": {"agent": "codex", "model": "gpt-6-mini"}},
            {"when": {"repository": "origin89hq/firmware"}, "use": {"agent": "codex", "model": "gpt-6-sol", "effort": "xhigh"}}
        ]
    }))?;
    policy.validate(&BTreeSet::from([firmware()?]))?;
    let request = |role, work_type: Option<&str>| -> TestResult<SelectionRequest> {
        Ok(SelectionRequest {
            role,
            work_type: work_type.map(WorkType::new).transpose()?,
            repository: Some(firmware()?),
            task_group: None,
        })
    };
    let lighter = selection(AgentFamily::Codex, Some("gpt-6-mini"), None)?;

    // The house rule names only the role, so the repository's docs rule
    // still applies to the reviewer's docs work.
    let docs_review = policy.resolve(&request(Role::Inspector, Some("docs"))?);
    assert_eq!(
        docs_review.source,
        SelectionSource::Repository {
            repository: firmware()?
        }
    );
    assert_eq!(docs_review.selection, lighter);
    // A role the house does not name takes it too.
    let docs_cook = policy.resolve(&request(Role::StationCook, Some("docs"))?);
    assert_eq!(docs_cook.selection, lighter);

    // Without the work type, the repository-wide rule still yields.
    let review = policy.resolve(&request(Role::Inspector, None)?);
    assert_eq!(review.source, SelectionSource::HouseRule);
    assert_eq!(
        review.selection,
        selection(AgentFamily::Claude, Some("sonnet"), None)?
    );
    // Another work type falls through to the repository-wide rule, which
    // still yields to the house's reviewer choice.
    let fix_review = policy.resolve(&request(Role::Inspector, Some("fix"))?);
    assert_eq!(fix_review.source, SelectionSource::HouseRule);

    // A house rule naming the role and the same work type keeps precedence.
    let docs_gate = policy.resolve(&request(Role::Expediter, Some("docs"))?);
    assert_eq!(docs_gate.source, SelectionSource::HouseRule);
    assert_eq!(
        docs_gate.selection,
        selection(AgentFamily::Claude, Some("opus"), None)?
    );
    // Its other work goes to the repository, as no house rule names it.
    let gate = policy.resolve(&request(Role::Expediter, None)?);
    assert_eq!(
        gate.selection,
        selection(AgentFamily::Codex, Some("gpt-6-sol"), Some("xhigh"))?
    );
    Ok(())
}

#[test]
fn launches_and_specs_persisted_before_selection_still_load() -> TestResult {
    // A LaunchWorker operation and a task spec written before agent selection
    // existed carry no `agent`; they load as "no selection".
    let mut written = serde_json::to_value(launch(None)?)?;
    assert!(
        written.get("agent").is_none(),
        "an absent selection is not written"
    );
    let operation: Operation = serde_json::from_value(written.clone())?;
    assert!(matches!(
        operation,
        Operation::LaunchWorker { agent: None, .. }
    ));

    written["agent"] = serde_json::to_value(AgentSelection::agent_default(AgentFamily::Codex))?;
    let with_agent: Operation = serde_json::from_value(written)?;
    assert!(matches!(
        with_agent,
        Operation::LaunchWorker { agent: Some(_), .. }
    ));

    let mut stored = serde_json::to_value(spec("task-old", None)?)?;
    assert!(stored.get("agent").is_none());
    let reloaded: TaskSpec = serde_json::from_value(stored.clone())?;
    assert_eq!(reloaded.agent, None);
    // A spec that carries one round-trips exactly.
    let pinned = policy()?.resolve(&SelectionRequest::new(Role::Inspector));
    stored = serde_json::to_value(spec("task-new", Some(pinned.clone()))?)?;
    let reloaded: TaskSpec = serde_json::from_value(stored)?;
    assert_eq!(reloaded.agent, Some(pinned));
    Ok(())
}
