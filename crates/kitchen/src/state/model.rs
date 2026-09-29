//! Persisted records and their transitions.
//!
//! Every transition runs on an in-memory copy inside one locked store
//! transaction. An error discards the copy, so a failed transition never
//! persists a partial change.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, CredentialId, EffectName, Error, HolderId, HouseId, TaskId, WorkflowId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, Authorization, BackendDescriptor, Capability,
        Claimant, Consent, ConsumerFence, ContractError, Disposition, Effect, EffectContext,
        EffectRequest, EffectSeq, Evidence, EvidenceRevision, EvidenceSubject, ExternalRef,
        FailureClass, Fence, HouseGrants, IdempotencyKey, LeaseTtl, NotAppliedReason, Operation,
        Receipt, ResourceKind, ResourceRef, RetryPolicy, ScheduleEffect, ScheduleRequirements,
        Settlement, SubmittedEffects, TaskSpec, Text, Timestamp, Trigger, UncertainReason,
    },
    state::{
        ConsumerEvent, ConsumerRecord, ConsumerState, Corruption, Limit, MarkerAttempt, MarkerFact,
        MarkerKey, MarkerRecording, StateError, WorkItem, WorkflowMarker,
        effects::{Found, SettledLookup},
        marker::{MarkerRefusal, MarkerWrite, Markers, PairPlan},
        retention::{
            self, Inventory, RetentionPolicy, RetentionReport, RetentionSubjects, StoreCapacity,
        },
    },
    workflows::intake,
};

/// The persisted schema version.
pub(crate) const SCHEMA_VERSION: u64 = 1;
/// Tasks per house store, settled ones included. Settled tasks are kept so
/// their identities and idempotency keys are never reused, until the workflow
/// that owns one retires it (see [`crate::state::HouseStore::retire_tasks`]).
pub const MAX_TASKS: usize = 4096;
/// Consumer leases per house store.
pub const MAX_CONSUMERS: usize = 256;
/// Effects per task.
pub const MAX_EFFECTS_PER_TASK: usize = 256;
/// Evidence items kept for the current evidence revision.
pub const MAX_EVIDENCE_PER_REVISION: usize = 128;
/// Risk decisions kept per effect, expired ones included.
pub const MAX_DECISIONS_PER_EFFECT: usize = 16;
/// Ownership history entries per task.
pub const MAX_OWNERSHIP_HISTORY: usize = 256;
/// Consumed message ids remembered per task.
pub const MAX_CONSUMED_MESSAGES: usize = 1024;
/// Bytes in the reason of a [`WriteAcknowledgement`].
pub const MAX_ACKNOWLEDGEMENT_REASON_BYTES: usize = 4096;

type Result<T> = std::result::Result<T, Error>;

fn fail<T>(error: StateError) -> Result<T> {
    Err(Error::State(error))
}

/// A time-limited ownership grant with its fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lease {
    holder: HolderId,
    trigger: Trigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    consumer: Option<ConsumerFence>,
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

    /// The trigger the owner acts under, which decides where its effects'
    /// authority comes from.
    #[must_use]
    pub const fn trigger(&self) -> &Trigger {
        &self.trigger
    }

    /// The workflow consumer lease the owner acts under, if any.
    #[must_use]
    pub const fn consumer(&self) -> Option<&ConsumerFence> {
        self.consumer.as_ref()
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
    /// Handed over: the owner reported that the outcome cannot be
    /// established. This is not a resolution. The task keeps its reservation
    /// and cannot start new work or settle until positive evidence arrives
    /// or a [`RiskDecision`] authorizes one specific action.
    Unresolvable {
        /// When recorded.
        at: Timestamp,
    },
    /// A handed-over effect whose outcome is still unknown, with a scoped
    /// decision about how the task may proceed. Resources it may have created
    /// keep uncertain ownership.
    Waived {
        /// The decision.
        decision: RiskDecision,
        /// When recorded.
        at: Timestamp,
    },
}

impl EffectState {
    /// Whether the outcome is established: applied or not applied.
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        match self {
            Self::Applied { .. } | Self::NotApplied { .. } => true,
            Self::Intended
            | Self::Uncertain { .. }
            | Self::Unresolvable { .. }
            | Self::Waived { .. } => false,
        }
    }

    /// The decision covering this effect at evidence revision `current`.
    /// A decision made at another revision has expired: the effect is handed
    /// over again until a new decision or positive evidence.
    const fn current_decision(&self, current: EvidenceRevision) -> Option<&RiskDecision> {
        match self {
            Self::Waived { decision, .. } if decision.revision.get() == current.get() => {
                Some(decision)
            }
            Self::Waived { .. }
            | Self::Intended
            | Self::Uncertain { .. }
            | Self::Applied { .. }
            | Self::NotApplied { .. }
            | Self::Unresolvable { .. } => None,
        }
    }

    /// Whether this is handed over without a current decision.
    const fn is_handed_over(&self, current: EvidenceRevision) -> bool {
        match self {
            Self::Unresolvable { .. } => true,
            Self::Waived { .. } => self.current_decision(current).is_none(),
            Self::Intended
            | Self::Uncertain { .. }
            | Self::Applied { .. }
            | Self::NotApplied { .. } => false,
        }
    }

    /// Whether the outcome still needs evidence and no current decision covers it.
    const fn needs_outcome(&self, current: EvidenceRevision) -> bool {
        match self {
            Self::Intended | Self::Uncertain { .. } => true,
            Self::Unresolvable { .. } | Self::Waived { .. } => self.is_handed_over(current),
            Self::Applied { .. } | Self::NotApplied { .. } => false,
        }
    }

    /// Whether this effect prevents new attempts and new effects.
    const fn blocks_work(&self, current: EvidenceRevision) -> bool {
        match self.current_decision(current) {
            Some(decision) => match decision.action {
                RiskAction::ContinueWork => false,
                RiskAction::SettleUnsuccessfully => true,
            },
            None => self.needs_outcome(current),
        }
    }

    /// Whether this effect prevents settling the task, successfully or not.
    const fn blocks_settlement(&self, success: bool, current: EvidenceRevision) -> bool {
        match self.current_decision(current) {
            Some(decision) => match decision.action {
                RiskAction::ContinueWork => false,
                RiskAction::SettleUnsuccessfully => success,
            },
            None => self.needs_outcome(current),
        }
    }
}

/// What a [`RiskDecision`] allows for a handed-over effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskAction {
    /// Accept that the effect may have happened and continue: new attempts
    /// and effects may start, which can duplicate the unknown effect.
    ContinueWork,
    /// Stop: the task may only settle as failed or cancelled.
    SettleUnsuccessfully,
}

/// A decision, made outside the owner's own report, about one handed-over
/// effect. It is bound to the effect's idempotency key and to the evidence
/// revision it was made at; who may decide is house policy enforced by the
/// caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RiskDecision {
    /// The effect the decision is about.
    pub effect: IdempotencyKey,
    /// Who decided.
    pub decided_by: HolderId,
    /// The evidence revision the decision was made at.
    pub revision: EvidenceRevision,
    /// What it allows.
    pub action: RiskAction,
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
    /// Unknown, and the owner cannot establish it: hand the effect over.
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
    authorization: Authorization,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    basis: Option<ExternalRef>,
    submissions: u32,
    decisions: Vec<RiskDecision>,
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

    /// Where the effect's authority came from.
    #[must_use]
    pub const fn authorization(&self) -> &Authorization {
        &self.authorization
    }

    /// The evidence the decision rested on, as its plan named it.
    #[must_use]
    pub const fn basis(&self) -> Option<&ExternalRef> {
        self.basis.as_ref()
    }

    /// Every risk decision accepted for this effect, oldest first,
    /// including expired ones.
    #[must_use]
    pub fn decisions(&self) -> &[RiskDecision] {
        &self.decisions
    }

    /// How many times the request was handed to the backend under its key.
    #[must_use]
    pub const fn submissions(&self) -> u32 {
        self.submissions
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
        /// The trigger the new holder acts under.
        trigger: Trigger,
        /// New fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// A holder claimed a task its previous owner relinquished.
    Adopted {
        /// The relinquished fence.
        previous: Fence,
        /// New holder.
        holder: HolderId,
        /// The trigger the new holder acts under.
        trigger: Trigger,
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
        /// The trigger the new holder acts under.
        trigger: Trigger,
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
    subject: Option<EvidenceSubject>,
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
    pub const fn subject(&self) -> Option<&EvidenceSubject> {
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

/// A person's review of the forge writes of a settled task that did not
/// succeed, recorded so that a guard that holds a subject because of those
/// writes can release it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteAcknowledgement {
    /// The person's session that acknowledged.
    pub by: HolderId,
    /// When.
    pub at: Timestamp,
    /// Why the person is satisfied to proceed.
    pub reason: Text,
    /// Writes whose outcome the forge could not prove at this time. The
    /// person accepted that they may or may not exist.
    #[serde(default)]
    pub unresolved: Vec<EffectName>,
}

/// The durable record of one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRecord {
    spec: TaskSpec,
    created_by: Claimant,
    created_at: Timestamp,
    state: TaskState,
    attempts: Vec<AttemptRecord>,
    effects: Vec<EffectRecord>,
    evidence: EvidenceLog,
    ownership: Vec<OwnershipEvent>,
    consumed: BTreeSet<ExternalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancel: Option<CancelRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acknowledgement: Option<WriteAcknowledgement>,
}

impl TaskRecord {
    /// The person's review of this settled task's forge writes, if recorded.
    #[must_use]
    pub const fn write_acknowledgement(&self) -> Option<&WriteAcknowledgement> {
        self.acknowledgement.as_ref()
    }

    /// Whether `message` was consumed for this task
    /// ([`crate::state::HouseStore::consume_message`]).
    #[must_use]
    pub fn has_consumed(&self, message: &ExternalRef) -> bool {
        self.consumed.contains(message)
    }

    /// The immutable specification.
    #[must_use]
    pub const fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    /// Who created the task, and under which trigger.
    #[must_use]
    pub const fn created_by(&self) -> &Claimant {
        &self.created_by
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

    /// Effects whose outcome is unknown and not covered by a decision:
    /// intended, uncertain, or handed over.
    pub fn unresolved_effects(&self) -> impl Iterator<Item = &EffectRecord> {
        self.effects
            .iter()
            .filter(|effect| effect.state.needs_outcome(self.evidence.revision))
    }

    fn blocking_work(&self) -> usize {
        self.effects
            .iter()
            .filter(|effect| effect.state.blocks_work(self.evidence.revision))
            .count()
    }

    fn blocking_settlement(&self, success: bool) -> usize {
        self.effects
            .iter()
            .filter(|effect| {
                effect
                    .state
                    .blocks_settlement(success, self.evidence.revision)
            })
            .count()
    }

    /// Whether a decision limits the task to an unsuccessful settlement.
    fn must_settle_unsuccessfully(&self) -> bool {
        self.effects.iter().any(|effect| {
            effect
                .state
                .current_decision(self.evidence.revision)
                .is_some_and(|decision| decision.action == RiskAction::SettleUnsuccessfully)
        })
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

    /// Handle a repeated request for the logical effect at `index`.
    fn repeat_effect(
        &mut self,
        index: usize,
        plan: &EffectPlan,
        backend: &BackendDescriptor,
        credential: &CredentialId,
        current: std::result::Result<(), ContractError>,
        now: Timestamp,
    ) -> Result<EffectStart> {
        let resubmission = Resubmission::for_effect(backend, &plan.effect);
        let Some(existing) = self.effects.get(index) else {
            return fail(StateError::CorruptState(Corruption::EffectSequence));
        };
        if existing.request.effect() != &plan.effect || existing.basis != plan.basis {
            return fail(StateError::EffectNameConflict(existing.seq));
        }
        if existing.request.backend() != &backend.backend {
            return fail(StateError::BackendMismatch {
                seq: existing.seq,
                recorded: existing.request.backend().clone(),
            });
        }
        let reconciled = match &existing.state {
            EffectState::Applied { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => {
                return Ok(EffectStart::Resolved(existing.clone()));
            }
            EffectState::NotApplied { .. } => {
                return fail(StateError::EffectNameConflict(existing.seq));
            }
            EffectState::Intended => false,
            EffectState::Uncertain { reason, .. } => reason.is_from_lookup(),
        };
        match resubmission {
            Resubmission::Refuse => fail(StateError::UnsafeRetry(existing.seq)),
            Resubmission::SameKey if !reconciled => {
                Ok(EffectStart::ReconcileFirst(existing.clone()))
            }
            // Resubmitting reuses the persisted request; it must still be
            // exactly what current authority permits.
            Resubmission::SameKey if existing.request.credential() != credential => {
                Err(ContractError::CredentialChanged.into())
            }
            Resubmission::SameKey => {
                // The persisted effect must still be valid now, such as a
                // decision request's binding to the current evidence
                // revision; the key and payload are kept as persisted.
                current?;
                let (name, attempt, seq) = (
                    existing.name.clone(),
                    existing.request.attempt(),
                    existing.seq,
                );
                // One budget for the logical effect, across every key it used.
                self.check_submission_budget(&name, attempt, seq, now)?;
                let Some(existing) = self.effects.get_mut(index) else {
                    return fail(StateError::CorruptState(Corruption::EffectSequence));
                };
                existing.submissions = existing.submissions.saturating_add(1);
                // The lookup that allowed this resubmission is consumed: a
                // concurrent caller must reconcile again before another.
                existing.state = EffectState::Intended;
                Ok(EffectStart::Execute(existing.clone()))
            }
        }
    }

    /// Check the retry policy's submission bound before another submission
    /// of the logical effect `name` in `attempt`: submissions under every key
    /// it used count, and time runs from its first intent.
    fn check_submission_budget(
        &self,
        name: &EffectName,
        attempt: AttemptNumber,
        seq: EffectSeq,
        now: Timestamp,
    ) -> Result<()> {
        let retry = self.spec.retry;
        let earlier = self
            .effects
            .iter()
            .filter(|effect| &effect.name == name && effect.request.attempt() == attempt);
        let mut submitted = 0_u32;
        let mut first = None;
        for effect in earlier {
            submitted = submitted.saturating_add(effect.submissions);
            first.get_or_insert(effect.intended_at);
        }
        let elapsed = first.map_or(Duration::ZERO, |first| now.saturating_since(first));
        if submitted >= retry.max_attempts() || elapsed > retry.max_elapsed() {
            return fail(StateError::SubmissionBudgetExhausted(seq));
        }
        Ok(())
    }

    /// A consent authorizes one logical effect; it cannot authorize another.
    fn check_consent_unused(
        &self,
        consent: &Consent,
        name: &EffectName,
        attempt: AttemptNumber,
    ) -> Result<()> {
        let reused = self.effects.iter().any(|effect| {
            matches!(&effect.authorization, Authorization::Consent { id, .. } if *id == consent.id)
                && (&effect.name != name || effect.request.attempt() != attempt)
        });
        if reused {
            return fail(StateError::ConsentReused);
        }
        Ok(())
    }

    /// Effects submitted, or possibly submitted, per executor family: every
    /// recorded effect except those established as not applied.
    fn submitted_effects(&self) -> SubmittedEffects {
        let mut submitted = SubmittedEffects::default();
        for effect in &self.effects {
            match effect.state {
                EffectState::NotApplied { .. } => {}
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Applied { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. } => submitted.add(effect.request.effect().executor()),
            }
        }
        submitted
    }

    /// Whether the task was given `resource` or an applied effect of this
    /// task created it. Touching a resource does not transfer it.
    fn owns_resource(&self, resource: &ResourceRef) -> bool {
        self.spec.resources.contains(resource)
            || self.effects.iter().any(|effect| {
                matches!(&effect.state, EffectState::Applied { receipt, .. }
                    if receipt.created().contains(resource))
            })
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

/// Result of [`crate::state::HouseStore::reserve_task`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reservation<R> {
    /// The task did not exist. It was created and claimed in one
    /// transaction, so it already holds its slot.
    Reserved(Lease),
    /// An identical task already existed and was open or had an expired
    /// claim. The guard passed and it was claimed (or taken over) in the same
    /// transaction, so it holds its slot again.
    Resumed(Lease),
    /// An identical task already existed and nothing was written: it has
    /// settled, or another claimant holds a live claim.
    Existing,
    /// The guard objected to the tasks as they were in the same transaction;
    /// nothing was written.
    Blocked(R),
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
enum Resubmission {
    /// The backend cannot deduplicate; refuse.
    Refuse,
    /// The backend deduplicates by key; resubmit with the same key.
    SameKey,
}

impl Resubmission {
    /// Same-key resubmission only where the executor declares `effect`'s
    /// kind idempotent.
    fn for_effect(backend: &BackendDescriptor, effect: &Effect) -> Self {
        if backend.idempotent(effect) {
            Self::SameKey
        } else {
            Self::Refuse
        }
    }
}

/// What a caller must do after [`crate::state::HouseStore::begin_effect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectStart {
    /// Intent is persisted; execute this request and record the outcome.
    /// For a resubmission, the record's submission count is already raised.
    Execute(EffectRecord),
    /// The logical effect has an unknown outcome from a submission. Look up
    /// its key and record the result before asking again; it is resubmitted
    /// only if the lookup cannot establish the outcome.
    ReconcileFirst(EffectRecord),
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
    pub effect: Effect,
    /// The person's consent for exactly this effect. Required under an
    /// interactive claim and refused under a scheduled one.
    pub consent: Option<Consent>,
    /// A reference to the evidence the decision rested on, such as the
    /// digest of a preview a person approved. Recorded with the effect for
    /// audit only; the store neither interprets nor authorizes by it.
    pub basis: Option<ExternalRef>,
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
    /// An effect was handed over: its outcome cannot be established, and the
    /// task waits for positive evidence or a [`RiskDecision`].
    HandedOver {
        /// The task.
        task: TaskId,
        /// The effect.
        seq: EffectSeq,
    },
    /// An unowned task has a pending cancellation that needs an owner to settle it.
    PendingCancellation {
        /// The task.
        task: TaskId,
    },
    /// A consumer relinquished its scope; the next consumer adopts it.
    AwaitingAdoption {
        /// The consumer scope.
        consumer: ConsumerId,
        /// The relinquishing holder.
        holder: HolderId,
        /// When it was relinquished.
        since: Timestamp,
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
    consumers: BTreeMap<ConsumerId, ConsumerRecord>,
    #[serde(default)]
    markers: Markers,
    /// The workflow requirements of each schedule a Kitchen install created
    /// or reused, kept apart from tasks so retention cannot drop them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    schedules: Vec<InstalledRequirements>,
    /// The last item a retention pass looked up, so the next bounded pass
    /// continues after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retention_cursor: Option<WorkItem>,
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

/// The workflow requirements recorded for one installed schedule. Activating
/// or trying it rechecks them against the executor; removing it forgets them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstalledRequirements {
    schedule: ResourceRef,
    requires: BTreeSet<Capability>,
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
            markers: Markers::new(),
            schedules: Vec::new(),
            retention_cursor: None,
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

    fn new_lease(&mut self, claimant: &Claimant, ttl: LeaseTtl, now: Timestamp) -> Lease {
        Lease {
            holder: claimant.holder.clone(),
            trigger: claimant.trigger.clone(),
            consumer: claimant.consumer.clone(),
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

    pub(crate) fn consumer(&self, id: &ConsumerId) -> Option<&ConsumerRecord> {
        self.consumers.get(id)
    }

    /// Require that `fence` is the current, live lease of its consumer.
    fn check_consumer(&self, fence: &ConsumerFence, now: Timestamp) -> Result<()> {
        match self
            .consumers
            .get(&fence.consumer)
            .map(ConsumerRecord::state)
        {
            None => fail(StateError::ConsumerNotFound(fence.consumer.clone())),
            Some(ConsumerState::Held { lease }) if lease.fence == fence.fence => {
                if lease.is_live(now) {
                    Ok(())
                } else {
                    fail(StateError::LeaseExpired {
                        expired_at: lease.expires_at,
                    })
                }
            }
            Some(
                ConsumerState::Held { .. }
                | ConsumerState::Relinquished { .. }
                | ConsumerState::Idle,
            ) => fail(StateError::StaleFence {
                presented: fence.fence,
            }),
        }
    }

    fn check_claimant(&self, claimant: &Claimant, now: Timestamp) -> Result<()> {
        match &claimant.consumer {
            Some(fence) => self.check_consumer(fence, now),
            None => Ok(()),
        }
    }

    pub(crate) fn create_task(
        &mut self,
        spec: TaskSpec,
        created_by: &Claimant,
        now: Timestamp,
    ) -> Result<Creation> {
        self.check_claimant(created_by, now)?;
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
            created_by: created_by.clone(),
            created_at: now,
            state: TaskState::Open,
            attempts: Vec::new(),
            effects: Vec::new(),
            evidence: EvidenceLog::default(),
            ownership: Vec::new(),
            consumed: BTreeSet::new(),
            cancel: None,
            acknowledgement: None,
        };
        self.tasks.insert(spec.id, record);
        Ok(Creation::Created)
    }

    pub(crate) fn reserve_task<R>(
        &mut self,
        spec: TaskSpec,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
        guard: impl FnOnce(&[&TaskRecord]) -> Result<Option<R>>,
    ) -> Result<Reservation<R>> {
        let id = spec.id.clone();
        if self.tasks.contains_key(&id) {
            // Validates that the existing task is identical.
            self.create_task(spec, claimant, now)?;
            // A settled task or one another claimant holds live is left
            // alone. Resuming an unfinished one takes the slot again, so the
            // guard runs against the other tasks in this transaction.
            match &self.task(&id)?.state {
                TaskState::Settled { .. } => return Ok(Reservation::Existing),
                TaskState::Claimed { lease } if lease.is_live(now) => {
                    return Ok(Reservation::Existing);
                }
                TaskState::Open | TaskState::Claimed { .. } => {}
            }
            let tasks: Vec<&TaskRecord> = self.tasks().collect();
            if let Some(blocked) = guard(&tasks)? {
                return Ok(Reservation::Blocked(blocked));
            }
            return self
                .take_over(&id, claimant, ttl, now)
                .map(Reservation::Resumed);
        }
        let tasks: Vec<&TaskRecord> = self.tasks().collect();
        if let Some(blocked) = guard(&tasks)? {
            return Ok(Reservation::Blocked(blocked));
        }
        self.create_task(spec, claimant, now)?;
        self.claim(&id, claimant, ttl, now)
            .map(Reservation::Reserved)
    }

    pub(crate) fn claim(
        &mut self,
        id: &TaskId,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.check_claimant(claimant, now)?;
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
        let lease = self.new_lease(claimant, ttl, now);
        let task = self.task_mut(id)?;
        let event = match task.ownership.last() {
            Some(OwnershipEvent::Relinquished {
                fence: previous, ..
            }) => OwnershipEvent::Adopted {
                previous: *previous,
                holder: claimant.holder.clone(),
                trigger: claimant.trigger.clone(),
                fence: lease.fence,
                at: now,
            },
            Some(
                OwnershipEvent::Claimed { .. }
                | OwnershipEvent::Adopted { .. }
                | OwnershipEvent::TakenOver { .. }
                | OwnershipEvent::Released { .. },
            )
            | None => OwnershipEvent::Claimed {
                holder: claimant.holder.clone(),
                trigger: claimant.trigger.clone(),
                fence: lease.fence,
                at: now,
            },
        };
        task.push_ownership(event)?;
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
        // Renewing extends authority: a claim made under a workflow consumer
        // renews only while that consumer lease is current and live.
        if let Some(TaskState::Claimed { lease }) = self.tasks.get(id).map(|task| &task.state)
            && lease.fence == fence
            && let Some(consumer) = &lease.consumer
        {
            self.check_consumer(consumer, now)?;
        }
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
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.check_claimant(claimant, now)?;
        let previous = match &self.task(id)?.state {
            TaskState::Open => return self.claim(id, claimant, ttl, now),
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
        let lease = self.new_lease(claimant, ttl, now);
        let task = self.task_mut(id)?;
        task.push_ownership(OwnershipEvent::TakenOver {
            previous,
            holder: claimant.holder.clone(),
            trigger: claimant.trigger.clone(),
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
        let unresolved = task.blocking_work();
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

    pub(crate) fn continue_attempt(
        &mut self,
        id: &TaskId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<Option<AttemptNumber>> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, true)?;
        let Some(attempt) = task.attempts.last_mut() else {
            return Ok(None);
        };
        match attempt.state {
            AttemptState::Running if attempt.fence == fence => Ok(Some(attempt.number)),
            AttemptState::Interrupted { .. } => {
                attempt.fence = fence;
                attempt.state = AttemptState::Running;
                Ok(Some(attempt.number))
            }
            AttemptState::Running
            | AttemptState::Finished { .. }
            | AttemptState::Cancelled { .. } => Ok(None),
        }
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
        let unresolved = task.blocking_settlement(outcome == AttemptOutcome::Succeeded);
        if unresolved > 0 {
            return fail(StateError::UnresolvedEffects { count: unresolved });
        }
        let stop = task.must_settle_unsuccessfully();
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
            AttemptOutcome::Failed(FailureClass::Retryable) if stop => Some(Settlement::Failed),
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
        if task.state == TaskState::Open && task.blocking_settlement(false) == 0 {
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
        let unresolved = task.blocking_settlement(false);
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
        backend: &BackendDescriptor,
        now: Timestamp,
    ) -> Result<EffectStart> {
        for house in [grants.house(), &backend.house] {
            if house != &self.house {
                return Err(ContractError::CrossHouse {
                    expected: self.house.clone(),
                    found: house.clone(),
                }
                .into());
            }
        }
        let house = self.house.clone();
        let nonce = self.nonce;
        // Work claimed under a workflow consumer stops when that consumer is
        // superseded, even while the task lease itself is live.
        if let Some(TaskState::Claimed { lease }) =
            self.tasks.get(&plan.task).map(|task| &task.state)
            && lease.fence == plan.fence
            && let Some(consumer) = &lease.consumer
        {
            self.check_consumer(consumer, now)?;
        }
        let scheduled = match plan.effect.schedule_requirements() {
            ScheduleRequirements::None => None,
            ScheduleRequirements::Declared(requires) => Some(requires),
            ScheduleRequirements::Unrecorded => {
                return fail(StateError::ScheduleRequirementsUnknown);
            }
            ScheduleRequirements::Installed { schedule, requires } => {
                let recorded = self
                    .schedules
                    .iter()
                    .find(|installed| &installed.schedule == schedule)
                    .map(|installed| &installed.requires)
                    .ok_or(StateError::ScheduleRequirementsUnknown)?;
                match requires {
                    None => return fail(StateError::ScheduleRequirementsUnknown),
                    Some(requires) if requires != recorded => {
                        return fail(StateError::ScheduleRequirementsMismatch);
                    }
                    Some(_) => Some(recorded),
                }
            }
        };
        backend.capabilities.require(
            self.task(&plan.task)?
                .spec
                .requires
                .for_executor(plan.effect.executor())
                .chain([plan.effect.required_capability()])
                .chain(scheduled.into_iter().flatten().copied()),
        )?;
        let task = self.task_mut(&plan.task)?;
        let trigger = task.owned_lease(plan.fence, now, true)?.trigger.clone();
        // Every launch uses the selection fixed when the task was created.
        if let Effect::Worker(Operation::LaunchWorker { agent, .. }) = &plan.effect
            && agent.as_ref() != task.spec.agent.as_ref().map(|resolved| &resolved.selection)
        {
            return fail(StateError::AgentSelectionMismatch);
        }
        // The executor must declare that it can launch exactly this
        // selection; one that cannot is refused before anything is reserved,
        // never left to run its default agent under the recorded selection.
        if let Effect::Worker(Operation::LaunchWorker {
            agent: Some(agent), ..
        }) = &plan.effect
        {
            backend.check_worker_selection(agent)?;
        }
        if let Some(target) = plan.effect.target()
            && (target.backend != backend.backend || !task.owns_resource(target))
        {
            return fail(StateError::ResourceNotOwned);
        }
        // After a cancellation request, only stopping a worker this task
        // launched may start; it may start while other effects are unresolved.
        let stopping = task.cancel.is_some();
        if stopping {
            match &plan.effect {
                Effect::Worker(Operation::CancelWorker { .. }) => {}
                Effect::Worker(
                    Operation::LaunchWorker { .. }
                    | Operation::MessageWorker { .. }
                    | Operation::ReplyToWorker { .. }
                    | Operation::ReleaseResource { .. },
                )
                | Effect::GitHub(_)
                | Effect::Roger(_)
                | Effect::Schedule(_) => return fail(StateError::CancelRequested),
            }
        }
        let attempt = match task.running_attempt_mut(plan.fence) {
            Some(attempt) => attempt.number,
            // A stop effect after takeover or adoption runs under the latest
            // attempt without starting ordinary work.
            None if stopping => match task.attempts.last() {
                Some(last) => last.number,
                None => return fail(StateError::NoRunningAttempt),
            },
            None => return fail(StateError::NoRunningAttempt),
        };
        if plan.decided_at != task.evidence.revision {
            return fail(StateError::StaleDecision {
                decided: plan.decided_at,
                current: task.evidence.revision,
            });
        }
        let permission = plan.effect.required_permission();
        let task_scope = task.spec.scope();
        let scope = plan.effect.scope(&task_scope);
        if !task_scope.covers(&scope) {
            return Err(ContractError::OutOfTaskScope {
                effect: scope,
                task: task_scope,
            }
            .into());
        }
        let (credential, authorization) = match (trigger, &plan.consent) {
            (Trigger::Scheduled | Trigger::Event(_), None) => (
                task.spec
                    .authority
                    .authorize(grants, permission, &scope, &backend.backend)?,
                Authorization::Standing,
            ),
            (Trigger::Scheduled | Trigger::Event(_), Some(_)) => {
                return Err(ContractError::ConsentNotAccepted.into());
            }
            (Trigger::Interactive, None) => {
                return Err(ContractError::ConsentRequired { permission }.into());
            }
            (Trigger::Interactive, Some(consent)) => {
                consent.check(&house, &plan.task, &plan.effect, plan.decided_at)?;
                let credential = grants.permitted(permission, &scope, &backend.backend)?;
                task.check_consent_unused(consent, &plan.name, attempt)?;
                (
                    credential,
                    Authorization::Consent {
                        id: consent.id.clone(),
                        given_by: consent.given_by.clone(),
                    },
                )
            }
        };
        let submitted = task.submitted_effects();
        let context = EffectContext {
            house: &house,
            task: &plan.task,
            task_scope: &task_scope,
            revision: task.evidence.revision,
            subject: task.evidence.subject.as_ref(),
            submitted: &submitted,
        };
        // Checks every submission must pass, including a same-key retry of
        // an existing intent; capacity is reserved only for new intents.
        let current = plan.effect.check(&context);
        let same_name = task.effects.iter().rposition(|effect| {
            effect.name == plan.name
                && effect.request.attempt() == attempt
                && !matches!(effect.state, EffectState::NotApplied { .. })
        });
        if let Some(index) = same_name {
            return task.repeat_effect(index, &plan, backend, &credential, current, now);
        }
        current?;
        let unresolved = task.blocking_work();
        if unresolved > 0 && !stopping {
            return fail(StateError::UnresolvedEffects { count: unresolved });
        }
        if task.effects.len() >= MAX_EFFECTS_PER_TASK {
            return fail(StateError::CapacityExceeded {
                limit: Limit::Effects,
            });
        }
        let seq = EffectSeq::new(u32::try_from(task.effects.len()).unwrap_or(u32::MAX));
        task.check_submission_budget(&plan.name, attempt, seq, now)?;
        plan.effect.admit(&context)?;
        let key = effect_key(&house, &plan.task, nonce, seq)?;
        let record = EffectRecord {
            seq,
            name: plan.name,
            decided_at: plan.decided_at,
            intended_at: now,
            request: EffectRequest::new(
                house,
                backend.backend.clone(),
                credential,
                plan.task,
                attempt,
                key,
                plan.effect,
            ),
            authorization,
            basis: plan.basis,
            submissions: 1,
            decisions: Vec::new(),
            state: EffectState::Intended,
        };
        task.effects.push(record.clone());
        Ok(EffectStart::Execute(record))
    }

    /// Record the outcome of submission number `submission` of an effect.
    /// A negative or uncertain result from an older submission is stale: a
    /// newer submission may still apply, so it cannot clear that uncertainty.
    pub(crate) fn record_submission_outcome(
        &mut self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        submission: u32,
        outcome: EffectOutcome,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        let task = self.task(id)?;
        let effect = task
            .effects
            .iter()
            .find(|effect| effect.seq == seq)
            .ok_or(Error::State(StateError::EffectNotFound(seq)))?;
        let stale = effect.submissions != submission
            && match outcome {
                EffectOutcome::Applied(_) => false,
                EffectOutcome::NotApplied(_)
                | EffectOutcome::Uncertain(_)
                | EffectOutcome::Unresolvable => true,
            };
        if stale {
            task.owned_lease(fence, now, false)?;
            return Ok(effect.clone());
        }
        self.record_effect_outcome(id, fence, seq, outcome, now)
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
        apply_outcome(effect, seq, outcome, now)?;
        let record = effect.clone();
        self.note_schedule(&record);
        Ok(record)
    }

    /// Record what a backend lookup proved about one write of a task that
    /// has settled. Needs no lease, since a settled task has none; only
    /// [`crate::state::reread_settled`] builds the lookup.
    pub(crate) fn record_settled_lookup(
        &mut self,
        id: &TaskId,
        seq: EffectSeq,
        lookup: SettledLookup,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        let task = self.task_mut(id)?;
        if task.settlement().is_none() {
            return fail(StateError::TaskNotSettled(id.clone()));
        }
        let effect = task
            .effects
            .iter_mut()
            .find(|effect| effect.seq == seq)
            .ok_or(Error::State(StateError::EffectNotFound(seq)))?;
        if lookup.key() != effect.request.key() {
            return fail(StateError::LookupScope(seq));
        }
        let next = match (&effect.state, lookup.into_found()) {
            (
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. },
                Found::Applied(receipt),
            ) => Some(EffectState::Applied { receipt, at: now }),
            (
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. },
                Found::Absent,
            ) => Some(EffectState::NotApplied {
                reason: NotAppliedReason::ConfirmedAbsent,
                at: now,
            }),
            (EffectState::Applied { receipt, .. }, Found::Applied(found)) if *receipt == found => {
                None
            }
            (EffectState::NotApplied { .. }, Found::Absent) => None,
            (EffectState::Applied { .. }, Found::Applied(_) | Found::Absent)
            | (EffectState::NotApplied { .. }, Found::Applied(_)) => {
                return fail(StateError::ConflictingOutcome(seq));
            }
        };
        if let Some(next) = next {
            effect.state = next;
        }
        let record = effect.clone();
        self.note_schedule(&record);
        Ok(record)
    }

    /// Keep an applied schedule install's workflow requirements with the
    /// schedule it created or reused, and forget them once it is removed.
    fn note_schedule(&mut self, effect: &EffectRecord) {
        let EffectState::Applied { receipt, .. } = &effect.state else {
            return;
        };
        match effect.request.effect() {
            Effect::Schedule(ScheduleEffect::InstallDisabled { schedule: spec }) => {
                // A spec without recorded requirements leaves the schedule
                // unrecorded, so it is never started.
                let Some(requires) = spec.requires() else {
                    return;
                };
                for schedule in receipt
                    .created()
                    .iter()
                    .chain(receipt.touched())
                    .filter(|resource| resource.kind == ResourceKind::Schedule)
                {
                    match self
                        .schedules
                        .iter_mut()
                        .find(|installed| &installed.schedule == schedule)
                    {
                        // Reinstalling never narrows what was recorded.
                        Some(installed) => installed.requires.extend(requires),
                        None => self.schedules.push(InstalledRequirements {
                            schedule: schedule.clone(),
                            requires: requires.clone(),
                        }),
                    }
                }
            }
            Effect::Schedule(ScheduleEffect::Remove { schedule }) => {
                self.schedules
                    .retain(|installed| &installed.schedule != schedule);
            }
            Effect::Schedule(ScheduleEffect::SetState { .. } | ScheduleEffect::Trial { .. })
            | Effect::Worker(_)
            | Effect::GitHub(_)
            | Effect::Roger(_) => {}
        }
    }

    /// Record that a person reviewed the writes of a settled task that did
    /// not succeed. Repeating the call keeps the first acknowledgement and
    /// reports it as already recorded.
    pub(crate) fn acknowledge_settled_writes(
        &mut self,
        id: &TaskId,
        claimant: &Claimant,
        reason: &Text,
        now: Timestamp,
    ) -> Result<(WriteAcknowledgement, bool)> {
        match claimant.trigger {
            Trigger::Interactive => {}
            Trigger::Scheduled | Trigger::Event(_) => {
                return fail(StateError::AcknowledgementNeedsPerson);
            }
        }
        let task = self.task_mut(id)?;
        match task.settlement() {
            None => return fail(StateError::TaskNotSettled(id.clone())),
            Some(Settlement::Succeeded) => {
                return fail(StateError::TaskSettled {
                    task: id.clone(),
                    settlement: Settlement::Succeeded,
                });
            }
            Some(Settlement::Failed | Settlement::Cancelled | Settlement::Exhausted) => {}
        }
        let wrote = task
            .effects
            .iter()
            .any(|effect| !matches!(effect.state, EffectState::NotApplied { .. }));
        if !wrote {
            return fail(StateError::NothingToAcknowledge(id.clone()));
        }
        if reason.as_str().len() > MAX_ACKNOWLEDGEMENT_REASON_BYTES {
            return fail(StateError::CapacityExceeded {
                limit: Limit::AcknowledgementReason,
            });
        }
        if let Some(recorded) = &task.acknowledgement {
            return Ok((recorded.clone(), true));
        }
        let mut unresolved: Vec<EffectName> = Vec::new();
        for effect in task
            .effects
            .iter()
            .filter(|effect| !effect.state.is_resolved())
        {
            if !unresolved.contains(&effect.name) {
                unresolved.push(effect.name.clone());
            }
        }
        let acknowledgement = WriteAcknowledgement {
            by: claimant.holder.clone(),
            at: now,
            reason: reason.clone(),
            unresolved,
        };
        task.acknowledgement = Some(acknowledgement.clone());
        Ok((acknowledgement, false))
    }

    pub(crate) fn accept_risk(
        &mut self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        decision: RiskDecision,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        let task = self.task_mut(id)?;
        task.owned_lease(fence, now, false)?;
        let revision = task.evidence.revision;
        let effect = task
            .effects
            .iter_mut()
            .find(|effect| effect.seq == seq)
            .ok_or(Error::State(StateError::EffectNotFound(seq)))?;
        if &decision.effect != effect.request.key() {
            return fail(StateError::DecisionScope(seq));
        }
        if decision.revision != revision {
            return fail(StateError::StaleDecision {
                decided: decision.revision,
                current: revision,
            });
        }
        match &effect.state {
            EffectState::Waived {
                decision: recorded, ..
            } if *recorded == decision => {}
            // Handed over, or waived at an older revision (expired).
            EffectState::Unresolvable { .. } | EffectState::Waived { .. }
                if effect.state.is_handed_over(revision) =>
            {
                if effect.decisions.len() >= MAX_DECISIONS_PER_EFFECT {
                    return fail(StateError::CapacityExceeded {
                        limit: Limit::Decisions,
                    });
                }
                effect.decisions.push(decision.clone());
                effect.state = EffectState::Waived { decision, at: now };
            }
            EffectState::Unresolvable { .. }
            | EffectState::Waived { .. }
            | EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::Applied { .. }
            | EffectState::NotApplied { .. } => {
                return fail(StateError::NotHandedOver(seq));
            }
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
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        let previous = match self.consumers.get(consumer).map(ConsumerRecord::state) {
            Some(ConsumerState::Held { lease }) if lease.is_live(now) => {
                return fail(StateError::ClaimHeld {
                    holder: lease.holder.clone(),
                    expires_at: lease.expires_at,
                });
            }
            Some(ConsumerState::Held { lease }) => {
                return fail(StateError::LeaseExpired {
                    expired_at: lease.expires_at,
                });
            }
            Some(ConsumerState::Relinquished { lease, .. }) => Some(lease.fence),
            Some(ConsumerState::Idle) => None,
            None if self.consumers.len() >= MAX_CONSUMERS => {
                return fail(StateError::CapacityExceeded {
                    limit: Limit::Consumers,
                });
            }
            None => None,
        };
        let lease = self.new_lease(claimant, ttl, now);
        let event = match previous {
            Some(previous) => ConsumerEvent::Adopted {
                previous,
                holder: claimant.holder.clone(),
                fence: lease.fence,
                at: now,
            },
            None => ConsumerEvent::Acquired {
                holder: claimant.holder.clone(),
                fence: lease.fence,
                at: now,
            },
        };
        self.consumers
            .entry(consumer.clone())
            .or_insert_with(|| ConsumerRecord::new(ConsumerState::Idle))
            .record(
                ConsumerState::Held {
                    lease: lease.clone(),
                },
                event,
            );
        Ok(lease)
    }

    fn held_consumer(
        &mut self,
        consumer: &ConsumerId,
        fence: Fence,
    ) -> Result<&mut ConsumerRecord> {
        let Some(record) = self.consumers.get_mut(consumer) else {
            return fail(StateError::ConsumerNotFound(consumer.clone()));
        };
        match &record.state {
            ConsumerState::Held { lease } if lease.fence == fence => Ok(record),
            ConsumerState::Held { .. }
            | ConsumerState::Relinquished { .. }
            | ConsumerState::Idle => fail(StateError::StaleFence { presented: fence }),
        }
    }

    pub(crate) fn renew_consumer(
        &mut self,
        consumer: &ConsumerId,
        fence: Fence,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        let record = self.held_consumer(consumer, fence)?;
        match &mut record.state {
            ConsumerState::Held { lease } if lease.is_live(now) => {
                lease.expires_at = now.saturating_add(ttl.duration());
                Ok(lease.clone())
            }
            ConsumerState::Held { lease } => fail(StateError::LeaseExpired {
                expired_at: lease.expires_at,
            }),
            ConsumerState::Relinquished { .. } | ConsumerState::Idle => {
                fail(StateError::StaleFence { presented: fence })
            }
        }
    }

    pub(crate) fn relinquish_consumer(
        &mut self,
        consumer: &ConsumerId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<()> {
        let record = self.held_consumer(consumer, fence)?;
        let lease = match &record.state {
            ConsumerState::Held { lease } => lease.clone(),
            ConsumerState::Relinquished { .. } | ConsumerState::Idle => {
                return fail(StateError::StaleFence { presented: fence });
            }
        };
        record.record(
            ConsumerState::Relinquished { lease, at: now },
            ConsumerEvent::Relinquished { fence, at: now },
        );
        Ok(())
    }

    pub(crate) fn release_consumer(
        &mut self,
        consumer: &ConsumerId,
        fence: Fence,
        now: Timestamp,
    ) -> Result<()> {
        let released = self.consumers.get(consumer).is_none_or(|record| {
            matches!(record.history.back(), Some(ConsumerEvent::Released { fence: last, .. }) if *last == fence)
        });
        if released {
            return Ok(());
        }
        self.held_consumer(consumer, fence)?.record(
            ConsumerState::Idle,
            ConsumerEvent::Released { fence, at: now },
        );
        Ok(())
    }

    pub(crate) fn take_over_consumer(
        &mut self,
        consumer: &ConsumerId,
        claimant: &Claimant,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        let previous = match self.consumers.get(consumer).map(ConsumerRecord::state) {
            Some(ConsumerState::Held { lease }) if lease.is_live(now) => {
                return fail(StateError::LeaseLive {
                    expires_at: lease.expires_at,
                });
            }
            Some(ConsumerState::Held { lease }) => lease.fence,
            Some(ConsumerState::Idle | ConsumerState::Relinquished { .. }) | None => {
                return self.acquire_consumer(consumer, claimant, ttl, now);
            }
        };
        let lease = self.new_lease(claimant, ttl, now);
        let event = ConsumerEvent::TakenOver {
            previous,
            holder: claimant.holder.clone(),
            fence: lease.fence,
            at: now,
        };
        if let Some(record) = self.consumers.get_mut(consumer) {
            record.record(
                ConsumerState::Held {
                    lease: lease.clone(),
                },
                event,
            );
        }
        Ok(lease)
    }

    pub(crate) fn record_marker(
        &mut self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<MarkerRecording> {
        self.check_claimant(recorded_by, now)?;
        self.markers
            .record(key, fact, recorded_by, now)
            .or_else(marker_refusal)
    }

    pub(crate) fn record_marker_unless<R>(
        &mut self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<Option<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.record_marker_guarded(key, fact, recorded_by, now, None, guard)
    }

    /// Like [`Self::record_marker_unless`], but a key that is already recorded
    /// is guarded too while `pending` names a task that does not exist yet.
    pub(crate) fn record_marker_unless_created<R>(
        &mut self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
        pending: &TaskId,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<Option<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.record_marker_guarded(key, fact, recorded_by, now, Some(pending), guard)
    }

    /// Record `first` and, when the guard asks for it, `second` in one
    /// transaction. The guard reads the workflow's markers before either is
    /// written. It blocks both, skips `second`, or records both; an error
    /// from either recording leaves neither written. Keys that are already
    /// recorded resolve as in [`Self::record_marker`].
    pub(crate) fn record_marker_pair_unless<R>(
        &mut self,
        first: (MarkerKey, MarkerFact),
        second: (MarkerKey, MarkerFact),
        recorded_by: &Claimant,
        now: Timestamp,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<PairPlan<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.check_claimant(recorded_by, now)?;
        let (first_key, first_fact) = first;
        let (second_key, second_fact) = second;
        let mut plan = PairPlan::Both;
        if self.markers.get(&first_key).is_none() || self.markers.get(&second_key).is_none() {
            let siblings: Vec<&WorkflowMarker> =
                self.markers.for_workflow(&first_key.workflow).collect();
            plan = guard(&siblings)?;
        }
        if let PairPlan::Block(blocked) = plan {
            return Ok(MarkerAttempt::Blocked(blocked));
        }
        let recorded = self
            .markers
            .record(first_key, first_fact, recorded_by, now)
            .or_else(marker_refusal)?;
        if matches!(plan, PairPlan::Both) {
            self.markers
                .record(second_key, second_fact, recorded_by, now)
                .or_else(marker_refusal)?;
        }
        match recorded {
            MarkerRecording::Recorded(marker) => Ok(MarkerAttempt::Recorded(marker)),
            MarkerRecording::AlreadyRecorded(marker) => Ok(MarkerAttempt::AlreadyRecorded(marker)),
            MarkerRecording::Superseded(_) => fail(StateError::MarkerConflict),
        }
    }

    fn record_marker_guarded<R>(
        &mut self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
        pending: Option<&TaskId>,
        guard: impl FnOnce(&[&WorkflowMarker]) -> Result<Option<R>>,
    ) -> Result<MarkerAttempt<R>> {
        self.check_claimant(recorded_by, now)?;
        // A recorded key is settled by `record`, never blocked, unless its
        // task is still to be created.
        let unfinished = pending.is_some_and(|task| !self.tasks.contains_key(task));
        if self.markers.get(&key).is_none() || unfinished {
            let siblings: Vec<&WorkflowMarker> = self.markers.for_workflow(&key.workflow).collect();
            if let Some(blocked) = guard(&siblings)? {
                return Ok(MarkerAttempt::Blocked(blocked));
            }
        }
        match self.markers.record(key, fact, recorded_by, now) {
            Ok(MarkerRecording::Recorded(marker)) => Ok(MarkerAttempt::Recorded(marker)),
            Ok(MarkerRecording::AlreadyRecorded(marker)) => {
                Ok(MarkerAttempt::AlreadyRecorded(marker))
            }
            // `record` never supersedes; the arm keeps the match exhaustive.
            Ok(MarkerRecording::Superseded(_)) => fail(StateError::MarkerConflict),
            Err(refusal) => marker_refusal(refusal),
        }
    }

    pub(crate) fn supersede_marker(
        &mut self,
        key: &MarkerKey,
        expected: &MarkerFact,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<MarkerRecording> {
        self.check_claimant(recorded_by, now)?;
        self.markers
            .supersede(key, expected, fact, recorded_by, now)
            .or_else(marker_refusal)
    }

    pub(crate) fn retire_markers(
        &mut self,
        markers: &[(MarkerKey, MarkerFact)],
    ) -> Result<Vec<MarkerKey>> {
        let mut retired = Vec::new();
        for (key, expected) in markers {
            match self.markers.retire(key, expected) {
                Ok(()) => retired.push(key.clone()),
                // Gone, or changed since the caller read it: kept as it is.
                Err(MarkerRefusal::Missing | MarkerRefusal::Conflict) => {}
                Err(refusal @ (MarkerRefusal::Full | MarkerRefusal::NotSupersedable)) => {
                    return marker_refusal(refusal);
                }
            }
        }
        Ok(retired)
    }

    pub(crate) fn retire_tasks(&mut self, ids: &[TaskId]) -> Result<Vec<TaskId>> {
        for id in ids {
            let Some(task) = self.tasks.get(id) else {
                continue;
            };
            let resolved = task.effects.iter().all(|effect| {
                matches!(
                    effect.state,
                    EffectState::Applied { .. } | EffectState::NotApplied { .. }
                )
            });
            if task.settlement().is_none() || !resolved {
                return fail(StateError::TaskNotRetirable(id.clone()));
            }
        }
        Ok(ids
            .iter()
            .filter(|id| self.tasks.remove(*id).is_some())
            .cloned()
            .collect())
    }

    pub(crate) fn capacity(&self) -> StoreCapacity {
        StoreCapacity::measure(
            self.tasks.values(),
            self.markers.iter(),
            self.consumers.len(),
        )
    }

    pub(crate) fn retention_subjects(&self) -> RetentionSubjects {
        RetentionSubjects::collect(
            self.tasks.values(),
            self.markers.iter(),
            self.retention_cursor.as_ref(),
        )
    }

    pub(crate) fn retention_plan(
        &self,
        policy: &RetentionPolicy,
        inventory: &Inventory,
        now: Timestamp,
    ) -> RetentionReport {
        let mut report = retention::plan(&self.tasks, self.markers.iter(), policy, inventory, now);
        report.intake = intake::plan_house_compaction(self.markers.iter(), &self.tasks)
            .into_iter()
            .map(|(summary, _)| summary)
            .collect();
        report
    }

    /// Compact intake and remove what [`Self::retention_plan`] selects, in
    /// this transaction. Both are planned from the same state: the generic
    /// pass selects no intake marker and keeps intake tasks, whose family it
    /// does not know.
    pub(crate) fn retain(
        &mut self,
        policy: &RetentionPolicy,
        inventory: &Inventory,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<RetentionReport> {
        let mut report = retention::plan(&self.tasks, self.markers.iter(), policy, inventory, now);
        let compactions = intake::plan_house_compaction(self.markers.iter(), &self.tasks);
        let mut compacted = false;
        for (summary, changes) in compactions {
            if !changes.is_empty() {
                compacted = true;
                self.compact_markers(&changes.retire, changes.writes, recorded_by, now)?;
            }
            report.intake.push(summary);
        }
        let keys: BTreeSet<MarkerKey> = report
            .markers
            .iter()
            .map(|retired| retired.key.clone())
            .collect();
        self.markers.remove_all(&keys);
        for retired in &report.tasks {
            self.tasks.remove(&retired.task);
        }
        if let Some(item) = inventory.last_lookup() {
            self.retention_cursor = Some(item.clone());
        }
        report.applied = compacted || !keys.is_empty() || !report.tasks.is_empty();
        Ok(report)
    }

    /// Remove every marker in `retire`, each of which must still hold its
    /// expected fact, then apply `writes`. Any refusal discards the whole
    /// transaction, so folded facts are never lost or counted twice.
    pub(crate) fn compact_markers(
        &mut self,
        retire: &[(MarkerKey, MarkerFact)],
        writes: Vec<MarkerWrite>,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<()> {
        self.check_claimant(recorded_by, now)?;
        for (key, expected) in retire {
            self.markers
                .retire(key, expected)
                .or_else(|refusal| match refusal {
                    MarkerRefusal::Missing => marker_refusal(MarkerRefusal::Conflict),
                    refusal @ (MarkerRefusal::Conflict
                    | MarkerRefusal::Full
                    | MarkerRefusal::NotSupersedable) => marker_refusal(refusal),
                })?;
        }
        for write in writes {
            match write {
                MarkerWrite::Record(key, fact) => {
                    self.markers
                        .record(key, fact, recorded_by, now)
                        .or_else(marker_refusal)?;
                }
                MarkerWrite::Supersede {
                    key,
                    expected,
                    fact,
                } => {
                    self.markers
                        .supersede(&key, &expected, fact, recorded_by, now)
                        .or_else(marker_refusal)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn marker(&self, key: &MarkerKey) -> Option<&WorkflowMarker> {
        self.markers.get(key)
    }

    pub(crate) fn markers<'a>(
        &'a self,
        workflow: &'a WorkflowId,
    ) -> impl Iterator<Item = &'a WorkflowMarker> {
        self.markers.for_workflow(workflow)
    }

    pub(crate) fn recovery_queue(&self, now: Timestamp) -> Vec<RecoveryItem> {
        let handed_over = self.tasks.values().flat_map(|task| {
            let open = task.settlement().is_none();
            task.effects
                .iter()
                .filter(move |effect| open && effect.state.is_handed_over(task.evidence.revision))
                .map(|effect| RecoveryItem::HandedOver {
                    task: task.spec.id.clone(),
                    seq: effect.seq,
                })
        });
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
                TaskState::Open => match task.unresolved_effects().count() {
                    0 if task.cancel.is_some() => {
                        Some(RecoveryItem::PendingCancellation { task: id })
                    }
                    0 => None,
                    count => Some(RecoveryItem::UnresolvedEffects { task: id, count }),
                },
            }
        });
        let consumers =
            self.consumers
                .iter()
                .filter_map(|(consumer, record)| match &record.state {
                    ConsumerState::Held { lease } if !lease.is_live(now) => {
                        Some(RecoveryItem::UncertainConsumer {
                            consumer: consumer.clone(),
                            holder: lease.holder.clone(),
                            expired_at: lease.expires_at,
                        })
                    }
                    ConsumerState::Relinquished { lease, at } => {
                        Some(RecoveryItem::AwaitingAdoption {
                            consumer: consumer.clone(),
                            holder: lease.holder.clone(),
                            since: *at,
                        })
                    }
                    ConsumerState::Held { .. } | ConsumerState::Idle => None,
                });
        tasks.chain(handed_over).chain(consumers).collect()
    }

    /// Check invariants that the type system cannot express.
    pub(crate) fn validate(&self) -> std::result::Result<(), Corruption> {
        if self.tasks.len() > MAX_TASKS
            || self.consumers.len() > MAX_CONSUMERS
            || self.schedules.len() > MAX_TASKS
        {
            return Err(Corruption::LimitExceeded);
        }
        self.markers.validate()?;
        for record in self.consumers.values() {
            record.validate(self.next_fence)?;
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
            || task.acknowledgement.as_ref().is_some_and(|recorded| {
                recorded.reason.as_str().len() > MAX_ACKNOWLEDGEMENT_REASON_BYTES
                    || recorded.unresolved.len() > MAX_EFFECTS_PER_TASK
            })
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
            if effect.submissions == 0
                || effect.submissions > RetryPolicy::MAX_ATTEMPTS
                || effect.decisions.len() > MAX_DECISIONS_PER_EFFECT
            {
                return Err(Corruption::LimitExceeded);
            }
            let request = &effect.request;
            if effect_key(&self.house, key, self.nonce, effect.seq)
                .ok()
                .as_ref()
                != Some(request.key())
            {
                return Err(Corruption::EffectKey);
            }
            if request.house() != &self.house
                || request.task() != key
                || usize::try_from(request.attempt().get())
                    .map_or(true, |number| number > task.attempts.len())
            {
                return Err(Corruption::EffectReference);
            }
        }
        let blocked = match task.settlement() {
            None => 0,
            Some(settlement) => task.blocking_settlement(settlement == Settlement::Succeeded),
        };
        if task.settlement().is_some()
            && (blocked > 0
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

fn marker_refusal<T>(refusal: MarkerRefusal) -> Result<T> {
    fail(match refusal {
        MarkerRefusal::Conflict => StateError::MarkerConflict,
        MarkerRefusal::Full => StateError::CapacityExceeded {
            limit: Limit::Markers,
        },
        MarkerRefusal::Missing => StateError::MarkerNotFound,
        MarkerRefusal::NotSupersedable => StateError::MarkerNotSupersedable,
    })
}

/// Replay the ownership history: each claim, adoption, or takeover gets a
/// larger fence than every earlier one; a relinquish, takeover, or release
/// names the current owner's fence; nothing follows a release; the replayed
/// owner matches the task state; and every attempt ran under an owned fence.
fn validate_ownership(task: &TaskRecord, next_fence: u64) -> std::result::Result<(), Corruption> {
    let mut owner: Option<(&HolderId, &Trigger, Fence)> = None;
    let mut owned = BTreeSet::new();
    let mut released = false;
    let mut relinquished = None;
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
        let current = owner.as_ref().map(|(_, _, fence)| *fence);
        let after_relinquish = relinquished.take();
        match event {
            OwnershipEvent::Claimed {
                holder,
                trigger,
                fence,
                ..
            } => {
                if owner.is_some() || after_relinquish.is_some() {
                    return Err(Corruption::Ownership);
                }
                issue(&mut owned, *fence)?;
                owner = Some((holder, trigger, *fence));
            }
            OwnershipEvent::Adopted {
                previous,
                holder,
                trigger,
                fence,
                ..
            } => {
                if owner.is_some() || after_relinquish != Some(*previous) {
                    return Err(Corruption::Ownership);
                }
                issue(&mut owned, *fence)?;
                owner = Some((holder, trigger, *fence));
            }
            OwnershipEvent::TakenOver {
                previous,
                holder,
                trigger,
                fence,
                ..
            } => {
                if current != Some(*previous) {
                    return Err(Corruption::Ownership);
                }
                issue(&mut owned, *fence)?;
                owner = Some((holder, trigger, *fence));
            }
            OwnershipEvent::Relinquished { fence, .. } | OwnershipEvent::Released { fence, .. } => {
                if current != Some(*fence) {
                    return Err(Corruption::Ownership);
                }
                owner = None;
                released = matches!(event, OwnershipEvent::Released { .. });
                if matches!(event, OwnershipEvent::Relinquished { .. }) {
                    relinquished = Some(*fence);
                }
            }
        }
    }
    let consistent = match &task.state {
        TaskState::Claimed { lease } => owner == Some((&lease.holder, &lease.trigger, lease.fence)),
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

/// The canonical idempotency key of effect `seq`: unique per house, task,
/// store incarnation (nonce), and effect number.
fn effect_key(
    house: &HouseId,
    task: &TaskId,
    nonce: u64,
    seq: EffectSeq,
) -> std::result::Result<IdempotencyKey, ContractError> {
    ExternalRef::new(&format!(
        "kitchen-{house}-{task}-{nonce:016x}-{}",
        seq.get()
    ))
    .map(IdempotencyKey::from_ref)
}

/// Move `effect` to the state `outcome` establishes, refusing an outcome that
/// contradicts a recorded one.
fn apply_outcome(
    effect: &mut EffectRecord,
    seq: EffectSeq,
    outcome: EffectOutcome,
    now: Timestamp,
) -> Result<()> {
    let next = match (&effect.state, outcome) {
        (EffectState::Intended | EffectState::Uncertain { .. }, outcome) => {
            Some(state_for(outcome, now))
        }
        (
            EffectState::Unresolvable { .. } | EffectState::Waived { .. },
            EffectOutcome::Applied(receipt),
        ) => Some(EffectState::Applied { receipt, at: now }),
        (
            EffectState::Unresolvable { .. } | EffectState::Waived { .. },
            EffectOutcome::NotApplied(reason),
        ) => Some(EffectState::NotApplied { reason, at: now }),
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
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. },
            EffectOutcome::Applied(_)
            | EffectOutcome::NotApplied(_)
            | EffectOutcome::Uncertain(_)
            | EffectOutcome::Unresolvable,
        ) => None,
    };
    if let Some(next) = next {
        effect.state = next;
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

impl crate::state::snapshot::Snapshot for StoreState {
    const SCHEMA: u64 = SCHEMA_VERSION;
    type Error = crate::Error;

    fn empty(house: HouseId, nonce: u64) -> Self {
        Self::new(house, nonce)
    }

    fn nonce(&self) -> u64 {
        StoreState::nonce(self)
    }

    fn validate(&self, _house: &HouseId) -> Result<()> {
        StoreState::validate(self).map_err(StateError::CorruptState)?;
        Ok(())
    }
}
