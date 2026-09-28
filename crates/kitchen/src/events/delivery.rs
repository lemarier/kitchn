//! The typed event envelope a delivery adapter hands to the intake.

use serde::{Deserialize, Serialize};

use crate::{
    contracts::{EventOrigin, Repository, Timestamp},
    events::EventError,
    state::{MarkerSubject, WorkItem},
};

/// Largest encoded envelope [`ForgeEvent::parse`] accepts.
pub const MAX_EVENT_BYTES: usize = 16 * 1024;

/// What happened to the work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForgeEventKind {
    /// An issue was opened.
    IssueOpened,
    /// A label was added to an issue.
    IssueLabeled,
    /// Someone commented on an issue.
    IssueCommented,
    /// A pull request was opened.
    PullRequestOpened,
    /// A pull request's head moved.
    PullRequestPushed,
    /// A review was submitted on a pull request.
    PullRequestReviewed,
    /// Someone commented on a pull request's diff.
    ReviewCommented,
}

impl ForgeEventKind {
    const fn is_pull_request(self) -> bool {
        match self {
            Self::IssueOpened | Self::IssueLabeled | Self::IssueCommented => false,
            Self::PullRequestOpened
            | Self::PullRequestPushed
            | Self::PullRequestReviewed
            | Self::ReviewCommented => true,
        }
    }
}

/// A delivered forge event: where it came from, what happened, the work item,
/// and the exact revision the source reported. Validated on construction and
/// deserialization, so a held value is consistent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawForgeEvent", into = "RawForgeEvent")]
pub struct ForgeEvent {
    repository: Repository,
    origin: EventOrigin,
    kind: ForgeEventKind,
    item: WorkItem,
    subject: MarkerSubject,
    occurred_at: Timestamp,
}

impl ForgeEvent {
    /// Validate an event.
    ///
    /// # Errors
    /// Returns [`EventError::UnsupportedItem`] for a work item that is not an
    /// issue or pull request, [`EventError::KindMismatch`] when `kind` is
    /// about the other item type, and [`EventError::SubjectMismatch`] when
    /// `subject` is not a Git revision for a pull request or a provider
    /// revision for an issue.
    pub fn new(
        origin: EventOrigin,
        kind: ForgeEventKind,
        item: WorkItem,
        subject: MarkerSubject,
        occurred_at: Timestamp,
    ) -> Result<Self, EventError> {
        let (repository, pull_request) = check_item(&item, &subject)?;
        if kind.is_pull_request() != pull_request {
            return Err(EventError::KindMismatch);
        }
        Ok(Self {
            repository,
            origin,
            kind,
            item,
            subject,
            occurred_at,
        })
    }

    /// Parse and validate an encoded envelope.
    ///
    /// # Errors
    /// Returns [`EventError::TooLarge`] beyond [`MAX_EVENT_BYTES`],
    /// [`EventError::Malformed`] for invalid JSON, unknown fields, or invalid
    /// values, and the errors of [`Self::new`].
    pub fn parse(encoded: &[u8]) -> Result<Self, EventError> {
        if encoded.len() > MAX_EVENT_BYTES {
            return Err(EventError::TooLarge {
                max: MAX_EVENT_BYTES,
            });
        }
        let raw: RawForgeEvent =
            serde_json::from_slice(encoded).map_err(|_| EventError::Malformed)?;
        Self::try_from(raw)
    }

    /// Where the event came from.
    #[must_use]
    pub const fn origin(&self) -> &EventOrigin {
        &self.origin
    }

    /// What happened.
    #[must_use]
    pub const fn kind(&self) -> ForgeEventKind {
        self.kind
    }

    /// The issue or pull request.
    #[must_use]
    pub const fn item(&self) -> &WorkItem {
        &self.item
    }

    /// The revision the source reported.
    #[must_use]
    pub const fn subject(&self) -> &MarkerSubject {
        &self.subject
    }

    /// When the source says the event happened.
    #[must_use]
    pub const fn occurred_at(&self) -> Timestamp {
        self.occurred_at
    }

    /// The item's repository.
    #[must_use]
    pub const fn repository(&self) -> &Repository {
        &self.repository
    }
}

/// A work item's revision observed by the fallback schedule's poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolledWork {
    repository: Repository,
    item: WorkItem,
    subject: MarkerSubject,
    observed_at: Timestamp,
}

impl PolledWork {
    /// Validate a polled observation.
    ///
    /// # Errors
    /// Returns [`EventError::UnsupportedItem`] or [`EventError::SubjectMismatch`]
    /// as [`ForgeEvent::new`] does.
    pub fn new(
        item: WorkItem,
        subject: MarkerSubject,
        observed_at: Timestamp,
    ) -> Result<Self, EventError> {
        let (repository, _) = check_item(&item, &subject)?;
        Ok(Self {
            repository,
            item,
            subject,
            observed_at,
        })
    }

    /// The issue or pull request.
    #[must_use]
    pub const fn item(&self) -> &WorkItem {
        &self.item
    }

    /// The observed revision.
    #[must_use]
    pub const fn subject(&self) -> &MarkerSubject {
        &self.subject
    }

    /// When the poll observed it.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// The item's repository.
    #[must_use]
    pub const fn repository(&self) -> &Repository {
        &self.repository
    }
}

/// The item's repository and whether it is a pull request, after checking
/// that it is an issue or a pull request and that `subject` is its kind of
/// revision.
fn check_item(item: &WorkItem, subject: &MarkerSubject) -> Result<(Repository, bool), EventError> {
    let (repository, pull_request) = match item {
        WorkItem::Issue { repository, .. } => (repository, false),
        WorkItem::PullRequest { repository, .. } => (repository, true),
        WorkItem::Resource { .. } | WorkItem::Repository { .. } | WorkItem::Task { .. } => {
            return Err(EventError::UnsupportedItem);
        }
    };
    match (subject, pull_request) {
        (MarkerSubject::Git(_), true) | (MarkerSubject::Issue(_), false) => {
            Ok((repository.clone(), pull_request))
        }
        (MarkerSubject::Git(_) | MarkerSubject::Issue(_) | MarkerSubject::Observation(_), _) => {
            Err(EventError::SubjectMismatch)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawForgeEvent {
    origin: EventOrigin,
    kind: ForgeEventKind,
    item: WorkItem,
    subject: MarkerSubject,
    occurred_at: Timestamp,
}

impl TryFrom<RawForgeEvent> for ForgeEvent {
    type Error = EventError;

    fn try_from(raw: RawForgeEvent) -> Result<Self, Self::Error> {
        Self::new(raw.origin, raw.kind, raw.item, raw.subject, raw.occurred_at)
    }
}

impl From<ForgeEvent> for RawForgeEvent {
    fn from(event: ForgeEvent) -> Self {
        Self {
            origin: event.origin,
            kind: event.kind,
            item: event.item,
            subject: event.subject,
            occurred_at: event.occurred_at,
        }
    }
}
