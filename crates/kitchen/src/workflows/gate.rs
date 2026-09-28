//! Exact-revision merge gate policy. All observations are supplied by a scoped reader;
//! this module performs no I/O and never runs code from a proposed change.
use crate::{
    HouseId,
    contracts::{CommitId, ExternalRef, IssueNumber, Repository, Text},
};

/// Evidence for one PR, collected completely at a single head and base.
#[derive(Debug, Clone)]
pub struct GateEvidence {
    /// Selected house whose policy and credentials apply.
    pub house: HouseId,
    /// House-authorized destination repository.
    pub repository: Repository,
    /// PR identity.
    pub number: IssueNumber,
    /// Exact proposed commit.
    pub head: CommitId,
    /// Exact base tip.
    pub base: CommitId,
    /// Seconds since the head was pushed.
    pub head_age_secs: u64,
    /// PR is currently open.
    pub open: Option<bool>,
    /// PR is a draft.
    pub draft: Option<bool>,
    /// Head branch belongs to the target repository.
    pub same_repository: Option<bool>,
    /// Base branch is the current default branch.
    pub targets_default: Option<bool>,
    /// PR author is allowed by house policy.
    pub author_allowed: Option<bool>,
    /// Provider reports CLEAN merge state.
    pub merge_clean: Option<bool>,
    /// Required branch protection is satisfied without admin bypass.
    pub protection_satisfied: Option<bool>,
    /// Rule 3: head contains the current base.
    pub contains_base: Option<bool>,
    /// Every required check and status is present and completed successfully, neutrally, or skipped.
    pub checks: Checks,
    /// Required reviewers and their exact-head outcomes.
    pub reviewers: Vec<ExpectedReviewer>,
    /// Complete review-thread observation; false means at least one unresolved thread.
    pub threads_resolved: Option<bool>,
    /// No outstanding current-head change request.
    pub no_change_request: Option<bool>,
    /// Gate semantic inspection, read-only and based on committed diff content.
    pub semantic_review: SemanticReview,
    /// Demonstrated findings worth fixing on this branch.
    pub verified_findings: Vec<VerifiedFinding>,
    /// Findings the gate disproved with linked evidence.
    pub disproved_findings: Vec<DisprovedFinding>,
    /// Commit actually inspected by the gate reviewer.
    pub semantic_head: Option<CommitId>,
    /// Base actually compared for the gate review.
    pub semantic_base: Option<CommitId>,
    /// The inspection read committed content without executing PR code with credentials.
    pub semantic_read_only: bool,
    /// An independent reviewer performed the inspection; family labels alone do not qualify.
    pub semantic_independent: bool,
    /// Linked issue acceptance evidence is complete.
    pub acceptance_met: Option<bool>,
    /// Required bench, flashing, and other hardware work is complete.
    pub hardware_complete: Option<bool>,
    /// Complete risk classification for the exact diff; `None` is unknown.
    pub risk_classes: Option<Vec<RiskClass>>,
    /// Human write-access approval scoped to both exact head and base.
    pub risk_approval: Option<RiskApproval>,
    /// Branch writer is still working.
    pub writer_working: bool,
}

/// Check conclusion for the full set at the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    /// The observation is still in progress.
    Pending,
    /// The complete observation passed.
    Passed,
    /// The complete observation failed.
    Failed,
    /// Required evidence is absent.
    Missing,
}
/// Outcome of an expected reviewer for one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewerOutcome {
    /// The review found no actionable issue.
    Clean,
    /// The review found actionable issues.
    Findings,
    /// The observation is still in progress.
    Pending,
    /// The reviewer or inspection could not complete.
    Unavailable,
}
/// House-configured reviewer and its observation.
#[derive(Debug, Clone)]
pub struct ExpectedReviewer {
    /// Stable reviewer identity.
    pub name: String,
    /// Actual commit reviewed; old-head reviews cannot pass.
    pub reviewed_head: Option<CommitId>,
    /// Submitted outcome, including quota and skip as unavailable.
    pub outcome: ReviewerOutcome,
}
/// Gate's own review. Partial or failed automated coverage is a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticReview {
    /// The review found no actionable issue.
    Clean,
    /// The review found actionable issues.
    Findings,
    /// Only part of the diff was inspected.
    Partial,
    /// The reviewer or inspection could not complete.
    Unavailable,
}
/// Risk classes requiring human write-access approval at this revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskClass {
    /// Equipment control, firmware, or safety logic.
    EquipmentSafety,
    /// Authorization, secrets, or token permissions.
    AuthorizationSecrets,
    /// Data deletion, migrations, or persisted formats.
    DurableData,
    /// Public APIs, schemas, protocols, or releases.
    PublicContractRelease,
    /// Workflows, CI, CODEOWNERS, agent instructions, or gate rules.
    WorkflowRules,
    /// Added dependencies or major upgrades.
    Dependencies,
    /// Deleted or weakened tests and checks.
    WeakenedValidation,
    /// More than 500 changed lines excluding lockfiles and generated output.
    LargeDiff,
}
/// Human decision checked against the house, action, revision, and write permission.
#[derive(Debug, Clone)]
pub struct RiskApproval {
    /// House that made the decision.
    pub house: HouseId,
    /// Repository where the decision applies.
    pub repository: Repository,
    /// Approved head.
    pub head: CommitId,
    /// Approved base.
    pub base: CommitId,
    /// Positive repository write-permission evidence for the approver.
    pub write_access: bool,
}
/// Review priority for a demonstrated finding within this PR's scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingPriority {
    /// Must be changed before merge.
    ActOn,
    /// Needs a considered change or a documented resolution.
    Consider,
}
/// A verified reviewer or gate finding, with its source and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFinding {
    /// Stable link to the finding.
    pub source: ExternalRef,
    /// Concrete trigger and consequence.
    pub reason: Text,
    /// Reviewer's priority.
    pub priority: FindingPriority,
}
/// A reviewer finding the gate disproved; the worker may reply with this evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisprovedFinding {
    /// Stable link to the original finding.
    pub source: ExternalRef,
    /// Evidence showing why no branch change is needed.
    pub evidence: Text,
}
/// Explicit grants are independent of one another.
#[derive(Debug, Clone, Default)]
pub struct GateGrants {
    /// Permit exact-head squash merge.
    pub merge: bool,
    /// Permit a bounded request to a branch worker to edit, commit, and push.
    pub fix_request: bool,
    /// Permit asking specifically configured reviewers for a fresh review.
    pub reviewer_invocation: bool,
    /// Exact reviewer triggers granted by house policy for this subject.
    pub review_triggers: Vec<ReviewTrigger>,
}
/// One explicitly permitted reviewer invocation, carried as data to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTrigger {
    /// Selected house.
    pub house: HouseId,
    /// Reviewer identity.
    pub reviewer: String,
    /// Exact granted trigger text; the adapter must not invent a command.
    pub command: Text,
    /// Repository where the command may be posted.
    pub repository: Repository,
    /// Head for which the request is allowed.
    pub head: CommitId,
}
/// Persistent per-PR accounting supplied from a house-scoped store.
#[derive(Debug, Clone, Default)]
pub struct GateHistory {
    /// Number of previous fix requests on this PR.
    pub fix_rounds: u8,
    /// Whether this head already received a fix request.
    pub requested_this_head: bool,
    /// Seconds since that request, if any.
    pub request_age_secs: Option<u64>,
    /// Handovers already posted for the PR.
    pub handovers: u8,
    /// Same-head verdict marker, including report-only verdicts.
    pub reported_subject: Option<(CommitId, CommitId)>,
    /// A person removed the handover label or answered a head-bound Ask.
    pub explicit_reopen: bool,
}
/// The distinct failed conditions. Consumers can render these without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Gap {
    /// The PR does not satisfy rule one or its evidence is unknown.
    Eligibility,
    /// Clean protected mergeability is unproven.
    Mergeability,
    /// The head does not contain the current base.
    BaseBehind,
    /// Required checks are pending, failed, or missing.
    Checks,
    /// A required current-head review is missing.
    ReviewerPending,
    /// A required review covers only an older commit.
    ReviewerStale,
    /// A required reviewer reported inability to review.
    ReviewerUnavailable,
    /// Review thread resolution is unproven.
    Threads,
    /// An outstanding change request remains.
    ChangeRequest,
    /// The independent review found actionable issues.
    SemanticFindings,
    /// The independent review did not cover the full diff.
    SemanticCoverage,
    /// Linked issue acceptance evidence is missing.
    Acceptance,
    /// Required physical verification is pending or unknown.
    Hardware,
    /// A risky change lacks exact-revision human approval.
    RiskApproval,
    /// The task lacks merge authority.
    MergeGrant,
    /// The two-request repair budget is exhausted.
    FixBudget,
}
/// One bounded decision at a pinned head and base.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    /// Wait for an unsettled PR or avoid repeating a recorded verdict.
    Skip,
    /// All rules and merge authority hold.
    Merge,
    /// Send one bounded request to the branch worker.
    /// Failed conditions recorded for this action.
    FixRequest {
        /// Failed fixable conditions.
        gaps: Vec<Gap>,
    },
    /// Escalate this revision to a person.
    /// Failed conditions recorded for this action.
    HandOver {
        /// Conditions requiring a person.
        gaps: Vec<Gap>,
    },
}
/// A decision and the exact revision to recheck before any effect.
#[derive(Debug, Clone)]
pub struct GateDecision {
    /// Selected house.
    pub house: HouseId,
    /// Destination repository.
    pub repository: Repository,
    /// PR number.
    pub number: IssueNumber,
    /// Pinned head.
    pub head: CommitId,
    /// Pinned base.
    pub base: CommitId,
    /// Chosen outcome.
    pub verdict: Verdict,
    /// Verified findings carried to a worker or handover.
    pub verified_findings: Vec<VerifiedFinding>,
    /// Disproved findings with reply evidence.
    pub disproved_findings: Vec<DisprovedFinding>,
}

/// Evaluate a fully supplied observation. Missing evidence always fails closed.
#[must_use]
pub fn evaluate(e: &GateEvidence, grants: GateGrants, history: GateHistory) -> GateDecision {
    let mut gaps = Vec::new();
    if e.open != Some(true)
        || e.draft != Some(false)
        || e.same_repository != Some(true)
        || e.targets_default != Some(true)
        || e.author_allowed != Some(true)
    {
        gaps.push(Gap::Eligibility);
    }
    if e.merge_clean != Some(true) || e.protection_satisfied != Some(true) {
        gaps.push(Gap::Mergeability);
    }
    if e.contains_base != Some(true) {
        gaps.push(Gap::BaseBehind);
    }
    match e.checks {
        Checks::Passed => (),
        Checks::Pending | Checks::Failed | Checks::Missing => gaps.push(Gap::Checks),
    }
    for reviewer in &e.reviewers {
        if reviewer.outcome == ReviewerOutcome::Unavailable {
            gaps.push(Gap::ReviewerUnavailable);
        } else if reviewer.outcome == ReviewerOutcome::Pending {
            gaps.push(Gap::ReviewerPending);
        } else if reviewer.reviewed_head.as_ref() != Some(&e.head) {
            gaps.push(Gap::ReviewerStale);
        } else if reviewer.outcome == ReviewerOutcome::Findings {
            gaps.push(Gap::ChangeRequest);
        }
    }
    if e.threads_resolved != Some(true) {
        gaps.push(Gap::Threads);
    }
    if e.no_change_request != Some(true) {
        gaps.push(Gap::ChangeRequest);
    }
    if e.semantic_head.as_ref() != Some(&e.head)
        || e.semantic_base.as_ref() != Some(&e.base)
        || !e.semantic_read_only
        || !e.semantic_independent
    {
        gaps.push(Gap::SemanticCoverage);
    }
    match e.semantic_review {
        SemanticReview::Clean if e.verified_findings.is_empty() => (),
        SemanticReview::Clean => gaps.push(Gap::SemanticFindings),
        SemanticReview::Findings if !e.verified_findings.is_empty() => {
            gaps.push(Gap::SemanticFindings)
        }
        SemanticReview::Findings => gaps.push(Gap::SemanticCoverage),
        SemanticReview::Partial | SemanticReview::Unavailable => gaps.push(Gap::SemanticCoverage),
    }
    if e.acceptance_met != Some(true) {
        gaps.push(Gap::Acceptance);
    }
    if e.hardware_complete != Some(true) {
        gaps.push(Gap::Hardware);
    }
    if e.risk_classes.is_none()
        || (e
            .risk_classes
            .as_ref()
            .is_some_and(|classes| !classes.is_empty())
            && !matches!(&e.risk_approval, Some(approval) if approval.write_access && approval.house == e.house && approval.repository == e.repository && approval.head == e.head && approval.base == e.base))
    {
        gaps.push(Gap::RiskApproval);
    }
    gaps.sort_by_key(|gap| *gap as u8);
    gaps.dedup();
    let verdict = if history.handovers >= 2
        || (history.reported_subject.as_ref() == Some(&(e.head.clone(), e.base.clone()))
            && !history.explicit_reopen)
        || e.head_age_secs < 1800
        || e.writer_working
        || (e.checks == Checks::Pending && e.head_age_secs < 86400)
        || (gaps.contains(&Gap::ReviewerPending) && e.head_age_secs < 86400)
        || (e.merge_clean == Some(false) && e.head_age_secs < 86400)
    {
        Verdict::Skip
    } else if gaps.is_empty() {
        if grants.merge {
            Verdict::Merge
        } else {
            Verdict::HandOver {
                gaps: vec![Gap::MergeGrant],
            }
        }
    } else if e.head_age_secs >= 86400
        && (e.checks == Checks::Pending || gaps.contains(&Gap::ReviewerPending))
    {
        Verdict::HandOver { gaps }
    } else if gaps.iter().all(|gap| {
        matches!(
            gap,
            Gap::BaseBehind
                | Gap::Checks
                | Gap::ChangeRequest
                | Gap::Threads
                | Gap::SemanticFindings
                | Gap::ReviewerPending
                | Gap::ReviewerStale
        )
    }) && grants.fix_request
        && history.fix_rounds < 2
        && !history.requested_this_head
        && (!gaps.contains(&Gap::ReviewerStale) || can_invoke_missing(e, &grants))
    {
        Verdict::FixRequest { gaps }
    } else if history.requested_this_head
        && (e.writer_working || history.request_age_secs.is_some_and(|age| age < 7200))
    {
        Verdict::Skip
    } else {
        if history.fix_rounds >= 2 {
            gaps.push(Gap::FixBudget);
        }
        Verdict::HandOver { gaps }
    };
    GateDecision {
        house: e.house.clone(),
        repository: e.repository.clone(),
        number: e.number,
        head: e.head.clone(),
        base: e.base.clone(),
        verdict,
        verified_findings: e.verified_findings.clone(),
        disproved_findings: e.disproved_findings.clone(),
    }
}

fn can_invoke_missing(e: &GateEvidence, grants: &GateGrants) -> bool {
    grants.reviewer_invocation
        && e.reviewers
            .iter()
            .filter(|reviewer| {
                reviewer.reviewed_head.as_ref() != Some(&e.head)
                    || reviewer.outcome == ReviewerOutcome::Pending
            })
            .all(|reviewer| {
                grants.review_triggers.iter().any(|trigger| {
                    trigger.reviewer.eq_ignore_ascii_case(&reviewer.name)
                        && trigger.repository == e.repository
                        && trigger.head == e.head
                })
            })
}

/// An effect boundary must re-read both refs immediately before a merge.
#[must_use]
pub fn still_current(decision: &GateDecision, head: &CommitId, base: &CommitId) -> bool {
    decision.head == *head && decision.base == *base
}

/// Fixed merge operation; the executor must use a head match and read back the merged PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    /// Authorized house.
    pub house: HouseId,
    /// Authorized repository.
    pub repository: Repository,
    /// PR to merge.
    pub number: IssueNumber,
    /// Exact head supplied to the provider's match-head guard.
    pub match_head: CommitId,
    /// Base tip re-read just before submission.
    pub checked_base: CommitId,
}
/// Why an effect request could not be prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestRefusal {
    /// The decision does not authorize this type of request.
    #[error("decision does not authorize this request")]
    WrongVerdict,
    /// The head or base changed since evaluation.
    #[error("head or base changed")]
    MovedRevision,
    /// Three merges have already been requested in this run.
    #[error("merge limit reached")]
    MergeLimit,
    /// Trial mode or an already recorded verdict forbids effects.
    #[error("recorded decision does not permit effects")]
    EffectsDisabled,
}
/// Prepare at most three head-matched squash merges per run. A merge executor still
/// checks the house and task grants, branch protection, and provider readback.
///
/// # Errors
/// Refuses an unapproved verdict, moving refs, or a fourth request.
pub fn merge_request(
    recorded: &RecordedDecision,
    current_head: &CommitId,
    current_base: &CommitId,
    merges_this_run: u8,
) -> Result<MergeRequest, RequestRefusal> {
    if recorded.mode != GateMode::Active || !recorded.new_record {
        return Err(RequestRefusal::EffectsDisabled);
    }
    let decision = &recorded.decision;
    if decision.verdict != Verdict::Merge {
        return Err(RequestRefusal::WrongVerdict);
    }
    if !still_current(decision, current_head, current_base) {
        return Err(RequestRefusal::MovedRevision);
    }
    if merges_this_run >= 3 {
        return Err(RequestRefusal::MergeLimit);
    }
    Ok(MergeRequest {
        house: decision.house.clone(),
        repository: decision.repository.clone(),
        number: decision.number,
        match_head: decision.head.clone(),
        checked_base: decision.base.clone(),
    })
}

/// Narrow work request sent through a capable worker backend, never a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixRequest {
    /// Authorized house.
    pub house: HouseId,
    /// Authorized repository.
    pub repository: Repository,
    /// PR to repair.
    pub number: IssueNumber,
    /// Head on which the findings were established.
    pub head: CommitId,
    /// Base against which the diff was inspected.
    pub base: CommitId,
    /// Only failed fixable conditions; the worker receives no merge or publication grant.
    pub gaps: Vec<Gap>,
    /// Demonstrated findings to address.
    pub verified_findings: Vec<VerifiedFinding>,
    /// Findings to reply to with disproving evidence.
    pub disproved_findings: Vec<DisprovedFinding>,
    /// The requested worker may invoke only the explicitly granted reviewers.
    pub review_triggers: Vec<ReviewTrigger>,
}
/// Construct a bounded repair request from a fix verdict.
///
/// # Errors
/// Other verdicts cannot start a repair worker.
pub fn fix_request(
    recorded: &RecordedDecision,
    grants: &GateGrants,
) -> Result<FixRequest, RequestRefusal> {
    if recorded.mode != GateMode::Active || !recorded.new_record {
        return Err(RequestRefusal::EffectsDisabled);
    }
    let decision = &recorded.decision;
    let Verdict::FixRequest { gaps } = &decision.verdict else {
        return Err(RequestRefusal::WrongVerdict);
    };
    if !grants.reviewer_invocation && !grants.review_triggers.is_empty() {
        return Err(RequestRefusal::EffectsDisabled);
    }
    if grants.review_triggers.iter().any(|trigger| {
        trigger.house != decision.house
            || trigger.repository != decision.repository
            || trigger.head != decision.head
    }) {
        return Err(RequestRefusal::MovedRevision);
    }
    Ok(FixRequest {
        house: decision.house.clone(),
        repository: decision.repository.clone(),
        number: decision.number,
        head: decision.head.clone(),
        base: decision.base.clone(),
        gaps: gaps.clone(),
        verified_findings: decision.verified_findings.clone(),
        disproved_findings: decision.disproved_findings.clone(),
        review_triggers: if gaps.contains(&Gap::ReviewerStale) {
            grants.review_triggers.clone()
        } else {
            Vec::new()
        },
    })
}

/// Trial mode records a verdict while forbidding every external effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GateMode {
    /// Persist the verdict only.
    ReportOnly,
    /// Allow separately authorized effects.
    Active,
}
/// Small typed payload for the generic house-scoped workflow marker store.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GateVerdictRecord {
    /// Selected house.
    pub house: HouseId,
    /// House-authorized repository.
    pub repository: Repository,
    /// PR number within its house-scoped repository key.
    pub number: IssueNumber,
    /// Revision judged.
    pub head: CommitId,
    /// Base judged.
    pub base: CommitId,
    /// Decision at that revision.
    pub verdict: Verdict,
    /// Whether effects were disabled for this evaluation.
    pub mode: GateMode,
}
/// Atomic marker boundary. The durable implementation belongs to the shared state store.
/// It must key records by house, workflow, repository, PR, head, and base.
pub trait GateMarkerStore {
    /// Persistence failure; it must not be swallowed as an unrecorded verdict.
    type Error;
    /// Read bounded history for a PR and its exact subject.
    ///
    /// # Errors
    /// Fails when history cannot be read completely.
    fn history(
        &self,
        house: &HouseId,
        repository: &Repository,
        number: IssueNumber,
        head: &CommitId,
        base: &CommitId,
    ) -> Result<GateHistory, Self::Error>;
    /// Atomically insert the record if absent. False means the exact subject was already handled.
    ///
    /// # Errors
    /// Fails on uncertain or incomplete persistence.
    fn record_if_absent(&mut self, record: GateVerdictRecord) -> Result<bool, Self::Error>;
}
/// The newly recorded decision and its effect mode.
#[derive(Debug, Clone)]
pub struct RecordedDecision {
    /// Pinned decision.
    pub decision: GateDecision,
    /// Controls all downstream effect submission.
    pub mode: GateMode,
    /// Whether this call added the record; false means a duplicate tick.
    pub new_record: bool,
}
/// Evaluate and record one exact subject before any external effect.
///
/// # Errors
/// Returns storage errors without attempting a handover or merge.
pub fn evaluate_and_record<S: GateMarkerStore>(
    store: &mut S,
    evidence: &GateEvidence,
    grants: GateGrants,
    mode: GateMode,
) -> Result<RecordedDecision, S::Error> {
    let history = store.history(
        &evidence.house,
        &evidence.repository,
        evidence.number,
        &evidence.head,
        &evidence.base,
    )?;
    let mut decision = evaluate(evidence, grants, history);
    if decision.verdict == Verdict::Skip {
        return Ok(RecordedDecision {
            decision,
            mode,
            new_record: false,
        });
    }
    let recorded = store.record_if_absent(GateVerdictRecord {
        house: decision.house.clone(),
        repository: decision.repository.clone(),
        number: decision.number,
        head: decision.head.clone(),
        base: decision.base.clone(),
        verdict: decision.verdict.clone(),
        mode,
    })?;
    if !recorded {
        decision.verdict = Verdict::Skip;
    }
    Ok(RecordedDecision {
        decision,
        mode,
        new_record: recorded,
    })
}
