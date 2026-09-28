//! House-private evidence and explicit autonomy restrictions.
//!
//! Observations are immutable revisions, never authority. Grants restrict existing
//! core standing grants; consumers must call [`Ledger::authorize`] immediately
//! before every effect as well as the core executor's authority checks. Revocation
//! cannot cancel an effect already submitted. Interactive consent is never stored
//! here. Runtime files must live outside repositories.
mod error;
mod model;
mod store;

pub use error::TrustError;
pub use model::*;
pub use store::Ledger;
