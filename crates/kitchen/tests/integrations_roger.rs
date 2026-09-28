//! Sanitized Roger answer fixtures; no live Roger calls.
use kitchen::{
    HouseId, TaskId,
    contracts::{CommitId, ExternalRef, Permission, Repository, Text},
    integrations::{
        github::{CredentialRef, HouseScope, IntegrationError, PostingBudget},
        roger::*,
    },
};
use serde_json::{Value, json};
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
fn fixture() -> Result<(HouseScope, DecisionBinding, Value)> {
    let house = HouseId::new("sample")?;
    let requester = ExternalRef::new("sample-gate")?;
    let repo = Repository::new("sample/project")?;
    let scope = HouseScope::new(
        house.clone(),
        [repo.clone()],
        requester.clone(),
        CredentialRef::new(house.clone(), ExternalRef::new("roger-gate")?, requester),
        PostingBudget::new(3)?,
        [Permission::AskHuman],
    )?;
    let binding = DecisionBinding {
        house,
        task: TaskId::new("task-1")?,
        owner: DecisionOwner::Merge,
        repository: repo,
        action: Permission::Merge,
        target: ExternalRef::new("pr:sample/project#1")?,
        revision: CommitId::new(&"a".repeat(40))?,
        limits: Text::new("squash into main")?,
    };
    let action = json!({"verb":"merge","target":"pr:sample/project#1","rev":"a".repeat(40),"limits":"squash into main"});
    let ask = json!({"id":"ask-1","requester":"sample-gate","repo":"sample/project","decisionKey":binding.decision_key()?,"kind":"approval","action":action,"resume":{"task":"task-1","rev":"a".repeat(40)},"state":"answered","supersededBy":null,"answer":{"decision":"approve","optionId":"approve","action":action,"input":null,"passkey":true}});
    Ok((scope, binding, ask))
}
fn status(
    scope: &HouseScope,
    binding: &DecisionBinding,
    ask: &Value,
) -> Result<std::result::Result<DecisionStatus, IntegrationError>> {
    Ok(validate_answer(
        scope,
        binding,
        &ExternalRef::new("ask-1")?,
        &serde_json::to_vec(ask)?,
    ))
}
#[test]
fn exact_answer_and_key_survive_serialization_restart() -> Result {
    let (scope, binding, ask) = fixture()?;
    let restarted: DecisionBinding = serde_json::from_slice(&serde_json::to_vec(&binding)?)?;
    assert_eq!(binding.decision_key()?, restarted.decision_key()?);
    assert_eq!(
        status(&scope, &restarted, &ask)?,
        Ok(DecisionStatus::Approved)
    );
    Ok(())
}
#[test]
fn cross_house_requester_task_target_and_revision_are_rejected() -> Result {
    let (scope, binding, ask) = fixture()?;
    for (pointer, value, error) in [
        (
            "/requester",
            json!("foreign-gate"),
            IntegrationError::ScopeMismatch,
        ),
        (
            "/decisionKey",
            json!("merge:foreign:task-1:merge:pr:sample/project#1"),
            IntegrationError::ScopeMismatch,
        ),
        (
            "/repo",
            json!("foreign/project"),
            IntegrationError::ScopeMismatch,
        ),
        (
            "/resume/task",
            json!("task-2"),
            IntegrationError::ScopeMismatch,
        ),
        (
            "/action/target",
            json!("pr:sample/project#2"),
            IntegrationError::ScopeMismatch,
        ),
        (
            "/action/rev",
            json!("b".repeat(40)),
            IntegrationError::StaleDecision,
        ),
        (
            "/resume/rev",
            json!("b".repeat(40)),
            IntegrationError::StaleDecision,
        ),
        (
            "/answer/action/limits",
            json!("any branch"),
            IntegrationError::ScopeMismatch,
        ),
    ] {
        let mut changed = ask.clone();
        *changed
            .pointer_mut(pointer)
            .ok_or("invalid fixture pointer")? = value;
        assert_eq!(status(&scope, &binding, &changed)?, Err(error), "{pointer}");
    }
    Ok(())
}
#[test]
fn no_answer_expiry_supersession_rejection_and_custom_input_are_not_approval() -> Result {
    let (scope, binding, ask) = fixture()?;
    for (state, expected) in [
        ("open", DecisionStatus::Unanswered),
        ("expired", DecisionStatus::Expired),
        ("withdrawn", DecisionStatus::Closed),
        ("superseded", DecisionStatus::Closed),
    ] {
        let mut changed = ask.clone();
        changed["state"] = json!(state);
        changed["answer"] = Value::Null;
        assert_eq!(status(&scope, &binding, &changed)?, Ok(expected));
    }
    let mut changed = ask.clone();
    changed["answer"]["decision"] = json!("reject");
    changed["answer"]["optionId"] = json!("reject");
    assert_eq!(
        status(&scope, &binding, &changed)?,
        Ok(DecisionStatus::Rejected)
    );
    changed["answer"]["decision"] = json!("other");
    changed["answer"]["optionId"] = json!("_custom");
    changed["answer"]["input"] = json!("Please revise");
    assert_eq!(
        status(&scope, &binding, &changed)?,
        Ok(DecisionStatus::Instructions(Some(Text::new(
            "Please revise"
        )?)))
    );
    Ok(())
}
#[test]
fn ambiguous_answer_unknown_prefix_and_oversized_response_fail_closed() -> Result {
    let (scope, binding, ask) = fixture()?;
    for (pointer, value) in [
        ("/answer", Value::Null),
        ("/answer/passkey", json!(false)),
        ("/answer/decision", json!("future")),
        ("/kind", json!("question")),
    ] {
        let mut changed = ask.clone();
        *changed
            .pointer_mut(pointer)
            .ok_or("invalid fixture pointer")? = value;
        assert_eq!(
            status(&scope, &binding, &changed)?,
            Err(IntegrationError::Unknown)
        );
    }
    assert!(serde_json::from_str::<DecisionOwner>("\"legacy\"").is_err());
    assert_eq!(
        validate_answer(
            &scope,
            &binding,
            &ExternalRef::new("ask-1")?,
            &vec![b' '; 128 * 1024 + 1]
        ),
        Err(IntegrationError::LimitExceeded)
    );
    Ok(())
}
