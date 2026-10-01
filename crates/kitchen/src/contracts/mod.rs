//! Orchestrator-neutral contracts: validated values, roles, authority,
//! capabilities, tasks, evidence, resources, and the execution boundary.
//!
//! These types carry no orchestrator-specific identifiers. Backend-native
//! handles appear only as opaque [`ExternalRef`] values inside adapter results.

/// Define a closed set of names: the enum, its serde names, [`ALL`], `as_str`,
/// `Display`, and `FromStr`, all from one list so no variant can be left out.
///
/// [`ALL`]: Capability::ALL
macro_rules! closed_names {
    (
        $(#[$meta:meta])*
        pub enum $name:ident ($kind:expr) {
            $( $(#[$vmeta:meta])* $variant:ident = $text:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            serde::Serialize, serde::Deserialize,
        )]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant, )+
        }

        impl $name {
            /// Every name, in declaration order.
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] = [$(Self::$variant),+];

            /// The stable serialized name.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text, )+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::contracts::ContractError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .into_iter()
                    .find(|item| item.as_str() == value)
                    .ok_or($crate::contracts::ContractError::InvalidValue { kind: $kind })
            }
        }
    };
}

mod authority;
mod backend;
mod capability;
pub mod conformance;
mod effects;
mod error;
mod evidence;
pub mod fake;
mod mailbox;
mod resource;
mod role;
mod schedules;
mod task;
mod trigger;
mod value;
mod verification;

pub use authority::{Grant, GrantScope, HouseGrants, Permission, TaskAuthority};
pub use backend::{
    BackendUnavailable, EffectExecutor, EffectFailure, EffectRequest, IdempotencyKey, Liveness,
    Lookup, MAX_INVENTORY_RESOURCES, MAX_RECEIPT_RESOURCES, NotAppliedReason, Operation,
    PinnedCheckout, Receipt, ResourceObservation, Retarget, UncertainReason, WorkerBackend,
    WorkerOutcome, WorkerState, Workspace, WorktreeStatus,
};
pub use capability::{BackendDescriptor, Capability, CapabilitySet, Support};
pub use effects::{
    AskKind, AskRisk, CloseReason, DecisionBinding, DecisionOwner, Effect, EffectContext,
    EffectKind, ExecutorKind, GitHubAction, GitHubEffect, GitHubMutation, IssueNumber,
    LabelDefinition, MAX_ASKS_PER_TASK, MergeMethod, PostingBudget, ReviewVerdict, RogerAsk,
    RogerEffect, ScheduleEffect, ScheduleRequirements, SubmittedEffects,
};
pub use error::ContractError;
pub use evidence::{
    CheckoutFact, CheckoutReport, Evidence, EvidenceKind, EvidenceRevision, EvidenceSubject,
    EvidenceVerdict,
};
pub use mailbox::{
    CoordinatorMailbox, Delivery, MAX_MAILBOX_WAIT, MailMessage, MailboxError, MessageKind,
};
pub use resource::{ResourceKind, ResourceRef};
pub use role::Role;
pub use schedules::ScheduleBackend;
pub use task::{
    AttemptNumber, AttemptOutcome, AttemptStart, CapabilityRequirements, Disposition, EffectSeq,
    FailureClass, Fence, Provenance, RetryPolicy, Settlement, TaskSpec,
};
pub use trigger::{Authorization, Claimant, Consent, ConsumerFence, EventOrigin, Trigger};
pub use value::{
    BranchName, Clock, CommitId, ExternalRef, LeaseTtl, MAX_BRANCH_NAME_BYTES,
    MAX_EXTERNAL_REF_BYTES, MAX_TEXT_BYTES, Repository, SystemClock, Text, Timestamp, ValueKind,
};
pub use verification::{
    DeviceClass, MAX_DEVICE_CLASS_BYTES, MAX_POLICY_REPOSITORIES, MAX_POLICY_WORK_TYPES,
    MAX_TARGETS_PER_WORK_TYPE, MAX_VERIFICATION_ENVIRONMENTS, OperatingSystem, RecordedEvidence,
    TargetStatus, VerificationAccess, VerificationEnvironments, VerificationError,
    VerificationExecutor, VerificationPolicy, VerificationReport, VerificationResult,
    VerificationTarget, authorize_access,
};
