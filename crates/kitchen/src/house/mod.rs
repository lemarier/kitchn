//! House-scoped configuration, policy, repository requirements, and diagnostics.
//! Configuration grants no authority merely by being installed.
mod config;
mod error;
mod requirements;
mod roles;

pub use config::*;
pub use error::HouseError;
pub use requirements::*;
pub use roles::*;
