//! Single-consumer workflow leases and their transfer audit.
//!
//! A workflow scope (for example, pickup for one house and repository set)
//! has at most one consumer. Ownership changes hands in three recorded ways:
//! an explicit relinquish followed by an adoption, a takeover of an expired
//! lease, or a release when the consumer stops with nothing in flight. An
//! expired lease is uncertain, never released.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{
    HolderId,
    contracts::{Fence, Timestamp},
    state::{Corruption, Lease},
};

/// Recent transfer events kept per consumer. Older events are dropped; the
/// history is an audit trail, not the source of the current state.
pub const MAX_CONSUMER_HISTORY: usize = 64;

/// Who holds a consumer scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ConsumerState {
    /// Released; the next consumer acquires it.
    Idle,
    /// Held under a lease. Check [`Lease::is_live`] for uncertainty.
    Held {
        /// The consumer's lease.
        lease: Lease,
    },
    /// The holder handed the scope over with work possibly in flight; the
    /// next consumer adopts it.
    Relinquished {
        /// The relinquished lease.
        lease: Lease,
        /// When it was relinquished.
        at: Timestamp,
    },
}

/// A recorded change of consumer ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ConsumerEvent {
    /// A consumer acquired an idle scope.
    Acquired {
        /// New holder.
        holder: HolderId,
        /// New fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// The holder handed the scope over.
    Relinquished {
        /// Relinquished fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// A consumer adopted a relinquished scope.
    Adopted {
        /// The relinquished fence.
        previous: Fence,
        /// New holder.
        holder: HolderId,
        /// New fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// A consumer took over an expired lease without a relinquish.
    TakenOver {
        /// The superseded fence.
        previous: Fence,
        /// New holder.
        holder: HolderId,
        /// New fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
    /// The holder stopped with nothing in flight.
    Released {
        /// Released fence.
        fence: Fence,
        /// When.
        at: Timestamp,
    },
}

/// A consumer scope's current holder and recent transfers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumerRecord {
    pub(super) state: ConsumerState,
    pub(super) history: VecDeque<ConsumerEvent>,
}

impl ConsumerRecord {
    pub(super) const fn new(state: ConsumerState) -> Self {
        Self {
            state,
            history: VecDeque::new(),
        }
    }

    /// Who holds the scope.
    #[must_use]
    pub const fn state(&self) -> &ConsumerState {
        &self.state
    }

    /// The current lease, when held.
    #[must_use]
    pub const fn lease(&self) -> Option<&Lease> {
        match &self.state {
            ConsumerState::Held { lease } => Some(lease),
            ConsumerState::Idle | ConsumerState::Relinquished { .. } => None,
        }
    }

    /// Recent transfers, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &ConsumerEvent> {
        self.history.iter()
    }

    pub(super) fn record(&mut self, state: ConsumerState, event: ConsumerEvent) {
        if self.history.len() >= MAX_CONSUMER_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(event);
        self.state = state;
    }

    /// The state must match the latest event, and no fence may be unissued.
    pub(super) fn validate(&self, next_fence: u64) -> Result<(), Corruption> {
        if self.history.len() > MAX_CONSUMER_HISTORY {
            return Err(Corruption::LimitExceeded);
        }
        let issued = |fence: Fence| fence.get() < next_fence;
        let consistent = match (&self.state, self.history.back()) {
            (ConsumerState::Idle, Some(ConsumerEvent::Released { .. })) => true,
            (
                ConsumerState::Held { lease },
                Some(
                    ConsumerEvent::Acquired { holder, fence, .. }
                    | ConsumerEvent::Adopted { holder, fence, .. }
                    | ConsumerEvent::TakenOver { holder, fence, .. },
                ),
            ) => lease.holder() == holder && lease.fence() == *fence && issued(*fence),
            (
                ConsumerState::Relinquished { lease, .. },
                Some(ConsumerEvent::Relinquished { fence, .. }),
            ) => lease.fence() == *fence && issued(*fence),
            (
                ConsumerState::Idle
                | ConsumerState::Held { .. }
                | ConsumerState::Relinquished { .. },
                None
                | Some(
                    ConsumerEvent::Acquired { .. }
                    | ConsumerEvent::Relinquished { .. }
                    | ConsumerEvent::Adopted { .. }
                    | ConsumerEvent::TakenOver { .. }
                    | ConsumerEvent::Released { .. },
                ),
            ) => false,
        };
        if consistent {
            Ok(())
        } else {
            Err(Corruption::Ownership)
        }
    }
}
