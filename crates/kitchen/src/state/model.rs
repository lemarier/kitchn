//! Persisted records and their transitions.
//!
//! Every transition runs on an in-memory copy inside one locked store
//! transaction. An error discards the copy, so a failed transition never
//! persists a partial change.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, EffectName, Error, HolderId, HouseId, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, CommitId, ContractError, Disposition,
        EffectRequest, EffectSeq, Evidence, EvidenceRevision, ExternalRef, FailureClass, Fence,
        HouseGrants, IdempotencyKey, LeaseTtl, NotAppliedReason, Operation, Receipt, Settlement,
        TaskSpec, Timestamp, UncertainReason,
    },
    state::{Corruption, Limit, StateError},
};

/// The persisted schema version.
pub(crate) const SCHEMA_VERSION: u64 = 1;
/// Tasks per house store, settled ones included. Settled tasks are kept so
/// their identities and idempotency keys are never reused; retention is not
/// implemented yet.
pub const MAX_TASKS: usize = 4096;
/// Consumer leases per house store.
pub const MAX_CONSUMERS: usize = 256;
/// Effects per task.
pub const MAX_EFFECTS_PER_TASK: usize = 256;
/// Evidence items kept for the current evidence revision.
pub const MAX_EVIDENCE_PER_REVISION: usize = 128;
/// Ownership history entries per task.
pub const MAX_OWNERSHIP_HISTORY: usize = 256;
/// Consumed message ids remembered per task.
pub const MAX_CONSUMED_MESSAGES: usize = 1024;

type Result<T> = std::result::Result<T, Error>;

fn fail<T>(error: StateError) -> Result<T> {
    Err(Error::State(error))
}

/// A time-limited ownership grant with its fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lease {
    holder: HolderId,
    fence: Fence,
    acquired_at: Timestamp,
    expires_at: Timestamp,
}

impl Lease {
    /// The owning claimant.
    #[must_use]
    pub const fn holder(&self) -> &HolderId {
        &self.holder
    }

    /// The fence presented by the owner for every change.
    #[must_use]
    pub const fn fence(&self) -> Fence {
        self.fence
    }

    /// When the lease was first acquired.
    #[must_use]
    pub const fn acquired_at(&self) -> Timestamp {
        self.acquired_at
    }

    /// When the lease stops being live unless renewed.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    /// Whether the lease is live at `now`. An expired lease means ownership is
    /// uncertain, not released.
    #[must_use]
    pub fn is_live(&self, now: Timestamp) -> bool {
        now < self.expires_at
    }
}

/// A task's ownership state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TaskState {
    /// Unclaimed and eligible for a claim.
    Open,
    /// Owned under a lease. Check [`Lease::is_live`] for uncertainty.
    Claimed {
        /// The owning lease.
        lease: Lease,
    },
    /// Terminal.
    Settled {
        /// The outcome.
        settlement: Settlement,
        /// When it settled.
        at: Timestamp,
    },
}

/// The state of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AttemptState {
    /// In progress under its fence.
    Running,
    /// Finished with a reported outcome.
    Finished {
        /// The outcome.
        outcome: AttemptOutcome,
        /// When it finished.
        at: Timestamp,
    },
    /// Its owner relinquished or lost the claim before finishing.
    Interrupted {
        /// When the interruption was recorded.
        at: Timestamp,
    },
    /// Stopped by cancellation.
    Cancelled {
        /// When cancellation was recorded.
        at: Timestamp,
    },
}

/// One attempt at a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptRecord {
    number: AttemptNumber,
    fence: Fence,
    started_at: Timestamp,
    state: AttemptState,
}

impl AttemptRecord {
    /// The one-based attempt number.
    #[must_use]
    pub const fn number(&self) -> AttemptNumber {
        self.number
    }

    /// The fence the attempt ran under.
    #[must_use]
    pub const fn fence(&self) -> Fence {
        self.fence
    }

    /// When it started.
    #[must_use]
    pub const fn started_at(&self) -> Timestamp {
        self.started_at
    }

    /// Its state.
    #[must_use]
    pub const fn state(&self) -> AttemptState {
        self.state
    }
}

/// What is known about an external effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum EffectState {
    /// Intent persisted; the outcome was never recorded (in flight or interrupted).
    Intended,
    /// The backend could not establish the outcome.
    Uncertain {
        /// Why.
        reason: UncertainReason,
        /// When recorded.
        at: Timestamp,
    },
    /// Applied, with the backend's receipt.
    Applied {
        /// The receipt.
        receipt: Receipt,
        /// When recorded.
        at: Timestamp,
    },
    /// Definitely not applied.
    NotApplied {
        /// Why.
        reason: NotAppliedReason,
        /// When recorded.
        at: Timestamp,
    },
    /// The owner accepted that the provider cannot establish the outcome.
    /// Resources it may have created keep uncertain ownership.
    Unresolvable {
        /// When recorded.
        at: Timestamp,
    },
}

impl EffectState {
    /// Whether the outcome is settled (applied, not applied, or explicitly unresolvable).
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        match self {
            Self::Intended | Self::Uncertain { .. } => false,
            Self::Applied { .. } | Self::NotApplied { .. } | Self::Unresolvable { .. } => true,
        }
    }
}

/// A reported effect outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectOutcome {
    /// Applied, with a receipt.
    Applied(Receipt),
    /// Definitely not applied.
    NotApplied(NotAppliedReason),
    /// Still unknown.
    Uncertain(UncertainReason),
    /// Unknown, and the owner accepts that it cannot be established.
    Unresolvable,
}

/// One persisted external effect: intent first, outcome later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectRecord {
    seq: EffectSeq,
    name: EffectName,
    decided_at: EvidenceRevision,
    intended_at: Timestamp,
    request: EffectRequest,
    state: EffectState,
}

impl EffectRecord {
    /// Effect number within the task.
    #[must_use]
    pub const fn seq(&self) -> EffectSeq {
        self.seq
    }

    /// The caller's logical name.
    #[must_use]
    pub const fn name(&self) -> &EffectName {
        &self.name
    }

    /// Evidence revision the decision was made at.
    #[must_use]
    pub const fn decided_at(&self) -> EvidenceRevision {
        self.decided_at
    }

    /// When the intent was persisted.
    #[must_use]
    pub const fn intended_at(&self) -> Timestamp {
        self.intended_at
    }

    /// The persisted request, including its idempotency key.
    #[must_use]
    pub const fn request(&self) -> &EffectRequest {
        &self.request
    }

    /// What is known about the outcome.
    #[must_use]
    pub const fn state(&self) -> &EffectState {
        &self.state
    }
}

/// A recorded ownership change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OwnershipEvent {
    /// A holder claimed an open task.
    Claimed {
        /// New holder.
        holder: HolderId,
        /// New fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// The owner gave the task back.
    Relinquished {
        /// Relinquished fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// A holder explicitly took over an expired claim.
    TakenOver {
        /// The superseded fence.
        previous: Fence,
        /// New holder.
        holder: HolderId,
        /// New fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// The claim ended because the task settled.
    Released {
        /// Released fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
}

/// Evidence for the current subject revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceLog {
    revision: EvidenceRevision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<CommitId>,
    items: Vec<Evidence>,
}

impl EvidenceLog {
    /// The current evidence revision.
    #[must_use]
    pub const fn revision(&self) -> EvidenceRevision {
        self.revision
    }

    /// The exact subject the current evidence is about.
    #[must_use]
    pub const fn subject(&self) -> Option<&CommitId> {
        self.subject.as_ref()
    }

    /// Evidence for the current subject. Superseded evidence is dropped.
    #[must_use]
    pub fn items(&self) -> &[Evidence] {
        &self.items
    }
}

/// A pending cancellation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelRequest {
    requested_by: HolderId,
    at: Timestamp,
}

impl CancelRequest {
    /// Who asked.
    #[must_use]
    pub const fn requested_by(&self) -> &HolderId {
        &self.requested_by
    }

    /// When.
    #[must_use]
    pub const fn at(&self) -> Timestamp {
        self.at
    }
}

/// The durable record of one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRecord {
    spec: TaskSpec,
    created_at: Timestamp,
    state: TaskState,
    attempts: Vec<AttemptRecord>,
    effects: Vec<EffectRecord>,
    evidence: EvidenceLog,
    ownership: Vec<OwnershipEvent>,
    consumed: BTreeSet<ExternalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancel: Option<CancelRequest>,
}

impl TaskRecord {
    /// The immutable specification.
    #[must_use]
    pub const fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    /// When the task was created.
    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Ownership state.
    #[must_use]
    pub const fn state(&self) -> &TaskState {
        &self.state
    }

    /// All attempts, oldest first.
    #[must_use]
    pub fn attempts(&self) -> &[AttemptRecord] {
        &self.attempts
    }

    /// All effects, oldest first.
    #[must_use]
    pub fn effects(&self) -> &[EffectRecord] {
        &self.effects
    }

    /// Current evidence.
    #[must_use]
    pub const fn evidence(&self) -> &EvidenceLog {
        &self.evidence
    }

    /// Ownership history, oldest first.
    #[must_use]
    pub fn ownership(&self) -> &[OwnershipEvent] {
        &self.ownership
    }

    /// The pending cancellation request, if any.
    #[must_use]
    pub const fn cancel_request(&self) -> Option<&CancelRequest> {
        self.cancel.as_ref()
    }

    /// Effects whose outcome is not yet resolved.
    pub fn unresolved_effects(&self) -> impl Iterator<Item = &EffectRecord> {
        self.effects
            .iter()
            .filter(|effect| !effect.state.is_resolved())
    }

    fn unresolved_count(&self) -> usize {
        self.unresolved_effects().count()
    }

    fn settlement(&self) -> Option<Settlement> {
        match self.state {
            TaskState::Settled { settlement, .. } => Some(settlement),
            TaskState::Open | TaskState::Claimed { .. } => None,
        }
    }

    /// The current lease if `fence` owns the task. With `require_live`, an
    /// expired lease is rejected: starting new work needs live ownership,
    /// while recording facts needs only the current fence.
    fn owned_lease(&self, fence: Fence, now: Timestamp, require_live: bool) -> Result<&Lease> {
        match &self.state {
            TaskState::Settled { settlement, .. } => fail(StateError::TaskSettled {
                task: self.spec.id.clone(),
                settlement: *settlement,
            }),
            TaskState::Open => fail(StateError::StaleFence { presented: fence }),
            TaskState::Claimed { lease } if lease.fence != fence => {
                fail(StateError::StaleFence { presented: fence })
            }
            TaskState::Claimed { lease } if require_live && !lease.is_live(now) => {
                fail(StateError::LeaseExpired {
                    expired_at: lease.expires_at,
                })
            }
            TaskState::Claimed { lease } => Ok(lease),
        }
    }

    fn attempt(&self, number: AttemptNumber) -> Option<&AttemptRecord> {
        let index = usize::try_from(number.get()).ok()?.checked_sub(1)?;
        self.attempts.get(index)
    }

    /// Attempts left under the count bound after attempt `number` finished.
    fn remaining_after(&self, number: AttemptNumber) -> u32 {
        self.spec.retry.max_attempts().saturating_sub(number.get())
    }

    fn running_attempt_mut(&mut self, fence: Fence) -> Option<&mut AttemptRecord> {
        self.attempts
            .last_mut()
            .filter(|attempt| attempt.fence == fence && attempt.state == AttemptState::Running)
    }

    fn interrupt_running(&mut self, at: Timestamp) {
        if let Some(attempt) = self.attempts.last_mut()
            && attempt.state == AttemptState::Running
        {
            attempt.state = AttemptState::Interrupted { at };
        }
    }

    fn push_ownership(&mut self, event: OwnershipEvent) -> Result<()> {
        if self.ownership.len() >= MAX_OWNERSHIP_HISTORY {
            return fail(StateError::CapacityExceeded {
                limit: Limit::OwnershipHistory,
            });
        }
        self.ownership.push(event);
        Ok(())
    }

    fn settle(&mut self, settlement: Settlement, fence: Fence, at: Timestamp) -> Result<()> {
        self.push_ownership(OwnershipEvent::Released { fence, at })?;
        self.state = TaskState::Settled { settlement, at };
        Ok(())
    }

    fn attempt_count(&self) -> u32 {
        u32::try_from(self.attempts.len()).unwrap_or(u32::MAX)
    }

    fn budget_spent(&self, now: Timestamp) -> bool {
        let retry = self.spec.retry;
        let count_spent = self.attempt_count() >= retry.max_attempts();
        let time_spent = self
            .attempts
            .first()
            .is_some_and(|first| now.saturating_since(first.started_at) > retry.max_elapsed());
        count_spent || time_spent
    }
}

/// Result of creating a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creation {
    /// A new task was stored.
    Created,
    /// An identical task already existed; nothing changed.
    AlreadyExists,
}

/// Result of a cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelStatus {
    /// The task was open with no unresolved effects and is now cancelled.
    Settled,
    /// Recorded; the owner (or the next owner) must stop and settle it.
    Pending,
    /// The task had already settled; nothing changed.
    AlreadySettled(Settlement),
}

/// Result of consuming an inbound message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumption {
    /// First delivery; handle it.
    New,
    /// Already consumed; ignore it.
    Duplicate,
}

/// Whether an uncertain effect may be resubmitted with its original key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resubmission {
    /// The backend cannot deduplicate; refuse.
    Refuse,
    /// The backend deduplicates by key; resubmit with the same key.
    SameKey,
}

/// What a caller must do after [`crate::state::HouseStore::begin_effect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectStart {
    /// Intent is persisted; execute this request and record the outcome.
    Execute(EffectRecord),
    /// This logical effect already has a resolved outcome; do not execute.
    Resolved(EffectRecord),
}

/// A request to start one logical effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectPlan {
    /// Owning task.
    pub task: TaskId,
    /// The owner's fence.
    pub fence: Fence,
    /// Logical name, unique within the attempt.
    pub name: EffectName,
    /// Evidence revision the decision was based on.
    pub decided_at: EvidenceRevision,
    /// The effect.
    pub operation: Operation,
}

/// Work needing an explicit recovery decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryItem {
    /// A task claim expired; ownership is uncertain until someone takes over.
    UncertainTaskOwner {
        /// The task.
        task: TaskId,
        /// Last known holder.
        holder: HolderId,
        /// When its lease expired.
        expired_at: Timestamp,
    },
    /// An unowned task has effects with unknown outcomes.
    UnresolvedEffects {
        /// The task.
        task: TaskId,
        /// Number of unresolved effects.
        count: usize,
    },
    /// An unowned task has a pending cancellation that needs an owner to settle it.
    PendingCancellation {
        /// The task.
        task: TaskId,
    },
    /// A consumer lease expired; ownership is uncertain until someone takes over.
    UncertainConsumer {
        /// The consumer scope.
        consumer: ConsumerId,
        /// Last known holder.
        holder: HolderId,
        /// When its lease expired.
        expired_at: Timestamp,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoreState {
    schema: u64,
    house: HouseId,
    nonce: u64,
    next_fence: u64,
    #[serde(deserialize_with = "unique_map")]
    tasks: BTreeMap<TaskId, TaskRecord>,
    #[serde(deserialize_with = "unique_map")]
    consumers: BTreeMap<ConsumerId, Lease>,
}

/// Deserialize a map, rejecting a repeated key instead of letting a later
/// entry silently replace an earlier one (and, with it, recorded ownership).
fn unique_map<'de, D, K, V>(deserializer: D) -> std::result::Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct UniqueMap<K, V>(std::marker::PhantomData<(K, V)>);

    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> serde::de::Visitor<'de>
        for UniqueMap<K, V>
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map with unique keys")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut map = BTreeMap::new();
            while let Some((key, value)) = access.next_entry()? {
                if map.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate key"));
                }
            }
            Ok(map)
        }
    }

    deserializer.deserialize_map(UniqueMap(std::marker::PhantomData))
}

#[derive(Deserialize)]
pub(crate) struct SchemaProbe {
    pub(crate) schema: u64,
    pub(crate) house: HouseId,
}

impl StoreState {
    pub(crate) const fn new(house: HouseId, nonce: u64) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            house,
            nonce,
            next_fence: 1,
            tasks: BTreeMap::new(),
            consumers: BTreeMap::new(),
        }
    }

    pub(crate) const fn nonce(&self) -> u64 {
        self.nonce
    }

    fn issue_fence(&mut self) -> Fence {
        let fence = Fence::new(self.next_fence);
        self.next_fence = self.next_fence.saturating_add(1);
        fence
    }

    fn new_lease(&mut self, holder: &HolderId, ttl: LeaseTtl, now: Timestamp) -> Lease {
        Lease {
            holder: holder.clone(),
            fence: self.issue_fence(),
            acquired_at: now,
            expires_at: now.saturating_add(ttl.duration()),
        }
    }

    pub(crate) fn task(&self, id: &TaskId) -> Result<&TaskRecord> {
        self.tasks
            .get(id)
            .ok_or_else(|| Error::State(StateError::TaskNotFound(id.clone())))
    }

    fn task_mut(&mut self, id: &TaskId) -> Result<&mut TaskRecord> {
        self.tasks
            .get_mut(id)
            .ok_or_else(|| Error::State(StateError::TaskNotFound(id.clone())))
    }

    pub(crate) fn tasks(&self) -> impl Iterator<Item = &TaskRecord> {
        self.tasks.values()
    }

    pub(crate) fn consumer(&self, id: &ConsumerId) -> Option<&Lease> {
        self.consumers.get(id)
    }

    pub(crate) fn create_task(&mut self, spec: TaskSpec, now: Timestamp) -> Result<Creation> {
        if spec.authority.house() != &self.house {
            return Err(ContractError::CrossHouse {
                expected: self.house.clone(),
                found: spec.authority.house().clone(),
            }
            .into());
        }
        if let Some(existing) = self.tasks.get(&spec.id) {
            return if existing.spec == spec {
                Ok(Creation::AlreadyExists)
            } else {
                fail(StateError::TaskConflict(spec.id))
            };
        }
        if self.tasks.len() >= MAX_TASKS {
            return fail(StateError::CapacityExceeded {
                limit: Limit::Tasks,
            });
        }
        let record = TaskRecord {
            spec: spec.clone(),
            created_at: now,
            state: TaskState::Open,
            attempts: Vec::new(),
            effects: Vec::new(),
            evidence: EvidenceLog::default(),
            ownership: Vec::new(),
            consumed: BTreeSet::new(),
            cancel: None,
        };
        self.tasks.insert(spec.id, record);
        Ok(Creation::Created)
    }

    pub(crate) fn claim(
        &mut self,
        id: &TaskId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        match &self.task(id)?.state {
            TaskState::Open => {}
            TaskState::Claimed { lease } if lease.is_live(now) => {
                return fail(StateError::ClaimHeld {
                    holder: lease.holder.clone(),
                    expires_at: lease.expires_at,
                });
            }
            TaskState::Claimed { lease } => {
                return fail(StateError::LeaseExpired {
                    expired_at: lease.expires_at,
                });
            }
            TaskState::Settled { settlement, .. } => {
                return fail(StateError::TaskSettled {
                    task: id.clone(),
                    settlement: *settlement,
                });
            }
        }
        let lease = self.new_lease(holder, ttl, now);
        let task = self.task_mut(id)?;
        task.push_ownership(OwnershipEvent::Claimed {
            holder: holder.clone(),
            fence: lease.fence,
            at: now,
        })?;
        task.state = TaskState::Claimed {
            lease: lease.clone(),
        };
        Ok(lease)
    }

    pub(crate) fn renew(
        &mut self,
        id: &TaskId,
        fence: Fence,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, true)?;
        match &mut task.state {
            TaskState::Claimed { lease } => {
                lease.expires_at = now.saturating_add(ttl.duration());
                Ok(lease.clone())
            }
            TaskState::Open | TaskState::Settled { .. } => {
                fail(StateError::StaleFence { presented: fence })
            }
        }
    }

    pub(crate) fn relinquish(&mut self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<()> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, false)?;
        task.push_ownership(OwnershipEvent::Relinquished { fence, at: now })?;
        task.interrupt_running(now);
        task.state = TaskState::Open;
        Ok(())
    }

    pub(crate) fn take_over(
        &mut self,
        id: &TaskId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        let previous = match &self.task(id)?.state {
            TaskState::Open => return self.claim(id, holder, ttl, now),
            TaskState::Claimed { lease } if lease.is_live(now) => {
                return fail(StateError::LeaseLive {
                    expires_at: lease.expires_at,
                });
            }
            TaskState::Claimed { lease } => lease.fence,
            TaskState::Settled { settlement, .. } => {
                return fail(StateError::TaskSettled {
                    task: id.clone(),
                    settlement: *settlement,
                });
            }
        };
        let lease = self.new_lease(holder, ttl, now);
        let task = self.task_mut(id)?;
        task.push_ownership(OwnershipEvent::TakenOver {
            previous,
            holder: holder.clone(),
            fence: lease.fence,
            at: now,
        })?;
        task.interrupt_running(now);
        task.state = TaskState::Claimed {
            lease: lease.clone(),
        };
        Ok(lease)
    }

    pub(crate) fn start_attempt(
        &mut self,
        id: &TaskId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<AttemptStart> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, true)?;
        if let Some(attempt) = task.running_attempt_mut(fence) {
            return Ok(AttemptStart::AlreadyRunning(attempt.number));
        }
        if task.cancel.is_some() {
            return fail(StateError::CancelRequested);
        }
        let unresolved = task.unresolved_count();
        if unresolved > 0 {
            return fail(StateError::UnresolvedEffects { count: unresolved });
        }
        if task.budget_spent(now) {
            task.settle(Settlement::Exhausted, fence, now)?;
            return Ok(AttemptStart::Exhausted);
        }
        let number = AttemptNumber::new(task.attempt_count().saturating_add(1)).ok_or(
            Error::State(StateError::CorruptState(Corruption::AttemptSequence)),
        )?;
        task.attempts.push(AttemptRecord {
            number,
            fence,
            started_at: now,
            state: AttemptState::Running,
        });
        Ok(AttemptStart::Started(number))
    }

    pub(crate) fn finish_attempt(
        &mut self,
        id: &TaskId,
        fence: Fence,
        number: AttemptNumber,
        outcome: AttemptOutcome,
        now: Timestamp,
    ) -> Result<Disposition> {
        let task = self.task_mut(id)?;
        if matches!(&task.state, TaskState::Claimed { lease } if lease.fence != fence) {
            return fail(StateError::StaleFence { presented: fence });
        }
        let Some(attempt) = task.attempt(number) else {
            return fail(StateError::AttemptNotFound(number));
        };
        if attempt.fence != fence {
            return fail(StateError::StaleFence { presented: fence });
        }
        match attempt.state {
            AttemptState::Running => {}
            AttemptState::Finished {
                outcome: recorded, ..
            } => return replayed_finish(task, number, recorded, outcome),
            AttemptState::Interrupted { .. } | AttemptState::Cancelled { .. } => {
                return fail(StateError::NoRunningAttempt);
            }
        }
        task.owned_lease(fence, now, false)?;
        let unresolved = task.unresolved_count();
        if unresolved > 0 {
            return fail(StateError::UnresolvedEffects { count: unresolved });
        }
        let Some(attempt) = task.running_attempt_mut(fence) else {
            return fail(StateError::NoRunningAttempt);
        };
        attempt.state = AttemptState::Finished { outcome, at: now };
        let settlement = match outcome {
            AttemptOutcome::Succeeded => Some(Settlement::Succeeded),
            AttemptOutcome::Failed(FailureClass::Permanent) => Some(Settlement::Failed),
            AttemptOutcome::Failed(FailureClass::Retryable) if task.cancel.is_some() => {
                Some(Settlement::Cancelled)
            }
            AttemptOutcome::Failed(FailureClass::Retryable) if task.budget_spent(now) => {
                Some(Settlement::Exhausted)
            }
            AttemptOutcome::Failed(FailureClass::Retryable) => None,
        };
        match settlement {
            Some(settlement) => {
                task.settle(settlement, fence, now)?;
                Ok(Disposition::Settled(settlement))
            }
            None => Ok(Disposition::RetryAvailable {
                remaining: task.remaining_after(number),
            }),
        }
    }

    pub(crate) fn request_cancel(
        &mut self,
        id: &TaskId,
        requested_by: &HolderId,
        now: Timestamp,
    ) -> Result<CancelStatus> {
        let task = self.task_mut(id)?;
        if let Some(settlement) = task.settlement() {
            return Ok(CancelStatus::AlreadySettled(settlement));
        }
        if task.cancel.is_none() {
            task.cancel = Some(CancelRequest {
                requested_by: requested_by.clone(),
                at: now,
            });
        }
        if task.state == TaskState::Open && task.unresolved_count() == 0 {
            task.state = TaskState::Settled {
                settlement: Settlement::Cancelled,
                at: now,
            };
            return Ok(CancelStatus::Settled);
        }
        Ok(CancelStatus::Pending)
    }

    pub(crate) fn settle_cancelled(
        &mut self,
        id: &TaskId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<()> {
        let task = self.task_mut(id)?;
        if task.settlement() == Some(Settlement::Cancelled)
            && matches!(task.ownership.last(), Some(OwnershipEvent::Released { fence: released, .. }) if *released == fence)
        {
            return Ok(());
        }
        let holder = task.owned_lease(fence, now, false)?.holder.clone();
        let unresolved = task.unresolved_count();
        if unresolved > 0 {
            return fail(StateError::UnresolvedEffects { count: unresolved });
        }
        if task.cancel.is_none() {
            task.cancel = Some(CancelRequest {
                requested_by: holder,
                at: now,
            });
        }
        if let Some(attempt) = task.running_attempt_mut(fence) {
            attempt.state = AttemptState::Cancelled { at: now };
        }
        task.settle(Settlement::Cancelled, fence, now)
    }

    pub(crate) fn begin_effect(
        &mut self,
        plan: EffectPlan,
        grants: &HouseGrants,
        resubmission: Resubmission,
        now: Timestamp,
    ) -> Result<EffectStart> {
        if grants.house() != &self.house {
            return Err(ContractError::CrossHouse {
                expected: self.house.clone(),
                found: grants.house().clone(),
            }
            .into());
        }
        let house = self.house.clone();
        let nonce = self.nonce;
        let task = self.task_mut(&plan.task)?;
        task.owned_lease(plan.fence, now, true)?;
        if task.cancel.is_some() {
            return fail(StateError::CancelRequested);
        }
        let attempt = match task.running_attempt_mut(plan.fence) {
            Some(attempt) => attempt.number,
            None => return fail(StateError::NoRunningAttempt),
        };
        if plan.decided_at != task.evidence.revision {
            return fail(StateError::StaleDecision {
                decided: plan.decided_at,
                current: task.evidence.revision,
            });
        }
        task.spec.authority.authorize(
            grants,
            plan.operation.required_permission(),
            &task.spec.scope(),
        )?;
        let same_name = task.effects.iter().rev().find(|effect| {
            effect.name == plan.name
                && effect.request.attempt() == attempt
                && !matches!(effect.state, EffectState::NotApplied { .. })
        });
        if let Some(existing) = same_name {
            if existing.request.operation() != &plan.operation {
                return fail(StateError::EffectNameConflict(existing.seq));
            }
            return match (&existing.state, resubmission) {
                (EffectState::Applied { .. } | EffectState::Unresolvable { .. }, _) => {
                    Ok(EffectStart::Resolved(existing.clone()))
                }
                (EffectState::Intended | EffectState::Uncertain { .. }, Resubmission::SameKey) => {
                    Ok(EffectStart::Execute(existing.clone()))
                }
                (EffectState::Intended | EffectState::Uncertain { .. }, Resubmission::Refuse) => {
                    fail(StateError::UnsafeRetry(existing.seq))
                }
                (EffectState::NotApplied { .. }, _) => {
                    fail(StateError::EffectNameConflict(existing.seq))
                }
            };
        }
        let unresolved = task.unresolved_count();
        if unresolved > 0 {
            return fail(StateError::UnresolvedEffects { count: unresolved });
        }
        if task.effects.len() >= MAX_EFFECTS_PER_TASK {
            return fail(StateError::CapacityExceeded {
                limit: Limit::Effects,
            });
        }
        let seq = EffectSeq::new(u32::try_from(task.effects.len()).unwrap_or(u32::MAX));
        let key = ExternalRef::new(&format!(
            "kitchen-{house}-{}-{nonce:016x}-{}",
            plan.task,
            seq.get()
        ))?;
        let record = EffectRecord {
            seq,
            name: plan.name,
            decided_at: plan.decided_at,
            intended_at: now,
            request: EffectRequest::new(
                house,
                plan.task,
                attempt,
                IdempotencyKey::from_ref(key),
                plan.operation,
            ),
            state: EffectState::Intended,
        };
        task.effects.push(record.clone());
        Ok(EffectStart::Execute(record))
    }

    pub(crate) fn record_effect_outcome(
        &mut self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        outcome: EffectOutcome,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, false)?;
        let effect = task
            .effects
            .iter_mut()
            .find(|effect| effect.seq == seq)
            .ok_or(Error::State(StateError::EffectNotFound(seq)))?;
        let next = match (&effect.state, outcome) {
            (EffectState::Intended | EffectState::Uncertain { .. }, outcome) => {
                Some(state_for(outcome, now))
            }
            (EffectState::Unresolvable { .. }, EffectOutcome::Applied(receipt)) => {
                Some(EffectState::Applied { receipt, at: now })
            }
            (EffectState::Unresolvable { .. }, EffectOutcome::NotApplied(reason)) => {
                Some(EffectState::NotApplied { reason, at: now })
            }
            (EffectState::Applied { receipt, .. }, EffectOutcome::Applied(reported))
                if *receipt != reported =>
            {
                return fail(StateError::ConflictingOutcome(seq));
            }
            (EffectState::Applied { .. }, EffectOutcome::NotApplied(_))
            | (EffectState::NotApplied { .. }, EffectOutcome::Applied(_)) => {
                return fail(StateError::ConflictingOutcome(seq));
            }
            (
                EffectState::Applied { .. }
                | EffectState::NotApplied { .. }
                | EffectState::Unresolvable { .. },
                EffectOutcome::Applied(_)
                | EffectOutcome::NotApplied(_)
                | EffectOutcome::Uncertain(_)
                | EffectOutcome::Unresolvable,
            ) => None,
        };
        if let Some(next) = next {
            effect.state = next;
        }
        Ok(effect.clone())
    }

    pub(crate) fn record_evidence(
        &mut self,
        id: &TaskId,
        fence: Fence,
        evidence: Evidence,
        now: Timestamp,
    ) -> Result<EvidenceRevision> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, false)?;
        let log = &mut task.evidence;
        if log.subject.as_ref() != Some(&evidence.subject) {
            log.revision = log.revision.next();
            log.subject = Some(evidence.subject.clone());
            log.items.clear();
        }
        if log.items.contains(&evidence) {
            return Ok(log.revision);
        }
        if log.items.len() >= MAX_EVIDENCE_PER_REVISION {
            return fail(StateError::CapacityExceeded {
                limit: Limit::Evidence,
            });
        }
        log.items.push(evidence);
        Ok(log.revision)
    }

    pub(crate) fn consume_message(
        &mut self,
        id: &TaskId,
        fence: Fence,
        message: &ExternalRef,
        now: Timestamp,
    ) -> Result<Consumption> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, true)?;
        if task.consumed.contains(message) {
            return Ok(Consumption::Duplicate);
        }
        if task.consumed.len() >= MAX_CONSUMED_MESSAGES {
            return fail(StateError::CapacityExceeded {
                limit: Limit::ConsumedMessages,
            });
        }
        task.consumed.insert(message.clone());
        Ok(Consumption::New)
    }

    pub(crate) fn acquire_consumer(
        &mut self,
        consumer: &ConsumerId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        match self.consumers.get(consumer) {
            Some(lease) if lease.is_live(now) => fail(StateError::ClaimHeld {
                holder: lease.holder.clone(),
                expires_at: lease.expires_at,
            }),
            Some(lease) => fail(StateError::LeaseExpired {
                expired_at: lease.expires_at,
            }),
            None if self.consumers.len() >= MAX_CONSUMERS => fail(StateError::CapacityExceeded {
                limit: Limit::Consumers,
            }),
            None => {
                let lease = self.new_lease(holder, ttl, now);
                self.consumers.insert(consumer.clone(), lease.clone());
                Ok(lease)
            }
        }
    }

    pub(crate) fn renew_consumer(
        &mut self,
        consumer: &ConsumerId,
        fence: Fence,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        let Some(lease) = self.consumers.get_mut(consumer) else {
            return fail(StateError::ConsumerNotFound(consumer.clone()));
        };
        if lease.fence != fence {
            return fail(StateError::StaleFence { presented: fence });
        }
        if !lease.is_live(now) {
            return fail(StateError::LeaseExpired {
                expired_at: lease.expires_at,
            });
        }
        lease.expires_at = now.saturating_add(ttl.duration());
        Ok(lease.clone())
    }

    pub(crate) fn release_consumer(&mut self, consumer: &ConsumerId, fence: Fence) -> Result<()> {
        match self.consumers.get(consumer) {
            None => Ok(()),
            Some(lease) if lease.fence == fence => {
                self.consumers.remove(consumer);
                Ok(())
            }
            Some(_) => fail(StateError::StaleFence { presented: fence }),
        }
    }

    pub(crate) fn take_over_consumer(
        &mut self,
        consumer: &ConsumerId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        match self.consumers.get(consumer) {
            None => self.acquire_consumer(consumer, holder, ttl, now),
            Some(lease) if lease.is_live(now) => fail(StateError::LeaseLive {
                expires_at: lease.expires_at,
            }),
            Some(_) => {
                let lease = self.new_lease(holder, ttl, now);
                self.consumers.insert(consumer.clone(), lease.clone());
                Ok(lease)
            }
        }
    }

    pub(crate) fn recovery_queue(&self, now: Timestamp) -> Vec<RecoveryItem> {
        let tasks = self.tasks.values().filter_map(|task| {
            let id = task.spec.id.clone();
            match &task.state {
                TaskState::Claimed { lease } if !lease.is_live(now) => {
                    Some(RecoveryItem::UncertainTaskOwner {
                        task: id,
                        holder: lease.holder.clone(),
                        expired_at: lease.expires_at,
                    })
                }
                TaskState::Claimed { .. } | TaskState::Settled { .. } => None,
                TaskState::Open => match task.unresolved_count() {
                    0 if task.cancel.is_some() => {
                        Some(RecoveryItem::PendingCancellation { task: id })
                    }
                    0 => None,
                    count => Some(RecoveryItem::UnresolvedEffects { task: id, count }),
                },
            }
        });
        let consumers = self
            .consumers
            .iter()
            .filter(|(_, lease)| !lease.is_live(now))
            .map(|(consumer, lease)| RecoveryItem::UncertainConsumer {
                consumer: consumer.clone(),
                holder: lease.holder.clone(),
                expired_at: lease.expires_at,
            });
        tasks.chain(consumers).collect()
    }

    /// Check invariants that the type system cannot express.
    pub(crate) fn validate(&self) -> std::result::Result<(), Corruption> {
        if self.tasks.len() > MAX_TASKS || self.consumers.len() > MAX_CONSUMERS {
            return Err(Corruption::LimitExceeded);
        }
        if self
            .consumers
            .values()
            .any(|lease| lease.fence.get() >= self.next_fence)
        {
            return Err(Corruption::FenceAhead);
        }
        self.tasks
            .iter()
            .try_for_each(|(key, task)| self.validate_task(key, task))
    }

    fn validate_task(
        &self,
        key: &TaskId,
        task: &TaskRecord,
    ) -> std::result::Result<(), Corruption> {
        if key != &task.spec.id {
            return Err(Corruption::TaskKey);
        }
        if task.spec.authority.house() != &self.house {
            return Err(Corruption::EffectReference);
        }
        if task.effects.len() > MAX_EFFECTS_PER_TASK
            || task.evidence.items.len() > MAX_EVIDENCE_PER_REVISION
            || task.ownership.len() > MAX_OWNERSHIP_HISTORY
            || task.consumed.len() > MAX_CONSUMED_MESSAGES
            || task.attempts.len()
                > usize::try_from(task.spec.retry.max_attempts()).unwrap_or(usize::MAX)
        {
            return Err(Corruption::LimitExceeded);
        }
        validate_ownership(task, self.next_fence)?;
        let current_fence = match &task.state {
            TaskState::Claimed { lease } => {
                if lease.fence.get() >= self.next_fence {
                    return Err(Corruption::FenceAhead);
                }
                Some(lease.fence)
            }
            TaskState::Open | TaskState::Settled { .. } => None,
        };
        for (index, attempt) in task.attempts.iter().enumerate() {
            let expected = u32::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(1));
            if Some(attempt.number.get()) != expected {
                return Err(Corruption::AttemptSequence);
            }
            if attempt.fence.get() >= self.next_fence {
                return Err(Corruption::FenceAhead);
            }
            let is_last = index.saturating_add(1) == task.attempts.len();
            if attempt.state == AttemptState::Running
                && (!is_last || current_fence != Some(attempt.fence))
            {
                return Err(Corruption::RunningAttempt);
            }
        }
        for (index, effect) in task.effects.iter().enumerate() {
            if u32::try_from(index).ok() != Some(effect.seq.get()) {
                return Err(Corruption::EffectSequence);
            }
            let request = &effect.request;
            if request.house() != &self.house
                || request.task() != key
                || usize::try_from(request.attempt().get())
                    .map_or(true, |number| number > task.attempts.len())
            {
                return Err(Corruption::EffectReference);
            }
        }
        if task.settlement().is_some()
            && (task.unresolved_count() > 0
                || task
                    .attempts
                    .iter()
                    .any(|attempt| attempt.state == AttemptState::Running))
        {
            return Err(Corruption::SettledWithWork);
        }
        Ok(())
    }
}

/// Replay the ownership history: each claim, adoption, or takeover gets a
/// larger fence than every earlier one; a relinquish, takeover, or release
/// names the current owner's fence; nothing follows a release; the replayed
/// owner matches the task state; and every attempt ran under an owned fence.
fn validate_ownership(task: &TaskRecord, next_fence: u64) -> std::result::Result<(), Corruption> {
    let mut owner: Option<(&HolderId, Fence)> = None;
    let mut owned = BTreeSet::new();
    let mut released = false;
    let issue = |owned: &mut BTreeSet<Fence>, fence: Fence| {
        if fence.get() >= next_fence {
            return Err(Corruption::FenceAhead);
        }
        if owned.last().is_some_and(|last| *last >= fence) {
            return Err(Corruption::Ownership);
        }
        owned.insert(fence);
        Ok(())
    };
    for event in &task.ownership {
        if released {
            return Err(Corruption::Ownership);
        }
        let current = owner.map(|(_, fence)| fence);
        match event {
            OwnershipEvent::Claimed { holder, fence, .. } => {
                if owner.is_some() {
                    return Err(Corruption::Ownership);
                }
                issue(&mut owned, *fence)?;
                owner = Some((holder, *fence));
            }
            OwnershipEvent::TakenOver {
                previous,
                holder,
                fence,
                ..
            } => {
                if current != Some(*previous) {
                    return Err(Corruption::Ownership);
                }
                issue(&mut owned, *fence)?;
                owner = Some((holder, *fence));
            }
            OwnershipEvent::Relinquished { fence, .. } | OwnershipEvent::Released { fence, .. } => {
                if current != Some(*fence) {
                    return Err(Corruption::Ownership);
                }
                owner = None;
                released = matches!(event, OwnershipEvent::Released { .. });
            }
        }
    }
    let consistent = match &task.state {
        TaskState::Claimed { lease } => owner == Some((&lease.holder, lease.fence)),
        TaskState::Open => owner.is_none() && !released,
        TaskState::Settled { .. } => owner.is_none(),
    };
    if !consistent
        || task
            .attempts
            .iter()
            .any(|attempt| !owned.contains(&attempt.fence))
    {
        return Err(Corruption::Ownership);
    }
    Ok(())
}

fn state_for(outcome: EffectOutcome, at: Timestamp) -> EffectState {
    match outcome {
        EffectOutcome::Applied(receipt) => EffectState::Applied { receipt, at },
        EffectOutcome::NotApplied(reason) => EffectState::NotApplied { reason, at },
        EffectOutcome::Uncertain(reason) => EffectState::Uncertain { reason, at },
        EffectOutcome::Unresolvable => EffectState::Unresolvable { at },
    }
}

/// The result of repeating `finish_attempt` for an attempt that already
/// finished: the same disposition it produced, or a conflict.
fn replayed_finish(
    task: &TaskRecord,
    number: AttemptNumber,
    recorded: AttemptOutcome,
    reported: AttemptOutcome,
) -> Result<Disposition> {
    if recorded != reported {
        return fail(StateError::ConflictingAttemptOutcome);
    }
    let is_last = task.attempts.last().map(AttemptRecord::number) == Some(number);
    Ok(match task.settlement() {
        Some(settlement) if is_last => Disposition::Settled(settlement),
        Some(_) | None => Disposition::RetryAvailable {
            remaining: task.remaining_after(number),
        },
    })
}
