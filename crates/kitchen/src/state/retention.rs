//! One retention policy for the house store's bounded, shared tables.
//!
//! Every workflow writes markers and tasks into the same store, and each
//! table holds at most [`MAX_MARKERS`] or [`MAX_TASKS`] entries. Without
//! retention a long-running house eventually refuses new work in every
//! workflow at once. This module decides, in one place, which records no
//! longer matter:
//!
//! - A marker whose rule is [`MarkerRule::UntilItemGone`] is retired once
//!   its work item is observed closed or no longer listed.
//! - A marker whose rule is [`MarkerRule::LatestSubject`] keeps only the
//!   newest subject per workflow, item, and schema; older subjects retire.
//!   All of them retire once the item is gone.
//! - A marker whose rule is [`MarkerRule::UntilTaskSettled`] is retired once
//!   the task it is about settled or is gone from the store.
//! - A settled task retires once it has been settled for the policy's window
//!   and its identity can never be created again: a budget window that has
//!   passed, or the repair rounds of a pull request that closed.
//!
//! - Attempt usage records ([`crate::state::AttemptUsage`]) and the events
//!   human time is derived from live on their task. They never keep a task
//!   and retire with it; a report counts them, so a preview shows what usage
//!   evidence a pass would remove.
//!
//! - An acknowledged house mailbox message retires once its attempt ended
//!   or its task settled or left the store: its worker is gone, so nobody
//!   reads its answer. An unacknowledged message is always kept, since the
//!   coordinator has not handled it.
//!
//! Everything else is kept, including every marker dedupe still needs
//! (asked questions, deliberation threads, reports owed to an owner) and
//! every task of an unknown family. Intake markers follow their owner's
//! rules instead, in the same pass and transaction: settled reservations fold
//! into a bounded dedupe window (see [`crate::workflows::intake`]).
//!
//! Retention fails safe. It retires only on positive evidence in the
//! caller's [`Inventory`]: an item observed gone, or a resource absent from
//! a complete backend listing. An item that was not observed, or whose
//! observation was partial, keeps everything that depends on it. A task is
//! never retired while it is unsettled, has an effect whose outcome is not
//! established, holds a failed write no person has acknowledged, created a
//! resource that is still listed or was not observed, or is named by a
//! marker that stays.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    ops::Bound,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, HouseId, TaskId, WorkflowId,
    contracts::{ExternalRef, IssueNumber, ResourceRef, Settlement, Timestamp},
    integrations::github::{GitHubClient, GitHubReadTransport, IssueState, Observation},
    state::{
        AttemptState, AttemptUsage, EffectState, MAX_CONSUMERS, MAX_MARKERS, MAX_TASKS, MarkerFact,
        MarkerKey, MarkerSchema, StateError, TaskRecord, TaskState, WorkItem, WorkflowMarker,
        mailbox::Mailbox,
    },
    workflows::{budget, intake::LedgerCompaction, pickup, repair},
};

/// The shortest settled-task window a policy accepts: the longest schedule
/// budget window, so a budget task retires only after its window ended.
pub const MIN_TASK_WINDOW: Duration = Duration::from_secs(31 * 24 * 60 * 60);

/// Usage, as a percentage of a table's limit, at which the table is
/// reported as near its limit.
pub const CAPACITY_WARNING_PERCENT: usize = 80;

/// How long settled tasks stay before retention may remove them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    task_window: Duration,
}

impl RetentionPolicy {
    /// A policy that keeps settled tasks for `task_window`.
    ///
    /// # Errors
    /// [`StateError::RetentionWindowTooShort`] below [`MIN_TASK_WINDOW`].
    pub const fn new(task_window: Duration) -> Result<Self, StateError> {
        if task_window.as_millis() < MIN_TASK_WINDOW.as_millis() {
            return Err(StateError::RetentionWindowTooShort);
        }
        Ok(Self { task_window })
    }

    /// How long settled tasks stay.
    #[must_use]
    pub const fn task_window(&self) -> Duration {
        self.task_window
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            task_window: MIN_TASK_WINDOW,
        }
    }
}

/// Whether an observed item still exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Presence {
    /// Open, or still listed.
    Present,
    /// Closed, merged, or deleted.
    Gone,
}

/// What the caller observed outside the store for one retention pass.
///
/// Record only complete, positive evidence: an item's presence from a
/// successful lookup of that item, and a backend's listing only when the
/// backend returned all of it. Anything not recorded here is treated as
/// present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    items: BTreeMap<WorkItem, Presence>,
    listings: BTreeMap<BackendId, BTreeSet<ResourceRef>>,
    last_lookup: Option<WorkItem>,
}

impl Inventory {
    /// Nothing observed: a pass against it retires only what needs no
    /// outside evidence.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: BTreeMap::new(),
            listings: BTreeMap::new(),
            last_lookup: None,
        }
    }

    /// Record a successful lookup of one issue, pull request, or other
    /// item. A later observation of the same item replaces it.
    pub fn observe(&mut self, item: WorkItem, presence: Presence) {
        self.items.insert(item, presence);
    }

    /// Record a backend's complete resource listing. A resource of that
    /// backend absent from `listed` counts as gone.
    pub fn list_backend(
        &mut self,
        backend: BackendId,
        listed: impl IntoIterator<Item = ResourceRef>,
    ) {
        self.listings.insert(backend, listed.into_iter().collect());
    }

    /// Look up the issues and pull requests of `subjects` through the
    /// house's forge client, at most `limit` of them in
    /// [`RetentionSubjects::lookup_order`], and record every answer the
    /// forge gave completely. An unavailable, partial, or unrecognized
    /// answer records nothing, so its item keeps everything. The last item
    /// looked up becomes the store's cursor when [`HouseStore::retain`]
    /// applies this inventory, so the next pass continues after it. Returns
    /// how many items were recorded.
    ///
    /// [`HouseStore::retain`]: crate::state::HouseStore::retain
    pub fn observe_forge<T: GitHubReadTransport>(
        &mut self,
        client: &GitHubClient<T>,
        house: &HouseId,
        subjects: &RetentionSubjects,
        limit: usize,
    ) -> usize {
        let mut observed = 0;
        for item in subjects.lookup_order().take(limit) {
            self.last_lookup = Some(item.clone());
            let state = match item {
                WorkItem::Issue { repository, number } => {
                    let Ok(number) = IssueNumber::new(number.get()) else {
                        continue;
                    };
                    match client.issue(house, repository, number) {
                        Observation::Known(issue) => issue.state,
                        Observation::Unavailable(_) | Observation::Unknown => continue,
                    }
                }
                WorkItem::PullRequest { repository, number } => {
                    let Ok(number) = IssueNumber::new(number.get()) else {
                        continue;
                    };
                    match client.pull_request(house, repository, number) {
                        Observation::Known(pull) => pull.state,
                        Observation::Unavailable(_) | Observation::Unknown => continue,
                    }
                }
                WorkItem::Resource { .. } | WorkItem::Repository { .. } | WorkItem::Task { .. } => {
                    continue;
                }
            };
            let presence = match state {
                IssueState::Open => Presence::Present,
                IssueState::Closed => Presence::Gone,
                IssueState::Unknown => continue,
            };
            self.observe(item.clone(), presence);
            observed += 1;
        }
        observed
    }

    /// The last item [`Self::observe_forge`] looked up, answered or not.
    #[must_use]
    pub const fn last_lookup(&self) -> Option<&WorkItem> {
        self.last_lookup.as_ref()
    }

    fn presence(&self, item: &WorkItem) -> Option<Presence> {
        match item {
            WorkItem::Resource { resource } => self.resource(resource),
            WorkItem::Issue { .. }
            | WorkItem::PullRequest { .. }
            | WorkItem::Repository { .. }
            | WorkItem::Task { .. } => self.items.get(item).copied(),
        }
    }

    fn resource(&self, resource: &ResourceRef) -> Option<Presence> {
        if let Some(listed) = self.listings.get(&resource.backend) {
            return Some(if listed.contains(resource) {
                Presence::Present
            } else {
                Presence::Gone
            });
        }
        self.items
            .get(&WorkItem::Resource {
                resource: resource.clone(),
            })
            .copied()
    }

    fn gone(&self, item: &WorkItem) -> bool {
        self.presence(item) == Some(Presence::Gone)
    }
}

/// How the store treats one marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MarkerRule {
    /// Dedupe needs it for as long as the house exists.
    Keep,
    /// Retired once its work item is observed gone.
    UntilItemGone,
    /// Only the newest subject per workflow, item, and schema matters.
    LatestSubject,
    /// Its owner compacts it; retention never removes it directly.
    Compacted,
    /// Retired once the task it is about settled or was retired. A marker
    /// about anything other than a task is kept.
    UntilTaskSettled,
}

/// The rule for each workflow marker schema. A schema not listed here is
/// kept, so a new workflow's facts survive until it states its own rule.
const MARKER_RULES: &[(&str, MarkerRule)] = &[
    // The gate's fix and hand-over budgets count every head of an open pull
    // request, so its verdicts stay until the pull request closes.
    ("gate.verdict", MarkerRule::UntilItemGone),
    ("gate.subject-budget", MarkerRule::UntilItemGone),
    ("gate.base-read-failures", MarkerRule::LatestSubject),
    ("ready-report", MarkerRule::LatestSubject),
    // Superseded in place per issue, so one per open issue.
    ("gardener.stale-handled", MarkerRule::UntilItemGone),
    ("triage.resolution", MarkerRule::UntilItemGone),
    ("cleanup.approval", MarkerRule::UntilItemGone),
    ("event.admission", MarkerRule::UntilItemGone),
    // Only the current window is read; earlier windows' tasks are settled.
    ("schedule-budget-exhausted", MarkerRule::LatestSubject),
    // Doctor reports each one until the owner is told.
    ("schedule-budget-undeliverable", MarkerRule::Keep),
    ("intake.reservation", MarkerRule::Compacted),
    ("intake.counted", MarkerRule::Compacted),
    ("intake.forgotten", MarkerRule::Compacted),
    ("deliberation.entry", MarkerRule::Keep),
    ("deliberation.record", MarkerRule::Keep),
    ("deliberation.pin", MarkerRule::Keep),
    // Delivered only to a worker of an unsettled task, so one held for a
    // settled task, even one written after its owner pruned, is never read.
    ("coordination.held-follow-up", MarkerRule::UntilTaskSettled),
    // A merged pull request is closed at once, but its decision is replay
    // evidence for a finding window; sampling::compact retires both.
    ("inspection-sampling.decision", MarkerRule::Compacted),
    ("inspection-sampling.rate-raise", MarkerRule::Compacted),
    // One per repository and one per station scope; replay and grant age
    // need them for as long as the house samples.
    ("inspection-sampling.selection-key", MarkerRule::Keep),
    ("inspection-sampling.grant-epoch", MarkerRule::Keep),
];

/// The retention rule for a marker's fact.
#[must_use]
pub fn marker_rule(fact: &MarkerFact) -> MarkerRule {
    match fact {
        // Asked questions are append-only, so they are never asked again.
        MarkerFact::QuestionAsked { .. } => MarkerRule::Keep,
        MarkerFact::Verdict { .. } => MarkerRule::UntilItemGone,
        MarkerFact::Workflow { schema, .. } => schema_rule(schema),
    }
}

fn schema_rule(schema: &MarkerSchema) -> MarkerRule {
    MARKER_RULES
        .iter()
        .find(|(name, _)| *name == schema.name())
        .map_or(MarkerRule::Keep, |(_, rule)| *rule)
}

/// Why a marker was retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MarkerRetirement {
    /// Its work item was observed gone.
    ItemGone,
    /// A newer subject of the same item replaced it.
    Superseded,
    /// The task it is about settled or was retired.
    TaskSettled,
}

/// Why a task was retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TaskRetirement {
    /// A budget window that ended; its id is never created again.
    WindowEnded,
    /// Its pull request was observed closed.
    ItemGone,
}

/// Why an acknowledged mailbox message was retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MailRetirement {
    /// The attempt that posted it ended.
    AttemptEnded,
    /// Its task settled or is no longer in the store.
    TaskSettled,
}

/// One mailbox message a retention pass removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetiredMail {
    /// The message id.
    pub id: ExternalRef,
    /// Its task.
    pub task: TaskId,
    /// Why.
    pub reason: MailRetirement,
}

/// One marker a retention pass removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetiredMarker {
    /// The marker's key.
    pub key: MarkerKey,
    /// Why.
    pub reason: MarkerRetirement,
}

/// One task a retention pass removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetiredTask {
    /// The task.
    pub task: TaskId,
    /// Why.
    pub reason: TaskRetirement,
    /// Attempts whose reported usage retires with the task.
    pub reported_usage: usize,
}

/// What a retention pass removed, or would remove in a preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    /// Whether any record was removed or compacted.
    pub applied: bool,
    /// Markers removed.
    pub markers: Vec<RetiredMarker>,
    /// Tasks removed.
    pub tasks: Vec<RetiredTask>,
    /// Intake compaction per repository.
    pub intake: Vec<LedgerCompaction>,
    /// Acknowledged mailbox messages removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mail: Vec<RetiredMail>,
}

/// Entries used in one bounded table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableUsage {
    /// Entries stored.
    pub used: usize,
    /// The table's limit.
    pub limit: usize,
}

impl TableUsage {
    /// Whether usage reached [`CAPACITY_WARNING_PERCENT`] of the limit.
    #[must_use]
    pub const fn near_limit(&self) -> bool {
        self.used.saturating_mul(100) >= self.limit.saturating_mul(CAPACITY_WARNING_PERCENT)
    }
}

/// How full the store's shared tables are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreCapacity {
    /// Tasks, settled ones included.
    pub tasks: TableUsage,
    /// Workflow markers.
    pub markers: TableUsage,
    /// Consumer leases.
    pub consumers: TableUsage,
    /// Markers per workflow, to show which workflow fills the table.
    #[serde(default)]
    pub markers_by_workflow: BTreeMap<WorkflowId, usize>,
    /// Settled tasks, which retention may remove once their subject is gone.
    #[serde(default)]
    pub settled_tasks: usize,
}

impl StoreCapacity {
    pub(super) fn measure<'a>(
        tasks: impl Iterator<Item = &'a TaskRecord>,
        markers: impl Iterator<Item = &'a WorkflowMarker>,
        consumers: usize,
    ) -> Self {
        let mut task_count = 0;
        let mut settled_tasks = 0;
        for task in tasks {
            task_count += 1;
            if matches!(task.state(), TaskState::Settled { .. }) {
                settled_tasks += 1;
            }
        }
        let mut markers_by_workflow: BTreeMap<WorkflowId, usize> = BTreeMap::new();
        let mut marker_count = 0;
        for marker in markers {
            marker_count += 1;
            *markers_by_workflow
                .entry(marker.key().workflow.clone())
                .or_default() += 1;
        }
        Self {
            tasks: TableUsage {
                used: task_count,
                limit: MAX_TASKS,
            },
            markers: TableUsage {
                used: marker_count,
                limit: MAX_MARKERS,
            },
            consumers: TableUsage {
                used: consumers,
                limit: MAX_CONSUMERS,
            },
            markers_by_workflow,
            settled_tasks,
        }
    }

    /// Whether any table is near its limit.
    #[must_use]
    pub const fn near_limit(&self) -> bool {
        self.tasks.near_limit() || self.markers.near_limit() || self.consumers.near_limit()
    }
}

/// The items whose presence a retention pass would use: every issue, pull
/// request, and resource a retirable marker or task depends on. Observe
/// these, record them in an [`Inventory`], then run the pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionSubjects {
    /// Issues and pull requests.
    pub items: BTreeSet<WorkItem>,
    /// Backends whose complete listing would let tasks and approvals retire.
    pub backends: BTreeSet<BackendId>,
    /// The last item the latest applied pass looked up. It need not be in
    /// `items` any more.
    pub cursor: Option<WorkItem>,
}

impl RetentionSubjects {
    /// `items` starting after the cursor and wrapping around, so bounded
    /// passes take turns: open items that stay subjects cannot hold every
    /// pass's lookups while later items are never checked.
    pub fn lookup_order(&self) -> impl Iterator<Item = &WorkItem> {
        let (after, through) = match &self.cursor {
            Some(cursor) => (
                self.items
                    .range::<WorkItem, _>((Bound::Excluded(cursor), Bound::Unbounded)),
                Some(
                    self.items
                        .range::<WorkItem, _>((Bound::Unbounded, Bound::Included(cursor))),
                ),
            ),
            None => (self.items.range::<WorkItem, _>(..), None),
        };
        after.chain(through.into_iter().flatten())
    }

    pub(super) fn collect<'a>(
        tasks: impl Iterator<Item = &'a TaskRecord>,
        markers: impl Iterator<Item = &'a WorkflowMarker>,
        cursor: Option<&WorkItem>,
    ) -> Self {
        let mut subjects = Self {
            cursor: cursor.cloned(),
            ..Self::default()
        };
        let mut add = |item: &WorkItem| match item {
            WorkItem::Resource { resource } => {
                subjects.backends.insert(resource.backend.clone());
            }
            WorkItem::Issue { .. } | WorkItem::PullRequest { .. } => {
                subjects.items.insert(item.clone());
            }
            WorkItem::Repository { .. } | WorkItem::Task { .. } => {}
        };
        for marker in markers {
            match marker_rule(marker.fact()) {
                MarkerRule::UntilItemGone | MarkerRule::LatestSubject => add(&marker.key().item),
                MarkerRule::Keep | MarkerRule::Compacted | MarkerRule::UntilTaskSettled => {}
            }
        }
        for task in tasks.filter(|task| matches!(task.state(), TaskState::Settled { .. })) {
            if let TaskFamily::Repair(item) = TaskFamily::of(task) {
                add(&item);
                for resource in created(task) {
                    add(&WorkItem::Resource {
                        resource: resource.clone(),
                    });
                }
            }
        }
        subjects
    }
}

/// Which retention rule a task's identity allows.
enum TaskFamily {
    /// A schedule budget window task, never created again after its window.
    BudgetWindow,
    /// Pickup or interactive work on one issue. Kept: the gate resolves its
    /// verdicts' effects in whichever task its caller names, and a pull
    /// request can stay open after its issue closes. While unsettled it
    /// keeps the issue's markers.
    Issue(WorkItem),
    /// A repair round of one pull request, created again only while the
    /// pull request is open.
    Repair(WorkItem),
    /// Any other task: kept until its workflow states a rule.
    Other,
}

impl TaskFamily {
    /// The issue or pull request the task works on.
    fn item(self) -> Option<WorkItem> {
        match self {
            Self::Issue(item) | Self::Repair(item) => Some(item),
            Self::BudgetWindow | Self::Other => None,
        }
    }
}

impl TaskFamily {
    fn of(task: &TaskRecord) -> Self {
        let spec = task.spec();
        let id = spec.id.as_str();
        let Some(repository) = &spec.repository else {
            let window = id
                .strip_prefix(budget::WORKFLOW)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|millis| {
                    millis
                        .parse::<u64>()
                        .is_ok_and(|value| value.to_string() == millis)
                });
            return if window {
                Self::BudgetWindow
            } else {
                Self::Other
            };
        };
        let Some((kind, number)) = id.rsplit_once('-').and_then(|(_, number)| {
            let number = number.parse::<u64>().ok()?;
            Some((IssueNumber::new(number).ok()?, NonZeroU64::new(number)?))
        }) else {
            return Self::Other;
        };
        let repository = repository.clone();
        // Recompute the id from its parts, so only ids the workflow itself
        // derives are recognized.
        let issue = pickup::issue_task_id(&pickup::IssueRef {
            repository: repository.clone(),
            number: kind,
        });
        if issue.is_ok_and(|derived| derived == spec.id) {
            return Self::Issue(WorkItem::Issue { repository, number });
        }
        let round = id
            .strip_prefix("repair")
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(round, _)| round.parse::<u8>().ok());
        if let Some(round) = round
            && repair::repair_task_id(&repository, kind, round)
                .is_ok_and(|derived| derived == spec.id)
        {
            return Self::Repair(WorkItem::PullRequest { repository, number });
        }
        Self::Other
    }
}

/// Resources the task's applied effects created.
fn created(task: &TaskRecord) -> impl Iterator<Item = &ResourceRef> {
    task.effects()
        .iter()
        .filter_map(|effect| match effect.state() {
            EffectState::Applied { receipt, .. } => Some(receipt.created()),
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::NotApplied { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => None,
        })
        .flatten()
}

/// Decide what a pass removes. Pure: the caller removes the records in the
/// same store transaction it read them in.
pub(super) fn plan<'a>(
    tasks: &BTreeMap<TaskId, TaskRecord>,
    markers: impl Iterator<Item = &'a WorkflowMarker>,
    policy: &RetentionPolicy,
    inventory: &Inventory,
    now: Timestamp,
) -> RetentionReport {
    let markers: Vec<&WorkflowMarker> = markers.collect();
    // Items an unsettled task still works on keep every marker about them.
    let live_items: BTreeSet<WorkItem> = tasks
        .values()
        .filter(|task| !matches!(task.state(), TaskState::Settled { .. }))
        .filter_map(|task| TaskFamily::of(task).item())
        .collect();
    let newest = newest_subjects(&markers);
    let retired_markers: Vec<RetiredMarker> = markers
        .iter()
        .filter(|marker| !live_items.contains(&marker.key().item))
        .filter_map(|marker| {
            let key = marker.key();
            let reason = match marker_rule(marker.fact()) {
                MarkerRule::Keep | MarkerRule::Compacted => None,
                MarkerRule::UntilItemGone => inventory
                    .gone(&key.item)
                    .then_some(MarkerRetirement::ItemGone),
                MarkerRule::LatestSubject if inventory.gone(&key.item) => {
                    Some(MarkerRetirement::ItemGone)
                }
                MarkerRule::LatestSubject => (newest.get(&group(marker)) != Some(&key))
                    .then_some(MarkerRetirement::Superseded),
                MarkerRule::UntilTaskSettled => {
                    task_settled(tasks, &key.item).then_some(MarkerRetirement::TaskSettled)
                }
            }?;
            Some(RetiredMarker {
                key: key.clone(),
                reason,
            })
        })
        .collect();
    let retired_keys: BTreeSet<&MarkerKey> =
        retired_markers.iter().map(|retired| &retired.key).collect();
    // Tasks a remaining marker names stay, such as a deliberation thread's.
    let named: BTreeSet<&TaskId> = markers
        .iter()
        .filter(|marker| !retired_keys.contains(marker.key()))
        .filter_map(|marker| match &marker.key().item {
            WorkItem::Task { task } => Some(task),
            WorkItem::Issue { .. }
            | WorkItem::PullRequest { .. }
            | WorkItem::Resource { .. }
            | WorkItem::Repository { .. } => None,
        })
        .collect();
    let mut groups: BTreeMap<WorkItem, Vec<(&TaskRecord, bool)>> = BTreeMap::new();
    let mut retired_tasks = Vec::new();
    for task in tasks.values() {
        let family = TaskFamily::of(task);
        let eligible = settled_long_enough(task, policy, now)
            && resolved(task)
            && !awaits_acknowledgement(task)
            && !named.contains(&task.spec().id)
            && created(task).all(|resource| inventory.resource(resource) == Some(Presence::Gone));
        match family {
            TaskFamily::BudgetWindow if eligible => retired_tasks.push(RetiredTask {
                task: task.spec().id.clone(),
                reason: TaskRetirement::WindowEnded,
                reported_usage: reported_usage(task),
            }),
            TaskFamily::Repair(item) => groups.entry(item).or_default().push((task, eligible)),
            TaskFamily::BudgetWindow | TaskFamily::Issue(_) | TaskFamily::Other => {}
        }
    }
    // Repair rounds count from the first missing one, so an item's tasks
    // retire together or not at all.
    for (item, members) in groups {
        if inventory.gone(&item) && members.iter().all(|(_, eligible)| *eligible) {
            retired_tasks.extend(members.into_iter().map(|(task, _)| RetiredTask {
                task: task.spec().id.clone(),
                reason: TaskRetirement::ItemGone,
                reported_usage: reported_usage(task),
            }));
        }
    }
    retired_tasks.sort_by(|left, right| left.task.cmp(&right.task));
    RetentionReport {
        applied: false,
        markers: retired_markers,
        tasks: retired_tasks,
        intake: Vec::new(),
        mail: Vec::new(),
    }
}

/// The acknowledged mailbox messages no worker still reads. Needs no
/// outside evidence: the store records attempts and settlement itself.
pub(super) fn mail_plan(
    mailbox: &Mailbox,
    tasks: &BTreeMap<TaskId, TaskRecord>,
) -> Vec<RetiredMail> {
    mailbox
        .stored()
        .filter(|(_, acknowledged)| *acknowledged)
        .filter_map(|(mail, _)| {
            let reason = match tasks.get(mail.task()) {
                None => MailRetirement::TaskSettled,
                Some(task) if matches!(task.state(), TaskState::Settled { .. }) => {
                    MailRetirement::TaskSettled
                }
                Some(task) => {
                    let open = task.attempts().last().is_some_and(|attempt| {
                        attempt.number() == mail.attempt()
                            && matches!(
                                attempt.state(),
                                AttemptState::Running | AttemptState::Interrupted { .. }
                            )
                    });
                    if open {
                        return None;
                    }
                    MailRetirement::AttemptEnded
                }
            };
            Some(RetiredMail {
                id: mail.id().ok()?,
                task: mail.task().clone(),
                reason,
            })
        })
        .collect()
}

type Group<'a> = (&'a WorkflowId, &'a WorkItem, Option<&'a MarkerSchema>);

fn group<'a>(marker: &'a WorkflowMarker) -> Group<'a> {
    let schema = match marker.fact() {
        MarkerFact::Workflow { schema, .. } => Some(schema),
        MarkerFact::Verdict { .. } | MarkerFact::QuestionAsked { .. } => None,
    };
    (&marker.key().workflow, &marker.key().item, schema)
}

/// The newest subject's key per workflow, item, and schema, by when its
/// current fact was recorded; the larger key breaks a tie.
fn newest_subjects<'a>(markers: &[&'a WorkflowMarker]) -> BTreeMap<Group<'a>, &'a MarkerKey> {
    let mut newest: BTreeMap<Group<'a>, (Timestamp, &'a MarkerKey)> = BTreeMap::new();
    for marker in markers {
        let candidate = (marker.recorded_at(), marker.key());
        newest
            .entry(group(marker))
            .and_modify(|current| {
                if candidate > *current {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }
    newest
        .into_iter()
        .map(|(group, (_, key))| (group, key))
        .collect()
}

/// Whether `item` is a task that settled or is no longer in the store.
fn task_settled(tasks: &BTreeMap<TaskId, TaskRecord>, item: &WorkItem) -> bool {
    match item {
        WorkItem::Task { task } => tasks
            .get(task)
            .is_none_or(|record| matches!(record.state(), TaskState::Settled { .. })),
        WorkItem::Issue { .. }
        | WorkItem::PullRequest { .. }
        | WorkItem::Resource { .. }
        | WorkItem::Repository { .. } => false,
    }
}

fn reported_usage(task: &TaskRecord) -> usize {
    task.attempts()
        .iter()
        .filter(|attempt| match attempt.usage() {
            AttemptUsage::Reported { .. } => true,
            AttemptUsage::NotReported => false,
        })
        .count()
}

fn settled_long_enough(task: &TaskRecord, policy: &RetentionPolicy, now: Timestamp) -> bool {
    match task.state() {
        TaskState::Settled { at, .. } => now.saturating_since(*at) >= policy.task_window,
        TaskState::Open | TaskState::Claimed { .. } => false,
    }
}

fn resolved(task: &TaskRecord) -> bool {
    task.effects()
        .iter()
        .all(|effect| effect.state().is_resolved())
}

/// A task that did not succeed but may have written, and that no person has
/// reviewed, holds its subject for the acknowledgement guards.
fn awaits_acknowledgement(task: &TaskRecord) -> bool {
    let failed = match task.state() {
        TaskState::Settled { settlement, .. } => match settlement {
            Settlement::Succeeded => false,
            Settlement::Failed | Settlement::Cancelled | Settlement::Exhausted => true,
        },
        TaskState::Open | TaskState::Claimed { .. } => true,
    };
    let wrote = task
        .effects()
        .iter()
        .any(|effect| !matches!(effect.state(), EffectState::NotApplied { .. }));
    failed && wrote && task.write_acknowledgement().is_none()
}
