//! Create-only adoption, registry-held repository bindings, and immutable
//! instruction installation. Nothing here writes to a repository working tree.
mod installer;
pub use installer::*;
mod registry;
mod remote;
mod snapshot;
pub use registry::*;
pub use remote::*;
pub use snapshot::*;
