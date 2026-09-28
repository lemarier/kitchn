//! Agent family, model, and effort selection per role and work type.
//!
//! A house configures an [`AgentPolicy`]. When a task is created, the
//! workflow resolves it once with [`AgentPolicy::resolve`] and stores the
//! [`ResolvedSelection`] in the task's specification. Every attempt launches
//! with that stored selection, and the state store refuses a launch that
//! differs from it, so retries keep it and a later policy change never
//! alters an active task. An owner who wants another selection creates a
//! replacement task with [`ResolvedSelection::owner`].
//!
//! Backends declare what each launch surface can honor as a
//! [`SelectionSupport`]; a selection they cannot provide is refused with the
//! gap named, never replaced by another model. The trust ledger attributes a
//! task to [`AgentSelection::attribution_model`].
//!
//! Selection is configuration. It grants no authority.

mod error;
mod model;
mod policy;
mod support;

pub use error::{SelectionError, SelectionValue};
pub use model::{
    AgentModel, AgentSelection, EffortLevel, MAX_MODEL_BYTES, MAX_NAME_BYTES, ResolvedSelection,
    SelectionSource, TaskGroup, WorkType,
};
pub use policy::{
    AgentPolicy, MAX_SELECTION_RULES, OfferedModels, RuleMatch, SelectionRequest, SelectionRule,
};
pub use support::{EffortSupport, SelectionGap, SelectionSupport};
