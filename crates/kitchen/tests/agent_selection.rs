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
    BackendId, CredentialId, Error, TaskId,
    adapters::orca::{OrcaBackend, OrcaConfig, WORKER_SELECTION},
    adoption::HouseRegistry,
    contracts::CapabilitySet,
    contracts::{
        AttemptNumber, AttemptOutcome, BranchName, Capability, CapabilityRequirements, Effect,
        EffectExecutor, EffectFailure, EffectRequest, EvidenceRevision, ExternalRef, FailureClass,
        Fence, Grant, HouseGrants, IdempotencyKey, NotAppliedReason, Operation, Permission,
        Provenance, Repository, RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Workspace,
    },
    house::{
        AccessStatus, DoctorCode, DoctorEvidence, HouseConfig, HouseError, RepositoryConfig, doctor,
    },
    scheduling::AgentFamily,
    selection::{
        AgentModel, AgentPolicy, AgentSelection, EffortLevel, MAX_SELECTION_RULES, OfferedModels,
        ResolvedSelection, RuleMatch, SelectionError, SelectionGap, SelectionRequest,
        SelectionRule, SelectionSource, TaskGroup, WorkType,
    },
    state::{EffectPlan, EffectState, StateError, run_effect},
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
        "schema": 1, "house": "origin89", "repository": "origin89hq/firmware",
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
    })
}

fn launch(agent: Option<AgentSelection>) -> TestResult<Operation> {
    Ok(Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new("Fix the failing check.")?,
        branch: None,
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
