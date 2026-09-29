//! Provider-side mutation and reconciliation. Durable ownership lives in core.
use super::{
    CloseReason, CredentialRef, GhCli, GitHubAction, GitHubMutation, GitHubReadTransport,
    HouseScope, IntegrationError, Label, LabelSetup, ReadLimits, ReadRequest,
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
            "--include".into(),
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
        let started = Instant::now();
        let token = self
            .verified_token(credential, timeout)
            .map_err(|_| EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        let output = self
            .run_with_token(&token, &args, &input, remaining, max_bytes)
            .map_err(|error| match error {
                IntegrationError::Timeout => EffectFailure::Uncertain(UncertainReason::Timeout),
                _ => EffectFailure::Uncertain(UncertainReason::Transport),
            })?;
        let status = output
            .stdout
            .split(|byte| *byte == b'\n')
            .next()
            .and_then(|line| std::str::from_utf8(line).ok())
            .and_then(|line| line.strip_prefix("HTTP/"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok());
        let retry_after = output
            .stdout
            .split(|byte| *byte == b'\n')
            .take_while(|line| !line.is_empty() && *line != b"\r")
            .filter_map(|line| std::str::from_utf8(line).ok())
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
                    .and_then(|(_, value)| value.trim().parse::<u64>().ok())
            })
            .map(Duration::from_secs);
        match (status, output.code) {
            (Some(429), _) => Err(EffectFailure::NotApplied(NotAppliedReason::RateLimited {
                retry_after,
            })),
            (Some(400..=499), _) => Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            (Some(200..=299), Some(0)) => Ok(output.stdout),
            (None, Some(4)) => Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            _ => Err(EffectFailure::Uncertain(UncertainReason::Transport)),
        }
    }
}

pub(crate) enum Inspection {
    Applied(Receipt),
    Missing,
    Conflict,
    /// An unmerged pull request at the expected head now targets another
    /// base. No new merge may start, but an earlier request carrying the
    /// expected head can still merge it, so this is not absence evidence.
    Retargeted,
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
    fn duplicate_target(
        &mut self,
        repository: &crate::contracts::Repository,
        number: u64,
    ) -> Result<Option<u64>, IntegrationError> {
        let (owner, name) = repository
            .as_str()
            .split_once('/')
            .ok_or(IntegrationError::InvalidInput)?;
        let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){duplicateOf{number repository{nameWithOwner}}}}}";
        let number = i64::try_from(number).map_err(|_| IntegrationError::InvalidInput)?;
        let bytes = self.transport.read(
            self.scope.credential(),
            &ReadRequest {
                endpoint: "graphql".into(),
                graphql: Some(
                    json!({"query":query,"variables":{"owner":owner,"name":name,"number":number}}),
                ),
            },
            self.remaining()?,
            self.remaining,
        )?;
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or(IntegrationError::LimitExceeded)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| IntegrationError::Unknown)?;
        if value.get("errors").is_some() {
            return Err(IntegrationError::Unknown);
        }
        let issue = value
            .pointer("/data/repository/issue")
            .ok_or(IntegrationError::Unknown)?;
        if issue.is_null() {
            return Err(IntegrationError::Unknown);
        }
        let target = issue.get("duplicateOf").ok_or(IntegrationError::Unknown)?;
        if target.is_null() {
            return Ok(None);
        }
        if target
            .pointer("/repository/nameWithOwner")
            .and_then(Value::as_str)
            != Some(repository.as_str())
        {
            return Ok(None);
        }
        target
            .get("number")
            .and_then(Value::as_u64)
            .map(Some)
            .ok_or(IntegrationError::Unknown)
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
            GitHubAction::CloseIssue { number, reason, .. } => {
                let issue = self.read(format!("{root}/issues/{}", number.get()))?;
                if issue.get("number").and_then(Value::as_u64) != Some(number.get())
                    || issue.get("pull_request").is_some()
                {
                    return Err(IntegrationError::Unknown);
                }
                match issue.get("state").and_then(Value::as_str) {
                    Some("open") => Ok(Inspection::Missing),
                    Some("closed") => {
                        let matches = match reason {
                            CloseReason::Completed => {
                                issue.get("state_reason").and_then(Value::as_str)
                                    == Some("completed")
                            }
                            CloseReason::NotPlanned => {
                                issue.get("state_reason").and_then(Value::as_str)
                                    == Some("not_planned")
                            }
                            CloseReason::Duplicate(of) => {
                                issue.get("state_reason").and_then(Value::as_str)
                                    == Some("duplicate")
                                    && self.duplicate_target(&mutation.repository, number.get())?
                                        == Some(of.get())
                            }
                        };
                        Ok(if matches {
                            Inspection::Applied(reference)
                        } else {
                            Inspection::Conflict
                        })
                    }
                    _ => Err(IntegrationError::Unknown),
                }
            }
            GitHubAction::MergePullRequest {
                number,
                expected_head,
                expected_base,
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
                // Read the merged state first: a merge of the expected head
                // happened even if the base was retargeted before or after it.
                let merged = pr
                    .get("merged")
                    .and_then(Value::as_bool)
                    .ok_or(IntegrationError::Unknown)?;
                if head != expected_head.as_str() {
                    return Ok(Inspection::Conflict);
                }
                if merged {
                    let sha = pr
                        .get("merge_commit_sha")
                        .and_then(Value::as_str)
                        .ok_or(IntegrationError::Unknown)?;
                    let merge = crate::contracts::CommitId::new(sha)
                        .map_err(|_| IntegrationError::Unknown)?;
                    let mut receipt =
                        Receipt::new(ExternalRef::new(merge.as_str())?, vec![], vec![])?;
                    // The merge of the approved head stays applied, but a
                    // different actual base is carried so it is never taken
                    // for the approved merge into the intended base.
                    let actual = pr
                        .pointer("/base/ref")
                        .and_then(Value::as_str)
                        .ok_or(IntegrationError::Unknown)?;
                    if actual != expected_base.as_str() {
                        receipt = receipt.with_retarget(crate::contracts::Retarget {
                            expected: expected_base.clone(),
                            actual: crate::contracts::BranchName::new(actual)
                                .map_err(|_| IntegrationError::Unknown)?,
                        });
                    }
                    return Ok(Inspection::Applied(receipt));
                }
                if pr.pointer("/base/ref").and_then(Value::as_str) != Some(expected_base.as_str()) {
                    return Ok(Inspection::Retargeted);
                }
                Ok(Inspection::Missing)
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
                // Markers are only trusted from the requester, so list just its issues:
                // the scan stays complete without paging through the whole repository.
                let entries = self.pages(&format!(
                    "{root}/issues?state=all&creator={}&sort=created&direction=desc",
                    encode_segment(self.scope.requester().as_str())
                ))?;
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
            GitHubAction::CloseIssue { number, reason, .. } => {
                // Unverified against the live API: the REST PATCH accepts
                // `duplicate_issue_id` (the canonical issue's database ID) with
                // `state_reason: duplicate`; the key is omitted otherwise.
                let body = match reason {
                    CloseReason::Completed => {
                        json!({"state":"closed","state_reason":"completed"})
                    }
                    CloseReason::NotPlanned => {
                        json!({"state":"closed","state_reason":"not_planned"})
                    }
                    CloseReason::Duplicate(of) => json!({
                        "state":"closed",
                        "state_reason":"duplicate",
                        "duplicate_issue_id": self.issue_id(&root, of.get())?,
                    }),
                };
                ("PATCH", format!("{root}/issues/{}", number.get()), body)
            }
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
            } => {
                let labels = self.pages(&format!("{root}/labels"))?;
                if !labels.iter().any(|entry| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case(label))
                }) {
                    return Err(IntegrationError::InvalidInput);
                }
                (
                    "POST",
                    format!("{root}/issues/{}/labels", issue.get()),
                    json!({"labels":[label]}),
                )
            }
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

#[cfg(all(test, unix))]
#[path = "../../../tests/common/executable.rs"]
mod executable;

#[cfg(all(test, unix))]
mod mutation_tests {
    use super::*;
    use crate::{CredentialId, HouseId, contracts::ExternalRef};
    use std::{cell::RefCell, collections::VecDeque};

    struct ReadFixture(RefCell<VecDeque<Value>>);
    impl GitHubReadTransport for ReadFixture {
        fn read(
            &self,
            _: &CredentialRef,
            _: &ReadRequest,
            _: Duration,
            _: usize,
        ) -> Result<Vec<u8>, IntegrationError> {
            serde_json::to_vec(
                &self
                    .0
                    .borrow_mut()
                    .pop_front()
                    .ok_or(IntegrationError::Unknown)?,
            )
            .map_err(|_| IntegrationError::Unknown)
        }
    }
    impl GitHubMutationTransport for ReadFixture {
        fn submit(
            &self,
            _: &CredentialRef,
            _: &MutationRequest,
            _: Duration,
            _: usize,
        ) -> Result<Vec<u8>, EffectFailure> {
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        }
    }

    #[test]
    fn close_issue_reconciles_same_reason_and_refuses_unauthorized_effect()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::contracts::{
            GitHubEffect, IdempotencyKey, IssueNumber, Permission, PostingBudget, Repository,
        };
        let house = HouseId::new("sample")?;
        let requester = ExternalRef::new("sample-bot")?;
        let credential =
            CredentialRef::new(house.clone(), CredentialId::new("gh")?, requester.clone());
        let scope = HouseScope::new(
            house,
            [Repository::new("sample/project")?],
            requester.clone(),
            credential,
            PostingBudget::new(2)?,
            [Permission::EditIssueRelationships],
        )?;
        let mutation = GitHubMutation {
            repository: Repository::new("sample/project")?,
            action: GitHubAction::CloseIssue {
                repository: Repository::new("sample/project")?,
                number: IssueNumber::new(1)?,
                reason: CloseReason::Completed,
            },
        };
        let mut foreign = mutation.clone();
        if let GitHubAction::CloseIssue { repository, .. } = &mut foreign.action {
            *repository = Repository::new("foreign/project")?;
        }
        assert!(foreign.validate().is_err());
        let key = IdempotencyKey::from_ref(ExternalRef::new("close-fixture")?);
        let read = ReadFixture(RefCell::new(VecDeque::from([
            json!({"number":1,"state":"closed","state_reason":"completed"}),
        ])));
        let mut provider = Provider::new(&scope, &read, ReadLimits::default());
        assert!(matches!(
            provider.inspect(&mutation, &key)?,
            Inspection::Applied(_)
        ));
        let read = ReadFixture(RefCell::new(VecDeque::from([
            json!({"number":1,"state":"closed","state_reason":"not_planned"}),
        ])));
        let mut provider = Provider::new(&scope, &read, ReadLimits::default());
        assert!(matches!(
            provider.inspect(&mutation, &key)?,
            Inspection::Conflict
        ));
        let read = ReadFixture(RefCell::new(VecDeque::from([
            json!({"number":1,"state":"open","state_reason":null}),
        ])));
        let mut provider = Provider::new(&scope, &read, ReadLimits::default());
        assert!(matches!(
            provider.inspect(&mutation, &key)?,
            Inspection::Missing
        ));
        let effect = GitHubEffect {
            requester,
            mutation,
            posting_budget: PostingBudget::new(2)?,
        };
        assert_eq!(effect.required_permission(), Permission::CloseIssue);
        let encoded = serde_json::to_vec(&effect)?;
        assert_eq!(serde_json::from_slice::<GitHubEffect>(&encoded)?, effect);
        // Relationship edits do not imply closing an issue.
        let executor = super::super::GitHubExecutor::new(
            crate::BackendId::new("github")?,
            scope,
            read,
            ReadLimits::default(),
        );
        assert!(matches!(
            executor.effect(effect.mutation.clone()),
            Err(IntegrationError::PermissionDenied)
        ));
        let granted = HouseScope::new(
            HouseId::new("sample")?,
            [Repository::new("sample/project")?],
            effect.requester.clone(),
            CredentialRef::new(
                HouseId::new("sample")?,
                CredentialId::new("gh")?,
                effect.requester.clone(),
            ),
            PostingBudget::new(2)?,
            [Permission::CloseIssue],
        )?;
        let executor = super::super::GitHubExecutor::new(
            crate::BackendId::new("github")?,
            granted,
            ReadFixture(RefCell::new(VecDeque::new())),
            ReadLimits::default(),
        );
        assert_eq!(executor.effect(effect.mutation.clone())?, effect);
        Ok(())
    }

    #[test]
    fn duplicate_close_uses_canonical_issue_id_and_reconciles_exact_target()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::contracts::{
            IdempotencyKey, IssueNumber, Permission, PostingBudget, Repository,
        };
        let house = HouseId::new("sample")?;
        let requester = ExternalRef::new("sample-bot")?;
        let credential =
            CredentialRef::new(house.clone(), CredentialId::new("gh")?, requester.clone());
        let scope = HouseScope::new(
            house,
            [Repository::new("sample/project")?],
            requester,
            credential,
            PostingBudget::new(1)?,
            [Permission::EditIssueRelationships],
        )?;
        let mutation = GitHubMutation {
            repository: Repository::new("sample/project")?,
            action: GitHubAction::CloseIssue {
                repository: Repository::new("sample/project")?,
                number: IssueNumber::new(1)?,
                reason: CloseReason::Duplicate(IssueNumber::new(2)?),
            },
        };
        let key = IdempotencyKey::from_ref(ExternalRef::new("close-duplicate")?);
        let canonical = json!({"number":2,"id":102});
        let read = ReadFixture(RefCell::new(VecDeque::from([canonical.clone()])));
        let mut provider = Provider::new(&scope, &read, ReadLimits::default());
        let prepared = provider.prepare(&mutation, &key)?;
        assert_eq!(prepared.body()["duplicate_issue_id"], 102);
        assert_eq!(prepared.body()["state_reason"], "duplicate");
        let read = ReadFixture(RefCell::new(VecDeque::new()));
        let mut provider = Provider::new(&scope, &read, ReadLimits::default());
        let completed = GitHubMutation {
            repository: mutation.repository.clone(),
            action: GitHubAction::CloseIssue {
                repository: mutation.repository.clone(),
                number: IssueNumber::new(1)?,
                reason: CloseReason::Completed,
            },
        };
        let prepared = provider.prepare(&completed, &key)?;
        assert_eq!(
            prepared.body(),
            &json!({"state":"closed","state_reason":"completed"})
        );
        let read = ReadFixture(RefCell::new(VecDeque::from([
            json!({"number":1,"state":"closed","state_reason":"duplicate","duplicate_issue_id":102}),
            json!({"data":{"repository":{"issue":{"duplicateOf":{"number":2,"repository":{"nameWithOwner":"sample/project"}}}}}}),
        ])));
        let mut provider = Provider::new(&scope, &read, ReadLimits::default());
        assert!(matches!(
            provider.inspect(&mutation, &key)?,
            Inspection::Applied(_)
        ));
        Ok(())
    }

    fn fake_cli(
        script: &str,
    ) -> Result<(tempfile::TempDir, GhCli, CredentialRef), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("gh");
        super::executable::write_executable(&executable, format!("#!/bin/sh\n{script}\n"))?;
        let token_path = directory.path().join("token");
        std::fs::write(&token_path, "fixture-secret")?;
        let credential = CredentialRef::new(
            HouseId::new("sample")?,
            CredentialId::new("gh")?,
            ExternalRef::new("sample-bot")?,
        );
        let cli = GhCli::new(
            executable,
            super::super::CredentialFile::new(credential.clone(), token_path)?,
        )?;
        Ok((directory, cli, credential))
    }

    #[test]
    fn submit_classifies_provider_refusals_and_ambiguous_outcomes()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = MutationRequest {
            method: "POST",
            endpoint: "repos/sample/project/issues/1/comments".into(),
            body: json!({"body":"fixture"}),
        };
        for (response, expected) in [
            (
                "HTTP/2 403 Forbidden",
                EffectFailure::NotApplied(NotAppliedReason::Rejected),
            ),
            (
                "HTTP/2 405 Method Not Allowed",
                EffectFailure::NotApplied(NotAppliedReason::Rejected),
            ),
            (
                "HTTP/2 409 Conflict",
                EffectFailure::NotApplied(NotAppliedReason::Rejected),
            ),
            (
                "HTTP/2 410 Gone",
                EffectFailure::NotApplied(NotAppliedReason::Rejected),
            ),
            (
                "HTTP/2 429 Too Many Requests\r\nRetry-After: 12",
                EffectFailure::NotApplied(NotAppliedReason::RateLimited {
                    retry_after: Some(Duration::from_secs(12)),
                }),
            ),
            (
                "HTTP/2 429 Too Many Requests\r\nRetry-After: Wed, 21 Oct 2026 07:28:00 GMT",
                EffectFailure::NotApplied(NotAppliedReason::RateLimited { retry_after: None }),
            ),
            (
                "HTTP/2 200 OK",
                EffectFailure::Uncertain(UncertainReason::Transport),
            ),
            (
                "HTTP/2 404 Not Found",
                EffectFailure::NotApplied(NotAppliedReason::Rejected),
            ),
            (
                "HTTP/2 422 Unprocessable",
                EffectFailure::NotApplied(NotAppliedReason::Rejected),
            ),
            (
                "HTTP/2 502 Bad Gateway",
                EffectFailure::Uncertain(UncertainReason::Transport),
            ),
        ] {
            let script = format!(
                "for arg in \"$@\"; do case \"$arg\" in *fixture-secret*) exit 9;; esac; done\ncase \" $* \" in *' user '*) printf '%s' '{{\"login\":\"sample-bot\"}}';; *' --include '*) printf '%s\\r\\n\\r\\n' '{response}'; exit 1;; *) exit 1;; esac"
            );
            let (_directory, cli, credential) = fake_cli(&script)?;
            assert_eq!(
                cli.submit(&credential, &request, Duration::from_secs(2), 4096),
                Err(expected)
            );
        }
        // gh exits 4 for authentication refusals without printing a status line.
        let (_directory, cli, credential) = fake_cli(
            "case \" $* \" in *' user '*) printf '%s' '{\"login\":\"sample-bot\"}';; *) exit 4;; esac",
        )?;
        assert_eq!(
            cli.submit(&credential, &request, Duration::from_secs(2), 4096),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
        let (_directory, cli, credential) = fake_cli(
            "case \" $* \" in *' user '*) printf '%s' '{\"login\":\"wrong\"}';; *) exit 9;; esac",
        )?;
        assert_eq!(
            cli.submit(&credential, &request, Duration::from_secs(2), 4096),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
        let (_directory, cli, credential) = fake_cli(
            "case \" $* \" in *' user '*) printf '%s' '{\"login\":\"sample-bot\"}';; *) while :; do :; done;; esac",
        )?;
        assert_eq!(
            cli.submit(&credential, &request, Duration::from_secs(3), 4096),
            Err(EffectFailure::Uncertain(UncertainReason::Timeout))
        );
        Ok(())
    }
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
