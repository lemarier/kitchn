//! Report intake policy with a fake connector. All tests are simulated: no
//! external service, credential, or forge is contacted.

mod common;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
};

use common::Fixture;

use kitchen::{
    BackendId, CredentialId, ErrorClass, HouseId, TaskId, WorkflowId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Effect, EffectExecutor,
        EffectFailure, EffectRequest, EvidenceRevision, ExternalRef, Fence, GitHubAction,
        GitHubEffect, GitHubMutation, Grant, HouseGrants, IssueNumber, Lookup, NotAppliedReason,
        Permission, PostingBudget, Receipt, Repository, TaskAuthority, Text, Timestamp,
        UncertainReason,
    },
    integrations::github::IssueState,
    state::{
        EffectPlan, EffectRecord, EffectState, HouseStore, StateError, StoreOptions, reconcile,
        run_effect,
    },
    workflows::intake::{
        Classified, ConnectorFailure, Counted, FetchRequest, IntakeError, IntakeLedger,
        IntakeSource, IntakeSources, KnownIssue, MAX_LISTED, MAX_PER_PROPOSAL, PostingAuthority,
        PrivacyClass, ProblemKey, Proposal, RawReport, ReadScope, Report, ReportConnector,
        ReportLink, SourceId, marker, plan, problem_marker,
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

fn uncounted() -> Result<Counted, Box<dyn std::error::Error>> {
    Ok(Counted::none(house()?, repo()?))
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
        &uncounted()?,
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
    let name = login.effect_name().ok_or("no effect name")?;
    assert!(name.as_str().starts_with("intake-draft-"));
    assert_eq!(name.as_str().len(), "intake-draft-".len() + 32);

    // The longest problem key still yields a valid effect name.
    let longest = "a".repeat(48);
    let report = sources.accept(&forum, raw("m4", "bugs", "dee", "x")?)?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &[classified(report, &longest)?],
        &BTreeMap::new(),
        &uncounted()?,
        &full_authority()?,
    )?;
    assert!(proposals.first().and_then(Proposal::effect_name).is_some());
    Ok(())
}

#[test]
fn reports_matching_an_open_issue_add_a_count_instead_of_a_duplicate() -> TestResult {
    let sources = sources()?;
    let forum = SourceId::new("forum")?;
    let first = sources.accept(&forum, raw("m1", "bugs", "ann", "login times out")?)?;
    let new = sources.accept(&forum, raw("m2", "bugs", "bob", "login hangs")?)?;
    let known = BTreeMap::from([(
        ProblemKey::new("login-timeout")?,
        KnownIssue {
            number: issue(12)?,
            state: IssueState::Open,
        },
    )]);
    let reports = [
        classified(first.clone(), "login-timeout")?,
        classified(new.clone(), "login-timeout")?,
        classified(new.clone(), "login-timeout")?,
    ];
    let proposals = plan(
        &house()?,
        &repo()?,
        &reports,
        &known,
        &uncounted()?,
        &full_authority()?,
    )?;
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
    assert_eq!(added, &vec![first.key(), new.key()]);
    let GitHubAction::PostComment { issue: on, body } = &mutation.action else {
        return Err("not a comment".into());
    };
    assert_eq!(*on, issue(12)?);
    assert!(
        body.as_str()
            .contains("2 new external reports for this problem; 2 counted in total.")
    );
    assert!(body.as_str().contains("<https://example.test/m1>"));
    assert!(body.as_str().contains("<https://example.test/m2>"));
    let name = proposals.first().and_then(Proposal::effect_name);
    assert!(
        name.as_ref()
            .is_some_and(|n| n.as_str().starts_with("intake-comment-"))
    );

    // A rerun with the same reports names the same effect, so an uncertain
    // submission is reconciled instead of posted twice.
    let retry = plan(
        &house()?,
        &repo()?,
        &reports,
        &known,
        &uncounted()?,
        &full_authority()?,
    )?;
    assert_eq!(retry.first().and_then(Proposal::effect_name), name);

    // Another batch with the same count is another mutation and another
    // name; before the fix both were `intake-12-2`.
    let other = sources.accept(&forum, raw("m3", "bugs", "cy", "login stalls")?)?;
    let changed = [
        classified(first, "login-timeout")?,
        classified(other, "login-timeout")?,
    ];
    let changed = plan(
        &house()?,
        &repo()?,
        &changed,
        &known,
        &uncounted()?,
        &full_authority()?,
    )?;
    assert_ne!(
        changed.first().and_then(Proposal::mutation),
        proposals.first().and_then(Proposal::mutation)
    );
    assert_ne!(changed.first().and_then(Proposal::effect_name), name);
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
        },
    )]);
    let proposals = plan(
        &house()?,
        &repo()?,
        &[classified(report, "login-timeout")?],
        &known,
        &uncounted()?,
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
        },
    )]);
    let error = plan(
        &house()?,
        &repo()?,
        &[classified(report, "login-timeout")?],
        &known,
        &uncounted()?,
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
        &uncounted()?,
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
        &uncounted()?,
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
            &uncounted()?,
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
        },
    )]);
    let reports = [
        classified(open_match, "login-timeout")?,
        classified(new_problem, "dark-mode")?,
    ];

    let unrelated = authority_with(&[Permission::EditLabels])?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &reports,
        &known,
        &uncounted()?,
        &unrelated,
    )?;
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
        plan(&house()?, &repo()?, &reports, &known, &uncounted()?, &none)?
            .iter()
            .all(|p| matches!(p, Proposal::MissingGrant { .. }))
    );

    // Comment-only authority comments but still cannot create issues.
    let comment_only = authority_with(&[Permission::PostComment])?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &reports,
        &known,
        &uncounted()?,
        &comment_only,
    )?;
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
        plan(
            &house()?,
            &repo()?,
            &[],
            &BTreeMap::new(),
            &uncounted()?,
            &other_repo
        )
        .err(),
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
            &uncounted()?,
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
            &uncounted()?,
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

// Durable counting through the house store and effect lifecycle. The forge
// is an in-memory fake; nothing is posted anywhere.

/// A fake forge that records applied mutations and can lose one response.
struct Forge {
    descriptor: BackendDescriptor,
    posted: RefCell<Vec<GitHubMutation>>,
    lose_next_response: Cell<bool>,
}

impl Forge {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            descriptor: BackendDescriptor {
                backend: backend()?,
                house: house()?,
                worker_selection: None,
                capabilities: CapabilitySet::supporting(Capability::ALL),
            },
            posted: RefCell::new(Vec::new()),
            lose_next_response: Cell::new(false),
        })
    }

    fn posts(&self) -> usize {
        self.posted.borrow().len()
    }
}

fn mutation_of(request: &EffectRequest) -> Option<&GitHubMutation> {
    match request.effect() {
        Effect::GitHub(effect) => Some(&effect.mutation),
        _ => None,
    }
}

fn receipt() -> Option<Receipt> {
    let reference = ExternalRef::new("forge-receipt").ok()?;
    Receipt::new(reference, Vec::new(), Vec::new()).ok()
}

impl EffectExecutor for Forge {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let mutation =
            mutation_of(request).ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
        self.posted.borrow_mut().push(mutation.clone());
        if self.lose_next_response.replace(false) {
            return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
        }
        receipt().ok_or(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let posted =
            mutation_of(request).is_some_and(|mutation| self.posted.borrow().contains(mutation));
        Ok(match (posted, receipt()) {
            (true, Some(receipt)) => Lookup::Applied(receipt),
            (true, None) => Lookup::Unknown,
            (false, _) => Lookup::Absent,
        })
    }
}

/// A house store with one claimed intake task holding forge grants.
struct Kitchen {
    fixture: Fixture,
    grants: HouseGrants,
}

impl Kitchen {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store =
            HouseStore::initialize(dir.path().join("house"), house()?, StoreOptions::default())?;
        let grants = HouseGrants::new(house()?, forge_grants()?);
        Ok(Self {
            fixture: Fixture { dir, store },
            grants,
        })
    }

    fn store(&self) -> &HouseStore {
        &self.fixture.store
    }

    /// Another handle on the same directory, as after a process restart.
    fn reopen(&self) -> Result<HouseStore, Box<dyn std::error::Error>> {
        Ok(HouseStore::open(
            self.fixture.dir.path().join("house"),
            house()?,
            StoreOptions::default(),
        )?)
    }

    fn start(&self, id: &str) -> Result<(TaskId, Fence), Box<dyn std::error::Error>> {
        let mut spec = common::spec(id)?;
        spec.repository = Some(repo()?);
        spec.authority = TaskAuthority::delegate(&self.grants, forge_grants()?)?;
        self.store()
            .create_task(spec, &common::creator()?, common::at(0))?;
        let task = TaskId::new(id)?;
        let fence = self
            .store()
            .claim(
                &task,
                &common::scheduled(id)?,
                common::ttl(600)?,
                common::at(0),
            )?
            .fence();
        self.store().start_attempt(&task, fence, common::at(0))?;
        Ok((task, fence))
    }

    fn ledger<'a>(
        &self,
        store: &'a HouseStore,
    ) -> Result<IntakeLedger<'a>, Box<dyn std::error::Error>> {
        Ok(IntakeLedger::new(
            store,
            WorkflowId::new("intake")?,
            repo()?,
        ))
    }

    /// Reserve `proposal` and submit it through the effect store.
    fn submit(
        &self,
        forge: &Forge,
        counted: &Counted,
        proposal: &Proposal,
        task: &TaskId,
        fence: Fence,
    ) -> Result<EffectRecord, Box<dyn std::error::Error>> {
        let name = self.ledger(self.store())?.reserve(
            counted,
            proposal,
            task,
            &common::scheduled("intake")?,
            common::at(1),
        )?;
        let mutation = proposal.mutation().ok_or("no mutation")?.clone();
        let effect = GitHubEffect {
            requester: ExternalRef::new("kitchen-bot")?,
            mutation,
            posting_budget: PostingBudget::new(10)?,
        };
        Ok(run_effect(
            self.store(),
            forge,
            &self.grants,
            EffectPlan {
                task: task.clone(),
                fence,
                name,
                decided_at: EvidenceRevision::INITIAL,
                effect: Effect::GitHub(effect),
                consent: None,
                basis: None,
            },
            &common::ManualClock::starting_at(1),
        )?)
    }
}

fn forge_grants() -> Result<Vec<Grant>, Box<dyn std::error::Error>> {
    [Permission::CreateIssue, Permission::PostComment]
        .into_iter()
        .map(|permission| -> Result<Grant, Box<dyn std::error::Error>> {
            Ok(Grant::repository(
                permission,
                repo()?,
                backend()?,
                CredentialId::new("forge")?,
            ))
        })
        .collect()
}

fn open_issue(number: u64) -> Result<BTreeMap<ProblemKey, KnownIssue>, Box<dyn std::error::Error>> {
    Ok(BTreeMap::from([(
        ProblemKey::new("login-timeout")?,
        KnownIssue {
            number: issue(number)?,
            state: IssueState::Open,
        },
    )]))
}

fn login(
    sources: &IntakeSources,
    ids: &[&str],
) -> Result<Vec<Classified>, Box<dyn std::error::Error>> {
    ids.iter()
        .map(|id| -> Result<Classified, Box<dyn std::error::Error>> {
            let report = sources.accept(
                &SourceId::new("support")?,
                raw(id, "inbox", PRIVATE_REPORTER, PRIVATE_TEXT)?,
            )?;
            classified(report, "login-timeout")
        })
        .collect()
}

fn applied(record: &EffectRecord) -> bool {
    matches!(record.state(), EffectState::Applied { .. })
}

#[test]
fn counted_reports_survive_a_restart_and_are_not_counted_twice() -> TestResult {
    let kitchen = Kitchen::new()?;
    let forge = Forge::new()?;
    let sources = sources()?;
    let (task, fence) = kitchen.start("intake-1")?;
    let ledger = kitchen.ledger(kitchen.store())?;

    // A new problem becomes a draft, counted once its effect is applied.
    let batch = login(&sources, &["msg-4471-a", "msg-4471-b"])?;
    let counted = ledger.counted(&task)?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &batch,
        &BTreeMap::new(),
        &counted,
        &full_authority()?,
    )?;
    let [draft @ Proposal::DraftIssue { .. }] = proposals.as_slice() else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    assert!(applied(
        &kitchen.submit(&forge, &counted, draft, &task, fence)?
    ));

    // The draft exists but the forge index has not seen it yet: planning
    // refuses rather than drafting the problem twice.
    let counted = ledger.counted(&task)?;
    let more = login(&sources, &["msg-4471-a", "msg-4471-b", "msg-4471-c"])?;
    assert_eq!(
        plan(
            &house()?,
            &repo()?,
            &more,
            &BTreeMap::new(),
            &counted,
            &full_authority()?
        )
        .err(),
        Some(IntakeError::IncompleteEvidence)
    );

    // Once the issue is known, a changed batch after the applied draft adds
    // only the new report, under a new effect name in the same attempt.
    let proposals = plan(
        &house()?,
        &repo()?,
        &more,
        &open_issue(3)?,
        &counted,
        &full_authority()?,
    )?;
    let [comment @ Proposal::AddReports { reports, total, .. }] = proposals.as_slice() else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    assert_eq!(reports.len(), 1);
    assert_eq!(*total, 3);
    assert_ne!(comment.effect_name(), draft.effect_name());
    assert!(applied(
        &kitchen.submit(&forge, &counted, comment, &task, fence)?
    ));
    assert_eq!(forge.posts(), 2);

    // After a restart, another task replaying the same reports proposes
    // nothing, and the next report is counted on top of three.
    let restarted = kitchen.reopen()?;
    kitchen.store().finish_attempt(
        &task,
        fence,
        kitchen::contracts::AttemptNumber::FIRST,
        kitchen::contracts::AttemptOutcome::Succeeded,
        common::at(2),
    )?;
    let (next, _) = kitchen.start("intake-2")?;
    let counted = kitchen.ledger(&restarted)?.counted(&next)?;
    assert!(
        plan(
            &house()?,
            &repo()?,
            &more,
            &open_issue(3)?,
            &counted,
            &full_authority()?
        )?
        .is_empty()
    );
    let latest = login(&sources, &["msg-4471-a", "msg-4471-d"])?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &latest,
        &open_issue(3)?,
        &counted,
        &full_authority()?,
    )?;
    let [Proposal::AddReports { reports, total, .. }] = proposals.as_slice() else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    assert_eq!((reports.len(), *total), (1, 4));

    // House state holds digests, never report ids, reporters, or text.
    let state = std::fs::read_to_string(kitchen.fixture.state_path())?;
    for private in ["msg-4471", PRIVATE_REPORTER, PRIVATE_TEXT] {
        assert!(!state.contains(private), "state leaks {private}");
    }
    Ok(())
}

#[test]
fn a_changed_batch_before_submission_reserves_its_own_effect() -> TestResult {
    let kitchen = Kitchen::new()?;
    let forge = Forge::new()?;
    let sources = sources()?;
    let (task, fence) = kitchen.start("intake-1")?;
    let ledger = kitchen.ledger(kitchen.store())?;
    let counted = ledger.counted(&task)?;
    let first = plan(
        &house()?,
        &repo()?,
        &login(&sources, &["m1"])?,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let first = first.first().ok_or("no proposal")?;
    let reserved = ledger.reserve(
        &counted,
        first,
        &task,
        &common::scheduled("intake")?,
        common::at(1),
    )?;
    // Reserving the same proposal again is a no-op.
    assert_eq!(
        ledger.reserve(
            &counted,
            first,
            &task,
            &common::scheduled("intake")?,
            common::at(1)
        )?,
        reserved
    );

    // Another task cannot plan while the reservation is unapplied.
    assert!(matches!(
        ledger.counted(&TaskId::new("intake-2")?),
        Err(kitchen::Error::Intake(IntakeError::Unreconciled))
    ));

    // The same task replans with a new report before submitting: the batch
    // is a different mutation with a different name, and it applies.
    let counted = ledger.counted(&task)?;
    let changed = plan(
        &house()?,
        &repo()?,
        &login(&sources, &["m1", "m2"])?,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let changed = changed.first().ok_or("no proposal")?;
    assert_ne!(changed.effect_name().as_ref(), Some(&reserved));
    assert!(applied(
        &kitchen.submit(&forge, &counted, changed, &task, fence)?
    ));
    assert_eq!(forge.posts(), 1);
    let counted = ledger.counted(&task)?;
    assert_eq!(counted.total(&ProblemKey::new("login-timeout")?), 2);
    Ok(())
}

#[test]
fn a_changed_batch_after_an_uncertain_submission_waits_for_reconciliation() -> TestResult {
    let kitchen = Kitchen::new()?;
    let forge = Forge::new()?;
    let sources = sources()?;
    let (task, fence) = kitchen.start("intake-1")?;
    let ledger = kitchen.ledger(kitchen.store())?;
    let counted = ledger.counted(&task)?;
    let first = plan(
        &house()?,
        &repo()?,
        &login(&sources, &["m1"])?,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let first = first.first().ok_or("no proposal")?;
    forge.lose_next_response.set(true);
    let uncertain = kitchen.submit(&forge, &counted, first, &task, fence)?;
    assert!(matches!(uncertain.state(), EffectState::Uncertain { .. }));

    // The uncertain report is not counted yet, so the replanned batch holds
    // both reports. It is another effect, and the store refuses to start it
    // while the first is unresolved: no name conflict and no second post.
    let counted = ledger.counted(&task)?;
    let changed = plan(
        &house()?,
        &repo()?,
        &login(&sources, &["m1", "m2"])?,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let changed = changed.first().ok_or("no proposal")?;
    let refused = kitchen
        .submit(&forge, &counted, changed, &task, fence)
        .err();
    assert!(
        refused.as_ref().is_some_and(|error| matches!(
            error.downcast_ref::<kitchen::Error>(),
            Some(kitchen::Error::State(StateError::UnresolvedEffects { .. }))
        )),
        "unexpected: {refused:?}"
    );
    assert_eq!(forge.posts(), 1);
    assert!(matches!(
        ledger.counted(&TaskId::new("intake-2")?),
        Err(kitchen::Error::Intake(IntakeError::Unreconciled))
    ));

    // Reconciling finds the first comment applied; the next plan counts it
    // and proposes only the new report.
    let report = reconcile(
        kitchen.store(),
        &forge,
        &task,
        fence,
        &common::ManualClock::starting_at(2),
    )?;
    assert_eq!(report.resolved.len(), 1);
    let counted = ledger.counted(&task)?;
    let rest = plan(
        &house()?,
        &repo()?,
        &login(&sources, &["m1", "m2"])?,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let [rest @ Proposal::AddReports { reports, total, .. }] = rest.as_slice() else {
        return Err(format!("unexpected proposals: {rest:?}").into());
    };
    assert_eq!((reports.len(), *total), (1, 2));
    assert!(applied(
        &kitchen.submit(&forge, &counted, rest, &task, fence)?
    ));
    assert_eq!(forge.posts(), 2);
    Ok(())
}

#[test]
fn reservations_refuse_stale_foreign_or_mutationless_input() -> TestResult {
    let kitchen = Kitchen::new()?;
    let forge = Forge::new()?;
    let sources = sources()?;
    let (task, fence) = kitchen.start("intake-1")?;
    let ledger = kitchen.ledger(kitchen.store())?;
    let recorder = common::scheduled("intake")?;
    let batch = login(&sources, &["m1"])?;
    let counted = ledger.counted(&task)?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &batch,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let proposal = proposals.first().ok_or("no proposal")?;
    assert!(applied(
        &kitchen.submit(&forge, &counted, proposal, &task, fence)?
    ));

    // State read before that reservation, or fabricated as empty, is stale.
    let another = plan(
        &house()?,
        &repo()?,
        &login(&sources, &["m2"])?,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let another = another.first().ok_or("no proposal")?;
    for stale in [&counted, &uncounted()?] {
        assert!(matches!(
            ledger.reserve(stale, another, &task, &recorder, common::at(1)),
            Err(kitchen::Error::Intake(IntakeError::StaleCount))
        ));
    }

    // Another repository's ledger, or another house's state, is refused.
    let current = ledger.counted(&task)?;
    let elsewhere = IntakeLedger::new(
        kitchen.store(),
        WorkflowId::new("intake")?,
        Repository::new("lemarier/other")?,
    );
    assert!(matches!(
        elsewhere.reserve(&current, another, &task, &recorder, common::at(1)),
        Err(kitchen::Error::Intake(IntakeError::Authority))
    ));
    let foreign = Counted::none(HouseId::new("elsewhere")?, repo()?);
    assert!(matches!(
        ledger.reserve(&foreign, another, &task, &recorder, common::at(1)),
        Err(kitchen::Error::Intake(IntakeError::CrossHouse))
    ));
    assert_eq!(
        plan(
            &house()?,
            &repo()?,
            &batch,
            &open_issue(12)?,
            &foreign,
            &full_authority()?
        )
        .err(),
        Some(IntakeError::CrossHouse)
    );

    // A proposal without a mutation has nothing to reserve.
    let held = Proposal::ClosedMatch {
        problem: ProblemKey::new("login-timeout")?,
        issue: issue(7)?,
        reports: Vec::new(),
    };
    assert!(matches!(
        ledger.reserve(&current, &held, &task, &recorder, common::at(1)),
        Err(kitchen::Error::Intake(IntakeError::InvalidReport))
    ));
    Ok(())
}

#[test]
fn a_full_proposal_fits_one_reservation_and_the_rest_wait() -> TestResult {
    let kitchen = Kitchen::new()?;
    let forge = Forge::new()?;
    let sources = sources()?;
    // The longest task id and problem key give the largest reservation.
    let (task, fence) = kitchen.start(&"t".repeat(64))?;
    let ledger = kitchen.ledger(kitchen.store())?;
    let problem = "p".repeat(48);
    let batch = (0..MAX_PER_PROPOSAL + 5)
        .map(|n| -> Result<Classified, Box<dyn std::error::Error>> {
            let report = sources.accept(
                &SourceId::new("support")?,
                raw(&format!("m{n:03}"), "inbox", "ann", "x")?,
            )?;
            classified(report, &problem)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let counted = ledger.counted(&task)?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &batch,
        &BTreeMap::new(),
        &counted,
        &full_authority()?,
    )?;
    let [draft @ Proposal::DraftIssue { reports, .. }] = proposals.as_slice() else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    assert_eq!(reports.len(), MAX_PER_PROPOSAL);
    assert!(applied(
        &kitchen.submit(&forge, &counted, draft, &task, fence)?
    ));

    let counted = ledger.counted(&task)?;
    let known = BTreeMap::from([(
        ProblemKey::new(&problem)?,
        KnownIssue {
            number: issue(3)?,
            state: IssueState::Open,
        },
    )]);
    let proposals = plan(
        &house()?,
        &repo()?,
        &batch,
        &known,
        &counted,
        &full_authority()?,
    )?;
    let [Proposal::AddReports { reports, total, .. }] = proposals.as_slice() else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    assert_eq!((reports.len(), *total), (5, MAX_PER_PROPOSAL + 5));
    Ok(())
}

#[test]
fn a_waived_effect_does_not_count_its_reports() -> TestResult {
    let kitchen = Kitchen::new()?;
    let forge = Forge::new()?;
    let sources = sources()?;
    let (task, fence) = kitchen.start("intake-1")?;
    let ledger = kitchen.ledger(kitchen.store())?;
    let batch = login(&sources, &["m1"])?;
    let counted = ledger.counted(&task)?;
    let proposals = plan(
        &house()?,
        &repo()?,
        &batch,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let proposal = proposals.first().ok_or("no proposal")?;
    forge.lose_next_response.set(true);
    let uncertain = kitchen.submit(&forge, &counted, proposal, &task, fence)?;
    assert!(matches!(uncertain.state(), EffectState::Uncertain { .. }));

    // A person hands the effect over and waives the risk: the forge never
    // confirmed the comment, so the outcome is still unknown.
    kitchen.store().record_effect_outcome(
        &task,
        fence,
        uncertain.seq(),
        kitchen::state::EffectOutcome::Unresolvable,
        common::at(2),
    )?;
    let waived = kitchen.store().accept_risk(
        &task,
        fence,
        uncertain.seq(),
        kitchen::state::RiskDecision {
            effect: uncertain.request().key().clone(),
            decided_by: common::holder("operator")?,
            revision: EvidenceRevision::INITIAL,
            action: kitchen::state::RiskAction::SettleUnsuccessfully,
        },
        common::at(3),
    )?;
    assert!(matches!(waived.state(), EffectState::Waived { .. }));
    kitchen.store().finish_attempt(
        &task,
        fence,
        kitchen::contracts::AttemptNumber::FIRST,
        kitchen::contracts::AttemptOutcome::Failed(kitchen::contracts::FailureClass::Retryable),
        common::at(4),
    )?;

    // Replaying the same report in a new task proposes it again.
    let (next, _) = kitchen.start("intake-2")?;
    let counted = ledger.counted(&next)?;
    assert_eq!(counted.total(&ProblemKey::new("login-timeout")?), 0);
    let replay = plan(
        &house()?,
        &repo()?,
        &batch,
        &open_issue(12)?,
        &counted,
        &full_authority()?,
    )?;
    let [Proposal::AddReports { reports, .. }] = replay.as_slice() else {
        return Err(format!("unexpected proposals: {replay:?}").into());
    };
    assert_eq!(reports.len(), 1);
    Ok(())
}
