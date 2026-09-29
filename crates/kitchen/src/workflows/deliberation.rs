//! Multi-agent deliberation threads and the context records they publish.
//!
//! Several role agents discuss one task in a [`Thread`]. A participant speaks
//! when mentioned; otherwise any participant may reply, within the thread's
//! turn, participant, and usage bounds. Reaching a bound ends the thread as
//! cut off. A closed thread publishes an immutable [`ContextRecord`]: the
//! decisions, the options rejected and why, open questions, and the messages
//! each decision came from. The record is pinned to the task, and later tasks
//! pin it by identity, so the station cook receives the same context instead
//! of a retyped summary.
//!
//! Storage: threads, records, and pins are typed workflow markers in the
//! house's [`crate::state::HouseStore`] under the [`DELIBERATION_WORKFLOW`]
//! workflow, keyed by the task. Each thread entry is one marker keyed by its
//! sequence number and written in one store transaction that replays the
//! thread and admits the entry against it, so there is no second copy of
//! thread state to drift, and a crash leaves either the whole entry or none.
//! A retried turn carries the same [`Turn::key`] and is reported as a
//! duplicate instead of being written again.
//!
//! Delivery and decisions: turns are delivered to participants through the
//! worker backend's messaging ([`turn_message`]), and a backend without
//! [`crate::contracts::Capability::WorkerMessaging`] is refused before a
//! thread opens. A question for a person is an ordinary Roger Ask bound to
//! the task ([`Deliberations::ask_human`]); the thread resumes only on an
//! answer to exactly that Ask at the task's current evidence revision.
//! Deliberation grants nothing: a thread can only ask questions, and its
//! records are data for later tasks, never permissions.
//!
//! Message text comes from agents and people, so it is untrusted. Everything
//! Kitchen renders from it ([`turn_message`], [`context_brief`],
//! [`Deliberations::ask_human`]) quotes it as delimited data.

mod brief;
mod record;
mod store;
mod thread;

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    ErrorClass, HouseId, IdentifierError,
    contracts::{
        BackendDescriptor, Capability, CapabilityRequirements, ContractError, ExecutorKind, Role,
    },
    id::validate_identifier,
};

pub use brief::{MAX_CONTEXT_BYTES, context_brief, mentions_in, turn_message};
pub use record::{
    ContextRecord, MAX_PINS_PER_TASK, MAX_RECORD_ITEM_BYTES, MAX_RECORD_ITEMS, PinnedRecord,
    RecordDecision, RecordDraft, RecordRef, RejectedOption, TaskContext,
};
pub use store::{AnswerOutcome, Deliberations, HumanQuestion, Posting, task_context};
pub use thread::{
    Closure, CutOffReason, Entry, HumanAnswer, MAX_HUMAN_QUESTIONS, MAX_PARTICIPANTS,
    MAX_THREAD_ENTRIES, MAX_TOPIC_BYTES, MAX_TURN_BYTES, MAX_TURNS, MessageSeq, NextTurn,
    Participant, Thread, ThreadBounds, ThreadSpec, ThreadStatus, Turn, TurnUsage,
};

/// Workflow id under which threads, records, and pins are recorded.
pub const DELIBERATION_WORKFLOW: &str = "deliberation";

macro_rules! deliberation_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate an identifier: 1–64 ASCII letters, digits, `-`, or `_`,
            /// starting with a letter or digit.
            ///
            /// # Errors
            /// Returns an [`IdentifierError`] without echoing the input.
            pub fn new(value: &str) -> Result<Self, IdentifierError> {
                validate_identifier(value)?;
                Ok(Self(value.to_owned()))
            }

            /// Borrow the identifier.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                validate_identifier(&value)?;
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}

deliberation_id!(
    ThreadId,
    "A deliberation thread identity, unique within its task."
);
deliberation_id!(
    RecordId,
    "A context record identity, unique within its house."
);

/// Why a deliberation operation was refused or failed. Message text is never
/// included.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeliberationError {
    /// The thread specification is outside its bounds: participant count,
    /// duplicate roles, a participant that is not a worker, or topic size.
    #[error("invalid thread specification")]
    InvalidSpec,
    /// A turn, question, or record field is empty, oversized, or malformed.
    #[error("invalid deliberation content")]
    InvalidContent,
    /// The role is not a participant of the thread.
    #[error("{0} is not a participant of the thread")]
    NotParticipant(Role),
    /// A role that is already a participant was invited again.
    #[error("{0} is already a participant of the thread")]
    AlreadyParticipant(Role),
    /// A mentioned participant holds the next turn.
    #[error("the next turn belongs to {expected}")]
    NotYourTurn {
        /// The mentioned participant that holds the turn.
        expected: Role,
    },
    /// The thread is waiting for a person's answer.
    #[error("the thread is waiting for a human answer")]
    AwaitingHuman,
    /// The thread is not waiting for a person's answer.
    #[error("the thread is not waiting for a human answer")]
    NotAwaitingHuman,
    /// The thread has ended.
    #[error("the thread has ended")]
    ThreadClosed,
    /// The thread has not ended, so it has no record yet.
    #[error("the thread has not ended")]
    ThreadOpen,
    /// The thread already asked its maximum number of questions.
    #[error("the thread asked its maximum of {MAX_HUMAN_QUESTIONS} human questions")]
    QuestionBound,
    /// No thread with this identity exists for the task.
    #[error("unknown deliberation thread")]
    UnknownThread,
    /// A thread with this identity exists with another specification.
    #[error("a different thread already uses this identity")]
    ThreadExists,
    /// Another writer changed the thread; read it again before retrying.
    #[error("the thread changed concurrently")]
    ThreadMoved,
    /// A decision or rejected option cites a message that is not a turn or
    /// human answer of the summarized thread.
    #[error("record source {0} is not a message of the thread")]
    UnknownSource(MessageSeq),
    /// A published record is immutable; this identity holds other content.
    #[error("a different record was already published under this identity")]
    RecordImmutable,
    /// No record with this identity exists in the house.
    #[error("unknown context record")]
    UnknownRecord,
    /// The superseded record is already superseded, or the thread's current
    /// record was not named as superseded.
    #[error("the record does not supersede the current record")]
    NotCurrentRecord,
    /// The task already pins its maximum number of records.
    #[error("the task pins its maximum number of records")]
    PinBound,
    /// A human decision does not belong to this thread's pending question,
    /// or is an approval, which deliberation never accepts.
    #[error("human decision does not match the thread's question")]
    DecisionMismatch,
    /// The task's evidence moved after the question was asked.
    #[error("human answer is for an older task revision")]
    StaleAnswer,
    /// The pinned context would exceed its bound or the brief's; nothing is
    /// truncated.
    #[error("the pinned context does not fit the brief")]
    ContextTooLarge,
    /// Persisted deliberation markers are inconsistent.
    #[error("deliberation markers are inconsistent")]
    Corrupt,
}

impl DeliberationError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidSpec
            | Self::InvalidContent
            | Self::UnknownSource(_)
            | Self::UnknownThread
            | Self::UnknownRecord => ErrorClass::InvalidInput,
            Self::NotParticipant(_)
            | Self::AlreadyParticipant(_)
            | Self::NotYourTurn { .. }
            | Self::AwaitingHuman
            | Self::NotAwaitingHuman
            | Self::ThreadClosed
            | Self::ThreadOpen
            | Self::QuestionBound
            | Self::PinBound
            | Self::ContextTooLarge
            | Self::DecisionMismatch
            | Self::StaleAnswer => ErrorClass::Refused,
            Self::ThreadExists
            | Self::ThreadMoved
            | Self::RecordImmutable
            | Self::NotCurrentRecord => ErrorClass::Conflict,
            Self::Corrupt => ErrorClass::Execution,
        }
    }
}

/// Capabilities a deliberation task requires: turns are delivered through
/// worker messaging. Put these in the task's
/// [`crate::contracts::TaskSpec::requires`] so the store refuses any effect
/// on a backend without them.
#[must_use]
pub fn requirements() -> CapabilityRequirements {
    CapabilityRequirements::new().with(ExecutorKind::Worker, [Capability::WorkerMessaging])
}

/// Refuse a worker backend that cannot deliver turns or serves another house.
///
/// # Errors
/// Returns [`ContractError::CrossHouse`] for another house's backend and
/// [`ContractError::UnsupportedCapabilities`] naming
/// [`Capability::WorkerMessaging`] when it is missing or partial.
pub fn check_backend(backend: &BackendDescriptor, house: &HouseId) -> Result<(), ContractError> {
    if &backend.house != house {
        return Err(ContractError::CrossHouse {
            expected: house.clone(),
            found: backend.house.clone(),
        });
    }
    backend
        .capabilities
        .require(requirements().for_executor(ExecutorKind::Worker))
}
