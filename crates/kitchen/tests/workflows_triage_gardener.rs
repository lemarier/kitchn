//! Offline triage and gardener behavior fixtures. Forge and Roger reads are
//! simulated; markers use a temporary house store.

mod common;

use kitchen::{
    CredentialId, HouseId, TaskId,
    contracts::{
        CommitId, DecisionBinding, EvidenceRevision, EvidenceSubject, ExternalRef, Permission,
        PostingBudget, Repository, Text,
    },
    integrations::github::{
        CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError, ReadLimits,
        ReadRequest,
    },
    integrations::roger::{DecisionStatus, RogerClient, RogerReadTransport},
};
use kitchen::{WorkflowId, contracts::Timestamp};
use kitchen::{
    contracts::{DecisionOwner, GitHubAction, IssueNumber},
    state::IssueRevision,
    workflows::{ClaimState, Precheck, WorkflowError, gardener, triage},
};
use std::{cell::RefCell, collections::VecDeque, fs, time::Duration};

#[derive(Default)]
struct FakeGitHub {
    pages: RefCell<VecDeque<Result<Vec<u8>, IntegrationError>>>,
    endpoints: RefCell<Vec<String>>,
}
impl GitHubReadTransport for FakeGitHub {
    fn read(
        &self,
        _: &CredentialRef,
        request: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        self.endpoints
            .borrow_mut()
            .push(request.endpoint().to_owned());
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
            endpoints: RefCell::default(),
        },
        ReadLimits::default(),
    ))
}

#[test]
fn triage_collects_complete_forge_sources_and_rejects_partial_reads()
-> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;
    let issue_json = json!({"repository_url":"https://api.github.com/repos/sample/project","id":10,"number":10,"title":"issue","state":"open","assignees":[],"labels":[],"updated_at":"2026-01-02T00:00:00Z","closed_at":null});
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
    let updated = Timestamp::from_unix_millis(1_767_312_000_000);
    assert_eq!(
        collected.revision()?,
        IssueRevision {
            updated_at: updated,
            last_comment: None,
        }
    );
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
                created_at: updated,
                updated_at: updated,
            });
    }
    assert_eq!(
        with_comments
            .last_comment()?
            .as_ref()
            .map(ExternalRef::as_str),
        Some("17")
    );
    assert_eq!(
        with_comments.revision()?,
        IssueRevision {
            updated_at: updated,
            last_comment: Some(ExternalRef::new("17")?),
        }
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
        subject: Some(EvidenceSubject {
            head: CommitId::new(&"a".repeat(40))?,
            base: None,
        }),
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
fn revision(seconds: u64, last_comment: Option<&str>) -> IssueRevision {
    IssueRevision {
        updated_at: common::at(seconds),
        last_comment: last_comment.and_then(|id| ExternalRef::new(id).ok()),
    }
}
#[expect(clippy::unwrap_used, reason = "fixed fixture decision ids")]
fn resolution(decision: &str, body: &str) -> triage::Resolution {
    triage::Resolution {
        decision: ExternalRef::new(decision).unwrap(),
        kind: triage::ResolutionKind::Evidence,
        body: body.into(),
    }
}
#[expect(clippy::unwrap_used, reason = "fixed fixture repository")]
fn triage_input() -> triage::Evidence {
    triage::Evidence {
        repository: Repository::new("sample/project").unwrap(),
        issue: issue(10),
        revision: revision(1, None),
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
        factual_resolution: Some(resolution("spec-10", "Resolved from code")),
        pending_product_questions: 0,
        existing_decisions: vec![],
        ready_label_present: false,
        ready_label: "agent-ready".into(),
        needs_spec_label: "needs-spec".into(),
        agent: None,
    }
}
/// Marker history supplied directly; the store-backed view has its own tests.
#[derive(Default)]
struct Recorded {
    asked: Vec<IssueRevision>,
    posted: Vec<(ExternalRef, triage::ResolutionKind)>,
}
impl triage::MarkerView for Recorded {
    fn asked(&self, _: &Repository, _: IssueNumber) -> Result<Vec<IssueRevision>, WorkflowError> {
        Ok(self.asked.clone())
    }
    fn resolution_posted(
        &self,
        _: &Repository,
        _: IssueNumber,
        resolution: &triage::Resolution,
    ) -> Result<bool, WorkflowError> {
        Ok(self
            .posted
            .iter()
            .any(|(decision, kind)| decision == &resolution.decision && *kind == resolution.kind))
    }
}
fn plan_after(
    markers: &Recorded,
    input: &triage::Evidence,
) -> Result<Vec<triage::Change>, WorkflowError> {
    triage::plan(input, &triage::History::read(markers, input)?)
}
fn precheck_after(markers: &Recorded, input: &triage::Evidence) -> Result<Precheck, WorkflowError> {
    triage::precheck(input, &triage::History::read(markers, input)?)
}
fn fresh_plan(input: &triage::Evidence) -> Result<Vec<triage::Change>, WorkflowError> {
    plan_after(&Recorded::default(), input)
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
    let mut markers = Recorded::default();
    let plan = plan_after(&markers, &input).unwrap();
    assert_eq!(plan.len(), 2);
    assert!(
        matches!(&plan[0], triage::Change::Mutation(GitHubAction::PostComment { body, .. }) if body.as_str() == "Resolved from code")
    );
    assert!(matches!(
        &plan[1],
        triage::Change::Mutation(GitHubAction::SetLabel { present: true, .. })
    ));
    input.changed_since_last_pass = false;
    assert_eq!(precheck_after(&markers, &input), Ok(Precheck::Actionable));
    markers.posted.push((
        ExternalRef::new("spec-10").unwrap(),
        triage::ResolutionKind::Evidence,
    ));
    assert_eq!(plan_after(&markers, &input).unwrap().len(), 1);
    input.ready_label_present = true;
    assert_eq!(plan_after(&markers, &input).unwrap().len(), 1); // clear needs-spec
    input.needs_spec = false;
    assert_eq!(precheck_after(&markers, &input), Ok(Precheck::Idle));
    assert!(plan_after(&markers, &input).unwrap().is_empty());
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
        revision: revision(1, None),
        state: triage::DecisionState::Expired,
    });
    assert_eq!(fresh_plan(&input).unwrap().len(), 1); // comment only
    input.existing_decisions[0].revision = revision(0, None);
    assert_eq!(fresh_plan(&input), Err(WorkflowError::DecisionMismatch));
    input.existing_decisions[0].revision = revision(1, None);
    input.existing_decisions[0].issue = issue(11);
    assert_eq!(fresh_plan(&input), Err(WorkflowError::DecisionMismatch));
    input.existing_decisions.clear();
    input.claim = ClaimState::ClaimedByOther;
    assert!(fresh_plan(&input).unwrap().is_empty());
    input.claim = ClaimState::Unknown;
    assert_eq!(fresh_plan(&input), Err(WorkflowError::IncompleteEvidence));
}
#[test]
#[expect(
    clippy::unwrap_used,
    reason = "test asserts the successful planning path"
)]
fn triage_asks_once_per_revision_within_the_task_budget() {
    let mut input = triage_input();
    let mut markers = Recorded::default();
    input.factual_resolution = None;
    input.pending_product_questions = 4;
    // Questions are batched into one ask per issue revision.
    let asks = |markers: &Recorded, input: &triage::Evidence| {
        plan_after(markers, input)
            .unwrap()
            .into_iter()
            .filter_map(|change| match change {
                triage::Change::Ask { ordinal } => Some(ordinal),
                triage::Change::Judgment(_) | triage::Change::Mutation(_) => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(asks(&markers, &input), vec![0]);
    markers.asked = vec![revision(0, None)];
    assert_eq!(asks(&markers, &input), vec![1]);
    markers.asked.push(revision(1, None));
    assert!(
        asks(&markers, &input).is_empty(),
        "already asked at this revision"
    );
    markers.asked = vec![
        revision(0, None),
        revision(0, Some("3")),
        revision(0, Some("4")),
    ];
    assert!(
        asks(&markers, &input).is_empty(),
        "task budget of three asks is spent"
    );
    markers.asked.clear();
    input.existing_decisions.push(triage::Decision {
        issue: issue(10),
        owner: DecisionOwner::Spec,
        revision: revision(1, None),
        state: triage::DecisionState::Open,
    });
    assert!(
        asks(&markers, &input).is_empty(),
        "an open decision is not repeated"
    );
    input.coverage.history = false;
    assert_eq!(
        precheck_after(&markers, &input),
        Err(WorkflowError::IncompleteEvidence)
    );
}

#[test]
fn a_posted_resolution_is_idle_while_the_plan_cannot_change_a_label() -> common::TestResult {
    let mut markers = Recorded::default();
    markers.posted.push((
        ExternalRef::new("spec-10")?,
        triage::ResolutionKind::Evidence,
    ));
    let mut input = triage_input();
    input.changed_since_last_pass = false;
    input.ready_label_present = true;

    // Each blocker stops the needs-spec removal, so neither the plan nor the
    // precheck has work, on this pass or any later one.
    let blocked: [fn(&mut triage::Evidence); 3] = [
        |input| input.open_dependencies = true,
        |input| input.pending_product_questions = 1,
        |input| {
            input.existing_decisions.push(triage::Decision {
                issue: input.issue,
                owner: DecisionOwner::Spec,
                revision: input.revision.clone(),
                state: triage::DecisionState::Open,
            });
        },
    ];
    for block in blocked {
        let mut case = input.clone();
        block(&mut case);
        assert!(plan_after(&markers, &case)?.is_empty());
        assert_eq!(precheck_after(&markers, &case), Ok(Precheck::Idle));
    }

    // Unblocked, both agree the needs-spec label comes off.
    assert_eq!(precheck_after(&markers, &input), Ok(Precheck::Actionable));
    assert_eq!(
        plan_after(&markers, &input)?,
        vec![triage::Change::Mutation(GitHubAction::SetLabel {
            issue: issue(10),
            label: "needs-spec".into(),
            present: false,
        })]
    );
    Ok(())
}

#[test]
fn unresolved_issue_requests_one_bounded_gardener_worker() -> common::TestResult {
    use kitchen::contracts::{Capability, Operation, Role, Workspace};
    let mut input = triage_input();
    input.factual_resolution = None;
    let plan = fresh_plan(&input);
    assert!(matches!(
        plan,
        Ok(changes) if matches!(&changes[..], [triage::Change::Judgment(Operation::LaunchWorker {
            role: Role::Gardener,
            workspace: Workspace::Isolated,
            brief,
            branch: None,
            agent: None,
        })] if brief.as_str().contains("sample/project issue #10"))
    ));
    let request = triage::judgment_request(
        &input,
        &triage::History::read(&Recorded::default(), &input)?,
    );
    assert!(
        matches!(request, Ok(Some(op)) if op.required_capability() == Capability::WorkerLaunchIsolated)
    );
    input.claim = ClaimState::ClaimedByOther;
    assert_eq!(
        triage::judgment_request(
            &input,
            &triage::History::read(&Recorded::default(), &input)?
        ),
        Ok(None)
    );
    Ok(())
}

#[test]
fn the_judgment_launch_carries_the_gardener_tasks_recorded_selection() -> common::TestResult {
    use kitchen::{
        contracts::Operation,
        scheduling::AgentFamily,
        selection::{AgentModel, AgentSelection},
    };
    let mut input = triage_input();
    input.factual_resolution = None;
    let recorded = AgentSelection {
        agent: AgentFamily::Claude,
        model: Some(AgentModel::new("sonnet")?),
        effort: None,
    };
    input.agent = Some(recorded.clone());
    let changes = fresh_plan(&input)?;
    assert!(
        matches!(
            &changes[..],
            [triage::Change::Judgment(Operation::LaunchWorker { agent: Some(agent), .. })]
                if agent == &recorded
        ),
        "{changes:?}"
    );
    Ok(())
}

fn triage_workflow() -> common::TestResult<WorkflowId> {
    Ok(WorkflowId::new("triage")?)
}

#[test]
fn store_markers_stop_a_repeated_question_until_the_issue_changes() -> common::TestResult {
    let fixture = common::Fixture::new()?;
    let markers = triage::IssueMarkers::new(&fixture.store, triage_workflow()?);
    let mut input = triage_input();
    input.factual_resolution = None;
    input.pending_product_questions = 1;
    let asked = |changes: &[triage::Change]| {
        changes
            .iter()
            .any(|change| matches!(change, triage::Change::Ask { .. }))
    };
    assert!(asked(&triage::plan_with_markers(&input, &markers)?));
    let workflow = triage_workflow()?;
    let recorder = common::scheduled("triage-tick")?;
    let record = |at: &IssueRevision, question: &str| -> common::TestResult<_> {
        Ok(triage::record_question(
            &fixture.store,
            &workflow,
            &input.repository,
            input.issue,
            at,
            ExternalRef::new(question)?,
            &recorder,
            common::at(5),
        ))
    };
    assert!(matches!(
        record(&input.revision, "ask-1")?,
        Ok(kitchen::state::MarkerRecording::Recorded(_))
    ));
    assert!(matches!(
        record(&input.revision, "ask-1")?,
        Ok(kitchen::state::MarkerRecording::AlreadyRecorded(_))
    ));
    // A restarted pass reads the durable marker and does not ask again.
    let reopened = fixture.reopen()?;
    let restarted = triage::IssueMarkers::new(&reopened, triage_workflow()?);
    assert!(!asked(&triage::plan_with_markers(&input, &restarted)?));
    // A second, different question at the same revision is refused.
    assert_eq!(
        record(&input.revision, "ask-2")?,
        Err(WorkflowError::DecisionMismatch)
    );
    // A new comment is a new revision; the prior ask still counts toward the budget.
    input.revision = revision(1, Some("40"));
    let changes = triage::plan_with_markers(&input, &markers)?;
    assert!(changes.contains(&triage::Change::Ask { ordinal: 1 }));
    // Another issue's markers do not affect this one.
    input.issue = issue(11);
    input.revision = revision(1, None);
    assert!(
        triage::plan_with_markers(&input, &markers)?.contains(&triage::Change::Ask { ordinal: 0 })
    );
    Ok(())
}

#[test]
fn unreadable_marker_store_fails_instead_of_asking_again() -> common::TestResult {
    let fixture = common::Fixture::new()?;
    fs::write(fixture.state_path(), b"{not json")?;
    let markers = triage::IssueMarkers::new(&fixture.store, triage_workflow()?);
    let mut input = triage_input();
    input.factual_resolution = None;
    input.pending_product_questions = 1;
    assert_eq!(
        triage::plan_with_markers(&input, &markers),
        Err(WorkflowError::PrecheckFailed)
    );
    // Work that needs no pass never reads the store.
    input.human_only = true;
    assert_eq!(triage::plan_with_markers(&input, &markers), Ok(vec![]));
    Ok(())
}

fn posts_resolution(changes: &[triage::Change]) -> bool {
    changes.iter().any(|change| {
        matches!(
            change,
            triage::Change::Mutation(GitHubAction::PostComment { .. })
        )
    })
}

#[test]
fn store_marker_stops_a_repeated_resolution_after_the_post_moves_the_revision() -> common::TestResult
{
    let fixture = common::Fixture::new()?;
    let workflow = triage_workflow()?;
    let markers = triage::IssueMarkers::new(&fixture.store, workflow.clone());
    let recorder = common::scheduled("triage-tick")?;
    let mut input = triage_input();
    let judged = input.revision.clone();
    assert!(posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    let record = |at: &IssueRevision, resolution: &triage::Resolution| {
        triage::record_resolution(
            &fixture.store,
            &workflow,
            &input.repository,
            input.issue,
            at,
            resolution,
            &recorder,
            common::at(5),
        )
    };
    assert!(matches!(
        record(&judged, &resolution("spec-10", "Resolved from code")),
        Ok(kitchen::state::MarkerRecording::Recorded(_))
    ));
    assert!(matches!(
        record(&judged, &resolution("spec-10", "Resolved from code")),
        Ok(kitchen::state::MarkerRecording::AlreadyRecorded(_))
    ));
    // Posting adds a comment, so the next pass sees a new revision. The
    // marker still matches after a restart and the comment is not repeated.
    input.revision = revision(2, Some("50"));
    input.changed_since_last_pass = true;
    let reopened = fixture.reopen()?;
    let restarted = triage::IssueMarkers::new(&reopened, triage_workflow()?);
    let changes = triage::plan_with_markers(&input, &restarted)?;
    assert!(!posts_resolution(&changes));
    assert!(
        changes.contains(&triage::Change::Mutation(GitHubAction::SetLabel {
            issue: issue(10),
            label: "agent-ready".into(),
            present: true,
        }))
    );
    // A resolution of another decision after new evidence is posted.
    input.factual_resolution = Some(resolution("spec-10-b", "Resolved by the new comment"));
    assert!(posts_resolution(&triage::plan_with_markers(
        &input, &restarted
    )?));
    // One judged revision records one decision's resolution.
    assert_eq!(
        record(&judged, &resolution("spec-10-b", "Another resolution")),
        Err(WorkflowError::DecisionMismatch)
    );
    assert_eq!(
        record(&judged, &resolution("spec-10", "  ")),
        Err(WorkflowError::IncompleteEvidence)
    );
    // Another issue's marker does not suppress this one.
    input.issue = issue(11);
    input.factual_resolution = Some(resolution("spec-10", "Resolved from code"));
    assert!(posts_resolution(&triage::plan_with_markers(
        &input, &restarted
    )?));
    Ok(())
}

#[test]
fn a_reworded_resolution_is_not_posted_again_after_the_gardeners_own_post() -> common::TestResult {
    let fixture = common::Fixture::new()?;
    let workflow = triage_workflow()?;
    let recorder = common::scheduled("triage-tick")?;
    let mut input = triage_input();
    let first = resolution("spec-10", "Resolved from code");
    triage::record_resolution(
        &fixture.store,
        &workflow,
        &input.repository,
        input.issue,
        &input.revision,
        &first,
        &recorder,
        common::at(5),
    )?;
    // The post moves the revision; the next judgment words it differently.
    let judged = input.revision.clone();
    input.revision = revision(2, Some("50"));
    input.factual_resolution = Some(resolution(
        "spec-10",
        "The limit is already enforced in the parser.",
    ));
    let markers = triage::IssueMarkers::new(&fixture.store, workflow.clone());
    assert!(!posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    // Recording the reworded resolution at the judged revision is the same
    // decision, not a conflict.
    assert!(matches!(
        triage::record_resolution(
            &fixture.store,
            &workflow,
            &input.repository,
            input.issue,
            &judged,
            &resolution("spec-10", "Reworded"),
            &recorder,
            common::at(6),
        ),
        Ok(kitchen::state::MarkerRecording::AlreadyRecorded(_))
    ));
    // The same decision settled another way is a distinct resolution.
    input.factual_resolution = Some(triage::Resolution {
        kind: triage::ResolutionKind::Answer,
        ..resolution("spec-10", "The owner chose limit 3.")
    });
    assert!(posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    Ok(())
}

fn record_digest_resolution(
    fixture: &common::Fixture,
    judged: &IssueRevision,
) -> common::TestResult {
    use kitchen::state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem};
    use std::num::{NonZeroU32, NonZeroU64};
    fixture.store.record_marker(
        MarkerKey {
            workflow: triage_workflow()?,
            item: WorkItem::Issue {
                repository: project(),
                number: NonZeroU64::new(10).ok_or("issue")?,
            },
            subject: MarkerSubject::Issue(judged.clone()),
        },
        MarkerFact::workflow(
            MarkerSchema::new("triage.resolution", NonZeroU32::MIN)?,
            &serde_json::json!({"digest": "00"}),
        )?,
        &common::scheduled("triage-tick")?,
        common::at(1),
    )?;
    Ok(())
}

#[test]
fn a_text_digest_resolution_marker_means_posted_with_the_decision_unknown() -> common::TestResult {
    let fixture = common::Fixture::new()?;
    // A version 1 marker holds only a text digest. It proves a resolution
    // was posted but not which decision it settled, so the pass neither
    // fails nor posts again.
    record_digest_resolution(&fixture, &revision(0, None))?;
    let markers = triage::IssueMarkers::new(&fixture.store, triage_workflow()?);
    let mut input = triage_input();
    assert!(!posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    // The same holds for any decision the pass judges next.
    input.factual_resolution = Some(resolution("spec-11", "Another answer"));
    assert!(!posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    Ok(())
}

#[test]
fn a_new_resolution_supersedes_a_text_digest_marker_at_its_revision() -> common::TestResult {
    use kitchen::state::{MarkerKey, MarkerRecording, MarkerSubject, WorkItem};
    use std::num::NonZeroU64;
    let fixture = common::Fixture::new()?;
    let workflow = triage_workflow()?;
    let judged = revision(0, None);
    record_digest_resolution(&fixture, &judged)?;
    let input = triage_input();
    let recorded = triage::record_resolution(
        &fixture.store,
        &workflow,
        &input.repository,
        input.issue,
        &judged,
        &resolution("spec-10", "Resolved from code"),
        &common::scheduled("triage-tick")?,
        common::at(2),
    )?;
    let MarkerRecording::Superseded(marker) = recorded else {
        return Err("expected the version 1 marker to be superseded".into());
    };
    assert_eq!(marker.history().len(), 1);
    // The identity now matches by decision, so the legacy marker no longer
    // blocks a different decision from posting.
    let markers = triage::IssueMarkers::new(&fixture.store, workflow.clone());
    assert!(!posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    let mut other = triage_input();
    other.factual_resolution = Some(resolution("spec-11", "Another answer"));
    assert!(posts_resolution(&triage::plan_with_markers(
        &other, &markers
    )?));
    let stored = fixture.store.marker(&MarkerKey {
        workflow,
        item: WorkItem::Issue {
            repository: project(),
            number: NonZeroU64::new(10).ok_or("issue")?,
        },
        subject: MarkerSubject::Issue(judged),
    })?;
    assert_eq!(stored.map(|stored| stored.history().len()), Some(1));
    Ok(())
}

#[test]
fn a_new_resolution_at_another_revision_outranks_a_text_digest_marker() -> common::TestResult {
    let fixture = common::Fixture::new()?;
    let workflow = triage_workflow()?;
    record_digest_resolution(&fixture, &revision(0, None))?;
    let input = triage_input();
    assert!(matches!(
        triage::record_resolution(
            &fixture.store,
            &workflow,
            &input.repository,
            input.issue,
            &input.revision,
            &resolution("spec-10", "Resolved from code"),
            &common::scheduled("triage-tick")?,
            common::at(2),
        ),
        Ok(kitchen::state::MarkerRecording::Recorded(_))
    ));
    let markers = triage::IssueMarkers::new(&fixture.store, workflow);
    assert!(!posts_resolution(&triage::plan_with_markers(
        &input, &markers
    )?));
    let mut other = triage_input();
    other.factual_resolution = Some(resolution("spec-11", "Another answer"));
    assert!(posts_resolution(&triage::plan_with_markers(
        &other, &markers
    )?));
    Ok(())
}

#[test]
fn an_unknown_resolution_marker_version_fails_closed() -> common::TestResult {
    use kitchen::state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem};
    use std::num::{NonZeroU32, NonZeroU64};
    let fixture = common::Fixture::new()?;
    fixture.store.record_marker(
        MarkerKey {
            workflow: triage_workflow()?,
            item: WorkItem::Issue {
                repository: project(),
                number: NonZeroU64::new(10).ok_or("issue")?,
            },
            subject: MarkerSubject::Issue(revision(0, None)),
        },
        MarkerFact::workflow(
            MarkerSchema::new("triage.resolution", NonZeroU32::new(3).ok_or("version")?)?,
            &serde_json::json!({"digest": "00"}),
        )?,
        &common::scheduled("triage-tick")?,
        common::at(1),
    )?;
    let markers = triage::IssueMarkers::new(&fixture.store, triage_workflow()?);
    assert_eq!(
        triage::plan_with_markers(&triage_input(), &markers),
        Err(WorkflowError::IncompleteEvidence)
    );
    Ok(())
}

#[test]
fn unreadable_or_foreign_resolution_markers_fail_closed() -> common::TestResult {
    use kitchen::state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem};
    use std::num::{NonZeroU32, NonZeroU64};
    let fixture = common::Fixture::new()?;
    let input = triage_input();
    // A triage marker written under another schema is not proof of either
    // outcome, so the pass stops instead of posting or skipping.
    fixture.store.record_marker(
        MarkerKey {
            workflow: triage_workflow()?,
            item: WorkItem::Issue {
                repository: project(),
                number: NonZeroU64::new(10).ok_or("issue")?,
            },
            subject: MarkerSubject::Issue(revision(0, None)),
        },
        MarkerFact::workflow(
            MarkerSchema::new("triage.other", NonZeroU32::MIN)?,
            &"payload",
        )?,
        &common::scheduled("triage-tick")?,
        common::at(1),
    )?;
    let markers = triage::IssueMarkers::new(&fixture.store, triage_workflow()?);
    assert_eq!(
        triage::plan_with_markers(&input, &markers),
        Err(WorkflowError::IncompleteEvidence)
    );
    fs::write(fixture.state_path(), b"{not json")?;
    assert_eq!(
        triage::plan_with_markers(&input, &markers),
        Err(WorkflowError::PrecheckFailed)
    );
    Ok(())
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

fn agent_labels() -> gardener::AgentLabels {
    gardener::AgentLabels {
        ready: "agent-ready".into(),
        working: "agent-working".into(),
    }
}
#[expect(clippy::unwrap_used, reason = "fixed fixture repository")]
fn project() -> Repository {
    Repository::new("sample/project").unwrap()
}
#[expect(clippy::unwrap_used, reason = "fixed house label fixture")]
fn hygiene_plan(issues: &[gardener::Issue]) -> Vec<gardener::Finding> {
    gardener::plan(&project(), issues, &agent_labels(), None).unwrap()
}
fn precheck_args() -> common::TestResult<gardener::PrecheckArgs> {
    Ok(gardener::PrecheckArgs {
        kitchen: "/opt/kitchen/bin/kitchen".into(),
        house: HouseId::new("sample")?,
        repository: project(),
        requester: ExternalRef::new("sample-bot")?,
        credential: CredentialId::new("read")?,
        credential_file: "/etc/kitchen/sample/read.token".into(),
        gh: "/usr/local/bin/gh".into(),
        store: "/var/lib/kitchen/sample".into(),
        labels: agent_labels(),
        window: gardener::PrecheckWindow::new(48, 30)?,
    })
}

#[test]
fn gardener_installs_a_disabled_daily_schedule_with_its_own_precheck() -> common::TestResult {
    use kitchen::{
        ConsumerId,
        contracts::{Effect, Permission, ScheduleEffect},
        scheduling::{AgentFamily, Recurrence, TimeOfDay, Timezone},
        selection::{AgentSelection, ResolvedSelection},
    };
    let consumer = ConsumerId::new("gardener-sample-project")?;
    let at = TimeOfDay::new(6, 30)?;
    let zone = Timezone::new("America/Toronto")?;
    let install = |args: &gardener::PrecheckArgs| {
        gardener::install(
            consumer.clone(),
            at,
            zone.clone(),
            ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Codex)),
            args,
        )
    };
    let Effect::Schedule(effect) = install(&precheck_args()?)? else {
        return Err("expected a schedule effect".into());
    };
    assert_eq!(effect.required_permission(), Permission::ManageSchedule);
    let ScheduleEffect::InstallDisabled { schedule } = effect else {
        return Err("expected a disabled install".into());
    };
    assert_eq!(schedule.workflow().as_str(), "gardener");
    assert_eq!(
        schedule.requires(),
        Some(&std::collections::BTreeSet::from(
            gardener::REQUIRED_CAPABILITIES
        )),
        "an installing backend must support the gardener's requirements"
    );
    assert_eq!(schedule.consumer(), &consumer);
    assert_eq!(schedule.recurrence(), &Recurrence::Daily(at));
    assert_eq!(schedule.timezone(), &zone);
    assert_eq!(
        schedule.agent(),
        &ResolvedSelection::owner(AgentSelection::agent_default(AgentFamily::Codex))
    );
    assert!(schedule.prompt().as_str().contains("sample/project"));
    let precheck = schedule.precheck().ok_or("missing precheck")?;
    let argv: Vec<&str> = precheck.argv().iter().map(Text::as_str).collect();
    assert_eq!(
        argv,
        [
            "/opt/kitchen/bin/kitchen",
            "gardener",
            "precheck",
            "--house",
            "sample",
            "--repository",
            "sample/project",
            "--requester",
            "sample-bot",
            "--credential",
            "read",
            "--credential-file",
            "/etc/kitchen/sample/read.token",
            "--gh",
            "/usr/local/bin/gh",
            "--store",
            "/var/lib/kitchen/sample",
            "--ready-label",
            "agent-ready",
            "--working-label",
            "agent-working",
            "--lookback-hours",
            "48",
            "--stale-days",
            "30",
        ]
    );
    assert_eq!(precheck.timeout().whole_seconds(), 120);

    // Relative programs and ambiguous labels never reach a schedule.
    for broken in [
        gardener::PrecheckArgs {
            kitchen: "kitchen".into(),
            ..precheck_args()?
        },
        gardener::PrecheckArgs {
            gh: "bin/gh".into(),
            ..precheck_args()?
        },
        gardener::PrecheckArgs {
            credential_file: "read.token".into(),
            ..precheck_args()?
        },
        gardener::PrecheckArgs {
            store: "house".into(),
            ..precheck_args()?
        },
        gardener::PrecheckArgs {
            labels: gardener::AgentLabels {
                ready: "agent-ready".into(),
                working: "agent-ready".into(),
            },
            ..precheck_args()?
        },
    ] {
        assert_eq!(install(&broken), Err(WorkflowError::IncompleteEvidence));
    }
    Ok(())
}

#[test]
fn gardener_precheck_window_is_bounded_and_ordered() -> common::TestResult {
    // One day of lookback, one stale day: the boundary where both cutoffs meet.
    let window = gardener::PrecheckWindow::new(24, 1)?;
    let now = Timestamp::from_unix_millis(10 * 86_400_000);
    assert_eq!(
        window.window(now),
        gardener::Window::new(
            Timestamp::from_unix_millis(9 * 86_400_000),
            Timestamp::from_unix_millis(9 * 86_400_000)
        )
    );
    // Near the epoch the change window starts at zero instead of wrapping.
    assert_eq!(
        window.window(Timestamp::from_unix_millis(1_000)),
        gardener::Window::new(
            Timestamp::from_unix_millis(0),
            Timestamp::from_unix_millis(0)
        )
    );
    assert!(gardener::PrecheckWindow::new(168, 365).is_ok());
    for (lookback, stale) in [(0, 30), (169, 30), (48, 0), (48, 366), (48, 1)] {
        assert_eq!(
            gardener::PrecheckWindow::new(lookback, stale),
            Err(WorkflowError::IncompleteEvidence),
            "{lookback}h/{stale}d"
        );
    }
    Ok(())
}

#[test]
fn precheck_results_decode_to_schedule_outcomes() {
    use kitchen::{scheduling::PrecheckOutcome, workflows::precheck_outcome};
    assert_eq!(
        precheck_outcome(Ok(Precheck::Actionable)),
        PrecheckOutcome::Actionable
    );
    assert_eq!(precheck_outcome(Ok(Precheck::Idle)), PrecheckOutcome::Idle);
    for error in [
        WorkflowError::PrecheckFailed,
        WorkflowError::IncompleteEvidence,
    ] {
        assert_eq!(precheck_outcome(Err(error)), PrecheckOutcome::Error);
    }
}

#[test]
fn gardener_has_independent_idle_error_and_actionable_precheck() {
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
        gardener::plan(&project(), &[hygiene_issue()], &labels, None),
        Err(WorkflowError::IncompleteEvidence)
    );
}

#[test]
fn gardener_rejects_unknown_issue_lifecycle() {
    let mut input = hygiene_issue();
    input.state = kitchen::integrations::github::IssueState::Unknown;
    let labels = agent_labels();
    assert_eq!(
        gardener::plan(&project(), &[input], &labels, None),
        Err(WorkflowError::IncompleteEvidence)
    );
    let mut uncertain = hygiene_issue();
    uncertain.claim = ClaimState::Unknown;
    assert_eq!(
        gardener::plan(&project(), &[uncertain], &labels, None),
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

fn forge_issue(number: u64, state: &str, updated: &str, labels: &[&str]) -> serde_json::Value {
    let labels: Vec<_> = labels
        .iter()
        .map(|name| serde_json::json!({"name": name, "color": "000000", "description": null}))
        .collect();
    serde_json::json!({"repository_url":"https://api.github.com/repos/sample/project","id":number,"number":number,"title":"issue","state":state,"assignees":[],"labels":labels,"updated_at":updated,"closed_at":null})
}

#[test]
fn gardener_precheck_reads_changed_and_open_inventory() -> common::TestResult {
    use serde_json::json;
    let house = HouseId::new("sample")?;
    // 2026-01-10 and 2026-01-01 in Unix milliseconds.
    let since = Timestamp::from_unix_millis(1_768_003_200_000);
    let stale_before = Timestamp::from_unix_millis(1_767_225_600_000);
    let window = gardener::Window::new(since, stale_before)?;
    let fresh = forge_issue(1, "open", "2026-01-05T00:00:00Z", &[]);
    let fixture = common::Fixture::new()?;
    let handled = gardener::StaleMarkers::new(&fixture.store)?;
    let signal = |pages: Vec<serde_json::Value>| -> common::TestResult<_> {
        let client = github_client(pages)?;
        let result = gardener::signal(
            &client,
            &house,
            &project(),
            &agent_labels(),
            window,
            Some(&handled),
        );
        Ok((result, client))
    };

    let (quiet, client) = signal(vec![json!([]), json!([fresh])])?;
    assert_eq!(gardener::precheck(quiet), Ok(Precheck::Idle));
    let endpoints = client.transport().endpoints.borrow().clone();
    assert!(endpoints[0].contains("state=all&since=2026-01-10T00:00:00Z"));
    assert!(endpoints[1].contains("state=open") && !endpoints[1].contains("since"));

    let closed = forge_issue(2, "closed", "2026-01-11T00:00:00Z", &["agent-working"]);
    let (residue, _) = signal(vec![json!([closed]), json!([fresh])])?;
    assert_eq!(
        residue,
        Ok(gardener::Signal {
            daily_changes: true,
            stale_issue: false,
            closed_agent_label: true,
        })
    );

    let old = forge_issue(3, "open", "2025-12-01T00:00:00Z", &[]);
    let (stale, _) = signal(vec![json!([]), json!([fresh, old])])?;
    assert_eq!(gardener::precheck(stale), Ok(Precheck::Actionable));

    let future = forge_issue(4, "locked", "2026-01-11T00:00:00Z", &[]);
    let (unknown, _) = signal(vec![json!([future]), json!([])])?;
    assert_eq!(unknown, Err(WorkflowError::IncompleteEvidence));

    let (unavailable, _) = signal(vec![json!([])])?;
    assert_eq!(
        gardener::precheck(unavailable),
        Err(WorkflowError::PrecheckFailed)
    );
    assert_eq!(
        gardener::Window::new(stale_before, since),
        Err(WorkflowError::IncompleteEvidence)
    );
    Ok(())
}

/// Markers are written only by `gardener::report_stale`; its recording paths
/// are covered by the `kitchn gardener report-stale` process tests and the
/// module's unit tests.
#[test]
fn a_foreign_fact_at_the_handled_key_fails_the_precheck() -> common::TestResult {
    use kitchen::state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject};
    use serde_json::json;
    let window = gardener::Window::new(
        Timestamp::from_unix_millis(1_768_003_200_000),
        Timestamp::from_unix_millis(1_767_225_600_000),
    )?;
    let old = forge_issue(3, "open", "2025-12-01T00:00:00Z", &[]);
    let fixture = common::Fixture::new()?;
    let markers = gardener::StaleMarkers::new(&fixture.store)?;
    assert_eq!(
        gardener_precheck(Some(&markers), window, json!([]), json!([old.clone()]))?,
        Ok(Precheck::Actionable)
    );
    assert!(!markers.handled(
        &project(),
        issue(3),
        Timestamp::from_unix_millis(1_764_547_200_000)
    )?);

    // A foreign fact at the handled key proves nothing: the precheck fails.
    fixture.store.record_marker(
        MarkerKey {
            workflow: WorkflowId::new(gardener::WORKFLOW)?,
            item: kitchen::state::WorkItem::Issue {
                repository: project(),
                number: std::num::NonZeroU64::new(3).ok_or("issue")?,
            },
            subject: MarkerSubject::Observation(ExternalRef::new("stale-handled")?),
        },
        MarkerFact::workflow(
            MarkerSchema::new("gardener.other", std::num::NonZeroU32::MIN)?,
            &"payload",
        )?,
        &common::scheduled("gardener-tick")?,
        common::at(1),
    )?;
    assert_eq!(
        gardener_precheck(Some(&markers), window, json!([]), json!([old.clone()]))?,
        Err(WorkflowError::IncompleteEvidence)
    );
    // An unhandled stale issue listed first does not hide it.
    let unhandled = forge_issue(2, "open", "2025-12-01T00:00:00Z", &[]);
    assert_eq!(
        gardener_precheck(Some(&markers), window, json!([]), json!([unhandled, old]))?,
        Err(WorkflowError::IncompleteEvidence)
    );
    Ok(())
}

/// Record a handled-stale marker as `report_stale` writes it: the revision
/// and the applied report that backs it (`bound`).
fn record_handled(
    fixture: &common::Fixture,
    repository: &Repository,
    number: u64,
    revision_millis: u64,
    bound: bool,
) -> common::TestResult {
    use kitchen::state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem};
    use serde_json::json;
    let mut payload = json!({"revision": revision_millis});
    if bound {
        payload["report"] = json!({
            "task": "gardener-stale-report",
            "receipt": "https://github.com/sample/project/issues/1#issuecomment-1",
        });
    }
    fixture.store.record_marker(
        MarkerKey {
            workflow: WorkflowId::new(gardener::WORKFLOW)?,
            item: WorkItem::Issue {
                repository: repository.clone(),
                number: std::num::NonZeroU64::new(number).ok_or("issue")?,
            },
            subject: MarkerSubject::Observation(ExternalRef::new("stale-handled")?),
        },
        MarkerFact::workflow(
            MarkerSchema::new("gardener.stale-handled", std::num::NonZeroU32::MIN)?,
            &payload,
        )?,
        &common::scheduled("gardener-tick")?,
        common::at(1),
    )?;
    Ok(())
}

/// 2025-12-02 and 2025-12-03 00:00:00Z in Unix milliseconds.
const STALE_A_HANDLED: u64 = 1_764_633_600_000;
const STALE_B_HANDLED: u64 = 1_764_720_000_000;

fn daily_window(day: u64) -> common::TestResult<gardener::Window> {
    // Each later day moves the change window and the stale cutoff forward.
    let shift = day * 86_400_000;
    Ok(gardener::Window::new(
        Timestamp::from_unix_millis(1_768_003_200_000 + shift),
        Timestamp::from_unix_millis(1_767_225_600_000 + shift),
    )?)
}

#[test]
fn every_stale_candidate_handled_at_its_current_revision_is_idle_each_day() -> common::TestResult {
    use serde_json::json;
    let fixture = common::Fixture::new()?;
    let markers = gardener::StaleMarkers::new(&fixture.store)?;
    let a = forge_issue(3, "open", "2025-12-02T00:00:00Z", &[]);
    let b = forge_issue(5, "open", "2025-12-03T00:00:00Z", &[]);
    let open = json!([a.clone(), b.clone()]);

    // Never handled: actionable.
    assert_eq!(
        gardener_precheck(Some(&markers), daily_window(0)?, json!([]), open.clone())?,
        Ok(Precheck::Actionable)
    );
    // One handled candidate does not idle the other.
    record_handled(&fixture, &project(), 3, STALE_A_HANDLED, true)?;
    assert_eq!(
        gardener_precheck(Some(&markers), daily_window(0)?, json!([]), open.clone())?,
        Ok(Precheck::Actionable)
    );
    // All handled: idle on every following day, including while the reports
    // still sit inside the change window.
    record_handled(&fixture, &project(), 5, STALE_B_HANDLED, true)?;
    for day in 0..4 {
        assert_eq!(
            gardener_precheck(
                Some(&markers),
                daily_window(day)?,
                json!([a.clone(), b.clone()]),
                open.clone()
            )?,
            Ok(Precheck::Idle),
            "day {day}"
        );
    }
    Ok(())
}

#[test]
fn a_new_revision_reopens_only_its_own_candidate() -> common::TestResult {
    use serde_json::json;
    let fixture = common::Fixture::new()?;
    let markers = gardener::StaleMarkers::new(&fixture.store)?;
    record_handled(&fixture, &project(), 3, STALE_A_HANDLED, true)?;
    record_handled(&fixture, &project(), 5, STALE_B_HANDLED, true)?;
    let a = forge_issue(3, "open", "2025-12-02T00:00:00Z", &[]);
    let b = forge_issue(5, "open", "2025-12-03T00:00:00Z", &[]);
    assert_eq!(
        gardener_precheck(Some(&markers), daily_window(0)?, json!([]), json!([a, b]))?,
        Ok(Precheck::Idle)
    );
    // Someone comments on issue 5 a second later: still stale, no longer the
    // handled revision.
    let touched = forge_issue(5, "open", "2025-12-03T00:00:01Z", &[]);
    let a = forge_issue(3, "open", "2025-12-02T00:00:00Z", &[]);
    assert_eq!(
        gardener_precheck(
            Some(&markers),
            daily_window(0)?,
            json!([]),
            json!([a.clone(), touched])
        )?,
        Ok(Precheck::Actionable)
    );
    // An older revision than the handled one is not handled either.
    let earlier = forge_issue(5, "open", "2025-12-02T23:59:59Z", &[]);
    assert_eq!(
        gardener_precheck(
            Some(&markers),
            daily_window(0)?,
            json!([]),
            json!([a, earlier])
        )?,
        Ok(Precheck::Actionable)
    );
    Ok(())
}

#[test]
fn a_marker_that_proves_no_report_or_names_another_repository_handles_nothing() -> common::TestResult
{
    use serde_json::json;
    let fixture = common::Fixture::new()?;
    let markers = gardener::StaleMarkers::new(&fixture.store)?;
    let stale = forge_issue(3, "open", "2025-12-02T00:00:00Z", &[]);
    // No applied report behind it.
    record_handled(&fixture, &project(), 3, STALE_A_HANDLED, false)?;
    assert_eq!(
        gardener_precheck(
            Some(&markers),
            daily_window(0)?,
            json!([]),
            json!([stale.clone()])
        )?,
        Ok(Precheck::Actionable)
    );
    // A report on the same issue number of another repository.
    let elsewhere = Repository::new("sample/other")?;
    record_handled(&fixture, &elsewhere, 3, STALE_A_HANDLED, true)?;
    assert_eq!(
        gardener_precheck(Some(&markers), daily_window(0)?, json!([]), json!([stale]))?,
        Ok(Precheck::Actionable)
    );
    Ok(())
}

#[test]
fn an_unreadable_marker_store_never_reads_as_a_handled_idle_day() -> common::TestResult {
    use serde_json::json;
    let fixture = common::Fixture::new()?;
    let markers = gardener::StaleMarkers::new(&fixture.store)?;
    record_handled(&fixture, &project(), 3, STALE_A_HANDLED, true)?;
    let stale = forge_issue(3, "open", "2025-12-02T00:00:00Z", &[]);
    assert_eq!(
        gardener_precheck(
            Some(&markers),
            daily_window(0)?,
            json!([]),
            json!([stale.clone()])
        )?,
        Ok(Precheck::Idle)
    );
    fs::write(fixture.state_path(), b"{not json")?;
    assert_eq!(
        gardener_precheck(Some(&markers), daily_window(0)?, json!([]), json!([stale]))?,
        Err(WorkflowError::PrecheckFailed)
    );
    Ok(())
}

/// The gardener precheck over `changed` and `open` inventory pages.
fn gardener_precheck(
    markers: Option<&gardener::StaleMarkers<'_>>,
    window: gardener::Window,
    changed: serde_json::Value,
    open: serde_json::Value,
) -> common::TestResult<Result<Precheck, WorkflowError>> {
    let client = github_client(vec![changed, open])?;
    Ok(gardener::precheck(gardener::signal(
        &client,
        &HouseId::new("sample")?,
        &project(),
        &agent_labels(),
        window,
        markers,
    )))
}

#[test]
fn gardener_closes_only_under_an_explicit_standing_grant() -> common::TestResult {
    use kitchen::{
        BackendId, CredentialId,
        contracts::{CloseReason, Grant, HouseGrants},
        integrations::github::IssueState,
    };
    let house = HouseId::new("sample")?;
    let backend = BackendId::new("github")?;
    let credential = CredentialId::new("gardener")?;
    let close = Grant::repository(
        Permission::CloseIssue,
        project(),
        backend.clone(),
        credential.clone(),
    );
    let label = Grant::repository(
        Permission::EditLabels,
        project(),
        backend.clone(),
        credential,
    );

    // Other permissions never imply closure; policy-only limits are not standing.
    let labels_only = HouseGrants::new(house.clone(), [label.clone()]);
    assert_eq!(
        gardener::CloseAuthority::from_grants(&labels_only, &project(), &backend)?,
        None
    );
    let consent_only =
        HouseGrants::with_limits(house.clone(), [close.clone(), label.clone()], [label])?;
    assert_eq!(
        gardener::CloseAuthority::from_grants(&consent_only, &project(), &backend)?,
        None
    );
    let granted = HouseGrants::new(house, [close]);
    let authority = gardener::CloseAuthority::from_grants(&granted, &project(), &backend)?
        .ok_or("standing close grant")?;

    let mut done = hygiene_issue();
    done.prose_blocker = None;
    done.parent_completed = true;
    let mut duplicate = hygiene_issue();
    duplicate.number = issue(21);
    duplicate.prose_blocker = None;
    duplicate.duplicate_of = Some(issue(15));
    let mut stale = hygiene_issue();
    stale.number = issue(22);
    stale.prose_blocker = None;
    stale.stale = true;
    let issues = [done.clone(), duplicate, stale];
    let findings = gardener::plan(&project(), &issues, &agent_labels(), Some(&authority))?;
    let closes = |reason: CloseReason, number: u64| {
        gardener::Finding::Mutation(GitHubAction::CloseIssue {
            repository: project(),
            number: issue(number),
            reason,
        })
    };
    assert_eq!(
        findings,
        vec![
            closes(CloseReason::Completed, 20),
            closes(CloseReason::Duplicate(issue(15)), 21),
            gardener::Finding::Review {
                issue: issue(22),
                reason: gardener::ReviewReason::Stale,
            },
        ]
    );
    // Without the grant the same inventory is review-only.
    assert!(
        hygiene_plan(&issues)
            .iter()
            .all(|finding| matches!(finding, gardener::Finding::Review { .. }))
    );
    // Claimed and human-only work is untouched even with the grant.
    let mut claimed = done.clone();
    claimed.claim = ClaimState::ClaimedByOther;
    let mut human = done;
    human.human_only = true;
    assert!(
        gardener::plan(
            &project(),
            &[claimed, human],
            &agent_labels(),
            Some(&authority)
        )?
        .is_empty()
    );
    // Authority for one repository cannot close in another.
    let other = Repository::new("sample/other")?;
    assert_eq!(
        gardener::plan(&other, &issues, &agent_labels(), Some(&authority)),
        Err(WorkflowError::DecisionMismatch)
    );
    let mut own_duplicate = hygiene_issue();
    own_duplicate.duplicate_of = Some(own_duplicate.number);
    assert_eq!(
        gardener::plan(
            &project(),
            &[own_duplicate],
            &agent_labels(),
            Some(&authority)
        ),
        Err(WorkflowError::IncompleteEvidence)
    );
    let mut closed = hygiene_issue();
    closed.state = IssueState::Closed;
    closed.parent_completed = true;
    assert!(
        gardener::plan(&project(), &[closed], &agent_labels(), Some(&authority))?.is_empty(),
        "a closed issue is never closed again"
    );
    Ok(())
}
