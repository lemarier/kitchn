//! Evidence based needs-spec decisions. Callers collect complete, bounded
//! issue history and code evidence; this module never reads an Orca session.

use super::{Precheck, WorkflowError};
use crate::contracts::{DecisionOwner, GitHubAction, IssueNumber, MAX_ASKS_PER_TASK};

/// A human decision with its exact subject revision and owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The declared decision family.
    pub owner: DecisionOwner,
    /// The subject revision the human saw.
    pub revision: String,
    /// Whether an answer remains open.
    pub state: DecisionState,
}

/// Human decision lifecycle. Expiry never means approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionState {
    /// Open.
    Open,
    /// Answered.
    Answered,
    /// Expired.
    Expired,
}

/// Report which workflow owns a legacy answer; no catch-all consumer exists.
pub fn route_answer(key: &str) -> Result<DecisionOwner, WorkflowError> {
    let (prefix, rest) = key
        .split_once(':')
        .ok_or(WorkflowError::UnknownDecisionOwner)?;
    if rest.is_empty() {
        return Err(WorkflowError::UnknownDecisionOwner);
    }
    match prefix {
        "task" => Ok(DecisionOwner::Task),
        "spec" => Ok(DecisionOwner::Spec),
        "merge" => Ok(DecisionOwner::Merge),
        _ => Err(WorkflowError::UnknownDecisionOwner),
    }
}

/// Sources required before the triage pass can use an issue's evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Issue timeline and comments were fully read.
    pub history: bool,
    /// Referenced code was inspected or confirmed absent.
    pub code: bool,
    /// Applicable requirements were inspected.
    pub requirements: bool,
    /// Dependencies and sub-issues were fully read.
    pub relationships: bool,
    /// Linked PRs were inspected or confirmed absent.
    pub linked_prs: bool,
}

impl Coverage {
    /// Whether every required evidence source has a complete answer.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.history && self.code && self.requirements && self.relationships && self.linked_prs
    }
}

/// One issue's evidence and prior pass state.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// Issue.
    pub issue: IssueNumber,
    /// Current issue evidence revision.
    pub revision: String,
    /// Evidence-source coverage; incomplete reads are errors.
    pub coverage: Coverage,
    /// Changed since last pass.
    pub changed_since_last_pass: bool,
    /// Needs spec.
    pub needs_spec: bool,
    /// Human only.
    pub human_only: bool,
    /// Claimed by other.
    pub claimed_by_other: bool,
    /// Open dependencies.
    pub open_dependencies: bool,
    /// Factual resolution.
    pub factual_resolution: Option<String>,
    /// Resolution already posted.
    pub resolution_already_posted: bool,
    /// Pending product questions.
    pub pending_product_questions: u32,
    /// Existing decisions.
    pub existing_decisions: Vec<Decision>,
    /// Ready label present.
    pub ready_label_present: bool,
    /// House-configured ready label.
    pub ready_label: String,
    /// House-configured specification label.
    pub needs_spec_label: String,
}

/// Reads durable no-repeat state for a single issue revision. The eventual
/// house marker store supplies this view; a missing or failed read is an error.
pub trait MarkerView {
    /// Whether the exact resolution for this revision was posted.
    fn resolution_posted(&self, issue: IssueNumber, revision: &str) -> Result<bool, WorkflowError>;

    /// Decisions already asked for this issue and revision.
    fn decisions(&self, issue: IssueNumber, revision: &str)
    -> Result<Vec<Decision>, WorkflowError>;
}

/// Refresh durable no-repeat evidence before planning a pass.
pub fn plan_with_markers(
    evidence: &Evidence,
    markers: &impl MarkerView,
) -> Result<Vec<Change>, WorkflowError> {
    let mut current = evidence.clone();
    current.resolution_already_posted =
        markers.resolution_posted(current.issue, &current.revision)?;
    current.existing_decisions = markers.decisions(current.issue, &current.revision)?;
    plan(&current)
}

/// Changes to preview; the caller persists and executes each typed effect only
/// after its trigger-specific authority and a fresh provider read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Proposed typed forge mutation.
    Mutation(GitHubAction),
    /// Proposed human question slot.
    Ask {
        /// Slot within the task's bounded question budget.
        ordinal: u32,
    },
}

/// No changes means an idle pass. There is never a repeated informational comment.
pub fn precheck(evidence: &Evidence) -> Result<Precheck, WorkflowError> {
    if !evidence.coverage.is_complete()
        || evidence.revision.is_empty()
        || evidence.ready_label.is_empty()
        || evidence.needs_spec_label.is_empty()
        || evidence.ready_label == evidence.needs_spec_label
    {
        return Err(WorkflowError::IncompleteEvidence);
    }
    if !evidence.needs_spec || evidence.human_only || evidence.claimed_by_other {
        return Ok(Precheck::Idle);
    }
    let unresolved = evidence
        .existing_decisions
        .iter()
        .any(|decision| decision.state != DecisionState::Answered);
    let pending_resolution = evidence.needs_spec
        && evidence.factual_resolution.is_some()
        && (!evidence.resolution_already_posted
            || !evidence.ready_label_present && !unresolved && !evidence.open_dependencies
            || evidence.needs_spec && evidence.ready_label_present);
    if evidence.changed_since_last_pass || pending_resolution {
        Ok(Precheck::Actionable)
    } else {
        Ok(Precheck::Idle)
    }
}

/// Plan one pass. An answered decision is useful only at the exact revision;
/// unresolved or expired questions never promote readiness.
pub fn plan(evidence: &Evidence) -> Result<Vec<Change>, WorkflowError> {
    if precheck(evidence)? == Precheck::Idle {
        return Ok(Vec::new());
    }
    if evidence.existing_decisions.iter().any(|decision| {
        decision.owner != DecisionOwner::Spec || decision.revision != evidence.revision
    }) {
        return Err(WorkflowError::DecisionMismatch);
    }
    let unresolved = evidence
        .existing_decisions
        .iter()
        .any(|decision| decision.state != DecisionState::Answered);
    let mut changes = Vec::new();
    if let Some(body) = &evidence.factual_resolution {
        if body.trim().is_empty() {
            return Err(WorkflowError::IncompleteEvidence);
        }
        if !evidence.resolution_already_posted {
            changes.push(Change::Mutation(GitHubAction::PostComment {
                issue: evidence.issue,
                body: crate::contracts::Text::new(body)
                    .map_err(|_| WorkflowError::IncompleteEvidence)?,
            }));
        }
    }
    let remaining = MAX_ASKS_PER_TASK.saturating_sub(evidence.existing_decisions.len() as u32);
    if evidence.factual_resolution.is_none() {
        for ordinal in 0..evidence.pending_product_questions.min(remaining) {
            changes.push(Change::Ask { ordinal });
        }
    }
    if evidence.factual_resolution.is_some()
        && !unresolved
        && !evidence.open_dependencies
        && evidence.pending_product_questions == 0
    {
        if !evidence.ready_label_present {
            changes.push(Change::Mutation(GitHubAction::SetLabel {
                issue: evidence.issue,
                label: evidence.ready_label.clone(),
                present: true,
            }));
        } else if evidence.needs_spec {
            changes.push(Change::Mutation(GitHubAction::SetLabel {
                issue: evidence.issue,
                label: evidence.needs_spec_label.clone(),
                present: false,
            }));
        }
    }
    Ok(changes)
}
