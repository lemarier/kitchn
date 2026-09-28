//! Durable integration effects with sanitized in-memory providers, never live writes.
mod common;
use common::{Fixture, ManualClock, TestResult, at, creator, house, plan, scheduled, spec, ttl};
use kitchen::{
    BackendId, CredentialId, Error, TaskId,
    contracts::*,
    integrations::{github::*, roger::*},
    state::{EffectState, HouseStore, run_effect},
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

#[derive(Clone, Copy)]
enum Fault {
    Reject,
    LoseAfterApply,
    LoseBeforeApply,
}
#[derive(Default)]
struct Remote {
    labels: Vec<Value>,
    comments: Vec<Value>,
    issues: Vec<Value>,
    relations: BTreeMap<String, Vec<Value>>,
    calls: Vec<(String, Value)>,
    fault: Option<Fault>,
    read_failure: bool,
    asks: BTreeMap<String, Value>,
    hide_lookup: bool,
}
struct Provider {
    remote: Rc<RefCell<Remote>>,
    store: HouseStore,
    task: TaskId,
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
        if remote.read_failure {
            return Err(IntegrationError::Unavailable);
        }
        let path = request.endpoint();
        let value = if path.contains("/comments?") {
            json!(remote.comments)
        } else if path.contains("/labels?") {
            json!(remote.labels)
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
            json!(remote.issues)
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
            self.intended(),
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
    let remote = Rc::new(RefCell::new(Remote::default()));
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
            self.store.task(&self.task).is_ok_and(|task| task
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
        if matches!(fault, Some(Fault::Reject | Fault::LoseBeforeApply)) {
            return Err(IntegrationError::Unavailable);
        }
        let binding = &ask.binding;
        let value = json!({"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","requester":credential.requester().as_str(),"repo":binding.repository.as_str(),"decisionKey":binding.decision_key()?,"resume":{"task":binding.task.as_str(),"rev":binding.subject.as_str()},"action":{"verb":binding.action.as_str(),"target":binding.target.as_str(),"rev":binding.subject.as_str(),"limits":binding.limits.as_str()},"kind":"approval","title":ask.title.as_str(),"body":ask.body.as_str(),"state":"open","answer":null,"supersededBy":null});
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
            subject: CommitId::new(&"a".repeat(40))?,
            limits: Text::new("squash into main")?,
        },
        kind: AskKind::Approval,
        risk: AskRisk::Sensitive,
        title: Text::new("Approve this revision?")?,
        body: Text::new("Sanitized evidence and recommendation")?,
        supersedes: None,
    })
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
    let effect = backend.effect(question(&task)?)?;
    let first = run_effect(
        &fixture.store,
        &backend,
        &grants,
        plan(&task, fence, "ask", effect.clone())?,
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
        plan(&task, fence, "ask", effect.clone())?,
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
    let wrong = backend.effect(question(&TaskId::new("wrong-task")?)?)?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan(&task, fence, "wrong", wrong)?,
            &ManualClock::starting_at(1)
        ),
        Err(Error::Contract(ContractError::DecisionBindingMismatch))
    ));
    let mut stale = question(&task)?;
    stale.binding.revision = serde_json::from_str("1")?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan(&task, fence, "stale", backend.effect(stale)?)?,
            &ManualClock::starting_at(1)
        ),
        Err(Error::Contract(ContractError::DecisionBindingMismatch))
    ));
    assert!(remote.borrow().calls.is_empty());
    let effect = backend.effect(question(&task)?)?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &grants,
            plan(&task, fence, "first", effect.clone())?,
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
            plan(&task, fence, "second", effect)?,
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
