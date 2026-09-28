//! Event-started work: the typed intake for delivered forge events and the
//! polling fallback that shares its work identity.
//!
//! A delivery adapter (such as the GitHub integration) turns a webhook into a
//! validated [`ForgeEvent`]; delivery itself is outside this module. The
//! [`EventIntake`] then decides, before any task exists:
//!
//! - the event belongs to the store's house and arrived from the intake's own
//!   source, a backend that declares [`crate::contracts::Capability::EventDelivery`];
//! - its repository is bound to the house;
//! - the claimant acts under the event's own [`crate::contracts::Trigger::Event`]
//!   (or [`crate::contracts::Trigger::Scheduled`] for a poll), never an
//!   interactive one, and under the route's consumer lease.
//!
//! Admitted work is keyed by workflow, work item, and the exact revision the
//! event or poll observed. The key is recorded as a workflow marker before the
//! task is created, and the task id is derived from the key, so a redelivery,
//! a poll of the same revision, or a restart between receipt and claim finds
//! the same task instead of creating another. Admissions are ordered only by
//! the store's own receipt order, never by the event's provider-supplied time,
//! which is kept for audit. Neither order says which revision is newer, so
//! before creating work the intake asks the forge, through a
//! [`RevisionSource`], whether the revision is still the item's current one;
//! a late event for a superseded head is reported [`Admission::Superseded`]
//! and writes nothing. Finishing an interrupted
//! admission is reported stale once the store has received another revision
//! of the same item since. Both the event receiver and the fallback schedule
//! act under one consumer lease, so only one of them consumes the workflow
//! scope at a time. The stale check and the marker write are one store
//! transaction, so racing deliveries under the same live fence are ordered by
//! the store.
//!
//! # One task per revision
//!
//! Deduplication is per (workflow, item, revision), never per event: two
//! distinct events about the same revision, such as a second review comment
//! on one head or two labels applied within the same second, produce one task,
//! and the later event is reported [`Admission::Duplicate`]. This is
//! intentional. A workflow that needs a task for each event must include the
//! event's identity (a comment id, for example) in the work item key it
//! routes on, so each event is a different item.
//!
//! Event-started work is unattended: its tasks run on standing grants only and
//! never accept a person's consent.

mod delivery;
mod error;
mod intake;

pub use delivery::{ForgeEvent, ForgeEventKind, MAX_EVENT_BYTES, PolledWork};
pub use error::EventError;
pub use intake::{
    ADMISSION_SCHEMA, Admission, EventIntake, EventRoute, RevisionSource, RevisionState, WorkOrder,
};
