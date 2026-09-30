//! Sanitized offline GitHub boundary regression tests.
use kitchen::{
    HouseId,
    contracts::{ExternalRef, Permission, Repository, Text},
    integrations::github::*,
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::VecDeque, time::Duration};
mod common;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct Fake {
    pages: RefCell<VecDeque<std::result::Result<Vec<u8>, IntegrationError>>>,
    requests: RefCell<Vec<String>>,
}
impl Fake {
    fn new(pages: Vec<std::result::Result<Value, IntegrationError>>) -> Result<Self> {
        let mut encoded = VecDeque::new();
        for page in pages {
            encoded.push_back(match page {
                Ok(value) => Ok(serde_json::to_vec(&value)?),
                Err(error) => Err(error),
            });
        }
        Ok(Self {
            pages: RefCell::new(encoded),
            requests: RefCell::default(),
        })
    }
}
impl GitHubReadTransport for Fake {
    fn read(
        &self,
        _: &CredentialRef,
        request: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> std::result::Result<Vec<u8>, IntegrationError> {
        self.requests.borrow_mut().push(request.endpoint().into());
        self.pages
            .borrow_mut()
            .pop_front()
            .unwrap_or(Err(IntegrationError::Unavailable))
    }
}
fn scope() -> Result<HouseScope> {
    let house = HouseId::new("sample")?;
    let requester = ExternalRef::new("sample-bot")?;
    Ok(HouseScope::new(
        house.clone(),
        [Repository::new("sample/project")?],
        requester.clone(),
        CredentialRef::new(house, kitchen::CredentialId::new("github-read")?, requester),
        PostingBudget::new(2)?,
        [Permission::PostComment],
    )?)
}
fn issue(number: u64) -> Value {
    json!({"repository_url":"https://api.github.com/repos/sample/project","id":number,"number":number,"title":"sanitized issue","state":"open","assignees":[],"labels":[],"updated_at":"2026-01-02T00:00:00Z","closed_at":null})
}
#[test]
fn scope_rejects_foreign_house_repository_requester_and_budget() -> Result {
    let selected = scope()?;
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    assert_eq!(
        selected.authorize_read(&HouseId::new("foreign")?, &repo),
        Err(IntegrationError::ScopeMismatch)
    );
    assert_eq!(
        selected.authorize_read(&house, &Repository::new("foreign/project")?),
        Err(IntegrationError::ScopeMismatch)
    );
    assert_eq!(
        selected.authorize_effect(&house, &repo, Permission::PostComment, 2),
        Err(IntegrationError::BudgetExhausted)
    );
    assert_eq!(
        selected.authorize_effect(&house, &repo, Permission::EditLabels, 0),
        Err(IntegrationError::MissingPermission(Permission::EditLabels))
    );
    selected.authorize_effect(&house, &repo, Permission::PostComment, 1)?;
    let mismatch = HouseScope::new(
        house,
        [repo],
        ExternalRef::new("wrong")?,
        selected.credential().clone(),
        PostingBudget::new(1)?,
        [],
    );
    assert!(matches!(mismatch, Err(IntegrationError::ScopeMismatch)));
    assert!(PostingBudget::new(101).is_err());
    assert!(IssueNumber::new(0).is_err());
    Ok(())
}
#[test]
fn rejected_read_never_reaches_transport() -> Result {
    let client = GitHubClient::new(scope()?, Fake::default(), ReadLimits::default());
    assert_eq!(
        client.issues(
            &HouseId::new("foreign")?,
            &Repository::new("sample/project")?
        ),
        Observation::Unavailable(IntegrationError::ScopeMismatch)
    );
    assert!(client.transport().requests.borrow().is_empty());
    Ok(())
}
#[test]
fn pagination_is_complete_and_excludes_pull_requests() -> Result {
    let mut first: Vec<_> = (1..=100).map(issue).collect();
    first[0]["pull_request"] = json!({"url":"https://example.invalid/pr/1"});
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(json!(first)), Ok(json!([issue(101)]))])?,
        ReadLimits::default(),
    );
    let Observation::Known(issues) = client.issues(
        &HouseId::new("sample")?,
        &Repository::new("sample/project")?,
    ) else {
        return Err("expected complete issue evidence".into());
    };
    assert_eq!(issues.len(), 100);
    assert_eq!(issues.last().map(|i| i.number.get()), Some(101));
    assert_eq!(
        client.transport().requests.borrow().as_slice(),
        [
            "repos/sample/project/issues?state=all&per_page=100&page=1",
            "repos/sample/project/issues?state=all&per_page=100&page=2"
        ]
    );
    Ok(())
}
#[test]
fn partial_failure_limit_and_malformed_data_never_return_success() -> Result {
    let page = json!((1..=100).map(issue).collect::<Vec<_>>());
    for (pages, limits, expected) in [
        (
            vec![Ok(page.clone()), Err(IntegrationError::Unavailable)],
            ReadLimits::default(),
            Observation::Unavailable(IntegrationError::Unavailable),
        ),
        (
            vec![Ok(page)],
            ReadLimits::new(Duration::from_secs(1), 1, 65536)?,
            Observation::Unavailable(IntegrationError::LimitExceeded),
        ),
        (
            vec![Ok(json!([{"number":0}]))],
            ReadLimits::default(),
            Observation::Unknown,
        ),
        (
            vec![Ok(json!([issue(1)]))],
            ReadLimits::new(Duration::from_secs(1), 1, 1)?,
            Observation::Unavailable(IntegrationError::LimitExceeded),
        ),
    ] {
        let client = GitHubClient::new(scope()?, Fake::new(pages)?, limits);
        assert_eq!(
            client.issues(
                &HouseId::new("sample")?,
                &Repository::new("sample/project")?
            ),
            expected
        );
    }
    Ok(())
}
#[test]
fn threads_follow_cursors_and_reject_partial_graphql_errors() -> Result {
    let page = |next: bool, cursor: &str| json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":cursor,"isResolved":false,"isOutdated":false}],"pageInfo":{"hasNextPage":next,"endCursor":cursor}}}}}});
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(page(true, "a")), Ok(page(false, "b"))])?,
        ReadLimits::default(),
    );
    let Observation::Known(threads) = client.threads(
        &HouseId::new("sample")?,
        &Repository::new("sample/project")?,
        IssueNumber::new(1)?,
    ) else {
        return Err("expected threads".into());
    };
    assert_eq!(
        threads.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(threads.iter().all(|t| !t.is_resolved));
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(
            json!({"data":{},"errors":[{"message":"sanitized failure"}]}),
        )])?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.threads(
            &HouseId::new("sample")?,
            &Repository::new("sample/project")?,
            IssueNumber::new(1)?
        ),
        Observation::Unknown
    );
    Ok(())
}
#[test]
fn unknown_permission_and_identity_mismatch_cannot_approve() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!({"user":{"login":"alice"},"permission":"future"})),
            Ok(json!({"user":{"login":"mallory"},"permission":"admin"})),
        ])?,
        ReadLimits::default(),
    );
    let Observation::Known(evidence) = client.permission(&house, &repo, "alice") else {
        return Err("expected permission evidence".into());
    };
    assert_eq!(evidence.permission, RepositoryPermission::Unknown);
    assert_eq!(
        client.permission(&house, &repo, "alice"),
        Observation::Unknown
    );
    assert_eq!(
        client.permission(&house, &repo, "../wrong"),
        Observation::Unavailable(IntegrationError::InvalidInput)
    );
    Ok(())
}

#[test]
fn label_setup_preserves_present_and_conflicting_definitions() -> Result {
    let wanted = LabelDefinition {
        name: "agent-ready".into(),
        color: "AABBCC".into(),
        description: "Ready to pick up".into(),
    };
    assert_eq!(wanted.inspect(&[])?, LabelSetup::Missing);
    let matching = Label {
        name: "agent-ready".into(),
        color: "aabbcc".into(),
        description: Some("Ready to pick up".into()),
    };
    assert_eq!(
        wanted.inspect(std::slice::from_ref(&matching))?,
        LabelSetup::Present
    );
    let conflict = Label {
        color: "000000".into(),
        ..matching.clone()
    };
    assert_eq!(
        wanted.inspect(std::slice::from_ref(&conflict))?,
        LabelSetup::Conflict
    );
    assert_eq!(conflict.color, "000000");
    assert_eq!(
        wanted.inspect(&[matching.clone(), matching]),
        Err(IntegrationError::Unknown)
    );
    let bad = LabelDefinition {
        name: "bad\nlabel".into(),
        ..wanted
    };
    assert_eq!(
        bad.validate().map_err(IntegrationError::from),
        Err(IntegrationError::InvalidInput)
    );
    Ok(())
}

#[cfg(unix)]
fn fake_cli(script: &str) -> Result<(tempfile::TempDir, std::path::PathBuf, CredentialFile)> {
    let root = tempfile::tempdir()?;
    let executable = root.path().join("fake-gh");
    common::executable::write_executable(&executable, format!("#!/bin/sh\n{script}\n"))?;
    let token = root.path().join("token");
    std::fs::write(&token, "sanitized-fixture-token")?;
    let credential = CredentialFile::new(scope()?.credential().clone(), token)?;
    Ok((root, executable, credential))
}
#[cfg(unix)]
#[test]
fn gh_cli_clears_environment_and_checks_requester_before_repository_read() -> Result {
    let (_root, executable, credential) = fake_cli(
        r#"
[ "$GH_TOKEN" = sanitized-fixture-token ] || exit 2
[ -z "$ROGER_TOKEN" ] || exit 2
[ "$GH_HOST" = github.com ] || exit 2
for arg in "$@"; do [ "$arg" != sanitized-fixture-token ] || exit 2; done
[ "$HOME" = "$GH_CONFIG_DIR" ] || exit 2
if [ "$4" = user ]; then
  printf '%s' '{"login":"sample-bot"}'
else
  printf '%s' '{"repository_url":"https://api.github.com/repos/sample/project","id":1,"number":1,"title":"sample","state":"open","assignees":[],"labels":[],"updated_at":"2026-01-02T00:00:00Z","closed_at":null}'
fi
"#,
    )?;
    let client = GitHubClient::new(
        scope()?,
        GhCli::new(executable, credential)?,
        ReadLimits::default(),
    );
    let Observation::Known(value) = client.issue(
        &HouseId::new("sample")?,
        &Repository::new("sample/project")?,
        IssueNumber::new(1)?,
    ) else {
        return Err("expected isolated CLI read".into());
    };
    assert_eq!(value.number.get(), 1);
    let (_root, executable, credential) = fake_cli("printf '%s' '{\"login\":\"foreign\"}'")?;
    let client = GitHubClient::new(
        scope()?,
        GhCli::new(executable, credential)?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.issues(
            &HouseId::new("sample")?,
            &Repository::new("sample/project")?
        ),
        Observation::Unavailable(IntegrationError::ScopeMismatch)
    );
    Ok(())
}
#[cfg(unix)]
#[test]
fn gh_cli_timeout_output_bound_and_failure_are_explicit() -> Result {
    // Only the busy loop needs its deadline to fire. The other cases end on
    // their own, so a generous deadline keeps them independent of machine load.
    for (script, expected, deadline) in [
        (
            "while :; do :; done",
            IntegrationError::Timeout,
            Duration::from_secs(1),
        ),
        (
            "exec /usr/bin/yes sanitized-output",
            IntegrationError::LimitExceeded,
            Duration::from_secs(60),
        ),
        (
            "exit 1",
            IntegrationError::Unavailable,
            Duration::from_secs(60),
        ),
    ] {
        let (_root, executable, credential) = fake_cli(script)?;
        let client = GitHubClient::new(
            scope()?,
            GhCli::new(executable, credential)?,
            ReadLimits::new(deadline, 1, 65536)?,
        );
        assert_eq!(
            client.issues(
                &HouseId::new("sample")?,
                &Repository::new("sample/project")?
            ),
            Observation::Unavailable(expected)
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh_cli_reports_a_missing_resource_apart_from_other_failures() -> Result {
    let identity = r#"if [ "$4" = user ]; then printf '%s' '{"login":"sample-bot"}'; exit 0; fi"#;
    for (body, expected) in [
        (
            r#"{"message":"Branch not found","status":"404"}"#,
            IntegrationError::NotFound,
        ),
        (
            r#"{"message":"Server Error","status":"502"}"#,
            IntegrationError::HttpStatus(502),
        ),
        ("gh: connection refused", IntegrationError::Unavailable),
        ("", IntegrationError::Unavailable),
    ] {
        let (_root, executable, credential) =
            fake_cli(&format!("{identity}\nprintf '%s' '{body}'\nexit 1"))?;
        let client = GitHubClient::new(
            scope()?,
            GhCli::new(executable, credential)?,
            ReadLimits::default(),
        );
        let branch = kitchen::contracts::BranchName::new("gone")?;
        assert_eq!(
            client.branch_tip(
                &HouseId::new("sample")?,
                &Repository::new("sample/project")?,
                &branch
            ),
            Observation::Unavailable(expected),
            "{body}"
        );
    }
    // A 404 body with a successful exit is still an answer, not absence.
    let (_root, executable, credential) = fake_cli(&format!(
        "{identity}\nprintf '%s' '{{\"message\":\"Not Found\",\"status\":\"404\"}}'"
    ))?;
    let client = GitHubClient::new(
        scope()?,
        GhCli::new(executable, credential)?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.branch_tip(
            &HouseId::new("sample")?,
            &Repository::new("sample/project")?,
            &kitchen::contracts::BranchName::new("gone")?
        ),
        Observation::Unknown
    );
    Ok(())
}

#[test]
fn exact_head_checks_reviews_dependencies_and_unknown_mergeability() -> Result {
    use kitchen::contracts::CommitId;
    let head = "a".repeat(40);
    let fake = Fake::new(vec![
        Ok(
            json!({"number":7,"state":"open","draft":false,"merged":false,"head":{"sha":head,"ref":"feature"},"base":{"sha":"b".repeat(40),"ref":"main"},"mergeable":null}),
        ),
        Ok(
            json!({"check_runs":[{"name":"ci","head_sha":head,"status":"completed","conclusion":"success"}]}),
        ),
        Ok(
            json!([{"id":1,"user":{"login":"copilot"},"commit_id":"b".repeat(40),"state":"COMMENTED","body":"Unable to review this pull request because the quota has been reached.","submitted_at":"2026-09-28T15:00:00Z"}]),
        ),
        Ok(json!([issue(4)])),
    ])?;
    let client = GitHubClient::new(scope()?, fake, ReadLimits::default());
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let number = IssueNumber::new(7)?;
    let Observation::Known(pr) = client.pull_request(&house, &repo, number) else {
        return Err("expected PR".into());
    };
    assert_eq!(pr.head.sha, CommitId::new(&head)?);
    assert_eq!(pr.mergeable, None);
    let Observation::Known(checks) = client.checks(&house, &repo, &pr.head.sha) else {
        return Err("expected checks".into());
    };
    assert_eq!(checks[0].conclusion, Some(CheckConclusion::Success));
    assert_eq!(checks[0].head_sha, pr.head.sha);
    let Observation::Known(reviews) = client.reviews(&house, &repo, number) else {
        return Err("expected reviews".into());
    };
    assert_ne!(reviews[0].commit_id, pr.head.sha);
    assert_eq!(
        reviews[0].body.as_deref(),
        Some("Unable to review this pull request because the quota has been reached.")
    );
    assert_eq!(
        reviews[0].submitted_at.map(|date| date.as_unix_millis()),
        Some(1790607600000)
    );
    let Observation::Known(deps) = client.dependencies(&house, &repo, number) else {
        return Err("expected dependencies".into());
    };
    assert_eq!(deps[0].number.get(), 4);
    Ok(())
}

#[test]
fn bounded_mutation_payloads_reject_self_links_and_bad_labels() -> Result {
    let repo = Repository::new("sample/project")?;
    let issue = IssueNumber::new(1)?;
    for action in [
        GitHubAction::LinkSubIssue {
            parent: issue,
            child: issue,
        },
        GitHubAction::LinkDependency {
            issue,
            blocker: issue,
        },
        GitHubAction::SetLabel {
            issue,
            label: "\n".into(),
            present: true,
        },
        GitHubAction::CreateLabel {
            label: LabelDefinition {
                name: "ready".into(),
                color: "xyz123".into(),
                description: String::new(),
            },
        },
        // Read-back matches titles exactly, so padding could never reconcile.
        GitHubAction::CreateIssue {
            title: Text::new(" padded ")?,
            body: Text::new("body")?,
        },
    ] {
        assert_eq!(
            GitHubMutation {
                repository: repo.clone(),
                action
            }
            .validate()
            .map_err(IntegrationError::from),
            Err(IntegrationError::InvalidInput)
        );
    }
    Ok(())
}

#[test]
fn provider_identity_substitution_is_unknown_and_dependency_source_is_retained() -> Result {
    let mut foreign = issue(4);
    foreign["repository_url"] = json!("https://api.github.com/repos/foreign/project");
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(foreign.clone()),
            Ok(json!([foreign])),
            Ok(
                json!({"check_runs":[{"name":"ci","head_sha":"b".repeat(40),"status":"completed","conclusion":"success"}]}),
            ),
        ])?,
        ReadLimits::default(),
    );
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    assert_eq!(
        client.issue(&house, &repo, IssueNumber::new(4)?),
        Observation::Unknown
    );
    let Observation::Known(dependencies) = client.dependencies(&house, &repo, IssueNumber::new(7)?)
    else {
        return Err("expected dependency reference".into());
    };
    assert_eq!(
        dependencies[0].repository,
        Repository::new("foreign/project")?
    );
    assert_eq!(
        client.checks(
            &house,
            &repo,
            &kitchen::contracts::CommitId::new(&"a".repeat(40))?
        ),
        Observation::Unknown
    );
    Ok(())
}

#[test]
fn gate_reads_bind_exact_head_and_preserve_missing_protection() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let head = kitchen::contracts::CommitId::new("1111111111111111111111111111111111111111")?;
    let base = kitchen::contracts::CommitId::new("2222222222222222222222222222222222222222")?;
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!({"behind_by":2,"ahead_by":3})),
            Ok(json!([{"context":"ci","state":"success","sha":head.as_str()}])),
            Ok(json!({"contexts":["ci"]})),
            Ok(json!({"sha":head.as_str(),"commit":{"committer":{"date":"2026-01-01T00:00:00Z"}}})),
        ])?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.compare(&house, &repo, &base, &head),
        Observation::Known(Compare {
            behind_by: 2,
            ahead_by: 3
        })
    );
    assert!(matches!(client.statuses(&house, &repo, &head), Observation::Known(v) if v.len() == 1));
    assert!(
        matches!(client.required_checks(&house, &repo, &kitchen::contracts::BranchName::new("main")?), Observation::Known(v) if v.contexts == ["ci"])
    );
    assert!(matches!(client.commit(&house, &repo, &head), Observation::Known(v) if v.sha == head));
    let missing = GitHubClient::new(
        scope()?,
        Fake::new(vec![Err(IntegrationError::Unavailable)])?,
        ReadLimits::default(),
    );
    assert_eq!(
        missing.required_checks(&house, &repo, &kitchen::contracts::BranchName::new("main")?),
        Observation::Unavailable(IntegrationError::Unavailable)
    );
    Ok(())
}

#[test]
fn statuses_refuse_moved_head_and_partial_page() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let head = kitchen::contracts::CommitId::new("1111111111111111111111111111111111111111")?;
    let other = kitchen::contracts::CommitId::new("2222222222222222222222222222222222222222")?;
    let moved = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(
            json!([{"context":"ci","state":"success","sha":other.as_str()}]),
        )])?,
        ReadLimits::default(),
    );
    assert_eq!(moved.statuses(&house, &repo, &head), Observation::Unknown);
    let full = json!(
        (0..100)
            .map(|_| json!({"context":"ci","state":"success","sha":head.as_str()}))
            .collect::<Vec<_>>()
    );
    let partial = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(full), Err(IntegrationError::Unavailable)])?,
        ReadLimits::default(),
    );
    assert_eq!(
        partial.statuses(&house, &repo, &head),
        Observation::Unavailable(IntegrationError::Unavailable)
    );
    Ok(())
}

#[test]
fn pull_request_commits_are_bounded_bound_to_the_head_and_keep_unlinked_accounts() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let number = IssueNumber::new(12)?;
    let head = kitchen::contracts::CommitId::new("1111111111111111111111111111111111111111")?;
    let other = kitchen::contracts::CommitId::new("2222222222222222222222222222222222222222")?;
    let read = |pages: Vec<std::result::Result<Value, IntegrationError>>| -> Result<_> {
        let client = GitHubClient::new(scope()?, Fake::new(pages)?, ReadLimits::default());
        Ok((
            client.pull_request_commits(&house, &repo, number, &head),
            client.transport().requests.borrow().clone(),
        ))
    };
    // Linked, unlinked (`null`, an empty object, an absent field) accounts.
    let (commits, requests) = read(vec![Ok(json!([
        {"sha": other.as_str(), "author": {"login": "dana"}, "committer": null},
        {"sha": head.as_str(), "author": {}, "commit": {"author": {"name": "Dana"}}},
    ]))])?;
    assert_eq!(
        commits,
        Observation::Known(vec![
            PullRequestCommit {
                sha: other.clone(),
                author: Some("dana".into()),
                committer: None,
            },
            PullRequestCommit {
                sha: head.clone(),
                author: None,
                committer: None,
            },
        ])
    );
    assert_eq!(
        requests,
        ["repos/sample/project/pulls/12/commits?per_page=100&page=1"]
    );
    // A list without the head is of another branch tip.
    let linked = |sha: &kitchen::contracts::CommitId| json!({"sha": sha.as_str(), "author": {"login": "dana"}, "committer": {"login": "dana"}});
    assert_eq!(
        read(vec![Ok(json!([linked(&other)]))])?.0,
        Observation::Unknown
    );
    assert_eq!(read(vec![Ok(json!([]))])?.0, Observation::Unknown);
    // Exactly the bound is read; one more is refused, never truncated.
    let mut full = vec![linked(&other); MAX_PULL_REQUEST_COMMITS];
    full[0] = linked(&head);
    assert!(matches!(
        read(vec![Ok(json!(full)), Ok(json!([]))])?.0,
        Observation::Known(commits) if commits.len() == MAX_PULL_REQUEST_COMMITS
    ));
    assert_eq!(
        read(vec![Ok(json!(full)), Ok(json!([linked(&other)]))])?.0,
        Observation::Unavailable(IntegrationError::LimitExceeded)
    );
    // A failed second page is a failed read, not a shorter list.
    assert_eq!(
        read(vec![Ok(json!(full)), Err(IntegrationError::Timeout)])?.0,
        Observation::Unavailable(IntegrationError::Timeout)
    );
    Ok(())
}

#[test]
fn required_checks_presence_fails_closed_for_missing_and_app_bound_checks() -> Result {
    let head = kitchen::contracts::CommitId::new("1111111111111111111111111111111111111111")?;
    let checks = RequiredChecks {
        contexts: vec!["ci".into()],
        checks: vec![],
    };
    assert_eq!(
        checks.presence(&[], &[], &head),
        RequiredCheckPresence::Missing
    );
    let status = CommitStatus {
        context: "ci".into(),
        state: StatusState::Success,
        sha: head.clone(),
    };
    assert_eq!(
        checks.presence(&[], &[status], &head),
        RequiredCheckPresence::Present
    );
    let app = RequiredChecks {
        contexts: vec![],
        checks: vec![RequiredCheck {
            context: "build".into(),
            app_id: Some(123),
        }],
    };
    assert_eq!(
        app.presence(&[], &[], &head),
        RequiredCheckPresence::Missing
    );
    let wrong_app: CheckRun = serde_json::from_value(
        json!({"name":"build","head_sha":head.as_str(),"status":"completed","conclusion":"success","app":{"id":122}}),
    )?;
    assert_eq!(
        app.presence(&[wrong_app], &[], &head),
        RequiredCheckPresence::Missing
    );
    let matching_app: CheckRun = serde_json::from_value(
        json!({"name":"build","head_sha":head.as_str(),"status":"completed","conclusion":"success","app":{"id":123}}),
    )?;
    assert_eq!(
        app.presence(&[matching_app], &[], &head),
        RequiredCheckPresence::Present
    );
    Ok(())
}

#[test]
fn branch_ref_reads_encode_the_branch_name() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let sha = "1111111111111111111111111111111111111111";
    for (name, path) in [
        ("main", "branches/main"),
        ("release/v1.0", "branches/release/v1.0"),
        ("feature#1", "branches/feature%231"),
        ("a%b&c+d", "branches/a%25b%26c%2Bd"),
    ] {
        let branch = kitchen::contracts::BranchName::new(name)?;
        let client = GitHubClient::new(
            scope()?,
            Fake::new(vec![Ok(json!({"name":name,"commit":{"sha":sha}}))])?,
            ReadLimits::default(),
        );
        assert!(
            matches!(client.branch_tip(&house, &repo, &branch), Observation::Known(tip) if tip.as_str() == sha),
            "{name}"
        );
        assert_eq!(
            client.transport().requests.borrow().as_slice(),
            [format!("repos/sample/project/{path}")],
        );
    }
    Ok(())
}

#[test]
fn branch_ref_answer_for_another_branch_is_unknown() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(
                json!({"name":"feature","commit":{"sha":"1111111111111111111111111111111111111111"}}),
            ),
            Err(IntegrationError::Unavailable),
        ])?,
        ReadLimits::default(),
    );
    let branch = kitchen::contracts::BranchName::new("feature#1")?;
    assert_eq!(
        client.branch_tip(&house, &repo, &branch),
        Observation::Unknown
    );
    assert_eq!(
        client.branch_tip(&house, &repo, &branch),
        Observation::Unavailable(IntegrationError::Unavailable)
    );
    Ok(())
}

#[test]
fn graphql_merge_status_requires_same_head() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let head = kitchen::contracts::CommitId::new("1111111111111111111111111111111111111111")?;
    let other = kitchen::contracts::CommitId::new("2222222222222222222222222222222222222222")?;
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(
                json!({"data":{"repository":{"pullRequest":{"headRefOid":head.as_str(),"mergeStateStatus":"CLEAN"}}}}),
            ),
            Ok(
                json!({"data":{"repository":{"pullRequest":{"headRefOid":other.as_str(),"mergeStateStatus":"CLEAN"}}}}),
            ),
        ])?,
        ReadLimits::default(),
    );
    assert!(matches!(
        client.merge_status(&house, &repo, IssueNumber::new(1)?, &head),
        Observation::Known(MergeStatus {
            status: MergeStatusValue::Clean,
            ..
        })
    ));
    assert_eq!(
        client.merge_status(&house, &repo, IssueNumber::new(1)?, &head),
        Observation::Unknown
    );
    Ok(())
}

#[test]
fn triage_reads_detail_timeline_comments_and_linked_pr() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let sha = "1111111111111111111111111111111111111111";
    let pr = json!({"number":9,"state":"closed","draft":false,"merged":true,"head":{"sha":sha,"ref":"feature","repo":{"full_name":"sample/project"}},"base":{"sha":sha,"ref":"main"},"mergeable":true,"merge_commit_sha":sha,"user":{"login":"author"},"author_association":"OWNER"});
    let cross = json!({"event":"cross-referenced","created_at":"2026-01-02T00:00:00Z","source":{"issue":{"number":9,"repository_url":"https://api.github.com/repos/sample/project","pull_request":{"url":"https://api.github.com/repos/sample/project/pulls/9"}}}});
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(
                json!({"number":1,"state":"open","user":{"login":"author"},"body":"untrusted text","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":null}),
            ),
            Ok(
                json!([{"id":1,"user":{"login":"author"},"body":"comment","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z"}]),
            ),
            Ok(json!([cross.clone()])),
            Ok(json!([cross])),
            Ok(
                json!({"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}),
            ),
            Ok(pr),
        ])?,
        ReadLimits::default(),
    );
    assert!(
        matches!(client.issue_detail(&house, &repo, IssueNumber::new(1)?), Observation::Known(v) if v.body.as_deref() == Some("untrusted text"))
    );
    assert!(
        matches!(client.comments(&house, &repo, IssueNumber::new(1)?), Observation::Known(v) if v.len() == 1)
    );
    assert!(
        matches!(client.timeline(&house, &repo, IssueNumber::new(1)?), Observation::Known(v) if v.len() == 1)
    );
    assert!(
        matches!(client.linked_pull_requests(&house, &repo, IssueNumber::new(1)?), Observation::Known(v) if v.len() == 1 && v[0].pull_request.merged)
    );
    Ok(())
}

#[test]
fn timeline_timestamps_are_parsed_at_the_boundary() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(json!([
            {"event":"labeled","created_at":"2026-09-28T16:00:00+02:00","label":{"name":"ready"}},
            {"event":"labeled","created_at":"1970-01-01T00:00:00Z","label":{"name":"ready"}},
            {"event":"committed"}
        ]))])?,
        ReadLimits::default(),
    );
    let Observation::Known(events) = client.timeline(&house, &repo, IssueNumber::new(1)?) else {
        return Err("timeline was not known".into());
    };
    let times: Vec<_> = events.iter().map(|event| event.created_at).collect();
    assert_eq!(
        times,
        vec![
            Some(kitchen::contracts::Timestamp::from_unix_millis(
                1_790_604_000_000
            )),
            Some(kitchen::contracts::Timestamp::from_unix_millis(0)),
            None,
        ]
    );
    Ok(())
}

#[test]
fn malformed_timeline_timestamps_make_the_timeline_unknown() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    for created_at in [
        json!("yesterday"),
        json!("2025-02-29T00:00:00Z"),
        json!("1969-12-31T23:59:59Z"),
        json!(1_790_604_000),
    ] {
        let client = GitHubClient::new(
            scope()?,
            Fake::new(vec![Ok(json!([
                {"event":"labeled","created_at":"2026-01-01T00:00:00Z","label":{"name":"ready"}},
                {"event":"unlabeled","created_at":created_at,"label":{"name":"ready"}}
            ]))])?,
            ReadLimits::default(),
        );
        assert_eq!(
            client.timeline(&house, &repo, IssueNumber::new(1)?),
            Observation::Unknown,
            "{created_at}"
        );
    }
    Ok(())
}

#[test]
fn triage_partial_timeline_is_unavailable() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let full = json!(
        (0..100)
            .map(|_| json!({"event":"labeled","label":{"name":"ready"}}))
            .collect::<Vec<_>>()
    );
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(full), Err(IntegrationError::Unavailable)])?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.timeline(&house, &repo, IssueNumber::new(1)?),
        Observation::Unavailable(IntegrationError::Unavailable)
    );
    Ok(())
}

#[test]
fn closing_pr_links_are_found_without_timeline_cross_reference() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let sha = "1111111111111111111111111111111111111111";
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!([])),
            Ok(
                json!({"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[{"number":9,"repository":{"nameWithOwner":"sample/project"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}),
            ),
            Ok(
                json!({"number":9,"state":"open","draft":false,"merged":false,"head":{"sha":sha,"ref":"feature"},"base":{"sha":sha,"ref":"main"},"mergeable":null}),
            ),
        ])?,
        ReadLimits::default(),
    );
    assert!(
        matches!(client.linked_pull_requests(&house, &repo, IssueNumber::new(1)?), Observation::Known(v) if v.len() == 1 && !v[0].pull_request.merged)
    );
    Ok(())
}

#[test]
fn required_checks_take_only_validated_branch_names() -> Result {
    for branch in [
        "",
        "..",
        "main/../other",
        "/main",
        "main/",
        "main//next",
        "main/.hidden",
    ] {
        assert!(
            kitchen::contracts::BranchName::new(branch).is_err(),
            "{branch}"
        );
    }
    Ok(())
}

#[test]
fn required_checks_encode_the_branch_name() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    for (name, path) in [
        ("main", "branches/main"),
        ("release/v1.0", "branches/release/v1.0"),
        ("feature#1", "branches/feature%231"),
        ("a%b&c+d", "branches/a%25b%26c%2Bd"),
    ] {
        let client = GitHubClient::new(
            scope()?,
            Fake::new(vec![Ok(json!({"contexts":["ci"],"checks":[]}))])?,
            ReadLimits::default(),
        );
        let branch = kitchen::contracts::BranchName::new(name)?;
        assert!(
            matches!(client.required_checks(&house, &repo, &branch), Observation::Known(v) if v.contexts == ["ci"]),
            "{name}"
        );
        assert_eq!(
            client.transport().requests.borrow().as_slice(),
            [format!(
                "repos/sample/project/{path}/protection/required_status_checks"
            )],
        );
    }
    Ok(())
}

#[test]
fn foreign_timeline_cross_reference_does_not_block_linked_prs() -> Result {
    let timeline = json!([{"event":"cross-referenced","source":{"issue":{"number":4,"repository_url":"https://api.github.com/repos/foreign/project","pull_request":{}}}}]);
    let closing = json!({"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}});
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(timeline), Ok(closing)])?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.linked_pull_requests(
            &HouseId::new("sample")?,
            &Repository::new("sample/project")?,
            IssueNumber::new(1)?
        ),
        Observation::Known(vec![])
    );
    assert_eq!(client.transport().requests.borrow().len(), 2);
    Ok(())
}

fn closing_references(nodes: &[(&str, u64)]) -> Value {
    let nodes: Vec<_> = nodes
        .iter()
        .map(|(repository, number)| {
            json!({"number":number,"repository":{"nameWithOwner":repository}})
        })
        .collect();
    json!({"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}})
}

#[test]
fn foreign_closing_reference_is_skipped_and_in_scope_ones_are_kept() -> Result {
    let house = HouseId::new("sample")?;
    let repo = Repository::new("sample/project")?;
    let sha = "1111111111111111111111111111111111111111";
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!([])),
            Ok(closing_references(&[
                ("foreign/project", 4),
                ("sample/project", 9),
            ])),
            Ok(
                json!({"number":9,"state":"open","draft":false,"merged":false,"head":{"sha":sha,"ref":"feature"},"base":{"sha":sha,"ref":"main"},"mergeable":null}),
            ),
        ])?,
        ReadLimits::default(),
    );
    let Observation::Known(linked) =
        client.linked_pull_requests(&house, &repo, IssueNumber::new(1)?)
    else {
        return Err("a foreign closing reference must not fail the read".into());
    };
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].repository, repo);
    assert_eq!(linked[0].pull_request.number.get(), 9);
    // Nothing is fetched from the repository outside the house scope.
    assert_eq!(
        *client.transport().requests.borrow(),
        [
            "repos/sample/project/issues/1/timeline?per_page=100&page=1",
            "graphql",
            "repos/sample/project/pulls/9"
        ]
    );

    let only_foreign = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!([])),
            Ok(closing_references(&[("foreign/project", 4)])),
        ])?,
        ReadLimits::default(),
    );
    assert_eq!(
        only_foreign.linked_pull_requests(&house, &repo, IssueNumber::new(1)?),
        Observation::Known(vec![])
    );
    assert_eq!(only_foreign.transport().requests.borrow().len(), 2);
    Ok(())
}

#[test]
fn malformed_closing_reference_still_fails_the_read() -> Result {
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!([])),
            Ok(closing_references(&[("not a repository", 4)])),
        ])?,
        ReadLimits::default(),
    );
    assert_eq!(
        client.linked_pull_requests(
            &HouseId::new("sample")?,
            &Repository::new("sample/project")?,
            IssueNumber::new(1)?
        ),
        Observation::Unknown
    );
    Ok(())
}

#[test]
fn inventory_filters_and_timestamps_are_typed() -> Result {
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![Ok(json!([issue(1)]))])?,
        ReadLimits::default(),
    );
    let found = client.issues_filtered(
        &HouseId::new("sample")?,
        &Repository::new("sample/project")?,
        Some(IssueState::Open),
        Some(kitchen::contracts::Timestamp::from_unix_millis(0)),
    );
    assert!(
        matches!(found, Observation::Known(v) if v[0].updated_at.as_unix_millis() > 0 && v[0].closed_at.is_none())
    );
    assert!(client.transport().requests.borrow()[0].contains("state=open&since=1970-01-01T"));

    // Offsets normalize to one instant; malformed and pre-epoch values are not guessed.
    let mut offset = issue(1);
    offset["updated_at"] = json!("2026-01-02T02:00:00+02:00");
    offset["closed_at"] = json!("2026-01-02T00:00:01Z");
    let mut malformed = issue(2);
    malformed["updated_at"] = json!("2026-01-02 00:00:00");
    let mut pre_epoch = issue(3);
    pre_epoch["updated_at"] = json!("1969-12-31T23:59:59Z");
    let client = GitHubClient::new(
        scope()?,
        Fake::new(vec![
            Ok(json!([offset])),
            Ok(json!([malformed])),
            Ok(json!([pre_epoch])),
        ])?,
        ReadLimits::default(),
    );
    let (house, repo) = (HouseId::new("sample")?, Repository::new("sample/project")?);
    let Observation::Known(found) = client.issues(&house, &repo) else {
        return Err("expected offset timestamp to parse".into());
    };
    assert_eq!(found[0].updated_at.as_unix_millis(), 1_767_312_000_000);
    assert_eq!(
        found[0].closed_at.map(|date| date.as_unix_millis()),
        Some(1_767_312_001_000)
    );
    assert!(!matches!(
        client.issues(&house, &repo),
        Observation::Known(_)
    ));
    assert!(!matches!(
        client.issues(&house, &repo),
        Observation::Known(_)
    ));
    // An unknown state filter is refused before any request.
    let before = client.transport().requests.borrow().len();
    assert_eq!(
        client.issues_filtered(&house, &repo, Some(IssueState::Unknown), None),
        Observation::Unavailable(IntegrationError::InvalidInput)
    );
    assert_eq!(client.transport().requests.borrow().len(), before);
    Ok(())
}
