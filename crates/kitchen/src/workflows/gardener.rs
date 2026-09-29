//! Independent daily issue hygiene policy. [`install`] declares the paused
//! daily schedule and its precheck; this workflow only previews changes under
//! separate house grants.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ClaimState, Precheck, WorkflowError, known, valid_label};
use crate::{
    BackendId, ConsumerId, CredentialId, HouseId, TaskId, WorkflowId,
    contracts::{
        AttemptOutcome, Capability, CapabilityRequirements, Claimant, Clock, CloseReason,
        ContractError, Effect, ExternalRef, Fence, GitHubAction, GitHubMutation, Grant, GrantScope,
        HouseGrants, IssueNumber, LeaseTtl, Permission, Provenance, Repository, RetryPolicy, Role,
        ScheduleEffect, TaskAuthority, TaskSpec, Text, Timestamp,
    },
    id::EffectName,
    integrations::github::{
        GitHubClient, GitHubExecutor, GitHubMutationTransport, GitHubReadTransport,
        Issue as GitHubIssue, IssueComment, IssueState,
    },
    scheduling::{
        self, PrecheckTimeout, Recurrence, ScheduleSpec, TimeOfDay, Timezone, WorkflowName,
    },
    selection::ResolvedSelection,
    state::{
        EffectPlan, EffectRecord, EffectState, HouseStore, MarkerFact, MarkerKey, MarkerRecording,
        MarkerSchema, MarkerSubject, StateError, TaskState, WorkItem,
    },
};

/// The workflow name the gardener schedule runs under.
pub const WORKFLOW: &str = "gardener";

/// Bound on one precheck run: an identity check and two bounded inventory
/// reads, each limited by the client's read timeout.
const PRECHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// Longest change lookback, one week.
const MAX_LOOKBACK_HOURS: u16 = 7 * 24;
/// Longest staleness cutoff, one year.
const MAX_STALE_DAYS: u16 = 365;

/// How far back a precheck looks for changes and how old an untouched open
/// issue must be to count as stale. A lookback longer than the schedule's
/// period re-reads a missed day instead of skipping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrecheckWindow {
    lookback_hours: u16,
    stale_days: u16,
}

impl PrecheckWindow {
    /// A lookback of 1–168 hours and a staleness cutoff of 1–365 days that
    /// is at least as old as the lookback.
    ///
    /// # Errors
    /// Refuses values outside those bounds.
    pub fn new(lookback_hours: u16, stale_days: u16) -> Result<Self, WorkflowError> {
        let valid = (1..=MAX_LOOKBACK_HOURS).contains(&lookback_hours)
            && (1..=MAX_STALE_DAYS).contains(&stale_days)
            && u32::from(stale_days) * 24 >= u32::from(lookback_hours);
        if valid {
            Ok(Self {
                lookback_hours,
                stale_days,
            })
        } else {
            Err(WorkflowError::IncompleteEvidence)
        }
    }

    /// The inventory window ending at `now`. Cutoffs before the epoch start
    /// at zero.
    ///
    /// # Errors
    /// None for a validated window; kept fallible because [`Window::new`] is.
    pub fn window(self, now: Timestamp) -> Result<Window, WorkflowError> {
        let before = |hours: u64| {
            Timestamp::from_unix_millis(
                now.as_unix_millis()
                    .saturating_sub(hours.saturating_mul(3_600_000)),
            )
        };
        Window::new(
            before(u64::from(self.lookback_hours)),
            before(u64::from(self.stale_days) * 24),
        )
    }
}

/// Everything the scheduled precheck needs, rendered as its argument vector
/// for `kitchn gardener precheck`. Paths are absolute because the backend
/// runs the precheck outside any checkout. The credential file path is
/// recorded in the schedule; the token itself never is.
#[derive(Debug, Clone)]
pub struct PrecheckArgs {
    /// The installed `kitchn` executable.
    pub kitchen: PathBuf,
    /// The house.
    pub house: HouseId,
    /// The repository to inspect.
    pub repository: Repository,
    /// The authenticated GitHub login the credential must belong to.
    pub requester: ExternalRef,
    /// The house's read credential name.
    pub credential: CredentialId,
    /// The private file holding that credential.
    pub credential_file: PathBuf,
    /// The GitHub CLI executable.
    pub gh: PathBuf,
    /// The house state store holding handled-stale markers.
    pub store: PathBuf,
    /// House agent labels.
    pub labels: AgentLabels,
    /// Lookback and staleness bounds.
    pub window: PrecheckWindow,
}

impl PrecheckArgs {
    /// The precheck's argument vector.
    ///
    /// # Errors
    /// Refuses a relative or non-UTF-8 path and invalid or equal labels.
    pub fn argv(&self) -> Result<Vec<Text>, WorkflowError> {
        if !labels_valid(&self.labels) {
            return Err(WorkflowError::IncompleteEvidence);
        }
        let lookback = self.window.lookback_hours.to_string();
        let stale = self.window.stale_days.to_string();
        [
            absolute(&self.kitchen)?,
            "gardener",
            "precheck",
            "--house",
            self.house.as_str(),
            "--repository",
            self.repository.as_str(),
            "--requester",
            self.requester.as_str(),
            "--credential",
            self.credential.as_str(),
            "--credential-file",
            absolute(&self.credential_file)?,
            "--gh",
            absolute(&self.gh)?,
            "--store",
            absolute(&self.store)?,
            "--ready-label",
            &self.labels.ready,
            "--working-label",
            &self.labels.working,
            "--lookback-hours",
            &lookback,
            "--stale-days",
            &stale,
        ]
        .into_iter()
        .map(|arg| Text::new(arg).map_err(|_| WorkflowError::IncompleteEvidence))
        .collect()
    }
}

fn absolute(path: &Path) -> Result<&str, WorkflowError> {
    path.is_absolute()
        .then(|| path.to_str())
        .flatten()
        .ok_or(WorkflowError::IncompleteEvidence)
}

/// The effect that installs the gardener's daily schedule for `consumer`,
/// paused. There is no gardener path that activates it: turning it on is a
/// separate schedule effect under its own permission. `agent` is the house
/// policy's resolution for
/// [`Workflow::Gardener`](crate::house::Workflow::Gardener)'s
/// [`schedule_request`](crate::house::Workflow::schedule_request).
///
/// # Errors
/// Refuses invalid precheck arguments.
pub fn install(
    consumer: ConsumerId,
    at: TimeOfDay,
    timezone: Timezone,
    agent: ResolvedSelection,
    precheck: &PrecheckArgs,
) -> Result<Effect, WorkflowError> {
    let invalid = |_| WorkflowError::IncompleteEvidence;
    let check = scheduling::Precheck::new(
        precheck.argv()?,
        PrecheckTimeout::new(PRECHECK_TIMEOUT).map_err(invalid)?,
    )
    .map_err(invalid)?;
    let prompt = Text::new(&format!(
        "Run the Kitchen gardener hygiene pass for {} in house {}. Preview findings only; every label change, dependency link, or close needs its own house grant. Post each stale-issue report only with `kitchn gardener report-stale`, which records the issue as handled once GitHub shows the post.",
        precheck.repository, precheck.house
    ))
    .map_err(|_| WorkflowError::IncompleteEvidence)?;
    let schedule = ScheduleSpec::new(
        WorkflowName::new(WORKFLOW).map_err(invalid)?,
        consumer,
        Recurrence::Daily(at),
        timezone,
        prompt,
        agent,
    )
    .with_precheck(check);
    Ok(Effect::Schedule(ScheduleEffect::InstallDisabled {
        schedule: schedule.into(),
    }))
}

/// Required backend support before scheduling is permitted.
pub const REQUIRED_CAPABILITIES: [Capability; 4] = [
    Capability::ScheduleManage,
    Capability::SchedulePrecheck,
    Capability::ScheduleSingleConsumer,
    Capability::ScheduleRunTimeout,
];

/// One issue with complete relationship and label evidence.
#[derive(Debug, Clone)]
pub struct Issue {
    /// Number.
    pub number: IssueNumber,
    /// Provider lifecycle; unknown is never treated as open or closed.
    pub state: IssueState,
    /// Human only.
    pub human_only: bool,
    /// Durable claim observation.
    pub claim: ClaimState,
    /// Labels.
    pub labels: Vec<String>,
    /// Prose blocker.
    pub prose_blocker: Option<IssueNumber>,
    /// Linked blocker.
    pub linked_blocker: bool,
    /// Blocker open.
    pub blocker_open: bool,
    /// Parent completed.
    pub parent_completed: bool,
    /// Merged work.
    pub merged_work: bool,
    /// Duplicate of.
    pub duplicate_of: Option<IssueNumber>,
    /// Stale and not handled since its last update
    /// ([`StaleMarkers::handled`]), matching what the precheck counts.
    pub stale: bool,
}

/// House-selected labels that Kitchen may inspect for residue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLabels {
    /// Label for an unclaimed ready issue.
    pub ready: String,
    /// Label mirrored from a durable claim.
    pub working: String,
}

impl AgentLabels {
    /// Check both labels are valid and distinct.
    ///
    /// # Errors
    /// Refuses an invalid or shared label.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if labels_valid(self) {
            Ok(())
        } else {
            Err(WorkflowError::IncompleteEvidence)
        }
    }
}

fn labels_valid(labels: &AgentLabels) -> bool {
    valid_label(&labels.ready) && valid_label(&labels.working) && labels.ready != labels.working
}

/// Why the independent gardener should inspect a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    /// An issue changed in the lookback and not handled since.
    pub daily_changes: bool,
    /// A stale issue not handled since its last update.
    pub stale_issue: bool,
    /// Closed agent label.
    pub closed_agent_label: bool,
}

/// The inventory window of one precheck. `since` should be the start of the
/// last completed pass, so a failed day is re-read rather than skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    since: Timestamp,
    stale_before: Timestamp,
}

impl Window {
    /// Changes at or after `since`; open issues untouched before
    /// `stale_before` are stale.
    ///
    /// # Errors
    /// Refuses a stale cutoff after the change window starts.
    pub fn new(since: Timestamp, stale_before: Timestamp) -> Result<Self, WorkflowError> {
        if stale_before > since {
            return Err(WorkflowError::IncompleteEvidence);
        }
        Ok(Self {
            since,
            stale_before,
        })
    }
}

/// Read the changed and open issue inventory for the precheck. An issue the
/// gardener handled and nobody updated since ([`StaleMarkers::handled`])
/// counts neither as a change nor as stale, so its report does not wake the
/// schedule every day. Without `handled` markers every issue counts. A
/// partial or unrecognized read is an error, never an idle day.
pub fn signal<T: GitHubReadTransport>(
    client: &GitHubClient<T>,
    house: &HouseId,
    repo: &Repository,
    labels: &AgentLabels,
    window: Window,
    handled: Option<&StaleMarkers<'_>>,
) -> Result<Signal, WorkflowError> {
    let changed = known(client.issues_filtered(house, repo, None, Some(window.since)))?;
    let open = known(client.issues_filtered(house, repo, Some(IssueState::Open), None))?;
    let mut closed_agent_label = false;
    for issue in &changed {
        match issue.state {
            IssueState::Open => {}
            IssueState::Closed => {
                closed_agent_label |= issue
                    .labels
                    .iter()
                    .any(|label| label.name == labels.ready || label.name == labels.working);
            }
            IssueState::Unknown => return Err(WorkflowError::IncompleteEvidence),
        }
    }
    if open.iter().any(|issue| issue.state != IssueState::Open) {
        return Err(WorkflowError::IncompleteEvidence);
    }
    // Every handled marker is read and checked, not only those of the
    // issues looked at before the first unhandled one.
    let revisions = match handled {
        Some(markers) => markers.revisions(repo)?,
        None => BTreeMap::new(),
    };
    let unhandled =
        |issue: &&GitHubIssue| revisions.get(&issue.number.get()) != Some(&issue.updated_at);
    let daily_changes = changed.iter().any(|issue| unhandled(&issue));
    let stale_issue = open
        .iter()
        .filter(|issue| issue.updated_at < window.stale_before)
        .any(|issue| unhandled(&issue));
    Ok(Signal {
        daily_changes,
        stale_issue,
        closed_agent_label,
    })
}

/// Schema of the gardener marker recording a handled stale issue.
const STALE_SCHEMA: &str = "gardener.stale-handled";
/// Subject of each issue's one handled-stale marker; the revision is in the
/// fact, so handling the issue again replaces it instead of adding a marker.
const STALE_SUBJECT: &str = "stale-handled";

/// The handled-stale marker payload: the issue revision the gardener's
/// report handled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StaleHandled {
    revision: Timestamp,
    /// The applied report that handled the issue. Every marker Kitchen
    /// writes has one; a marker without one was written by the earlier
    /// unchecked recording API, proves no report, and is not honored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<ReportBinding>,
}

/// The applied report effect a handled-stale marker was recorded from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReportBinding {
    /// The report task holding the applied post.
    task: TaskId,
    /// The applied comment's URL from the post's receipt.
    receipt: ExternalRef,
}

impl StaleHandled {
    fn fact(&self) -> Result<MarkerFact, WorkflowError> {
        MarkerFact::workflow(stale_schema()?, self).map_err(|_| WorkflowError::IncompleteEvidence)
    }

    fn decode(fact: &MarkerFact) -> Result<Self, WorkflowError> {
        fact.decode(&stale_schema()?)
            .map_err(|_| WorkflowError::IncompleteEvidence)
    }

    /// The handled revision, only when an applied report backs it.
    fn handled_revision(fact: &MarkerFact) -> Result<Option<Timestamp>, WorkflowError> {
        let handled = Self::decode(fact)?;
        Ok(handled.report.map(|_| handled.revision))
    }
}

fn stale_subject() -> Result<MarkerSubject, WorkflowError> {
    ExternalRef::new(STALE_SUBJECT)
        .map(MarkerSubject::Observation)
        .map_err(|_| WorkflowError::IncompleteEvidence)
}

fn stale_schema() -> Result<MarkerSchema, WorkflowError> {
    MarkerSchema::new(STALE_SCHEMA, NonZeroU32::MIN).map_err(|_| WorkflowError::IncompleteEvidence)
}

/// Handled-stale markers in the house store: one per issue, holding the
/// issue revision the gardener's applied report handled. The issue counts as
/// handled only while its last update is still that revision, so the
/// gardener's own report does not wake the schedule and any later update
/// does. Markers are written only by [`report_stale`].
#[derive(Debug, Clone)]
pub struct StaleMarkers<'a> {
    store: &'a HouseStore,
    workflow: WorkflowId,
}

impl<'a> StaleMarkers<'a> {
    /// The gardener's markers in `store`.
    ///
    /// # Errors
    /// None in practice; the workflow id is a validated constant.
    pub fn new(store: &'a HouseStore) -> Result<Self, WorkflowError> {
        Ok(Self {
            store,
            workflow: WorkflowId::new(WORKFLOW).map_err(|_| WorkflowError::IncompleteEvidence)?,
        })
    }

    fn key(&self, repository: &Repository, issue: IssueNumber) -> Result<MarkerKey, WorkflowError> {
        Ok(MarkerKey {
            workflow: self.workflow.clone(),
            item: WorkItem::Issue {
                repository: repository.clone(),
                number: NonZeroU64::new(issue.get()).ok_or(WorkflowError::IncompleteEvidence)?,
            },
            subject: stale_subject()?,
        })
    }

    /// The handled revision of each issue number in `repository`, from one
    /// store read. Every marker at a handled-stale key must decode; a
    /// foreign fact there proves nothing and is
    /// [`WorkflowError::IncompleteEvidence`]. A marker without a report
    /// binding is skipped.
    fn revisions(
        &self,
        repository: &Repository,
    ) -> Result<BTreeMap<u64, Timestamp>, WorkflowError> {
        let subject = stale_subject()?;
        self.store
            .markers(&self.workflow)
            .map_err(|_| WorkflowError::PrecheckFailed)?
            .iter()
            .filter(|marker| marker.key().subject == subject)
            .filter_map(|marker| match &marker.key().item {
                WorkItem::Issue {
                    repository: owner,
                    number,
                } if owner == repository => Some((number.get(), marker.fact())),
                WorkItem::Issue { .. }
                | WorkItem::PullRequest { .. }
                | WorkItem::Repository { .. }
                | WorkItem::Resource { .. }
                | WorkItem::Task { .. } => None,
            })
            .filter_map(|(number, fact)| {
                StaleHandled::handled_revision(fact)
                    .map(|revision| revision.map(|revision| (number, revision)))
                    .transpose()
            })
            .collect()
    }

    /// Whether `issue` was handled and has not been updated since: its last
    /// update is still the revision an applied report was recorded for.
    ///
    /// # Errors
    /// A failed read is [`WorkflowError::PrecheckFailed`]; a marker of another
    /// kind at this key proves nothing and is
    /// [`WorkflowError::IncompleteEvidence`].
    pub fn handled(
        &self,
        repository: &Repository,
        issue: IssueNumber,
        updated_at: Timestamp,
    ) -> Result<bool, WorkflowError> {
        let key = self.key(repository, issue)?;
        let Some(marker) = self
            .store
            .marker(&key)
            .map_err(|_| WorkflowError::PrecheckFailed)?
        else {
            return Ok(false);
        };
        Ok(StaleHandled::handled_revision(marker.fact())? == Some(updated_at))
    }

    /// The revision an applied report last handled `issue` at, if any.
    fn revision(
        &self,
        repository: &Repository,
        issue: IssueNumber,
    ) -> Result<Option<Timestamp>, WorkflowError> {
        let key = self.key(repository, issue)?;
        self.store
            .marker(&key)
            .map_err(|_| WorkflowError::PrecheckFailed)?
            .map(|marker| StaleHandled::handled_revision(marker.fact()))
            .transpose()
            .map(Option::flatten)
    }

    /// Record the applied report in `handled`. Recording the same revision
    /// again changes nothing, and a newer revision replaces the issue's
    /// marker in place, which a full marker table still allows. A newer
    /// revision already recorded is [`WorkflowError::DecisionMismatch`]; a
    /// marker without a report binding is replaced.
    fn record_report(
        &self,
        repository: &Repository,
        issue: IssueNumber,
        handled: StaleHandled,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> crate::Result<MarkerRecording> {
        let key = self.key(repository, issue)?;
        let fact = handled.fact()?;
        let Some(current) = self.store.marker(&key)? else {
            return self.store.record_marker(key, fact, recorded_by, now);
        };
        if StaleHandled::handled_revision(current.fact())?
            .is_some_and(|recorded| recorded > handled.revision)
        {
            return Err(WorkflowError::DecisionMismatch.into());
        }
        self.store
            .supersede_marker(&key, current.fact(), fact, recorded_by, now)
    }
}

/// What one stale report acts under: the house store, GitHub reads and
/// writes for the stale issue's repository, and the scheduled claimant.
pub struct StaleReportPass<'a, R, M> {
    /// The house's state store.
    pub store: &'a HouseStore,
    /// Reads of the stale issue.
    pub client: &'a GitHubClient<R>,
    /// The executor the report comment is posted through.
    pub executor: &'a GitHubExecutor<M>,
    /// The house's grants.
    pub grants: &'a HouseGrants,
    /// The comment grant each report task is delegated. Keep it the same for
    /// an issue; a changed grant is refused as a changed task.
    pub authority: Grant,
    /// Kitchen and house guidance revisions recorded on each task.
    pub provenance: Provenance,
    /// The claimant posting the report.
    pub claimant: &'a Claimant,
    /// The claim's lease.
    pub ttl: LeaseTtl,
    /// Time source.
    pub clock: &'a dyn Clock,
}

/// The result of [`report_stale`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReportOutcome {
    /// The report is applied and the issue is recorded as handled at
    /// `revision`.
    Recorded {
        /// The applied comment's URL.
        receipt: ExternalRef,
        /// The handled revision: the issue's last update after the report
        /// when the report is its latest activity, otherwise the revision
        /// the report was decided on.
        revision: Timestamp,
        /// The issue changed after the report was decided on, other than by
        /// the report itself. That activity stays unhandled and wakes the
        /// next precheck.
        later_activity: bool,
    },
    /// The issue is already recorded as handled at its current revision;
    /// nothing was posted.
    AlreadyHandled,
    /// The report did not apply, so nothing was recorded and the issue keeps
    /// waking the schedule. An uncertain post is looked up on the next run,
    /// never posted twice.
    NotPosted(Box<EffectRecord>),
}

/// Bound on report attempts for one issue revision; each run claims one.
const REPORT_ATTEMPTS: u32 = 16;
/// Bound on the time one issue revision's report task keeps retrying.
const REPORT_ELAPSED: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Name of the report effect, one per report task.
const REPORT_EFFECT: &str = "gardener-stale-report";
/// Prefix of a report effect's basis, which holds the issue revision the
/// report was decided on in Unix milliseconds.
const OBSERVED_BASIS: &str = "gardener-issue-revision:";

/// Post the gardener's stale report on `issue` and record the issue as
/// handled once the post is applied.
///
/// The report is a comment posted through the house's persisted-effect path
/// ([`crate::state::run_effect`]) under one task per issue and previously
/// handled revision, so its intent is stored before the post and GitHub is
/// read back before it counts as applied. Only that applied effect records
/// the marker, which keeps the task and the comment URL; no other comment on
/// the issue counts.
///
/// The issue revision read before the post is persisted with the effect as
/// the revision the report was decided on. After the post the issue and its
/// comments are read again. When the report comment is the issue's latest
/// activity, the issue's new last update is recorded, so the report does not
/// wake the next precheck. When anything else changed the issue after that
/// decision, only the decided revision is recorded, so the later activity
/// wakes the next precheck.
///
/// A run is restart-safe: a run after a crash finds the same task, looks an
/// unresolved post up instead of posting again, and records the marker from
/// an applied post it finds. A run after the marker was recorded finds the
/// issue handled and posts nothing. A post that did not apply records
/// nothing ([`StaleReportOutcome::NotPosted`]).
///
/// # Errors
/// [`WorkflowError::DecisionMismatch`] for a closed issue,
/// [`WorkflowError::IncompleteEvidence`] and [`WorkflowError::PrecheckFailed`]
/// for incomplete or failed reads, integration refusals of the comment,
/// and task creation, claim, effect, and marker refusals, such as a claim
/// another run holds.
pub fn report_stale<R: GitHubReadTransport, M: GitHubMutationTransport>(
    pass: &StaleReportPass<'_, R, M>,
    repository: &Repository,
    issue: IssueNumber,
    body: Text,
) -> crate::Result<StaleReportOutcome> {
    let markers = StaleMarkers::new(pass.store)?;
    let house = pass.grants.house();
    let current = open_issue(pass.client, house, repository, issue)?;
    let observed = current.updated_at;
    if markers.handled(repository, issue, observed)? {
        return Ok(StaleReportOutcome::AlreadyHandled);
    }
    let prior = markers.revision(repository, issue)?;
    let task = report_task(repository, issue, prior)?;
    // An existing task is continued as created, so a later guidance
    // revision cannot strand an unfinished report.
    let existing = match pass.store.task(&task) {
        Err(crate::Error::State(StateError::TaskNotFound(_))) => {
            pass.store.create_task(
                TaskSpec {
                    id: task.clone(),
                    role: Role::Gardener,
                    repository: Some(repository.clone()),
                    authority: TaskAuthority::delegate(pass.grants, [pass.authority.clone()])?,
                    retry: RetryPolicy::new(REPORT_ATTEMPTS, REPORT_ELAPSED)?,
                    provenance: pass.provenance.clone(),
                    requires: CapabilityRequirements::new(),
                    resources: BTreeSet::new(),
                    agent: None,
                    work_type: None,
                },
                pass.claimant,
                pass.clock.now(),
            )?;
            pass.store.task(&task)?
        }
        other => other?,
    };
    let record = match existing.state() {
        // A report task settles only after its post applied; the marker may
        // still be due after a crash.
        TaskState::Settled { .. } => {
            applied_report(existing.effects()).ok_or(WorkflowError::DecisionMismatch)?
        }
        TaskState::Open | TaskState::Claimed { .. } => {
            let fence = claim(pass, &task)?;
            let posted = post(pass, &task, fence, repository, issue, body, observed);
            let settled = matches!(pass.store.task(&task)?.state(), TaskState::Settled { .. });
            if !settled {
                pass.store.relinquish(&task, fence, pass.clock.now())?;
            }
            let record = posted?;
            if !is_applied(&record) {
                return Ok(StaleReportOutcome::NotPosted(Box::new(record)));
            }
            record
        }
    };
    let EffectState::Applied { receipt, .. } = record.state() else {
        return Err(WorkflowError::IncompleteEvidence.into());
    };
    let receipt = receipt.reference().clone();
    let decided = decided_revision(&record)?;
    // A closed issue needs no marker.
    let reported = open_issue(pass.client, house, repository, issue)?;
    let comments = known(pass.client.comments(house, repository, issue))?;
    let (revision, later_activity) =
        handled_revision(decided, reported.updated_at, &comments, &receipt)?;
    markers.record_report(
        repository,
        issue,
        StaleHandled {
            revision,
            report: Some(ReportBinding {
                task,
                receipt: receipt.clone(),
            }),
        },
        pass.claimant,
        pass.clock.now(),
    )?;
    Ok(StaleReportOutcome::Recorded {
        receipt,
        revision,
        later_activity,
    })
}

fn observed_basis(observed: Timestamp) -> crate::Result<ExternalRef> {
    Ok(ExternalRef::new(&format!(
        "{OBSERVED_BASIS}{}",
        observed.as_unix_millis()
    ))?)
}

/// The issue revision the applied report was decided on, from its basis.
fn decided_revision(record: &EffectRecord) -> Result<Timestamp, WorkflowError> {
    record
        .basis()
        .and_then(|basis| basis.as_str().strip_prefix(OBSERVED_BASIS))
        .and_then(|millis| millis.parse().ok())
        .map(Timestamp::from_unix_millis)
        .ok_or(WorkflowError::IncompleteEvidence)
}

/// The revision to record for a report decided at `decided`, given the
/// issue's last update `reread` and its `comments` read after the post, and
/// whether the issue has activity after the decision other than the report.
///
/// The report is the latest activity when the issue was last updated no
/// later than the report comment's creation and no other comment changed
/// after `decided`; then `reread` is recorded. Otherwise `decided` is, so
/// the later activity stays unhandled. A report comment missing from the
/// read, such as a deleted one, counts as later activity.
///
/// # Errors
/// [`WorkflowError::IncompleteEvidence`] for a receipt that names no comment.
fn handled_revision(
    decided: Timestamp,
    reread: Timestamp,
    comments: &[IssueComment],
    receipt: &ExternalRef,
) -> Result<(Timestamp, bool), WorkflowError> {
    let id: u64 = receipt
        .as_str()
        .rsplit_once("#issuecomment-")
        .and_then(|(_, id)| id.parse().ok())
        .ok_or(WorkflowError::IncompleteEvidence)?;
    let report_is_latest = comments
        .iter()
        .find(|comment| comment.id == id)
        .is_some_and(|report| reread <= report.created_at)
        && comments
            .iter()
            .all(|comment| comment.id == id || comment.updated_at <= decided);
    if report_is_latest {
        Ok((reread, false))
    } else {
        Ok((decided, true))
    }
}

fn open_issue<R: GitHubReadTransport>(
    client: &GitHubClient<R>,
    house: &HouseId,
    repository: &Repository,
    issue: IssueNumber,
) -> Result<GitHubIssue, WorkflowError> {
    let current = known(client.issue(house, repository, issue))?;
    match current.state {
        IssueState::Open => Ok(current),
        IssueState::Closed => Err(WorkflowError::DecisionMismatch),
        IssueState::Unknown => Err(WorkflowError::IncompleteEvidence),
    }
}

/// The report task for `issue` since its `prior` handled revision. It stays
/// the same until a report is recorded, so a restarted run continues it.
fn report_task(
    repository: &Repository,
    issue: IssueNumber,
    prior: Option<Timestamp>,
) -> crate::Result<TaskId> {
    let mut digest = Sha256::new();
    let prior = prior.map_or(0, Timestamp::as_unix_millis).to_string();
    let issue = issue.get().to_string();
    for part in [repository.as_str(), &issue, &prior] {
        // Length-prefixed, so no two part lists share a digest input.
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    let mut id = String::from("gardener-stale-");
    for byte in digest.finalize().iter().take(16) {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(TaskId::new(&id)?)
}

/// Claim `task`, taking over a claim whose lease expired, and continue its
/// attempt or start the next.
fn claim<R, M>(pass: &StaleReportPass<'_, R, M>, task: &TaskId) -> crate::Result<Fence> {
    let now = pass.clock.now();
    let lease = match pass.store.claim(task, pass.claimant, pass.ttl, now) {
        Err(crate::Error::State(StateError::LeaseExpired { .. })) => {
            pass.store.take_over(task, pass.claimant, pass.ttl, now)?
        }
        other => other?,
    };
    let fence = lease.fence();
    let started = pass
        .store
        .continue_attempt(task, fence, now)
        .and_then(|running| match running {
            Some(_) => Ok(()),
            None => pass.store.start_attempt(task, fence, now).map(|_| ()),
        });
    if let Err(error) = started {
        pass.store.relinquish(task, fence, now)?;
        return Err(error);
    }
    Ok(fence)
}

/// Post the report under the claimed `task`, or find the post an earlier
/// run applied, and settle the task once it is applied.
fn post<R, M: GitHubMutationTransport>(
    pass: &StaleReportPass<'_, R, M>,
    task: &TaskId,
    fence: Fence,
    repository: &Repository,
    issue: IssueNumber,
    body: Text,
    observed: Timestamp,
) -> crate::Result<EffectRecord> {
    crate::state::reconcile(pass.store, pass.executor, task, fence, pass.clock)?;
    let record = pass.store.task(task)?;
    let record = match applied_report(record.effects()) {
        Some(applied) => applied,
        None => {
            // An earlier post is resubmitted as recorded, with the revision
            // it was decided on, so a changed body cannot post a second,
            // different report.
            let (effect, basis) = match record
                .effects()
                .iter()
                .rev()
                .find(|effect| effect.name().as_str() == REPORT_EFFECT)
            {
                Some(earlier) => (
                    earlier.request().effect().clone(),
                    observed_basis(decided_revision(earlier)?)?,
                ),
                None => (
                    Effect::GitHub(pass.executor.effect(GitHubMutation {
                        repository: repository.clone(),
                        action: GitHubAction::PostComment { issue, body },
                    })?),
                    observed_basis(observed)?,
                ),
            };
            crate::state::run_effect(
                pass.store,
                pass.executor,
                pass.grants,
                EffectPlan {
                    task: task.clone(),
                    fence,
                    name: EffectName::new(REPORT_EFFECT)?,
                    decided_at: record.evidence().revision(),
                    effect,
                    consent: None,
                    basis: Some(basis),
                },
                pass.clock,
            )?
        }
    };
    if is_applied(&record) {
        let now = pass.clock.now();
        if let Some(attempt) = pass.store.continue_attempt(task, fence, now)? {
            pass.store
                .finish_attempt(task, fence, attempt, AttemptOutcome::Succeeded, now)?;
        }
    }
    Ok(record)
}

/// The task's applied report post, if any.
fn applied_report(effects: &[EffectRecord]) -> Option<EffectRecord> {
    effects
        .iter()
        .find(|effect| effect.name().as_str() == REPORT_EFFECT && is_applied(effect))
        .cloned()
}

const fn is_applied(record: &EffectRecord) -> bool {
    match record.state() {
        EffectState::Applied { .. } => true,
        EffectState::Intended
        | EffectState::Uncertain { .. }
        | EffectState::NotApplied { .. }
        | EffectState::Unresolvable { .. }
        | EffectState::Waived { .. } => false,
    }
}

/// Evidence that the house holds a standing [`Permission::CloseIssue`] grant
/// for one repository. No other permission implies it, and consent-only
/// policy limits do not count for unattended runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseAuthority {
    repository: Repository,
}

impl CloseAuthority {
    /// The authority for `repository` on `destination`, or `None` without a
    /// standing grant.
    ///
    /// # Errors
    /// Refuses policy that names two credentials for the same grant.
    pub fn from_grants(
        grants: &HouseGrants,
        repository: &Repository,
        destination: &BackendId,
    ) -> Result<Option<Self>, WorkflowError> {
        let scope = GrantScope::Repository(repository.clone());
        let credential = match grants.permitted(Permission::CloseIssue, &scope, destination) {
            Ok(credential) => credential,
            Err(ContractError::AuthorityExpansion { .. }) => return Ok(None),
            Err(_) => return Err(WorkflowError::DecisionMismatch),
        };
        let grant = Grant::repository(
            Permission::CloseIssue,
            repository.clone(),
            destination.clone(),
            credential,
        );
        Ok(grants.covers(&grant).then(|| Self {
            repository: repository.clone(),
        }))
    }
}

/// Distinct gardener precheck. Errors remain errors, not idle ticks.
pub fn precheck(signal: Result<Signal, WorkflowError>) -> Result<Precheck, WorkflowError> {
    let signal = signal?;
    if signal.daily_changes || signal.stale_issue || signal.closed_agent_label {
        Ok(Precheck::Actionable)
    } else {
        Ok(Precheck::Idle)
    }
}

/// A proposed action and its separate permission boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A mutation suitable for a fresh-read and authority check.
    Mutation(GitHubAction),
    /// Human assessment is needed; no automatic close or duplicate mark.
    Review {
        /// Candidate issue.
        issue: IssueNumber,
        /// Reason to ask for review.
        reason: ReviewReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Reason for review without an automatic close.
pub enum ReviewReason {
    /// Completed parent.
    CompletedParent,
    /// Merged work.
    MergedWork,
    /// Duplicate.
    Duplicate,
    /// Stale.
    Stale,
}

/// Preview only actionable findings. A closed issue's agent label is removed
/// only when it has no live claim. Human-only and claimed work is untouched.
/// Completed, merged, and duplicate issues are proposed for closure only with
/// `close` authority for `repository`; stale issues always need review.
pub fn plan(
    repository: &Repository,
    issues: &[Issue],
    labels: &AgentLabels,
    close: Option<&CloseAuthority>,
) -> Result<Vec<Finding>, WorkflowError> {
    if !labels_valid(labels) {
        return Err(WorkflowError::IncompleteEvidence);
    }
    if close.is_some_and(|authority| &authority.repository != repository) {
        return Err(WorkflowError::DecisionMismatch);
    }
    let mut findings = Vec::new();
    for issue in issues {
        if issue.state == IssueState::Unknown {
            return Err(WorkflowError::IncompleteEvidence);
        }
        if issue.claim == ClaimState::Unknown {
            return Err(WorkflowError::IncompleteEvidence);
        }
        if issue.human_only || issue.claim == ClaimState::ClaimedByOther {
            continue;
        }
        if issue.state == IssueState::Closed {
            for label in &issue.labels {
                if label == &labels.ready || label == &labels.working {
                    findings.push(Finding::Mutation(GitHubAction::SetLabel {
                        issue: issue.number,
                        label: label.clone(),
                        present: false,
                    }));
                }
            }
            continue;
        }
        if let Some(blocker) = issue.prose_blocker
            && issue.blocker_open
            && !issue.linked_blocker
            && blocker != issue.number
        {
            findings.push(Finding::Mutation(GitHubAction::LinkDependency {
                issue: issue.number,
                blocker,
            }));
        }
        let closure = match issue.duplicate_of {
            Some(original) if original != issue.number => Some(CloseReason::Duplicate(original)),
            Some(_) => return Err(WorkflowError::IncompleteEvidence),
            None if issue.parent_completed || issue.merged_work => Some(CloseReason::Completed),
            None => None,
        };
        if let (Some(reason), Some(_)) = (closure, close) {
            findings.push(Finding::Mutation(GitHubAction::CloseIssue {
                repository: repository.clone(),
                number: issue.number,
                reason,
            }));
            continue;
        }
        for (active, reason) in [
            (issue.parent_completed, ReviewReason::CompletedParent),
            (issue.merged_work, ReviewReason::MergedWork),
            (issue.duplicate_of.is_some(), ReviewReason::Duplicate),
            (issue.stale, ReviewReason::Stale),
        ] {
            if active {
                findings.push(Finding::Review {
                    issue: issue.number,
                    reason,
                });
            }
        }
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HolderId,
        integrations::github::User,
        state::{Limit, MAX_MARKERS, StoreOptions},
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const DECIDED: Timestamp = Timestamp::from_unix_millis(1_000_000);
    const POSTED: Timestamp = Timestamp::from_unix_millis(2_000_000);
    const LATER: Timestamp = Timestamp::from_unix_millis(3_000_000);

    fn comment(id: u64, created: Timestamp, updated: Timestamp) -> IssueComment {
        IssueComment {
            id,
            user: User {
                login: "sample-bot".into(),
            },
            body: "comment".into(),
            created_at: created,
            updated_at: updated,
        }
    }

    fn receipt() -> TestResult<ExternalRef> {
        Ok(ExternalRef::new(
            "https://github.com/sample/project/issues/3#issuecomment-77",
        )?)
    }

    #[test]
    fn the_report_as_latest_activity_is_recorded_at_the_reread() -> TestResult {
        let earlier = comment(5, DECIDED, DECIDED);
        let report = comment(77, POSTED, POSTED);
        assert_eq!(
            handled_revision(DECIDED, POSTED, &[earlier, report.clone()], &receipt()?)?,
            (POSTED, false)
        );
        // An issue left at the decided revision is handled there.
        assert_eq!(
            handled_revision(DECIDED, DECIDED, &[report], &receipt()?)?,
            (DECIDED, false)
        );
        Ok(())
    }

    #[test]
    fn activity_after_the_decision_keeps_the_decided_revision() -> TestResult {
        let report = comment(77, POSTED, POSTED);
        // Another update after the post, such as a label change.
        assert_eq!(
            handled_revision(DECIDED, LATER, std::slice::from_ref(&report), &receipt()?)?,
            (DECIDED, true)
        );
        // Another comment after the decision, even one before the post.
        let answer = comment(80, DECIDED, Timestamp::from_unix_millis(1_500_000));
        assert_eq!(
            handled_revision(DECIDED, POSTED, &[answer, report], &receipt()?)?,
            (DECIDED, true)
        );
        // The report comment is gone, as after a deletion.
        assert_eq!(
            handled_revision(DECIDED, POSTED, &[], &receipt()?)?,
            (DECIDED, true)
        );
        Ok(())
    }

    #[test]
    fn a_receipt_naming_no_comment_is_incomplete_evidence() -> TestResult {
        let issue = ExternalRef::new("https://github.com/sample/project/issues/3")?;
        assert_eq!(
            handled_revision(DECIDED, POSTED, &[comment(77, POSTED, POSTED)], &issue),
            Err(WorkflowError::IncompleteEvidence)
        );
        Ok(())
    }

    fn store() -> TestResult<(tempfile::TempDir, HouseStore)> {
        let dir = tempfile::tempdir()?;
        let store = HouseStore::initialize(
            dir.path().join("house"),
            HouseId::new("sample")?,
            StoreOptions::default(),
        )?;
        Ok((dir, store))
    }

    fn reported(revision: Timestamp) -> TestResult<StaleHandled> {
        Ok(StaleHandled {
            revision,
            report: Some(ReportBinding {
                task: TaskId::new("gardener-stale-report")?,
                receipt: receipt()?,
            }),
        })
    }

    fn tick() -> TestResult<Claimant> {
        Ok(Claimant::scheduled(HolderId::new("gardener-tick")?))
    }

    fn project() -> TestResult<Repository> {
        Ok(Repository::new("sample/project")?)
    }

    #[test]
    fn a_handled_issue_keeps_one_marker_at_its_newest_revision() -> TestResult {
        let (_dir, store) = store()?;
        let markers = StaleMarkers::new(&store)?;
        let issue = IssueNumber::new(3)?;
        let (project, tick) = (project()?, tick()?);
        let record = |revision| -> TestResult<crate::Result<MarkerRecording>> {
            Ok(markers.record_report(&project, issue, reported(revision)?, &tick, LATER))
        };
        assert!(matches!(record(DECIDED)?, Ok(MarkerRecording::Recorded(_))));
        assert!(matches!(
            record(DECIDED)?,
            Ok(MarkerRecording::AlreadyRecorded(_))
        ));
        assert!(matches!(
            record(POSTED)?,
            Ok(MarkerRecording::Superseded(_))
        ));
        assert!(markers.handled(&project, issue, POSTED)?);
        assert!(!markers.handled(&project, issue, DECIDED)?);
        // An older revision never replaces the newer handled one.
        assert!(matches!(
            record(DECIDED)?,
            Err(crate::Error::Workflow(WorkflowError::DecisionMismatch))
        ));
        assert!(markers.handled(&project, issue, POSTED)?);
        assert_eq!(store.markers(&markers.workflow)?.len(), 1);
        Ok(())
    }

    #[test]
    fn a_marker_without_a_report_is_not_honored_and_is_replaced() -> TestResult {
        let (_dir, store) = store()?;
        let markers = StaleMarkers::new(&store)?;
        let issue = IssueNumber::new(3)?;
        // As the removed unchecked `record` wrote it.
        let legacy = StaleHandled {
            revision: LATER,
            report: None,
        };
        store.record_marker(
            markers.key(&project()?, issue)?,
            legacy.fact()?,
            &tick()?,
            DECIDED,
        )?;
        assert!(!markers.handled(&project()?, issue, LATER)?);
        assert!(markers.revisions(&project()?)?.is_empty());
        assert_eq!(markers.revision(&project()?, issue)?, None);
        // A report replaces it, even at an older revision.
        assert!(matches!(
            markers.record_report(&project()?, issue, reported(POSTED)?, &tick()?, LATER),
            Ok(MarkerRecording::Superseded(_))
        ));
        assert!(markers.handled(&project()?, issue, POSTED)?);
        Ok(())
    }

    #[test]
    fn a_full_marker_table_still_moves_a_handled_issue_forward() -> TestResult {
        let (dir, store) = store()?;
        let issue = IssueNumber::new(3)?;
        StaleMarkers::new(&store)?.record_report(
            &project()?,
            issue,
            reported(DECIDED)?,
            &tick()?,
            DECIDED,
        )?;
        drop(store);
        // Fill the house's shared marker table to its bound.
        let path = dir.path().join("house").join("state.json");
        let mut state: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let markers = state
            .get_mut("markers")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or("markers")?;
        let template = markers.first().cloned().ok_or("marker")?;
        for number in 1_000..(1_000 + MAX_MARKERS - markers.len()) {
            let mut marker = template.clone();
            marker["key"]["item"]["number"] = number.into();
            markers.push(marker);
        }
        std::fs::write(&path, serde_json::to_vec(&state)?)?;
        let store = HouseStore::open(
            dir.path().join("house"),
            HouseId::new("sample")?,
            StoreOptions::default(),
        )?;
        let markers = StaleMarkers::new(&store)?;
        // The handled issue moves to its new revision in place.
        assert!(matches!(
            markers.record_report(&project()?, issue, reported(POSTED)?, &tick()?, LATER),
            Ok(MarkerRecording::Superseded(_))
        ));
        assert!(markers.handled(&project()?, issue, POSTED)?);
        // A new issue cannot be recorded, and stays unhandled.
        let other = IssueNumber::new(9)?;
        assert!(matches!(
            markers.record_report(&project()?, other, reported(POSTED)?, &tick()?, LATER),
            Err(crate::Error::State(StateError::CapacityExceeded {
                limit: Limit::Markers
            }))
        ));
        assert!(!markers.handled(&project()?, other, POSTED)?);
        Ok(())
    }
}
