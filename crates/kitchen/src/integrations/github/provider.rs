//! Provider-side mutation and reconciliation. Durable ownership lives in core.
use super::{
    CredentialRef, GhCli, GitHubAction, GitHubMutation, GitHubReadTransport, HouseScope,
    IntegrationError, Label, LabelSetup, ReadLimits, ReadRequest,
};
use crate::contracts::{
    EffectFailure, ExternalRef, IdempotencyKey, NotAppliedReason, Receipt, UncertainReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// A bounded mutation prepared from a typed payload after house validation.
#[derive(Clone)]
pub struct MutationRequest {
    method: &'static str,
    endpoint: String,
    body: Value,
}
impl std::fmt::Debug for MutationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutationRequest")
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("body", &"[private]")
            .finish()
    }
}
impl MutationRequest {
    /// Fixed HTTP method selected by the operation.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        self.method
    }
    /// Relative provider endpoint selected by the operation.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    /// JSON request body. Callers must not log private issue context.
    #[must_use]
    pub const fn body(&self) -> &Value {
        &self.body
    }
}
/// Mutation transport, injected separately from workflow policy.
/// Any failure after submission is uncertain unless the provider proves refusal.
pub trait GitHubMutationTransport: GitHubReadTransport {
    /// Submit one bounded request. No automatic retry is permitted.
    ///
    /// # Errors
    /// Errors may indicate an uncertain external outcome and require reconciliation.
    fn submit(
        &self,
        credential: &CredentialRef,
        request: &MutationRequest,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, EffectFailure>;
}
impl GitHubMutationTransport for GhCli {
    fn submit(
        &self,
        credential: &CredentialRef,
        request: &MutationRequest,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, EffectFailure> {
        let args = vec![
            "api".into(),
            "--hostname".into(),
            "github.com".into(),
            "--method".into(),
            request.method.into(),
            request.endpoint.clone(),
            "--input".into(),
            "-".into(),
        ];
        let input = serde_json::to_vec(&request.body)
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let output = self
            .call(credential, &args, &input, timeout, max_bytes)
            .map_err(|error| match error {
                IntegrationError::ScopeMismatch | IntegrationError::InvalidInput => {
                    EffectFailure::NotApplied(NotAppliedReason::Rejected)
                }
                IntegrationError::Timeout => EffectFailure::Uncertain(UncertainReason::Timeout),
                _ => EffectFailure::Uncertain(UncertainReason::Transport),
            })?;
        if output.code != Some(0) {
            return Err(EffectFailure::Uncertain(UncertainReason::Transport));
        }
        Ok(output.stdout)
    }
}

pub(crate) enum Inspection {
    Applied(Receipt),
    Missing,
    Conflict,
}

/// Per-operation read budget; an exhausted page budget is never absence evidence.
pub(crate) struct Provider<'a, T> {
    pub scope: &'a HouseScope,
    pub transport: &'a T,
    started: Instant,
    remaining: usize,
    limits: ReadLimits,
}
impl<'a, T: GitHubReadTransport> Provider<'a, T> {
    pub fn new(scope: &'a HouseScope, transport: &'a T, limits: ReadLimits) -> Self {
        Self {
            scope,
            transport,
            started: Instant::now(),
            remaining: limits.bytes(),
            limits,
        }
    }
    pub fn remaining(&self) -> Result<Duration, IntegrationError> {
        self.limits
            .timeout()
            .checked_sub(self.started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(IntegrationError::Timeout)
    }
    pub fn read(&mut self, endpoint: String) -> Result<Value, IntegrationError> {
        if self.remaining == 0 {
            return Err(IntegrationError::LimitExceeded);
        }
        let bytes = self.transport.read(
            self.scope.credential(),
            &ReadRequest {
                endpoint,
                graphql: None,
            },
            self.remaining()?,
            self.remaining,
        )?;
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or(IntegrationError::LimitExceeded)?;
        serde_json::from_slice(&bytes).map_err(|_| IntegrationError::Unknown)
    }
    fn pages(&mut self, endpoint: &str) -> Result<Vec<Value>, IntegrationError> {
        let mut all = Vec::new();
        for page in 1..=self.limits.pages() {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let value = self.read(format!("{endpoint}{separator}per_page=100&page={page}"))?;
            let entries = value.as_array().ok_or(IntegrationError::Unknown)?;
            if entries.len() > 100 {
                return Err(IntegrationError::LimitExceeded);
            }
            all.extend(entries.iter().cloned());
            if entries.len() < 100 {
                return Ok(all);
            }
        }
        Err(IntegrationError::LimitExceeded)
    }
    pub fn inspect(
        &mut self,
        mutation: &GitHubMutation,
        key: &IdempotencyKey,
    ) -> Result<Inspection, IntegrationError> {
        mutation.validate()?;
        self.scope
            .authorize_read(self.scope.house(), &mutation.repository)?;
        let root = format!("repos/{}", mutation.repository);
        let reference = receipt(key)?;
        match &mutation.action {
            GitHubAction::MergePullRequest {
                number,
                expected_head,
                ..
            } => {
                let pr = self.read(format!("{root}/pulls/{}", number.get()))?;
                if pr.get("number").and_then(Value::as_u64) != Some(number.get()) {
                    return Err(IntegrationError::Unknown);
                }
                let head = pr
                    .pointer("/head/sha")
                    .and_then(Value::as_str)
                    .ok_or(IntegrationError::Unknown)?;
                if head != expected_head.as_str() {
                    return Ok(Inspection::Conflict);
                }
                match pr.get("merged").and_then(Value::as_bool) {
                    Some(true) => {
                        let sha = pr
                            .get("merge_commit_sha")
                            .and_then(Value::as_str)
                            .ok_or(IntegrationError::Unknown)?;
                        let merge = crate::contracts::CommitId::new(sha)
                            .map_err(|_| IntegrationError::Unknown)?;
                        let receipt =
                            Receipt::new(ExternalRef::new(merge.as_str())?, vec![], vec![])?;
                        Ok(Inspection::Applied(receipt))
                    }
                    Some(false) => Ok(Inspection::Missing),
                    None => Err(IntegrationError::Unknown),
                }
            }
            GitHubAction::PostComment { issue, body } => {
                let expected = marked(body.as_str(), key);
                let entries = self.pages(&format!("{root}/issues/{}/comments", issue.get()))?;
                inspect_markers(
                    &entries,
                    "body",
                    &expected,
                    self.scope.requester().as_str(),
                    mutation.repository.as_str(),
                )
            }
            GitHubAction::CreateIssue { title, body } => {
                let expected = marked(body.as_str(), key);
                let entries = self.pages(&format!("{root}/issues?state=all"))?;
                let matches: Vec<_> = entries
                    .into_iter()
                    .filter(|v| {
                        v.get("pull_request").is_none()
                            && v.get("title").and_then(Value::as_str) == Some(title.as_str())
                    })
                    .collect();
                inspect_markers(
                    &matches,
                    "body",
                    &expected,
                    self.scope.requester().as_str(),
                    mutation.repository.as_str(),
                )
            }
            GitHubAction::SetLabel {
                issue,
                label,
                present,
            } => {
                let labels = self.pages(&format!("{root}/issues/{}/labels", issue.get()))?;
                let mut found = false;
                for value in labels {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or(IntegrationError::Unknown)?;
                    found |= name.eq_ignore_ascii_case(label);
                }
                Ok(if found == *present {
                    Inspection::Applied(reference)
                } else {
                    Inspection::Missing
                })
            }
            GitHubAction::CreateLabel { label } => {
                let labels: Vec<Label> = self
                    .pages(&format!("{root}/labels"))?
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<Result<_, _>>()
                    .map_err(|_| IntegrationError::Unknown)?;
                Ok(match label.inspect(&labels)? {
                    LabelSetup::Present => Inspection::Applied(reference),
                    LabelSetup::Missing => Inspection::Missing,
                    LabelSetup::Conflict => Inspection::Conflict,
                })
            }
            GitHubAction::LinkSubIssue { parent, child } => self.relationship(
                &format!("{root}/issues/{}/sub_issues", parent.get()),
                child.get(),
                mutation.repository.as_str(),
                reference,
            ),
            GitHubAction::LinkDependency { issue, blocker } => self.relationship(
                &format!("{root}/issues/{}/dependencies/blocked_by", issue.get()),
                blocker.get(),
                mutation.repository.as_str(),
                reference,
            ),
        }
    }
    fn relationship(
        &mut self,
        endpoint: &str,
        number: u64,
        repository: &str,
        receipt: Receipt,
    ) -> Result<Inspection, IntegrationError> {
        for entry in self.pages(endpoint)? {
            if entry
                .get("number")
                .and_then(Value::as_u64)
                .ok_or(IntegrationError::Unknown)?
                == number
            {
                let source = entry
                    .get("repository_url")
                    .and_then(Value::as_str)
                    .ok_or(IntegrationError::Unknown)?;
                if source != format!("https://api.github.com/repos/{repository}") {
                    continue;
                }
                return Ok(Inspection::Applied(receipt));
            }
        }
        Ok(Inspection::Missing)
    }
    pub fn prepare(
        &mut self,
        mutation: &GitHubMutation,
        key: &IdempotencyKey,
    ) -> Result<MutationRequest, IntegrationError> {
        let root = format!("repos/{}", mutation.repository);
        let (method, endpoint, body) = match &mutation.action {
            GitHubAction::MergePullRequest {
                number,
                expected_head,
                ..
            } => (
                "PUT",
                format!("{root}/pulls/{}/merge", number.get()),
                json!({"sha": expected_head.as_str(), "merge_method": "squash"}),
            ),
            GitHubAction::PostComment { issue, body } => (
                "POST",
                format!("{root}/issues/{}/comments", issue.get()),
                json!({"body":marked(body.as_str(),key)}),
            ),
            GitHubAction::CreateIssue { title, body } => (
                "POST",
                format!("{root}/issues"),
                json!({"title":title.as_str(),"body":marked(body.as_str(),key)}),
            ),
            GitHubAction::SetLabel {
                issue,
                label,
                present: true,
            } => (
                "POST",
                format!("{root}/issues/{}/labels", issue.get()),
                json!({"labels":[label]}),
            ),
            GitHubAction::SetLabel {
                issue,
                label,
                present: false,
            } => (
                "DELETE",
                format!(
                    "{root}/issues/{}/labels/{}",
                    issue.get(),
                    encode_segment(label)
                ),
                json!({}),
            ),
            GitHubAction::CreateLabel { label } => (
                "POST",
                format!("{root}/labels"),
                json!({"name":label.name,"color":label.color,"description":label.description}),
            ),
            GitHubAction::LinkSubIssue { parent, child } => {
                let id = self.issue_id(&root, child.get())?;
                (
                    "POST",
                    format!("{root}/issues/{}/sub_issues", parent.get()),
                    json!({"sub_issue_id":id,"replace_parent":false}),
                )
            }
            GitHubAction::LinkDependency { issue, blocker } => {
                let id = self.issue_id(&root, blocker.get())?;
                (
                    "POST",
                    format!("{root}/issues/{}/dependencies/blocked_by", issue.get()),
                    json!({"issue_id":id}),
                )
            }
        };
        Ok(MutationRequest {
            method,
            endpoint,
            body,
        })
    }
    fn issue_id(&mut self, root: &str, number: u64) -> Result<u64, IntegrationError> {
        let value = self.read(format!("{root}/issues/{number}"))?;
        if value.get("number").and_then(Value::as_u64) != Some(number)
            || value.get("pull_request").is_some()
        {
            return Err(IntegrationError::Unknown);
        }
        value
            .get("id")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0 && *v <= i64::MAX as u64)
            .ok_or(IntegrationError::Unknown)
    }
}
pub(crate) fn receipt(key: &IdempotencyKey) -> Result<Receipt, IntegrationError> {
    Receipt::new(
        ExternalRef::new(key.as_str()).map_err(|_| IntegrationError::InvalidInput)?,
        vec![],
        vec![],
    )
    .map_err(|_| IntegrationError::InvalidInput)
}
fn marked(body: &str, key: &IdempotencyKey) -> String {
    format!("{body}\n\n<!-- kitchen:{} -->", key.as_str())
}
fn inspect_markers(
    entries: &[Value],
    field: &str,
    expected: &str,
    requester: &str,
    repository: &str,
) -> Result<Inspection, IntegrationError> {
    #[derive(Deserialize)]
    struct Author {
        login: String,
    }
    let mut found = None;
    for entry in entries {
        let body = entry
            .get(field)
            .and_then(Value::as_str)
            .ok_or(IntegrationError::Unknown)?;
        if body == expected {
            let user: Author = serde_json::from_value(
                entry
                    .get("user")
                    .cloned()
                    .ok_or(IntegrationError::Unknown)?,
            )
            .map_err(|_| IntegrationError::Unknown)?;
            if user.login.eq_ignore_ascii_case(requester) {
                if found.is_some() {
                    return Err(IntegrationError::Unknown);
                }
                let url = entry
                    .get("html_url")
                    .and_then(Value::as_str)
                    .ok_or(IntegrationError::Unknown)?;
                if !url.starts_with(&format!("https://github.com/{repository}/issues/"))
                    && !url.starts_with(&format!("https://github.com/{repository}/pull/"))
                {
                    return Err(IntegrationError::Unknown);
                }
                found = Some(Receipt::new(ExternalRef::new(url)?, vec![], vec![])?);
            }
        }
    }
    Ok(match found {
        Some(receipt) => Inspection::Applied(receipt),
        None => Inspection::Missing,
    })
}
fn encode_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
