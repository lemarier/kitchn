//! Durable, house-scoped task ownership.
//!
//! [`HouseStore`] records tasks, fenced claims, attempts, effect intents and
//! outcomes, evidence revisions, consumed messages, and single-consumer
//! leases in runtime storage the caller selects. [`run_effect`] and
//! [`reconcile`] connect the store to an [`crate::contracts::ExecutionBackend`].
//!
//! Ownership rules:
//!
//! - A claim is a lease with a fence. Every change presents the fence; a
//!   stale fence is rejected.
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
//! - An effect whose outcome cannot be established is handed over, not
//!   resolved: the task keeps its reservation until positive evidence or a
//!   scoped [`RiskDecision`] allows one specific action.

mod consumer;
mod effects;
mod error;
mod model;
mod store;

pub use consumer::{ConsumerEvent, ConsumerRecord, ConsumerState, MAX_CONSUMER_HISTORY};
pub use effects::{ReconcileReport, reconcile, run_effect};
pub use error::{Corruption, Limit, StateError, StorageOperation};
pub use model::{
    AttemptRecord, AttemptState, CancelRequest, CancelStatus, Consumption, Creation, EffectOutcome,
    EffectPlan, EffectRecord, EffectStart, EffectState, EvidenceLog, Lease, MAX_CONSUMED_MESSAGES,
    MAX_CONSUMERS, MAX_EFFECTS_PER_TASK, MAX_EVIDENCE_PER_REVISION, MAX_OWNERSHIP_HISTORY,
    MAX_TASKS, OwnershipEvent, RecoveryItem, RiskAction, RiskDecision, TaskRecord, TaskState,
};
pub use store::{HouseStore, StoreOptions};
