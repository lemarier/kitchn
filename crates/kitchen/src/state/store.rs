//! The file-backed, house-scoped store.
//!
//! Layout inside the caller-supplied directory:
//!
//! - `store.json`: the initialization marker naming the house and the store's
//!   random nonce. Written once by [`HouseStore::initialize`]; its presence
//!   means the store is established, so a missing snapshot is an error.
//! - `state.lock`: an advisory lock file. Writers take an exclusive lock and
//!   readers a shared one, with a bounded wait.
//! - `state.json`: the committed snapshot, replaced atomically by writing
//!   `state.json.tmp`, syncing it, renaming it over the snapshot, and syncing
//!   the directory. A crash leaves either the old or the new snapshot; a stale
//!   temporary file is ignored and overwritten by the next write.
//!
//! Each call is one transaction: lock, read and validate, apply one
//! transition to a copy, write if it changed, unlock. The lock is never held
//! across backend calls. On Unix, newly created directories and files are
//! readable only by their owner because the state holds private briefs.
//!
//! Trust assumptions: the directory is private to the Kitchen user. The store
//! refuses a symlinked store directory and managed files that are symlinks or
//! other non-regular files, checked before every transaction; it cannot
//! prevent a process running as the same user from racing those checks.
//! Snapshot storage suits a house's working set (see [`crate::state::MAX_TASKS`]);
//! every write rewrites the whole snapshot.

use std::path::Path;

use crate::{
    ConsumerId, Error, HolderId, HouseId, TaskId, WorkflowId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, Claimant, Disposition, EffectExecutor,
        EffectSeq, Evidence, EvidenceRevision, ExternalRef, Fence, HouseGrants, LeaseTtl, TaskSpec,
        Text, Timestamp,
    },
    state::{
        CancelStatus, ConsumerRecord, Consumption, Creation, EffectOutcome, EffectPlan,
        EffectRecord, EffectStart, Lease, MarkerAttempt, MarkerFact, MarkerKey, MarkerRecording,
        RecoveryItem, Reservation, RiskDecision, TaskRecord, WorkflowMarker, WriteAcknowledgement,
        effects::SettledLookup,
        marker::PairPlan,
        model::StoreState,
        snapshot::{SnapshotStore, StoreLayout, StoreOptions},
    },
};

#[cfg(doc)]
use crate::{contracts::ContractError, state::StateError};

type Result<T> = std::result::Result<T, Error>;

const HOUSE_LAYOUT: StoreLayout = StoreLayout {
    marker: "store.json",
    snapshot: "state.json",
    temporary: "state.json.tmp",
    lock: "state.lock",
    pretty: true,
    require_private: false,
    priority_intent: None,
    priority_reserve_bytes: 0,
};

/// Durable, house-scoped task ownership state.
///
/// Handles are cheap and hold no open files; separate handles and processes
/// coordinate through the lock file. The directory must be house-scoped
/// runtime storage outside any Git checkout.
///
/// A settled task's writes change only through checked paths:
/// [`crate::state::reread_settled`] records what the backend's own lookup
/// proved, and [`crate::workflows::decomposition::acknowledge`] records a
/// person's acknowledgement. The store methods behind them are not public,
/// so a caller cannot supply an outcome, receipt, or acknowledgement itself:
///
/// ```compile_fail,E0624
/// # use kitchen::{TaskId, contracts::{Claimant, Text, Timestamp}, state::HouseStore};
/// fn acknowledge(store: &HouseStore, id: &TaskId, who: &Claimant, why: &Text, now: Timestamp) {
///     let _ = store.acknowledge_settled_writes(id, who, why, now);
/// }
/// ```
///
/// ```compile_fail,E0624
/// # use kitchen::{TaskId, contracts::{EffectSeq, Timestamp}, state::HouseStore};
/// fn record(store: &HouseStore, id: &TaskId, seq: EffectSeq, now: Timestamp) {
///     let _ = store.record_settled_lookup(id, seq, unimplemented!(), now);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct HouseStore {
    engine: SnapshotStore<StoreState>,
}

impl HouseStore {
    /// Create a new, empty store for `house` in `dir`, creating the directory.
    ///
    /// # Errors
    /// Returns [`StateError::AlreadyInitialized`] when `dir` already holds a
    /// store or a snapshot, [`StateError::StorageInsideRepository`] inside a
    /// Git checkout, [`StateError::RedirectedPath`] for a symlinked directory
    /// or managed file, and storage errors.
    pub fn initialize(
        dir: impl AsRef<Path>,
        house: HouseId,
        options: StoreOptions,
    ) -> Result<Self> {
        Ok(Self {
            engine: SnapshotStore::initialize(dir, house, options, HOUSE_LAYOUT)?,
        })
    }

    /// Open the established store for `house` in `dir`.
    ///
    /// # Errors
    /// Returns [`StateError::NotInitialized`] when `dir` holds no store,
    /// [`StateError::StateMissing`] when an established store lost its
    /// snapshot, [`ContractError::CrossHouse`] when it belongs to another
    /// house, [`StateError::RedirectedPath`] for symlinked paths, and storage
    /// or corruption errors. It never writes a replacement snapshot.
    pub fn open(dir: impl AsRef<Path>, house: HouseId, options: StoreOptions) -> Result<Self> {
        Ok(Self {
            engine: SnapshotStore::open(dir, house, options, HOUSE_LAYOUT)?,
        })
    }

    /// The house this store serves.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        self.engine.house()
    }

    /// Create a task, recording who created it and under which trigger.
    /// Repeating an identical creation is a no-op and keeps the original
    /// creator.
    ///
    /// # Errors
    /// Rejects a foreign-house authority, a reused id with a different
    /// specification, and a full store.
    pub fn create_task(
        &self,
        spec: TaskSpec,
        created_by: &Claimant,
        now: Timestamp,
    ) -> Result<Creation> {
        self.transact(|state| state.create_task(spec, created_by, now))
    }

    /// Create a task and claim it for `claimant` unless `guard` objects to
    /// the tasks already stored. The guard reads them, and the task is
    /// created and claimed, in one store transaction, so a concurrent
    /// reservation cannot slip between the check and the write and the new
    /// task never exists unclaimed. A task that already exists must be
    /// identical. If it is open or its claim expired, the guard runs on the
    /// tasks in the same transaction and it is claimed again
    /// ([`Reservation::Resumed`]); if it has settled or is live-claimed by
    /// someone else, it is reported as [`Reservation::Existing`] untouched.
    /// The guard returns the reason to block, and
    /// nothing is written then.
    ///
    /// # Errors
    /// Returns the errors of [`Self::create_task`] and [`Self::claim`], and
    /// any the guard returns.
    pub fn reserve_task<R>(
        &self,
        spec: TaskSpec,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
        guard: impl FnOnce(&[&TaskRecord]) -> Result<Option<R>>,
    ) -> Result<Reservation<R>> {
        self.transact(|state| state.reserve_task(spec, claimant, ttl, now, guard))
    }

    /// Read one task.
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotFound`] or a storage error.
    pub fn task(&self, id: &TaskId) -> Result<TaskRecord> {
        self.read(|state| state.task(id).cloned())?
    }

    /// Read every task, ordered by id.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn tasks(&self) -> Result<Vec<TaskRecord>> {
        self.read(|state| state.tasks().cloned().collect())
    }

    /// Claim an open task under the claimant's trigger. A live or expired
    /// claim by anyone, including the same holder id or another trigger, is
    /// refused; an expired claim needs [`Self::take_over`]. Claiming a task
    /// its previous owner relinquished records an adoption.
    ///
    /// # Errors
    /// Returns [`StateError::ClaimHeld`], [`StateError::LeaseExpired`], or
    /// [`StateError::TaskSettled`].
    pub fn claim(
        &self,
        id: &TaskId,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.claim(id, claimant, ttl, now))
    }

    /// Extend a live claim.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] or [`StateError::LeaseExpired`].
    pub fn renew(&self, id: &TaskId, fence: Fence, ttl: LeaseTtl, now: Timestamp) -> Result<Lease> {
        self.transact(|state| state.renew(id, fence, ttl, now))
    }

    /// Give a claim back, interrupting a running attempt. Unresolved effects
    /// stay recorded for the next owner to reconcile.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] when `fence` no longer owns the task.
    pub fn relinquish(&self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<()> {
        self.transact(|state| state.relinquish(id, fence, now))
    }

    /// Explicitly take over an expired claim with a new, larger fence. The
    /// previous owner's running attempt becomes interrupted and its fence stale.
    ///
    /// # Errors
    /// Returns [`StateError::LeaseLive`] while the current lease is live.
    pub fn take_over(
        &self,
        id: &TaskId,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.take_over(id, claimant, ttl, now))
    }

    /// Start an attempt, or report the running one. Settles the task as
    /// exhausted when the retry budget is spent.
    ///
    /// # Errors
    /// Refuses without a live claim, after a cancellation request, and while
    /// effects are unresolved.
    pub fn start_attempt(&self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<AttemptStart> {
        self.transact(|state| state.start_attempt(id, fence, now))
    }

    /// Continue the task's latest attempt under `fence` without spending the
    /// retry budget: the running attempt, or the latest attempt when its
    /// previous owner relinquished or lost the claim before finishing it. An
    /// adopting or taking-over owner supervises the same attempt and its
    /// worker under its own fence. `None` when the latest attempt ended or
    /// none started; nothing changes then.
    ///
    /// # Errors
    /// Refuses without a live claim at `fence`.
    pub fn continue_attempt(
        &self,
        id: &TaskId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<Option<AttemptNumber>> {
        self.transact(|state| state.continue_attempt(id, fence, now))
    }

    /// Finish `attempt`, which must be the running attempt. Repeating the
    /// same report for any earlier attempt replays that attempt's result and
    /// changes nothing.
    ///
    /// # Errors
    /// Refuses an unknown or interrupted attempt, unresolved effects, and a
    /// report that contradicts the recorded one.
    pub fn finish_attempt(
        &self,
        id: &TaskId,
        fence: Fence,
        attempt: AttemptNumber,
        outcome: AttemptOutcome,
        now: Timestamp,
    ) -> Result<Disposition> {
        self.transact(|state| state.finish_attempt(id, fence, attempt, outcome, now))
    }

    /// Request cancellation. Needs no claim; an open task without unresolved
    /// effects settles immediately, otherwise its owner must stop its
    /// workers and settle it. Cancellation proves no rollback: an uncertain
    /// effect keeps blocking settlement until it is reconciled or a
    /// [`RiskDecision`] allows an unsuccessful settlement.
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotFound`] or a storage error.
    pub fn request_cancel(
        &self,
        id: &TaskId,
        requested_by: &HolderId,
        now: Timestamp,
    ) -> Result<CancelStatus> {
        self.transact(|state| state.request_cancel(id, requested_by, now))
    }

    /// Settle the owned task as cancelled after its effects are resolved.
    /// Cancellation does not roll back applied effects.
    ///
    /// # Errors
    /// Refuses a stale fence and unresolved effects.
    pub fn settle_cancelled(&self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<()> {
        self.transact(|state| state.settle_cancelled(id, fence, now))
    }

    /// Persist the intent for one effect on `executor` before it is executed.
    ///
    /// Every check uses the executor's own descriptor, so a caller cannot
    /// pass a more permissive one than the executor that runs the effect.
    /// Checks, in one transaction: the grants' and backend's house, the
    /// backend's capabilities and declared agent selections, live
    /// ownership, no pending cancellation (after one, only
    /// [`crate::contracts::Operation::CancelWorker`] for a worker an applied
    /// effect of this task reported may start, even while other effects are
    /// unresolved), a running attempt, the decision's evidence revision, task authority
    /// against the house's current grants, and that no other effect is
    /// unresolved. The intent records the backend namespace; a repeated
    /// request for the same logical effect must come from that backend.
    /// An uncertain effect is resubmitted with its key only when the backend
    /// declares the effect's kind idempotent
    /// ([`crate::contracts::BackendDescriptor::idempotent`]).
    ///
    /// # Errors
    /// Returns the first failed check.
    pub fn begin_effect(
        &self,
        plan: EffectPlan,
        grants: &HouseGrants,
        executor: &dyn EffectExecutor,
        now: Timestamp,
    ) -> Result<EffectStart> {
        let backend = executor.descriptor();
        self.transact(|state| state.begin_effect(plan, grants, backend, now))
    }

    /// Record what is known about an effect. Only the current fence may
    /// record; repeating a consistent report is a no-op.
    ///
    /// # Errors
    /// Returns [`StateError::ConflictingOutcome`] when the report contradicts
    /// a resolved outcome.
    pub fn record_effect_outcome(
        &self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        outcome: EffectOutcome,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| state.record_effect_outcome(id, fence, seq, outcome, now))
    }

    /// Record what the backend returned for submission number `submission`
    /// of an effect (see [`EffectRecord::submissions`]). A not-applied or
    /// uncertain result from an older submission is ignored, since a newer
    /// submission may still apply; a receipt is accepted from any submission.
    ///
    /// # Errors
    /// As [`Self::record_effect_outcome`].
    pub fn record_submission_outcome(
        &self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        submission: u32,
        outcome: EffectOutcome,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| {
            state.record_submission_outcome(id, fence, seq, submission, outcome, now)
        })
    }

    /// Record a scoped decision about a handed-over effect (one recorded as
    /// [`EffectOutcome::Unresolvable`]). The decision must name the effect's
    /// idempotency key and the task's current evidence revision. Repeating
    /// the same decision is a no-op.
    ///
    /// # Errors
    /// Returns [`StateError::DecisionScope`] for another effect,
    /// [`StateError::StaleDecision`] for an older revision, and
    /// [`StateError::NotHandedOver`] unless the effect is handed over.
    pub fn accept_risk(
        &self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        decision: RiskDecision,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| state.accept_risk(id, fence, seq, decision, now))
    }

    /// Record what a backend lookup proved about one write of a settled task.
    /// A settled task has no lease, so this takes no fence. Only
    /// [`crate::state::reread_settled`] can build a [`SettledLookup`], so the
    /// outcome and any receipt come from the executor, never from a caller.
    ///
    /// An intended, uncertain, handed-over, or waived write becomes applied
    /// or not applied; a waived write keeps its decision in the effect's
    /// decision history. An established outcome is never rewritten: the same
    /// answer is a no-op and a different one is refused.
    ///
    /// # Errors
    /// [`StateError::TaskNotSettled`] while the task is unsettled,
    /// [`StateError::EffectNotFound`] for an unknown write,
    /// [`StateError::LookupScope`] for a lookup of another write, and
    /// [`StateError::ConflictingOutcome`] when the answer contradicts a
    /// recorded one.
    pub(crate) fn record_settled_lookup(
        &self,
        id: &TaskId,
        seq: EffectSeq,
        lookup: SettledLookup,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| state.record_settled_lookup(id, seq, lookup, now))
    }

    /// Record a person's review of the writes of a settled task that did not
    /// succeed. The record names `claimant`, `now`, `reason`, and the writes
    /// whose outcome is still unknown, all taken here rather than from a
    /// caller-built record. The first acknowledgement stays; repeating the
    /// call returns it unchanged. Returns the stored record and whether it
    /// was already there.
    ///
    /// # Errors
    /// [`StateError::AcknowledgementNeedsPerson`] for a non-interactive
    /// claimant, [`StateError::TaskNotSettled`] while the task is unsettled,
    /// [`StateError::TaskSettled`] for a task that settled successfully,
    /// [`StateError::NothingToAcknowledge`] when no write reached or may have
    /// reached the backend, and [`StateError::CapacityExceeded`] for a reason
    /// longer than [`crate::state::MAX_ACKNOWLEDGEMENT_REASON_BYTES`].
    pub(crate) fn acknowledge_settled_writes(
        &self,
        id: &TaskId,
        claimant: &Claimant,
        reason: &Text,
        now: Timestamp,
    ) -> Result<(WriteAcknowledgement, bool)> {
        self.transact(|state| state.acknowledge_settled_writes(id, claimant, reason, now))
    }

    /// Record evidence. A new subject (head or base) starts a new evidence revision
    /// and drops superseded evidence, invalidating decisions made earlier.
    ///
    /// # Errors
    /// Refuses a stale fence and a full evidence log.
    pub fn record_evidence(
        &self,
        id: &TaskId,
        fence: Fence,
        evidence: Evidence,
        now: Timestamp,
    ) -> Result<EvidenceRevision> {
        self.transact(|state| state.record_evidence(id, fence, evidence, now))
    }

    /// Mark an inbound message as consumed, reporting duplicates.
    ///
    /// # Errors
    /// Refuses without live ownership and when the bounded set is full.
    pub fn consume_message(
        &self,
        id: &TaskId,
        fence: Fence,
        message: &ExternalRef,
        now: Timestamp,
    ) -> Result<Consumption> {
        self.transact(|state| state.consume_message(id, fence, message, now))
    }

    /// Acquire the single-consumer lease for a workflow scope. Acquiring a
    /// relinquished scope records an adoption of it.
    ///
    /// # Errors
    /// Returns [`StateError::ClaimHeld`] while another lease is live and
    /// [`StateError::LeaseExpired`] when an expired lease needs a takeover.
    pub fn acquire_consumer(
        &self,
        consumer: &ConsumerId,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.acquire_consumer(consumer, claimant, ttl, now))
    }

    /// Extend a live consumer lease.
    ///
    /// # Errors
    /// Refuses a missing lease, a stale fence, and an expired lease.
    pub fn renew_consumer(
        &self,
        consumer: &ConsumerId,
        fence: Fence,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.renew_consumer(consumer, fence, ttl, now))
    }

    /// Hand the scope over with work possibly in flight. The next consumer
    /// adopts it; until then it waits in the recovery queue.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] unless `fence` holds the lease.
    pub fn relinquish_consumer(
        &self,
        consumer: &ConsumerId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.relinquish_consumer(consumer, fence, now))
    }

    /// Release a consumer lease when nothing is in flight. Repeating the
    /// release, or releasing an unknown scope, is a no-op.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] when another fence holds the lease.
    pub fn release_consumer(
        &self,
        consumer: &ConsumerId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.release_consumer(consumer, fence, now))
    }

    /// Explicitly take over an expired consumer lease. The history records a
    /// takeover, distinct from a relinquish and adoption.
    ///
    /// # Errors
    /// Returns [`StateError::LeaseLive`] while the lease is live.
    pub fn take_over_consumer(
        &self,
        consumer: &ConsumerId,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.take_over_consumer(consumer, claimant, ttl, now))
    }

    /// Read a consumer scope's holder and recent transfers.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn consumer(&self, consumer: &ConsumerId) -> Result<Option<ConsumerRecord>> {
        self.read(|state| state.consumer(consumer).cloned())
    }

    /// Record a workflow marker: a fact about one work item at one exact
    /// evidence subject. Recording the same fact again returns the original
    /// marker; a different fact under the same key is refused. Markers grant
    /// nothing and are separate from effects. When `recorded_by` acts under a
    /// consumer lease, that lease must be current and live.
    ///
    /// # Errors
    /// Returns [`StateError::MarkerConflict`] for a different fact,
    /// [`StateError::CapacityExceeded`] at [`crate::state::MAX_MARKERS`], and
    /// consumer lease errors.
    pub fn record_marker(
        &self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<MarkerRecording> {
        self.transact(|state| state.record_marker(key, fact, recorded_by, now))
    }

    /// Record a workflow marker unless `guard` objects to the workflow's
    /// markers. The guard reads them and the marker is written in one store
    /// transaction, so a concurrent recording cannot slip between the check
    /// and the write. A key that is already recorded is never blocked; it
    /// resolves as in [`Self::record_marker`]. The guard returns the reason
    /// to block, and nothing is written then.
    ///
    /// # Errors
    /// Returns the errors of [`Self::record_marker`] and any the guard returns.
    pub fn record_marker_unless<R>(
        &self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<Option<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.transact(|state| state.record_marker_unless(key, fact, recorded_by, now, guard))
    }

    /// Record `first` and, as the guard decides, `second` in one store
    /// transaction. A refusal or error leaves neither written, so callers
    /// never see one marker without the other they meant to write with it.
    ///
    /// # Errors
    /// Returns the errors of [`Self::record_marker`], for either marker, and
    /// any the guard returns.
    pub(crate) fn record_marker_pair_unless<R>(
        &self,
        first: (MarkerKey, MarkerFact),
        second: (MarkerKey, MarkerFact),
        recorded_by: &Claimant,
        now: Timestamp,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<PairPlan<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.transact(|state| {
            state.record_marker_pair_unless(first, second, recorded_by, now, guard)
        })
    }

    /// Like [`Self::record_marker_unless`], but a key that is already
    /// recorded is guarded too while the task `pending` does not exist. This
    /// lets a redelivery that finishes an interrupted admission be checked
    /// against markers recorded since, in the same transaction.
    ///
    /// # Errors
    /// Returns the errors of [`Self::record_marker_unless`].
    pub fn record_marker_unless_created<R>(
        &self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
        pending: &TaskId,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<Option<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.transact(|state| {
            state.record_marker_unless_created(key, fact, recorded_by, now, pending, guard)
        })
    }

    /// Replace the fact recorded under `key`, but only if it is still
    /// `expected` (compare-and-supersede), for example when a gate's verdict
    /// for the same head changes. The prior fact, its recorder, and times
    /// move to the marker's history, which keeps the newest
    /// [`crate::state::MAX_MARKER_HISTORY`] entries and counts dropped ones;
    /// supersession is never refused for capacity. Superseding with the
    /// current fact is a no-op. Asked questions are append-only.
    ///
    /// # Errors
    /// Returns [`StateError::MarkerNotFound`] without a marker,
    /// [`StateError::MarkerConflict`] when the current fact is not
    /// `expected`, [`StateError::MarkerNotSupersedable`] for a question, and
    /// consumer lease errors.
    pub fn supersede_marker(
        &self,
        key: &MarkerKey,
        expected: &MarkerFact,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<MarkerRecording> {
        self.transact(|state| state.supersede_marker(key, expected, fact, recorded_by, now))
    }

    /// Remove each marker recorded under its key whose fact is still the
    /// expected one (compare-and-remove), in one transaction, and return the
    /// keys removed. A marker that is gone or whose fact changed since it was
    /// read, such as by a concurrent renewal, is kept. Only the workflow that
    /// owns a marker should retire it, once the fact can no longer matter.
    ///
    /// # Errors
    /// Returns [`StateError::MarkerNotSupersedable`] for an asked question,
    /// which is never removed; nothing is removed then.
    pub fn retire_markers(&self, markers: &[(MarkerKey, MarkerFact)]) -> Result<Vec<MarkerKey>> {
        self.transact(|state| state.retire_markers(markers))
    }

    /// Remove settled tasks whose every effect applied or definitely did
    /// not, in one transaction, freeing their places under
    /// [`crate::state::MAX_TASKS`], and return the ones removed; an absent
    /// task is already gone.
    ///
    /// Retiring forgets a task, so its identity can be created again, and a
    /// recreated task derives the same idempotency keys for its effects. Only
    /// the workflow that owns a task may retire it, and only when it can show
    /// it will never create that identity again.
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotRetirable`] for an unsettled task or one
    /// with an unresolved or waived effect; nothing is removed then.
    pub fn retire_tasks(&self, ids: &[TaskId]) -> Result<Vec<TaskId>> {
        self.transact(|state| state.retire_tasks(ids))
    }

    /// Read the marker recorded under `key`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn marker(&self, key: &MarkerKey) -> Result<Option<WorkflowMarker>> {
        self.read(|state| state.marker(key).cloned())
    }

    /// Read every marker a workflow recorded, oldest first.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn markers(&self, workflow: &WorkflowId) -> Result<Vec<WorkflowMarker>> {
        self.read(|state| state.markers(workflow).cloned().collect())
    }

    /// Work that needs an explicit recovery decision: expired owners,
    /// unowned unresolved effects, and unowned pending cancellations.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn recovery_queue(&self, now: Timestamp) -> Result<Vec<RecoveryItem>> {
        self.read(|state| state.recovery_queue(now))
    }

    fn transact<T>(&self, apply: impl FnOnce(&mut StoreState) -> Result<T>) -> Result<T> {
        self.engine.transact(apply)
    }

    fn read<T>(&self, view: impl FnOnce(&StoreState) -> T) -> Result<T> {
        self.engine.read(view)
    }

    /// Run `view` while holding the shared lock, so no transaction on this
    /// store, a takeover included, commits until it returns. `view` may take
    /// another store's lock; that store must never take this lock while
    /// holding its own, or the two could wait on each other until timeout.
    pub(crate) fn read_holding<T>(&self, view: impl FnOnce(&StoreState) -> T) -> Result<T> {
        self.read(view)
    }
}
