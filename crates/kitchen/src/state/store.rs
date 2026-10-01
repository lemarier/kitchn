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
        AttemptNumber, AttemptOutcome, AttemptStart, BackendDescriptor, Claimant, ConsumerFence,
        Delivery, Disposition, EffectExecutor, EffectSeq, Evidence, EvidenceKind, EvidenceRevision,
        ExternalRef, Fence, HouseGrants, IssueNumber, LeaseTtl, RecordedEvidence, TaskSpec, Text,
        Timestamp, VerificationError,
    },
    scheduling::IntervalMinutes,
    state::{
        CancelStatus, ConsumerRecord, Consumption, Creation, EffectOutcome, EffectPlan,
        EffectRecord, EffectStart, Lease, MarkerAttempt, MarkerFact, MarkerKey, MarkerRecording,
        RecoveryItem, Reservation, RiskDecision, TaskRecord, WorkflowMarker, WriteAcknowledgement,
        effects::SettledLookup,
        mailbox::{
            AnswerState, Answered, MAX_MAILBOX_MESSAGES, MailAnswer, MailSender, OpenQuestion,
            WorkerPost,
        },
        marker::{MarkerWrite, PairPlan},
        model::StoreState,
        retention::{
            Inventory, RetentionPolicy, RetentionReport, RetentionSubjects, StoreCapacity,
            TableUsage,
        },
        runs::{RunId, RunRecord, RunSettle, RunStart},
        snapshot::{SnapshotStore, StoreLayout, StoreOptions},
        usage::{AttemptUsageEntry, UsageReport},
    },
    workflows::tick::{Pass, PassReport},
};

#[cfg(doc)]
use crate::{
    contracts::ContractError,
    state::MailError,
    state::{AttemptUsage, StateError, UsageError},
    workflows::tick::{MAX_PASS_RUNTIME, TickError},
};

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

    /// The store's directory, which `kitchn` commands name with `--store`.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.engine.dir()
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

    /// Finish a running attempt as exhausted when its workflow's durable
    /// subject budget has expired across task generations. The caller must
    /// verify that deadline; unresolved effects still prevent settlement.
    ///
    /// # Errors
    /// Refuses a stale claim, a non-running attempt, or unresolved effects.
    pub(crate) fn finish_attempt_exhausted(
        &self,
        id: &TaskId,
        fence: Fence,
        attempt: AttemptNumber,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.finish_attempt_exhausted(id, fence, attempt, now))
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
    /// Refuses a stale fence and a full evidence log, and refuses
    /// [`EvidenceKind::AuthorizedVerification`] with
    /// [`VerificationError::NotRun`]: only
    /// [`crate::state::run_verification`] records that kind.
    pub fn record_evidence(
        &self,
        id: &TaskId,
        fence: Fence,
        evidence: Evidence,
        now: Timestamp,
    ) -> Result<EvidenceRevision> {
        match evidence.kind {
            EvidenceKind::AuthorizedVerification(_) => Err(VerificationError::NotRun.into()),
            EvidenceKind::Check
            | EvidenceKind::WorkerReport(_)
            | EvidenceKind::Verification(_)
            | EvidenceKind::ForgeMerge(_) => {
                self.transact(|state| state.record_evidence(id, fence, evidence, now))
            }
        }
    }

    /// Record the result of a verification run. Only
    /// [`crate::state::run_verification`] calls this.
    pub(crate) fn record_verification(
        &self,
        id: &TaskId,
        fence: Fence,
        evidence: Evidence,
        now: Timestamp,
    ) -> Result<EvidenceRevision> {
        self.transact(|state| state.record_evidence(id, fence, evidence, now))
    }

    /// The task `fence` owns with a live lease, for starting a verification
    /// run.
    pub(crate) fn verification_task(
        &self,
        id: &TaskId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<TaskRecord> {
        self.read(|state| state.verification_task(id, fence, now).cloned())?
    }

    /// A task's current evidence, for [`VerificationReport::evaluate`].
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotFound`] or a storage error.
    ///
    /// [`VerificationReport::evaluate`]: crate::contracts::VerificationReport::evaluate
    pub fn recorded_evidence(&self, id: &TaskId) -> Result<RecordedEvidence> {
        self.read(|state| {
            state
                .task(id)
                .map(|task| RecordedEvidence::new(task.evidence().items().to_vec()))
        })?
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

    /// Record what `backend` reported for attempt `attempt`. The current
    /// owner records it, or the owner that settled the task, since a report
    /// often arrives after the attempt ended. Repeating the same report
    /// changes nothing. An attempt without a report stays
    /// [`AttemptUsage::NotReported`].
    ///
    /// # Errors
    /// [`ContractError::CrossHouse`] for another house's backend,
    /// [`ContractError::UnsupportedCapabilities`] when `backend` does not
    /// declare [`Capability::UsageAttribution`], [`UsageError::EmptyReport`],
    /// [`UsageError::AlreadyReported`] for a different report,
    /// [`StateError::AttemptNotFound`], and [`StateError::StaleFence`].
    ///
    /// [`Capability::UsageAttribution`]: crate::contracts::Capability::UsageAttribution
    pub fn record_attempt_usage(
        &self,
        id: &TaskId,
        fence: Fence,
        attempt: AttemptNumber,
        backend: &BackendDescriptor,
        report: UsageReport,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.record_attempt_usage(id, fence, attempt, backend, report, now))
    }

    /// Record that a person's reply to worker question `question`, asked at
    /// `asked_at`, was delivered now, in the running attempt. Record only
    /// replies a person gave, not a coordinator's own answers. Recording the
    /// same question again changes nothing.
    ///
    /// # Errors
    /// [`UsageError::ReplyBeforeQuestion`] when `asked_at` is after `now`,
    /// [`UsageError::TooManyReplies`], [`StateError::NoRunningAttempt`], and
    /// ownership errors.
    pub fn record_human_reply(
        &self,
        id: &TaskId,
        fence: Fence,
        question: &ExternalRef,
        asked_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.record_human_reply(id, fence, question, asked_at, now))
    }

    /// Record that a person's reply to worker question `question`, asked at
    /// `asked_at`, was delivered at `answered_at` to the worker of attempt
    /// `attempt`, whether or not that attempt has ended: a delivered reply
    /// found after a restart belongs to the attempt it reached. The current
    /// owner records it, or the owner that settled the task. Recording the
    /// same question again changes nothing.
    ///
    /// # Errors
    /// [`UsageError::ReplyBeforeQuestion`] when `asked_at` is after
    /// `answered_at`, [`UsageError::TooManyReplies`],
    /// [`StateError::AttemptNotFound`], and [`StateError::StaleFence`].
    pub fn record_attempt_reply(
        &self,
        id: &TaskId,
        fence: Fence,
        attempt: AttemptNumber,
        question: &ExternalRef,
        asked_at: Timestamp,
        answered_at: Timestamp,
    ) -> Result<()> {
        self.transact(|state| {
            state.record_attempt_reply(id, fence, attempt, question, asked_at, answered_at)
        })
    }

    /// Link the task to pull request `number` in its repository. The owner
    /// that recorded usage may link it; linking the same one again changes
    /// nothing.
    ///
    /// # Errors
    /// [`UsageError::NoRepository`], [`UsageError::PullRequestConflict`]
    /// when another pull request is linked, and [`StateError::StaleFence`].
    pub fn link_pull_request(&self, id: &TaskId, fence: Fence, number: IssueNumber) -> Result<()> {
        self.transact(|state| state.link_pull_request(id, fence, number))
    }

    /// Every attempt's usage in this house, with its task, station, work
    /// type, pull request, and derived human time.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn attempt_usage(&self) -> Result<Vec<AttemptUsageEntry>> {
        self.read(StoreState::attempt_usage)
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

    /// Like [`Self::record_marker_unless`], recorded by the owner of `task`'s
    /// live claim at `fence`. The claim is checked in the same transaction as
    /// the guard and the write, so an owner whose claim expired or was taken
    /// over, or whose task settled, since it read the task writes nothing.
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotFound`], [`StateError::TaskSettled`],
    /// [`StateError::StaleFence`] for another claim or an open task,
    /// [`StateError::LeaseExpired`], and the errors of
    /// [`Self::record_marker_unless`].
    pub fn record_task_marker_unless<R>(
        &self,
        key: MarkerKey,
        fact: MarkerFact,
        task: &TaskId,
        fence: Fence,
        now: Timestamp,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<Option<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.transact(|state| state.record_task_marker_unless(key, fact, task, fence, now, guard))
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

    /// Like [`Self::supersede_marker`], by the owner of `task`'s live claim at
    /// `fence`, checked in the same transaction as the write.
    ///
    /// # Errors
    /// Returns the claim errors of [`Self::record_task_marker_unless`] and
    /// the errors of [`Self::supersede_marker`].
    pub fn supersede_task_marker(
        &self,
        key: &MarkerKey,
        expected: &MarkerFact,
        fact: MarkerFact,
        task: &TaskId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<MarkerRecording> {
        self.transact(|state| state.supersede_task_marker(key, expected, fact, task, fence, now))
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

    /// How full the store's shared tables are, for doctor and operators.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn capacity(&self) -> Result<StoreCapacity> {
        self.read(StoreState::capacity)
    }

    /// The issues, pull requests, and backends whose observation would let
    /// a retention pass remove something. Observe them into an
    /// [`Inventory`] before calling [`Self::retain`].
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn retention_subjects(&self) -> Result<RetentionSubjects> {
        self.read(StoreState::retention_subjects)
    }

    /// Preview a retention pass: what [`Self::retain`] would remove now.
    /// Writes nothing.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn preview_retention(
        &self,
        policy: &RetentionPolicy,
        inventory: &Inventory,
        now: Timestamp,
    ) -> Result<RetentionReport> {
        self.read(|state| state.retention_plan(policy, inventory, now))
    }

    /// Apply the house retention policy in one transaction (see
    /// [`crate::state::RetentionPolicy`]). Markers and settled tasks are
    /// removed only on the positive evidence in `inventory`; anything a
    /// claim, an unresolved effect, an unacknowledged failed write, a live
    /// resource, or a remaining marker still needs is kept. In the same
    /// transaction every repository's intake markers are compacted, recorded
    /// by `recorded_by` (see [`crate::workflows::intake::IntakeLedger::compact`]);
    /// a repository whose intake markers cannot be read is reported refused
    /// and left unchanged.
    ///
    /// # Errors
    /// Returns a storage error, or a consumer lease error for
    /// `recorded_by`; nothing is changed then.
    pub fn retain(
        &self,
        policy: &RetentionPolicy,
        inventory: &Inventory,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<RetentionReport> {
        self.transact(|state| state.retain(policy, inventory, recorded_by, now))
    }

    /// Replace markers with fewer ones in one transaction: remove each of
    /// `retire`, whose facts must be unchanged, then apply `writes`. A
    /// changed or missing marker, a conflicting write, or a full store
    /// refuses the whole call.
    ///
    /// # Errors
    /// [`StateError::MarkerConflict`] when a marker changed since it was
    /// read, [`StateError::MarkerNotSupersedable`] for an asked question,
    /// [`StateError::CapacityExceeded`], and consumer lease errors.
    pub(crate) fn compact_markers(
        &self,
        retire: &[(MarkerKey, MarkerFact)],
        writes: Vec<MarkerWrite>,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.compact_markers(retire, writes, recorded_by, now))
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

    /// Post a worker's question, report, or escalation into the house
    /// mailbox and return its message id.
    ///
    /// # Errors
    /// [`MailError::NoOpenAttempt`] or [`MailError::NotSender`] unless
    /// `sender` holds the task's open attempt, [`MailError::Full`],
    /// [`MailError::TooLarge`], and [`StateError::TaskNotFound`] for a task
    /// not in this house's store.
    pub fn post_mail(
        &self,
        sender: &MailSender,
        post: WorkerPost,
        now: Timestamp,
    ) -> Result<ExternalRef> {
        self.transact(|state| state.post_mail(sender, post, now))
    }

    /// Post one idempotent coordinator question for a running task. The
    /// subject is its stable key within the attempt; conflicting text is
    /// refused and an exact replay returns the first message id.
    pub fn post_mail_unique(
        &self,
        sender: &MailSender,
        post: WorkerPost,
        now: Timestamp,
    ) -> Result<ExternalRef> {
        self.transact(|state| state.post_mail_unique(sender, post, now))
    }

    /// The answer to `question`, for the worker that asked it.
    ///
    /// # Errors
    /// As for [`Self::post_mail`], plus [`MailError::UnknownMessage`] for a
    /// message another task or attempt posted, and
    /// [`MailError::NotAQuestion`].
    pub fn mail_answer(&self, sender: &MailSender, question: &ExternalRef) -> Result<AnswerState> {
        self.read(|state| state.mail_answer(sender, question))?
    }

    /// Answer a worker question. A person's answer is recorded as a human
    /// reply on the asking attempt ([`Self::record_attempt_reply`]).
    ///
    /// # Errors
    /// [`MailError::UnknownMessage`], [`MailError::NotAQuestion`],
    /// [`MailError::AlreadyAnswered`] for a different earlier answer,
    /// [`MailError::NoOwner`] for a person's answer while nobody owns the
    /// task, and the reply's recording errors.
    pub fn answer_mail(&self, question: &ExternalRef, answer: MailAnswer) -> Result<Answered> {
        self.transact(|state| state.answer_mail(question, answer))
    }

    /// Unanswered questions, oldest first, at most `limit`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn open_questions(&self, limit: usize) -> Result<Vec<OpenQuestion>> {
        self.read(|state| state.open_questions(limit))?
    }

    /// How full the house mailbox is.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn mailbox_usage(&self) -> Result<TableUsage> {
        self.read(|state| TableUsage {
            used: state.mailbox_len(),
            limit: MAX_MAILBOX_MESSAGES,
        })
    }

    pub(crate) fn mail_last_posted(&self) -> Result<u64> {
        self.read(StoreState::mail_last_posted)
    }

    pub(crate) fn adopt_mailbox(&self, reader: &ConsumerFence, now: Timestamp) -> Result<()> {
        self.transact(|state| state.adopt_mailbox(reader, now))
    }

    pub(crate) fn mail_delivery(
        &self,
        reader: &ConsumerFence,
        now: Timestamp,
    ) -> Result<Option<Delivery>> {
        self.transact(|state| state.mail_delivery(reader, now))
    }

    pub(crate) fn acknowledge_mail(
        &self,
        reader: &ConsumerFence,
        delivery: &ExternalRef,
        now: Timestamp,
    ) -> Result<Option<Delivery>> {
        self.transact(|state| state.acknowledge_mail(reader, delivery, now))
    }

    /// Start a tick pass when it is due: record a run whose pass lease
    /// expired as uncertain, then take the lease and record a running entry,
    /// in one transaction. An uncertain run of the pass comes back as
    /// [`RunStart::Blocked`], holding no lease, until a person settles it
    /// with [`Self::settle_run`]. A live lease is [`RunStart::Busy`] and
    /// changes nothing, so a duplicate trigger is harmless.
    ///
    /// # Errors
    /// Consumer capacity and storage errors.
    pub fn start_run(
        &self,
        pass: Pass,
        every: IntervalMinutes,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<RunStart> {
        self.transact(|state| state.start_run(pass, every, holder, ttl, now))
    }

    /// Record that a person settled an uncertain run of `pass`: who, when,
    /// and why, with the unresolved effects of the tasks the run recorded.
    /// The pass may then run again when due. Settling a settled run returns
    /// the first record unchanged.
    ///
    /// Check what the run did first: it may have acted without recording a
    /// task, so no unresolved effect does not mean it did nothing.
    ///
    /// # Errors
    /// [`TickError::SettleNeedsPerson`] unless `claimant` is interactive,
    /// [`StateError::CapacityExceeded`] for a reason longer than
    /// [`crate::state::MAX_ACKNOWLEDGEMENT_REASON_BYTES`],
    /// [`TickError::UnknownRun`] for no such run of `pass`,
    /// [`TickError::NotUncertain`] for a running or ended run, and storage
    /// errors.
    pub fn settle_run(
        &self,
        pass: Pass,
        run: RunId,
        claimant: &Claimant,
        reason: &Text,
        now: Timestamp,
    ) -> Result<RunSettle> {
        self.transact(|state| state.settle_run(pass, run, claimant, reason, now))
    }

    /// Extend a live run's pass lease by `ttl`, so a slow pass is not taken
    /// over. The lease never extends past [`MAX_PASS_RUNTIME`] after the run
    /// started; returns the new expiry.
    ///
    /// # Errors
    /// [`TickError::Superseded`] once the run's lease lapsed, its runtime
    /// ran out, or another tick recorded it as uncertain;
    /// [`TickError::NotRunOwner`], [`TickError::AlreadyFinished`],
    /// [`TickError::UnknownRun`], and storage errors.
    pub fn renew_run(
        &self,
        run: RunId,
        fence: Fence,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Timestamp> {
        self.transact(|state| state.renew_run(run, fence, ttl, now))
    }

    /// Record that a live run is about to touch `task`, before it creates
    /// intent or effects for it. A blocked run reports these tasks'
    /// unresolved effects. Repeating is a no-op.
    ///
    /// # Errors
    /// The errors of [`Self::renew_run`], and [`TickError::TooManyTasks`].
    pub fn record_run_task(
        &self,
        run: RunId,
        fence: Fence,
        task: &TaskId,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.record_run_task(run, fence, task, now))
    }

    /// Record how a live run ended and release its pass lease. Repeating
    /// the same end is a no-op.
    ///
    /// # Errors
    /// [`TickError::Superseded`] once the run's lease lapsed, its runtime
    /// ran out, or another tick recorded it as uncertain; nothing changes
    /// then. [`TickError::UnknownRun`], [`TickError::NotRunOwner`] for
    /// another fence, [`TickError::AlreadyFinished`] for a different earlier
    /// end, and [`TickError::TooMuchEvidence`].
    pub fn finish_run(
        &self,
        run: RunId,
        fence: Fence,
        report: PassReport,
        now: Timestamp,
    ) -> Result<()> {
        self.transact(|state| state.finish_run(run, fence, report, now))
    }

    /// The run ledger, oldest first.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn runs(&self) -> Result<Vec<RunRecord>> {
        self.read(|state| state.runs().to_vec())
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
