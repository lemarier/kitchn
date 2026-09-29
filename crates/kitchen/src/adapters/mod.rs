//! Orchestrator adapters behind the [`crate::contracts::WorkerBackend`] boundary.
//!
//! Each adapter maps Kitchen's orchestrator-neutral contracts onto one
//! runtime. Workflow and house policy never depend on these modules.

pub mod orca;
mod resolve;

pub use resolve::{BackendError, OrcaSession, backend_binding, resolve_backend};
