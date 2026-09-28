//! Report intake policy with a fake connector. All tests are simulated: no
//! external service, credential, or forge is contacted.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

use kitchen::{
    BackendId, CredentialId, ErrorClass, HouseId,
    contracts::{
        ExternalRef, GitHubAction, Grant, HouseGrants, IssueNumber, Permission, Repository,
        TaskAuthority, Text, Timestamp,
    },
    integrations::github::IssueState,
    workflows::intake::{
        Classified, ConnectorFailure, FetchRequest, IntakeError, IntakeSource, IntakeSources,
        KnownIssue, MAX_LISTED, PostingAuthority, PrivacyClass, ProblemKey, Proposal, RawReport,
        ReadScope, Report, ReportConnector, ReportLink, SourceId, marker, plan, problem_marker,
    },
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PRIVATE_TEXT: &str = "my account alice@example.com cannot log in";
const PRIVATE_REPORTER: &str = "customer-4471";

/// One fetch as the fake connector saw it.
struct Seen {
    house: HouseId,
    source: SourceId,
    credential: CredentialId,
    channels: Vec<ExternalRef>,
    limit: usize,
}

/// Fake connector: returns a scripted result and records what it was asked.
struct FakeConnector {
    result: Result<Vec<RawReport>, ConnectorFailure>,
    requests: RefCell<Vec<Seen>>,
}

impl FakeConnector {
    fn new(result: Result<Vec<RawReport>, ConnectorFailure>) -> Self {
        Self {
            result,
            requests: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.requests.borrow().len()
    }
}

impl ReportConnector for FakeConnector {
    fn fetch(&self, request: &FetchRequest<'_>) -> Result<Vec<RawReport>, ConnectorFailure> {
        self.requests.borrow_mut().push(Seen {
            house: request.house.clone(),
            source: request.source.clone(),
            credential: request.credential.clone(),
            channels: request.scope.channels().cloned().collect(),
            limit: request.limit,
        });
        self.result.clone()
    }
}

fn house() -> Result<HouseId, Box<dyn std::error::Error>> {
    Ok(HouseId::new("home")?)
}

fn repo() -> Result<Repository, Box<dyn std::error::Error>> {
    Ok(Repository::new("lemarier/kitchen")?)
}

fn backend() -> Result<BackendId, Box<dyn std::error::Error>> {
    Ok(BackendId::new("github")?)
}

fn source(
    id: &str,
    channel: &str,
    privacy: PrivacyClass,
) -> Result<IntakeSource, Box<dyn std::error::Error>> {
    Ok(IntakeSource {
        id: SourceId::new(id)?,
        credential: CredentialId::new(&format!("{id}-token"))?,
        scope: ReadScope::new([ExternalRef::new(channel)?])?,
        privacy,
    })
}

fn sources() -> Result<IntakeSources, Box<dyn std::error::Error>> {
    Ok(IntakeSources::new(
        house()?,
        [
            source("forum", "bugs", PrivacyClass::Public)?,
            source("social", "mentions", PrivacyClass::LinkOnly)?,
            source("support", "inbox", PrivacyClass::Private)?,
        ],
    )?)
}

fn raw(
    id: &str,
    channel: &str,
    reporter: &str,
    text: &str,
) -> Result<RawReport, Box<dyn std::error::Error>> {
    Ok(RawReport {
        id: ExternalRef::new(id)?,
        channel: ExternalRef::new(channel)?,
        link: Some(ReportLink::new(&format!("https://example.test/{id}"))?),
        reporter: ExternalRef::new(reporter)?,
        text: Text::new(text)?,
        received_at: Timestamp::from_unix_millis(1_000),
    })
}

fn classified(report: Report, problem: &str) -> Result<Classified, Box<dyn std::error::Error>> {
    Ok(Classified {
        report,
        problem: ProblemKey::new(problem)?,
    })
}

fn issue(number: u64) -> Result<IssueNumber, Box<dyn std::error::Error>> {
    Ok(IssueNumber::new(number)?)
}

fn full_authority() -> Result<PostingAuthority, Box<dyn std::error::Error>> {
    authority_with(&[Permission::CreateIssue, Permission::PostComment])
}

fn authority_with(
    permissions: &[Permission],
) -> Result<PostingAuthority, Box<dyn std::error::Error>> {
    let grants: Vec<Grant> = permissions
        .iter()
        .map(|permission| -> Result<Grant, Box<dyn std::error::Error>> {
            Ok(Grant::repository(
                *permission,
                repo()?,
                backend()?,
                CredentialId::new("forge")?,
            ))
        })
        .collect::<Result<_, _>>()?;
    let house_grants = HouseGrants::new(house()?, grants.clone());
    let task = TaskAuthority::delegate(&house_grants, grants)?;
    Ok(PostingAuthority::from_task(
        &task,
        &house_grants,
        &repo()?,
        &backend()?,
    )?)
}

fn body(proposal: &Proposal) -> Option<String> {
    match &proposal.mutation()?.action {
        GitHubAction::PostComment { body, .. } | GitHubAction::CreateIssue { body, .. } => {
            Some(body.as_str().to_owned())
        }
        _ => None,
    }
}

#[test]
fn undeclared_source_is_refused_before_the_connector_runs() -> TestResult {
    let sources = sources()?;
    let connector = FakeConnector::new(Ok(vec![raw("m1", "bugs", "ann", "broken")?]));
    let error = sources
        .collect(&SourceId::new("chat")?, &connector, 10)
        .err();
    assert_eq!(error, Some(IntakeError::UndeclaredSource));
    assert_eq!(connector.calls(), 0);
    assert_eq!(
        sources
            .accept(&SourceId::new("chat")?, raw("m1", "bugs", "ann", "broken")?)
            .err(),
        Some(IntakeError::UndeclaredSource)
    );
    assert_eq!(
        kitchen::Error::from(IntakeError::UndeclaredSource).class(),
        ErrorClass::Refused
    );
    Ok(())
}

#[test]
fn declared_source_reads_with_its_credential_and_scope() -> TestResult {
    let sources = sources()?;
    let connector = FakeConnector::new(Ok(vec![
        raw("m1", "bugs", "ann", "login times out")?,
        raw("m2", "bugs", "bob", "login hangs")?,
    ]));
    let reports = sources.collect(&SourceId::new("forum")?, &connector, 10)?;
    assert_eq!(reports.len(), 2);
    let home = house()?;
    assert!(
        reports
            .iter()
            .all(|report| report.house() == &home && report.privacy() == PrivacyClass::Public)
    );
    let requests = connector.requests.borrow();
    let seen = requests.first().ok_or("connector not called")?;
    assert_eq!(seen.house, house()?);
    assert_eq!(seen.source.as_str(), "forum");
    assert_eq!(seen.credential.as_str(), "forum-token");
    assert_eq!(seen.channels, vec![ExternalRef::new("bugs")?]);
    assert_eq!(seen.limit, 10);
    Ok(())
}

#[test]
fn connector_results_outside_bounds_or_scope_are_refused_whole() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    let out_of_scope = FakeConnector::new(Ok(vec![
        raw("m1", "bugs", "ann", "ok")?,
        raw("m2", "private-dm", "bob", "outside scope")?,
    ]));
    assert_eq!(
        sources.collect(&forum, &out_of_scope, 10).err(),
        Some(IntakeError::OutOfScope)
    );
    let oversized = FakeConnector::new(Ok(vec![
        raw("m1", "bugs", "ann", "a")?,
        raw("m2", "bugs", "bob", "b")?,
    ]));
    assert_eq!(
        sources.collect(&forum, &oversized, 1).err(),
        Some(IntakeError::InvalidReport)
    );
    let empty = FakeConnector::new(Ok(Vec::new()));
    assert_eq!(
        sources.collect(&forum, &empty, 0).err(),
        Some(IntakeError::InvalidReport)
    );
    assert_eq!(empty.calls(), 0);
    assert!(sources.collect(&forum, &empty, 500)?.is_empty());
    Ok(())
}

#[test]
fn connector_failures_are_errors_not_empty_batches() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    for (failure, expected, class) in [
        (
            ConnectorFailure::Unavailable,
            IntakeError::SourceUnavailable,
            ErrorClass::Execution,
        ),
        (
            ConnectorFailure::Unauthorized,
            IntakeError::SourceUnauthorized,
            ErrorClass::Refused,
        ),
        (
            ConnectorFailure::Malformed,
            IntakeError::InvalidReport,
            ErrorClass::InvalidInput,
        ),
    ] {
        let connector = FakeConnector::new(Err(failure));
        let error = sources.collect(&forum, &connector, 10).err();
        assert_eq!(error, Some(expected));
        assert_eq!(expected.class(), class);
    }
    Ok(())
}

#[test]
fn declarations_are_validated() -> TestResult {
    assert_eq!(
        IntakeSources::new(
            house()?,
            [
                source("forum", "bugs", PrivacyClass::Public)?,
                source("forum", "other", PrivacyClass::Private)?,
            ],
        )
        .err(),
        Some(IntakeError::InvalidDeclaration)
    );
    let too_many = (0..=32)
        .map(|n| source(&format!("s{n}"), "c", PrivacyClass::Private))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        IntakeSources::new(house()?, too_many).err(),
        Some(IntakeError::InvalidDeclaration)
    );
    assert_eq!(
        ReadScope::new(Vec::new()).err(),
        Some(IntakeError::InvalidDeclaration)
    );
    for bad in ["", "Forum", "-forum", "has space", &"a".repeat(49)] {
        assert_eq!(
            SourceId::new(bad).err(),
            Some(IntakeError::InvalidDeclaration)
        );
    }
    assert!(SourceId::new(&"a".repeat(48)).is_ok());
    for bad in [
        "http://example.test/x",
        "https://",
        "https://x/<script>",
        "javascript:x",
    ] {
        assert_eq!(ReportLink::new(bad).err(), Some(IntakeError::InvalidReport));
    }
    Ok(())
}

#[test]
fn duplicate_reports_become_one_draft_proposal() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    let first = sources.accept(&forum, raw("m1", "bugs", "ann", "login times out")?)?;
    let redelivered = sources.accept(&forum, raw("m1", "bugs", "ann", "login times out")?)?;
    let second = sources.accept(&forum, raw("m2", "bugs", "bob", "login hangs")?)?;
    let other = sources.accept(&forum, raw("m3", "bugs", "cy", "dark mode")?)?;
    let reports = [
        classified(first, "login-timeout")?,
        classified(redelivered, "login-timeout")?,
        classified(second, "login-timeout")?,
        classified(other, "dark-mode")?,
    ];
    let proposals = plan(
        &house()?,
        &repo()?,
        &reports,
        &BTreeMap::new(),
        &full_authority()?,
    )?;
    assert_eq!(proposals.len(), 2);
    let login = proposals
        .iter()
        .find(|p| matches!(p, Proposal::DraftIssue { problem, .. } if problem.as_str() == "login-timeout"))
        .ok_or("missing login draft")?;
    let Proposal::DraftIssue {
        reports, mutation, ..
    } = login
    else {
        return Err("not a draft".into());
    };
    assert_eq!(reports.len(), 2);
    assert_eq!(mutation.repository, repo()?);
    let GitHubAction::CreateIssue { title, body } = &mutation.action else {
        return Err("not an issue".into());
    };
    assert_eq!(title.as_str(), "Intake: login-timeout");
    assert!(
        body.as_str()
            .starts_with(&marker(&ProblemKey::new("login-timeout")?))
    );
    assert!(body.as_str().contains("Draft from 2 external reports."));
    assert!(body.as_str().contains("<https://example.test/m1>"));
    assert!(body.as_str().contains("<https://example.test/m2>"));
    assert_eq!(
        problem_marker(body.as_str()),
        Some(ProblemKey::new("login-timeout")?)
    );
    assert_eq!(
        login.effect_name().map(|n| n.as_str().to_owned()),
        Some("intake-draft-login-timeout".to_owned())
    );

    // The longest problem key still yields a valid effect name.
    let longest = "a".repeat(48);
    let report = sources.accept(&forum, raw("m4", "bugs", "dee", "x")?)?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &[classified(report, &longest)?],
        &BTreeMap::new(),
        &full_authority()?,
    )?;
    assert!(proposals.first().and_then(Proposal::effect_name).is_some());
    Ok(())
}

#[test]
fn reports_matching_an_open_issue_add_a_count_instead_of_a_duplicate() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    let counted = sources.accept(&forum, raw("m1", "bugs", "ann", "login times out")?)?;
    let new = sources.accept(&forum, raw("m2", "bugs", "bob", "login hangs")?)?;
    let known = BTreeMap::from([(
        ProblemKey::new("login-timeout")?,
        KnownIssue {
            number: issue(12)?,
            state: IssueState::Open,
            counted: BTreeSet::from([counted.key()]),
        },
    )]);
    let reports = [
        classified(counted.clone(), "login-timeout")?,
        classified(new.clone(), "login-timeout")?,
    ];
    let proposals = plan(&house()?, &repo()?, &reports, &known, &full_authority()?)?;
    let [
        Proposal::AddReports {
            issue: target,
            reports: added,
            mutation,
            ..
        },
    ] = proposals.as_slice()
    else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    assert_eq!(*target, issue(12)?);
    assert_eq!(added, &vec![new.key()]);
    let GitHubAction::PostComment { issue: on, body } = &mutation.action else {
        return Err("not a comment".into());
    };
    assert_eq!(*on, issue(12)?);
    assert!(
        body.as_str()
            .contains("1 new external report for this problem; 2 counted in total.")
    );
    assert!(body.as_str().contains("<https://example.test/m2>"));
    assert!(!body.as_str().contains("<https://example.test/m1>"));
    let name = proposals.first().and_then(Proposal::effect_name);
    assert_eq!(name.as_ref().map(|n| n.as_str()), Some("intake-12-2"));

    // A rerun before the comment is recorded names the same effect, so an
    // uncertain submission is reconciled instead of posted twice.
    let retry = plan(&house()?, &repo()?, &reports, &known, &full_authority()?)?;
    assert_eq!(retry.first().and_then(Proposal::effect_name), name);

    // A rerun after the comment is recorded proposes nothing.
    let mut recorded = known;
    if let Some(entry) = recorded.get_mut(&ProblemKey::new("login-timeout")?) {
        entry.counted.insert(new.key());
    }
    assert!(plan(&house()?, &repo()?, &reports, &recorded, &full_authority()?)?.is_empty());
    Ok(())
}

#[test]
fn a_report_matching_a_closed_issue_is_held_for_review() -> TestResult {
    let sources = sources()?;
    let report = sources.accept(&SourceId::new("forum")?, raw("m9", "bugs", "ann", "again")?)?;
    let key = report.key();
    let known = BTreeMap::from([(
        ProblemKey::new("login-timeout")?,
        KnownIssue {
            number: issue(7)?,
            state: IssueState::Closed,
            counted: BTreeSet::new(),
        },
    )]);
    let proposals = plan(
        &house()?,
        &repo()?,
        &[classified(report, "login-timeout")?],
        &known,
        &full_authority()?,
    )?;
    assert_eq!(
        proposals,
        vec![Proposal::ClosedMatch {
            problem: ProblemKey::new("login-timeout")?,
            issue: issue(7)?,
            reports: vec![key],
        }]
    );
    assert!(proposals.iter().all(|p| p.mutation().is_none()));
    assert!(proposals.iter().all(|p| p.effect_name().is_none()));
    Ok(())
}

#[test]
fn unknown_issue_state_is_incomplete_evidence() -> TestResult {
    let sources = sources()?;
    let report = sources.accept(&SourceId::new("forum")?, raw("m9", "bugs", "ann", "x")?)?;
    let known = BTreeMap::from([(
        ProblemKey::new("login-timeout")?,
        KnownIssue {
            number: issue(7)?,
            state: IssueState::Unknown,
            counted: BTreeSet::new(),
        },
    )]);
    let error = plan(
        &house()?,
        &repo()?,
        &[classified(report, "login-timeout")?],
        &known,
        &full_authority()?,
    )
    .err();
    assert_eq!(error, Some(IntakeError::IncompleteEvidence));
    Ok(())
}

#[test]
fn private_content_is_redacted_by_privacy_class() -> TestResult {
    let sources = sources()?;
    let public = sources.accept(
        &SourceId::new("forum")?,
        raw("p1", "bugs", "ann", "public text with ``` fence")?,
    )?;
    let link_only = sources.accept(
        &SourceId::new("social")?,
        raw("l1", "mentions", "handle-77", "social post text")?,
    )?;
    let private_one = sources.accept(
        &SourceId::new("support")?,
        raw("s1", "inbox", PRIVATE_REPORTER, PRIVATE_TEXT)?,
    )?;
    let private_two = sources.accept(
        &SourceId::new("support")?,
        raw("s2", "inbox", "customer-9", "second private message")?,
    )?;
    let reports = [
        classified(public, "login-timeout")?,
        classified(link_only, "login-timeout")?,
        classified(private_one, "login-timeout")?,
        classified(private_two, "login-timeout")?,
    ];
    let proposals = plan(
        &house()?,
        &repo()?,
        &reports,
        &BTreeMap::new(),
        &full_authority()?,
    )?;
    let text = proposals.first().and_then(body).ok_or("no body")?;

    assert!(text.contains("Draft from 4 external reports."));
    // Public: link, reporter, and quote in a fence longer than any run inside.
    assert!(text.contains("`forum`: <https://example.test/p1> from `ann`"));
    assert!(text.contains("````text\n  public text with ``` fence\n  ````"));
    // Link only: link without reporter or text.
    assert!(text.contains("`social`: <https://example.test/l1>\n"));
    assert!(!text.contains("handle-77"));
    assert!(!text.contains("social post text"));
    // Private: a count per source only.
    assert!(text.contains("`support`: 2 private reports"));
    for secret in [
        PRIVATE_TEXT,
        PRIVATE_REPORTER,
        "customer-9",
        "second private message",
        "https://example.test/s",
    ] {
        assert!(!text.contains(secret), "leaked private content");
    }
    // Debug output of a report hides the reporter and text too.
    let debug = format!("{reports:?}");
    assert!(!debug.contains(PRIVATE_TEXT) && !debug.contains(PRIVATE_REPORTER));
    Ok(())
}

#[test]
fn listing_is_bounded_and_the_rest_counted() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    let reports = (0..MAX_LISTED + 3)
        .map(|n| -> Result<Classified, Box<dyn std::error::Error>> {
            let report = sources.accept(&forum, raw(&format!("m{n}"), "bugs", "ann", "x")?)?;
            classified(report, "login-timeout")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &reports,
        &BTreeMap::new(),
        &full_authority()?,
    )?;
    let text = proposals.first().and_then(body).ok_or("no body")?;
    assert_eq!(text.matches("<https://example.test/").count(), MAX_LISTED);
    assert!(text.contains("- 3 more not listed"));

    let over = (0..=500)
        .map(|n| -> Result<Classified, Box<dyn std::error::Error>> {
            let report = sources.accept(&forum, raw(&format!("o{n}"), "bugs", "ann", "x")?)?;
            classified(report, "login-timeout")
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        plan(
            &house()?,
            &repo()?,
            &over,
            &BTreeMap::new(),
            &full_authority()?
        )
        .err(),
        Some(IntakeError::InvalidReport)
    );
    Ok(())
}

#[test]
fn missing_posting_grant_posts_nothing() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    let open_match = sources.accept(&forum, raw("m1", "bugs", "ann", "a")?)?;
    let new_problem = sources.accept(&forum, raw("m2", "bugs", "bob", "b")?)?;
    let known = BTreeMap::from([(
        ProblemKey::new("login-timeout")?,
        KnownIssue {
            number: issue(12)?,
            state: IssueState::Open,
            counted: BTreeSet::new(),
        },
    )]);
    let reports = [
        classified(open_match, "login-timeout")?,
        classified(new_problem, "dark-mode")?,
    ];

    let unrelated = authority_with(&[Permission::EditLabels])?;
    let proposals = plan(&house()?, &repo()?, &reports, &known, &unrelated)?;
    assert_eq!(proposals.len(), 2);
    assert!(proposals.iter().all(|p| p.mutation().is_none()));
    let missing: BTreeSet<Permission> = proposals
        .iter()
        .filter_map(|p| match p {
            Proposal::MissingGrant { permission, .. } => Some(*permission),
            _ => None,
        })
        .collect();
    assert_eq!(
        missing,
        BTreeSet::from([Permission::CreateIssue, Permission::PostComment])
    );

    // Report-only intake behaves the same.
    let none = PostingAuthority::none(repo()?);
    assert!(
        plan(&house()?, &repo()?, &reports, &known, &none)?
            .iter()
            .all(|p| matches!(p, Proposal::MissingGrant { .. }))
    );

    // Comment-only authority comments but still cannot create issues.
    let comment_only = authority_with(&[Permission::PostComment])?;
    let proposals = plan(&house()?, &repo()?, &reports, &known, &comment_only)?;
    assert!(
        proposals
            .iter()
            .any(|p| matches!(p, Proposal::AddReports { .. }))
    );
    assert!(proposals.iter().any(|p| matches!(
        p,
        Proposal::MissingGrant {
            permission: Permission::CreateIssue,
            ..
        }
    )));
    Ok(())
}

#[test]
fn revoked_or_foreign_authority_is_refused() -> TestResult {
    let grant = Grant::repository(
        Permission::CreateIssue,
        repo()?,
        backend()?,
        CredentialId::new("forge")?,
    );
    let granted = HouseGrants::new(house()?, [grant.clone()]);
    let task = TaskAuthority::delegate(&granted, [grant])?;

    let revoked = HouseGrants::new(house()?, []);
    assert_eq!(
        PostingAuthority::from_task(&task, &revoked, &repo()?, &backend()?).err(),
        Some(IntakeError::Authority)
    );
    let foreign = HouseGrants::new(HouseId::new("elsewhere")?, []);
    assert_eq!(
        PostingAuthority::from_task(&task, &foreign, &repo()?, &backend()?).err(),
        Some(IntakeError::Authority)
    );

    let other_repo = PostingAuthority::none(Repository::new("lemarier/other")?);
    assert_eq!(
        plan(&house()?, &repo()?, &[], &BTreeMap::new(), &other_repo).err(),
        Some(IntakeError::Authority)
    );
    Ok(())
}

#[test]
fn cross_house_and_conflicting_reports_are_refused() -> TestResult {
    let home = sources()?;
    let elsewhere = IntakeSources::new(
        HouseId::new("elsewhere")?,
        [source("forum", "bugs", PrivacyClass::Public)?],
    )?;
    let forum = SourceId::new("forum")?;
    let foreign = elsewhere.accept(&forum, raw("m1", "bugs", "ann", "x")?)?;
    assert_eq!(
        plan(
            &house()?,
            &repo()?,
            &[classified(foreign, "login-timeout")?],
            &BTreeMap::new(),
            &full_authority()?,
        )
        .err(),
        Some(IntakeError::CrossHouse)
    );

    let report = home.accept(&forum, raw("m1", "bugs", "ann", "x")?)?;
    let conflicting = [
        classified(report.clone(), "login-timeout")?,
        classified(report, "dark-mode")?,
    ];
    assert_eq!(
        plan(
            &house()?,
            &repo()?,
            &conflicting,
            &BTreeMap::new(),
            &full_authority()?
        )
        .err(),
        Some(IntakeError::ConflictingClassification)
    );
    Ok(())
}

#[test]
fn problem_markers_round_trip_and_ignore_malformed_input() -> TestResult {
    let key = ProblemKey::new("login-timeout")?;
    assert_eq!(
        problem_marker(&format!("intro\n{}\nrest", marker(&key))),
        Some(key)
    );
    assert_eq!(problem_marker("no marker here"), None);
    assert_eq!(problem_marker("<!-- kitchen-intake:Bad Key -->"), None);
    assert_eq!(problem_marker("<!-- kitchen-intake:unterminated"), None);
    Ok(())
}
