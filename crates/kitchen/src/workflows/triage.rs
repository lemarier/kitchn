//! Evidence based needs-spec decisions. Callers collect complete, bounded
//! issue history and code evidence; this module never reads an Orca session.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use super::{ClaimState, Precheck, WorkflowError, known, valid_label};
use crate::{
    HouseId, WorkflowId,
    contracts::{
        Claimant, DecisionBinding, DecisionOwner, ExternalRef, GitHubAction, IssueNumber,
        MAX_ASKS_PER_TASK, Operation, Repository, Role, Text, Timestamp, Workspace,
    },
    integrations::github::{
        GitHubClient, GitHubReadTransport, IntegrationError, Issue, IssueComment, IssueDetail,
        LinkedPullRequest, TimelineEvent,
    },
    integrations::roger::{DecisionStatus, RogerClient, RogerReadTransport},
    selection::AgentSelection,
    state::{
        HouseStore, IssueRevision, MarkerFact, MarkerKey, MarkerRecording, MarkerSchema,
        MarkerSubject, StateError, WorkItem,
    },
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
        other => WorkflowError::precheck(other),
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

    /// The provider revision a human answer or a marker is bound to: the
    /// issue's last update and the newest comment observed.
    pub fn revision(&self) -> Result<IssueRevision, WorkflowError> {
        Ok(IssueRevision {
            updated_at: self.detail.updated_at,
            last_comment: self.last_comment()?,
        })
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
    pub revision: IssueRevision,
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
    pub revision: IssueRevision,
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
    pub factual_resolution: Option<Resolution>,
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
    /// The agent selection recorded on the gardener task
    /// ([`TaskSpec::agent`](crate::contracts::TaskSpec::agent)). The judgment
    /// launch carries exactly this, because the store refuses a launch that
    /// differs from the task's selection.
    pub agent: Option<AgentSelection>,
}

/// Reads durable no-repeat state for one issue. A missing or failed read is
/// an error, never an empty history.
pub trait MarkerView {
    /// Every revision at which a question about `issue` was asked.
    fn asked(
        &self,
        repository: &Repository,
        issue: IssueNumber,
    ) -> Result<Vec<IssueRevision>, WorkflowError>;

    /// Whether a resolution with `resolution`'s identity (decision and kind)
    /// was already posted on `issue`, at any revision and in any wording.
    /// Posting a comment moves the revision, so the lookup cannot be keyed
    /// by the current one.
    fn resolution_posted(
        &self,
        repository: &Repository,
        issue: IssueNumber,
        resolution: &Resolution,
    ) -> Result<bool, WorkflowError>;
}

/// What earlier passes did about one issue, read only through a
/// [`MarkerView`] so callers cannot assert it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    asked_at: Vec<IssueRevision>,
    resolution_posted: bool,
}

impl History {
    /// Read `evidence`'s issue history from `markers`.
    ///
    /// # Errors
    /// Returns the view's error; a failed read never becomes empty history.
    pub fn read(markers: &impl MarkerView, evidence: &Evidence) -> Result<Self, WorkflowError> {
        let resolution_posted = match &evidence.factual_resolution {
            Some(resolution) => {
                markers.resolution_posted(&evidence.repository, evidence.issue, resolution)?
            }
            None => false,
        };
        Ok(Self {
            asked_at: markers.asked(&evidence.repository, evidence.issue)?,
            resolution_posted,
        })
    }
}

/// How a resolution settles its needs-spec decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResolutionKind {
    /// Settled from issue history, code, or requirements evidence.
    Evidence,
    /// Settled by a human answer to a recorded question.
    Answer,
}

/// A resolution to post on an issue. Its identity is the question or
/// decision it settles and its kind, never its wording, so a reworded
/// resolution for the same decision is not posted again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The question or decision this resolution settles.
    pub decision: ExternalRef,
    /// How it was settled.
    pub kind: ResolutionKind,
    /// The comment to post.
    pub body: String,
}

/// Schema of the triage marker recording a posted resolution.
const RESOLUTION_SCHEMA: &str = "triage.resolution";

/// Version 2 keys the marker by decision identity.
const RESOLUTION_SCHEMA_VERSION: NonZeroU32 = NonZeroU32::MIN.saturating_add(1);

/// Version 1 held only a digest of the posted text. It proves a resolution
/// was posted but not which decision it settled, so it is never decoded.
const LEGACY_RESOLUTION_SCHEMA_VERSION: NonZeroU32 = NonZeroU32::MIN;

/// The posted resolution's identity. Issue text is not copied into the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolutionPosted {
    decision: ExternalRef,
    kind: ResolutionKind,
}

impl ResolutionPosted {
    fn of(resolution: &Resolution) -> Self {
        Self {
            decision: resolution.decision.clone(),
            kind: resolution.kind,
        }
    }
}

fn resolution_schema() -> Result<MarkerSchema, WorkflowError> {
    MarkerSchema::new(RESOLUTION_SCHEMA, RESOLUTION_SCHEMA_VERSION)
        .map_err(|_| WorkflowError::IncompleteEvidence)
}

/// Whether `fact` is a version 1 resolution marker: a resolution was posted,
/// decision unknown.
fn is_legacy_resolution(fact: &MarkerFact) -> Result<bool, WorkflowError> {
    let legacy = MarkerSchema::new(RESOLUTION_SCHEMA, LEGACY_RESOLUTION_SCHEMA_VERSION)
        .map_err(|_| WorkflowError::IncompleteEvidence)?;
    Ok(matches!(fact, MarkerFact::Workflow { schema, .. } if *schema == legacy))
}

/// Question markers in the house store, keyed by workflow, issue, and
/// [`MarkerSubject::Issue`]. An edited or newly commented issue is a new key.
#[derive(Debug, Clone)]
pub struct IssueMarkers<'a> {
    store: &'a HouseStore,
    workflow: WorkflowId,
}

impl<'a> IssueMarkers<'a> {
    /// Read `workflow`'s markers from `store`.
    #[must_use]
    pub const fn new(store: &'a HouseStore, workflow: WorkflowId) -> Self {
        Self { store, workflow }
    }
}

fn work_item(repository: &Repository, issue: IssueNumber) -> Result<WorkItem, WorkflowError> {
    Ok(WorkItem::Issue {
        repository: repository.clone(),
        number: NonZeroU64::new(issue.get()).ok_or(WorkflowError::IncompleteEvidence)?,
    })
}

impl MarkerView for IssueMarkers<'_> {
    fn asked(
        &self,
        repository: &Repository,
        issue: IssueNumber,
    ) -> Result<Vec<IssueRevision>, WorkflowError> {
        let item = work_item(repository, issue)?;
        let markers = self
            .store
            .markers(&self.workflow)
            .map_err(WorkflowError::precheck_store)?;
        markers
            .iter()
            .filter(|marker| marker.key().item == item)
            .filter(|marker| matches!(marker.fact(), MarkerFact::QuestionAsked { .. }))
            .map(|marker| match &marker.key().subject {
                MarkerSubject::Issue(revision) => Ok(revision.clone()),
                MarkerSubject::Git(_) | MarkerSubject::Observation(_) => {
                    Err(WorkflowError::IncompleteEvidence)
                }
            })
            .collect()
    }

    fn resolution_posted(
        &self,
        repository: &Repository,
        issue: IssueNumber,
        resolution: &Resolution,
    ) -> Result<bool, WorkflowError> {
        let item = work_item(repository, issue)?;
        let schema = resolution_schema()?;
        let wanted = ResolutionPosted::of(resolution);
        let markers = self
            .store
            .markers(&self.workflow)
            .map_err(WorkflowError::precheck_store)?;
        let mut posted = false;
        let mut legacy = false;
        let mut identified = false;
        for marker in markers.iter().filter(|marker| marker.key().item == item) {
            match marker.fact() {
                MarkerFact::QuestionAsked { .. } => {}
                fact if is_legacy_resolution(fact)? => legacy = true,
                // Any other fact under the triage workflow is unexpected; it
                // proves neither outcome, so the pass stops.
                fact @ (MarkerFact::Workflow { .. } | MarkerFact::Verdict { .. }) => {
                    let recorded: ResolutionPosted = fact
                        .decode(&schema)
                        .map_err(|_| WorkflowError::IncompleteEvidence)?;
                    identified = true;
                    posted |= recorded == wanted;
                }
            }
        }
        // A version 1 marker stands for "posted, decision unknown" until the
        // issue has a marker that names a decision.
        Ok(posted || legacy && !identified)
    }
}

/// Record that `question` was asked about `issue` at `revision`. Recording
/// the same question again changes nothing; a different question at the same
/// revision is refused, because a revision gets at most one ask.
#[expect(
    clippy::too_many_arguments,
    reason = "each value is part of the durable marker key or its provenance"
)]
pub fn record_question(
    store: &HouseStore,
    workflow: &WorkflowId,
    repository: &Repository,
    issue: IssueNumber,
    revision: &IssueRevision,
    question: ExternalRef,
    recorded_by: &Claimant,
    now: Timestamp,
) -> Result<MarkerRecording, WorkflowError> {
    let key = MarkerKey {
        workflow: workflow.clone(),
        item: work_item(repository, issue)?,
        subject: MarkerSubject::Issue(revision.clone()),
    };
    record(
        store,
        key,
        MarkerFact::QuestionAsked { question },
        recorded_by,
        now,
    )
}

/// Record that `resolution` was posted on `issue` after being judged at
/// `revision`. Record it before submitting the comment effect, which owns
/// delivery and reconciliation; the marker then outlives the revision change
/// the comment causes. Recording the same decision and kind again, in any
/// wording, changes nothing; another decision or kind at the same judged
/// revision is refused. A version 1 (text digest) marker at the judged
/// revision is superseded.
#[expect(
    clippy::too_many_arguments,
    reason = "each value is part of the durable marker key or its provenance"
)]
pub fn record_resolution(
    store: &HouseStore,
    workflow: &WorkflowId,
    repository: &Repository,
    issue: IssueNumber,
    revision: &IssueRevision,
    resolution: &Resolution,
    recorded_by: &Claimant,
    now: Timestamp,
) -> Result<MarkerRecording, WorkflowError> {
    if resolution.body.trim().is_empty() {
        return Err(WorkflowError::IncompleteEvidence);
    }
    let key = MarkerKey {
        workflow: workflow.clone(),
        item: work_item(repository, issue)?,
        subject: MarkerSubject::Issue(revision.clone()),
    };
    let fact = MarkerFact::workflow(resolution_schema()?, &ResolutionPosted::of(resolution))
        .map_err(|_| WorkflowError::IncompleteEvidence)?;
    // A version 1 marker at this revision is replaced by the identified one.
    // The swap compares against the fact just read, so a concurrent change
    // is refused rather than overwritten.
    let existing = store.marker(&key).map_err(WorkflowError::precheck_store)?;
    if let Some(existing) =
        existing.filter(|marker| matches!(is_legacy_resolution(marker.fact()), Ok(true)))
    {
        return store
            .supersede_marker(&key, existing.fact(), fact, recorded_by, now)
            .map_err(marker_error);
    }
    record(store, key, fact, recorded_by, now)
}

fn record(
    store: &HouseStore,
    key: MarkerKey,
    fact: MarkerFact,
    recorded_by: &Claimant,
    now: Timestamp,
) -> Result<MarkerRecording, WorkflowError> {
    store
        .record_marker(key, fact, recorded_by, now)
        .map_err(marker_error)
}

fn marker_error(error: crate::Error) -> WorkflowError {
    match error {
        crate::Error::State(StateError::MarkerConflict) => WorkflowError::DecisionMismatch,
        other => WorkflowError::precheck_store(other),
    }
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
    plan(evidence, &History::read(markers, evidence)?)
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
pub fn judgment_request(
    evidence: &Evidence,
    history: &History,
) -> Result<Option<Operation>, WorkflowError> {
    if precheck(evidence, history)? == Precheck::Idle
        || evidence.factual_resolution.is_some()
        || evidence.pending_product_questions > 0
        || !evidence.existing_decisions.is_empty()
    {
        return Ok(None);
    }
    let brief = format!(
        "Inspect {} issue #{} as updated at {} with newest comment {}; return factual resolution or exact product questions, without posting",
        evidence.repository,
        evidence.issue.get(),
        evidence.revision.updated_at,
        evidence
            .revision
            .last_comment
            .as_ref()
            .map_or("none", ExternalRef::as_str)
    );
    Ok(Some(Operation::LaunchWorker {
        role: Role::Gardener,
        workspace: Workspace::Isolated,
        brief: Text::new(&brief).map_err(|_| WorkflowError::IncompleteEvidence)?,
        branch: None,
        agent: evidence.agent.clone(),
    }))
}

/// No changes means an idle pass. There is never a repeated informational comment.
pub fn precheck(evidence: &Evidence, history: &History) -> Result<Precheck, WorkflowError> {
    if !evidence.coverage.is_complete()
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
    // The same predicates the plan uses, so the precheck is actionable only
    // when the plan would post the resolution or change a label.
    let pending_resolution = evidence.factual_resolution.is_some()
        && (!history.resolution_posted || label_change(evidence).is_some());
    if evidence.changed_since_last_pass || pending_resolution {
        Ok(Precheck::Actionable)
    } else {
        Ok(Precheck::Idle)
    }
}

/// Plan one pass. An answered decision is useful only at the exact revision;
/// unresolved or expired questions never promote readiness.
pub fn plan(evidence: &Evidence, history: &History) -> Result<Vec<Change>, WorkflowError> {
    if precheck(evidence, history)? == Precheck::Idle {
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
    let mut changes = Vec::new();
    if let Some(operation) = judgment_request(evidence, history)? {
        changes.push(Change::Judgment(operation));
    }
    if let Some(resolution) = &evidence.factual_resolution {
        if resolution.body.trim().is_empty() {
            return Err(WorkflowError::IncompleteEvidence);
        }
        if !history.resolution_posted {
            changes.push(Change::Mutation(GitHubAction::PostComment {
                issue: evidence.issue,
                body: crate::contracts::Text::new(&resolution.body)
                    .map_err(|_| WorkflowError::IncompleteEvidence)?,
            }));
        }
    }
    // One ask per issue revision batches its questions, so a marker keyed by
    // the revision prevents repeating it. Prior asks count toward the budget.
    let asked = u32::try_from(history.asked_at.len()).unwrap_or(u32::MAX);
    if evidence.factual_resolution.is_none()
        && evidence.pending_product_questions > 0
        && evidence.existing_decisions.is_empty()
        && !history.asked_at.contains(&evidence.revision)
        && asked < MAX_ASKS_PER_TASK
    {
        changes.push(Change::Ask { ordinal: asked });
    }
    if let Some(change) = label_change(evidence) {
        changes.push(Change::Mutation(change));
    }
    Ok(changes)
}

/// The readiness label change a resolved issue needs: add the ready label,
/// then remove needs-spec. None while a decision is unresolved, a dependency
/// is open, or a product question is pending.
fn label_change(evidence: &Evidence) -> Option<GitHubAction> {
    let unresolved = evidence
        .existing_decisions
        .iter()
        .any(|decision| decision.state != DecisionState::Answered);
    if evidence.factual_resolution.is_none()
        || unresolved
        || evidence.open_dependencies
        || evidence.pending_product_questions > 0
    {
        return None;
    }
    if !evidence.ready_label_present {
        Some(GitHubAction::SetLabel {
            issue: evidence.issue,
            label: evidence.ready_label.clone(),
            present: true,
        })
    } else if evidence.needs_spec {
        Some(GitHubAction::SetLabel {
            issue: evidence.issue,
            label: evidence.needs_spec_label.clone(),
            present: false,
        })
    } else {
        None
    }
}
