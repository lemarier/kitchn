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
//! the same task instead of creating another. An event that occurred before
//! an already admitted event about a newer revision of the same item is
//! reported stale; polls do not take part in that ordering. Both the event
//! receiver and the fallback schedule act under one consumer lease, so only
//! one of them consumes the workflow scope at a time.
//!
//! Event-started work is unattended: its tasks run on standing grants only and
//! never accept a person's consent.

mod delivery;
mod error;
mod intake;

pub use delivery::{ForgeEvent, ForgeEventKind, MAX_EVENT_BYTES, PolledWork};
pub use error::EventError;
pub use intake::{ADMISSION_SCHEMA, Admission, EventIntake, EventRoute, WorkOrder};
