//! What started a piece of work, and where its authority comes from.
//!
//! The same task and effect code serves every trigger:
//!
//! - [`Trigger::Scheduled`]: no person is present. Effects act only on the
//!   task's authority delegated from the house's standing grants.
//! - [`Trigger::Event`]: a delivered forge event started the work. No person
//!   is present, so it is unattended exactly like a scheduled run: effects
//!   use standing grants only and consent is refused, even when a person's
//!   action caused the event.
//! - [`Trigger::Interactive`]: a person is present. Each effect needs that
//!   person's [`Consent`] for exactly that effect, bounded by house policy
//!   limits. Consent is never stored as a grant and never covers another
//!   effect.
//!
//! Every trigger claims work through the same durable claims, so an item
//! claimed by one is refused to the others until a recorded relinquish and
//! adoption, a takeover after expiry, or settlement.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, ConsumerId, HolderId, HouseId, TaskId,
    contracts::{ContractError, Effect, EvidenceRevision, ExternalRef, Fence},
};

/// What started a piece of work.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// A scheduled run with no person present.
    Scheduled,
    /// A session with a person present.
    Interactive,
    /// A delivered forge event, with no person present.
    Event(EventOrigin),
}

impl Trigger {
    /// Whether no person is present, so effects use standing grants only
    /// and consent is refused.
    #[must_use]
    pub const fn is_unattended(&self) -> bool {
        match self {
            Self::Scheduled | Self::Event(_) => true,
            Self::Interactive => false,
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheduled => formatter.write_str("scheduled"),
            Self::Interactive => formatter.write_str("interactive"),
            Self::Event(origin) => write!(formatter, "event {origin}"),
        }
    }
}

/// Where a delivered event came from: the house it was delivered for, the
/// source namespace that delivered it, and the source's own identity for the
/// event. A redelivery of the same event carries the same origin.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventOrigin {
    /// The house the event was delivered for.
    pub house: HouseId,
    /// The delivering source: one forge namespace (instance and account),
    /// named like a backend.
    pub source: BackendId,
    /// The source's identity for this event, such as a webhook delivery id.
    /// Unique within `source`; a redelivery keeps it.
    pub event: ExternalRef,
}

impl fmt::Display for EventOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/{}", self.house, self.source, self.event)
    }
}

/// A workflow consumer lease a claimant acts under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumerFence {
    /// The consumer scope.
    pub consumer: ConsumerId,
    /// The consumer lease's fence.
    pub fence: Fence,
}

/// Who claims work, and under which trigger.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claimant {
    /// The claiming instance, such as one scheduled tick or one session.
    pub holder: HolderId,
    /// The trigger it acts under.
    pub trigger: Trigger,
    /// The workflow consumer lease the claimant acts under, if any. When set,
    /// task creation, claims, and every effect of the claim require that
    /// lease to be current and live, so a superseded consumer cannot act.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer: Option<ConsumerFence>,
}

impl Claimant {
    /// A scheduled claimant.
    #[must_use]
    pub const fn scheduled(holder: HolderId) -> Self {
        Self {
            holder,
            trigger: Trigger::Scheduled,
            consumer: None,
        }
    }

    /// A claimant acting on a delivered event.
    #[must_use]
    pub const fn event(holder: HolderId, origin: EventOrigin) -> Self {
        Self {
            holder,
            trigger: Trigger::Event(origin),
            consumer: None,
        }
    }

    /// An interactive claimant.
    #[must_use]
    pub const fn interactive(holder: HolderId) -> Self {
        Self {
            holder,
            trigger: Trigger::Interactive,
            consumer: None,
        }
    }

    /// Act under the consumer lease `fence` for `consumer`.
    #[must_use]
    pub fn under(mut self, consumer: ConsumerId, fence: Fence) -> Self {
        self.consumer = Some(ConsumerFence { consumer, fence });
        self
    }
}

/// A person's approval of exactly one effect: this house, task, effect,
/// and evidence revision. The caller obtains it from the person for each
/// action; it is not a grant and is recorded only as audit on the effect it
/// authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consent {
    /// A unique reference for this approval, chosen by the session.
    pub id: ExternalRef,
    /// The person who approved.
    pub given_by: HolderId,
    /// The house.
    pub house: HouseId,
    /// The task.
    pub task: TaskId,
    /// The exact effect approved.
    pub effect: Effect,
    /// The evidence revision the person saw.
    pub revision: EvidenceRevision,
}

impl Consent {
    /// Check that this consent covers the effect.
    ///
    /// # Errors
    /// Returns [`ContractError::ConsentMismatch`] when any part differs.
    pub fn check(
        &self,
        house: &HouseId,
        task: &TaskId,
        effect: &Effect,
        revision: EvidenceRevision,
    ) -> Result<(), ContractError> {
        if &self.house == house
            && &self.task == task
            && &self.effect == effect
            && self.revision == revision
        {
            Ok(())
        } else {
            Err(ContractError::ConsentMismatch)
        }
    }
}

/// The recorded source of an effect's authority, for audit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Authorization {
    /// The task's standing authority, under a scheduled claim.
    Standing,
    /// A person's consent, under an interactive claim.
    #[serde(rename_all = "camelCase")]
    Consent {
        /// The consent reference.
        id: ExternalRef,
        /// Who approved.
        given_by: HolderId,
    },
}
