//! The executor-neutral execution boundary.
//!
//! Kitchen persists an [`EffectRequest`] before handing it to an
//! [`EffectExecutor`], then records what the executor reports. A
//! [`WorkerBackend`] is an executor that also runs and observes workers.
//! Executors keep their native state; they return receipts and observations
//! rather than becoming a second source of truth for task ownership.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, CredentialId, HouseId, TaskId,
    contracts::{
        AttemptNumber, BackendDescriptor, Capability, ContractError, Effect, ExternalRef,
        Permission, ResourceRef, Role, Text, ValueKind,
    },
};

/// Where a worker runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "resource", rename_all = "kebab-case")]
pub enum Workspace {
    /// A fresh isolated workspace created by the backend.
    Isolated,
    /// An existing workspace the task already owns, such as a branch under repair.
    Existing(ResourceRef),
}

/// An external effect Kitchen may ask a backend to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Operation {
    /// Start a worker. A receipt means the request was accepted, not that the
    /// agent is ready; readiness needs a separate observation.
    LaunchWorker {
        /// The worker's role.
        role: Role,
        /// Where it runs.
        workspace: Workspace,
        /// The standalone brief given to the worker.
        brief: Text,
    },
    /// Deliver a message to a worker.
    MessageWorker {
        /// Target worker.
        worker: ResourceRef,
        /// Message body.
        body: Text,
    },
    /// Answer a question a worker asked.
    ReplyToWorker {
        /// Target worker.
        worker: ResourceRef,
        /// The backend's reference for the question being answered.
        question: ExternalRef,
        /// Reply body.
        body: Text,
    },
    /// Stop a worker. Cancellation does not prove rollback of its effects.
    CancelWorker {
        /// Target worker.
        worker: ResourceRef,
    },
    /// Release a resource the task owns.
    ReleaseResource {
        /// Target resource.
        resource: ResourceRef,
    },
}

impl Operation {
    /// The backend capability this operation needs.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::LaunchWorker { .. } => Capability::WorkerLaunchIsolated,
            Self::MessageWorker { .. } | Self::ReplyToWorker { .. } => Capability::WorkerMessaging,
            Self::CancelWorker { .. } => Capability::WorkerCancel,
            Self::ReleaseResource { .. } => Capability::ResourceRelease,
        }
    }

    /// The task permission this operation needs.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::LaunchWorker { .. } => Permission::LaunchWorker,
            Self::MessageWorker { .. } | Self::ReplyToWorker { .. } => Permission::MessageWorker,
            Self::CancelWorker { .. } => Permission::CancelWorker,
            Self::ReleaseResource { .. } => Permission::ReleaseResource,
        }
    }
}

/// The key a backend uses to deduplicate one logical effect across retries
/// and restarts. Kitchen derives it once and persists it with the intent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(ExternalRef);

impl IdempotencyKey {
    /// Wrap a key read back from a backend or chosen by a test fixture.
    #[must_use]
    pub const fn from_ref(value: ExternalRef) -> Self {
        Self(value)
    }

    /// Borrow the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A persisted request for one external effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectRequest {
    house: HouseId,
    backend: BackendId,
    credential: CredentialId,
    task: TaskId,
    attempt: AttemptNumber,
    key: IdempotencyKey,
    effect: Effect,
}

impl EffectRequest {
    /// Assemble a request. Workflows obtain requests from the state store,
    /// which persists them first; adapters and conformance fixtures may build
    /// them directly.
    #[must_use]
    pub const fn new(
        house: HouseId,
        backend: BackendId,
        credential: CredentialId,
        task: TaskId,
        attempt: AttemptNumber,
        key: IdempotencyKey,
        effect: Effect,
    ) -> Self {
        Self {
            house,
            backend,
            credential,
            task,
            attempt,
            key,
            effect,
        }
    }

    /// The house whose credentials and destinations apply.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// The backend namespace the intent was persisted for. The key is only
    /// meaningful there, so only that backend may execute or look it up.
    #[must_use]
    pub const fn backend(&self) -> &BackendId {
        &self.backend
    }

    /// The house-owned credential the authorizing grant names. The backend
    /// uses exactly this credential; there is no fallback.
    #[must_use]
    pub const fn credential(&self) -> &CredentialId {
        &self.credential
    }

    /// The owning task.
    #[must_use]
    pub const fn task(&self) -> &TaskId {
        &self.task
    }

    /// The owning attempt.
    #[must_use]
    pub const fn attempt(&self) -> AttemptNumber {
        self.attempt
    }

    /// The deduplication key.
    #[must_use]
    pub const fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    /// The requested effect.
    #[must_use]
    pub const fn effect(&self) -> &Effect {
        &self.effect
    }
}

/// Maximum resources one receipt may report.
pub const MAX_RECEIPT_RESOURCES: usize = 16;

/// A backend's positive confirmation that an effect was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawReceipt", into = "RawReceipt")]
pub struct Receipt {
    reference: ExternalRef,
    resources: Vec<ResourceRef>,
}

impl Receipt {
    /// Build a receipt for the backend's request reference and created resources.
    ///
    /// # Errors
    /// Returns [`ContractError::InvalidValue`] beyond [`MAX_RECEIPT_RESOURCES`].
    pub fn new(reference: ExternalRef, resources: Vec<ResourceRef>) -> Result<Self, ContractError> {
        if resources.len() > MAX_RECEIPT_RESOURCES {
            return Err(ContractError::InvalidValue {
                kind: ValueKind::Receipt,
            });
        }
        Ok(Self {
            reference,
            resources,
        })
    }

    /// The backend's reference for the applied request.
    #[must_use]
    pub const fn reference(&self) -> &ExternalRef {
        &self.reference
    }

    /// Resources the effect created or touched.
    #[must_use]
    pub fn resources(&self) -> &[ResourceRef] {
        &self.resources
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawReceipt {
    reference: ExternalRef,
    resources: Vec<ResourceRef>,
}

impl TryFrom<RawReceipt> for Receipt {
    type Error = ContractError;

    fn try_from(raw: RawReceipt) -> Result<Self, Self::Error> {
        Self::new(raw.reference, raw.resources)
    }
}

impl From<Receipt> for RawReceipt {
    fn from(receipt: Receipt) -> Self {
        Self {
            reference: receipt.reference,
            resources: receipt.resources,
        }
    }
}

/// Why a backend knows an effect was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "capability", rename_all = "kebab-case")]
pub enum NotAppliedReason {
    /// The backend does not support the operation's capability.
    Unsupported(Capability),
    /// The request named a house this backend instance does not serve.
    CrossHouse,
    /// The request was persisted for another backend namespace.
    ForeignBackend,
    /// The provider refused the request before acting.
    Rejected,
    /// A lookup established that the provider never applied the key.
    ConfirmedAbsent,
}

/// Why the outcome of an effect is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UncertainReason {
    /// The call exceeded its deadline.
    Timeout,
    /// The connection or subprocess failed mid-request.
    Transport,
    /// The provider may have acted but its response was lost or malformed.
    ResponseLost,
    /// A reconciliation lookup could not establish the outcome.
    LookupInconclusive,
    /// The backend offers no lookup, so the outcome cannot be established.
    LookupUnsupported,
}

impl UncertainReason {
    /// Whether a reconciliation lookup, rather than a submission, produced
    /// this state. Only then may an idempotent backend receive the key again.
    #[must_use]
    pub const fn is_from_lookup(self) -> bool {
        match self {
            Self::LookupInconclusive | Self::LookupUnsupported => true,
            Self::Timeout | Self::Transport | Self::ResponseLost => false,
        }
    }
}

/// An `execute` failure. Only [`EffectFailure::NotApplied`] proves nothing happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum EffectFailure {
    /// The effect definitely did not happen.
    #[error("effect not applied: {0:?}")]
    NotApplied(NotAppliedReason),
    /// The effect may or may not have happened; reconcile before retrying.
    #[error("effect outcome unknown: {0:?}")]
    Uncertain(UncertainReason),
}

/// What a backend knows about an idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The effect was applied.
    Applied(Receipt),
    /// The provider can prove it never applied this key and that no earlier
    /// invocation can still apply it.
    Absent,
    /// The provider cannot establish the outcome.
    Unknown,
}

/// A worker's terminal result as reported by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerOutcome {
    /// The worker reported success.
    Succeeded,
    /// The worker reported failure.
    Failed,
    /// The worker was cancelled.
    Cancelled,
}

/// An observation of a worker. Silence or process age never implies settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerState {
    /// Accepted but not yet shown to be running.
    Starting,
    /// Positive readiness evidence was observed.
    Ready,
    /// Waiting for a reply to a question.
    AwaitingReply,
    /// The worker settled with an outcome.
    Settled(WorkerOutcome),
    /// The backend has no record of the worker.
    Missing,
    /// The backend cannot tell.
    Unknown,
}

/// A read-only backend call failed; no state may be inferred from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum BackendUnavailable {
    /// The call exceeded its deadline.
    #[error("backend call timed out")]
    Timeout,
    /// The connection or subprocess failed.
    #[error("backend transport failed")]
    Transport,
    /// The backend does not declare the capability the call needs.
    #[error("backend does not support {0}")]
    Unsupported(Capability),
    /// The answer would exceed its bound, such as
    /// [`MAX_INVENTORY_RESOURCES`].
    #[error("backend answer exceeds its bound")]
    LimitExceeded,
}

/// Maximum resources one [`WorkerBackend::inventory`] call returns.
pub const MAX_INVENTORY_RESOURCES: usize = 1024;

/// Whether an inventoried resource is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Liveness {
    /// The backend has positive evidence the resource is in use.
    Live,
    /// The backend has positive evidence its process or session ended.
    Exited,
    /// The backend cannot tell; never treat as exited.
    Unverifiable,
}

/// One resource as the backend sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceObservation {
    /// The resource.
    pub resource: ResourceRef,
    /// The backend's record of what owns it, such as the idempotency key of
    /// the request that created it; `None` when the backend records none.
    pub owner: Option<ExternalRef>,
    /// Whether it is in use.
    pub liveness: Liveness,
}

/// Performs persisted effects for exactly one house in one namespace.
///
/// Contract, checked by [`crate::contracts::conformance::run`]:
///
/// - [`BackendDescriptor::backend`] names one provider namespace: the
///   instance and account whose idempotency keys and lookups it uses.
/// - Every call is bounded by a deadline; an expired call reports
///   [`EffectFailure::Uncertain`] or [`BackendUnavailable::Timeout`], never success.
/// - `execute` refuses a request for another house with
///   [`NotAppliedReason::CrossHouse`], a request persisted for another
///   backend namespace with [`NotAppliedReason::ForeignBackend`], and an
///   effect whose capability is not fully supported with
///   [`NotAppliedReason::Unsupported`], in each case without acting.
/// - `execute` returns [`EffectFailure::NotApplied`] only when the effect
///   definitely did not happen.
/// - With [`Capability::EffectIdempotentRequests`], resubmitting a key returns
///   the original receipt without repeating the effect.
/// - With [`Capability::EffectLookup`], `lookup` reports an applied request's
///   receipt and returns [`Lookup::Absent`] only with proof that the key was
///   not applied and cannot be applied later, including by an earlier
///   invocation that is still in flight; otherwise [`Lookup::Unknown`].
///   Without it, `lookup` returns [`BackendUnavailable::Unsupported`].
pub trait EffectExecutor {
    /// Namespace, house, and declared capabilities.
    fn descriptor(&self) -> &BackendDescriptor;

    /// Perform one effect.
    ///
    /// # Errors
    /// Returns an [`EffectFailure`] distinguishing "definitely not applied"
    /// from "outcome unknown".
    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure>;

    /// Look up what happened to a persisted request without performing it.
    /// The executor receives the whole request, so it can inspect the
    /// original target (such as a repository, operation, and marker, or a
    /// decision binding) when it has no global key index.
    ///
    /// # Errors
    /// Returns [`BackendUnavailable`] when the executor cannot be queried.
    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable>;
}

/// An executor that runs and observes workers.
///
/// Contract, checked by [`crate::contracts::conformance::run_worker`]:
/// `observe_worker` reports readiness only on positive evidence, and
/// `inventory` returns at most [`MAX_INVENTORY_RESOURCES`] observations,
/// reporting [`Liveness::Unverifiable`] rather than guessing.
pub trait WorkerBackend: EffectExecutor {
    /// Observe a worker's state without changing it.
    ///
    /// # Errors
    /// Returns [`BackendUnavailable`] when the backend cannot be queried.
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable>;

    /// List the resources this backend manages for the house, read-only.
    ///
    /// # Errors
    /// The default reports [`BackendUnavailable::Unsupported`] for
    /// [`Capability::ResourceInventory`]; implementations return
    /// [`BackendUnavailable::LimitExceeded`] rather than truncating.
    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        Err(BackendUnavailable::Unsupported(
            Capability::ResourceInventory,
        ))
    }
}
