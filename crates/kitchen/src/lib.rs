//! Kitchen's reusable library for portable agent workflows.
//!
//! Domain contracts and policy belong here. Orchestrator-specific execution
//! belongs behind the [`contracts::ExecutionBackend`] boundary. Durable task
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

pub mod contracts;
mod error;
mod id;
pub mod state;

pub use error::{Error, ErrorClass, Result};
pub use id::{BackendId, ConsumerId, EffectName, HolderId, HouseId, IdentifierError, TaskId};
