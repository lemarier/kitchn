//! Typed provider responses. Missing or unrecognized facts remain unknown.

use crate::contracts::{CommitId, IssueNumber, Repository, Text, Timestamp};
use serde::{Deserialize, Deserializer};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Parse a provider RFC 3339 timestamp once, at the response boundary.
/// Instants before the Unix epoch are refused.
fn parse_timestamp<E: serde::de::Error>(value: &str) -> Result<Timestamp, E> {
    let date = OffsetDateTime::parse(value, &Rfc3339).map_err(E::custom)?;
    let millis = u64::try_from(date.unix_timestamp_nanos() / 1_000_000).map_err(E::custom)?;
    Ok(Timestamp::from_unix_millis(millis))
}
fn timestamp<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
    parse_timestamp(&String::deserialize(deserializer)?)
}
fn optional_timestamp<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Timestamp>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|value| parse_timestamp(&value))
        .transpose()
}

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
    /// Source repository, retained for cross-repository dependency evidence.
    #[serde(rename = "repository_url", deserialize_with = "repository_url")]
    pub repository: Repository,
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
    /// Last update timestamp.
    #[serde(deserialize_with = "timestamp")]
    pub updated_at: Timestamp,
    /// Closure timestamp, if any.
    #[serde(default, deserialize_with = "optional_timestamp")]
    pub closed_at: Option<Timestamp>,
}
/// Issue detail for triage, including untrusted body text as data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueDetail {
    /// Repository-local number.
    pub number: IssueNumber,
    /// Current lifecycle.
    pub state: IssueState,
    /// Author identity.
    pub user: User,
    /// Untrusted issue body.
    pub body: Option<String>,
    /// Creation timestamp.
    #[serde(deserialize_with = "timestamp")]
    pub created_at: Timestamp,
    /// Last update timestamp.
    #[serde(deserialize_with = "timestamp")]
    pub updated_at: Timestamp,
    /// Closure timestamp, if any.
    #[serde(default, deserialize_with = "optional_timestamp")]
    pub closed_at: Option<Timestamp>,
}
/// One issue comment, returned through complete pagination.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueComment {
    /// Provider comment ID.
    pub id: u64,
    /// Author identity.
    pub user: User,
    /// Untrusted comment body.
    pub body: String,
    /// Creation timestamp.
    #[serde(deserialize_with = "timestamp")]
    pub created_at: Timestamp,
    /// Last update timestamp.
    #[serde(deserialize_with = "timestamp")]
    pub updated_at: Timestamp,
}
/// One issue timeline event; unknown kinds remain visible to callers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineEvent {
    /// Provider event kind.
    pub event: TimelineKind,
    /// Event timestamp, if supplied.
    #[serde(default, deserialize_with = "optional_timestamp")]
    pub created_at: Option<Timestamp>,
    /// Actor, if supplied.
    pub actor: Option<User>,
    /// Label involved in a label event.
    pub label: Option<TimelineLabel>,
    /// Cross-referenced source issue or pull request.
    pub source: Option<TimelineSource>,
}
/// Issue history event kinds used by triage; other kinds remain explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineKind {
    /// Label added.
    Labeled,
    /// Label removed.
    Unlabeled,
    /// Issue or PR cross-reference.
    CrossReferenced,
    /// Issue closed.
    Closed,
    /// Issue reopened.
    Reopened,
    /// Commit reference.
    Committed,
    /// Other reference.
    Referenced,
    /// Connected issue.
    Connected,
    /// Disconnected issue.
    Disconnected,
    /// Future or unsupported event kind.
    #[serde(other)]
    Unknown,
}
/// Label identity carried by a timeline change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineLabel {
    /// Label name at the event.
    pub name: String,
}
/// Source wrapper on cross-reference events.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineSource {
    /// Referencing issue or PR.
    pub issue: Option<TimelineIssue>,
}
/// Cross-referencing item; a pull_request field marks a PR.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineIssue {
    /// Item number in its repository.
    pub number: IssueNumber,
    /// Source repository.
    #[serde(deserialize_with = "repository_url")]
    pub repository_url: Repository,
    /// Present when the source is a pull request.
    pub pull_request: Option<serde_json::Value>,
}
/// PR referenced by an issue, with its live state and merge result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedPullRequest {
    /// Source repository.
    pub repository: Repository,
    /// Source pull request.
    pub pull_request: PullRequest,
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
    /// GitHub's detailed merge state, when supplied.
    #[serde(default)]
    pub mergeable_state: Option<MergeState>,
    /// Author identity.
    #[serde(default)]
    pub user: Option<User>,
    /// Author relationship to the repository.
    #[serde(default)]
    pub author_association: Option<AuthorAssociation>,
    /// Merge commit when merged.
    #[serde(default)]
    pub merge_commit_sha: Option<CommitId>,
}
/// Open pull-request list item used to select a checkout's PR.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenPullRequest {
    /// Repository-local PR number.
    pub number: IssueNumber,
    /// Current lifecycle.
    pub state: IssueState,
    /// Head commit, branch, and repository identity.
    pub head: GitRef,
    /// Base branch; list selection does not need its commit.
    pub base: PullRequestBaseRef,
    /// Author identity.
    pub user: Option<User>,
}

/// Base branch in a pull-request list item.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestBaseRef {
    /// Branch name.
    #[serde(rename = "ref")]
    pub name: String,
}
/// Whether the PR source is inside the selected repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadLocation {
    /// Source and destination repository match.
    SameRepository,
    /// Source is a different repository.
    Fork,
    /// Provider omitted head repository identity.
    Unknown,
}
impl PullRequest {
    /// Classify the source repository without guessing after a fork is deleted.
    #[must_use]
    pub fn head_location(&self, destination: &Repository) -> HeadLocation {
        head_location(&self.head, destination)
    }
}

impl OpenPullRequest {
    /// Classify the source repository without guessing after a fork is deleted.
    #[must_use]
    pub fn head_location(&self, destination: &Repository) -> HeadLocation {
        head_location(&self.head, destination)
    }
}

fn head_location(head: &GitRef, destination: &Repository) -> HeadLocation {
    match head.repo.as_ref() {
        Some(repo) if &repo.full_name == destination => HeadLocation::SameRepository,
        Some(_) => HeadLocation::Fork,
        None => HeadLocation::Unknown,
    }
}

#[cfg(test)]
mod pull_request_list_tests {
    use super::{HeadLocation, IssueState, OpenPullRequest};
    use crate::contracts::{CommitId, IssueNumber, Repository};

    #[test]
    fn github_list_item_without_merged_deserializes() -> Result<(), Box<dyn std::error::Error>> {
        let head = "a".repeat(40);
        let response = serde_json::json!([{
            "number": 12,
            "state": "open",
            "draft": false,
            "merged_at": null,
            "head": {"sha": head, "ref": "review-branch", "repo": {"full_name": "acme/app"}},
            "base": {"sha": "b".repeat(40), "ref": "main"},
            "user": {"login": "author"}
        }]);
        let pulls: Vec<OpenPullRequest> = serde_json::from_value(response)?;
        assert_eq!(pulls.len(), 1);
        let pull = &pulls[0];
        assert_eq!(pull.number, IssueNumber::new(12)?);
        assert_eq!(pull.state, IssueState::Open);
        assert_eq!(pull.head.sha, CommitId::new(&head)?);
        assert_eq!(pull.head.name, "review-branch");
        assert_eq!(pull.base.name, "main");
        assert_eq!(
            pull.user.as_ref().map(|user| user.login.as_str()),
            Some("author")
        );
        assert_eq!(
            pull.head_location(&Repository::new("acme/app")?),
            HeadLocation::SameRepository
        );
        Ok(())
    }
}

/// A named Git reference at an exact object id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitRef {
    /// Full object id.
    pub sha: CommitId,
    /// Branch name.
    #[serde(rename = "ref")]
    pub name: String,
    /// Head repository; `None` means deleted or unavailable.
    #[serde(default)]
    pub repo: Option<GitRepository>,
}
/// A branch and the commit its ref points at now.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Branch {
    /// Branch name.
    pub name: String,
    /// Current tip.
    pub commit: BranchCommit,
}
/// The commit a branch ref points at.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BranchCommit {
    /// Full object id.
    pub sha: CommitId,
}
/// REST mergeability detail; unknown provider values never grant readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Provider response field.
pub enum MergeState {
    /// Provider state.
    Clean,
    /// Provider state.
    Dirty,
    /// Provider state.
    Blocked,
    /// Provider state.
    Behind,
    /// Provider state.
    Unstable,
    /// Provider state.
    Draft,
    /// Provider state.
    Unknown,
    #[serde(other)]
    /// Provider state.
    Unsupported,
}
/// GraphQL merge state tied to the selected PR head.
#[derive(Debug, Clone, PartialEq, Eq)]
/// Provider response data.
pub struct MergeStatus {
    /// Provider response field.
    pub head: CommitId,
    /// Provider response field.
    pub status: MergeStatusValue,
}
/// GitHub GraphQL mergeStateStatus, with future values explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
/// Provider response field.
pub enum MergeStatusValue {
    /// Provider state.
    Behind,
    /// Provider state.
    Blocked,
    /// Provider state.
    Clean,
    /// Provider state.
    Dirty,
    /// Provider state.
    Draft,
    /// Provider state.
    HasHooks,
    /// Provider state.
    Unstable,
    /// Provider state.
    Unknown,
    #[serde(other)]
    /// Provider state.
    Unsupported,
}
/// Repository identity on a pull-request head or base.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct GitRepository {
    /// Canonical owner/name identity.
    pub full_name: Repository,
}
/// Author relationship, including unknown future values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
/// Provider response field.
pub enum AuthorAssociation {
    /// Provider state.
    Owner,
    /// Provider state.
    Member,
    /// Provider state.
    Collaborator,
    /// Provider state.
    Contributor,
    /// Provider state.
    FirstTimeContributor,
    /// Provider state.
    FirstTimer,
    /// Provider state.
    Mannequin,
    /// Provider state.
    None,
    #[serde(other)]
    /// Provider state.
    Unknown,
}
/// Complete compare result for a base and head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct Compare {
    /// Provider response field.
    pub behind_by: u64,
    /// Provider response field.
    pub ahead_by: u64,
}
/// Commit status at the selected revision.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct CommitStatus {
    /// Provider response field.
    pub context: String,
    /// Provider response field.
    pub state: StatusState,
    /// Provider response field.
    pub sha: CommitId,
}
/// Provider status, including unsupported values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Provider response field.
pub enum StatusState {
    /// Provider state.
    Pending,
    /// Provider state.
    Success,
    /// Provider state.
    Failure,
    /// Provider state.
    Error,
    #[serde(other)]
    /// Provider state.
    Unknown,
}
/// Repository default branch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct RepositoryInfo {
    /// Provider response field.
    pub default_branch: String,
}
/// Head commit timestamp, as supplied by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct CommitInfo {
    /// Provider response field.
    pub sha: CommitId,
    /// Provider response field.
    pub commit: CommitDetail,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct CommitDetail {
    /// Provider response field.
    pub committer: CommitPerson,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct CommitPerson {
    /// Provider response field.
    #[serde(deserialize_with = "timestamp")]
    pub date: Timestamp,
}
/// One commit of a pull request, with the forge accounts its author and
/// committer are linked to. The forge links a commit to an account by the
/// email in the commit, so a login here is the forge's attribution, not
/// proof of who pushed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestCommit {
    /// The commit.
    pub sha: CommitId,
    /// The author's forge login; `None` when the forge links no account.
    #[serde(default, deserialize_with = "linked_login")]
    pub author: Option<String>,
    /// The committer's forge login; `None` when the forge links no account.
    #[serde(default, deserialize_with = "linked_login")]
    pub committer: Option<String>,
}
/// The login of a commit's linked account. The forge answers `null` or an
/// empty object when it links none.
fn linked_login<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    struct Account {
        #[serde(default)]
        login: Option<String>,
    }
    Ok(Option::<Account>::deserialize(deserializer)?
        .and_then(|account| account.login)
        .filter(|login| !login.is_empty()))
}
/// Required status contexts from branch protection.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
/// Provider response data.
pub struct RequiredChecks {
    /// Provider response field.
    pub contexts: Vec<String>,
    /// Required check-run names, optionally bound to a GitHub App.
    #[serde(default)]
    pub checks: Vec<RequiredCheck>,
}
/// One protected check-run requirement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RequiredCheck {
    /// Check-run name.
    pub context: String,
    /// Required GitHub App identity, when branch protection specifies one.
    pub app_id: Option<i64>,
}
/// Whether every named required check exists at the selected head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Provider response field.
pub enum RequiredCheckPresence {
    /// Provider state.
    Present,
    /// Provider state.
    Missing,
    /// Provider state.
    Unknown,
}
impl RequiredChecks {
    /// Compare complete check and status inventories; duplicate required names are ambiguous.
    #[must_use]
    pub fn presence(
        &self,
        checks: &[CheckRun],
        statuses: &[CommitStatus],
        head: &CommitId,
    ) -> RequiredCheckPresence {
        if checks.iter().any(|v| &v.head_sha != head)
            || statuses.iter().any(|v| &v.sha != head)
            || self.contexts.iter().any(|name| name.is_empty())
            || self.checks.iter().any(|check| check.context.is_empty())
        {
            return RequiredCheckPresence::Unknown;
        }
        let mut contexts = std::collections::BTreeSet::new();
        if self.contexts.iter().any(|name| !contexts.insert(name)) {
            return RequiredCheckPresence::Unknown;
        }
        let mut check_names = std::collections::BTreeSet::new();
        if self
            .checks
            .iter()
            .any(|check| !check_names.insert(&check.context))
        {
            return RequiredCheckPresence::Unknown;
        }
        if self
            .checks
            .iter()
            .any(|check| check.app_id.is_some_and(|id| id < -1 || id == 0))
        {
            return RequiredCheckPresence::Unknown;
        }
        if self.contexts.iter().all(|name| {
            checks.iter().any(|v| &v.name == name) || statuses.iter().any(|v| &v.context == name)
        }) && self.checks.iter().all(|required| {
            checks.iter().any(|v| {
                v.name == required.context
                    && match required.app_id {
                        None | Some(-1) => true,
                        Some(id) => v.app.as_ref().is_some_and(|app| app.id == id),
                    }
            })
        }) {
            RequiredCheckPresence::Present
        } else {
            RequiredCheckPresence::Missing
        }
    }
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
    /// GitHub App identity when supplied by the provider.
    #[serde(default)]
    pub app: Option<CheckApp>,
}
/// App that submitted a check run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckApp {
    /// Provider app ID.
    pub id: i64,
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
    /// Review text is untrusted provider data; callers classify it without executing it.
    pub body: Option<String>,
    /// Provider submission timestamp, absent for pending reviews.
    #[serde(default, deserialize_with = "optional_timestamp")]
    pub submitted_at: Option<Timestamp>,
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

/// An unresolved review thread with the comment text needed by a writer.
/// Text and paths are untrusted forge data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowUpThread {
    /// GraphQL thread node id, used for a later reply and resolution.
    pub id: String,
    /// Explicit resolution state.
    pub is_resolved: bool,
    /// Whether the diff position became outdated.
    pub is_outdated: bool,
    /// File path named by the review.
    pub path: String,
    /// Current diff line, absent for an outdated thread.
    pub line: Option<u32>,
    /// Original diff line.
    pub original_line: Option<u32>,
    /// Comments on the thread.
    pub comments: FollowUpComments,
}

/// One bounded page of thread comments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowUpComments {
    /// Comment nodes.
    pub nodes: Vec<FollowUpComment>,
    /// Whether this page omitted comments.
    pub page_info: ThreadPageInfo,
}

/// One review comment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FollowUpComment {
    /// GraphQL comment node id.
    pub id: String,
    /// Untrusted review text.
    pub body: String,
    /// Author, including a bot when one wrote the comment.
    pub author: Option<User>,
}

/// Cursor evidence for a bounded GraphQL connection.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPageInfo {
    /// Whether more nodes exist.
    pub has_next_page: bool,
    /// Next cursor, when there is another page.
    pub end_cursor: Option<String>,
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

fn repository_url<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Repository, D::Error> {
    let value = String::deserialize(deserializer)?;
    let name = value
        .strip_prefix("https://api.github.com/repos/")
        .ok_or_else(|| serde::de::Error::custom("invalid repository reference"))?;
    Repository::new(name).map_err(serde::de::Error::custom)
}
