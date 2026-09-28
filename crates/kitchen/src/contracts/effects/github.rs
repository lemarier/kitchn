//! Typed GitHub effect payloads and atomic per-task admission.
use crate::contracts::{
    BranchName, Capability, CommitId, ContractError, EffectContext, ExecutorKind, GrantScope,
    Permission, Repository, Text, ValueKind,
};
use serde::{Deserialize, Serialize};
fn invalid() -> ContractError {
    ContractError::InvalidValue {
        kind: ValueKind::Text,
    }
}
/// An issue or pull request number, excluding zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct IssueNumber(u64);
impl IssueNumber {
    /// Validate a provider number.
    ///
    /// # Errors
    /// Zero and values outside signed provider integer range are refused.
    pub fn new(value: u64) -> Result<Self, ContractError> {
        if value == 0 || value > i64::MAX as u64 {
            return Err(invalid());
        }
        Ok(Self(value))
    }
    /// Numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
impl TryFrom<u64> for IssueNumber {
    type Error = ContractError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<IssueNumber> for u64 {
    fn from(value: IssueNumber) -> Self {
        value.0
    }
}

/// Maximum distinct logical effects for one task, including uncertain intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct PostingBudget(u32);

impl PostingBudget {
    /// Zero explicitly disables posting; at most 100 logical effects per task.
    ///
    /// # Errors
    /// Returns an invalid-input error above the fixed ceiling.
    pub fn new(limit: u32) -> Result<Self, ContractError> {
        if limit > 100 {
            return Err(invalid());
        }
        Ok(Self(limit))
    }

    /// Configured maximum submissions.
    #[must_use]
    pub const fn limit(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for PostingBudget {
    type Error = ContractError;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<PostingBudget> for u32 {
    fn from(value: PostingBudget) -> Self {
        value.limit()
    }
}
/// Desired workflow label. Existing labels are never modified by setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelDefinition {
    /// Label name (1–50 bytes, no controls).
    pub name: String,
    /// Six hexadecimal RGB digits.
    pub color: String,
    /// Description, at most 100 bytes.
    pub description: String,
}
impl LabelDefinition {
    /// Check the provider's bounded fields before persistence or execution.
    ///
    /// # Errors
    /// Rejects invalid names, colors, and descriptions.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_label_name(&self.name)?;
        if self.color.len() != 6
            || !self.color.bytes().all(|b| b.is_ascii_hexdigit())
            || self.description.len() > 100
            || self.description.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        Ok(())
    }
}
/// One typed GitHub mutation; no arbitrary endpoint or shell command is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubMutation {
    /// Destination selected from the house allowlist.
    pub repository: Repository,
    /// The permitted operation.
    pub action: GitHubAction,
}
/// Closed mutation set used by workflow setup and issue coordination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum GitHubAction {
    /// Close one issue with an explicit provider reason.
    CloseIssue {
        /// Destination repository; must agree with the enclosing mutation.
        repository: Repository,
        /// Destination issue.
        number: IssueNumber,
        /// Reason recorded by GitHub.
        reason: CloseReason,
    },
    /// Merge only the named head using GitHub's squash method.
    MergePullRequest {
        /// Destination pull request.
        number: IssueNumber,
        /// Exact head approved for merging.
        expected_head: CommitId,
        /// Base branch approved for the merge; a retargeted PR is refused.
        expected_base: BranchName,
        /// Fixed merge method.
        method: MergeMethod,
    },
    /// Add one comment, identified by its persisted idempotency marker.
    PostComment {
        /// Destination issue/PR.
        issue: IssueNumber,
        /// Comment text.
        body: Text,
    },
    /// Add/remove exactly one issue label, preserving other labels.
    SetLabel {
        /// Destination issue/PR.
        issue: IssueNumber,
        /// Label name.
        label: String,
        /// Desired membership.
        present: bool,
    },
    /// Create an issue with an idempotency marker.
    CreateIssue {
        /// Issue title.
        title: Text,
        /// Issue body.
        body: Text,
    },
    /// Link two existing issues in this repository; never replace a parent.
    LinkSubIssue {
        /// Parent issue.
        parent: IssueNumber,
        /// Child issue.
        child: IssueNumber,
    },
    /// Record an existing issue as a blocking dependency in this repository.
    LinkDependency {
        /// Blocked issue.
        issue: IssueNumber,
        /// Blocking issue.
        blocker: IssueNumber,
    },
    /// Create a missing label, never modify an existing definition.
    CreateLabel {
        /// Desired label.
        label: LabelDefinition,
    },
}
/// GitHub's issue closure reasons. Duplicate retains the referenced issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "of", rename_all = "kebab-case")]
pub enum CloseReason {
    /// Work completed.
    Completed,
    /// Work will not be planned.
    NotPlanned,
    /// Duplicate of an existing issue.
    Duplicate(IssueNumber),
}
/// Merge strategy; this integration permits squash only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeMethod {
    /// Squash the pull request.
    Squash,
}
impl GitHubMutation {
    /// Validate bounded content and relationships.
    ///
    /// # Errors
    /// Rejects self-links, malformed labels, and oversized titles/bodies.
    pub fn validate(&self) -> Result<(), ContractError> {
        match &self.action {
            GitHubAction::CloseIssue { repository, .. } if repository != &self.repository => {
                Err(invalid())
            }
            GitHubAction::CloseIssue {
                number,
                reason: CloseReason::Duplicate(of),
                ..
            } if number == of => Err(invalid()),
            GitHubAction::CloseIssue { .. } => Ok(()),
            GitHubAction::PostComment { body, .. } if body.as_str().len() > 60 * 1024 => {
                Err(invalid())
            }
            GitHubAction::MergePullRequest { .. } | GitHubAction::PostComment { .. } => Ok(()),
            GitHubAction::SetLabel { label, .. } => validate_label_name(label),
            GitHubAction::CreateIssue { title, body } => {
                if title.as_str().len() > 256
                    || title.as_str().trim() != title.as_str()
                    || title.as_str().chars().any(char::is_control)
                    || body.as_str().len() > 60 * 1024
                {
                    Err(invalid())
                } else {
                    Ok(())
                }
            }
            GitHubAction::LinkSubIssue { parent, child } if parent == child => Err(invalid()),
            GitHubAction::LinkDependency { issue, blocker } if issue == blocker => Err(invalid()),
            GitHubAction::LinkSubIssue { .. } | GitHubAction::LinkDependency { .. } => Ok(()),
            GitHubAction::CreateLabel { label } => label.validate(),
        }
    }
}
fn validate_label_name(name: &str) -> Result<(), ContractError> {
    if name.is_empty() || name.len() > 50 || name.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(())
}

/// Persisted GitHub effect with the selected house's logical posting ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubEffect {
    /// Authenticated requester persisted with intent.
    pub requester: crate::contracts::ExternalRef,
    /// Typed destination and mutation.
    pub mutation: GitHubMutation,
    /// Maximum logical forge effects for this task; executor also enforces current house policy.
    pub posting_budget: PostingBudget,
}
impl GitHubEffect {
    /// Required provider capability.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        Capability::ForgeMutation
    }
    /// Exact action permission.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self.mutation.action {
            GitHubAction::CloseIssue { .. } => Permission::CloseIssue,
            GitHubAction::MergePullRequest { .. } => Permission::Merge,
            GitHubAction::PostComment { .. } => Permission::PostComment,
            GitHubAction::SetLabel { .. } | GitHubAction::CreateLabel { .. } => {
                Permission::EditLabels
            }
            GitHubAction::CreateIssue { .. } => Permission::CreateIssue,
            GitHubAction::LinkSubIssue { .. } | GitHubAction::LinkDependency { .. } => {
                Permission::EditIssueRelationships
            }
        }
    }
    /// Exact repository destination used for authority checks.
    #[must_use]
    pub fn scope(&self) -> GrantScope {
        GrantScope::Repository(self.mutation.repository.clone())
    }
    /// Validate the mutation on every submission, including same-key retries.
    ///
    /// # Errors
    /// Refuses malformed action payloads.
    pub fn check(&self, _context: &EffectContext<'_>) -> Result<(), ContractError> {
        self.mutation.validate()?;
        Ok(())
    }
    /// Validate input and atomically reserve one logical posting slot.
    ///
    /// # Errors
    /// Refuses invalid payloads and exhausted task budgets.
    pub fn admit(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        if context.submitted.for_executor(ExecutorKind::GitHub) >= self.posting_budget.limit() {
            return Err(ContractError::EffectBudgetExhausted {
                executor: ExecutorKind::GitHub,
                limit: self.posting_budget.limit(),
            });
        }
        Ok(())
    }
}
