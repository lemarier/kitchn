//! Typed provider responses. Missing or unrecognized facts remain unknown.

use crate::contracts::{CommitId, Text};
use serde::Deserialize;

/// Whether a complete observation was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation<T> {
    /// A complete, parsed response.
    Known(T),
    /// The provider could not be queried within the bounds.
    Unavailable(super::IntegrationError),
    /// The provider returned incomplete, ambiguous, or malformed evidence.
    Unknown,
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
    pub fn new(value: u64) -> Result<Self, super::IntegrationError> {
        if value == 0 || value > i64::MAX as u64 {
            return Err(super::IntegrationError::InvalidInput);
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
    type Error = super::IntegrationError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<IssueNumber> for u64 {
    fn from(value: IssueNumber) -> Self {
        value.0
    }
}

/// Provider lifecycle, preserving future or unsupported values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    /// Open.
    Open,
    /// Closed.
    Closed,
    /// Unsupported value, never equivalent to closed.
    #[serde(other)]
    Unknown,
}

/// A label as returned by the forge.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Label {
    /// Label name.
    pub name: String,
    /// Six hexadecimal RGB digits.
    pub color: String,
    /// Optional description.
    pub description: Option<String>,
}

/// A forge user identity, without credentials.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    /// GitHub login.
    pub login: String,
}

/// Issue evidence; relationships are fetched separately with pagination.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Issue {
    /// Provider database id used for relationship endpoints.
    pub id: u64,
    /// Repository-local issue number.
    pub number: IssueNumber,
    /// Title; its debug representation is redacted.
    pub title: Text,
    /// Current lifecycle.
    pub state: IssueState,
    /// Assigned users.
    pub assignees: Vec<User>,
    /// Applied labels.
    pub labels: Vec<Label>,
}

/// Pull-request head and base, with explicit unknown mergeability.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequest {
    /// Repository-local PR number.
    pub number: IssueNumber,
    /// Current lifecycle.
    pub state: IssueState,
    /// Draft status.
    pub draft: bool,
    /// Whether the pull request merged.
    pub merged: bool,
    /// Exact head.
    pub head: GitRef,
    /// Exact base.
    pub base: GitRef,
    /// `None` means GitHub has not computed mergeability.
    pub mergeable: Option<bool>,
}

/// A named Git reference at an exact object id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitRef {
    /// Full object id.
    pub sha: CommitId,
    /// Branch name.
    #[serde(rename = "ref")]
    pub name: String,
}

/// A CI check. Unknown statuses and conclusions never pass.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckRun {
    /// Check name.
    pub name: String,
    /// Exact tested commit.
    pub head_sha: CommitId,
    /// Provider status.
    pub status: CheckStatus,
    /// Completed result, if known.
    pub conclusion: Option<CheckConclusion>,
}

/// Check execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Waiting.
    Queued,
    /// Running.
    InProgress,
    /// Finished; inspect conclusion.
    Completed,
    /// Unrecognized provider state.
    #[serde(other)]
    Unknown,
}

/// Check result; policy chooses which non-success outcomes are acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    /// Passed.
    Success,
    /// Failed.
    Failure,
    /// Cancelled.
    Cancelled,
    /// Timed out.
    TimedOut,
    /// No pass/fail result.
    Neutral,
    /// Skipped.
    Skipped,
    /// Approval/action needed.
    ActionRequired,
    /// Stale result.
    Stale,
    /// Unrecognized provider conclusion.
    #[serde(other)]
    Unknown,
}

/// Review state tied to its actual reviewed commit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Review {
    /// Review id.
    pub id: u64,
    /// Reviewer.
    pub user: User,
    /// Reviewed commit.
    pub commit_id: CommitId,
    /// Review outcome.
    pub state: ReviewState,
}

/// A review outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    /// Approved this commit.
    Approved,
    /// Changes requested.
    ChangesRequested,
    /// Comment only.
    Commented,
    /// Dismissed.
    Dismissed,
    /// Not submitted.
    Pending,
    /// Unrecognized state.
    #[serde(other)]
    Unknown,
}

/// Thread resolution evidence obtained from GraphQL, not inferred from comments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    /// GraphQL node id.
    pub id: String,
    /// Explicit provider resolution state.
    pub is_resolved: bool,
    /// Whether the thread concerns an earlier diff.
    pub is_outdated: bool,
}

/// Permission evidence for one named user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PermissionEvidence {
    /// The user whose permission was queried.
    pub user: User,
    /// Repository permission.
    pub permission: RepositoryPermission,
}

/// GitHub repository access, including explicit unsupported values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryPermission {
    /// No access.
    None,
    /// Read-only.
    Read,
    /// Triage.
    Triage,
    /// Write.
    Write,
    /// Maintain.
    Maintain,
    /// Admin.
    Admin,
    /// Legacy API read alias.
    Pull,
    /// Legacy API write alias.
    Push,
    /// Unsupported evidence, never approval.
    #[serde(other)]
    Unknown,
}
