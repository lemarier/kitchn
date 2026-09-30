//! Bounded evidence collection; partial pages are never reported complete.

use super::{CredentialRef, HouseScope, IntegrationError, IssueNumber, evidence::*};
use crate::{
    HouseId,
    contracts::{BranchName, CommitId, Repository, Timestamp},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Finite budget for a complete evidence query, including every page.
#[derive(Debug, Clone, Copy)]
pub struct ReadLimits {
    timeout: Duration,
    pages: u16,
    bytes: usize,
}
impl ReadLimits {
    /// Bound a query to at most 60 seconds, 100 pages, and 8 MiB.
    ///
    /// # Errors
    /// Zero or excessive bounds are rejected.
    pub fn new(timeout: Duration, pages: u16, bytes: usize) -> Result<Self, IntegrationError> {
        if timeout.is_zero()
            || timeout > Duration::from_secs(60)
            || pages == 0
            || pages > 100
            || bytes == 0
            || bytes > 8 * 1024 * 1024
        {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(Self {
            timeout,
            pages,
            bytes,
        })
    }
    /// Maximum pages per query.
    #[must_use]
    pub const fn pages(self) -> u16 {
        self.pages
    }
    /// Query timeout.
    #[must_use]
    pub const fn timeout(self) -> Duration {
        self.timeout
    }
    /// Total response byte ceiling.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }
}
impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            pages: 20,
            bytes: 2 * 1024 * 1024,
        }
    }
}

/// Commits [`GitHubClient::pull_request_commits`] reads for one pull
/// request. The forge lists at most 250 and drops the rest without saying
/// so; this bound is below that, so a dropped commit is never read as absent.
pub const MAX_PULL_REQUEST_COMMITS: usize = 100;

/// A read request constructed by the scoped client, never by a workflow shell.
#[derive(Debug, Clone)]
pub struct ReadRequest {
    pub(crate) endpoint: String,
    pub(crate) graphql: Option<Value>,
    /// The repository-bound token scope for a GitHub App read or effect.
    pub(crate) access: Option<super::TokenScope>,
}
impl ReadRequest {
    /// Relative GitHub API endpoint, with no caller-selected host.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    /// GraphQL variables and query, when reading threads.
    #[must_use]
    pub const fn graphql(&self) -> Option<&Value> {
        self.graphql.as_ref()
    }
    /// The token scope of this read.
    #[must_use]
    pub const fn access(&self) -> Option<&super::TokenScope> {
        self.access.as_ref()
    }
}

/// Authenticated read-only transport. Implementations must honor both bounds.
/// Credential resolution must match the entire reference, including house and requester.
pub trait GitHubReadTransport {
    /// Execute one page; return only its JSON response body.
    ///
    /// # Errors
    /// Authentication, network, timeout, and output-limit failures remain errors.
    fn read(
        &self,
        credential: &CredentialRef,
        request: &ReadRequest,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, IntegrationError>;

    /// Whether the credential reaches `repository` at all, asked once before
    /// any effect. Only a GitHub App can be missing from a repository, so the
    /// default answers yes without I/O; [`super::GhCli`] asks GitHub for an app.
    ///
    /// # Errors
    /// Credential, transport, and response failures.
    fn installed(
        &self,
        _credential: &CredentialRef,
        _repository: &crate::contracts::Repository,
        _timeout: Duration,
    ) -> Result<super::Installed, IntegrationError> {
        Ok(super::Installed::Yes)
    }
}

/// A GitHub client selected for exactly one house and credential namespace.
pub struct GitHubClient<T> {
    scope: HouseScope,
    transport: T,
    limits: ReadLimits,
    read_access: Option<super::TokenScope>,
}
impl<T: GitHubReadTransport> GitHubClient<T> {
    /// Select the house before any I/O.
    pub const fn new(scope: HouseScope, transport: T, limits: ReadLimits) -> Self {
        Self {
            scope,
            transport,
            limits,
            read_access: None,
        }
    }
    /// Scope this client's reads to one repository and App token permission set.
    /// Token credentials ignore this scope; App credentials require it.
    ///
    /// # Errors
    /// Refuses a repository outside the selected house.
    pub fn with_read_access(mut self, access: super::TokenScope) -> Result<Self, IntegrationError> {
        self.scope
            .authorize_read(self.scope.house(), access.repository())?;
        self.read_access = Some(access);
        Ok(self)
    }
    fn read_access(
        &self,
        repository: &Repository,
        extra: Option<super::AppPermission>,
    ) -> Option<super::TokenScope> {
        self.read_access
            .as_ref()
            .filter(|access| access.repository() == repository && extra.is_none())
            .cloned()
            .or_else(|| Some(super::TokenScope::for_read(repository, extra)))
    }
    /// House policy used for every request.
    #[must_use]
    pub const fn scope(&self) -> &HouseScope {
        &self.scope
    }
    /// Inspect the injected boundary, including fake call evidence.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Read one issue.
    pub fn issue(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Issue> {
        match self.single::<Issue>(house, repo, format!("issues/{}", number.get())) {
            Observation::Known(issue) if &issue.repository != repo || issue.number != number => {
                Observation::Unknown
            }
            result => result,
        }
    }
    /// Read issue body, author, lifecycle, and timestamps.
    pub fn issue_detail(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<IssueDetail> {
        match self.single::<IssueDetail>(house, repo, format!("issues/{}", number.get())) {
            Observation::Known(issue) if issue.number != number => Observation::Unknown,
            result => result,
        }
    }
    /// Read all issue comments within the configured page, byte, and time bounds.
    pub fn comments(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<IssueComment>> {
        self.pages(
            house,
            repo,
            &format!("issues/{}/comments", number.get()),
            None,
            false,
        )
    }
    /// Read issue timeline events, including label changes and cross-references.
    pub fn timeline(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<TimelineEvent>> {
        self.pages(
            house,
            repo,
            &format!("issues/{}/timeline", number.get()),
            None,
            false,
        )
    }
    /// Resolve referenced PRs, with a total deadline and at most 100 distinct links.
    pub fn linked_pull_requests(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<LinkedPullRequest>> {
        observe((|| {
            let started = Instant::now();
            let timeline = match self.timeline(house, repo, number) {
                Observation::Known(v) => v,
                Observation::Unknown => return Err(IntegrationError::Unknown),
                Observation::Unavailable(error) => return Err(error),
            };
            let mut refs = std::collections::BTreeSet::new();
            for event in timeline {
                if let Some(source) = event.source.and_then(|v| v.issue)
                    && source.pull_request.is_some()
                {
                    if self
                        .scope
                        .authorize_read(house, &source.repository_url)
                        .is_err()
                    {
                        continue;
                    }
                    refs.insert((source.repository_url, source.number.get()));
                    if refs.len() > 100 {
                        return Err(IntegrationError::LimitExceeded);
                    }
                }
            }
            let mut remaining = self.limits.bytes;
            let mut cursor: Option<String> = None;
            let mut closing_complete = false;
            for _ in 0..self.limits.pages {
                let request = ReadRequest {
                    access: self.read_access(repo, None),
                    endpoint: "graphql".into(),
                    graphql: Some(json!({
                        "query":"query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){issue(number:$number){closedByPullRequestsReferences(first:100,after:$cursor){nodes{number repository{nameWithOwner}} pageInfo{hasNextPage endCursor}}}}}",
                        "variables":{"owner":repo.owner(),"name":repo.name(),"number":number.get(),"cursor":cursor}
                    })),
                };
                let value: Value = self.fetch(&request, started, &mut remaining)?;
                if value.get("errors").is_some() {
                    return Err(IntegrationError::Unknown);
                }
                let connection = value
                    .pointer("/data/repository/issue/closedByPullRequestsReferences")
                    .ok_or(IntegrationError::Unknown)?;
                let nodes = connection
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(IntegrationError::Unknown)?;
                if nodes.len() > 100 {
                    return Err(IntegrationError::LimitExceeded);
                }
                for node in nodes {
                    let name = node
                        .pointer("/repository/nameWithOwner")
                        .and_then(Value::as_str)
                        .ok_or(IntegrationError::Unknown)?;
                    let linked_repo =
                        Repository::new(name).map_err(|_| IntegrationError::Unknown)?;
                    // Like timeline references, a closing PR outside the house scope is
                    // not evidence for this house and must not hide the in-scope ones.
                    if self.scope.authorize_read(house, &linked_repo).is_err() {
                        continue;
                    }
                    let number = node
                        .get("number")
                        .and_then(Value::as_u64)
                        .ok_or(IntegrationError::Unknown)?;
                    let linked_number =
                        IssueNumber::new(number).map_err(|_| IntegrationError::Unknown)?;
                    refs.insert((linked_repo, linked_number.get()));
                    if refs.len() > 100 {
                        return Err(IntegrationError::LimitExceeded);
                    }
                }
                let next = connection
                    .pointer("/pageInfo/hasNextPage")
                    .and_then(Value::as_bool)
                    .ok_or(IntegrationError::Unknown)?;
                if !next {
                    closing_complete = true;
                    break;
                }
                let next_cursor = connection
                    .pointer("/pageInfo/endCursor")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty() && v.len() <= 1024)
                    .ok_or(IntegrationError::Unknown)?;
                if cursor.as_deref() == Some(next_cursor) {
                    return Err(IntegrationError::Unknown);
                }
                cursor = Some(next_cursor.into());
            }
            if !closing_complete {
                return Err(IntegrationError::LimitExceeded);
            }
            let mut result = Vec::new();
            for (repository, linked_number) in refs {
                let request = ReadRequest {
                    access: self.read_access(&repository, None),
                    endpoint: format!("repos/{repository}/pulls/{linked_number}"),
                    graphql: None,
                };
                let pr: PullRequest = self.fetch(&request, started, &mut remaining)?;
                if pr.number.get() != linked_number {
                    return Err(IntegrationError::Unknown);
                }
                result.push(LinkedPullRequest {
                    repository,
                    pull_request: pr,
                });
            }
            Ok(result)
        })())
    }
    /// Read all issue pages (pull requests are excluded).
    pub fn issues(&self, house: &HouseId, repo: &Repository) -> Observation<Vec<Issue>> {
        self.issues_filtered(house, repo, None, None)
    }
    /// Read issue inventory with optional state and updated-since filters.
    pub fn issues_filtered(
        &self,
        house: &HouseId,
        repo: &Repository,
        state: Option<IssueState>,
        since: Option<Timestamp>,
    ) -> Observation<Vec<Issue>> {
        let mut endpoint = format!(
            "issues?state={}",
            match state {
                Some(IssueState::Open) => "open",
                Some(IssueState::Closed) => "closed",
                Some(IssueState::Unknown) =>
                    return Observation::Unavailable(IntegrationError::InvalidInput),
                None => "all",
            }
        );
        if let Some(since) = since {
            let nanos = i128::from(since.as_unix_millis()) * 1_000_000;
            let Ok(date) = time::OffsetDateTime::from_unix_timestamp_nanos(nanos) else {
                return Observation::Unavailable(IntegrationError::InvalidInput);
            };
            let Ok(value) = date.format(&time::format_description::well_known::Rfc3339) else {
                return Observation::Unavailable(IntegrationError::InvalidInput);
            };
            endpoint.push_str("&since=");
            endpoint.push_str(&value);
        }
        match self.pages::<Issue>(house, repo, &endpoint, None, true) {
            Observation::Known(issues) if issues.iter().any(|issue| &issue.repository != repo) => {
                Observation::Unknown
            }
            result => result,
        }
    }
    /// Read explicit blocked-by relationships.
    pub fn dependencies(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<Issue>> {
        self.pages(
            house,
            repo,
            &format!("issues/{}/dependencies/blocked_by", number.get()),
            None,
            false,
        )
    }
    /// Read sub-issues.
    pub fn sub_issues(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<Issue>> {
        self.pages(
            house,
            repo,
            &format!("issues/{}/sub_issues", number.get()),
            None,
            false,
        )
    }
    /// List all open pull requests, refusing incomplete pagination.
    pub fn open_pull_requests(
        &self,
        house: &HouseId,
        repo: &Repository,
    ) -> Observation<Vec<OpenPullRequest>> {
        self.pages_with_permission(
            house,
            repo,
            "pulls?state=open",
            None,
            false,
            Some(super::AppPermission::PullRequests),
        )
    }

    /// Read the exact current head and mergeability.
    pub fn pull_request(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<PullRequest> {
        match self.single::<PullRequest>(house, repo, format!("pulls/{}", number.get())) {
            Observation::Known(pr) if pr.number != number => Observation::Unknown,
            result => result,
        }
    }
    /// Read a branch's current tip from its ref. A pull request's `base.sha`
    /// is the base recorded on the PR object and can lag the branch.
    pub fn branch_tip(
        &self,
        house: &HouseId,
        repo: &Repository,
        branch: &BranchName,
    ) -> Observation<CommitId> {
        let path = format!("branches/{}", encode_branch_path(branch));
        match self.single::<Branch>(house, repo, path) {
            Observation::Known(found) if found.name == branch.as_str() => {
                Observation::Known(found.commit.sha)
            }
            Observation::Known(_) => Observation::Unknown,
            Observation::Unknown => Observation::Unknown,
            Observation::Unavailable(error) => Observation::Unavailable(error),
        }
    }
    /// Read GraphQL mergeStateStatus and bind it to the exact selected head.
    pub fn merge_status(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
        head: &CommitId,
    ) -> Observation<MergeStatus> {
        observe((|| {
            self.scope.authorize_read(house, repo)?;
            let request = ReadRequest {
                access: self.read_access(repo, None),
                endpoint: "graphql".into(),
                graphql: Some(json!({
                    "query":"query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid mergeStateStatus}}}",
                    "variables":{"owner":repo.owner(),"name":repo.name(),"number":number.get()}
                })),
            };
            let value: Value =
                self.fetch(&request, Instant::now(), &mut self.limits.bytes.clone())?;
            if value.get("errors").is_some() {
                return Err(IntegrationError::Unknown);
            }
            let pr = value
                .pointer("/data/repository/pullRequest")
                .ok_or(IntegrationError::Unknown)?;
            let actual = pr
                .get("headRefOid")
                .and_then(Value::as_str)
                .ok_or(IntegrationError::Unknown)?;
            if actual != head.as_str() {
                return Err(IntegrationError::Unknown);
            }
            let status: MergeStatusValue = serde_json::from_value(
                pr.get("mergeStateStatus")
                    .cloned()
                    .ok_or(IntegrationError::Unknown)?,
            )
            .map_err(|_| IntegrationError::Unknown)?;
            Ok(MergeStatus {
                head: head.clone(),
                status,
            })
        })())
    }
    /// Read check runs at the explicitly selected commit.
    pub fn checks(
        &self,
        house: &HouseId,
        repo: &Repository,
        head: &CommitId,
    ) -> Observation<Vec<CheckRun>> {
        match self.pages_with_permission::<CheckRun>(
            house,
            repo,
            &format!("commits/{head}/check-runs"),
            Some("check_runs"),
            false,
            Some(super::AppPermission::Checks),
        ) {
            Observation::Known(checks) if checks.iter().any(|check| &check.head_sha != head) => {
                Observation::Unknown
            }
            result => result,
        }
    }
    /// Compare an exact base and head; a partial or mismatched response is unknown.
    pub fn compare(
        &self,
        house: &HouseId,
        repo: &Repository,
        base: &CommitId,
        head: &CommitId,
    ) -> Observation<Compare> {
        self.single(house, repo, format!("compare/{base}...{head}"))
    }
    /// Read commit statuses for one exact head, across every page.
    pub fn statuses(
        &self,
        house: &HouseId,
        repo: &Repository,
        head: &CommitId,
    ) -> Observation<Vec<CommitStatus>> {
        match self.pages_with_permission(
            house,
            repo,
            &format!("commits/{head}/statuses"),
            None,
            false,
            Some(super::AppPermission::Statuses),
        ) {
            Observation::Known(statuses)
                if statuses
                    .iter()
                    .any(|status: &CommitStatus| &status.sha != head) =>
            {
                Observation::Unknown
            }
            result => result,
        }
    }
    /// Read the repository's default branch.
    pub fn repository(&self, house: &HouseId, repo: &Repository) -> Observation<RepositoryInfo> {
        self.single(house, repo, String::new())
    }
    /// Read branch-protection requirements; inaccessible protection is unavailable.
    pub fn required_checks(
        &self,
        house: &HouseId,
        repo: &Repository,
        branch: &BranchName,
    ) -> Observation<RequiredChecks> {
        self.single_with_permission(
            house,
            repo,
            format!(
                "branches/{}/protection/required_status_checks",
                encode_branch_path(branch)
            ),
            Some(super::AppPermission::Administration),
        )
    }
    /// Read the head commit's provider timestamp.
    pub fn commit(
        &self,
        house: &HouseId,
        repo: &Repository,
        head: &CommitId,
    ) -> Observation<CommitInfo> {
        match self.single::<CommitInfo>(house, repo, format!("commits/{head}")) {
            Observation::Known(info) if &info.sha != head => Observation::Unknown,
            result => result,
        }
    }
    /// Read every commit of a pull request, which must include `head`. More
    /// than [`MAX_PULL_REQUEST_COMMITS`] is
    /// [`IntegrationError::LimitExceeded`], never a truncated list; a list
    /// without `head` is unknown.
    pub fn pull_request_commits(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
        head: &CommitId,
    ) -> Observation<Vec<PullRequestCommit>> {
        match self.pages::<PullRequestCommit>(
            house,
            repo,
            &format!("pulls/{}/commits", number.get()),
            None,
            false,
        ) {
            Observation::Known(commits) if commits.len() > MAX_PULL_REQUEST_COMMITS => {
                Observation::Unavailable(IntegrationError::LimitExceeded)
            }
            Observation::Known(commits) if !commits.iter().any(|commit| &commit.sha == head) => {
                Observation::Unknown
            }
            result => result,
        }
    }
    /// Read submitted reviews with their actual commit ids.
    pub fn reviews(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<Review>> {
        self.pages(
            house,
            repo,
            &format!("pulls/{}/reviews", number.get()),
            None,
            false,
        )
    }
    /// Read repository labels, including conflicting definitions.
    pub fn labels(&self, house: &HouseId, repo: &Repository) -> Observation<Vec<Label>> {
        self.pages(house, repo, "labels", None, false)
    }
    /// Read one user's permission, rejecting identity substitution.
    pub fn permission(
        &self,
        house: &HouseId,
        repo: &Repository,
        login: &str,
    ) -> Observation<PermissionEvidence> {
        if login.is_empty()
            || login.len() > 39
            || !login
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Observation::Unavailable(IntegrationError::InvalidInput);
        }
        match self.single::<PermissionEvidence>(
            house,
            repo,
            format!("collaborators/{login}/permission"),
        ) {
            Observation::Known(evidence) if evidence.user.login.eq_ignore_ascii_case(login) => {
                Observation::Known(evidence)
            }
            Observation::Known(_) | Observation::Unknown => Observation::Unknown,
            Observation::Unavailable(error) => Observation::Unavailable(error),
        }
    }
    /// Read resolution state for every review thread with bounded cursor pagination.
    pub fn threads(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<Vec<ReviewThread>> {
        observe((|| {
            self.scope.authorize_read(house, repo)?;
            let started = Instant::now();
            let mut remaining = self.limits.bytes;
            let mut cursor: Option<String> = None;
            let mut result = Vec::new();
            for _ in 0..self.limits.pages {
                let request = ReadRequest {
                    access: self.read_access(repo, None),
                    endpoint: "graphql".into(),
                    graphql: Some(json!({
                        "query": "query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100,after:$cursor){nodes{id isResolved isOutdated} pageInfo{hasNextPage endCursor}}}}}",
                        "variables": {"owner":repo.owner(),"name":repo.name(),"number":number.get(),"cursor":cursor}
                    })),
                };
                let value: Value = self.fetch(&request, started, &mut remaining)?;
                if value.get("errors").is_some() {
                    return Err(IntegrationError::Unknown);
                }
                let connection = value
                    .pointer("/data/repository/pullRequest/reviewThreads")
                    .ok_or(IntegrationError::Unknown)?;
                let nodes: Vec<ReviewThread> = serde_json::from_value(
                    connection
                        .get("nodes")
                        .cloned()
                        .ok_or(IntegrationError::Unknown)?,
                )
                .map_err(|_| IntegrationError::Unknown)?;
                if nodes.len() > 100 {
                    return Err(IntegrationError::LimitExceeded);
                }
                result.extend(nodes);
                let next = connection
                    .pointer("/pageInfo/hasNextPage")
                    .and_then(Value::as_bool)
                    .ok_or(IntegrationError::Unknown)?;
                if !next {
                    return Ok(result);
                }
                let next_cursor = connection
                    .pointer("/pageInfo/endCursor")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty() && v.len() <= 1024)
                    .ok_or(IntegrationError::Unknown)?;
                if cursor.as_deref() == Some(next_cursor) {
                    return Err(IntegrationError::Unknown);
                }
                cursor = Some(next_cursor.into());
            }
            Err(IntegrationError::LimitExceeded)
        })())
    }
    fn single<R: DeserializeOwned>(
        &self,
        house: &HouseId,
        repo: &Repository,
        endpoint: String,
    ) -> Observation<R> {
        self.single_with_permission(house, repo, endpoint, None)
    }
    fn single_with_permission<R: DeserializeOwned>(
        &self,
        house: &HouseId,
        repo: &Repository,
        endpoint: String,
        extra: Option<super::AppPermission>,
    ) -> Observation<R> {
        observe((|| {
            self.scope.authorize_read(house, repo)?;
            self.fetch(
                &ReadRequest {
                    access: self.read_access(repo, extra),
                    endpoint: if endpoint.is_empty() {
                        format!("repos/{repo}")
                    } else {
                        format!("repos/{repo}/{endpoint}")
                    },
                    graphql: None,
                },
                Instant::now(),
                &mut self.limits.bytes.clone(),
            )
        })())
    }
    fn pages<R: DeserializeOwned>(
        &self,
        house: &HouseId,
        repo: &Repository,
        endpoint: &str,
        field: Option<&str>,
        exclude_prs: bool,
    ) -> Observation<Vec<R>> {
        self.pages_with_permission(house, repo, endpoint, field, exclude_prs, None)
    }
    fn pages_with_permission<R: DeserializeOwned>(
        &self,
        house: &HouseId,
        repo: &Repository,
        endpoint: &str,
        field: Option<&str>,
        exclude_prs: bool,
        extra: Option<super::AppPermission>,
    ) -> Observation<Vec<R>> {
        observe((|| {
            self.scope.authorize_read(house, repo)?;
            let started = Instant::now();
            let mut remaining = self.limits.bytes;
            let mut result = Vec::new();
            for page in 1..=self.limits.pages {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let request = ReadRequest {
                    access: self.read_access(repo, extra),
                    endpoint: format!("repos/{repo}/{endpoint}{separator}per_page=100&page={page}"),
                    graphql: None,
                };
                let value: Value = self.fetch(&request, started, &mut remaining)?;
                let entries = match field {
                    Some(field) => value.get(field),
                    None => Some(&value),
                }
                .and_then(Value::as_array)
                .ok_or(IntegrationError::Unknown)?;
                if entries.len() > 100 {
                    return Err(IntegrationError::LimitExceeded);
                }
                for entry in entries {
                    if exclude_prs && entry.get("pull_request").is_some() {
                        continue;
                    }
                    result.push(
                        serde_json::from_value(entry.clone())
                            .map_err(|_| IntegrationError::Unknown)?,
                    );
                }
                if entries.len() < 100 {
                    return Ok(result);
                }
            }
            Err(IntegrationError::LimitExceeded)
        })())
    }
    fn fetch<R: DeserializeOwned>(
        &self,
        request: &ReadRequest,
        started: Instant,
        remaining: &mut usize,
    ) -> Result<R, IntegrationError> {
        let timeout = self
            .limits
            .timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(IntegrationError::Timeout)?;
        if *remaining == 0 {
            return Err(IntegrationError::LimitExceeded);
        }
        let bytes = self
            .transport
            .read(self.scope.credential(), request, timeout, *remaining)?;
        if started.elapsed() >= self.limits.timeout {
            return Err(IntegrationError::Timeout);
        }
        *remaining = remaining
            .checked_sub(bytes.len())
            .ok_or(IntegrationError::LimitExceeded)?;
        serde_json::from_slice(&bytes).map_err(|_| IntegrationError::Unknown)
    }
}
/// Percent-encode a branch name for a REST path. `/` separates the name's
/// components and stays literal; `#`, `%`, `&`, and other reserved bytes
/// would otherwise end or change the path.
pub(super) fn encode_branch_path(branch: &BranchName) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(branch.as_str().len());
    for byte in branch.as_str().bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
fn observe<T>(result: Result<T, IntegrationError>) -> Observation<T> {
    match result {
        Ok(value) => Observation::Known(value),
        Err(IntegrationError::Unknown) => Observation::Unknown,
        Err(error) => Observation::Unavailable(error),
    }
}
