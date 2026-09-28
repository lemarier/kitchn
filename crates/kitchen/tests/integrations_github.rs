//! Sanitized offline GitHub boundary regression tests.
use kitchen::{
    HouseId,
    contracts::{ExternalRef, Permission, Repository},
    integrations::github::*,
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::VecDeque, time::Duration};
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
        CredentialRef::new(house, ExternalRef::new("github-read")?, requester),
        PostingBudget::new(2)?,
        [Permission::PostComment],
    )?)
}
fn issue(number: u64) -> Value {
    json!({"id":number,"number":number,"title":"sanitized issue","state":"open","assignees":[],"labels":[]})
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
        Err(IntegrationError::PermissionDenied)
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
    assert_eq!(bad.validate(), Err(IntegrationError::InvalidInput));
    Ok(())
}

#[cfg(unix)]
fn fake_cli(script: &str) -> Result<(tempfile::TempDir, std::path::PathBuf, CredentialFile)> {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir()?;
    let executable = root.path().join("fake-gh");
    std::fs::write(&executable, format!("#!/bin/sh\n{script}\n"))?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
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
if [ "$4" = user ]; then
  printf '%s' '{"login":"sample-bot"}'
else
  printf '%s' '{"id":1,"number":1,"title":"sample","state":"open","assignees":[],"labels":[]}'
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
    for (script, expected) in [
        ("while :; do :; done", IntegrationError::Timeout),
        (
            "exec /usr/bin/yes sanitized-output",
            IntegrationError::LimitExceeded,
        ),
        ("exit 1", IntegrationError::Unavailable),
    ] {
        let (_root, executable, credential) = fake_cli(script)?;
        let client = GitHubClient::new(
            scope()?,
            GhCli::new(executable, credential)?,
            ReadLimits::new(Duration::from_secs(1), 1, 65536)?,
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
            json!([{"id":1,"user":{"login":"reviewer"},"commit_id":"b".repeat(40),"state":"APPROVED"}]),
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
    ] {
        assert_eq!(
            GitHubMutation {
                repository: repo.clone(),
                action
            }
            .validate(),
            Err(IntegrationError::InvalidInput)
        );
    }
    Ok(())
}
