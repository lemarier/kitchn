//! Kitchen's reusable library for portable agent workflows.
//!
//! Domain contracts and policy belong here. Orchestrator-specific execution
//! belongs behind explicit adapter boundaries. The workspace bootstrap does not
//! yet implement workflow execution or grant authority to external systems.
//!
//! ```
//! use kitchen::{HouseId, TaskId};
//! # fn main() -> Result<(), kitchen::Error> {
//! let house = HouseId::new("home")?;
//! let task: TaskId = "task-42".parse()?;
//! assert_eq!(house.as_str(), "home");
//! assert_eq!(task.as_str(), "task-42");
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod id;

pub use error::Error;
pub use id::{HouseId, TaskId};
