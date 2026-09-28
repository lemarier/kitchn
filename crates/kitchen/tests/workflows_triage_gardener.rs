//! Offline triage and gardener behavior fixtures.

use kitchen::{
    CredentialId, HouseId, TaskId,
    contracts::{
        CommitId, DecisionBinding, EvidenceRevision, ExternalRef, Permission, PostingBudget,
        Repository, Text,
    },
    integrations::github::{
        CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError, ReadLimits,
        ReadRequest,
    },
    integrations::roger::{DecisionStatus, RogerClient, RogerReadTransport},
};
use kitchen::{
    contracts::{DecisionOwner, GitHubAction, IssueNumber},
    workflows::{ClaimState, Precheck, WorkflowError, gardener, triage},
};
use std::{cell::RefCell, collections::VecDeque, time::Duration};

#[derive(Default)]
struct FakeGitHub {
    pages: RefCell<VecDeque<Result<Vec<u8>, IntegrationError>>>,
}
impl GitHubReadTransport for FakeGitHub {
    fn read(
        &self,
        _: &CredentialRef,
        _: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        self.pages
            .borrow_mut()
            .pop_front()
            .unwrap_or(Err(IntegrationError::Unavailable))
    }
}
struct FakeRoger(Vec<u8>);
impl RogerReadTransport for FakeRoger {
    fn get(
        &self,
        _: &CredentialRef,
        _: &ExternalRef,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        Ok(self.0.clone())
    }
}
fn github_scope() -> Result<HouseScope, Box<dyn std::error::Error>> {
    let house = HouseId::new("sample")?;
    let requester = ExternalRef::new("sample-bot")?;
    Ok(HouseScope::new(
        house.clone(),
        [Repository::new("sample/project")?],
        requester.clone(),
        CredentialRef::new(house, CredentialId::new("read")?, requester),
        PostingBudget::new(0)?,
        [Permission::PostComment],
    )?)
}
fn github_client(
    pages: Vec<serde_json::Value>,
) -> Result<GitHubClient<FakeGitHub>, Box<dyn std::error::Error>> {
    let bytes = pages
        .into_iter()
        .map(|page| serde_json::to_vec(&page).map_err(|_| IntegrationError::Unknown))
        .collect();
    Ok(GitHubClient::new(
        github_scope()?,
        FakeGitHub {
            pages: RefCell::new(bytes),
        },
        ReadLimits::default(),
    ))
}

#[test]
fn triage_collects_complete_forge_sources_and_rejects_partial_reads()
-> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;
    let issue_json = json!({"repository_url":"https://api.github.com/repos/sample/project","id":10,"number":10,"title":"issue","state":"open","assignees":[],"labels":[]});
    let detail = json!({"number":10,"state":"open","user":{"login":"owner"},"body":"request","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null});
    let pages = vec![
        issue_json.clone(),
        detail.clone(),
        json!([]),
        json!([]),
        json!([]),
        json!([]),
        json!({"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}),
        detail.clone(),
        issue_json,
    ];
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let client = github_client(pages.clone())?;
    let collected = triage::collect_issue(&client, &house, &repo, issue(10))?;
    assert_eq!(collected.detail.body.as_deref(), Some("request"));
    assert!(collected.linked_prs.is_empty());
    assert_eq!(collected.last_comment()?, None);
    let mut with_comments = collected.clone();
    for id in [17, 11] {
        with_comments
            .comments
            .push(kitchen::integrations::github::IssueComment {
                id,
                user: kitchen::integrations::github::User {
                    login: "owner".into(),
                },
                body: "question".into(),
                created_at: "2026-01-02T00:00:00Z".into(),
                updated_at: "2026-01-02T00:00:00Z".into(),
            });
    }
    assert_eq!(
        with_comments
            .last_comment()?
            .as_ref()
            .map(ExternalRef::as_str),
        Some("17")
    );
    let mut stale = pages.clone();
    stale[7]["updated_at"] = json!("2026-01-03T00:00:00Z");
    let client = github_client(stale)?;
    assert!(matches!(
        triage::collect_issue(&client, &house, &repo, issue(10)),
        Err(WorkflowError::IncompleteEvidence)
    ));
    let mut stale_labels = pages.clone();
    stale_labels[8]["labels"] =
        json!([{"name":"agent-working","color":"000000","description":null}]);
    let client = github_client(stale_labels)?;
    assert!(matches!(
        triage::collect_issue(&client, &house, &repo, issue(10)),
        Err(WorkflowError::IncompleteEvidence)
    ));
    let client = github_client(pages[..3].to_vec())?;
    assert!(matches!(
        triage::collect_issue(&client, &house, &repo, issue(10)),
        Err(WorkflowError::PrecheckFailed)
    ));
    let client = github_client(pages)?;
    assert!(matches!(
        triage::collect_issue(&client, &HouseId::new("foreign")?, &repo, issue(10)),
        Err(WorkflowError::PrecheckFailed)
    ));
    Ok(())
}

#[test]
fn spec_answer_is_routed_and_bound_before_it_can_resume() -> Result<(), Box<dyn std::error::Error>>
{
    use serde_json::json;
    let mut binding = DecisionBinding {
        house: HouseId::new("sample")?,
        task: TaskId::new("t01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
        owner: DecisionOwner::Spec,
        repository: Repository::new("sample/project")?,
        action: Permission::AskHuman,
        target: ExternalRef::new("issue:sample/project#10")?,
        revision: EvidenceRevision::INITIAL,
        subject: CommitId::new(&"a".repeat(40))?,
        limits: Text::new("question only")?,
    };
    let ask_id = ExternalRef::new("01ARZ3NDEKTSV4RRFFQ69G5FAV")?;
    let answer = json!({"id":ask_id,"requester":"sample-bot","repo":"sample/project","decisionKey":binding.decision_key()?,"kind":"question","action":null,"resume":{"task":binding.task,"rev":"a".repeat(40)},"state":"answered","supersededBy":null,"answer":{"decision":"other","optionId":"_custom","action":null,"input":"Use limit 3","passkey":false}});
    let client = RogerClient::new(
        github_scope()?,
        FakeRoger(serde_json::to_vec(&answer)?),
        ReadLimits::default(),
    );
    assert!(
        matches!(triage::poll_spec_answer(&client, &binding, &ask_id), Ok(DecisionStatus::Instructions(Some(text))) if text.as_str() == "Use limit 3")
    );
    binding.house = HouseId::new("foreign")?;
    assert_eq!(
        triage::poll_spec_answer(&client, &binding, &ask_id),
        Err(WorkflowError::DecisionMismatch)
    );
    binding.house = HouseId::new("sample")?;
    binding.owner = DecisionOwner::Merge;
    assert_eq!(
        triage::poll_spec_answer(&client, &binding, &ask_id),
        Err(WorkflowError::DecisionMismatch)
    );
    Ok(())
}

#[expect(clippy::unwrap_used, reason = "fixed fixture issue numbers")]
fn issue(n: u64) -> IssueNumber {
    IssueNumber::new(n).unwrap()
}
#[expect(clippy::unwrap_used, reason = "fixed fixture repository")]
fn triage_input() -> triage::Evidence {
    triage::Evidence {
        repository: Repository::new("sample/project").unwrap(),
        issue: issue(10),
        revision: "r1".into(),
        coverage: triage::Coverage {
            history: true,
            code: true,
            requirements: true,
            relationships: true,
            linked_prs: true,
        },
        changed_since_last_pass: true,
        needs_spec: true,
        human_only: false,
        claim: ClaimState::Unclaimed,
        open_dependencies: false,
        factual_resolution: Some("Resolved from code".into()),
        resolution_already_posted: false,
        pending_product_questions: 0,
        existing_decisions: vec![],
        ready_label_present: false,
        ready_label: "agent-ready".into(),
        needs_spec_label: "needs-spec".into(),
    }
}
#[test]
fn routing_is_closed_and_reports_unknown_keys() {
    assert_eq!(triage::route_answer("task:one"), Ok(DecisionOwner::Task));
    assert_eq!(triage::route_answer("spec:one"), Ok(DecisionOwner::Spec));
    assert_eq!(triage::route_answer("merge:one"), Ok(DecisionOwner::Merge));
    assert_eq!(
        triage::route_answer("policy:one"),
        Err(WorkflowError::UnknownDecisionOwner)
    );
    assert_eq!(
        triage::route_answer("spec:"),
        Err(WorkflowError::UnknownDecisionOwner)
    );
}
#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test asserts the successful planning path"
)]
fn triage_resolves_then_rerun_is_idle() {
    let mut input = triage_input();
    let plan = triage::plan(&input).unwrap();
    assert_eq!(plan.len(), 2);
    assert!(
        matches!(&plan[0], triage::Change::Mutation(GitHubAction::PostComment { body, .. }) if body.as_str() == "Resolved from code")
    );
    assert!(matches!(
        &plan[1],
        triage::Change::Mutation(GitHubAction::SetLabel { present: true, .. })
    ));
    input.changed_since_last_pass = false;
    assert_eq!(triage::precheck(&input), Ok(Precheck::Actionable));
    input.resolution_already_posted = true;
    assert_eq!(triage::plan(&input).unwrap().len(), 1);
    input.ready_label_present = true;
    assert_eq!(triage::plan(&input).unwrap().len(), 1); // clear needs-spec
    input.needs_spec = false;
    assert_eq!(triage::precheck(&input), Ok(Precheck::Idle));
    assert!(triage::plan(&input).unwrap().is_empty());
}
#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test asserts the successful planning path"
)]
fn triage_keeps_unanswered_and_expired_decisions() {
    let mut input = triage_input();
    input.existing_decisions.push(triage::Decision {
        issue: issue(10),
        owner: DecisionOwner::Spec,
        revision: "r1".into(),
        state: triage::DecisionState::Expired,
    });
    assert_eq!(triage::plan(&input).unwrap().len(), 1); // comment only
    input.existing_decisions[0].revision = "old".into();
    assert_eq!(triage::plan(&input), Err(WorkflowError::DecisionMismatch));
    input.existing_decisions[0].revision = "r1".into();
    input.existing_decisions[0].issue = issue(11);
    assert_eq!(triage::plan(&input), Err(WorkflowError::DecisionMismatch));
    input.existing_decisions.clear();
    input.claim = ClaimState::ClaimedByOther;
    assert!(triage::plan(&input).unwrap().is_empty());
    input.claim = ClaimState::Unknown;
    assert_eq!(triage::plan(&input), Err(WorkflowError::IncompleteEvidence));
}
#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test asserts the successful planning path"
)]
fn triage_caps_asks_and_rejects_partial_evidence() {
    let mut input = triage_input();
    input.factual_resolution = None;
    input.pending_product_questions = 4;
    assert_eq!(triage::plan(&input).unwrap().len(), 3);
    input.existing_decisions.push(triage::Decision {
        issue: issue(10),
        owner: DecisionOwner::Spec,
        revision: "r1".into(),
        state: triage::DecisionState::Open,
    });
    assert_eq!(triage::plan(&input).unwrap().len(), 2);
    input.coverage.history = false;
    assert_eq!(
        triage::precheck(&input),
        Err(WorkflowError::IncompleteEvidence)
    );
}

#[test]
fn unresolved_issue_requests_one_bounded_gardener_worker() {
    use kitchen::contracts::{Capability, Operation, Role, Workspace};
    let mut input = triage_input();
    input.factual_resolution = None;
    let plan = triage::plan(&input);
    assert!(matches!(
        plan,
        Ok(changes) if matches!(&changes[..], [triage::Change::Judgment(Operation::LaunchWorker {
            role: Role::Gardener,
            workspace: Workspace::Isolated,
            brief,
        })] if brief.as_str().contains("sample/project issue #10"))
    ));
    let request = triage::judgment_request(&input);
    assert!(
        matches!(request, Ok(Some(op)) if op.required_capability() == Capability::WorkerLaunchIsolated)
    );
    input.claim = ClaimState::ClaimedByOther;
    assert_eq!(triage::judgment_request(&input), Ok(None));
}

struct FakeMarkers {
    posted: Result<bool, WorkflowError>,
    decisions: Result<Vec<triage::Decision>, WorkflowError>,
}

impl triage::MarkerView for FakeMarkers {
    fn resolution_posted(
        &self,
        _issue: IssueNumber,
        _revision: &str,
    ) -> Result<bool, WorkflowError> {
        self.posted
    }

    fn decisions(
        &self,
        _issue: IssueNumber,
        _revision: &str,
    ) -> Result<Vec<triage::Decision>, WorkflowError> {
        self.decisions.clone()
    }
}

#[test]
fn marker_read_controls_repetition_and_propagates_failure() {
    let mut input = triage_input();
    input.resolution_already_posted = false;
    let markers = FakeMarkers {
        posted: Ok(true),
        decisions: Ok(vec![]),
    };
    assert!(
        matches!(triage::plan_with_markers(&input, &markers), Ok(changes) if changes.len() == 1)
    );
    let failed = FakeMarkers {
        posted: Err(WorkflowError::PrecheckFailed),
        decisions: Ok(vec![]),
    };
    assert_eq!(
        triage::plan_with_markers(&input, &failed),
        Err(WorkflowError::PrecheckFailed)
    );
    let failed_decisions = FakeMarkers {
        posted: Ok(true),
        decisions: Err(WorkflowError::PrecheckFailed),
    };
    assert_eq!(
        triage::plan_with_markers(&input, &failed_decisions),
        Err(WorkflowError::PrecheckFailed)
    );
    input.human_only = true;
    assert_eq!(triage::plan_with_markers(&input, &failed), Ok(vec![]));
}
fn hygiene_issue() -> gardener::Issue {
    gardener::Issue {
        number: issue(20),
        state: kitchen::integrations::github::IssueState::Open,
        human_only: false,
        claim: ClaimState::Unclaimed,
        labels: vec![],
        prose_blocker: Some(issue(19)),
        linked_blocker: false,
        blocker_open: true,
        parent_completed: false,
        merged_work: false,
        duplicate_of: None,
        stale: false,
    }
}

#[expect(clippy::unwrap_used, reason = "fixed house label fixture")]
fn hygiene_plan(issues: &[gardener::Issue]) -> Vec<gardener::Finding> {
    gardener::plan(
        issues,
        &gardener::AgentLabels {
            ready: "agent-ready".into(),
            working: "agent-working".into(),
        },
    )
    .unwrap()
}
#[test]
fn gardener_has_independent_idle_error_and_actionable_precheck() {
    assert_eq!(gardener::SCHEDULE.owner, "gardener");
    assert_eq!(gardener::SCHEDULE.cadence, "daily");
    assert_eq!(gardener::SCHEDULE.precheck, "gardener-hygiene");
    let quiet = gardener::Signal {
        daily_changes: false,
        stale_issue: false,
        closed_agent_label: false,
    };
    assert_eq!(gardener::precheck(Ok(quiet)), Ok(Precheck::Idle));
    assert_eq!(
        gardener::precheck(Err(WorkflowError::PrecheckFailed)),
        Err(WorkflowError::PrecheckFailed)
    );
    assert_eq!(
        gardener::precheck(Ok(gardener::Signal {
            stale_issue: true,
            ..quiet
        })),
        Ok(Precheck::Actionable)
    );
}

#[test]
#[expect(clippy::unwrap_used, reason = "fixed fixture identifiers")]
fn gardener_activation_requires_backend_precheck_capability() {
    use kitchen::{
        BackendId, HouseId,
        contracts::{Capability, CapabilitySet, EffectExecutor, fake::FakeBackend},
    };
    let backend = FakeBackend::new(
        BackendId::new("fixture").unwrap(),
        HouseId::new("house").unwrap(),
        CapabilitySet::supporting([Capability::ScheduleManage]),
    );
    let error = backend
        .descriptor()
        .capabilities
        .require(gardener::REQUIRED_CAPABILITIES);
    assert!(error.is_err());
    let capable = FakeBackend::fully_capable(
        BackendId::new("fixture").unwrap(),
        HouseId::new("house").unwrap(),
    );
    assert_eq!(
        capable
            .descriptor()
            .capabilities
            .require(gardener::REQUIRED_CAPABILITIES),
        Ok(())
    );
}
#[test]
fn gardener_previews_dependency_once_and_preserves_claims() {
    let mut input = hygiene_issue();
    assert!(matches!(
        &hygiene_plan(&[input.clone()])[0],
        gardener::Finding::Mutation(GitHubAction::LinkDependency { .. })
    ));
    input.linked_blocker = true;
    assert!(hygiene_plan(&[input.clone()]).is_empty());
    input.linked_blocker = false;
    input.claim = ClaimState::ClaimedByOther;
    assert!(hygiene_plan(&[input]).is_empty());
}

#[test]
fn gardener_refuses_ambiguous_house_labels() {
    let labels = gardener::AgentLabels {
        ready: "same".into(),
        working: "same".into(),
    };
    assert_eq!(
        gardener::plan(&[hygiene_issue()], &labels),
        Err(WorkflowError::IncompleteEvidence)
    );
}

#[test]
fn gardener_rejects_unknown_issue_lifecycle() {
    let mut input = hygiene_issue();
    input.state = kitchen::integrations::github::IssueState::Unknown;
    let labels = gardener::AgentLabels {
        ready: "agent-ready".into(),
        working: "agent-working".into(),
    };
    assert_eq!(
        gardener::plan(&[input], &labels),
        Err(WorkflowError::IncompleteEvidence)
    );
    let mut uncertain = hygiene_issue();
    uncertain.claim = ClaimState::Unknown;
    assert_eq!(
        gardener::plan(&[uncertain], &labels),
        Err(WorkflowError::IncompleteEvidence)
    );
}
#[test]
fn gardener_reports_closed_residue_and_review_only_work() {
    let mut closed = hygiene_issue();
    closed.state = kitchen::integrations::github::IssueState::Closed;
    closed.labels = vec!["agent-working".into(), "user-label".into()];
    assert_eq!(hygiene_plan(&[closed.clone()]).len(), 1);
    closed.claim = ClaimState::ClaimedByOther;
    assert!(hygiene_plan(&[closed]).is_empty());
    let mut open = hygiene_issue();
    open.prose_blocker = None;
    open.merged_work = true;
    open.duplicate_of = Some(issue(15));
    open.stale = true;
    assert_eq!(hygiene_plan(&[open]).len(), 3);
}
