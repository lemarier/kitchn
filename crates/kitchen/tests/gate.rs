//! Exact-revision gate policy scenarios.
mod common;
use common::{TestResult, commit};
use kitchen::workflows::gate::{self, *};
use kitchen::{
    HouseId,
    contracts::{ExternalRef, IssueNumber, Repository, Text},
};

fn ready() -> TestResult<GateEvidence> {
    let head = commit('a')?;
    Ok(GateEvidence {
        house: HouseId::new("kitchen")?,
        repository: Repository::new("lemarier/kitchen")?,
        number: IssueNumber::new(9)?,
        head: head.clone(),
        head_branch: Some("feature/gate".into()),
        base: commit('b')?,
        head_age_secs: Some(3600),
        open: Some(true),
        draft: Some(false),
        same_repository: Some(true),
        targets_default: Some(true),
        author_allowed: Some(true),
        merge_clean: Some(true),
        merge_behind: Some(false),
        protection_satisfied: Some(true),
        contains_base: Some(true),
        checks: Checks::Passed,
        reviewers: vec![ExpectedReviewer {
            name: "reviewer".into(),
            reviewed_head: Some(head.clone()),
            outcome: ReviewerOutcome::Clean,
        }],
        threads_resolved: Some(true),
        no_change_request: Some(true),
        semantic_review: SemanticReview::Clean,
        semantic_source: Some(ExternalRef::new("https://example.invalid/review/9")?),
        verified_findings: Vec::new(),
        disproved_findings: Vec::new(),
        semantic_head: Some(head.clone()),
        semantic_base: Some(commit('b')?),
        semantic_read_only: true,
        semantic_independent: true,
        acceptance_met: Some(true),
        hardware_complete: Some(true),
        risk_classes: Some(Vec::new()),
        risk_approval: None,
        writer_working: false,
        supporting_subject: Some((head.clone(), commit('b')?)),
        reopen_event: None,
    })
}
fn grants() -> GateGrants {
    GateGrants {
        merge: true,
        fix_request: true,
        fix_delivery_capable: true,
        reviewer_invocation: true,
        review_triggers: Vec::new(),
    }
}
#[test]
fn all_rules_merge_only_with_grant_and_exact_refs() -> TestResult {
    let e = ready()?;
    let d = gate::evaluate(&e, grants(), GateHistory::default());
    assert_eq!(d.verdict, Verdict::Merge);
    assert!(gate::still_current(&d, &e.head, &e.base));
    assert!(!gate::still_current(&d, &commit('c')?, &e.base));
    assert!(!gate::still_current(&d, &e.head, &commit('c')?));
    assert!(
        matches!(gate::evaluate(&e,GateGrants::default(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::MergeGrant))
    );
    Ok(())
}
#[test]
fn settled_behind_head_requests_bounded_fix() -> TestResult {
    let mut e = ready()?;
    e.contains_base = Some(false);
    e.merge_clean = Some(false);
    e.merge_behind = Some(true);
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::FixRequest{gaps} if gaps==vec![Gap::BaseBehind])
    );
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory{fix_rounds:2,..GateHistory::default()}).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::FixBudget))
    );
    Ok(())
}
#[test]
fn stale_reviews_and_approvals_do_not_pass() -> TestResult {
    let mut e = ready()?;
    e.reviewers[0].reviewed_head = Some(commit('c')?);
    assert!(matches!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::HandOver { gaps } if gaps.contains(&Gap::ReviewerStale)
    ));
    e.head_age_secs = Some(86400);
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::ReviewerStale))
    );
    e.reviewers[0].reviewed_head = Some(e.head.clone());
    e.risk_classes = Some(vec![RiskClass::AuthorizationSecrets]);
    e.risk_approval = Some(RiskApproval {
        approver: "human".into(),
        source: ExternalRef::new("https://example.invalid/approval/9")?,
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: commit('c')?,
        base: e.base.clone(),
        write_access: true,
        approval_verified: true,
    });
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::RiskApproval))
    );
    Ok(())
}
#[test]
fn quota_and_partial_semantic_review_are_gaps() -> TestResult {
    let mut e = ready()?;
    e.reviewers[0].outcome = ReviewerOutcome::Unavailable;
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::ReviewerUnavailable))
    );
    e.reviewers[0].outcome = ReviewerOutcome::Clean;
    e.semantic_review = SemanticReview::Partial;
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::SemanticCoverage))
    );
    Ok(())
}
#[test]
fn report_marker_suppresses_repeat_and_fix_is_once_per_head() -> TestResult {
    let mut e = ready()?;
    e.acceptance_met = Some(false);
    assert!(matches!(
        gate::evaluate(
            &e,
            grants(),
            GateHistory {
                reported_subject: Some((e.head.clone(), e.base.clone())),
                ..GateHistory::default()
            }
        )
        .verdict,
        Verdict::Skip
    ));
    e.acceptance_met = Some(true);
    e.checks = Checks::Failed;
    assert!(matches!(
        gate::evaluate(
            &e,
            grants(),
            GateHistory {
                requested_this_head: true,
                request_age_secs: Some(100),
                ..GateHistory::default()
            }
        )
        .verdict,
        Verdict::Skip
    ));
    assert!(matches!(
        gate::evaluate(
            &e,
            grants(),
            GateHistory {
                requested_this_head: true,
                request_age_secs: Some(7200),
                ..GateHistory::default()
            }
        )
        .verdict,
        Verdict::HandOver { .. }
    ));
    Ok(())
}
#[test]
fn risk_approval_cannot_override_other_rules() -> TestResult {
    let mut e = ready()?;
    e.risk_classes = Some(vec![RiskClass::AuthorizationSecrets]);
    e.risk_approval = Some(RiskApproval {
        approver: "human".into(),
        source: ExternalRef::new("https://example.invalid/approval/9")?,
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: e.head.clone(),
        base: e.base.clone(),
        write_access: true,
        approval_verified: true,
    });
    e.hardware_complete = Some(false);
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::Hardware))
    );
    Ok(())
}
#[test]
fn merge_request_rechecks_refs_and_run_limit() -> TestResult {
    let e = ready()?;
    let d = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    assert_eq!(
        gate::merge_request(&d, &e.head, &e.base, 0)?.match_head,
        e.head
    );
    assert_eq!(
        gate::merge_request(&d, &e.head, &commit('c')?, 0),
        Err(RequestRefusal::MovedRevision)
    );
    assert_eq!(
        gate::merge_request(&d, &e.head, &e.base, 3),
        Err(RequestRefusal::MergeLimit)
    );
    let mutation = gate::merge_request(&d, &e.head, &e.base, 0)?.mutation();
    assert_eq!(mutation.repository, e.repository);
    assert!(
        matches!(mutation.action,kitchen::contracts::GitHubAction::MergePullRequest{number,expected_head,method:kitchen::contracts::MergeMethod::Squash} if number==e.number && expected_head==e.head)
    );
    Ok(())
}
#[test]
fn repair_request_only_from_fix_verdict() -> TestResult {
    let mut e = ready()?;
    e.contains_base = Some(false);
    let d = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    assert_eq!(
        gate::fix_request(&d, &grants()).map(|r| r.gaps)?,
        vec![Gap::BaseBehind]
    );
    e.contains_base = Some(true);
    let d = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    assert_eq!(
        gate::fix_request(&d, &grants()),
        Err(RequestRefusal::WrongVerdict)
    );
    Ok(())
}
#[test]
fn semantic_review_must_be_independent_complete_and_exact() -> TestResult {
    let mut e = ready()?;
    e.semantic_base = Some(commit('c')?);
    assert!(
        matches!(gate::evaluate(&e, grants(), GateHistory::default()).verdict, Verdict::HandOver { gaps } if gaps.contains(&Gap::SemanticCoverage))
    );
    e.semantic_base = Some(e.base.clone());
    e.semantic_independent = false;
    assert!(
        matches!(gate::evaluate(&e, grants(), GateHistory::default()).verdict, Verdict::HandOver { gaps } if gaps.contains(&Gap::SemanticCoverage))
    );
    e.semantic_independent = true;
    e.semantic_read_only = false;
    assert!(
        matches!(gate::evaluate(&e, grants(), GateHistory::default()).verdict, Verdict::HandOver { gaps } if gaps.contains(&Gap::SemanticCoverage))
    );
    Ok(())
}
#[test]
fn unknown_risk_and_unverified_permission_block_merge() -> TestResult {
    let mut e = ready()?;
    e.risk_classes = None;
    assert!(
        matches!(gate::evaluate(&e, grants(), GateHistory::default()).verdict, Verdict::HandOver { gaps } if gaps.contains(&Gap::RiskApproval))
    );
    e.risk_classes = Some(vec![RiskClass::AuthorizationSecrets]);
    e.risk_approval = Some(RiskApproval {
        approver: "human".into(),
        source: ExternalRef::new("https://example.invalid/approval/9")?,
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: e.head.clone(),
        base: e.base.clone(),
        write_access: false,
        approval_verified: true,
    });
    assert!(
        matches!(gate::evaluate(&e, grants(), GateHistory::default()).verdict, Verdict::HandOver { gaps } if gaps.contains(&Gap::RiskApproval))
    );
    Ok(())
}

#[derive(Default)]
struct FakeMarkers {
    records: Vec<GateVerdictRecord>,
    fail: bool,
}
impl GateMarkerStore for FakeMarkers {
    type Error = std::io::Error;
    fn history(
        &self,
        house: &HouseId,
        repository: &Repository,
        number: IssueNumber,
        head: &kitchen::contracts::CommitId,
        base: &kitchen::contracts::CommitId,
        now_unix_secs: u64,
    ) -> Result<GateHistory, Self::Error> {
        let matching: Vec<_> = self
            .records
            .iter()
            .filter(|r| &r.house == house && &r.repository == repository && r.number == number)
            .collect();
        let request = matching
            .iter()
            .filter(|r| {
                &r.head == head
                    && &r.base == base
                    && matches!(r.verdict, Verdict::FixRequest { .. })
            })
            .max_by_key(|r| r.recorded_unix_secs);
        let reported = matching.iter().any(|r| {
            &r.head == head
                && &r.base == base
                && (r.mode == GateMode::ReportOnly
                    || matches!(r.verdict, Verdict::HandOver { .. } | Verdict::Merge))
        });
        Ok(GateHistory {
            fix_rounds: matching
                .iter()
                .filter(|r| matches!(r.verdict, Verdict::FixRequest { .. }))
                .count()
                .try_into()
                .unwrap_or(u8::MAX),
            requested_this_head: request.is_some(),
            request_age_secs: request.and_then(|r| now_unix_secs.checked_sub(r.recorded_unix_secs)),
            last_handover_unix_secs: matching
                .iter()
                .filter(|r| {
                    &r.head == head
                        && &r.base == base
                        && matches!(r.verdict, Verdict::HandOver { .. })
                })
                .map(|r| r.recorded_unix_secs)
                .max(),
            handovers: matching
                .iter()
                .filter(|r| matches!(r.verdict, Verdict::HandOver { .. }))
                .count()
                .try_into()
                .unwrap_or(u8::MAX),
            reported_subject: reported.then(|| (head.clone(), base.clone())),
            ..GateHistory::default()
        })
    }
    fn record_if_absent(&mut self, record: GateVerdictRecord) -> Result<bool, Self::Error> {
        if self.fail {
            return Err(std::io::Error::other("fake persistence failure"));
        }
        if self.records.iter().any(|r| {
            r.house == record.house
                && r.repository == record.repository
                && r.number == record.number
                && r.head == record.head
                && r.base == record.base
                && r.round == record.round
                && std::mem::discriminant(&r.verdict) == std::mem::discriminant(&record.verdict)
        }) {
            return Ok(false);
        }
        self.records.push(record);
        Ok(true)
    }
}
#[test]
fn report_only_records_once_per_exact_subject_without_effect() -> TestResult {
    let mut store = FakeMarkers::default();
    let mut e = ready()?;
    let first = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly, 100)?;
    assert_eq!(first.decision.verdict, Verdict::Merge);
    assert_eq!(first.mode, GateMode::ReportOnly);
    assert_eq!(
        gate::merge_request(&first, &e.head, &e.base, 0),
        Err(RequestRefusal::EffectsDisabled)
    );
    let second = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly, 100)?;
    assert_eq!(second.decision.verdict, Verdict::Skip);
    assert_eq!(store.records.len(), 1);
    e.repository = Repository::new("other/repository")?;
    let other = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly, 100)?;
    assert!(other.new_record);
    assert_eq!(store.records.len(), 2);
    e.house = HouseId::new("other-house")?;
    let cross = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly, 100)?;
    assert!(cross.new_record);
    assert_eq!(store.records.len(), 3);
    e.base = commit('c')?;
    let moved = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly, 100)?;
    assert!(moved.new_record);
    assert_eq!(store.records.len(), 4);
    Ok(())
}
#[test]
fn uncertain_marker_write_stops_decision() -> TestResult {
    let mut store = FakeMarkers {
        fail: true,
        ..FakeMarkers::default()
    };
    let e = ready()?;
    assert!(gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 100).is_err());
    assert!(store.records.is_empty());
    Ok(())
}
#[test]
fn moving_and_conflicting_heads_skip_until_stalled() -> TestResult {
    let mut e = ready()?;
    e.head_age_secs = Some(1799);
    assert_eq!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::Skip
    );
    e.head_age_secs = Some(3600);
    e.merge_clean = Some(false);
    assert_eq!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::Skip
    );
    e.head_age_secs = Some(86400);
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::Mergeability))
    );
    Ok(())
}
#[test]
fn unknown_rule_one_and_missing_checks_never_merge() -> TestResult {
    let mut e = ready()?;
    e.same_repository = None;
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::Eligibility))
    );
    e.same_repository = Some(true);
    e.checks = Checks::Missing;
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::FixRequest{gaps} if gaps.contains(&Gap::Checks))
    );
    Ok(())
}
#[test]
fn reviewer_request_needs_its_separate_grant() -> TestResult {
    let mut e = ready()?;
    e.reviewers[0].reviewed_head = Some(commit('c')?);
    e.head_age_secs = Some(86400);
    let grants = GateGrants {
        reviewer_invocation: false,
        ..grants()
    };
    assert!(
        matches!(gate::evaluate(&e,grants,GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::ReviewerStale))
    );
    Ok(())
}
#[test]
fn reviewer_trigger_is_exactly_granted_and_head_scoped() -> TestResult {
    let mut e = ready()?;
    e.reviewers[0].reviewed_head = Some(commit('c')?);
    let trigger = ReviewTrigger {
        house: e.house.clone(),
        reviewer: "reviewer".into(),
        command: Text::new("@reviewer review")?,
        repository: e.repository.clone(),
        head: e.head.clone(),
    };
    let mut granted = grants();
    granted.review_triggers.push(trigger.clone());
    let mut history = GateHistory::default();
    e.head_age_secs = Some(3600);
    let recorded = RecordedDecision {
        decision: gate::evaluate(&e, granted.clone(), history.clone()),
        mode: GateMode::Active,
        new_record: true,
    };
    assert_eq!(
        gate::fix_request(&recorded, &granted)?.review_triggers,
        vec![trigger.clone()]
    );
    e.reviewers[0].reviewed_head = Some(e.head.clone());
    e.reviewers[0].outcome = ReviewerOutcome::Findings;
    let findings = RecordedDecision {
        decision: gate::evaluate(&e, granted.clone(), history.clone()),
        mode: GateMode::Active,
        new_record: true,
    };
    assert!(
        gate::fix_request(&findings, &granted)?
            .review_triggers
            .is_empty()
    );
    let mut wrong = granted;
    wrong.review_triggers[0].head = commit('d')?;
    assert_eq!(
        gate::fix_request(&recorded, &wrong),
        Err(RequestRefusal::MovedRevision)
    );
    history.reported_subject = Some((e.head.clone(), e.base.clone()));
    assert_eq!(gate::evaluate(&e, grants(), history).verdict, Verdict::Skip);
    Ok(())
}
#[test]
fn fix_request_carries_verified_and_disproved_finding_evidence() -> TestResult {
    let mut e = ready()?;
    e.semantic_review = SemanticReview::Findings;
    e.verified_findings.push(VerifiedFinding {
        source: ExternalRef::new("https://github.com/lemarier/kitchen/pull/23#discussion_r1")?,
        reason: Text::new("unbounded retry after ambiguous merge")?,
        priority: FindingPriority::ActOn,
    });
    e.disproved_findings.push(DisprovedFinding {
        source: ExternalRef::new("https://github.com/lemarier/kitchen/pull/23#discussion_r2")?,
        evidence: Text::new("caller bounds attempts to two")?,
    });
    let recorded = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    let request = gate::fix_request(&recorded, &grants())?;
    assert_eq!(request.verified_findings, e.verified_findings);
    assert_eq!(request.disproved_findings, e.disproved_findings);
    assert!(request.review_triggers.is_empty());
    e.verified_findings.clear();
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::SemanticCoverage))
    );
    Ok(())
}

struct ForgeFake {
    responses: std::cell::RefCell<std::collections::VecDeque<serde_json::Value>>,
}
impl kitchen::integrations::github::GitHubReadTransport for ForgeFake {
    fn read(
        &self,
        _: &kitchen::integrations::github::CredentialRef,
        _: &kitchen::integrations::github::ReadRequest,
        _: std::time::Duration,
        _: usize,
    ) -> Result<Vec<u8>, kitchen::integrations::github::IntegrationError> {
        let next = self
            .responses
            .borrow_mut()
            .pop_front()
            .ok_or(kitchen::integrations::github::IntegrationError::Unavailable)?;
        serde_json::to_vec(&next)
            .map_err(|_| kitchen::integrations::github::IntegrationError::Unknown)
    }
}
fn forge_client(
    head: &str,
    missing_checks: bool,
) -> TestResult<kitchen::integrations::github::GitHubClient<ForgeFake>> {
    use kitchen::integrations::github::{
        CredentialRef, GitHubClient, HouseScope, PostingBudget, ReadLimits,
    };
    use serde_json::json;
    let house = HouseId::new("kitchen")?;
    let repo = Repository::new("lemarier/kitchen")?;
    let requester = ExternalRef::new("gate-reader")?;
    let scope = HouseScope::new(
        house.clone(),
        [repo.clone()],
        requester.clone(),
        CredentialRef::new(house, kitchen::CredentialId::new("read")?, requester),
        PostingBudget::new(0)?,
        [],
    )?;
    let responses = vec![
        json!({"number":9,"state":"open","draft":false,"merged":false,"head":{"sha":head,"ref":"topic","repo":{"full_name":"lemarier/kitchen"}},"base":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","ref":"main"},"mergeable":true,"user":{"login":"allowed"}}),
        json!({"default_branch":"main"}),
        json!({"data":{"repository":{"pullRequest":{"headRefOid":head,"mergeStateStatus":"CLEAN"}}}}),
        json!({"behind_by":0,"ahead_by":1}),
        json!({"check_runs":[{"name":"build","head_sha":head,"status":"completed","conclusion":"success"}]}),
        json!([]),
        json!({"contexts":[if missing_checks {"missing"} else {"build"}],"checks":[]}),
        json!([{"id":11,"user":{"login":"reviewer"},"commit_id":head,"state":"APPROVED","submitted_at":"2026-09-28T14:05:00Z"}]),
        json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}),
        json!({"sha":head,"commit":{"committer":{"date":"2026-09-28T14:00:00Z"}}}),
        json!([]),
    ];
    Ok(GitHubClient::new(
        scope,
        ForgeFake {
            responses: std::cell::RefCell::new(responses.into()),
        },
        ReadLimits::default(),
    ))
}
fn supplement(e: &GateEvidence) -> GateSupplement {
    GateSupplement {
        semantic_review: e.semantic_review.clone(),
        semantic_source: e.semantic_source.clone(),
        verified_findings: e.verified_findings.clone(),
        disproved_findings: e.disproved_findings.clone(),
        semantic_head: e.semantic_head.clone(),
        semantic_base: e.semantic_base.clone(),
        semantic_read_only: e.semantic_read_only,
        semantic_independent: e.semantic_independent,
        acceptance_met: e.acceptance_met,
        hardware_complete: e.hardware_complete,
        risk_classes: e.risk_classes.clone(),
        risk_approval: e.risk_approval.clone(),
        writer_working: e.writer_working,
        subject: e.supporting_subject.clone(),
    }
}
#[test]
fn scoped_forge_reads_feed_exact_head_gate() -> TestResult {
    let e = ready()?;
    let client = forge_client(e.head.as_str(), false)?;
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert_eq!(observed.head_age_secs, Some(3600));
    assert_eq!(observed.checks, Checks::Passed);
    assert_eq!(
        gate::evaluate(&observed, grants(), GateHistory::default()).verdict,
        Verdict::Merge
    );
    Ok(())
}
#[test]
fn forge_behind_state_requests_a_bounded_fix() -> TestResult {
    use serde_json::json;
    let e = ready()?;
    let client = forge_client(e.head.as_str(), false)?;
    {
        let mut responses = client.transport().responses.borrow_mut();
        responses[2] = json!({"data":{"repository":{"pullRequest":{"headRefOid":e.head.as_str(),"mergeStateStatus":"BEHIND"}}}});
        responses[3] = json!({"behind_by":1,"ahead_by":1});
    }
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert!(matches!(
        gate::evaluate(&observed, grants(), GateHistory::default()).verdict,
        Verdict::FixRequest { gaps } if gaps.contains(&Gap::BaseBehind) && !gaps.contains(&Gap::Mergeability)
    ));
    Ok(())
}
#[test]
fn required_check_absence_from_forge_refuses_merge() -> TestResult {
    let e = ready()?;
    let client = forge_client(e.head.as_str(), true)?;
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert_eq!(observed.checks, Checks::Missing);
    assert!(
        matches!(gate::evaluate(&observed,grants(),GateHistory::default()).verdict,Verdict::FixRequest{gaps} if gaps.contains(&Gap::Checks))
    );
    Ok(())
}
#[test]
fn fix_request_record_times_out_to_one_handover() -> TestResult {
    let mut store = FakeMarkers::default();
    let mut e = ready()?;
    e.checks = Checks::Failed;
    let first = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 100)?;
    assert!(matches!(first.decision.verdict, Verdict::FixRequest { .. }));
    let waiting = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 7299)?;
    assert_eq!(waiting.decision.verdict, Verdict::Skip);
    let handover = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 7300)?;
    assert!(matches!(
        handover.decision.verdict,
        Verdict::HandOver { .. }
    ));
    let repeat = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 7301)?;
    assert_eq!(repeat.decision.verdict, Verdict::Skip);
    assert_eq!(store.records.len(), 2);
    Ok(())
}
#[test]
fn forge_reread_blocks_a_moved_head_before_merge_effect() -> TestResult {
    let e = ready()?;
    let recorded = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    let current = forge_client(e.head.as_str(), false)?;
    assert_eq!(
        gate::merge_request_from_forge(&recorded, &current, 0)?.match_head,
        e.head
    );
    let moved_head = commit('c')?;
    let moved = forge_client(moved_head.as_str(), false)?;
    assert_eq!(
        gate::merge_request_from_forge(&recorded, &moved, 0),
        Err(kitchen::integrations::github::IntegrationError::StaleDecision)
    );
    Ok(())
}
#[test]
fn supporting_evidence_moves_with_head_and_base() -> TestResult {
    let mut e = ready()?;
    e.supporting_subject = Some((commit('c')?, e.base.clone()));
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::SupportingSubject))
    );
    e.supporting_subject = Some((e.head.clone(), commit('c')?));
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::SupportingSubject))
    );
    Ok(())
}
#[test]
fn handover_effects_are_typed_and_report_only_has_none() -> TestResult {
    let mut e = ready()?;
    e.hardware_complete = Some(false);
    let decision = gate::evaluate(&e, grants(), GateHistory::default());
    let trial = RecordedDecision {
        decision: decision.clone(),
        mode: GateMode::ReportOnly,
        new_record: true,
    };
    assert_eq!(
        gate::handover_request(&trial),
        Err(RequestRefusal::EffectsDisabled)
    );
    let active = RecordedDecision {
        decision,
        mode: GateMode::Active,
        new_record: true,
    };
    let handover = gate::handover_request(&active)?;
    let effects = handover.mutations()?;
    assert_eq!(effects.len(), 2);
    assert!(
        matches!(&effects[0].action,kitchen::contracts::GitHubAction::SetLabel{label,present:true,..} if label=="needs-human-review")
    );
    assert!(
        matches!(&effects[1].action,kitchen::contracts::GitHubAction::PostComment{body,..} if body.as_str().contains(e.head.as_str()) && body.as_str().contains("Hardware"))
    );
    Ok(())
}
#[test]
fn comment_after_change_request_does_not_clear_it() -> TestResult {
    let e = ready()?;
    let client = forge_client(e.head.as_str(), false)?;
    let mut responses = client.transport().responses.borrow_mut();
    if let Some(reviews) = responses.get_mut(7) {
        *reviews = serde_json::json!([
            {"id":10,"user":{"login":"reviewer"},"commit_id":e.head.as_str(),"state":"CHANGES_REQUESTED","submitted_at":"2026-09-28T14:05:00Z"},
            {"id":11,"user":{"login":"reviewer"},"commit_id":e.head.as_str(),"state":"COMMENTED","submitted_at":"2026-09-28T14:06:00Z"}
        ]);
    }
    drop(responses);
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert_eq!(observed.no_change_request, Some(false));
    assert!(
        matches!(gate::evaluate(&observed,grants(),GateHistory::default()).verdict,Verdict::FixRequest{gaps} if gaps.contains(&Gap::ChangeRequest))
    );
    Ok(())
}
#[test]
fn run_caps_evaluations_and_confirmed_merges() -> TestResult {
    let mut store = FakeMarkers::default();
    let e = ready()?;
    let mut run = GateRun::new();
    for i in 0..3 {
        let result = run.evaluate_next(&mut store, &e, grants(), GateMode::ReportOnly, i)?;
        assert!(result.is_some());
    }
    assert_eq!(run.evaluated(), 3);
    assert!(
        run.evaluate_next(&mut store, &e, grants(), GateMode::ReportOnly, 4)?
            .is_none()
    );
    let recorded = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    let request = gate::merge_request(&recorded, &e.head, &e.base, 0)?;
    for number in 9..=11 {
        let mut distinct = request.clone();
        distinct.number = IssueNumber::new(number)?;
        let client = forge_client(e.head.as_str(), false)?;
        if let Some(pr) = client.transport().responses.borrow_mut().get_mut(0) {
            pr["number"] = serde_json::json!(number);
            pr["state"] = serde_json::json!("closed");
            pr["merged"] = serde_json::json!(true);
            pr["merge_commit_sha"] = serde_json::json!("cccccccccccccccccccccccccccccccccccccccc");
        }
        run.confirm_merge(&distinct, &client)?;
        if number == 9 {
            assert_eq!(
                run.confirm_merge(&distinct, &client),
                Err(kitchen::integrations::github::IntegrationError::StaleDecision)
            );
        }
    }
    let client = forge_client(e.head.as_str(), false)?;
    assert_eq!(
        run.confirm_merge(&request, &client),
        Err(kitchen::integrations::github::IntegrationError::StaleDecision)
    );
    Ok(())
}
#[test]
fn quota_review_from_forge_is_unavailable() -> TestResult {
    let e = ready()?;
    let client = forge_client(e.head.as_str(), false)?;
    let mut responses = client.transport().responses.borrow_mut();
    if let Some(reviews) = responses.get_mut(7) {
        *reviews = serde_json::json!([{"id":11,"user":{"login":"reviewer"},"commit_id":e.head.as_str(),"state":"COMMENTED","body":"Copilot has reached their quota limit and review was skipped","submitted_at":"2026-09-28T14:05:00Z"}]);
    }
    drop(responses);
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert_eq!(observed.reviewers[0].outcome, ReviewerOutcome::Unavailable);
    assert!(
        matches!(gate::evaluate(&observed,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::ReviewerUnavailable))
    );
    Ok(())
}
#[test]
fn merge_readback_requires_closed_merged_and_commit() -> TestResult {
    let e = ready()?;
    let recorded = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    let request = gate::merge_request(&recorded, &e.head, &e.base, 0)?;
    let mut run = GateRun::new();
    let open = forge_client(e.head.as_str(), false)?;
    assert_eq!(
        run.confirm_merge(&request, &open),
        Err(kitchen::integrations::github::IntegrationError::Unknown)
    );
    let no_commit = forge_client(e.head.as_str(), false)?;
    if let Some(pr) = no_commit.transport().responses.borrow_mut().get_mut(0) {
        pr["state"] = serde_json::json!("closed");
        pr["merged"] = serde_json::json!(true);
    }
    assert_eq!(
        run.confirm_merge(&request, &no_commit),
        Err(kitchen::integrations::github::IntegrationError::Unknown)
    );
    Ok(())
}
#[test]
fn semantic_request_is_pinned_and_read_only() -> TestResult {
    let e = ready()?;
    let request = e.semantic_request();
    assert_eq!(request.house, e.house);
    assert_eq!(request.repository, e.repository);
    assert_eq!(request.head, e.head);
    assert_eq!(request.base, e.base);
    assert_eq!(request.mode, CommittedDiffReview::ReadOnly);
    Ok(())
}
#[test]
fn invalid_forge_branch_cannot_target_fix_worker() -> TestResult {
    let e = ready()?;
    let client = forge_client(e.head.as_str(), false)?;
    if let Some(pr) = client.transport().responses.borrow_mut().get_mut(0) {
        pr["head"]["ref"] = serde_json::json!("../unsafe");
    }
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert!(observed.head_branch.is_none());
    assert!(
        matches!(gate::evaluate(&observed,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::BranchTarget))
    );
    Ok(())
}
struct WorkerFake {
    supported: bool,
    delivered: Vec<FixRequest>,
    fail: bool,
}
impl GateWorkerBackend for WorkerFake {
    type Error = std::io::Error;
    fn supports(&self, _: &FixRequest) -> bool {
        self.supported
    }
    fn deliver(&mut self, request: FixRequest) -> Result<ExternalRef, Self::Error> {
        if self.fail {
            return Err(std::io::Error::other("uncertain delivery"));
        }
        self.delivered.push(request);
        ExternalRef::new("worker-receipt").map_err(|_| std::io::Error::other("invalid receipt"))
    }
}
#[test]
fn fake_worker_receives_one_narrow_fix_request() -> TestResult {
    let mut e = ready()?;
    e.contains_base = Some(false);
    let active = RecordedDecision {
        decision: gate::evaluate(&e, grants(), GateHistory::default()),
        mode: GateMode::Active,
        new_record: true,
    };
    let mut worker = WorkerFake {
        supported: true,
        delivered: Vec::new(),
        fail: false,
    };
    assert_eq!(
        gate::dispatch_fix(&active, &grants(), &mut worker)
            .map_err(|_| "dispatch failed")?
            .as_str(),
        "worker-receipt"
    );
    assert_eq!(worker.delivered.len(), 1);
    assert_eq!(worker.delivered[0].head_branch, "feature/gate");
    assert_eq!(worker.delivered[0].gaps, vec![Gap::BaseBehind]);
    assert!(worker.delivered[0].review_triggers.is_empty());
    let trial = RecordedDecision {
        mode: GateMode::ReportOnly,
        ..active.clone()
    };
    assert!(matches!(
        gate::dispatch_fix(&trial, &grants(), &mut worker),
        Err(FixDispatchError::Refused(RequestRefusal::EffectsDisabled))
    ));
    assert_eq!(worker.delivered.len(), 1);
    let mut unsupported = WorkerFake {
        supported: false,
        delivered: Vec::new(),
        fail: false,
    };
    assert!(matches!(
        gate::dispatch_fix(&active, &grants(), &mut unsupported),
        Err(FixDispatchError::Unsupported)
    ));
    assert!(unsupported.delivered.is_empty());
    let mut uncertain = WorkerFake {
        supported: true,
        delivered: Vec::new(),
        fail: true,
    };
    assert!(matches!(
        gate::dispatch_fix(&active, &grants(), &mut uncertain),
        Err(FixDispatchError::Backend(_))
    ));
    assert!(uncertain.delivered.is_empty());
    Ok(())
}
#[test]
fn fix_grant_without_backend_capability_hands_over() -> TestResult {
    let mut e = ready()?;
    e.contains_base = Some(false);
    let grants = GateGrants {
        fix_delivery_capable: false,
        ..grants()
    };
    assert!(
        matches!(gate::evaluate(&e,grants,GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::BaseBehind))
    );
    Ok(())
}
#[test]
fn risk_approval_requires_scoped_write_permission() -> TestResult {
    let mut e = ready()?;
    e.risk_classes = Some(vec![RiskClass::AuthorizationSecrets]);
    e.risk_approval = Some(RiskApproval {
        approver: "human".into(),
        source: ExternalRef::new("https://example.invalid/approval/9")?,
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: e.head.clone(),
        base: e.base.clone(),
        write_access: true,
        approval_verified: true,
    });
    let policy = ForgeGatePolicy {
        authors: vec!["allowed".into()],
        expected_reviewers: vec!["reviewer".into()],
    };
    let write = forge_client(e.head.as_str(), false)?;
    if let Some(reviews) = write.transport().responses.borrow_mut().get_mut(7) {
        *reviews = serde_json::json!([
          {"id":11,"user":{"login":"reviewer"},"commit_id":e.head.as_str(),"state":"APPROVED","submitted_at":"2026-09-28T14:05:00Z"},
          {"id":12,"user":{"login":"human"},"commit_id":e.head.as_str(),"state":"APPROVED","submitted_at":"2026-09-28T14:06:00Z"}
        ]);
    }
    write
        .transport()
        .responses
        .borrow_mut()
        .push_back(serde_json::json!({"user":{"login":"human"},"permission":"write"}));
    let observed = gate::collect_forge_evidence(
        &write,
        &e.house,
        &e.repository,
        e.number,
        &policy,
        supplement(&e),
        1790607600,
    )?;
    assert_eq!(
        gate::evaluate(&observed, grants(), GateHistory::default()).verdict,
        Verdict::Merge
    );
    let read = forge_client(e.head.as_str(), false)?;
    if let Some(reviews) = read.transport().responses.borrow_mut().get_mut(7) {
        *reviews = serde_json::json!([
          {"id":11,"user":{"login":"reviewer"},"commit_id":e.head.as_str(),"state":"APPROVED","submitted_at":"2026-09-28T14:05:00Z"},
          {"id":12,"user":{"login":"human"},"commit_id":e.head.as_str(),"state":"APPROVED","submitted_at":"2026-09-28T14:06:00Z"}
        ]);
    }
    read.transport()
        .responses
        .borrow_mut()
        .push_back(serde_json::json!({"user":{"login":"human"},"permission":"read"}));
    let observed = gate::collect_forge_evidence(
        &read,
        &e.house,
        &e.repository,
        e.number,
        &policy,
        supplement(&e),
        1790607600,
    )?;
    assert!(
        matches!(gate::evaluate(&observed,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::RiskApproval))
    );
    Ok(())
}
#[test]
fn write_permission_without_current_head_approval_is_insufficient() -> TestResult {
    let mut e = ready()?;
    e.risk_classes = Some(vec![RiskClass::WorkflowRules]);
    e.risk_approval = Some(RiskApproval {
        approver: "human".into(),
        source: ExternalRef::new("https://example.invalid/approval/9")?,
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: e.head.clone(),
        base: e.base.clone(),
        write_access: true,
        approval_verified: true,
    });
    let client = forge_client(e.head.as_str(), false)?;
    client
        .transport()
        .responses
        .borrow_mut()
        .push_back(serde_json::json!({"user":{"login":"human"},"permission":"write"}));
    let observed = gate::collect_forge_evidence(
        &client,
        &e.house,
        &e.repository,
        e.number,
        &ForgeGatePolicy {
            authors: vec!["allowed".into()],
            expected_reviewers: vec!["reviewer".into()],
        },
        supplement(&e),
        1790607600,
    )?;
    assert!(
        matches!(gate::evaluate(&observed,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::RiskApproval))
    );
    Ok(())
}
#[test]
fn label_removal_allows_one_same_head_reevaluation() -> TestResult {
    let mut store = FakeMarkers::default();
    let mut e = ready()?;
    e.hardware_complete = Some(false);
    let first = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 100)?;
    assert!(matches!(first.decision.verdict, Verdict::HandOver { .. }));
    e.reopen_event = Some(GateReopenEvent {
        head: e.head.clone(),
        base: e.base.clone(),
        at_unix_secs: 101,
        actor: "human".into(),
    });
    let second = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 102)?;
    assert!(matches!(second.decision.verdict, Verdict::HandOver { .. }));
    let third = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active, 103)?;
    assert_eq!(third.decision.verdict, Verdict::Skip);
    assert_eq!(store.records.len(), 2);
    Ok(())
}
#[test]
fn forge_label_removal_requires_actor_write_access() -> TestResult {
    let e = ready()?;
    let policy = ForgeGatePolicy {
        authors: vec!["allowed".into()],
        expected_reviewers: vec!["reviewer".into()],
    };
    let write = forge_client(e.head.as_str(), false)?;
    if let Some(timeline) = write.transport().responses.borrow_mut().get_mut(10) {
        *timeline = serde_json::json!([{"event":"unlabeled","created_at":"2026-09-28T14:30:00Z","actor":{"login":"human"},"label":{"name":"needs-human-review"}}]);
    }
    write
        .transport()
        .responses
        .borrow_mut()
        .push_back(serde_json::json!({"user":{"login":"human"},"permission":"write"}));
    let observed = gate::collect_forge_evidence(
        &write,
        &e.house,
        &e.repository,
        e.number,
        &policy,
        supplement(&e),
        1790607600,
    )?;
    assert_eq!(
        observed
            .reopen_event
            .as_ref()
            .map(|event| event.actor.as_str()),
        Some("human")
    );
    let read = forge_client(e.head.as_str(), false)?;
    if let Some(timeline) = read.transport().responses.borrow_mut().get_mut(10) {
        *timeline = serde_json::json!([{"event":"unlabeled","created_at":"2026-09-28T14:30:00Z","actor":{"login":"human"},"label":{"name":"needs-human-review"}}]);
    }
    read.transport()
        .responses
        .borrow_mut()
        .push_back(serde_json::json!({"user":{"login":"human"},"permission":"read"}));
    let observed = gate::collect_forge_evidence(
        &read,
        &e.house,
        &e.repository,
        e.number,
        &policy,
        supplement(&e),
        1790607600,
    )?;
    assert!(observed.reopen_event.is_none());
    Ok(())
}
#[test]
fn closed_or_draft_pr_has_no_effect() -> TestResult {
    let mut e = ready()?;
    e.open = Some(false);
    assert_eq!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::Skip
    );
    e.open = Some(true);
    e.draft = Some(true);
    assert_eq!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::Skip
    );
    Ok(())
}
