//! Decision identity and strict response validation.

use crate::integrations::github::{HouseScope, IntegrationError};
use crate::{
    HouseId, TaskId,
    contracts::{CommitId, ExternalRef, Permission, Repository, Text},
};
use serde::{Deserialize, Serialize};

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
    pub revision: CommitId,
    /// Human-visible action constraints.
    pub limits: Text,
}
impl DecisionBinding {
    /// Stable name for this house/task/action, independent of process lifetime.
    ///
    /// # Errors
    /// Refuses oversized keys or action constraints.
    pub fn decision_key(&self) -> Result<String, IntegrationError> {
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
            return Err(IntegrationError::InvalidInput);
        }
        Ok(key)
    }
    /// Validate the selected house before reading an answer or submitting an Ask.
    ///
    /// # Errors
    /// Refuses cross-house/repository decisions and invalid bounded content.
    pub fn validate(&self, scope: &HouseScope) -> Result<(), IntegrationError> {
        scope.authorize_read(&self.house, &self.repository)?;
        self.decision_key()?;
        Ok(())
    }
}

/// An answer's state. Only an exact, consistent approved answer can approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionStatus {
    /// Waiting for a person; restart later with the same persisted Ask id.
    Unanswered,
    /// No answer arrived before expiry.
    Expired,
    /// Withdrawn or superseded; never consent.
    Closed,
    /// Explicit rejection.
    Rejected,
    /// Human instructions that grant no approval.
    Instructions(Option<Text>),
    /// Approval of precisely the supplied binding; other policy gates still apply.
    Approved,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ask {
    id: ExternalRef,
    requester: ExternalRef,
    repo: Repository,
    decision_key: String,
    kind: String,
    action: Option<Action>,
    resume: Resume,
    state: String,
    superseded_by: Option<String>,
    answer: Option<Answer>,
}
#[derive(Deserialize, PartialEq, Eq)]
struct Action {
    verb: String,
    target: ExternalRef,
    rev: CommitId,
    limits: Option<Text>,
}
#[derive(Deserialize)]
struct Resume {
    task: TaskId,
    rev: CommitId,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    decision: String,
    option_id: String,
    action: Option<Action>,
    input: Option<Text>,
    passkey: bool,
}

/// Parse a provider response and enforce its original persisted scope.
///
/// # Errors
/// Rejects malformed, cross-house/requester/task/action, stale, or ambiguous answers.
/// It does not consume the answer: a workflow must persist its continuation effect
/// using the same task and evidence revision before acting.
pub fn validate_answer(
    scope: &HouseScope,
    binding: &DecisionBinding,
    ask_id: &ExternalRef,
    bytes: &[u8],
) -> Result<DecisionStatus, IntegrationError> {
    binding.validate(scope)?;
    if bytes.len() > 128 * 1024 {
        return Err(IntegrationError::LimitExceeded);
    }
    let ask: Ask = serde_json::from_slice(bytes).map_err(|_| IntegrationError::Unknown)?;
    if &ask.id != ask_id
        || &ask.requester != scope.requester()
        || ask.repo != binding.repository
        || ask.decision_key != binding.decision_key()?
        || ask.resume.task != binding.task
    {
        return Err(IntegrationError::ScopeMismatch);
    }
    if ask.resume.rev != binding.revision {
        return Err(IntegrationError::StaleDecision);
    }
    let action = Action {
        verb: binding.action.as_str().into(),
        target: binding.target.clone(),
        rev: binding.revision.clone(),
        limits: Some(binding.limits.clone()),
    };
    if let Some(received) = &ask.action {
        if received.rev != binding.revision {
            return Err(IntegrationError::StaleDecision);
        }
        if received != &action {
            return Err(IntegrationError::ScopeMismatch);
        }
    } else if ask.kind == "approval" {
        return Err(IntegrationError::Unknown);
    }
    if ask.superseded_by.is_some() {
        return Ok(DecisionStatus::Closed);
    }
    match ask.state.as_str() {
        "open" if ask.answer.is_none() => Ok(DecisionStatus::Unanswered),
        "expired" if ask.answer.is_none() => Ok(DecisionStatus::Expired),
        "withdrawn" | "superseded" => Ok(DecisionStatus::Closed),
        "answered" => {
            let answer = ask.answer.ok_or(IntegrationError::Unknown)?;
            if ask.kind == "approval" && answer.action.as_ref() != Some(&action) {
                return Err(IntegrationError::ScopeMismatch);
            }
            match answer.decision.as_str() {
                "approve"
                    if ask.kind == "approval"
                        && answer.option_id == "approve"
                        && answer.passkey =>
                {
                    Ok(DecisionStatus::Approved)
                }
                "reject" if ask.kind == "approval" && answer.option_id == "reject" => {
                    Ok(DecisionStatus::Rejected)
                }
                "other" if matches!(ask.kind.as_str(), "approval" | "question") => {
                    Ok(DecisionStatus::Instructions(answer.input))
                }
                _ => Err(IntegrationError::Unknown),
            }
        }
        _ => Err(IntegrationError::Unknown),
    }
}
