//! The coordinator mailbox: worker deliveries, acknowledgement, and run
//! adoption.
//!
//! Workers report to their coordinator through the backend: questions,
//! completion reports, escalations, and liveness notes. A backend that
//! declares [`Capability::WorkerDeliveries`] hands them over in batches
//! ([`Delivery`]). Delivery is at least once per message: the backend replays
//! the oldest unacknowledged batch until the coordinator acknowledges it by
//! its batch id, so a coordinator that crashes between reading and handling
//! a batch loses nothing. A backend that declares [`Capability::RunTransfer`]
//! lets a new coordinator adopt the run after a restart; the previous one is
//! then fenced from the mailbox, and the adopter receives every
//! unacknowledged message, possibly regrouped under a new batch id. A handler
//! that must not act twice deduplicates by message id, never by batch id.
//! Coordination reads deliveries from the backend when it declares them and
//! from the house store's mailbox ([`crate::state::HouseMailbox`]) when it
//! does not ([`crate::workflows::coordination::MailboxRoute`]).
//!
//! The contract says nothing about where the mailbox lives, so a backend
//! may transport messages that Kitchen itself stores. Checked by
//! [`crate::contracts::conformance::run_mailbox`].

use std::time::Duration;

use crate::contracts::{
    BackendUnavailable, ExternalRef, ResourceRef, Text, WorkerBackend, WorkerOutcome,
};

#[cfg(doc)]
use crate::contracts::Capability;

/// Longest single mailbox wait.
pub const MAX_MAILBOX_WAIT: Duration = Duration::from_secs(15 * 60);

/// What a mailbox message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// A worker question that expects a reply.
    Question,
    /// A worker's terminal report.
    WorkerDone,
    /// A worker needs the coordinator to act.
    Escalation,
    /// A liveness signal; never completion evidence.
    Heartbeat,
    /// Status or any other informational type.
    Status,
    /// A type Kitchen does not recognize.
    Other,
}

/// One mailbox message, bounded and typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    /// The backend's message id, used to reply.
    pub id: ExternalRef,
    /// The message type.
    pub kind: MessageKind,
    /// The sending worker, when the backend names it.
    pub worker: Option<ResourceRef>,
    /// The worker's reported outcome, only on [`MessageKind::WorkerDone`].
    pub outcome: Option<WorkerOutcome>,
    /// The subject, truncated to Kitchen's text bound.
    pub subject: Option<Text>,
    /// The body, truncated to Kitchen's text bound.
    pub body: Option<Text>,
}

/// One unacknowledged mailbox batch. The backend replays it until acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The id that acknowledges the batch. It names this grouping only: after
    /// run adoption the same messages can return under another id.
    pub id: ExternalRef,
    /// Messages in arrival order. Rows Kitchen cannot identify are dropped,
    /// counted in `unreadable`.
    pub messages: Vec<MailMessage>,
    /// Rows without a valid id; the batch must not be acknowledged blindly.
    pub unreadable: usize,
}

impl Delivery {
    /// Messages that need the coordinator: questions, reports, escalations,
    /// and unrecognized types. Heartbeats and status notes are liveness and
    /// information only. Whether a batch may be acknowledged without
    /// handling is [`Self::is_idle`], not an empty result here, since an
    /// unreadable row may be a report or escalation.
    pub fn actionable(&self) -> impl Iterator<Item = &MailMessage> {
        self.messages.iter().filter(|message| {
            matches!(
                message.kind,
                MessageKind::Question
                    | MessageKind::WorkerDone
                    | MessageKind::Escalation
                    | MessageKind::Other
            )
        })
    }

    /// Whether the batch holds nothing to handle: no actionable message and
    /// no unreadable row. Only such a batch is acknowledged without handling
    /// while the wait continues; any other batch goes to the coordinator.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.unreadable == 0 && self.actionable().next().is_none()
    }
}

/// A mailbox call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MailboxError {
    /// Another coordinator adopted the run: this one no longer reads its
    /// mailbox and must stop consuming.
    #[error("another coordinator adopted the run")]
    Fenced,
    /// The mailbox could not be read; nothing may be inferred from it,
    /// including that no message is waiting.
    #[error(transparent)]
    Unavailable(#[from] BackendUnavailable),
}

/// A worker backend that carries worker deliveries to its coordinator.
///
/// Contract, checked by [`crate::contracts::conformance::run_mailbox`]:
///
/// - Without a declared [`Capability::WorkerDeliveries`], the delivery calls
///   return [`BackendUnavailable::Unsupported`]; without a declared
///   [`Capability::RunTransfer`], so does [`Self::adopt_run`].
/// - Delivery is at least once: [`Self::next_delivery`] returns the oldest
///   unacknowledged batch, the same one on every call, until
///   [`Self::acknowledge`] names it. Messages arrive in the order workers
///   sent them.
/// - Acknowledging a batch that is not the current one, because it was
///   already acknowledged or was read before an adoption, succeeds, consumes
///   nothing, and returns the current batch.
/// - After a restart, a new instance that adopts the run receives every
///   unacknowledged message, oldest first, possibly in a batch with a new
///   id; it acknowledges by that new id. The previous instance's calls fail
///   with [`MailboxError::Fenced`]. Adoption stops and moves no worker.
/// - `None` means nothing is waiting now. It is a checkpoint, never evidence
///   that a worker stopped.
pub trait CoordinatorMailbox: WorkerBackend {
    /// Make this instance the run's coordinator, fencing the previous one.
    ///
    /// Call this only after Kitchen's store recorded the adoption, such as a
    /// relinquish followed by an adopt.
    ///
    /// # Errors
    /// [`MailboxError::Unavailable`] when the backend cannot be reached or
    /// does not declare [`Capability::RunTransfer`].
    fn adopt_run(&self) -> Result<(), MailboxError>;

    /// Read the oldest unacknowledged batch without consuming it.
    ///
    /// # Errors
    /// [`MailboxError::Fenced`] after another coordinator adopted the run,
    /// and [`MailboxError::Unavailable`].
    fn next_delivery(&self) -> Result<Option<Delivery>, MailboxError>;

    /// Acknowledge a batch after every message in it was handled, and read
    /// the next one.
    ///
    /// # Errors
    /// As for [`Self::next_delivery`].
    fn acknowledge(&self, delivery: &ExternalRef) -> Result<Option<Delivery>, MailboxError>;

    /// Wait up to `wait` (at most [`MAX_MAILBOX_WAIT`]) for a question,
    /// report, or escalation, and return the oldest unacknowledged batch.
    /// The batch can hold heartbeats and notes too; use
    /// [`Delivery::actionable`] and keep waiting when it has none.
    ///
    /// # Errors
    /// As for [`Self::next_delivery`].
    fn await_delivery(&self, wait: Duration) -> Result<Option<Delivery>, MailboxError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(
        id: &str,
        kind: MessageKind,
    ) -> Result<MailMessage, crate::contracts::ContractError> {
        Ok(MailMessage {
            id: ExternalRef::new(id)?,
            kind,
            worker: None,
            outcome: None,
            subject: None,
            body: None,
        })
    }

    #[test]
    fn only_liveness_and_notes_are_not_actionable() -> Result<(), Box<dyn std::error::Error>> {
        let kinds = [
            MessageKind::Question,
            MessageKind::WorkerDone,
            MessageKind::Escalation,
            MessageKind::Heartbeat,
            MessageKind::Status,
            MessageKind::Other,
        ];
        let delivery = Delivery {
            id: ExternalRef::new("d1")?,
            messages: kinds
                .iter()
                .enumerate()
                .map(|(n, kind)| message(&format!("m{n}"), *kind))
                .collect::<Result<_, _>>()?,
            unreadable: 0,
        };
        let actionable: Vec<_> = delivery.actionable().map(|m| m.kind).collect();
        assert_eq!(
            actionable,
            [
                MessageKind::Question,
                MessageKind::WorkerDone,
                MessageKind::Escalation,
                MessageKind::Other,
            ]
        );
        assert!(!delivery.is_idle());
        let empty = Delivery {
            messages: Vec::new(),
            ..delivery
        };
        assert_eq!(empty.actionable().count(), 0);
        assert!(empty.is_idle());
        // Only heartbeats, but a row could not be read: it may be a report,
        // so the batch is not idle and is never acknowledged blindly.
        let heartbeats = Delivery {
            messages: vec![message("h", MessageKind::Heartbeat)?],
            unreadable: 1,
            ..empty
        };
        assert_eq!(heartbeats.actionable().count(), 0);
        assert!(!heartbeats.is_idle());
        Ok(())
    }
}
