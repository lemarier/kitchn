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
//! changing any pin voids it until new evidence is earned. Re-evaluating trust
//! per guidance revision by policy is planned in #44.
mod error;
mod model;
mod store;

pub use error::TrustError;
pub use model::*;
pub use store::EARNED_AUTONOMY_PERMISSIONS;
pub use store::Ledger;
