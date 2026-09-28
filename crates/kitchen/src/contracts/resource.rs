//! Backend resources that tasks create and cleanup must account for.

use serde::{Deserialize, Serialize};

use crate::{BackendId, contracts::ExternalRef};

/// The kind of a backend resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ResourceKind {
    /// A running or settled agent worker.
    Worker,
    /// A workspace checkout.
    Worktree,
    /// A terminal or console.
    Terminal,
    /// A branch created for the task.
    Branch,
    /// An installed schedule.
    Schedule,
}

/// A resource identified by its backend and an opaque backend handle.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRef {
    /// Resource kind.
    pub kind: ResourceKind,
    /// The backend that owns the handle.
    pub backend: BackendId,
    /// The backend-native handle, opaque to Kitchen.
    pub handle: ExternalRef,
}
