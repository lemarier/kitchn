//! Agent-selection failures.

use std::fmt;

use crate::{ErrorClass, scheduling::AgentFamily};

use super::SelectionGap;

/// The kind of selection value a validation error refers to. Rejected input is
/// never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SelectionValue {
    /// A provider model identifier.
    Model,
    /// A reasoning effort level.
    Effort,
    /// A house-defined work type.
    WorkType,
    /// A house-defined task group.
    TaskGroup,
}

impl fmt::Display for SelectionValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Model => "model",
            Self::Effort => "effort",
            Self::WorkType => "work type",
            Self::TaskGroup => "task group",
        })
    }
}

/// Why an agent policy or selection was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SelectionError {
    /// A value failed validation.
    #[error("invalid {kind}")]
    InvalidValue {
        /// Which value.
        kind: SelectionValue,
    },
    /// The policy has more rules than [`super::MAX_SELECTION_RULES`].
    #[error("agent policy has too many rules")]
    TooManyRules,
    /// Two rules match exactly the same scope, role, and work type.
    #[error("agent policy has two rules for the same scope, role, and work type")]
    DuplicateRule,
    /// A rule names both a task group and a repository.
    #[error("an agent policy rule may name a task group or a repository, not both")]
    AmbiguousScope,
    /// A house-scope rule matches every role and work type; that is the default.
    #[error("an unscoped agent policy rule must name a role or work type; use the default")]
    UnscopedCatchAll,
    /// A rule names a repository outside the house allowlist.
    #[error("an agent policy rule names a repository this house does not serve")]
    RepositoryNotServed,
    /// The backend cannot launch exactly this selection. Nothing was launched.
    #[error("the backend cannot provide agent selection {}: {}", .agent.as_str(), gap_list(.gaps))]
    Unsupported {
        /// The selected agent family.
        agent: AgentFamily,
        /// Every missing piece, never empty.
        gaps: Vec<SelectionGap>,
    },
}

fn gap_list(gaps: &[SelectionGap]) -> String {
    gaps.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl SelectionError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidValue { .. }
            | Self::TooManyRules
            | Self::DuplicateRule
            | Self::AmbiguousScope
            | Self::UnscopedCatchAll => ErrorClass::InvalidInput,
            Self::RepositoryNotServed | Self::Unsupported { .. } => ErrorClass::Refused,
        }
    }
}
