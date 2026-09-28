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
        base: commit('b')?,
        head_age_secs: 3600,
        open: Some(true),
        draft: Some(false),
        same_repository: Some(true),
        targets_default: Some(true),
        author_allowed: Some(true),
        merge_clean: Some(true),
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
    })
}
fn grants() -> GateGrants {
    GateGrants {
        merge: true,
        fix_request: true,
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
    e.head_age_secs = 86400;
    assert!(
        matches!(gate::evaluate(&e,grants(),GateHistory::default()).verdict,Verdict::HandOver{gaps} if gaps.contains(&Gap::ReviewerStale))
    );
    e.reviewers[0].reviewed_head = Some(e.head.clone());
    e.risk_classes = Some(vec![RiskClass::AuthorizationSecrets]);
    e.risk_approval = Some(RiskApproval {
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: commit('c')?,
        base: e.base.clone(),
        write_access: true,
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
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: e.head.clone(),
        base: e.base.clone(),
        write_access: true,
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
        house: e.house.clone(),
        repository: e.repository.clone(),
        head: e.head.clone(),
        base: e.base.clone(),
        write_access: false,
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
    ) -> Result<GateHistory, Self::Error> {
        Ok(GateHistory {
            reported_subject: self
                .records
                .iter()
                .find(|r| {
                    &r.house == house
                        && &r.repository == repository
                        && r.number == number
                        && &r.head == head
                        && &r.base == base
                })
                .map(|r| (r.head.clone(), r.base.clone())),
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
    let first = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly)?;
    assert_eq!(first.decision.verdict, Verdict::Merge);
    assert_eq!(first.mode, GateMode::ReportOnly);
    assert_eq!(
        gate::merge_request(&first, &e.head, &e.base, 0),
        Err(RequestRefusal::EffectsDisabled)
    );
    let second = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly)?;
    assert_eq!(second.decision.verdict, Verdict::Skip);
    assert_eq!(store.records.len(), 1);
    e.repository = Repository::new("other/repository")?;
    let other = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly)?;
    assert!(other.new_record);
    assert_eq!(store.records.len(), 2);
    e.house = HouseId::new("other-house")?;
    let cross = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly)?;
    assert!(cross.new_record);
    assert_eq!(store.records.len(), 3);
    e.base = commit('c')?;
    let moved = gate::evaluate_and_record(&mut store, &e, grants(), GateMode::ReportOnly)?;
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
    assert!(gate::evaluate_and_record(&mut store, &e, grants(), GateMode::Active).is_err());
    assert!(store.records.is_empty());
    Ok(())
}
#[test]
fn moving_and_conflicting_heads_skip_until_stalled() -> TestResult {
    let mut e = ready()?;
    e.head_age_secs = 1799;
    assert_eq!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::Skip
    );
    e.head_age_secs = 3600;
    e.merge_clean = Some(false);
    assert_eq!(
        gate::evaluate(&e, grants(), GateHistory::default()).verdict,
        Verdict::Skip
    );
    e.head_age_secs = 86400;
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
    e.head_age_secs = 86400;
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
    e.head_age_secs = 3600;
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
