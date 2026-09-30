//! House-private evidence and explicit, revocable standing authority.
//!
//! Observations are immutable revisions, never authority. Bind a prospective task,
//! project approved current grants with [`Ledger::standing_for_task`], then
//! delegate its core authority. Project again before each effect and pass that
//! value to the core executor. Revocation cannot cancel an effect already
//! submitted. Interactive consent is never stored here. Runtime files must
//! live outside repositories.
//!
//! Earned standing is tied to the exact instruction pins of the evidence tasks:
//! changing any pin voids it until new evidence is earned. Graduation to
//! unattended runs handles guidance changes by house policy through explicit
//! owner decisions ([`GraduationDecision`]).
//!
//! History stays until an operator archives records no grant needs; see
//! [`Ledger::archive`].
mod archive;
mod error;
mod graduation;
mod model;
mod store;

pub use archive::{
    ARCHIVE_FILE, ARCHIVE_SCHEMA, Archival, ArchiveBatch, ArchiveDigest, ArchiveReport,
    ArchivedStream, KeptRecords,
};
pub use error::TrustError;
pub use graduation::*;
pub use model::*;
pub use store::EARNED_AUTONOMY_PERMISSIONS;
pub use store::{Capacity, Ledger};
pub(crate) use store::{Document, store_error};
