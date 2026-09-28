//! Human-decision effects. Owned by #7, which extends this payload.

use serde::{Deserialize, Serialize};

use crate::{
    HouseId, TaskId,
    contracts::{
        Capability, ContractError, EffectContext, EvidenceRevision, ExecutorKind, Permission, Text,
    },
};

/// Decision requests a task may have submitted, answered or not. #7 owns
/// this bound and may make it house policy.
pub const MAX_ASKS_PER_TASK: u32 = 3;

/// What a decision is about: the house, task, action, and evidence revision
/// it answers for. An answer resumes only this scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionBinding {
    /// The house.
    pub house: HouseId,
    /// The task.
    pub task: TaskId,
    /// The action the decision authorizes or refuses.
    pub action: Permission,
    /// The evidence revision the question was asked at.
    pub revision: EvidenceRevision,
}

/// A human decision request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RogerEffect {
    /// Ask one question.
    #[serde(rename_all = "camelCase")]
    Ask {
        /// The decision's scope.
        binding: DecisionBinding,
        /// The question.
        question: Text,
    },
}

impl RogerEffect {
    /// The executor capability this effect needs.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::Ask { .. } => Capability::AskHuman,
        }
    }

    /// The task permission this effect needs.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::Ask { .. } => Permission::AskHuman,
        }
    }

    /// Per-submission check: the binding must name this house, task, and
    /// current evidence revision, for a first submission and every retry.
    ///
    /// # Errors
    /// Returns [`ContractError::DecisionBindingMismatch`].
    pub fn check(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::Ask { binding, .. } => {
                if &binding.house != context.house
                    || &binding.task != context.task
                    || binding.revision != context.revision
                {
                    return Err(ContractError::DecisionBindingMismatch);
                }
                Ok(())
            }
        }
    }

    /// Admission hook for a new ask: the task may ask at most
    /// [`MAX_ASKS_PER_TASK`].
    ///
    /// # Errors
    /// Returns [`ContractError::EffectBudgetExhausted`].
    pub fn admit(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::Ask { .. } => {
                if context.submitted.for_executor(ExecutorKind::Roger) >= MAX_ASKS_PER_TASK {
                    return Err(ContractError::EffectBudgetExhausted {
                        executor: ExecutorKind::Roger,
                        limit: MAX_ASKS_PER_TASK,
                    });
                }
                Ok(())
            }
        }
    }
}
