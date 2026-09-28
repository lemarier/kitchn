//! Typed mutation payloads; execution is connected to the durable core effect path.
use super::{IntegrationError, IssueNumber, Label};
use crate::contracts::{Permission, Repository, Text};
use serde::{Deserialize, Serialize};

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
    pub fn validate(&self) -> Result<(), IntegrationError> {
        validate_label_name(&self.name)?;
        if self.color.len() != 6
            || !self.color.bytes().all(|b| b.is_ascii_hexdigit())
            || self.description.len() > 100
            || self.description.chars().any(char::is_control)
        {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(())
    }
    /// Compare a fully fetched repository label inventory without mutating it.
    ///
    /// # Errors
    /// Rejects invalid input and ambiguous duplicate names.
    pub fn inspect(&self, labels: &[Label]) -> Result<LabelSetup, IntegrationError> {
        self.validate()?;
        let mut matches = labels
            .iter()
            .filter(|label| label.name.eq_ignore_ascii_case(&self.name));
        let found = matches.next();
        if matches.next().is_some() {
            return Err(IntegrationError::Unknown);
        }
        Ok(match found {
            None => LabelSetup::Missing,
            Some(label)
                if label.color.eq_ignore_ascii_case(&self.color)
                    && label.description.as_deref().unwrap_or("") == self.description =>
            {
                LabelSetup::Present
            }
            Some(_) => LabelSetup::Conflict,
        })
    }
}
/// Result of previewing one desired workflow label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSetup {
    /// Creation is needed, through persisted intent only.
    Missing,
    /// Already configured; no effect is needed.
    Present,
    /// Existing definition differs; report and leave it untouched.
    Conflict,
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
impl GitHubMutation {
    /// Validate bounded content and relationships.
    ///
    /// # Errors
    /// Rejects self-links, malformed labels, and oversized titles/bodies.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        match &self.action {
            GitHubAction::PostComment { body, .. } if body.as_str().len() > 60 * 1024 => {
                Err(IntegrationError::InvalidInput)
            }
            GitHubAction::PostComment { .. } => Ok(()),
            GitHubAction::SetLabel { label, .. } => validate_label_name(label),
            GitHubAction::CreateIssue { title, body } => {
                if title.as_str().len() > 256
                    || title.as_str().chars().any(char::is_control)
                    || body.as_str().len() > 60 * 1024
                {
                    Err(IntegrationError::InvalidInput)
                } else {
                    Ok(())
                }
            }
            GitHubAction::LinkSubIssue { parent, child } if parent == child => {
                Err(IntegrationError::InvalidInput)
            }
            GitHubAction::LinkDependency { issue, blocker } if issue == blocker => {
                Err(IntegrationError::InvalidInput)
            }
            GitHubAction::LinkSubIssue { .. } | GitHubAction::LinkDependency { .. } => Ok(()),
            GitHubAction::CreateLabel { label } => label.validate(),
        }
    }
    /// Permission required for comments/label changes already present in core.
    /// Other issue effects acquire dedicated permissions in the shared contract.
    #[must_use]
    pub const fn existing_permission(&self) -> Option<Permission> {
        match self.action {
            GitHubAction::PostComment { .. } => Some(Permission::PostComment),
            GitHubAction::SetLabel { .. } | GitHubAction::CreateLabel { .. } => {
                Some(Permission::EditLabels)
            }
            GitHubAction::CreateIssue { .. }
            | GitHubAction::LinkSubIssue { .. }
            | GitHubAction::LinkDependency { .. } => None,
        }
    }
}
fn validate_label_name(name: &str) -> Result<(), IntegrationError> {
    if name.is_empty() || name.len() > 50 || name.chars().any(char::is_control) {
        return Err(IntegrationError::InvalidInput);
    }
    Ok(())
}
