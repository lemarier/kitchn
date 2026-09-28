//! Sanitized Roger answer fixtures; no live Roger calls.
use kitchen::{
    HouseId, TaskId,
    contracts::{CommitId, EvidenceSubject, ExternalRef, Permission, Repository, Text},
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
        CredentialRef::new(
            house.clone(),
            kitchen::CredentialId::new("roger-gate")?,
            requester,
        ),
        PostingBudget::new(3)?,
        [Permission::AskHuman],
    )?;
    let binding = DecisionBinding {
        house,
        task: TaskId::new("t01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
        owner: DecisionOwner::Merge,
        repository: repo,
        action: Permission::Merge,
        target: ExternalRef::new("pr:sample/project#1")?,
        revision: kitchen::contracts::EvidenceRevision::INITIAL,
        subject: Some(EvidenceSubject {
            head: CommitId::new(&"a".repeat(40))?,
            base: None,
        }),
        limits: Text::new("squash into main")?,
    };
    let action = json!({"verb":"merge","target":"pr:sample/project#1","rev":"a".repeat(40),"limits":"squash into main"});
    let ask = json!({"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","requester":"sample-gate","repo":"sample/project","decisionKey":binding.decision_key()?,"kind":"approval","action":action,"resume":{"task":"t01ARZ3NDEKTSV4RRFFQ69G5FAV","rev":"a".repeat(40)},"state":"answered","supersededBy":null,"answer":{"decision":"approve","optionId":"approve","action":action,"input":null,"passkey":true}});
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
        &ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
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
            json!("merge:foreign:t01ARZ3NDEKTSV4RRFFQ69G5FAV:merge:pr:sample/project#1"),
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
    // Roger resumes at a head; a binding without an evidence subject has none.
    let mut unbound = binding;
    unbound.subject = None;
    assert_eq!(
        status(&scope, &unbound, &ask)?,
        Err(IntegrationError::InvalidInput)
    );
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
            &ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
            &vec![b' '; 128 * 1024 + 1]
        ),
        Err(IntegrationError::LimitExceeded)
    );
    Ok(())
}

#[derive(Default)]
struct Offline(std::cell::Cell<u32>);
impl RogerReadTransport for Offline {
    fn get(
        &self,
        _: &CredentialRef,
        _: &ExternalRef,
        _: std::time::Duration,
        _: usize,
    ) -> std::result::Result<Vec<u8>, IntegrationError> {
        self.0.set(self.0.get() + 1);
        Err(IntegrationError::Unavailable)
    }
}
#[test]
fn offline_recovery_does_not_create_a_new_ask() -> Result {
    let (scope, binding, _) = fixture()?;
    let client = RogerClient::new(
        scope,
        Offline::default(),
        kitchen::integrations::github::ReadLimits::default(),
    );
    let id = ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?;
    assert_eq!(
        client.poll(&binding, &id),
        Err(IntegrationError::Unavailable)
    );
    assert_eq!(
        client.poll(&binding, &id),
        Err(IntegrationError::Unavailable)
    );
    let mut foreign = binding;
    foreign.house = HouseId::new("foreign")?;
    assert_eq!(
        client.poll(&foreign, &id),
        Err(IntegrationError::ScopeMismatch)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn optional_roger_capability_detection_needs_no_credentials() -> Result {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let binary = dir.path().join("roger");
    assert_eq!(RogerCli::detect(&binary)?, RogerAvailability::NotInstalled);
    std::fs::write(&binary, "#!/bin/sh\nprintf '%s' 'unsupported old cli'\n")?;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
    assert_eq!(RogerCli::detect(&binary)?, RogerAvailability::Unsupported);
    std::fs::write(
        &binary,
        "#!/bin/sh\n[ -z \"$ROGER_TOKEN\" ] || exit 2\nprintf '%s' '--idem --decision-key --action-rev --action-target --action-limits --resume-task --resume-rev --body-file'\n",
    )?;
    assert_eq!(RogerCli::detect(&binary)?, RogerAvailability::Available);
    std::fs::write(&binary, "#!/bin/sh\nexit 1\n")?;
    assert_eq!(
        RogerCli::detect(&binary),
        Err(IntegrationError::Unavailable)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn roger_cli_uses_selected_house_url_and_isolated_token_for_get_find_submit() -> Result {
    use kitchen::contracts::IdempotencyKey;
    use kitchen::integrations::github::CredentialFile;
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    let (scope, binding, mut receipt) = fixture()?;
    receipt["title"] = json!("-review");
    receipt["body"] = json!("fixture body");
    let directory = tempfile::tempdir()?;
    let executable = directory.path().join("roger");
    let log = directory.path().join("calls");
    let token_path = directory.path().join("token");
    std::fs::write(&token_path, "fixture-roger-token")?;
    let script = format!(
        r##"#!/bin/sh
if [ "$1" = ask ] && [ "$2" = --help ]; then printf '%s' '--idem --decision-key --action-rev --action-target --action-limits --resume-task --resume-rev --body-file'; exit 0; fi
[ "$ROGER_URL" = https://roger.example.test ] || exit 8
[ "$ROGER_TOKEN" = fixture-roger-token ] || exit 8
[ -z "$GH_TOKEN" ] || exit 8
for arg in "$@"; do [ "$arg" != fixture-roger-token ] || exit 8; done
printf '%s\n' "$*" >> '{}'
case "$1" in
  get) printf '%s' '{{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","requester":"sample-gate"}}' ;;
  list) printf '%s' '{{"asks":[]}}' ;;
  ask) input=$(/bin/cat); [ "$input" = 'fixture body' ] || exit 8; printf '%s' '{}' ;;
  *) exit 8 ;;
esac
"##,
        log.display(),
        receipt
    );
    std::fs::write(&executable, script)?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let credential = CredentialFile::new(scope.credential().clone(), token_path)?;
    let probe = ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?;
    assert!(matches!(
        RogerCli::new(
            executable.clone(),
            credential.clone(),
            probe.clone(),
            "http://wrong.test".into()
        ),
        Err(IntegrationError::InvalidInput)
    ));
    for url in [
        "https://user@roger.example.test",
        "https://roger.example.test/path",
        "https://roger.example.test?query",
        "https://roger.example.test:0",
        "https://roger.example.test:65536",
        "https://roger.example.test:no",
        "https://-bad.test",
        "https://bad-.test",
        "https://",
    ] {
        assert!(
            matches!(
                RogerCli::new(
                    executable.clone(),
                    credential.clone(),
                    probe.clone(),
                    url.into()
                ),
                Err(IntegrationError::InvalidInput)
            ),
            "{url}"
        );
    }
    let cli = RogerCli::new(
        executable,
        credential,
        probe.clone(),
        "https://roger.example.test".into(),
    )?;
    let bytes = cli.get(scope.credential(), &probe, Duration::from_secs(2), 4096)?;
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes)?["requester"],
        "sample-gate"
    );
    let ask = RogerAsk {
        binding,
        kind: AskKind::Approval,
        risk: AskRisk::Routine,
        title: Text::new("-review")?,
        body: Text::new("fixture body")?,
        supersedes: Some(probe),
    };
    assert_eq!(
        cli.find(scope.credential(), &ask, Duration::from_secs(2), 4096)?,
        None
    );
    let key = IdempotencyKey::from_ref(ExternalRef::new("fixture-key")?);
    let bytes = cli.submit(scope.credential(), &ask, &key, Duration::from_secs(2), 4096)?;
    assert_eq!(serde_json::from_slice::<Value>(&bytes)?["title"], "-review");
    let calls = std::fs::read_to_string(log)?;
    assert!(calls.contains("--title=-review"));
    assert!(calls.contains("--idem fixture-key"));
    assert!(calls.contains("--supersedes 01ARZ3NDEKTSV4RRFFQ69G5FAV"));
    let wrong_binary = directory.path().join("wrong-roger");
    std::fs::write(
        &wrong_binary,
        "#!/bin/sh\nif [ \"$2\" = --help ]; then printf '%s' '--idem --decision-key --action-rev --action-target --action-limits --resume-task --resume-rev --body-file'; else printf '%s' '{\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"requester\":\"foreign\"}'; fi\n",
    )?;
    std::fs::set_permissions(&wrong_binary, std::fs::Permissions::from_mode(0o700))?;
    let wrong_credential =
        CredentialFile::new(scope.credential().clone(), directory.path().join("token"))?;
    let wrong = RogerCli::new(
        wrong_binary,
        wrong_credential,
        ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
        "https://roger.example.test".into(),
    )?;
    assert_eq!(
        wrong.get(
            scope.credential(),
            &ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
            Duration::from_secs(2),
            4096
        ),
        Err(IntegrationError::ScopeMismatch)
    );
    Ok(())
}

#[cfg(unix)]
const CURRENT_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
#[cfg(unix)]
const EARLIER_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";

/// A Roger CLI whose `list --open` and `list --answered` print the given Asks.
#[cfg(unix)]
fn roger_listing(
    scope: &HouseScope,
    open: &[Value],
    answered: &[Value],
) -> Result<(tempfile::TempDir, RogerCli)> {
    use kitchen::integrations::github::CredentialFile;
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let executable = directory.path().join("roger");
    let token_path = directory.path().join("token");
    std::fs::write(&token_path, "fixture-roger-token")?;
    let script = format!(
        r##"#!/bin/sh
if [ "$1" = ask ] && [ "$2" = --help ]; then printf '%s' '--idem --decision-key --action-rev --action-target --action-limits --resume-task --resume-rev --body-file'; exit 0; fi
[ "$ROGER_TOKEN" = fixture-roger-token ] || exit 8
case "$1 $2" in
  "get --") printf '%s' '{{"id":"{CURRENT_ID}","requester":"sample-gate"}}' ;;
  "list --open") printf '%s' '{}' ;;
  "list --answered") printf '%s' '{}' ;;
  *) exit 8 ;;
esac
"##,
        json!({"asks": open}),
        json!({"asks": answered})
    );
    std::fs::write(&executable, script)?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let cli = RogerCli::new(
        executable,
        CredentialFile::new(scope.credential().clone(), token_path)?,
        ExternalRef::new(CURRENT_ID)?,
        "https://roger.example.test".into(),
    )?;
    Ok((directory, cli))
}

/// The Ask being looked up, and the receipt Roger holds for it as an open request.
#[cfg(unix)]
fn lookup_fixture() -> Result<(HouseScope, RogerAsk, Value)> {
    let (scope, binding, mut receipt) = fixture()?;
    receipt["title"] = json!("Approve this revision?");
    receipt["body"] = json!("current evidence");
    receipt["state"] = json!("open");
    let ask = RogerAsk {
        binding,
        kind: AskKind::Approval,
        risk: AskRisk::Sensitive,
        title: Text::new("Approve this revision?")?,
        body: Text::new("current evidence")?,
        supersedes: None,
    };
    Ok((scope, ask, receipt))
}

/// An earlier Ask for the same decision key and head, with different content.
#[cfg(unix)]
fn earlier_ask(current: &Value) -> Value {
    let mut earlier = current.clone();
    earlier["id"] = json!(EARLIER_ID);
    earlier["title"] = json!("Approve the earlier revision?");
    earlier["body"] = json!("earlier evidence");
    earlier["state"] = json!("answered");
    earlier
}

#[cfg(unix)]
#[test]
fn find_skips_an_earlier_ask_that_shares_the_key_and_head() -> Result {
    use std::time::Duration;
    let (scope, ask, current) = lookup_fixture()?;
    let earlier = earlier_ask(&current);
    let find = |open: Vec<Value>, answered: Vec<Value>| -> Result<_> {
        let (_directory, cli) = roger_listing(&scope, &open, &answered)?;
        Ok(cli.find(scope.credential(), &ask, Duration::from_secs(5), 64 * 1024))
    };
    let current_id = Some(ExternalRef::new(CURRENT_ID)?);
    // The current request is open and the earlier one was answered.
    assert_eq!(
        find(vec![current.clone()], vec![earlier.clone()])?,
        Ok(current_id.clone())
    );
    // The listing order does not matter: the earlier request may come first.
    assert_eq!(find(vec![earlier.clone()], vec![current])?, Ok(current_id));
    // Only an unrelated request exists, so there is no Ask to recover.
    assert_eq!(find(vec![], vec![earlier])?, Ok(None));
    Ok(())
}

#[cfg(unix)]
#[test]
fn find_still_rejects_a_matching_candidate_that_is_malformed_or_ambiguous() -> Result {
    use std::time::Duration;
    let (scope, ask, current) = lookup_fixture()?;
    let earlier = earlier_ask(&current);
    let find = |open: Vec<Value>, answered: Vec<Value>| -> Result<_> {
        let (_directory, cli) = roger_listing(&scope, &open, &answered)?;
        Ok(cli.find(scope.credential(), &ask, Duration::from_secs(5), 64 * 1024))
    };
    // A candidate with this content but no id is not skipped.
    let mut no_id = current.clone();
    no_id.as_object_mut().ok_or("object")?.remove("id");
    assert_eq!(
        find(vec![no_id], vec![earlier.clone()])?,
        Err(IntegrationError::Unknown)
    );
    // Nor is one whose id is not a Roger id.
    let mut bad_id = current.clone();
    bad_id["id"] = json!("--flag");
    assert_eq!(
        find(vec![bad_id], vec![earlier])?,
        Err(IntegrationError::InvalidInput)
    );
    // Two different Asks with exactly this content stay ambiguous.
    let mut duplicate = current.clone();
    duplicate["id"] = json!(EARLIER_ID);
    assert_eq!(
        find(vec![current], vec![duplicate])?,
        Err(IntegrationError::Unknown)
    );
    Ok(())
}
