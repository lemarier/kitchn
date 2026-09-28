//! Evidence based needs-spec decisions. Callers collect complete, bounded
//! issue history and code evidence; this module never reads an Orca session.

use super::{ClaimState, Precheck, WorkflowError, valid_label};
use crate::{
    HouseId,
    contracts::{
        DecisionBinding, DecisionOwner, ExternalRef, GitHubAction, IssueNumber, MAX_ASKS_PER_TASK,
        Operation, Repository, Role, Text, Workspace,
    },
    integrations::github::{
        GitHubClient, GitHubReadTransport, IntegrationError, Issue, IssueComment, IssueDetail,
        LinkedPullRequest, Observation, TimelineEvent,
    },
    integrations::roger::{DecisionStatus, RogerClient, RogerReadTransport},
};

/// Poll only a specification answer with its persisted, exact binding.
/// Other decision families have their own consumers; unknown keys are errors.
pub fn poll_spec_answer<T: RogerReadTransport>(
    client: &RogerClient<T>,
    binding: &DecisionBinding,
    ask: &ExternalRef,
) -> Result<DecisionStatus, WorkflowError> {
    let key = binding
        .decision_key()
        .map_err(|_| WorkflowError::DecisionMismatch)?;
    if binding.owner != DecisionOwner::Spec || route_answer(&key)? != DecisionOwner::Spec {
        return Err(WorkflowError::DecisionMismatch);
    }
    client.poll(binding, ask).map_err(|error| match error {
        IntegrationError::ScopeMismatch | IntegrationError::StaleDecision => {
            WorkflowError::DecisionMismatch
        }
        _ => WorkflowError::PrecheckFailed,
    })
}

/// Complete forge inputs for an issue. Code and house requirements still need
/// their own explicit evidence before a resolution can be chosen.
#[derive(Debug, Clone)]
pub struct IssueSources {
    /// Current issue and labels.
    pub issue: Issue,
    /// Body, author, and timestamps.
    pub detail: IssueDetail,
    /// Entire comment history.
    pub comments: Vec<IssueComment>,
    /// Entire event history.
    pub timeline: Vec<TimelineEvent>,
    /// Explicit blocked-by relationships.
    pub blockers: Vec<Issue>,
    /// Pull requests linked from or closing this issue.
    pub linked_prs: Vec<LinkedPullRequest>,
}

impl IssueSources {
    /// Newest observed comment identity for a durable issue marker key.
    pub fn last_comment(&self) -> Result<Option<ExternalRef>, WorkflowError> {
        self.comments
            .iter()
            .map(|comment| comment.id)
            .max()
            .map(|id| {
                ExternalRef::new(&id.to_string()).map_err(|_| WorkflowError::IncompleteEvidence)
            })
            .transpose()
    }
}

fn known<T>(observation: Observation<T>) -> Result<T, WorkflowError> {
    match observation {
        Observation::Known(value) => Ok(value),
        Observation::Unavailable(_) => Err(WorkflowError::PrecheckFailed),
        Observation::Unknown => Err(WorkflowError::IncompleteEvidence),
    }
}

/// Collect a complete, bounded forge snapshot before policy or an agent runs.
/// Every read is house scoped by #7's client; a partial read stops the pass.
pub fn collect_issue<T: GitHubReadTransport>(
    client: &GitHubClient<T>,
    house: &HouseId,
    repo: &Repository,
    number: IssueNumber,
) -> Result<IssueSources, WorkflowError> {
    let issue = known(client.issue(house, repo, number))?;
    let detail = known(client.issue_detail(house, repo, number))?;
    let comments = known(client.comments(house, repo, number))?;
    let timeline = known(client.timeline(house, repo, number))?;
    let blockers = known(client.dependencies(house, repo, number))?;
    let linked_prs = known(client.linked_pull_requests(house, repo, number))?;
    if known(client.issue_detail(house, repo, number))? != detail
        || known(client.issue(house, repo, number))? != issue
    {
        return Err(WorkflowError::IncompleteEvidence);
    }
    Ok(IssueSources {
        issue,
        detail,
        comments,
        timeline,
        blockers,
        linked_prs,
    })
}

/// A human decision with its exact subject revision and owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Issue the question was bound to.
    pub issue: IssueNumber,
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
    /// Source repository selected by the house.
    pub repository: Repository,
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
    /// Durable claim observation.
    pub claim: ClaimState,
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
    if !evidence.needs_spec || evidence.human_only || evidence.claim == ClaimState::ClaimedByOther {
        return Ok(Vec::new());
    }
    if evidence.claim == ClaimState::Unknown {
        return Err(WorkflowError::IncompleteEvidence);
    }
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
    /// Bounded isolated worker request for evidence judgment.
    Judgment(Operation),
    /// Proposed typed forge mutation.
    Mutation(GitHubAction),
    /// Proposed human question slot.
    Ask {
        /// Slot within the task's bounded question budget.
        ordinal: u32,
    },
}

/// Request bounded judgment when evidence alone cannot resolve the issue or
/// identify a concrete product question. The store must claim the issue and
/// persist this effect before any backend launch.
pub fn judgment_request(evidence: &Evidence) -> Result<Option<Operation>, WorkflowError> {
    if precheck(evidence)? == Precheck::Idle
        || evidence.factual_resolution.is_some()
        || evidence.pending_product_questions > 0
        || !evidence.existing_decisions.is_empty()
    {
        return Ok(None);
    }
    let brief = format!(
        "Inspect {} issue #{} at evidence revision {}; return factual resolution or exact product questions, without posting",
        evidence.repository,
        evidence.issue.get(),
        evidence.revision
    );
    Ok(Some(Operation::LaunchWorker {
        role: Role::Gardener,
        workspace: Workspace::Isolated,
        brief: Text::new(&brief).map_err(|_| WorkflowError::IncompleteEvidence)?,
    }))
}

/// No changes means an idle pass. There is never a repeated informational comment.
pub fn precheck(evidence: &Evidence) -> Result<Precheck, WorkflowError> {
    if !evidence.coverage.is_complete()
        || evidence.revision.is_empty()
        || !valid_label(&evidence.ready_label)
        || !valid_label(&evidence.needs_spec_label)
        || evidence.ready_label == evidence.needs_spec_label
    {
        return Err(WorkflowError::IncompleteEvidence);
    }
    if evidence.claim == ClaimState::Unknown {
        return Err(WorkflowError::IncompleteEvidence);
    }
    if !evidence.needs_spec || evidence.human_only || evidence.claim == ClaimState::ClaimedByOther {
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
        decision.owner != DecisionOwner::Spec
            || decision.issue != evidence.issue
            || decision.revision != evidence.revision
    }) {
        return Err(WorkflowError::DecisionMismatch);
    }
    if evidence.existing_decisions.len() > MAX_ASKS_PER_TASK as usize {
        return Err(WorkflowError::IncompleteEvidence);
    }
    let unresolved = evidence
        .existing_decisions
        .iter()
        .any(|decision| decision.state != DecisionState::Answered);
    let mut changes = Vec::new();
    if let Some(operation) = judgment_request(evidence)? {
        changes.push(Change::Judgment(operation));
    }
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
