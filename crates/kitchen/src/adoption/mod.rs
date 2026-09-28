//! Create-only adoption and immutable instruction installation.
mod installer;
pub use installer::*;
mod registry;
mod snapshot;
pub use registry::*;
pub use snapshot::*;
