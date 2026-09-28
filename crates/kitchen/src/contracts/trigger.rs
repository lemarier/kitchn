//! What started a piece of work, and where its authority comes from.
//!
//! The same task and effect code serves both triggers:
//!
//! - [`Trigger::Scheduled`]: no person is present. Effects act only on the
//!   task's authority delegated from the house's standing grants.
//! - [`Trigger::Interactive`]: a person is present. Each effect needs that
//!   person's [`Consent`] for exactly that effect, bounded by house policy
//!   limits. Consent is never stored as a grant and never covers another
//!   effect.
//!
//! Both triggers claim work through the same durable claims, so an item
//! claimed by one is refused to the other until a recorded relinquish and
//! adoption, a takeover after expiry, or settlement.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    HolderId, HouseId, TaskId,
    contracts::{ContractError, Effect, EvidenceRevision, ExternalRef},
};

/// What started a piece of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// A scheduled run with no person present.
    Scheduled,
    /// A session with a person present.
    Interactive,
}

impl fmt::Display for Trigger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Scheduled => "scheduled",
            Self::Interactive => "interactive",
        })
    }
}

/// Who claims work, and under which trigger.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claimant {
    /// The claiming instance, such as one scheduled tick or one session.
    pub holder: HolderId,
    /// The trigger it acts under.
    pub trigger: Trigger,
}

impl Claimant {
    /// A scheduled claimant.
    #[must_use]
    pub const fn scheduled(holder: HolderId) -> Self {
        Self {
            holder,
            trigger: Trigger::Scheduled,
        }
    }

    /// An interactive claimant.
    #[must_use]
    pub const fn interactive(holder: HolderId) -> Self {
        Self {
            holder,
            trigger: Trigger::Interactive,
        }
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
