//! Durable integration effects with sanitized in-memory providers, never live writes.
mod common;
use common::{Fixture, ManualClock, TestResult, at, creator, house, plan, scheduled, spec, ttl};
use kitchen::{
    BackendId, CredentialId, Error, HouseId, TaskId,
    contracts::*,
    house::MergeSubject,
    integrations::{github::*, roger::*},
    state::{EffectRecord, EffectState, HouseStore, StateError, run_effect},
    workflows::gate::MergeGrant,
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

#[derive(Clone, Copy)]
enum Fault {
    Reject,
    LoseAfterApply,
    LoseBeforeApply,
    /// Roger's transport fails with exactly this error.
    Roger(IntegrationError),
}
#[derive(Default)]
struct Remote {
    pull_request: Option<Value>,
    labels: Vec<Value>,
    defined_labels: Option<Vec<Value>>,
    comments: Vec<Value>,
    issues: Vec<Value>,
    issue_one: Option<Value>,
    relations: BTreeMap<String, Vec<Value>>,
    calls: Vec<(String, Value)>,
    fault: Option<Fault>,
    read_failure: bool,
    /// GitHub reads served, including failed ones.
    reads: std::cell::Cell<usize>,
    asks: BTreeMap<String, Value>,
    hide_lookup: bool,
}
/// One query-string value of a relative endpoint.
fn query<'a>(endpoint: &'a str, name: &str) -> Option<&'a str> {
    endpoint
        .split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}
struct Provider {
    remote: Rc<RefCell<Remote>>,
    store: HouseStore,
    task: TaskId,
    conformance_probe: bool,
}
impl Provider {
    fn intended(&self) -> bool {
        self.store.task(&self.task).is_ok_and(|task| {
            task.effects()
                .iter()
                .any(|effect| matches!(effect.state(), EffectState::Intended))
        })
    }
}
impl GitHubReadTransport for Provider {
    fn read(
        &self,
        _: &CredentialRef,
        request: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let remote = self.remote.borrow();
        remote.reads.set(remote.reads.get() + 1);
        if remote.read_failure {
            return Err(IntegrationError::Unavailable);
        }
        let path = request.endpoint();
        let value = if path.contains("/comments?") {
            json!(remote.comments)
        } else if path.ends_with("/pulls/1") {
            remote
                .pull_request
                .clone()
                .ok_or(IntegrationError::Unknown)?
        } else if path.contains("/labels?") {
            if path.contains("/issues/") {
                json!(remote.labels)
            } else {
                json!(remote.defined_labels.as_ref().unwrap_or(&remote.labels))
            }
        } else if path.ends_with("/issues/1") {
            remote.issue_one.clone().ok_or(IntegrationError::Unknown)?
        } else if path.ends_with("/issues/2") {
            json!({"id":102,"number":2})
        } else if path.contains("/sub_issues?") || path.contains("/dependencies/blocked_by?") {
            json!(
                remote
                    .relations
                    .get(path.split('?').next().ok_or(IntegrationError::Unknown)?)
                    .cloned()
                    .unwrap_or_default()
            )
        } else if path.contains("/issues?") {
            // Like GitHub: filter by creator, order by creation, then page.
            let mut issues: Vec<_> = remote
                .issues
                .iter()
                .filter(|issue| {
                    query(path, "creator")
                        .is_none_or(|login| issue["user"]["login"].as_str() == Some(login))
                })
                .collect();
            if query(path, "direction") == Some("desc") {
                issues.reverse();
            }
            let number = |name: &str, default: usize| {
                query(path, name)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(default)
            };
            let per_page = number("per_page", 30);
            json!(
                issues
                    .into_iter()
                    .skip(number("page", 1).saturating_sub(1) * per_page)
                    .take(per_page)
                    .collect::<Vec<_>>()
            )
        } else {
            return Err(IntegrationError::Unknown);
        };
        serde_json::to_vec(&value).map_err(|_| IntegrationError::Unknown)
    }
}
impl GitHubMutationTransport for Provider {
    fn submit(
        &self,
        _: &CredentialRef,
        request: &MutationRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, EffectFailure> {
        assert!(
            self.conformance_probe || self.intended(),
            "intent must be on disk before provider submission"
        );
        let mut remote = self.remote.borrow_mut();
        remote
            .calls
            .push((request.endpoint().into(), request.body().clone()));
        let fault = remote.fault.take();
        if matches!(fault, Some(Fault::Reject)) {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        if matches!(fault, Some(Fault::LoseBeforeApply)) {
            return Err(EffectFailure::Uncertain(UncertainReason::Timeout));
        }
        let path = request.endpoint();
        let body = request.body();
        if path.ends_with("/comments") {
            remote.comments.push(json!({"id":1,"body":body["body"],"user":{"login":"sample-bot"},"html_url":"https://github.com/sample/project/issues/1#issuecomment-1"}));
        } else if path.ends_with("/pulls/1/merge") {
            let pr = remote
                .pull_request
                .as_mut()
                .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            if pr["head"]["sha"] != body["sha"] {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            pr["merged"] = json!(true);
            pr["merge_commit_sha"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        } else if path.ends_with("/issues/1") && request.method() == "PATCH" {
            let issue = remote
                .issue_one
                .as_mut()
                .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            issue["state"] = body["state"].clone();
            issue["state_reason"] = body["state_reason"].clone();
        } else if path.ends_with("/issues") {
            remote.issues.push(json!({"id":103,"number":3,"title":body["title"],"body":body["body"],"user":{"login":"sample-bot"},"html_url":"https://github.com/sample/project/issues/3"}));
        } else if path.contains("/sub_issues") || path.contains("/dependencies/blocked_by") {
            remote.relations.entry(path.into()).or_default().push(json!({"id":102,"number":2,"repository_url":"https://api.github.com/repos/sample/project"}));
        } else if path.contains("/issues/") && path.ends_with("/labels") {
            remote
                .labels
                .push(json!({"name":body["labels"][0],"color":"aabbcc","description":"fixture"}));
        } else if path.contains("/issues/") && request.method() == "DELETE" {
            remote.labels.clear();
        } else if path.ends_with("/labels") {
            remote.labels.push(body.clone());
        } else {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        if matches!(fault, Some(Fault::LoseAfterApply)) {
            return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
        }
        Ok(b"{}".to_vec())
    }
}
fn setup(
    fixture: &Fixture,
    budget: u32,
    permissions: &[Permission],
    backend: &str,
) -> TestResult<(HouseScope, HouseGrants, TaskId, Fence)> {
    let repo = Repository::new("sample/project")?;
    let backend = BackendId::new(backend)?;
    let credential = CredentialId::new("sample-credential")?;
    let requester = ExternalRef::new("sample-bot")?;
    let grants_vec: Vec<_> = permissions
        .iter()
        .map(|p| Grant::repository(*p, repo.clone(), backend.clone(), credential.clone()))
        .collect();
    let grants = HouseGrants::new(house()?, grants_vec.clone());
    let mut work = spec("task-1")?;
    work.repository = Some(repo.clone());
    work.authority = TaskAuthority::delegate(&grants, grants_vec)?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let task = TaskId::new("task-1")?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("owner")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let scope = HouseScope::new(
        house()?,
        [repo],
        requester.clone(),
        CredentialRef::new(house()?, credential, requester),
        PostingBudget::new(budget)?,
        permissions.iter().copied(),
    )?;
    Ok((scope, grants, task, fence))
}
fn provider(fixture: &Fixture, task: &TaskId, remote: Rc<RefCell<Remote>>) -> TestResult<Provider> {
    Ok(Provider {
        remote,
        store: fixture.reopen()?,
        task: task.clone(),
        conformance_probe: false,
    })
}
fn mutation(action: GitHubAction) -> TestResult<GitHubMutation> {
    Ok(GitHubMutation {
        repository: Repository::new("sample/project")?,
        action,
    })
}
fn label(name: &str) -> GitHubAction {
    GitHubAction::CreateLabel {
        label: LabelDefinition {
            name: name.into(),
            color: "aabbcc".into(),
            description: "fixture".into(),
        },
    }
}

#[test]
fn github_lost_response_reconciles_after_restart_without_duplicate_comment() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 3, &[Permission::PostComment], "github")?;
    let remote = Rc::new(RefCell::new(Remote {
        fault: Some(Fault::LoseAfterApply),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let effect = backend.effect(mutation(GitHubAction::PostComment {
        issue: IssueNumber::new(1)?,
        body: Text::new("sanitized comment")?,
    })?)?;
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "comment", effect.clone())?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(first.state(), EffectState::Uncertain { .. }));
    let restarted = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let reconciled = kitchen::state::reconcile(
        &fixture.reopen()?,
        &restarted,
        &task,
        fence,
        &ManualClock::starting_at(2),
    )?;
    assert_eq!(reconciled.resolved.len(), 1);
    let second = run_effect(
        &fixture.reopen()?,
        &restarted,
        &grants,
        plan(&task, fence, "comment", effect)?,
        &ManualClock::starting_at(2),
    )?;
    let EffectState::Applied { receipt, .. } = second.state() else {
        return Err("expected reconciled comment".into());
    };
    assert_eq!(
        receipt.reference().as_str(),
        "https://github.com/sample/project/issues/1#issuecomment-1"
    );
    assert_eq!(remote.borrow().calls.len(), 1);
    assert_eq!(remote.borrow().comments.len(), 1);
    Ok(())
}
#[test]
fn close_issue_requires_its_grant_and_reconciles_lost_response() -> TestResult {
    let close = || -> TestResult<GitHubMutation> {
        mutation(GitHubAction::CloseIssue {
            repository: Repository::new("sample/project")?,
            number: IssueNumber::new(1)?,
            reason: CloseReason::NotPlanned,
        })
    };
    // Relationship edits do not imply closing an issue.
    let fixture = Fixture::new()?;
    let (scope, _, task, _) = setup(&fixture, 3, &[Permission::EditIssueRelationships], "github")?;
    let remote = Rc::new(RefCell::new(Remote::default()));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote)?,
        ReadLimits::default(),
    );
    assert_eq!(
        backend.effect(close()?),
        Err(IntegrationError::PermissionDenied)
    );

    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 3, &[Permission::CloseIssue], "github")?;
    let remote = Rc::new(RefCell::new(Remote {
        issue_one: Some(json!({"number":1,"state":"open","state_reason":null})),
        fault: Some(Fault::LoseAfterApply),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let effect = backend.effect(close()?)?;
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "close", effect.clone())?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(first.state(), EffectState::Uncertain { .. }));
    let restarted = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let reconciled = kitchen::state::reconcile(
        &fixture.reopen()?,
        &restarted,
        &task,
        fence,
        &ManualClock::starting_at(2),
    )?;
    assert_eq!(reconciled.resolved.len(), 1);
    let second = run_effect(
        &fixture.reopen()?,
        &restarted,
        &grants,
        plan(&task, fence, "close", effect)?,
        &ManualClock::starting_at(3),
    )?;
    assert!(matches!(second.state(), EffectState::Applied { .. }));
    let remote = remote.borrow();
    assert_eq!(remote.calls.len(), 1);
    assert_eq!(
        remote.calls[0],
        (
            "repos/sample/project/issues/1".into(),
            json!({"state":"closed","state_reason":"not_planned"})
        )
    );
    assert_eq!(
        remote.issue_one,
        Some(json!({"number":1,"state":"closed","state_reason":"not_planned"}))
    );
    Ok(())
}
#[test]
fn github_unknown_absence_never_retries_and_partial_reads_cannot_post() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 3, &[Permission::EditLabels], "github")?;
    let remote = Rc::new(RefCell::new(Remote {
        fault: Some(Fault::LoseBeforeApply),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let effect = backend.effect(mutation(label("ready"))?)?;
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "label", effect.clone())?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    for now in 2..=3 {
        let report = kitchen::state::reconcile(
            &fixture.store,
            &backend,
            &task,
            fence,
            &ManualClock::starting_at(now),
        )?;
        assert_eq!(report.unresolved.len(), 1);
        assert!(matches!(
            run_effect(
                &fixture.store,
                &backend,
                &grants,
                plan(&task, fence, "label", effect.clone())?,
                &ManualClock::starting_at(now)
            ),
            Err(Error::State(kitchen::state::StateError::UnsafeRetry(_)))
        ));
    }
    assert_eq!(remote.borrow().calls.len(), 1);
    assert!(remote.borrow().labels.is_empty());
    remote.borrow_mut().read_failure = true;
    let request = fixture.store.task(&task)?.effects()[0].request().clone();
    assert!(backend.lookup(&request).is_err());
    assert_eq!(remote.borrow().calls.len(), 1);
    Ok(())
}
#[test]
fn label_setup_noop_conflict_permission_denial_partial_failure_and_rerun() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 4, &[Permission::EditLabels], "github")?;
    let remote = Rc::new(RefCell::new(Remote {
        labels: vec![
            json!({"name":"present","color":"aabbcc","description":"fixture"}),
            json!({"name":"conflict","color":"000000","description":"custom"}),
        ],
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let run = |name: &str| -> TestResult<_> {
        Ok(run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan(&task, fence, name, backend.effect(mutation(label(name))?)?)?,
            &ManualClock::starting_at(1),
        )?)
    };
    assert!(matches!(
        run("present")?.state(),
        EffectState::Applied { .. }
    ));
    assert!(matches!(
        run("conflict")?.state(),
        EffectState::NotApplied { .. }
    ));
    assert!(remote.borrow().calls.is_empty());
    assert!(matches!(run("first")?.state(), EffectState::Applied { .. }));
    remote.borrow_mut().fault = Some(Fault::Reject);
    assert!(matches!(
        run("second")?.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));
    assert!(matches!(run("first")?.state(), EffectState::Applied { .. }));
    assert!(matches!(
        run("second")?.state(),
        EffectState::Applied { .. }
    ));
    let state = remote.borrow();
    assert_eq!(state.calls.len(), 3);
    assert_eq!(state.labels.len(), 4);
    assert_eq!(state.labels[1]["color"], "000000");
    Ok(())
}

#[test]
fn set_label_requires_existing_repository_definition() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::EditLabels], "github")?;
    let remote = Rc::new(RefCell::new(Remote::default()));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let action = GitHubAction::SetLabel {
        issue: IssueNumber::new(1)?,
        label: "undefined".into(),
        present: true,
    };
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(
            &task,
            fence,
            "apply",
            backend.effect(mutation(action.clone())?)?,
        )?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(
        first.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));
    assert!(remote.borrow().calls.is_empty());
    remote.borrow_mut().defined_labels = Some(vec![
        json!({"name":"undefined","color":"aabbcc","description":"fixture"}),
    ]);
    let second = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "apply", backend.effect(mutation(action)?)?)?,
        &ManualClock::starting_at(2),
    )?;
    assert!(matches!(second.state(), EffectState::Applied { .. }));
    assert_eq!(remote.borrow().calls.len(), 1);
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh_cli_provider_refusal_can_be_corrected_and_rerun() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::EditLabels], "github")?;
    let directory = tempfile::tempdir()?;
    let executable = directory.path().join("gh");
    let mode = directory.path().join("mode");
    let applied = directory.path().join("applied");
    std::fs::write(&mode, "deny")?;
    common::executable::write_executable(
        &executable,
        format!(
            r##"#!/bin/sh
case " $* " in
  *' user '*) printf '%s' '{{"login":"sample-bot"}}' ;;
  *' --method GET '*'issues/1/labels'*) if [ -e '{}' ]; then printf '%s' '[{{"name":"defined"}}]'; else printf '%s' '[]'; fi ;;
  *' --method GET '*'repos/sample/project/labels'*) printf '%s' '[{{"name":"defined","color":"aabbcc","description":"fixture"}}]' ;;
  *' --method POST '*)
    [ "$GH_TOKEN" = fixture-secret ] || exit 9
    for arg in "$@"; do [ "$arg" != fixture-secret ] || exit 9; done
    if [ "$(/bin/cat '{}')" = deny ]; then printf 'HTTP/2 403 Forbidden\r\n\r\n'; exit 1; fi
    /usr/bin/touch '{}'; printf 'HTTP/2 200 OK\r\n\r\n{{}}' ;;
  *) exit 9 ;;
esac
"##,
            applied.display(),
            mode.display(),
            applied.display()
        ),
    )?;
    let token = directory.path().join("token");
    std::fs::write(&token, "fixture-secret")?;
    let cli = GhCli::new(
        executable,
        CredentialFile::new(scope.credential().clone(), token)?,
    )?;
    let backend = GitHubExecutor::new(BackendId::new("github")?, scope, cli, ReadLimits::default());
    let effect = backend.effect(mutation(GitHubAction::SetLabel {
        issue: IssueNumber::new(1)?,
        label: "defined".into(),
        present: true,
    })?)?;
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "set", effect.clone())?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(
        first.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));
    assert!(!applied.exists());
    std::fs::write(&mode, "allow")?;
    let second = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "set", effect)?,
        &ManualClock::starting_at(2),
    )?;
    assert!(matches!(second.state(), EffectState::Applied { .. }));
    assert!(applied.exists());
    Ok(())
}
#[test]
fn persisted_budget_and_credential_mismatch_refuse_before_transport() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 1, &[Permission::EditLabels], "github")?;
    let remote = Rc::new(RefCell::new(Remote::default()));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let effect = backend.effect(mutation(label("first"))?)?;
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "first", effect.clone())?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(record.state(), EffectState::Applied { .. }));
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan(
                &task,
                fence,
                "second",
                backend.effect(mutation(label("second"))?)?
            )?,
            &ManualClock::starting_at(2)
        ),
        Err(Error::Contract(ContractError::EffectBudgetExhausted { .. }))
    ));
    let wrong = EffectRequest::new(
        house()?,
        BackendId::new("github")?,
        CredentialId::new("foreign")?,
        task,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("untrusted-key")?),
        effect.into(),
    );
    assert_eq!(
        backend.execute(&wrong),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert_eq!(remote.borrow().calls.len(), 1);
    Ok(())
}
#[test]
fn issue_creation_labels_and_relationships_use_typed_requests() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(
        &fixture,
        8,
        &[
            Permission::CreateIssue,
            Permission::EditLabels,
            Permission::EditIssueRelationships,
        ],
        "github",
    )?;
    let remote = Rc::new(RefCell::new(Remote {
        defined_labels: Some(vec![
            json!({"name":"ready / next","color":"aabbcc","description":"fixture"}),
        ]),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let one = IssueNumber::new(1)?;
    let two = IssueNumber::new(2)?;
    let actions = [
        GitHubAction::CreateIssue {
            title: Text::new("sanitized issue")?,
            body: Text::new("sanitized body")?,
        },
        GitHubAction::SetLabel {
            issue: one,
            label: "ready / next".into(),
            present: true,
        },
        GitHubAction::SetLabel {
            issue: one,
            label: "ready / next".into(),
            present: false,
        },
        GitHubAction::LinkSubIssue {
            parent: one,
            child: two,
        },
        GitHubAction::LinkDependency {
            issue: one,
            blocker: two,
        },
    ];
    for (index, action) in actions.into_iter().enumerate() {
        let record = run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan(
                &task,
                fence,
                &format!("effect-{index}"),
                backend.effect(mutation(action)?)?,
            )?,
            &ManualClock::starting_at(1),
        )?;
        assert!(matches!(record.state(), EffectState::Applied { .. }));
    }
    let remote = remote.borrow();
    assert!(remote.calls[2].0.ends_with("ready%20%2F%20next"));
    assert_eq!(remote.calls[3].1["sub_issue_id"], 102);
    assert_eq!(remote.calls[4].1["issue_id"], 102);
    assert_eq!(remote.issues.len(), 1);
    Ok(())
}

/// Issues numbered from 10, each authored by `login`, with distinct titles.
fn authored_issues(login: &str, count: u64) -> Vec<Value> {
    (0..count)
        .map(|n| {
            json!({
                "id": 1_000 + n,
                "number": 10 + n,
                "title": format!("unrelated {n}"),
                "body": "unrelated",
                "user": {"login": login},
                "html_url": format!("https://github.com/sample/project/issues/{}", 10 + n),
            })
        })
        .collect()
}
fn create_issue_effect(
    fixture: &Fixture,
    remote: &Rc<RefCell<Remote>>,
) -> TestResult<(
    GitHubExecutor<Provider>,
    HouseGrants,
    TaskId,
    Fence,
    GitHubEffect,
)> {
    let (scope, grants, task, fence) = setup(fixture, 3, &[Permission::CreateIssue], "github")?;
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let effect = backend.effect(mutation(GitHubAction::CreateIssue {
        title: Text::new("sanitized issue")?,
        body: Text::new("sanitized body")?,
    })?)?;
    Ok((backend, grants, task, fence, effect))
}
#[test]
fn create_issue_marker_scan_stays_complete_in_a_large_repository() -> TestResult {
    let fixture = Fixture::new()?;
    // Twice the default page budget of other people's issues.
    let remote = Rc::new(RefCell::new(Remote {
        issues: authored_issues("someone-else", 2_100),
        fault: Some(Fault::LoseAfterApply),
        ..Remote::default()
    }));
    let (backend, grants, task, fence, effect) = create_issue_effect(&fixture, &remote)?;
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "issue", effect.clone())?,
        &ManualClock::starting_at(1),
    )?;
    // The pre-submit scan completed, so the issue was submitted and only its response lost.
    assert!(matches!(first.state(), EffectState::Uncertain { .. }));
    let reconciled = kitchen::state::reconcile(
        &fixture.reopen()?,
        &backend,
        &task,
        fence,
        &ManualClock::starting_at(2),
    )?;
    assert_eq!(reconciled.resolved.len(), 1);
    let second = run_effect(
        &fixture.reopen()?,
        &backend,
        &grants,
        plan(&task, fence, "issue", effect)?,
        &ManualClock::starting_at(2),
    )?;
    let EffectState::Applied { receipt, .. } = second.state() else {
        return Err("expected the reconciled issue".into());
    };
    assert_eq!(
        receipt.reference().as_str(),
        "https://github.com/sample/project/issues/3"
    );
    let remote = remote.borrow();
    assert_eq!(remote.calls.len(), 1);
    assert_eq!(
        remote
            .issues
            .iter()
            .filter(|issue| issue["user"]["login"] == "sample-bot")
            .count(),
        1
    );
    Ok(())
}
#[test]
fn create_issue_fails_closed_when_the_requesters_own_issues_exceed_the_scan_budget() -> TestResult {
    let fixture = Fixture::new()?;
    let remote = Rc::new(RefCell::new(Remote {
        issues: authored_issues("sample-bot", 2_100),
        ..Remote::default()
    }));
    let (backend, grants, task, fence, effect) = create_issue_effect(&fixture, &remote)?;
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "issue", effect)?,
        &ManualClock::starting_at(1),
    )?;
    // An incomplete scan is not proof of absence: nothing may be submitted.
    assert!(matches!(
        record.state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::Rejected,
            ..
        }
    ));
    assert!(remote.borrow().calls.is_empty());
    Ok(())
}

impl RogerReadTransport for Provider {
    fn get(
        &self,
        _: &CredentialRef,
        ask: &ExternalRef,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let remote = self.remote.borrow();
        if remote.read_failure {
            return Err(IntegrationError::Unavailable);
        }
        let value = remote
            .asks
            .values()
            .find(|value| value["id"].as_str() == Some(ask.as_str()))
            .ok_or(IntegrationError::Unavailable)?;
        serde_json::to_vec(value).map_err(|_| IntegrationError::Unknown)
    }
}
impl RogerMutationTransport for Provider {
    fn submit(
        &self,
        credential: &CredentialRef,
        ask: &RogerAsk,
        key: &IdempotencyKey,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        assert!(
            self.conformance_probe
                || self.store.task(&self.task).is_ok_and(|task| task
                    .effects()
                    .iter()
                    .any(|effect| effect.request().key() == key && effect.submissions() > 0)),
            "Roger must receive persisted intent and a recorded submission"
        );
        let mut remote = self.remote.borrow_mut();
        remote.calls.push((
            key.as_str().into(),
            json!({"task":ask.binding.task.as_str()}),
        ));
        let fault = remote.fault.take();
        if let Some(Fault::Roger(error)) = fault {
            return Err(error);
        }
        if matches!(fault, Some(Fault::Reject | Fault::LoseBeforeApply)) {
            return Err(IntegrationError::Unavailable);
        }
        let binding = &ask.binding;
        let value = json!({"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","requester":credential.requester().as_str(),"repo":binding.repository.as_str(),"decisionKey":binding.decision_key()?,"resume":{"task":binding.task.as_str(),"rev":binding.head()?.as_str()},"action":{"verb":binding.action.as_str(),"target":binding.target.as_str(),"rev":binding.head()?.as_str(),"limits":binding.limits.as_str()},"kind":"approval","title":ask.title.as_str(),"body":ask.body.as_str(),"state":"open","answer":null,"supersededBy":null});
        let value = remote
            .asks
            .entry(key.as_str().into())
            .or_insert(value)
            .clone();
        if matches!(fault, Some(Fault::LoseAfterApply)) {
            return Err(IntegrationError::Timeout);
        }
        serde_json::to_vec(&value).map_err(|_| IntegrationError::Unknown)
    }
    fn find(
        &self,
        _: &CredentialRef,
        ask: &RogerAsk,
        _: Duration,
        _: usize,
    ) -> Result<Option<ExternalRef>, IntegrationError> {
        let remote = self.remote.borrow();
        if remote.read_failure {
            return Err(IntegrationError::Unavailable);
        }
        if remote.hide_lookup {
            return Ok(None);
        }
        let key = ask.binding.decision_key()?;
        remote
            .asks
            .values()
            .find(|value| value["decisionKey"].as_str() == Some(&key))
            .map(|value| {
                ExternalRef::new(value["id"].as_str().ok_or(IntegrationError::Unknown)?)
                    .map_err(IntegrationError::from)
            })
            .transpose()
    }
}
fn question(task: &TaskId) -> TestResult<RogerAsk> {
    Ok(RogerAsk {
        binding: DecisionBinding {
            house: house()?,
            task: task.clone(),
            owner: DecisionOwner::Merge,
            repository: Repository::new("sample/project")?,
            action: Permission::Merge,
            target: ExternalRef::new("pr:sample/project#1")?,
            revision: EvidenceRevision::INITIAL,
            subject: Some(EvidenceSubject {
                head: CommitId::new(&"a".repeat(40))?,
                base: None,
            }),
            limits: Text::new("squash into main")?,
        },
        kind: AskKind::Approval,
        risk: AskRisk::Sensitive,
        title: Text::new("Approve this revision?")?,
        body: Text::new("Sanitized evidence and recommendation")?,
        supersedes: None,
    })
}

/// Records the evidence subject `question` names and returns its revision:
/// core refuses an ask whose subject differs from the task's current one.
fn record_question_subject(
    fixture: &Fixture,
    task: &TaskId,
    fence: Fence,
) -> TestResult<EvidenceRevision> {
    Ok(fixture.store.record_evidence(
        task,
        fence,
        Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: CommitId::new(&"a".repeat(40))?,
                base: None,
            },
            source: ExternalRef::new("ci-1")?,
            observed_at: at(1),
        },
        at(1),
    )?)
}
fn evidenced_question(fixture: &Fixture, task: &TaskId, fence: Fence) -> TestResult<RogerAsk> {
    let mut ask = question(task)?;
    ask.binding.revision = record_question_subject(fixture, task, fence)?;
    Ok(ask)
}

/// A plan decided at the task's current evidence revision.
fn plan_at(
    task: &TaskId,
    fence: Fence,
    name: &str,
    effect: impl Into<Effect>,
    revision: EvidenceRevision,
) -> TestResult<kitchen::state::EffectPlan> {
    let mut plan = plan(task, fence, name, effect)?;
    plan.decided_at = revision;
    Ok(plan)
}

#[test]
fn github_and_roger_executors_pass_shared_conformance() -> TestResult {
    use kitchen::contracts::conformance::{ConformanceFixture, run};
    for family in ["github", "roger"] {
        let fixture = Fixture::new()?;
        let permission = if family == "github" {
            Permission::EditLabels
        } else {
            Permission::AskHuman
        };
        let (scope, _grants, task, _fence) = setup(&fixture, 3, &[permission], family)?;
        let remote = Rc::new(RefCell::new(Remote::default()));
        let mut transport = provider(&fixture, &task, remote)?;
        transport.conformance_probe = true;
        let contract = ConformanceFixture {
            house: house()?,
            foreign_house: HouseId::new("foreign")?,
            foreign_backend: BackendId::new("foreign")?,
            credential: scope.credential().name().clone(),
            task: task.clone(),
            repository: Repository::new("sample/project")?,
            run_tag: ExternalRef::new("fixture")?,
            brief: Text::new("fixture")?,
        };
        if family == "github" {
            let executor = GitHubExecutor::new(
                BackendId::new(family)?,
                scope,
                transport,
                ReadLimits::default(),
            );
            let effect = Effect::GitHub(executor.effect(mutation(label("conformance-probe"))?)?);
            run(&executor, &contract, &effect)?;
        } else {
            let executor = RogerExecutor::new(
                BackendId::new(family)?,
                scope,
                transport,
                ReadLimits::default(),
            );
            let effect = Effect::Roger(executor.effect(question(&task)?)?);
            run(&executor, &contract, &effect)?;
        }
    }
    Ok(())
}
#[test]
fn roger_restart_native_idempotency_recovers_lost_response_and_preserves_one_ask() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 3, &[Permission::AskHuman], "roger")?;
    let remote = Rc::new(RefCell::new(Remote {
        fault: Some(Fault::LoseAfterApply),
        hide_lookup: true,
        ..Remote::default()
    }));
    let backend = RogerExecutor::new(
        BackendId::new("roger")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let asked = evidenced_question(&fixture, &task, fence)?;
    let revision = asked.binding.revision;
    let effect = backend.effect(asked)?;
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan_at(&task, fence, "ask", effect.clone(), revision)?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(first.state(), EffectState::Uncertain { .. }));
    let restarted = RogerExecutor::new(
        BackendId::new("roger")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let second = run_effect(
        &fixture.reopen()?,
        &restarted,
        &grants,
        plan_at(&task, fence, "ask", effect.clone(), revision)?,
        &ManualClock::starting_at(2),
    )?;
    let EffectState::Applied { receipt, .. } = second.state() else {
        return Err("expected native idempotent receipt".into());
    };
    assert_eq!(remote.borrow().asks.len(), 1);
    assert_eq!(remote.borrow().calls.len(), 2);
    assert_eq!(remote.borrow().calls[0].0, remote.borrow().calls[1].0);
    let reader = RogerClient::new(
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    assert_eq!(
        reader.poll(&effect.ask.binding, receipt.reference())?,
        DecisionStatus::Unanswered
    );
    remote.borrow_mut().read_failure = true;
    assert_eq!(
        reader.poll(&effect.ask.binding, receipt.reference()),
        Err(IntegrationError::Unavailable)
    );
    assert_eq!(remote.borrow().calls.len(), 2);
    Ok(())
}
#[test]
fn roger_stale_counter_wrong_task_and_budget_fail_before_submission() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 1, &[Permission::AskHuman], "roger")?;
    let remote = Rc::new(RefCell::new(Remote::default()));
    let backend = RogerExecutor::new(
        BackendId::new("roger")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let current = record_question_subject(&fixture, &task, fence)?;
    let mut wrong = question(&TaskId::new("wrong-task")?)?;
    wrong.binding.revision = current;
    let wrong = backend.effect(wrong)?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan_at(&task, fence, "wrong", wrong, current)?,
            &ManualClock::starting_at(1)
        ),
        Err(Error::Contract(ContractError::DecisionBindingMismatch))
    ));
    let mut stale = question(&task)?;
    stale.binding.revision = EvidenceRevision::INITIAL;
    assert_ne!(stale.binding.revision, current);
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan_at(&task, fence, "stale", backend.effect(stale)?, current)?,
            &ManualClock::starting_at(1)
        ),
        Err(Error::Contract(ContractError::DecisionBindingMismatch))
    ));
    let mut moved = question(&task)?;
    moved.binding.revision = current;
    moved.binding.subject = Some(EvidenceSubject {
        head: CommitId::new(&"b".repeat(40))?,
        base: None,
    });
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan_at(&task, fence, "moved", backend.effect(moved)?, current)?,
            &ManualClock::starting_at(1)
        ),
        Err(Error::Contract(ContractError::DecisionBindingMismatch))
    ));
    assert!(remote.borrow().calls.is_empty());
    let mut effect = question(&task)?;
    effect.binding.revision = current;
    let effect = backend.effect(effect)?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan_at(&task, fence, "first", effect.clone(), current)?,
            &ManualClock::starting_at(1)
        )?
        .state(),
        EffectState::Applied { .. }
    ));
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan_at(&task, fence, "second", effect, current)?,
            &ManualClock::starting_at(2)
        ),
        Err(Error::Contract(ContractError::EffectBudgetExhausted { .. }))
    ));
    assert_eq!(remote.borrow().calls.len(), 1);
    Ok(())
}

#[test]
fn revoked_posting_still_allows_read_only_reconciliation() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::EditLabels], "github")?;
    let remote = Rc::new(RefCell::new(Remote {
        fault: Some(Fault::LoseAfterApply),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    let effect = backend.effect(mutation(label("ready"))?)?;
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "label", effect)?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(record.state(), EffectState::Uncertain { .. }));
    let read_only = HouseScope::new(
        house()?,
        [Repository::new("sample/project")?],
        scope.requester().clone(),
        scope.credential().clone(),
        PostingBudget::new(0)?,
        [],
    )?;
    let revoked = GitHubExecutor::new(
        BackendId::new("github")?,
        read_only,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    );
    assert_eq!(
        revoked.execute(record.request()),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert!(matches!(
        revoked.lookup(record.request())?,
        Lookup::Applied(_)
    ));
    assert_eq!(remote.borrow().calls.len(), 1);
    Ok(())
}

const HEAD: &str = "1111111111111111111111111111111111111111";
const BASE: &str = "2222222222222222222222222222222222222222";
const MOVED: &str = "3333333333333333333333333333333333333333";

fn commit(hex: &str) -> TestResult<CommitId> {
    Ok(CommitId::new(hex)?)
}

/// A merge approved at `head` and, when the evidence has one, at `base`.
fn merge(head: &str, base: Option<&str>) -> TestResult<GitHubAction> {
    Ok(GitHubAction::MergePullRequest {
        number: IssueNumber::new(1)?,
        expected_head: commit(head)?,
        expected_base: BranchName::new("main")?,
        expected_base_commit: base.map(commit).transpose()?,
        method: MergeMethod::Squash,
    })
}

/// A persisted merge effect built without [`GitHubExecutor::effect`].
fn unchecked_merge(scope: &HouseScope, base: Option<&str>) -> TestResult<GitHubEffect> {
    Ok(GitHubEffect {
        requester: scope.requester().clone(),
        mutation: mutation(merge(HEAD, base)?)?,
        posting_budget: scope.budget(),
    })
}

/// Pull request 1 at [`HEAD`] and [`BASE`].
fn merge_subject() -> TestResult<MergeSubject> {
    Ok(MergeSubject {
        repository: Repository::new("sample/project")?,
        number: IssueNumber::new(1)?,
        head: commit(HEAD)?,
        base: commit(BASE)?,
    })
}

/// Authority of a house with a standing merge grant for `sample/project`
/// on `github`, the given readiness policy, and no readiness assessment.
fn merge_authority(
    policy: &[(&str, kitchen::house::ReadinessLevel)],
) -> TestResult<kitchen::house::IssuedAuthority> {
    let mut config: kitchen::house::HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    let repository = Repository::new("sample/project")?;
    let merge = Grant::repository(
        Permission::Merge,
        repository.clone(),
        BackendId::new("github")?,
        CredentialId::new("sample-credential")?,
    );
    config.house = house()?;
    config.repositories = [repository.clone()].into();
    config.posting_destinations = [repository].into();
    config.grants = [merge.clone()].into();
    config.policy_limits = [merge].into();
    for (work_type, level) in policy {
        config.merge_readiness.insert(Text::new(work_type)?, *level);
    }
    Ok(config.issue_authority(&[], &[])?)
}

/// The readiness-checked grant to merge [`merge_subject`], from a house
/// without a readiness policy.
fn granted() -> TestResult<MergeGrant> {
    Ok(MergeGrant::resolve(
        &merge_authority(&[])?,
        &merge_subject()?,
        &BackendId::new("github")?,
    )?)
}

fn open_pull_request(head: &str, base_ref: &str) -> Value {
    json!({"number":1,"merged":false,"head":{"sha":head},"base":{"ref":base_ref}})
}

/// Records `head` and `base` as the task's current evidence subject and
/// returns the revision a plan must be decided at.
fn record_subject(
    fixture: &Fixture,
    task: &TaskId,
    fence: Fence,
    head: &str,
    base: Option<&str>,
) -> TestResult<EvidenceRevision> {
    Ok(fixture.store.record_evidence(
        task,
        fence,
        Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: commit(head)?,
                base: base.map(commit).transpose()?,
            },
            source: ExternalRef::new("ci-1")?,
            observed_at: at(1),
        },
        at(1),
    )?)
}

#[test]
fn squash_merge_is_exact_head_and_reconciles_lost_response() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::Merge], "github")?;
    let revision = record_subject(&fixture, &task, fence, HEAD, Some(BASE))?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(HEAD, "main")),
        fault: Some(Fault::LoseAfterApply),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    let effect = backend.effect(mutation(merge(HEAD, Some(BASE))?)?)?;
    assert_eq!(effect.required_permission(), Permission::Merge);
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan_at(&task, fence, "merge", effect, revision)?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(first.state(), EffectState::Uncertain { .. }));
    let restarted = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    let reconciled = kitchen::state::reconcile(
        &fixture.reopen()?,
        &restarted,
        &task,
        fence,
        &ManualClock::starting_at(2),
    )?;
    assert_eq!(reconciled.resolved.len(), 1);
    assert_eq!(remote.borrow().calls.len(), 1);
    assert_eq!(remote.borrow().calls[0].1["merge_method"], "squash");
    assert_eq!(remote.borrow().calls[0].1["sha"], HEAD);
    Ok(())
}

#[test]
fn squash_merge_rejects_retargeted_base_branch_before_submission() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::Merge], "github")?;
    let revision = record_subject(&fixture, &task, fence, HEAD, Some(BASE))?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(HEAD, "release")),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    let effect = backend.effect(mutation(merge(HEAD, Some(BASE))?)?)?;
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan_at(&task, fence, "merge-base", effect, revision)?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(record.state(), EffectState::NotApplied { .. }));
    assert!(remote.borrow().calls.is_empty());
    Ok(())
}

#[test]
fn squash_merge_rejects_pull_request_head_that_moved_after_evidence() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::Merge], "github")?;
    let revision = record_subject(&fixture, &task, fence, HEAD, Some(BASE))?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(MOVED, "main")),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    let effect = backend.effect(mutation(merge(HEAD, Some(BASE))?)?)?;
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan_at(&task, fence, "merge", effect, revision)?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(record.state(), EffectState::NotApplied { .. }));
    assert!(remote.borrow().calls.is_empty());
    Ok(())
}

#[test]
fn merge_is_admitted_only_at_the_tasks_evidence_subject() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 3, &[Permission::Merge], "github")?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(HEAD, "main")),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    let clock = ManualClock::starting_at(1);
    let attempt = |name: &str,
                   action: GitHubAction,
                   revision: EvidenceRevision|
     -> TestResult<Result<EffectRecord, Error>> {
        let plan = plan_at(
            &task,
            fence,
            name,
            backend.effect(mutation(action)?)?,
            revision,
        )?;
        Ok(run_effect(&fixture.store, &backend, &grants, plan, &clock))
    };
    let refused = |name: &str, action: GitHubAction, revision: EvidenceRevision| -> TestResult {
        let result = attempt(name, action, revision)?;
        assert!(
            matches!(
                result,
                Err(Error::Contract(ContractError::DecisionBindingMismatch))
            ),
            "{name}: {result:?}"
        );
        Ok(())
    };
    refused(
        "no-evidence",
        merge(HEAD, Some(BASE))?,
        EvidenceRevision::INITIAL,
    )?;
    let evidence = record_subject(&fixture, &task, fence, MOVED, Some(BASE))?;
    refused("moved-head", merge(HEAD, Some(BASE))?, evidence)?;
    let evidence = record_subject(&fixture, &task, fence, HEAD, Some(MOVED))?;
    refused("moved-base", merge(HEAD, Some(BASE))?, evidence)?;
    // A merge grant always names a base commit, so a baseless merge is
    // refused before a plan exists.
    assert_eq!(
        backend.effect(mutation(merge(HEAD, None)?)?),
        Err(IntegrationError::PermissionDenied)
    );
    let evidence = record_subject(&fixture, &task, fence, HEAD, None)?;
    refused("base-unseen", merge(HEAD, Some(BASE))?, evidence)?;
    // A refused merge leaves no persisted intent and GitHub saw no request.
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert!(remote.borrow().calls.is_empty());
    let evidence = record_subject(&fixture, &task, fence, HEAD, Some(BASE))?;
    let merged = attempt("merge", merge(HEAD, Some(BASE))?, evidence)??;
    assert!(matches!(merged.state(), EffectState::Applied { .. }));
    assert_eq!(remote.borrow().calls.len(), 1);
    assert_eq!(remote.borrow().calls[0].1["sha"], HEAD);
    Ok(())
}

#[test]
fn merge_of_a_subject_without_a_base_is_not_granted() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 2, &[Permission::Merge], "github")?;
    let revision = record_subject(&fixture, &task, fence, HEAD, None)?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(HEAD, "main")),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope.clone(),
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    assert_eq!(
        backend.effect(mutation(merge(HEAD, None)?)?),
        Err(IntegrationError::PermissionDenied)
    );
    // Persisted without the executor's admission, it is still refused.
    let record = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan_at(
            &task,
            fence,
            "merge",
            unchecked_merge(&scope, None)?,
            revision,
        )?,
        &ManualClock::starting_at(1),
    )?;
    assert!(matches!(record.state(), EffectState::NotApplied { .. }));
    assert_eq!(remote.borrow().reads.get(), 0);
    assert!(remote.borrow().calls.is_empty());
    Ok(())
}

#[test]
fn merge_below_readiness_is_refused_on_the_generic_executor_path() -> TestResult {
    use kitchen::house::{HouseError, ReadinessLevel};
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(&fixture, 3, &[Permission::Merge], "github")?;
    let revision = record_subject(&fixture, &task, fence, HEAD, Some(BASE))?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(HEAD, "main")),
        ..Remote::default()
    }));
    // The house holds a standing merge grant, but firmware work is below
    // policy and no owner approved this pull request: no grant resolves.
    let below = merge_authority(&[("firmware", ReadinessLevel::Covered)])?;
    assert!(matches!(
        MergeGrant::resolve(&below, &merge_subject()?, &BackendId::new("github")?),
        Err(HouseError::BelowReadiness { .. })
    ));
    let clock = ManualClock::starting_at(1);
    let attempt = |name: &str, backend: &GitHubExecutor<Provider>| {
        run_effect(
            &fixture.store,
            backend,
            &grants,
            plan_at(
                &task,
                fence,
                name,
                unchecked_merge(&scope, Some(BASE))?,
                revision,
            )?,
            &clock,
        )
        .map_err(Into::<Box<dyn std::error::Error>>::into)
    };
    let executor = |grant: MergeGrant| -> TestResult<GitHubExecutor<Provider>> {
        Ok(GitHubExecutor::new(
            BackendId::new("github")?,
            scope.clone(),
            provider(&fixture, &task, remote.clone())?,
            ReadLimits::default(),
        )
        .with_merge_grant(grant))
    };
    // Without a grant, or with one for another head, neither admission nor
    // execution of an independently persisted merge reaches GitHub.
    let mut moved = merge_subject()?;
    moved.head = commit(MOVED)?;
    let other = MergeGrant::resolve(&merge_authority(&[])?, &moved, &BackendId::new("github")?)?;
    for (name, grant) in [("ungranted", MergeGrant::none()), ("other-head", other)] {
        let backend = executor(grant)?;
        assert_eq!(
            backend.effect(mutation(merge(HEAD, Some(BASE))?)?),
            Err(IntegrationError::PermissionDenied)
        );
        let record = attempt(name, &backend)?;
        assert!(
            matches!(
                record.state(),
                EffectState::NotApplied {
                    reason: NotAppliedReason::Rejected,
                    ..
                }
            ),
            "{name}: {:?}",
            record.state()
        );
    }
    assert_eq!(remote.borrow().reads.get(), 0);
    assert!(remote.borrow().calls.is_empty());
    // The same house at the required level merges through the same path.
    let backend = executor(granted()?)?;
    let merged = attempt("granted", &backend)?;
    assert!(matches!(merged.state(), EffectState::Applied { .. }));
    assert_eq!(remote.borrow().calls.len(), 1);
    assert_eq!(remote.borrow().calls[0].1["sha"], HEAD);
    Ok(())
}

#[test]
fn merge_reconciliation_clears_a_moved_head_but_not_an_unchanged_one() -> TestResult {
    let fixture = Fixture::new()?;
    let (scope, grants, task, fence) = setup(
        &fixture,
        3,
        &[Permission::Merge, Permission::PostComment],
        "github",
    )?;
    let revision = record_subject(&fixture, &task, fence, HEAD, Some(BASE))?;
    let remote = Rc::new(RefCell::new(Remote {
        pull_request: Some(open_pull_request(HEAD, "main")),
        fault: Some(Fault::LoseBeforeApply),
        ..Remote::default()
    }));
    let backend = GitHubExecutor::new(
        BackendId::new("github")?,
        scope,
        provider(&fixture, &task, remote.clone())?,
        ReadLimits::default(),
    )
    .with_merge_grant(granted()?);
    let clock = ManualClock::starting_at(1);
    let lost = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan_at(
            &task,
            fence,
            "merge",
            backend.effect(mutation(merge(HEAD, Some(BASE))?)?)?,
            revision,
        )?,
        &clock,
    )?;
    assert!(matches!(
        lost.state(),
        EffectState::Uncertain {
            reason: UncertainReason::Timeout,
            ..
        }
    ));
    let comment = backend.effect(mutation(GitHubAction::PostComment {
        issue: IssueNumber::new(1)?,
        body: Text::new("merge could not be completed")?,
    })?)?;
    let follow_up = |name: &str| -> TestResult<Result<EffectRecord, Error>> {
        let plan = plan_at(&task, fence, name, comment.clone(), revision)?;
        Ok(run_effect(&fixture.store, &backend, &grants, plan, &clock))
    };
    // The head is unchanged, so GitHub cannot show the request never arrived:
    // it stays unresolved and blocks the task's next effect.
    let unchanged = kitchen::state::reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(unchanged.resolved.is_empty());
    assert_eq!(unchanged.unresolved.len(), 1);
    assert!(matches!(
        unchanged.unresolved[0].state(),
        EffectState::Uncertain {
            reason: UncertainReason::LookupInconclusive,
            ..
        }
    ));
    assert!(matches!(
        follow_up("blocked")?,
        Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
    ));
    // A pull request that moved off the approved head can no longer receive
    // the lost merge, because the request carried the approved head as `sha`.
    remote.borrow_mut().pull_request = Some(open_pull_request(MOVED, "main"));
    let moved = kitchen::state::reconcile(&fixture.store, &backend, &task, fence, &clock)?;
    assert!(moved.unresolved.is_empty());
    assert_eq!(moved.resolved.len(), 1);
    assert!(matches!(
        moved.resolved[0].state(),
        EffectState::NotApplied {
            reason: NotAppliedReason::ConfirmedAbsent,
            ..
        }
    ));
    assert_eq!(
        remote.borrow().calls.len(),
        1,
        "reconciliation never writes"
    );
    assert!(matches!(
        follow_up("proceeds")??.state(),
        EffectState::Applied { .. }
    ));
    Ok(())
}

#[test]
fn roger_submit_errors_map_to_definite_refusal_or_uncertainty() -> TestResult {
    enum Expected {
        NotApplied(NotAppliedReason),
        Uncertain(UncertainReason),
    }
    // Only errors raised before Roger could have acted are definite refusals.
    let cases = [
        (
            IntegrationError::InvalidInput,
            Expected::NotApplied(NotAppliedReason::Rejected),
        ),
        (
            IntegrationError::ScopeMismatch,
            Expected::NotApplied(NotAppliedReason::Rejected),
        ),
        (
            IntegrationError::Timeout,
            Expected::Uncertain(UncertainReason::Timeout),
        ),
        (
            IntegrationError::Unavailable,
            Expected::Uncertain(UncertainReason::Transport),
        ),
        (
            IntegrationError::Unknown,
            Expected::Uncertain(UncertainReason::Transport),
        ),
    ];
    for (error, expected) in cases {
        let fixture = Fixture::new()?;
        let (scope, grants, task, fence) = setup(&fixture, 1, &[Permission::AskHuman], "roger")?;
        let remote = Rc::new(RefCell::new(Remote {
            fault: Some(Fault::Roger(error)),
            ..Remote::default()
        }));
        let backend = RogerExecutor::new(
            BackendId::new("roger")?,
            scope,
            provider(&fixture, &task, remote.clone())?,
            ReadLimits::default(),
        );
        let asked = evidenced_question(&fixture, &task, fence)?;
        let revision = asked.binding.revision;
        let effect = backend.effect(asked)?;
        let clock = ManualClock::starting_at(1);
        let ask = |name: &str| -> TestResult<Result<EffectRecord, Error>> {
            let plan = plan_at(&task, fence, name, effect.clone(), revision)?;
            Ok(run_effect(&fixture.store, &backend, &grants, plan, &clock))
        };
        let first = ask("ask")??;
        assert_eq!(remote.borrow().calls.len(), 1, "{error:?}");
        match expected {
            Expected::NotApplied(reason) => {
                assert!(
                    matches!(
                        first.state(),
                        EffectState::NotApplied { reason: got, .. } if *got == reason
                    ),
                    "{error:?}: {:?}",
                    first.state()
                );
                // A definite refusal releases the task's single ask.
                assert!(matches!(
                    ask("again")??.state(),
                    EffectState::Applied { .. }
                ));
                assert_eq!(remote.borrow().calls.len(), 2);
            }
            Expected::Uncertain(reason) => {
                assert!(
                    matches!(
                        first.state(),
                        EffectState::Uncertain { reason: got, .. } if *got == reason
                    ),
                    "{error:?}: {:?}",
                    first.state()
                );
                // An uncertain ask blocks the task until it is reconciled.
                assert!(matches!(
                    ask("again")?,
                    Err(Error::State(StateError::UnresolvedEffects { count: 1 }))
                ));
                assert_eq!(remote.borrow().calls.len(), 1);
            }
        }
    }
    Ok(())
}
