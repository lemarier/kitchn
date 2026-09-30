//! Durable, house-scoped task ownership.
//!
//! [`HouseStore`] records tasks, fenced claims, attempts, effect intents and
//! outcomes, evidence revisions, consumed messages, and single-consumer
//! leases in runtime storage the caller selects. [`run_effect`] and
//! [`reconcile`] connect the store to an [`crate::contracts::EffectExecutor`];
//! [`run_verification`] connects it to a
//! [`crate::contracts::VerificationExecutor`].
//!
//! Ownership rules:
//!
//! - A claim is a lease with a fence. Every change presents the fence; a
//!   stale fence is rejected.
//! - Scheduled and interactive work share the same claims. A claim records
//!   its [`crate::contracts::Trigger`]: effects under a scheduled claim use
//!   the task's standing authority; effects under an interactive claim need
//!   a [`crate::contracts::Consent`] for exactly that effect, within house
//!   policy limits.
//! - A claimant may act under a workflow consumer lease
//!   ([`crate::contracts::Claimant::under`]). Creating a task, claiming it,
//!   and every effect of that claim then require the consumer lease to be
//!   current and live, so a superseded or expired consumer cannot act.
//! - Starting work (attempts, effects, message consumption) needs a live
//!   lease. Recording facts (outcomes, evidence, finishing) needs only the
//!   current fence.
//! - An expired lease means ownership is uncertain, not released. It appears
//!   in the recovery queue and changes hands only through an explicit
//!   takeover, which interrupts the old attempt and issues a larger fence.
//! - A deliberate transfer is a recorded relinquish followed by an adoption,
//!   distinct from a takeover after expiry. Neither proves that workers the
//!   previous owner started have stopped; their effects stay recorded for the
//!   new owner to reconcile.
//! - Intent is persisted before every effect. While any effect is unresolved,
//!   no new effect or attempt starts; the owner reconciles first.
//! - Workflow markers record facts (a verdict for one head, a question
//!   already asked) keyed by workflow, work item, and exact evidence subject.
//!   They grant nothing and are separate from effects.
//! - An effect whose outcome cannot be established is handed over, not
//!   resolved: the task keeps its reservation until positive evidence or a
//!   scoped [`RiskDecision`] allows one specific action.
//! - One retention policy ([`RetentionPolicy`]) removes markers and settled
//!   tasks no workflow still needs, only on positive outside evidence.
//! - Workers without a backend mailbox post questions, reports, and
//!   escalations into the house mailbox ([`HouseMailbox`]), which a single
//!   fenced coordinator reads; retention removes acknowledged messages once
//!   their attempt ended.
//! - Each attempt carries its backend-reported usage or an explicit
//!   [`AttemptUsage::NotReported`]; human time is derived from recorded
//!   replies and interactive claims, never reported ([`AttemptUsageEntry`]).

mod consumer;
mod effects;
mod error;
mod mailbox;
mod marker;
mod model;
mod retention;
pub(crate) mod snapshot;
mod store;
mod usage;
mod verification;

pub use consumer::{ConsumerEvent, ConsumerRecord, ConsumerState, MAX_CONSUMER_HISTORY};
pub use effects::{ReconcileReport, reconcile, reread_settled, run_effect};
pub use error::{Corruption, Limit, StateError, StorageOperation};
pub use mailbox::{
    AnswerState, Answered, Answerer, HouseMailbox, MAX_MAIL_BATCH, MAX_MAIL_BODY_BYTES,
    MAX_MAIL_PER_TASK, MAX_MAIL_SUBJECT_BYTES, MAX_MAILBOX_MESSAGES, MAX_UNACKNOWLEDGED_PER_TASK,
    MailAnswer, MailError, MailSender, OpenQuestion, PostKind, ReportedOutcome, WorkerPost,
};
pub use marker::{
    IssueRevision, MAX_MARKER_HISTORY, MAX_MARKER_PAYLOAD_BYTES, MAX_MARKERS, MarkerAttempt,
    MarkerFact, MarkerKey, MarkerPayload, MarkerRecording, MarkerSchema, MarkerSubject,
    SupersededFact, WorkItem, WorkflowMarker,
};
pub(crate) use marker::{MarkerWrite, PairPlan};
pub(crate) use model::StoreState;
pub use model::{
    AttemptRecord, AttemptState, CancelRequest, CancelStatus, Consumption, Creation, EffectOutcome,
    EffectPlan, EffectRecord, EffectStart, EffectState, EvidenceLog, Lease,
    MAX_ACKNOWLEDGEMENT_REASON_BYTES, MAX_CONSUMED_MESSAGES, MAX_CONSUMERS,
    MAX_DECISIONS_PER_EFFECT, MAX_EFFECTS_PER_TASK, MAX_EVIDENCE_PER_REVISION,
    MAX_OWNERSHIP_HISTORY, MAX_TASKS, OwnershipEvent, RecoveryItem, Reservation, RiskAction,
    RiskDecision, TaskRecord, TaskState, WriteAcknowledgement,
};
pub use retention::{
    CAPACITY_WARNING_PERCENT, Inventory, MIN_TASK_WINDOW, MailRetirement, MarkerRetirement,
    MarkerRule, Presence, RetentionPolicy, RetentionReport, RetentionSubjects, RetiredMail,
    RetiredMarker, RetiredTask, StoreCapacity, TableUsage, TaskRetirement, marker_rule,
};
pub use snapshot::StoreOptions;
pub use store::HouseStore;
pub use usage::{
    AttemptUsage, AttemptUsageEntry, Cost, CostBasis, HumanReply, HumanTime,
    MAX_HUMAN_REPLIES_PER_ATTEMPT, TokenCounts, UsageError, UsageReport, UsdMicros,
};
pub use verification::{VerificationPlan, run_verification};
