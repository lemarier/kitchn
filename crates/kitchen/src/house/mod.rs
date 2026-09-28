//! House-scoped configuration, policy, repository requirements, and diagnostics.
//! Configuration grants no authority merely by being installed.
mod config;
mod doctor;
mod error;
mod readiness;
mod requirements;
mod roles;

pub use config::*;
pub use doctor::*;
pub use error::HouseError;
pub use readiness::*;
pub use requirements::*;
pub use roles::*;
