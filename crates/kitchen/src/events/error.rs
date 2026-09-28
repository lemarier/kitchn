//! Errors of the event intake. Rejected input is never echoed.

use crate::{ErrorClass, contracts::Repository};

/// A refused or malformed event, or a misconfigured intake.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EventError {
    /// The envelope is not a valid event.
    #[error("event envelope could not be parsed")]
    Malformed,
    /// The envelope exceeds the size bound.
    #[error("event envelope exceeds {max} bytes")]
    TooLarge {
        /// Largest accepted envelope.
        max: usize,
    },
    /// Only issues and pull requests start event work.
    #[error("event work item must be an issue or a pull request")]
    UnsupportedItem,
    /// The event kind is about another kind of work item.
    #[error("event kind does not match its work item")]
    KindMismatch,
    /// The revision is not the kind a work item of this type has: a Git head
    /// for a pull request, a provider revision for an issue.
    #[error("event revision does not match its work item")]
    SubjectMismatch,
    /// A route must start work for at least one event kind.
    #[error("an event route must name at least one event kind")]
    EmptyRoute,
    /// The event arrived from a source other than the intake's delivering
    /// backend.
    #[error("event source is not this intake's delivering backend")]
    UnknownSource,
    /// The event names a repository the house is not bound to.
    #[error("repository {0} is not bound to this house")]
    RepositoryNotBound(Repository),
    /// The claimant does not act under the admitting trigger: the event's own
    /// origin for an event, a scheduled run for a poll.
    #[error("claimant does not act under the admitting trigger")]
    TriggerMismatch,
    /// The claimant does not act under the route's consumer lease.
    #[error("claimant must act under the route's consumer lease")]
    ConsumerRequired,
    /// The planned task does not have the work order's id and repository.
    #[error("planned task does not match its work order")]
    PlanMismatch,
    /// The work key could not be encoded.
    #[error("event work key could not be encoded")]
    Encoding,
}

impl EventError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Malformed
            | Self::TooLarge { .. }
            | Self::UnsupportedItem
            | Self::KindMismatch
            | Self::SubjectMismatch
            | Self::EmptyRoute
            | Self::PlanMismatch => ErrorClass::InvalidInput,
            Self::UnknownSource
            | Self::RepositoryNotBound(_)
            | Self::TriggerMismatch
            | Self::ConsumerRequired => ErrorClass::Refused,
            Self::Encoding => ErrorClass::Execution,
        }
    }
}
