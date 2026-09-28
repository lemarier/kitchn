//! Orchestrator-neutral contracts: validated values, roles, authority,
//! capabilities, tasks, evidence, resources, and the execution boundary.
//!
//! These types carry no orchestrator-specific identifiers. Backend-native
//! handles appear only as opaque [`ExternalRef`] values inside adapter results.

mod authority;
mod backend;
mod capability;
pub mod conformance;
mod error;
mod evidence;
pub mod fake;
mod resource;
mod role;
mod task;
mod trigger;
mod value;

pub use authority::{Grant, GrantScope, HouseGrants, Permission, TaskAuthority};
pub use backend::{
    BackendUnavailable, EffectFailure, EffectRequest, ExecutionBackend, IdempotencyKey, Lookup,
    MAX_RECEIPT_RESOURCES, NotAppliedReason, Operation, Receipt, UncertainReason, WorkerOutcome,
    WorkerState, Workspace,
};
pub use capability::{BackendDescriptor, Capability, CapabilitySet, Support};
pub use error::ContractError;
pub use evidence::{Evidence, EvidenceKind, EvidenceRevision, EvidenceVerdict};
pub use resource::{ResourceKind, ResourceRef};
pub use role::Role;
pub use task::{
    AttemptNumber, AttemptOutcome, AttemptStart, Disposition, EffectSeq, FailureClass, Fence,
    Provenance, RetryPolicy, Settlement, TaskSpec,
};
pub use trigger::{Authorization, Claimant, Consent, Trigger};
pub use value::{
    Clock, CommitId, ExternalRef, LeaseTtl, MAX_EXTERNAL_REF_BYTES, MAX_TEXT_BYTES, Repository,
    SystemClock, Text, Timestamp, ValueKind,
};
