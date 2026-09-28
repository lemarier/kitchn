//! Offline triage and gardener behavior fixtures.

use kitchen::{
    contracts::{DecisionOwner, GitHubAction, IssueNumber},
    workflows::{Precheck, WorkflowError, gardener, triage},
};

#[expect(clippy::unwrap_used, reason = "fixed fixture issue numbers")]
fn issue(n: u64) -> IssueNumber {
    IssueNumber::new(n).unwrap()
}
fn triage_input() -> triage::Evidence {
    triage::Evidence {
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
        claimed_by_other: false,
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
        owner: DecisionOwner::Spec,
        revision: "r1".into(),
        state: triage::DecisionState::Expired,
    });
    assert_eq!(triage::plan(&input).unwrap().len(), 1); // comment only
    input.existing_decisions[0].revision = "old".into();
    assert_eq!(triage::plan(&input), Err(WorkflowError::DecisionMismatch));
    input.existing_decisions.clear();
    input.claimed_by_other = true;
    assert!(triage::plan(&input).unwrap().is_empty());
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

struct FakeMarkers {
    posted: Result<bool, WorkflowError>,
    decisions: Vec<triage::Decision>,
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
        Ok(self.decisions.clone())
    }
}

#[test]
fn marker_read_controls_repetition_and_propagates_failure() {
    let mut input = triage_input();
    input.resolution_already_posted = false;
    let markers = FakeMarkers {
        posted: Ok(true),
        decisions: vec![],
    };
    assert!(
        matches!(triage::plan_with_markers(&input, &markers), Ok(changes) if changes.len() == 1)
    );
    let failed = FakeMarkers {
        posted: Err(WorkflowError::PrecheckFailed),
        decisions: vec![],
    };
    assert_eq!(
        triage::plan_with_markers(&input, &failed),
        Err(WorkflowError::PrecheckFailed)
    );
}
fn hygiene_issue() -> gardener::Issue {
    gardener::Issue {
        number: issue(20),
        closed: false,
        human_only: false,
        claimed_by_other: false,
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
    input.claimed_by_other = true;
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
fn gardener_reports_closed_residue_and_review_only_work() {
    let mut closed = hygiene_issue();
    closed.closed = true;
    closed.labels = vec!["agent-working".into(), "user-label".into()];
    assert_eq!(hygiene_plan(&[closed.clone()]).len(), 1);
    closed.claimed_by_other = true;
    assert!(hygiene_plan(&[closed]).is_empty());
    let mut open = hygiene_issue();
    open.prose_blocker = None;
    open.merged_work = true;
    open.duplicate_of = Some(issue(15));
    open.stale = true;
    assert_eq!(hygiene_plan(&[open]).len(), 3);
}
