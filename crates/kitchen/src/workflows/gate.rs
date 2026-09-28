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
    pub head_age_secs: Option<u64>,
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
    /// Link to the actual independent review record.
    pub semantic_source: Option<ExternalRef>,
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
    /// Revision to which acceptance, hardware, and risk observations apply.
    pub supporting_subject: Option<(CommitId, CommitId)>,
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
    /// Head timestamp is unavailable or malformed.
    HeadAge,
    /// The head does not contain the current base.
    BaseBehind,
    /// Supporting evidence belongs to another head or base.
    SupportingSubject,
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
    if e.head_age_secs.is_none() {
        gaps.push(Gap::HeadAge);
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
        || e.semantic_source.is_none()
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
    if e.supporting_subject.as_ref() != Some(&(e.head.clone(), e.base.clone())) {
        gaps.push(Gap::SupportingSubject);
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
    let age = e.head_age_secs.unwrap_or(86400);
    let verdict = if history.handovers >= 2
        || (history.reported_subject.as_ref() == Some(&(e.head.clone(), e.base.clone()))
            && !history.explicit_reopen)
        || age < 1800
        || e.writer_working
        || (e.checks == Checks::Pending && age < 86400)
        || (gaps.contains(&Gap::ReviewerPending) && age < 86400)
        || (e.merge_clean == Some(false) && age < 86400)
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
    } else if age >= 86400 && (e.checks == Checks::Pending || gaps.contains(&Gap::ReviewerPending))
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
impl MergeRequest {
    /// Build the typed #7 effect; the state store must persist and authorize it
    /// before execution. The provider checks `expected_head` and uses squash.
    #[must_use]
    pub fn mutation(&self) -> crate::contracts::GitHubMutation {
        crate::contracts::GitHubMutation {
            repository: self.repository.clone(),
            action: crate::contracts::GitHubAction::MergePullRequest {
                number: self.number,
                expected_head: self.match_head.clone(),
                method: crate::contracts::MergeMethod::Squash,
            },
        }
    }
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

/// Re-read the provider's head and base immediately before preparing the
/// persisted merge effect. A moved ref is refused; the provider still enforces
/// the head match when it receives the squash request.
///
/// # Errors
/// Refuses incomplete reads, a moved revision, or a non-merge verdict.
pub fn merge_request_from_forge<T: crate::integrations::github::GitHubReadTransport>(
    recorded: &RecordedDecision,
    client: &crate::integrations::github::GitHubClient<T>,
    merges_this_run: u8,
) -> Result<MergeRequest, crate::integrations::github::IntegrationError> {
    use crate::integrations::github::{IntegrationError, Observation};
    let decision = &recorded.decision;
    let pr = match client.pull_request(&decision.house, &decision.repository, decision.number) {
        Observation::Known(pr) => pr,
        Observation::Unknown => return Err(IntegrationError::Unknown),
        Observation::Unavailable(error) => return Err(error),
    };
    if pr.state != crate::integrations::github::IssueState::Open || pr.draft || pr.merged {
        return Err(IntegrationError::StaleDecision);
    }
    merge_request(recorded, &pr.head.sha, &pr.base.sha, merges_this_run)
        .map_err(|_| IntegrationError::StaleDecision)
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

/// Bounded person handoff at an exact revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandOverRequest {
    /// Selected house.
    pub house: HouseId,
    /// Destination repository.
    pub repository: Repository,
    /// PR to hand over.
    pub number: IssueNumber,
    /// Judged head.
    pub head: CommitId,
    /// Judged base.
    pub base: CommitId,
    /// Failed conditions.
    pub gaps: Vec<Gap>,
    /// Findings a person must inspect.
    pub findings: Vec<VerifiedFinding>,
}

/// Prepare a handoff only from a newly recorded active verdict.
///
/// # Errors
/// Report-only, duplicate, and other verdicts cannot post a handoff.
pub fn handover_request(recorded: &RecordedDecision) -> Result<HandOverRequest, RequestRefusal> {
    if recorded.mode != GateMode::Active || !recorded.new_record {
        return Err(RequestRefusal::EffectsDisabled);
    }
    let decision = &recorded.decision;
    let Verdict::HandOver { gaps } = &decision.verdict else {
        return Err(RequestRefusal::WrongVerdict);
    };
    Ok(HandOverRequest {
        house: decision.house.clone(),
        repository: decision.repository.clone(),
        number: decision.number,
        head: decision.head.clone(),
        base: decision.base.clone(),
        gaps: gaps.clone(),
        findings: decision.verified_findings.clone(),
    })
}

impl HandOverRequest {
    /// Typed label and comment effects. The caller persists each through the
    /// house-scoped effect store and reconciles uncertain outcomes before retry.
    ///
    /// # Errors
    /// Refuses an oversized or invalid summary without producing effects.
    pub fn mutations(
        &self,
    ) -> Result<Vec<crate::contracts::GitHubMutation>, crate::contracts::ContractError> {
        use crate::contracts::{GitHubAction, GitHubMutation};
        let mut body = format!(
            "Gate handover for head {} against base {}.\nFailed conditions: {:?}.",
            self.head, self.base, self.gaps
        );
        for finding in &self.findings {
            use std::fmt::Write as _;
            let _ = write!(
                body,
                "\nFinding: {} — {}",
                finding.source,
                finding.reason.as_str()
            );
        }
        use std::fmt::Write as _;
        let _ = write!(
            body,
            "\n<!-- kitchen-gate handover head={} base={} -->",
            self.head, self.base
        );
        let text = Text::new(&body)?;
        let mutations = vec![
            GitHubMutation {
                repository: self.repository.clone(),
                action: GitHubAction::SetLabel {
                    issue: self.number,
                    label: "needs-human-review".into(),
                    present: true,
                },
            },
            GitHubMutation {
                repository: self.repository.clone(),
                action: GitHubAction::PostComment {
                    issue: self.number,
                    body: text,
                },
            },
        ];
        for mutation in &mutations {
            mutation.validate()?;
        }
        Ok(mutations)
    }
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
    /// Zero-based request or handover round, preserving an explicit reopen.
    pub round: u8,
    /// Wall-clock second when the decision was recorded, for the fix timeout.
    pub recorded_unix_secs: u64,
}
/// Atomic marker boundary. The durable implementation belongs to the shared state store.
/// It must key records by house, workflow, repository, PR, head, base, and
/// verdict kind and round, so an expired fix request can become one handover at
/// the same head and one explicit reopen can produce a second handover.
pub trait GateMarkerStore {
    /// Persistence failure; it must not be swallowed as an unrecorded verdict.
    type Error;
    /// Read bounded history for a PR and its exact subject. A fix request sets
    /// `requested_this_head`, not `reported_subject`; it must time out after two
    /// hours if the writer is idle. Report-only and handover records set
    /// `reported_subject` for deduplication.
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
        now_unix_secs: u64,
    ) -> Result<GateHistory, Self::Error>;
    /// Atomically insert the record if absent. False means this verdict kind was
    /// already recorded for the exact subject.
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
    now_unix_secs: u64,
) -> Result<RecordedDecision, S::Error> {
    let history = store.history(
        &evidence.house,
        &evidence.repository,
        evidence.number,
        &evidence.head,
        &evidence.base,
        now_unix_secs,
    )?;
    let round = history.handovers;
    let fix_round = history.fix_rounds;
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
        round: if matches!(decision.verdict, Verdict::FixRequest { .. }) {
            fix_round
        } else {
            round
        },
        recorded_unix_secs: now_unix_secs,
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

/// House-defined forge expectations. The list is complete for this workflow run.
#[derive(Debug, Clone)]
pub struct ForgeGatePolicy {
    /// PR authors eligible for unattended merge.
    pub authors: Vec<String>,
    /// Reviewer logins required at the current head.
    pub expected_reviewers: Vec<String>,
}
/// Non-forge evidence supplied by independent house-scoped reviewers and workers.
#[derive(Debug, Clone)]
pub struct GateSupplement {
    /// Gate semantic review result.
    pub semantic_review: SemanticReview,
    /// Link to the independent review record.
    pub semantic_source: Option<ExternalRef>,
    /// Verified actionable findings.
    pub verified_findings: Vec<VerifiedFinding>,
    /// Findings disproved with evidence.
    pub disproved_findings: Vec<DisprovedFinding>,
    /// Head actually reviewed.
    pub semantic_head: Option<CommitId>,
    /// Base actually reviewed.
    pub semantic_base: Option<CommitId>,
    /// Review read committed content without running PR code with credentials.
    pub semantic_read_only: bool,
    /// Independent review was observed; model family alone is insufficient.
    pub semantic_independent: bool,
    /// Linked issue acceptance evidence.
    pub acceptance_met: Option<bool>,
    /// Hardware verification completion.
    pub hardware_complete: Option<bool>,
    /// Complete diff risk classification.
    pub risk_classes: Option<Vec<RiskClass>>,
    /// Exact-revision human decision, if one exists.
    pub risk_approval: Option<RiskApproval>,
    /// Branch writer status.
    pub writer_working: bool,
    /// Exact revision of this supporting evidence.
    pub subject: Option<(CommitId, CommitId)>,
}

/// Collect forge facts through #7's scoped read side. Missing secondary observations
/// remain unknown and cannot produce a merge decision. The caller supplies a Unix
/// seconds clock value and separately attested non-forge evidence.
///
/// # Errors
/// Returns an integration error if the PR itself cannot be identified.
pub fn collect_forge_evidence<T: crate::integrations::github::GitHubReadTransport>(
    client: &crate::integrations::github::GitHubClient<T>,
    house: &HouseId,
    repository: &Repository,
    number: IssueNumber,
    policy: &ForgeGatePolicy,
    supplement: GateSupplement,
    now_unix_secs: u64,
) -> Result<GateEvidence, crate::integrations::github::IntegrationError> {
    use crate::integrations::github::{HeadLocation, MergeStatusValue, Observation};
    let pr = match client.pull_request(house, repository, number) {
        Observation::Known(pr) => pr,
        Observation::Unavailable(error) => return Err(error),
        Observation::Unknown => return Err(crate::integrations::github::IntegrationError::Unknown),
    };
    let head = pr.head.sha.clone();
    let base = pr.base.sha.clone();
    let repository_info = known(client.repository(house, repository));
    let merge_status = known(client.merge_status(house, repository, number, &head));
    let comparison = known(client.compare(house, repository, &base, &head));
    let runs = known(client.checks(house, repository, &head));
    let statuses = known(client.statuses(house, repository, &head));
    let required = known(client.required_checks(house, repository, &pr.base.name));
    let reviews = known(client.reviews(house, repository, number));
    let threads = known(client.threads(house, repository, number));
    let commit = known(client.commit(house, repository, &head));
    let head_age_secs = commit
        .as_ref()
        .and_then(|commit| parse_github_utc(&commit.commit.committer.date))
        .and_then(|committed| now_unix_secs.checked_sub(committed));
    let reviewers = expected_reviews(&policy.expected_reviewers, reviews.as_deref(), &head);
    let no_change_request = reviews.as_deref().and_then(|reviews| {
        use crate::integrations::github::ReviewState;
        let mut ordered: Vec<_> = reviews.iter().collect();
        ordered.sort_by_key(|review| review.id);
        let mut outstanding = std::collections::BTreeMap::new();
        for review in ordered {
            let entry = outstanding
                .entry(review.user.login.to_ascii_lowercase())
                .or_insert(false);
            match review.state {
                ReviewState::ChangesRequested => *entry = true,
                ReviewState::Approved | ReviewState::Dismissed => *entry = false,
                ReviewState::Commented | ReviewState::Pending => (),
                ReviewState::Unknown => return None,
            }
        }
        Some(outstanding.values().all(|requested| !requested))
    });
    let checks = classify_checks(
        runs.as_deref(),
        statuses.as_deref(),
        required.as_ref(),
        &head,
    );
    Ok(GateEvidence {
        house: house.clone(),
        repository: repository.clone(),
        number,
        head,
        base,
        head_age_secs,
        open: Some(pr.state == crate::integrations::github::IssueState::Open && !pr.merged),
        draft: Some(pr.draft),
        same_repository: match pr.head_location(repository) {
            HeadLocation::SameRepository => Some(true),
            HeadLocation::Fork => Some(false),
            HeadLocation::Unknown => None,
        },
        targets_default: repository_info.map(|info| info.default_branch == pr.base.name),
        author_allowed: pr.user.map(|user| {
            policy
                .authors
                .iter()
                .any(|author| author.eq_ignore_ascii_case(&user.login))
        }),
        merge_clean: merge_status
            .as_ref()
            .map(|status| status.status == MergeStatusValue::Clean),
        protection_satisfied: merge_status.map(|status| status.status == MergeStatusValue::Clean),
        contains_base: comparison.map(|comparison| comparison.behind_by == 0),
        checks,
        reviewers,
        threads_resolved: threads.map(|threads| threads.iter().all(|thread| thread.is_resolved)),
        no_change_request,
        semantic_review: supplement.semantic_review,
        semantic_source: supplement.semantic_source,
        verified_findings: supplement.verified_findings,
        disproved_findings: supplement.disproved_findings,
        semantic_head: supplement.semantic_head,
        semantic_base: supplement.semantic_base,
        semantic_read_only: supplement.semantic_read_only,
        semantic_independent: supplement.semantic_independent,
        acceptance_met: supplement.acceptance_met,
        hardware_complete: supplement.hardware_complete,
        risk_classes: supplement.risk_classes,
        risk_approval: supplement.risk_approval,
        writer_working: supplement.writer_working,
        supporting_subject: supplement.subject,
    })
}
fn known<T>(observation: crate::integrations::github::Observation<T>) -> Option<T> {
    match observation {
        crate::integrations::github::Observation::Known(value) => Some(value),
        crate::integrations::github::Observation::Unavailable(_)
        | crate::integrations::github::Observation::Unknown => None,
    }
}
fn classify_checks(
    runs: Option<&[crate::integrations::github::CheckRun]>,
    statuses: Option<&[crate::integrations::github::CommitStatus]>,
    required: Option<&crate::integrations::github::RequiredChecks>,
    head: &CommitId,
) -> Checks {
    use crate::integrations::github::{
        CheckConclusion, CheckStatus, RequiredCheckPresence, StatusState,
    };
    let (Some(runs), Some(statuses), Some(required)) = (runs, statuses, required) else {
        return Checks::Missing;
    };
    match required.presence(runs, statuses, head) {
        RequiredCheckPresence::Present => (),
        RequiredCheckPresence::Missing | RequiredCheckPresence::Unknown => return Checks::Missing,
    }
    if runs.iter().any(|run| {
        run.status == CheckStatus::Unknown
            || (run.status == CheckStatus::Completed
                && !matches!(
                    run.conclusion,
                    Some(
                        CheckConclusion::Success
                            | CheckConclusion::Neutral
                            | CheckConclusion::Skipped
                    )
                ))
    }) || statuses.iter().any(|status| {
        matches!(
            status.state,
            StatusState::Failure | StatusState::Error | StatusState::Unknown
        )
    }) {
        return Checks::Failed;
    }
    if runs.iter().any(|run| run.status != CheckStatus::Completed)
        || statuses
            .iter()
            .any(|status| status.state == StatusState::Pending)
    {
        return Checks::Pending;
    }
    Checks::Passed
}
fn expected_reviews(
    names: &[String],
    reviews: Option<&[crate::integrations::github::Review]>,
    head: &CommitId,
) -> Vec<ExpectedReviewer> {
    names
        .iter()
        .map(|name| {
            let matching = reviews.and_then(|reviews| {
                reviews
                    .iter()
                    .filter(|review| {
                        review.user.login.eq_ignore_ascii_case(name) && &review.commit_id == head
                    })
                    .max_by_key(|review| review.id)
                    .or_else(|| {
                        reviews
                            .iter()
                            .filter(|review| review.user.login.eq_ignore_ascii_case(name))
                            .max_by_key(|review| review.id)
                    })
            });
            let (reviewed_head, outcome) = match matching {
                None => (None, ReviewerOutcome::Pending),
                Some(review) => {
                    let outcome = match review.state {
                        crate::integrations::github::ReviewState::Approved
                        | crate::integrations::github::ReviewState::Commented => {
                            ReviewerOutcome::Clean
                        }
                        crate::integrations::github::ReviewState::ChangesRequested => {
                            ReviewerOutcome::Findings
                        }
                        crate::integrations::github::ReviewState::Dismissed
                        | crate::integrations::github::ReviewState::Pending
                        | crate::integrations::github::ReviewState::Unknown => {
                            ReviewerOutcome::Pending
                        }
                    };
                    (Some(review.commit_id.clone()), outcome)
                }
            };
            ExpectedReviewer {
                name: name.clone(),
                reviewed_head,
                outcome,
            }
        })
        .collect()
}
fn parse_github_utc(value: &str) -> Option<u64> {
    let b = value.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let part = |start: usize, end: usize| -> Option<u64> {
        b[start..end].iter().try_fold(0_u64, |acc, digit| {
            if digit.is_ascii_digit() {
                Some(acc * 10 + u64::from(digit - b'0'))
            } else {
                None
            }
        })
    };
    let year = part(0, 4)?;
    let month = part(5, 7)?;
    let day = part(8, 10)?;
    let hour = part(11, 13)?;
    let minute = part(14, 16)?;
    let second = part(17, 19)?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let leap = |y: u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let index = usize::try_from(month - 1).ok()?;
    let max_day = month_days[index] + u64::from(index == 1 && leap(year));
    if day == 0 || day > max_day {
        return None;
    }
    let years = (1970..year).map(|y| 365 + u64::from(leap(y))).sum::<u64>();
    let months = month_days[..index].iter().sum::<u64>() + u64::from(month > 2 && leap(year));
    Some(((years + months + day - 1) * 24 + hour) * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::parse_github_utc;
    #[test]
    fn parses_github_utc_and_refuses_invalid_calendar_dates() {
        assert_eq!(parse_github_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_github_utc("2026-09-28T14:00:00Z"),
            Some(1_790_604_000)
        );
        assert!(parse_github_utc("2025-02-29T00:00:00Z").is_none());
        assert!(parse_github_utc("2024-02-29T23:59:59Z").is_some());
        assert!(parse_github_utc("2026-09-28T14:00:00+02:00").is_none());
    }
}
