//! Kitchen's reusable library for portable agent workflows.
//!
//! Domain contracts and policy belong here. Orchestrator-specific execution
//! belongs behind the [`contracts::EffectExecutor`] boundary. Durable task
//! ownership lives in [`state::HouseStore`], in house-scoped runtime storage
//! chosen by the caller. The library grants no authority by itself: effects
//! need task authority delegated from explicit house grants.
//!
//! ```
//! use kitchen::{HouseId, TaskId};
//! # fn main() -> Result<(), kitchen::IdentifierError> {
//! let house = HouseId::new("home")?;
//! let task: TaskId = "task-42".parse()?;
//! assert_eq!(house.as_str(), "home");
//! assert_eq!(task.as_str(), "task-42");
//! # Ok(())
//! # }
//! ```

pub mod adapters;
pub mod adoption;
pub mod contracts;
mod error;
pub mod house;
mod id;
pub mod integrations;
pub mod scaffold;
pub mod scheduling;
pub mod state;
pub mod trust;
pub mod workflows;

pub use error::{Error, ErrorClass, Result};
pub use id::{
    BackendId, ConsumerId, CredentialId, EffectName, HolderId, HouseId, IdentifierError, TaskId,
    WorkflowId,
};
