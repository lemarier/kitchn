//! Typed human-decision payloads bound to durable task revisions.
use crate::{
    HouseId, TaskId,
    contracts::{
        Capability, CommitId, ContractError, EffectContext, EvidenceRevision, EvidenceSubject,
        ExecutorKind, ExternalRef, GrantScope, Permission, PostingBudget, Repository, Text,
        ValueKind,
    },
};
use serde::{Deserialize, Serialize};
fn invalid() -> ContractError {
    ContractError::InvalidValue {
        kind: ValueKind::Text,
    }
}
fn validate_id(id: &ExternalRef) -> Result<(), ContractError> {
    if id.as_str().len() != 26
        || !id
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase() && !b"ILOU".contains(&b))
    {
        return Err(invalid());
    }
    Ok(())
}
/// Hard ceiling on distinct human questions per task, including unresolved ones.
pub const MAX_ASKS_PER_TASK: u32 = 3;
/// Exactly one declared workflow owns each decision family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionOwner {
    /// Task coordinator.
    Task,
    /// Specification inbox.
    Spec,
    /// Exact-head gate.
    Merge,
}
impl DecisionOwner {
    /// Stable closed prefix; unknown legacy prefixes require an explicit migration.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Spec => "spec",
            Self::Merge => "merge",
        }
    }
}

/// Exact authority condition a human is asked to decide; never itself a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionBinding {
    /// Selected house.
    pub house: HouseId,
    /// Durable task.
    pub task: TaskId,
    /// Sole consumer of this decision family.
    pub owner: DecisionOwner,
    /// Allowed repository.
    pub repository: Repository,
    /// The action being considered.
    pub action: Permission,
    /// Exact destination within the repository.
    pub target: ExternalRef,
    /// Exact revision of the subject.
    pub revision: EvidenceRevision,
    /// The exact subject (head and base) the question is about; `None` when
    /// the task had no evidence subject yet. Roger requests require one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<EvidenceSubject>,
    /// Human-visible action constraints.
    pub limits: Text,
}
impl DecisionBinding {
    /// The head commit the human sees and a decision resumes at.
    ///
    /// # Errors
    /// Returns [`ContractError::DecisionBindingMismatch`] without a subject.
    pub fn head(&self) -> Result<&CommitId, ContractError> {
        self.subject
            .as_ref()
            .map(|subject| &subject.head)
            .ok_or(ContractError::DecisionBindingMismatch)
    }

    /// Stable name for this house/task/action, independent of process lifetime.
    ///
    /// # Errors
    /// Refuses oversized keys or action constraints.
    pub fn decision_key(&self) -> Result<String, ContractError> {
        let target = self.target.as_str();
        let task_target = target == format!("task:{}", self.task);
        let repository_target = target.split_once('#').is_some_and(|(prefix, number)| {
            (prefix == format!("pr:{}", self.repository)
                || prefix == format!("issue:{}", self.repository))
                && number
                    .parse::<u64>()
                    .is_ok_and(|value| value > 0 && value <= i64::MAX as u64)
        });
        if !task_target && !repository_target {
            return Err(ContractError::DecisionBindingMismatch);
        }
        if self.action == Permission::Merge
            && !target.starts_with(&format!("pr:{}#", self.repository))
        {
            return Err(ContractError::DecisionBindingMismatch);
        }
        let key = format!(
            "{}:{}:{}:{}:{}",
            self.owner.prefix(),
            self.house,
            self.task,
            self.action,
            self.target
        );
        if key.len() > 200
            || self.target.as_str().len() > 200
            || self.limits.as_str().len() > 500
            || self.limits.as_str().chars().any(char::is_control)
        {
            return Err(invalid());
        }
        Ok(key)
    }
}
/// Human decision kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskKind {
    /// Exact action approval with approve/reject options.
    Approval,
    /// Instructions only; answers cannot approve an action.
    Question,
}
/// Explicit operator-selected consequence level; never inferred downward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskRisk {
    /// Reversible routine work.
    Routine,
    /// Secrets, money, authorization, or data loss.
    Sensitive,
    /// Irreversible production, release, or equipment effects.
    Irreversible,
}
/// Payload to be persisted before asking a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RogerAsk {
    /// Exact decision scope.
    pub binding: DecisionBinding,
    /// Approval or question.
    pub kind: AskKind,
    /// Consequence level.
    pub risk: AskRisk,
    /// One-line title.
    pub title: Text,
    /// Sanitized question context.
    pub body: Text,
    /// Existing open request replaced after a revision change.
    pub supersedes: Option<ExternalRef>,
}
impl RogerAsk {
    /// Validate input before persistence and any effect.
    ///
    /// # Errors
    /// Refuses invalid binding, titles, body size, and Ask references.
    pub fn validate(&self) -> Result<(), ContractError> {
        self.binding.decision_key()?;
        if self.title.as_str().chars().count() > 120
            || self.title.as_str().chars().any(char::is_control)
            || self.body.as_str().len() > 16 * 1024
        {
            return Err(invalid());
        }
        if let Some(id) = &self.supersedes {
            validate_id(id)?;
        }
        Ok(())
    }
}

/// A persisted Roger request with an explicit per-task question ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RogerEffect {
    /// Authenticated requester persisted with intent.
    pub requester: crate::contracts::ExternalRef,
    /// Exact request persisted before calling Roger.
    pub ask: RogerAsk,
    /// Current selected house budget, also checked by the executor.
    pub posting_budget: PostingBudget,
}
impl RogerEffect {
    /// Required provider capability.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        Capability::AskHuman
    }
    /// Asking records a human decision and grants no execution authority.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        Permission::AskHuman
    }
    /// Recheck the exact decision binding on every submission: house, task,
    /// evidence revision, exact evidence subject (head and base), and
    /// repository scope.
    ///
    /// # Errors
    /// Refuses foreign or stale decisions and invalid payloads.
    pub fn check(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        self.ask.validate()?;
        let binding = &self.ask.binding;
        if &binding.house != context.house
            || &binding.task != context.task
            || binding.revision != context.revision
            || binding.subject.as_ref() != context.subject
            || !context
                .task_scope
                .covers(&GrantScope::Repository(binding.repository.clone()))
        {
            return Err(ContractError::DecisionBindingMismatch);
        }
        Ok(())
    }
    /// Reserve one logical ask within the task budget.
    ///
    /// # Errors
    /// Refuses an exhausted budget.
    pub fn admit(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        let limit = self.posting_budget.limit().min(MAX_ASKS_PER_TASK);
        if context.submitted.for_executor(ExecutorKind::Roger) >= limit {
            return Err(ContractError::EffectBudgetExhausted {
                executor: ExecutorKind::Roger,
                limit,
            });
        }
        Ok(())
    }
}
