//! Bounded evidence collection; partial pages are never reported complete.

use super::{CredentialRef, HouseScope, IntegrationError, evidence::*};
use crate::{
    HouseId,
    contracts::{CommitId, Repository},
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

/// A read request constructed by the scoped client, never by a workflow shell.
#[derive(Debug, Clone)]
pub struct ReadRequest {
    pub(crate) endpoint: String,
    pub(crate) graphql: Option<Value>,
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
}

/// A GitHub client selected for exactly one house and credential namespace.
pub struct GitHubClient<T> {
    scope: HouseScope,
    transport: T,
    limits: ReadLimits,
}
impl<T: GitHubReadTransport> GitHubClient<T> {
    /// Select the house before any I/O.
    pub const fn new(scope: HouseScope, transport: T, limits: ReadLimits) -> Self {
        Self {
            scope,
            transport,
            limits,
        }
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
        self.single(house, repo, format!("issues/{}", number.get()))
    }
    /// Read all issue pages (pull requests are excluded).
    pub fn issues(&self, house: &HouseId, repo: &Repository) -> Observation<Vec<Issue>> {
        self.pages(house, repo, "issues?state=all", None, true)
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
    /// Read the exact current head and mergeability.
    pub fn pull_request(
        &self,
        house: &HouseId,
        repo: &Repository,
        number: IssueNumber,
    ) -> Observation<PullRequest> {
        self.single(house, repo, format!("pulls/{}", number.get()))
    }
    /// Read check runs at the explicitly selected commit.
    pub fn checks(
        &self,
        house: &HouseId,
        repo: &Repository,
        head: &CommitId,
    ) -> Observation<Vec<CheckRun>> {
        self.pages(
            house,
            repo,
            &format!("commits/{head}/check-runs"),
            Some("check_runs"),
            false,
        )
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
        observe((|| {
            self.scope.authorize_read(house, repo)?;
            self.fetch(
                &ReadRequest {
                    endpoint: format!("repos/{repo}/{endpoint}"),
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
        observe((|| {
            self.scope.authorize_read(house, repo)?;
            let started = Instant::now();
            let mut remaining = self.limits.bytes;
            let mut result = Vec::new();
            for page in 1..=self.limits.pages {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let request = ReadRequest {
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
fn observe<T>(result: Result<T, IntegrationError>) -> Observation<T> {
    match result {
        Ok(value) => Observation::Known(value),
        Err(IntegrationError::Unknown) => Observation::Unknown,
        Err(error) => Observation::Unavailable(error),
    }
}
