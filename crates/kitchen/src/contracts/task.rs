//! Task definitions, attempts, retry bounds, and settlement.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    num::NonZeroU32,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    TaskId,
    contracts::{
        Capability, CommitId, ContractError, ExecutorKind, GrantScope, Repository, ResourceRef,
        Role, TaskAuthority, ValueKind,
    },
};

/// A monotonically increasing ownership token. Every claim or takeover gets a
/// larger fence; state changes presenting an older fence are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fence(u64);

impl Fence {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw token, for adapters that forward it to a fenced backend.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Fence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "fence {}", self.0)
    }
}

/// A one-based attempt number within a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptNumber(NonZeroU32);

impl AttemptNumber {
    /// The first attempt.
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    /// Build from a one-based count; `None` for zero.
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// The one-based number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for AttemptNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "attempt {}", self.0)
    }
}

/// Bounds on repeated attempts: a maximum count and a maximum elapsed time
/// measured from the first attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawRetryPolicy", into = "RawRetryPolicy")]
pub struct RetryPolicy {
    max_attempts: u32,
    max_elapsed: Duration,
}

impl RetryPolicy {
    /// Largest accepted attempt count.
    pub const MAX_ATTEMPTS: u32 = 16;
    /// Longest accepted elapsed budget (30 days).
    pub const MAX_ELAPSED: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    /// Validate a retry budget.
    ///
    /// # Errors
    /// Returns [`ContractError::InvalidValue`] when `max_attempts` is outside
    /// `1..=MAX_ATTEMPTS` or `max_elapsed` is outside one second to `MAX_ELAPSED`.
    pub fn new(max_attempts: u32, max_elapsed: Duration) -> Result<Self, ContractError> {
        if !(1..=Self::MAX_ATTEMPTS).contains(&max_attempts)
            || max_elapsed < Duration::from_secs(1)
            || max_elapsed > Self::MAX_ELAPSED
        {
            return Err(ContractError::InvalidValue {
                kind: ValueKind::RetryPolicy,
            });
        }
        Ok(Self {
            max_attempts,
            max_elapsed,
        })
    }

    /// Maximum attempts, counting interrupted and cancelled ones.
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }

    /// Maximum time from the first attempt's start to a new attempt's start.
    #[must_use]
    pub const fn max_elapsed(self) -> Duration {
        self.max_elapsed
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawRetryPolicy {
    max_attempts: u32,
    max_elapsed_ms: u64,
}

impl TryFrom<RawRetryPolicy> for RetryPolicy {
    type Error = ContractError;

    fn try_from(raw: RawRetryPolicy) -> Result<Self, Self::Error> {
        Self::new(raw.max_attempts, Duration::from_millis(raw.max_elapsed_ms))
    }
}

impl From<RetryPolicy> for RawRetryPolicy {
    fn from(policy: RetryPolicy) -> Self {
        Self {
            max_attempts: policy.max_attempts,
            max_elapsed_ms: u64::try_from(policy.max_elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// Instruction revisions pinned when the task was created. Active tasks keep
/// them even after the house or Kitchen updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    /// Kitchen revision.
    pub kitchen: CommitId,
    /// House guidance revision.
    pub house_guidance: CommitId,
    /// Repository instruction revision, when the task targets a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_instructions: Option<CommitId>,
}

/// Everything fixed when a task is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSpec {
    /// Portable task identity.
    pub id: TaskId,
    /// The responsible role.
    pub role: Role,
    /// Target repository; `None` for house-level work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    /// Delegated authority; its house must match the store's.
    pub authority: TaskAuthority,
    /// Attempt bounds.
    pub retry: RetryPolicy,
    /// Pinned instruction revisions.
    pub provenance: Provenance,
    /// Existing resources given to the task when it was created, such as a
    /// worktree under repair. Targeted operations act only on these or on
    /// resources the task's own applied effects created; the creator must
    /// hold the authority to hand them over.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub resources: BTreeSet<ResourceRef>,
    /// Capabilities the task's workflow requires from each executor family,
    /// such as launch readiness from the worker backend. An effect is refused
    /// on an executor that does not fully support the requirements for its
    /// own family, in addition to the capability the effect itself needs;
    /// other families' requirements do not apply to it.
    pub requires: CapabilityRequirements,
}

/// Capability requirements per executor family. Persisted as a map keyed by
/// executor kind; a repeated key is rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CapabilityRequirements(BTreeMap<ExecutorKind, BTreeSet<Capability>>);

impl CapabilityRequirements {
    /// No requirements.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Also require `capabilities` from executors of `executor`'s family.
    #[must_use]
    pub fn with(
        mut self,
        executor: ExecutorKind,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Self {
        self.0.entry(executor).or_default().extend(capabilities);
        self
    }

    /// The capabilities required from an executor of `executor`'s family.
    pub fn for_executor(&self, executor: ExecutorKind) -> impl Iterator<Item = Capability> + '_ {
        self.0.get(&executor).into_iter().flatten().copied()
    }

    /// Every executor family with requirements, and its capabilities.
    pub fn iter(&self) -> impl Iterator<Item = (ExecutorKind, &BTreeSet<Capability>)> {
        self.0
            .iter()
            .map(|(executor, capabilities)| (*executor, capabilities))
    }
}

impl<'de> Deserialize<'de> for CapabilityRequirements {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Requirements;

        impl<'de> serde::de::Visitor<'de> for Requirements {
            type Value = CapabilityRequirements;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map from executor kind to capabilities")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut map = BTreeMap::new();
                while let Some((executor, capabilities)) =
                    access.next_entry::<ExecutorKind, BTreeSet<Capability>>()?
                {
                    if map.insert(executor, capabilities).is_some() {
                        return Err(serde::de::Error::custom("duplicate executor kind"));
                    }
                }
                Ok(CapabilityRequirements(map))
            }
        }

        deserializer.deserialize_map(Requirements)
    }
}

impl TaskSpec {
    /// The scope that effects of this task are authorized against.
    #[must_use]
    pub fn scope(&self) -> GrantScope {
        self.repository
            .clone()
            .map_or(GrantScope::House, GrantScope::Repository)
    }
}

/// Whether a failed attempt may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// Another attempt may succeed.
    Retryable,
    /// Retrying cannot help.
    Permanent,
}

/// How an attempt ended, as reported by its claim holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "class", rename_all = "kebab-case")]
pub enum AttemptOutcome {
    /// The task's work is complete.
    Succeeded,
    /// The attempt failed.
    Failed(FailureClass),
}

/// The terminal state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Settlement {
    /// Work completed.
    Succeeded,
    /// Work failed permanently.
    Failed,
    /// Cancelled on request.
    Cancelled,
    /// The retry budget ran out.
    Exhausted,
}

impl fmt::Display for Settlement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Exhausted => "exhausted",
        })
    }
}

/// What happens after an attempt finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The task settled; the claim was released.
    Settled(Settlement),
    /// The holder keeps the claim and may start another attempt.
    RetryAvailable {
        /// Attempts left under the count bound.
        remaining: u32,
    },
}

/// The result of asking to start an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStart {
    /// A new attempt began.
    Started(AttemptNumber),
    /// This fence already has a running attempt; nothing changed.
    AlreadyRunning(AttemptNumber),
    /// The retry budget is spent; the task is now settled as exhausted.
    Exhausted,
}

/// A zero-based effect number within a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectSeq(u32);

impl EffectSeq {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The zero-based number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for EffectSeq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{}", self.0)
    }
}
