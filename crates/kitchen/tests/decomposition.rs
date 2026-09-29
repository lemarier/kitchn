//! Project decomposition against a sanitized in-memory forge. Simulated
//! evidence only: no live GitHub read or write happens here.
mod common;
use common::{Fixture, ManualClock, TestResult, commit, house, interactive, scheduled, ttl};
use kitchen::{
    BackendId, CredentialId, EffectName, Error, ErrorClass, HolderId,
    contracts::{
        Clock, EffectFailure, ExternalRef, Grant, HouseGrants, IssueNumber, NotAppliedReason,
        Permission, PostingBudget, Provenance, Repository, Settlement, Text, Timestamp,
        UncertainReason,
    },
    integrations::github::{
        CredentialRef, GitHubExecutor, GitHubMutationTransport, GitHubReadTransport, HouseScope,
        IntegrationError, MutationRequest, ReadLimits, ReadRequest,
    },
    state::{EffectOutcome, EffectRecord, EffectState, RiskAction, RiskDecision, TaskState},
    workflows::decomposition::{
        AcknowledgeReport, ApplyOptions, ApplyOutcome, Approval, Blocker, DecompositionError,
        IssueKey, OverlapResolution, OwnedPath, Preview, Proposal, ProposedIssue, WriteKind,
        Writer, acknowledge, apply, preview, task_id,
    },
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, time::Duration};

const REPO: &str = "sample/project";
const REQUESTER: &str = "sample-bot";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The forge refuses the request.
    Reject,
    /// The forge applies the request, then the response is lost.
    LoseAfterApply,
    /// The request is lost before the forge sees it.
    LoseBeforeApply,
}

/// A forge holding numbered issues and their relationships.
#[derive(Default)]
struct Forge {
    issues: Vec<Value>,
    /// Relationship lists keyed by endpoint path, as `(number, repository_url)`.
    relations: BTreeMap<String, Vec<Value>>,
    submissions: usize,
    /// A fault for the submission with this 1-based number.
    fault: Option<(usize, Fault)>,
    /// Every read fails, so a lookup cannot prove anything.
    reads_fail: bool,
}

impl Forge {
    fn seeded() -> Self {
        let mut forge = Self::default();
        for (number, title) in [(1, "Epic"), (7, "Existing prerequisite")] {
            forge.issues.push(json!({
                "id": 1000 + number, "number": number, "title": title, "body": "",
                "user": {"login": "maintainer"},
                "html_url": format!("https://github.com/{REPO}/issues/{number}"),
            }));
        }
        forge
    }
    fn next_number(&self) -> u64 {
        self.issues
            .iter()
            .filter_map(|issue| issue["number"].as_u64())
            .max()
            .unwrap_or(0)
            + 1
    }
    fn created(&self) -> Vec<&Value> {
        self.issues
            .iter()
            .filter(|issue| issue["user"]["login"] == REQUESTER)
            .collect()
    }
    fn created_titled(&self, title: &str) -> Vec<u64> {
        self.created()
            .into_iter()
            .filter(|issue| issue["title"] == title)
            .filter_map(|issue| issue["number"].as_u64())
            .collect()
    }
    fn relation(&self, issue: u64, kind: &str) -> Vec<u64> {
        self.relations
            .get(&format!("repos/{REPO}/issues/{issue}/{kind}"))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["number"].as_u64())
            .collect()
    }
    fn number_of_id(&self, id: u64) -> Option<u64> {
        self.issues
            .iter()
            .find(|issue| issue["id"].as_u64() == Some(id))
            .and_then(|issue| issue["number"].as_u64())
    }
}

fn query<'a>(endpoint: &'a str, name: &str) -> Option<&'a str> {
    endpoint
        .split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

struct Transport<'a>(&'a RefCell<Forge>);

impl GitHubReadTransport for Transport<'_> {
    fn read(
        &self,
        _: &CredentialRef,
        request: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let forge = self.0.borrow();
        if forge.reads_fail {
            return Err(IntegrationError::Unknown);
        }
        let endpoint = request.endpoint();
        let path = endpoint.split('?').next().unwrap_or(endpoint);
        let issues_root = format!("repos/{REPO}/issues");
        let page = |entries: Vec<Value>| {
            let page: usize = query(endpoint, "page")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            json!(
                entries
                    .into_iter()
                    .skip((page - 1) * 100)
                    .take(100)
                    .collect::<Vec<_>>()
            )
        };
        let value = if path == issues_root {
            let mut listed: Vec<Value> = forge
                .issues
                .iter()
                .filter(|issue| {
                    query(endpoint, "creator").is_none_or(|login| issue["user"]["login"] == login)
                })
                .cloned()
                .collect();
            listed.reverse();
            page(listed)
        } else if path.ends_with("/sub_issues") || path.ends_with("/dependencies/blocked_by") {
            page(forge.relations.get(path).cloned().unwrap_or_default())
        } else if let Some(number) = path
            .strip_prefix(&format!("{issues_root}/"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            forge
                .issues
                .iter()
                .find(|issue| issue["number"].as_u64() == Some(number))
                .cloned()
                .ok_or(IntegrationError::Unknown)?
        } else {
            return Err(IntegrationError::Unknown);
        };
        serde_json::to_vec(&value).map_err(|_| IntegrationError::Unknown)
    }
}

impl GitHubMutationTransport for Transport<'_> {
    fn submit(
        &self,
        _: &CredentialRef,
        request: &MutationRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, EffectFailure> {
        let mut forge = self.0.borrow_mut();
        forge.submissions += 1;
        let fault = forge
            .fault
            .filter(|(at, _)| *at == forge.submissions)
            .map(|(_, fault)| fault);
        match fault {
            Some(Fault::Reject) => {
                return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
            }
            Some(Fault::LoseBeforeApply) => {
                return Err(EffectFailure::Uncertain(UncertainReason::Timeout));
            }
            Some(Fault::LoseAfterApply) | None => {}
        }
        let path = request.endpoint().to_owned();
        let body = request.body().clone();
        if path == format!("repos/{REPO}/issues") {
            let number = forge.next_number();
            forge.issues.push(json!({
                "id": 1000 + number, "number": number,
                "title": body["title"], "body": body["body"],
                "user": {"login": REQUESTER},
                "html_url": format!("https://github.com/{REPO}/issues/{number}"),
            }));
        } else if path.ends_with("/sub_issues") || path.ends_with("/dependencies/blocked_by") {
            let id = body["sub_issue_id"]
                .as_u64()
                .or_else(|| body["issue_id"].as_u64())
                .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            let number = forge
                .number_of_id(id)
                .ok_or(EffectFailure::NotApplied(NotAppliedReason::Rejected))?;
            forge.relations.entry(path).or_default().push(json!({
                "number": number,
                "repository_url": format!("https://api.github.com/repos/{REPO}"),
            }));
        } else {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        }
        if fault == Some(Fault::LoseAfterApply) {
            return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
        }
        Ok(b"{}".to_vec())
    }
}

struct House {
    fixture: Fixture,
    grants: HouseGrants,
    scope: HouseScope,
    clock: ManualClock,
}

impl House {
    fn new(budget: u32) -> TestResult<Self> {
        let repo = Repository::new(REPO)?;
        let backend = BackendId::new("github")?;
        let credential = CredentialId::new("sample-credential")?;
        let requester = ExternalRef::new(REQUESTER)?;
        let permissions = [Permission::CreateIssue, Permission::EditIssueRelationships];
        let grants = HouseGrants::new(
            house()?,
            permissions
                .iter()
                .map(|p| Grant::repository(*p, repo.clone(), backend.clone(), credential.clone()))
                .collect::<Vec<_>>(),
        );
        let scope = HouseScope::new(
            house()?,
            [repo],
            requester.clone(),
            CredentialRef::new(house()?, credential, requester),
            PostingBudget::new(budget)?,
            permissions,
        )?;
        Ok(Self {
            fixture: Fixture::new()?,
            grants,
            scope,
            clock: ManualClock::starting_at(100),
        })
    }

    /// Run `apply` as a person in a session, against `forge`.
    fn apply(
        &self,
        forge: &RefCell<Forge>,
        proposal: &Proposal,
        approval: &Approval,
    ) -> TestResult<kitchen::workflows::decomposition::ApplyReport> {
        self.apply_with(&self.clock, forge, proposal, approval)
    }

    /// Like [`Self::apply`], reading time from `clock`.
    fn apply_with(
        &self,
        clock: &dyn Clock,
        forge: &RefCell<Forge>,
        proposal: &Proposal,
        approval: &Approval,
    ) -> TestResult<kitchen::workflows::decomposition::ApplyReport> {
        let executor = GitHubExecutor::new(
            BackendId::new("github")?,
            self.scope.clone(),
            Transport(forge),
            ReadLimits::default(),
        );
        let writer = Writer {
            store: &self.fixture.store,
            executor: &executor,
            grants: &self.grants,
            clock,
        };
        self.clock.advance(1);
        Ok(apply(
            &writer,
            proposal,
            approval,
            &interactive("session")?,
            &options()?,
        )?)
    }

    /// Acknowledge `task`'s writes as `claimant`, re-reading `forge` when
    /// `reread` is set.
    fn acknowledge(
        &self,
        forge: &RefCell<Forge>,
        task: &kitchen::TaskId,
        claimant: &kitchen::contracts::Claimant,
        reread: bool,
    ) -> TestResult<AcknowledgeReport> {
        let executor = GitHubExecutor::new(
            BackendId::new("github")?,
            self.scope.clone(),
            Transport(forge),
            ReadLimits::default(),
        );
        self.clock.advance(1);
        Ok(acknowledge(
            &self.fixture.store,
            reread.then_some(&executor as &dyn kitchen::contracts::EffectExecutor),
            task,
            claimant,
            &Text::new("the earlier issues were reviewed by hand")?,
            &self.clock,
        )?)
    }
}

/// A clock that runs one action the first time it is read, to interleave
/// another caller at a chosen point inside `apply`.
struct InterleavingClock<'a> {
    inner: &'a ManualClock,
    action: RefCell<Option<Box<dyn FnOnce() -> TestResult + 'a>>>,
    failure: RefCell<Option<String>>,
}

impl Clock for InterleavingClock<'_> {
    fn now(&self) -> Timestamp {
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = action()
        {
            *self.failure.borrow_mut() = Some(error.to_string());
        }
        self.inner.now()
    }
}

fn options() -> TestResult<ApplyOptions> {
    Ok(ApplyOptions {
        provenance: Provenance {
            kitchen: commit('a')?,
            house_guidance: commit('b')?,
            repository_instructions: None,
        },
        lease: ttl(600)?,
    })
}

fn key(value: &str) -> TestResult<IssueKey> {
    Ok(IssueKey::new(value)?)
}

fn issue(name: &str, paths: &[&str], blocked_by: Vec<Blocker>) -> TestResult<ProposedIssue> {
    Ok(ProposedIssue {
        key: key(name)?,
        phase: Some("Phase 1".into()),
        title: format!("Build {name}"),
        outcome: format!("The {name} part works end to end."),
        owned_paths: paths
            .iter()
            .map(|path| OwnedPath::new(path))
            .collect::<Result<_, _>>()?,
        acceptance: vec![format!("{name} has tests"), format!("{name} is documented")],
        blocked_by,
    })
}

fn proposed(name: &str) -> TestResult<Blocker> {
    Ok(Blocker::Proposed(key(name)?))
}

/// Three issues listed out of dependency order: docs needs api, api needs
/// core, and core needs existing issue #7. All become sub-issues of #1.
fn project() -> TestResult<Proposal> {
    Ok(Proposal {
        repository: Repository::new(REPO)?,
        parent: Some(IssueNumber::new(1)?),
        issues: vec![
            issue("docs", &["docs/"], vec![proposed("api")?])?,
            issue("api", &["crates/api"], vec![proposed("core")?])?,
            issue(
                "core",
                &["crates/core", "crates/core/src/lib.rs"],
                vec![Blocker::Existing(IssueNumber::new(7)?)],
            )?,
        ],
    })
}

fn approval_of(preview: &Preview) -> TestResult<Approval> {
    Ok(Approval {
        id: ExternalRef::new("session-approval-1")?,
        given_by: HolderId::new("owner")?,
        digest: preview.digest.clone(),
    })
}

fn decomposition_error(error: &Error) -> Option<&DecompositionError> {
    match error {
        Error::Decomposition(error) => Some(error),
        _ => None,
    }
}

#[test]
fn preview_orders_blockers_first_and_shows_every_write() -> TestResult {
    let proposal = project()?;
    let shown = preview(&proposal)?;
    let order: Vec<&str> = shown.issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(order, ["core", "api", "docs"]);
    let core = shown.issues.first().ok_or("core")?;
    assert_eq!(
        core.body,
        "Parent: #1\n\n## Outcome\n\nThe core part works end to end.\n\n## Ownership\n\n\
         - `crates/core`\n- `crates/core/src/lib.rs`\n\n## Acceptance criteria\n\n\
         - [ ] core has tests\n- [ ] core is documented\n\n## Dependencies\n\n- #7\n"
    );
    // An issue's own overlapping paths are not a conflict between issues.
    assert!(shown.overlaps.is_empty());
    assert!(shown.ready());
    assert_eq!(
        (
            shown.writes.issues,
            shown.writes.sub_issue_links,
            shown.writes.dependencies
        ),
        (3, 3, 3)
    );
    let text = shown.render();
    assert!(text.contains("1. [core] Build core") && text.contains("Blocked by: #7"));
    assert!(text.ends_with(shown.digest.as_str()));

    // The digest is stable for the same proposal and moves with any change.
    assert_eq!(preview(&proposal)?.digest, shown.digest);
    let mut edited = proposal;
    if let Some(docs) = edited.issues.first_mut() {
        docs.acceptance.push("docs build in CI".into());
    }
    assert_ne!(preview(&edited)?.digest, shown.digest);
    Ok(())
}

#[test]
fn preview_rejects_a_dependency_cycle_with_its_path() -> TestResult {
    let proposal = Proposal {
        repository: Repository::new(REPO)?,
        parent: None,
        issues: vec![
            issue("free", &["free"], vec![])?,
            issue("a", &["a"], vec![proposed("c")?])?,
            issue("b", &["b"], vec![proposed("a")?, proposed("free")?])?,
            issue("c", &["c"], vec![proposed("b")?])?,
        ],
    };
    let error = preview(&proposal).err().ok_or("a cycle must be refused")?;
    assert_eq!(error.class(), ErrorClass::InvalidInput);
    let Some(DecompositionError::Cycle(keys)) = decomposition_error(&error) else {
        return Err(format!("expected a cycle, got {error}").into());
    };
    let names: Vec<&str> = keys.iter().map(IssueKey::as_str).collect();
    assert_eq!(names, ["a", "c", "b", "a"]);
    assert_eq!(error.to_string(), "dependency cycle: a -> c -> b -> a");
    Ok(())
}

#[test]
fn preview_rejects_malformed_proposals() -> TestResult {
    let base = project()?;
    let refused = |proposal: &Proposal| -> TestResult<DecompositionError> {
        let error = preview(proposal).err().ok_or("must be refused")?;
        decomposition_error(&error)
            .cloned()
            .ok_or_else(|| format!("unexpected {error}").into())
    };

    let mut unknown = base.clone();
    if let Some(docs) = unknown.issues.first_mut() {
        docs.blocked_by = vec![proposed("missing")?];
    }
    assert!(matches!(
        refused(&unknown)?,
        DecompositionError::UnknownBlocker { .. }
    ));

    let mut selfish = base.clone();
    if let Some(docs) = selfish.issues.first_mut() {
        docs.blocked_by = vec![proposed("docs")?];
    }
    assert_eq!(
        refused(&selfish)?,
        DecompositionError::InvalidBlocker(key("docs")?)
    );

    let mut repeated = base.clone();
    if let Some(docs) = repeated.issues.first_mut() {
        docs.blocked_by = vec![proposed("api")?, proposed("api")?];
    }
    assert_eq!(
        refused(&repeated)?,
        DecompositionError::InvalidBlocker(key("docs")?)
    );

    let mut duplicate = base.clone();
    duplicate.issues.push(issue("api", &["other"], vec![])?);
    assert_eq!(
        refused(&duplicate)?,
        DecompositionError::DuplicateKey(key("api")?)
    );

    let mut vague = base.clone();
    if let Some(docs) = vague.issues.first_mut() {
        docs.acceptance.clear();
    }
    assert_eq!(
        refused(&vague)?,
        DecompositionError::InvalidField {
            field: "acceptance"
        }
    );

    let mut unowned = base.clone();
    if let Some(docs) = unowned.issues.first_mut() {
        docs.owned_paths.clear();
    }
    assert_eq!(
        refused(&unowned)?,
        DecompositionError::InvalidField {
            field: "ownedPaths"
        }
    );

    let mut empty = base.clone();
    empty.issues.clear();
    assert_eq!(refused(&empty)?, DecompositionError::IssueCount);

    // The largest allowed proposal is accepted; one more issue is not.
    let mut largest = base;
    largest.issues = (0..kitchen::workflows::decomposition::MAX_ISSUES)
        .map(|n| issue(&format!("part-{n}"), &[&format!("part/{n}")], vec![]))
        .collect::<TestResult<_>>()?;
    assert!(preview(&largest).is_ok());
    largest.issues.push(issue("extra", &["extra"], vec![])?);
    assert_eq!(refused(&largest)?, DecompositionError::IssueCount);

    // External JSON is parsed strictly at the boundary.
    let escaping = json!({"repository": REPO, "issues": [{
        "key": "a", "title": "A", "outcome": "o", "ownedPaths": ["../etc"],
        "acceptance": ["x"]}]});
    assert!(serde_json::from_value::<Proposal>(escaping).is_err());
    let unknown_field = json!({"repository": REPO, "issues": [], "labels": ["x"]});
    assert!(serde_json::from_value::<Proposal>(unknown_field).is_err());
    Ok(())
}

#[test]
fn overlapping_ownership_is_flagged_until_ordered() -> TestResult {
    let mut proposal = Proposal {
        repository: Repository::new(REPO)?,
        parent: None,
        issues: vec![
            issue("state", &["crates/kitchen/src/state"], vec![])?,
            issue("store", &["crates/kitchen/src/state/store.rs"], vec![])?,
            issue("cli", &["crates/kitchen-cli"], vec![])?,
        ],
    };
    let unordered = preview(&proposal)?;
    assert!(!unordered.ready());
    let [overlap] = unordered.overlaps.as_slice() else {
        return Err(format!("expected one overlap, got {:?}", unordered.overlaps).into());
    };
    assert_eq!(
        (overlap.left.as_str(), overlap.right.as_str()),
        ("state", "store")
    );
    assert_eq!(overlap.resolution, OverlapResolution::Unordered);
    assert!(unordered.render().contains("UNORDERED"));

    // A transitive dependency orders them: store waits for cli, cli for state.
    if let Some(store) = proposal.issues.get_mut(1) {
        store.blocked_by = vec![proposed("cli")?];
    }
    if let Some(cli) = proposal.issues.get_mut(2) {
        cli.blocked_by = vec![proposed("state")?];
    }
    let ordered = preview(&proposal)?;
    assert!(ordered.ready());
    let [overlap] = ordered.overlaps.as_slice() else {
        return Err("the overlap stays listed once ordered".into());
    };
    assert_eq!(
        overlap.resolution,
        OverlapResolution::Ordered {
            first: key("state")?
        }
    );
    Ok(())
}

#[test]
fn apply_creates_every_issue_and_edge_once() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let proposal = project()?;
    let shown = preview(&proposal)?;
    let report = house.apply(&forge, &proposal, &approval_of(&shown)?)?;
    assert_eq!(report.outcome, ApplyOutcome::Completed);

    let numbers: Vec<u64> = ["core", "api", "docs"]
        .iter()
        .map(|name| {
            report
                .issues
                .get(&key(name)?)
                .map(|n| n.get())
                .ok_or_else(|| format!("{name} not created").into())
        })
        .collect::<TestResult<_>>()?;
    assert_eq!(numbers, [8, 9, 10]);
    let seen = forge.borrow();
    assert_eq!(seen.created().len(), 3);
    let core = seen.created().first().copied().ok_or("core")?.clone();
    assert_eq!(core["title"], "Build core");
    assert!(
        core["body"]
            .as_str()
            .is_some_and(|b| b.starts_with("Parent: #1\n"))
    );
    assert_eq!(seen.relation(1, "sub_issues"), [8, 9, 10]);
    assert_eq!(seen.relation(8, "dependencies/blocked_by"), [7]);
    assert_eq!(seen.relation(9, "dependencies/blocked_by"), [8]);
    assert_eq!(seen.relation(10, "dependencies/blocked_by"), [9]);
    assert_eq!(seen.submissions, 9);
    assert!(report.written.iter().all(|written| !written.reused));
    drop(seen);

    // The task settled; running the same approval again writes nothing.
    let id = task_id(&shown.digest)?;
    assert!(matches!(
        house.fixture.store.task(&id)?.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    let again = house.apply(&forge, &proposal, &approval_of(&shown)?)?;
    assert_eq!(again.outcome, ApplyOutcome::Settled(Settlement::Succeeded));
    assert_eq!(again.written.len(), 9);
    assert!(again.written.iter().all(|written| written.reused));
    assert_eq!(forge.borrow().submissions, 9);
    Ok(())
}

#[test]
fn nothing_is_written_without_an_approval_of_this_exact_preview() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let proposal = project()?;
    let approved = approval_of(&preview(&proposal)?)?;

    // The proposal changed after the person approved it.
    let mut changed = proposal.clone();
    if let Some(docs) = changed.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let stale = house.apply(&forge, &changed, &approved)?;
    assert_eq!(stale.outcome, ApplyOutcome::StaleApproval);
    assert_eq!(stale.task, None);

    // An approval of some other preview.
    let other = Approval {
        digest: format!("sha256:{}", "0".repeat(64)).parse()?,
        ..approved.clone()
    };
    assert_eq!(
        house.apply(&forge, &proposal, &other)?.outcome,
        ApplyOutcome::StaleApproval
    );

    // A scheduled run cannot apply even with the right digest.
    let executor = GitHubExecutor::new(
        BackendId::new("github")?,
        house.scope.clone(),
        Transport(&forge),
        ReadLimits::default(),
    );
    let writer = Writer {
        store: &house.fixture.store,
        executor: &executor,
        grants: &house.grants,
        clock: &house.clock,
    };
    let error = apply(
        &writer,
        &proposal,
        &approved,
        &scheduled("tick")?,
        &options()?,
    )
    .err()
    .ok_or("a scheduled apply must be refused")?;
    assert_eq!(error.class(), ErrorClass::Refused);
    assert_eq!(
        decomposition_error(&error),
        Some(&DecompositionError::ApprovalNeedsPerson)
    );

    // Unordered overlaps block an otherwise approved preview.
    let mut overlapping = proposal;
    overlapping
        .issues
        .push(issue("guide", &["docs/guide.md"], vec![])?);
    let not_ready = preview(&overlapping)?;
    assert_eq!(
        house
            .apply(&forge, &overlapping, &approval_of(&not_ready)?)?
            .outcome,
        ApplyOutcome::NotReady
    );

    assert_eq!(forge.borrow().submissions, 0);
    assert!(house.fixture.store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn a_preview_over_the_posting_budget_writes_nothing() -> TestResult {
    let house = House::new(8)?;
    let forge = RefCell::new(Forge::seeded());
    let proposal = project()?;
    let shown = preview(&proposal)?;
    let report = house.apply(&forge, &proposal, &approval_of(&shown)?)?;
    assert_eq!(
        report.outcome,
        ApplyOutcome::OverBudget {
            needed: 9,
            limit: 8
        }
    );
    assert_eq!(forge.borrow().submissions, 0);
    assert!(house.fixture.store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn retry_after_a_refused_write_completes_the_set_without_duplicates() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let approval = approval_of(&preview(&proposal)?)?;

    let first = house.apply(&forge, &proposal, &approval)?;
    let ApplyOutcome::NotApplied { effect, reason } = &first.outcome else {
        return Err(format!("expected a refusal, got {:?}", first.outcome).into());
    };
    assert_eq!(effect.as_str(), "issue-api");
    assert_eq!(*reason, NotAppliedReason::Rejected);
    assert_eq!(forge.borrow().created().len(), 1);
    assert!(forge.borrow().relations.is_empty());

    let second = house.apply(&forge, &proposal, &approval)?;
    assert_eq!(second.outcome, ApplyOutcome::Completed);
    let reused: Vec<(&str, &WriteKind)> = second
        .written
        .iter()
        .filter(|written| written.reused)
        .map(|written| (written.issue.as_str(), &written.write))
        .collect();
    assert_eq!(reused, [("core", &WriteKind::Create)]);
    let forge = forge.borrow();
    for title in ["Build core", "Build api", "Build docs"] {
        assert_eq!(forge.created_titled(title).len(), 1, "{title} exactly once");
    }
    assert_eq!(forge.relation(1, "sub_issues").len(), 3);
    // One refused submission, then the eight writes still missing.
    assert_eq!(forge.submissions, 1 + 1 + 8);
    Ok(())
}

#[test]
fn a_lost_response_is_reconciled_before_anything_else_is_written() -> TestResult {
    let house = House::new(20)?;
    // The fifth write is core's sub-issue link.
    let forge = RefCell::new(Forge {
        fault: Some((5, Fault::LoseAfterApply)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let approval = approval_of(&preview(&proposal)?)?;

    let first = house.apply(&forge, &proposal, &approval)?;
    let ApplyOutcome::Uncertain { effect } = &first.outcome else {
        return Err(format!("expected an uncertain write, got {:?}", first.outcome).into());
    };
    assert_eq!(effect.as_str(), "parent-api");
    assert_eq!(forge.borrow().submissions, 5);

    let second = house.apply(&forge, &proposal, &approval)?;
    assert_eq!(second.outcome, ApplyOutcome::Completed);
    let forge = forge.borrow();
    assert_eq!(forge.created().len(), 3);
    assert_eq!(forge.relation(1, "sub_issues"), [8, 9, 10]);
    // The lost link was found on the forge, not submitted again.
    assert_eq!(forge.submissions, 9);
    Ok(())
}

#[test]
fn an_unknown_outcome_stops_every_later_write() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((1, Fault::LoseBeforeApply)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let approval = approval_of(&preview(&proposal)?)?;

    for _ in 0..2 {
        let report = house.apply(&forge, &proposal, &approval)?;
        let ApplyOutcome::Uncertain { effect } = &report.outcome else {
            return Err(format!("expected an uncertain write, got {:?}", report.outcome).into());
        };
        assert_eq!(effect.as_str(), "issue-core");
    }
    // GitHub cannot prove the issue absent, so it is never created twice
    // and nothing after it is attempted.
    assert_eq!(forge.borrow().submissions, 1);
    assert!(forge.borrow().created().is_empty());
    Ok(())
}

#[test]
fn a_revised_proposal_waits_for_the_unfinished_one() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let original = preview(&proposal)?;
    house.apply(&forge, &proposal, &approval_of(&original)?)?;

    let mut revised = proposal;
    if let Some(docs) = revised.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let report = house.apply(&forge, &revised, &approval_of(&preview(&revised)?)?)?;
    assert_eq!(
        report.outcome,
        ApplyOutcome::EarlierUnfinished {
            task: task_id(&original.digest)?
        }
    );
    // Only the original's two submissions reached the forge.
    assert_eq!(forge.borrow().submissions, 2);
    Ok(())
}

/// Runs `proposal` once with the forge's fault armed, then again after its
/// retry budget has lapsed, so the task settles as exhausted.
fn exhausted_after_a_refused_write(
    house: &House,
    forge: &RefCell<Forge>,
    proposal: &Proposal,
    approval: &Approval,
) -> TestResult {
    house.apply(forge, proposal, approval)?;
    house.clock.advance(8 * 24 * 60 * 60);
    let settled = house.apply(forge, proposal, approval)?;
    assert_eq!(
        settled.outcome,
        ApplyOutcome::Settled(Settlement::Exhausted)
    );
    Ok(())
}

#[test]
fn a_task_that_exhausted_after_writing_keeps_the_repository_guard() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let original = preview(&proposal)?;
    let approval = approval_of(&original)?;
    exhausted_after_a_refused_write(&house, &forge, &proposal, &approval)?;
    let submitted = forge.borrow().submissions;
    let created = forge.borrow().created().len();
    assert_eq!(created, 1);

    let mut revised = proposal.clone();
    if let Some(docs) = revised.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let report = house.apply(&forge, &revised, &approval_of(&preview(&revised)?)?)?;
    let ApplyOutcome::EarlierSettledWithWrites {
        task,
        settlement,
        writes,
    } = &report.outcome
    else {
        return Err(format!("expected a held guard, got {:?}", report.outcome).into());
    };
    assert_eq!(*task, task_id(&original.digest)?);
    assert_eq!(*settlement, Settlement::Exhausted);
    let names: Vec<&str> = writes.iter().map(EffectName::as_str).collect();
    assert_eq!(names, ["issue-core"]);
    assert_eq!(report.task, None);
    // Nothing more reached the forge.
    assert_eq!(forge.borrow().submissions, submitted);
    assert_eq!(forge.borrow().created().len(), created);

    // The original digest still reports its own settlement, unchanged.
    let again = house.apply(&forge, &proposal, &approval)?;
    assert_eq!(again.outcome, ApplyOutcome::Settled(Settlement::Exhausted));
    Ok(())
}

#[test]
fn a_task_that_exhausted_without_writing_releases_the_repository_guard() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((1, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let approval = approval_of(&preview(&proposal)?)?;
    exhausted_after_a_refused_write(&house, &forge, &proposal, &approval)?;
    assert!(forge.borrow().created().is_empty());

    let mut revised = proposal;
    if let Some(docs) = revised.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let report = house.apply(&forge, &revised, &approval_of(&preview(&revised)?)?)?;
    assert_eq!(report.outcome, ApplyOutcome::Completed);
    Ok(())
}

#[test]
fn a_revision_may_replace_a_proposal_that_wrote_nothing() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((1, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let first = house.apply(&forge, &proposal, &approval_of(&preview(&proposal)?)?)?;
    assert!(matches!(first.outcome, ApplyOutcome::NotApplied { .. }));

    let mut revised = proposal;
    if let Some(docs) = revised.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let report = house.apply(&forge, &revised, &approval_of(&preview(&revised)?)?)?;
    assert_eq!(report.outcome, ApplyOutcome::Completed);
    let seen = forge.borrow();
    assert_eq!(seen.created().len(), 3);
    assert_eq!(seen.created_titled("Build the docs site").len(), 1);
    Ok(())
}

#[test]
fn the_rendered_preview_is_the_exact_text_that_is_posted() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let proposal = project()?;
    let shown = preview(&proposal)?;
    let text = shown.render();
    house.apply(&forge, &proposal, &approval_of(&shown)?)?;
    let seen = forge.borrow();
    let created = seen.created();
    assert_eq!(created.len(), 3);
    for issue in created {
        let title = issue["title"].as_str().ok_or("title")?;
        // The executor appends its own idempotency marker after the body.
        let body = issue["body"]
            .as_str()
            .and_then(|body| body.split("<!-- kitchen:").next())
            .ok_or("body")?
            .trim_end();
        assert!(text.contains(title), "title not shown: {title}");
        assert!(text.contains(body), "body not shown: {body}");
    }
    Ok(())
}

#[test]
fn editing_only_the_outcome_changes_the_body_the_preview_and_the_digest() -> TestResult {
    let proposal = project()?;
    let shown = preview(&proposal)?;
    let mut edited = proposal;
    if let Some(docs) = edited.issues.first_mut() {
        docs.outcome = "The docs part ships a different outcome.".into();
    }
    let changed = preview(&edited)?;
    assert_ne!(changed.digest, shown.digest);
    assert_ne!(changed.render(), shown.render());
    assert!(
        changed
            .render()
            .contains("The docs part ships a different outcome.")
    );
    assert!(!shown.render().contains("a different outcome"));
    Ok(())
}

#[test]
fn a_second_preview_that_interleaves_with_apply_is_refused() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        // The interleaved run creates one issue, then its second write is
        // refused, leaving its task unfinished with an applied write.
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let first = project()?;
    let mut second = first.clone();
    if let Some(docs) = second.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let first_approval = approval_of(&preview(&first)?)?;
    let second_preview = preview(&second)?;
    let clock = InterleavingClock {
        inner: &house.clock,
        action: RefCell::new(Some(Box::new(|| {
            // Runs after `second` passed its checks and before it creates
            // its task.
            house.apply(&forge, &first, &first_approval)?;
            Ok(())
        }))),
        failure: RefCell::new(None),
    };
    let report = house.apply_with(&clock, &forge, &second, &approval_of(&second_preview)?)?;
    assert_eq!(clock.failure.borrow().as_deref(), None);
    assert_eq!(
        report.outcome,
        ApplyOutcome::EarlierUnfinished {
            task: task_id(&preview(&first)?.digest)?
        }
    );
    assert_eq!(report.task, None);
    // Only the first preview's submissions reached the forge.
    assert_eq!(forge.borrow().submissions, 2);
    assert!(
        forge
            .borrow()
            .created_titled("Build the docs site")
            .is_empty()
    );
    Ok(())
}

#[test]
fn a_refused_revision_and_the_same_preview_retry_do_not_duplicate() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let approval = approval_of(&preview(&proposal)?)?;
    house.apply(&forge, &proposal, &approval)?;
    let mut revised = proposal.clone();
    if let Some(docs) = revised.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let refused = house.apply(&forge, &revised, &approval_of(&preview(&revised)?)?)?;
    assert!(matches!(
        refused.outcome,
        ApplyOutcome::EarlierUnfinished { .. }
    ));
    // Retrying the original digest resumes its task and completes the set.
    let resumed = house.apply(&forge, &proposal, &approval)?;
    assert_eq!(resumed.outcome, ApplyOutcome::Completed);
    assert_eq!(forge.borrow().created().len(), 3);
    Ok(())
}

#[test]
fn distinct_blocked_by_edges_never_share_an_effect_name() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    // Key `n7` beside existing issue #7, and keys that read as one another
    // once joined with hyphens: `a-by-b` blocked by `c`, `a` blocked by `b-by-c`.
    let proposal = Proposal {
        repository: Repository::new(REPO)?,
        parent: None,
        issues: vec![
            issue("n7", &["one/"], vec![])?,
            issue("c", &["two/"], vec![])?,
            issue("b-by-c", &["three/"], vec![])?,
            issue("a-by-b", &["four/"], vec![proposed("c")?])?,
            issue("a", &["five/"], vec![proposed("b-by-c")?])?,
            issue(
                "x",
                &["six/"],
                vec![proposed("n7")?, Blocker::Existing(IssueNumber::new(7)?)],
            )?,
        ],
    };
    let shown = preview(&proposal)?;
    let report = house.apply(&forge, &proposal, &approval_of(&shown)?)?;
    assert_eq!(report.outcome, ApplyOutcome::Completed);
    let number = |name: &str| -> TestResult<u64> {
        report
            .issues
            .get(&key(name)?)
            .map(|n| n.get())
            .ok_or_else(|| format!("{name} not created").into())
    };
    let seen = forge.borrow();
    let blocked_by = |name: &str| -> TestResult<Vec<u64>> {
        let mut found = seen.relation(number(name)?, "dependencies/blocked_by");
        found.sort_unstable();
        Ok(found)
    };
    let mut x = vec![number("n7")?, 7];
    x.sort_unstable();
    assert_eq!(blocked_by("x")?, x);
    assert_eq!(blocked_by("a-by-b")?, [number("c")?]);
    assert_eq!(blocked_by("a")?, [number("b-by-c")?]);
    Ok(())
}

fn names(list: &[EffectName]) -> Vec<&str> {
    list.iter().map(EffectName::as_str).collect()
}

/// A revision of `proposal` that a new approval covers.
fn revised(proposal: &Proposal) -> TestResult<(Proposal, Approval)> {
    let mut revised = proposal.clone();
    if let Some(docs) = revised.issues.first_mut() {
        docs.title = "Build the docs site".into();
    }
    let approval = approval_of(&preview(&revised)?)?;
    Ok((revised, approval))
}

#[test]
fn acknowledging_a_settled_task_releases_the_repository() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let original = preview(&proposal)?;
    exhausted_after_a_refused_write(&house, &forge, &proposal, &approval_of(&original)?)?;
    let (next, next_approval) = revised(&proposal)?;
    let held = house.apply(&forge, &next, &next_approval)?;
    assert!(matches!(
        held.outcome,
        ApplyOutcome::EarlierSettledWithWrites { .. }
    ));

    let id = task_id(&original.digest)?;
    let report = house.acknowledge(&forge, &id, &interactive("owner-session")?, true)?;
    assert!(report.reread);
    assert!(!report.already_acknowledged);
    assert!(report.unresolved.is_empty());
    assert_eq!(report.acknowledgement.by.as_str(), "owner-session");
    assert_eq!(
        report.acknowledgement.reason.as_str(),
        "the earlier issues were reviewed by hand"
    );
    let recorded = house.fixture.store.task(&id)?;
    assert_eq!(
        recorded.write_acknowledgement(),
        Some(&report.acknowledgement)
    );
    assert!(matches!(
        recorded.state(),
        TaskState::Settled {
            settlement: Settlement::Exhausted,
            ..
        }
    ));

    let released = house.apply(&forge, &next, &next_approval)?;
    assert_eq!(released.outcome, ApplyOutcome::Completed);
    Ok(())
}

#[test]
fn a_scheduled_claimant_cannot_acknowledge() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let original = preview(&proposal)?;
    exhausted_after_a_refused_write(&house, &forge, &proposal, &approval_of(&original)?)?;
    let id = task_id(&original.digest)?;

    let refused = house.acknowledge(&forge, &id, &scheduled("tick")?, true);
    let Err(error) = refused else {
        return Err("a scheduled claimant acknowledged".into());
    };
    assert!(error.to_string().contains("interactive claimant"));
    assert_eq!(house.fixture.store.task(&id)?.write_acknowledgement(), None);
    let (next, next_approval) = revised(&proposal)?;
    let still = house.apply(&forge, &next, &next_approval)?;
    assert!(matches!(
        still.outcome,
        ApplyOutcome::EarlierSettledWithWrites { .. }
    ));
    Ok(())
}

/// A task that settled with `issue-core` unproven: the first submission is
/// lost, and a person then waives the unknown write to cancel the task.
fn settled_with_an_unknown_write(
    house: &House,
    forge: &RefCell<Forge>,
    fault: Fault,
) -> TestResult<(Proposal, kitchen::TaskId)> {
    forge.borrow_mut().fault = Some((1, fault));
    let proposal = project()?;
    let original = preview(&proposal)?;
    let report = house.apply(forge, &proposal, &approval_of(&original)?)?;
    assert!(matches!(report.outcome, ApplyOutcome::Uncertain { .. }));
    let id = task_id(&original.digest)?;

    let store = &house.fixture.store;
    let operator = interactive("operator")?;
    let now = house.clock.now();
    let fence = store.claim(&id, &operator, ttl(600)?, now)?.fence();
    let lost = store
        .task(&id)?
        .effects()
        .first()
        .cloned()
        .ok_or("no write was recorded")?;
    store.record_effect_outcome(&id, fence, lost.seq(), EffectOutcome::Unresolvable, now)?;
    let decision = RiskDecision {
        effect: lost.request().key().clone(),
        decided_by: operator.holder.clone(),
        revision: store.task(&id)?.evidence().revision(),
        action: RiskAction::SettleUnsuccessfully,
    };
    store.accept_risk(&id, fence, lost.seq(), decision, now)?;
    store.request_cancel(&id, &operator.holder, now)?;
    store.settle_cancelled(&id, fence, now)?;
    assert!(matches!(
        store.task(&id)?.state(),
        TaskState::Settled {
            settlement: Settlement::Cancelled,
            ..
        }
    ));
    Ok((proposal, id))
}

#[test]
fn a_reread_resolves_an_unknown_write_the_forge_proves_applied() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let (_, id) = settled_with_an_unknown_write(&house, &forge, Fault::LoseAfterApply)?;

    let report = house.acknowledge(&forge, &id, &interactive("owner-session")?, true)?;
    assert_eq!(names(&report.applied), ["issue-core"]);
    assert!(report.unresolved.is_empty());
    assert!(report.acknowledgement.unresolved.is_empty());
    let after = house.fixture.store.task(&id)?;
    assert!(matches!(
        after.effects().first().map(EffectRecord::state),
        Some(EffectState::Applied { .. })
    ));
    Ok(())
}

#[test]
fn an_unproven_write_needs_the_acknowledgement_to_release() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let (proposal, id) = settled_with_an_unknown_write(&house, &forge, Fault::LoseAfterApply)?;
    let (next, next_approval) = revised(&proposal)?;
    forge.borrow_mut().reads_fail = true;
    let held = house.apply(&forge, &next, &next_approval)?;
    assert!(matches!(
        held.outcome,
        ApplyOutcome::EarlierSettledWithWrites { .. }
    ));
    // Reads still fail: the re-read proves nothing, so the write stays unknown.
    let report = house.acknowledge(&forge, &id, &interactive("owner-session")?, true)?;
    assert!(report.reread);
    assert!(report.applied.is_empty() && report.absent.is_empty());
    assert_eq!(names(&report.unresolved), ["issue-core"]);
    assert_eq!(names(&report.acknowledgement.unresolved), ["issue-core"]);
    let after = house.fixture.store.task(&id)?;
    assert!(matches!(
        after.effects().first().map(EffectRecord::state),
        Some(EffectState::Waived { .. })
    ));

    forge.borrow_mut().reads_fail = false;
    let released = house.apply(&forge, &next, &next_approval)?;
    assert_eq!(released.outcome, ApplyOutcome::Completed);
    Ok(())
}

#[test]
fn acknowledging_without_a_backend_reads_nothing() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let (_, id) = settled_with_an_unknown_write(&house, &forge, Fault::LoseBeforeApply)?;
    let reads_before = forge.borrow().submissions;

    let report = house.acknowledge(&forge, &id, &interactive("owner-session")?, false)?;
    assert!(!report.reread);
    assert_eq!(names(&report.unresolved), ["issue-core"]);
    assert_eq!(forge.borrow().submissions, reads_before);
    Ok(())
}

#[test]
fn a_repeated_acknowledgement_keeps_the_first_record() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let proposal = project()?;
    let original = preview(&proposal)?;
    exhausted_after_a_refused_write(&house, &forge, &proposal, &approval_of(&original)?)?;
    let id = task_id(&original.digest)?;

    let first = house.acknowledge(&forge, &id, &interactive("first-session")?, true)?;
    let second = house.acknowledge(&forge, &id, &interactive("second-session")?, true)?;
    assert!(second.already_acknowledged);
    assert_eq!(second.acknowledgement, first.acknowledgement);
    assert_eq!(second.acknowledgement.by.as_str(), "first-session");
    Ok(())
}

#[test]
fn only_a_settled_unsuccessful_decomposition_can_be_acknowledged() -> TestResult {
    let house = House::new(20)?;
    let forge = RefCell::new(Forge::seeded());
    let proposal = project()?;
    let original = preview(&proposal)?;
    let approval = approval_of(&original)?;
    let id = task_id(&original.digest)?;
    let person = interactive("owner-session")?;

    // No such task.
    assert!(house.acknowledge(&forge, &id, &person, true).is_err());

    // Settled successfully: nothing is held.
    let done = house.apply(&forge, &proposal, &approval)?;
    assert_eq!(done.outcome, ApplyOutcome::Completed);
    let error = house
        .acknowledge(&forge, &id, &person, true)
        .err()
        .ok_or("a successful task was acknowledged")?;
    assert!(
        error.to_string().contains("does not hold") || error.to_string().contains("not a settled")
    );
    assert_eq!(house.fixture.store.task(&id)?.write_acknowledgement(), None);

    // Unsettled: the unfinished guard is not an acknowledgement's to lift.
    let pending = RefCell::new(Forge {
        fault: Some((2, Fault::Reject)),
        ..Forge::seeded()
    });
    let other = House::new(20)?;
    other.apply(&pending, &proposal, &approval)?;
    assert!(other.acknowledge(&pending, &id, &person, true).is_err());
    let unsettled = other
        .fixture
        .store
        .acknowledge_settled_writes(
            &id,
            kitchen::state::WriteAcknowledgement {
                by: person.holder.clone(),
                at: other.clock.now(),
                reason: Text::new("too early")?,
                unresolved: Vec::new(),
            },
        )
        .err()
        .ok_or("an unsettled task was acknowledged")?;
    assert!(matches!(
        unsettled,
        Error::State(kitchen::state::StateError::TaskNotSettled(_))
    ));
    Ok(())
}
