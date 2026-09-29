//! The push boundary: authority, fresh PR and branch state, and the
//! compare-and-swap update. Fakes cover the decision matrix; a local bare
//! Git remote covers the real compare-and-swap. Simulated evidence: no real
//! forge, credentials, or network is used.

mod common;
mod workflows_support;

use std::cell::{Cell, RefCell};

use common::{TestResult, commit, ttl};
use kitchen::{
    BackendId, ErrorClass, TaskId,
    contracts::{
        BranchName, CommitId, ContractError, Fence, Grant, HouseGrants, IssueNumber, Permission,
        Repository, TaskAuthority,
    },
    state::StateError,
    workflows::{
        coordination::{LaunchOutcome, launch_worker},
        pickup::{Base, ClaimOutcome, TaskTemplate, WorkerBrief, claim_issue, issue_task_id},
        push::{
            GitConfigKey, PullRequests, PushBoundary, PushIntent, PushOutcome, PushPermit,
            PushRefusal, RefUpdater, RemoteBranches, UpdateFailure,
        },
        repair::{Mergeability, Observed, PullRequestState, PullRequestView},
    },
};
use workflows_support::{World, branch, brief, issue, template, under_consumer};

fn number(value: u64) -> TestResult<IssueNumber> {
    Ok(IssueNumber::new(value)?)
}

fn github() -> TestResult<BackendId> {
    Ok(BackendId::new("github")?)
}

/// Worker lifecycle on the fake orchestrator, plus pushes to the GitHub
/// namespace.
fn push_grant_list() -> TestResult<Vec<Grant>> {
    let mut grants = common::WORKER_PERMISSIONS
        .iter()
        .map(|permission| common::grant(*permission))
        .collect::<TestResult<Vec<_>>>()?;
    grants.push(Grant::house(
        Permission::PushBranch,
        github()?,
        common::credential()?,
    ));
    Ok(grants)
}

fn house_grants_of(grants: &[Grant]) -> TestResult<HouseGrants> {
    Ok(HouseGrants::new(common::house()?, grants.to_vec()))
}

/// A claimed pickup task whose worker was launched on `lemarier/issue-5`,
/// and whose authority includes `PushBranch` when `grants` delegates it.
struct Pushing {
    world: World,
    task: TaskId,
    fence: Fence,
    github: BackendId,
}

fn pushing_with(grants: &HouseGrants, requested: Vec<Grant>) -> TestResult<Pushing> {
    pushing_on(grants, requested, Base::DefaultBranch)
}

/// Launch `setup`'s worker with `brief`, which must be accepted.
fn launch(setup: &Pushing, brief: &WorkerBrief) -> TestResult {
    match launch_worker(
        &setup.world.ctx(),
        &setup.task,
        setup.fence,
        kitchen::contracts::Workspace::Isolated,
        brief,
    )? {
        LaunchOutcome::Accepted { .. } => Ok(()),
        other => Err(format!("launch not accepted: {other:?}").into()),
    }
}

fn pushing_on(grants: &HouseGrants, requested: Vec<Grant>, base: Base) -> TestResult<Pushing> {
    let mut world = World::new()?;
    world.grants = grants.clone();
    let mut spec: TaskTemplate = template()?;
    spec.authority = TaskAuthority::delegate(grants, requested)?;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        &world.fixture.store,
        &spec,
        &issue(5)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let setup = Pushing {
        task: issue_task_id(&issue(5)?)?,
        fence: lease.fence(),
        github: github()?,
        world,
    };
    launch(&setup, &WorkerBrief { base, ..brief(5)? })?;
    Ok(setup)
}

fn pushing() -> TestResult<Pushing> {
    let list = push_grant_list()?;
    pushing_with(&house_grants_of(&list)?, list)
}

/// Scripted reads that count how often they are asked.
struct Reads {
    pull_request: Observed<Option<PullRequestView>>,
    remote: Observed<Option<CommitId>>,
    default_branch: Observed<BranchName>,
    bound: Observed<bool>,
    pull_request_reads: Cell<u32>,
    remote_reads: Cell<u32>,
    heads_asked: RefCell<Vec<String>>,
}

impl Reads {
    fn new(
        pull_request: Observed<Option<PullRequestView>>,
        remote: Observed<Option<CommitId>>,
    ) -> Self {
        Self {
            pull_request,
            remote,
            default_branch: BranchName::new("main").map_or(Observed::Unknown, Observed::Known),
            bound: Observed::Known(true),
            pull_request_reads: Cell::new(0),
            remote_reads: Cell::new(0),
            heads_asked: RefCell::new(Vec::new()),
        }
    }

    fn total(&self) -> u32 {
        self.pull_request_reads.get() + self.remote_reads.get()
    }
}

impl PullRequests for Reads {
    fn pull_request(&self, _: IssueNumber) -> Observed<Option<PullRequestView>> {
        self.pull_request_reads
            .set(self.pull_request_reads.get() + 1);
        self.pull_request.clone()
    }

    fn default_branch(&self) -> Observed<BranchName> {
        self.default_branch.clone()
    }
}

impl RemoteBranches for Reads {
    fn reads_from(&self, _: &Repository) -> Observed<bool> {
        self.bound
    }

    fn head(&self, branch: &BranchName) -> Observed<Option<CommitId>> {
        self.remote_reads.set(self.remote_reads.get() + 1);
        self.heads_asked
            .borrow_mut()
            .push(branch.as_str().to_owned());
        self.remote.clone()
    }
}

type Call = (Option<CommitId>, String, CommitId);

/// Records what it was asked to update and answers with `result`.
struct Recorder {
    result: Result<(), UpdateFailure>,
    calls: RefCell<Vec<Call>>,
}

impl Recorder {
    fn answering(result: Result<(), UpdateFailure>) -> Self {
        Self {
            result,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl RefUpdater for Recorder {
    fn pushes_to(&self, _: &Repository) -> Observed<bool> {
        Observed::Known(true)
    }

    fn redirect(&self, _: &Repository) -> Observed<Option<GitConfigKey>> {
        Observed::Known(None)
    }

    fn update(
        &self,
        permit: &PushPermit,
        branch: &BranchName,
        commit: &CommitId,
    ) -> Result<(), UpdateFailure> {
        self.calls.borrow_mut().push((
            permit.replaces().cloned(),
            branch.as_str().to_owned(),
            commit.clone(),
        ));
        self.result.clone()
    }
}

fn boundary<'a>(
    setup: &'a Pushing,
    reads: &'a Reads,
    updater: &'a dyn RefUpdater,
) -> PushBoundary<'a> {
    PushBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        stack_tool: None,
        clock: &setup.world.clock,
        pull_requests: reads,
        remote: reads,
        updater,
    }
}

fn view(pr: u64, state: PullRequestState, head_branch: &str) -> TestResult<PullRequestView> {
    Ok(PullRequestView {
        number: number(pr)?,
        state,
        head: commit('d')?,
        head_branch: head_branch.to_owned(),
        base_branch: "main".to_owned(),
        mergeability: Mergeability::Clean,
    })
}

fn open(pr: u64) -> TestResult<Observed<Option<PullRequestView>>> {
    Ok(Observed::Known(Some(view(
        pr,
        PullRequestState::Open,
        "lemarier/issue-5",
    )?)))
}

fn update_intent(expected: Option<CommitId>) -> TestResult<PushIntent> {
    Ok(PushIntent {
        pull_request: Some(number(5)?),
        expected_remote: expected,
    })
}

fn first_intent() -> TestResult<PushIntent> {
    Ok(PushIntent {
        pull_request: None,
        expected_remote: None,
    })
}

#[test]
fn a_checked_push_updates_only_the_head_it_read() -> TestResult {
    let setup = pushing()?;
    let (old, new) = (commit('d')?, commit('e')?);
    let reads = Reads::new(open(5)?, Observed::Known(Some(old.clone())));
    let updater = Recorder::answering(Ok(()));
    let outcome = boundary(&setup, &reads, &updater).push(
        &setup.task,
        setup.fence,
        &update_intent(Some(old.clone()))?,
        &new,
    )?;
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            replaced: Some(old.clone())
        }
    );
    // The compare value of the swap is exactly the head that was read.
    assert_eq!(
        *updater.calls.borrow(),
        vec![(Some(old), "lemarier/issue-5".to_owned(), new.clone())]
    );
    // The boundary reads and updates only the branch it is bound to: the
    // intent names no branch a worker could pick.
    assert_eq!(*reads.heads_asked.borrow(), vec!["lemarier/issue-5"]);

    // A first push reads no pull request, and must find no branch.
    let setup = pushing()?;
    let reads = Reads::new(Observed::Unknown, Observed::Known(None));
    let updater = Recorder::answering(Ok(()));
    let outcome = boundary(&setup, &reads, &updater).push(
        &setup.task,
        setup.fence,
        &first_intent()?,
        &new,
    )?;
    assert_eq!(outcome, PushOutcome::Pushed { replaced: None });
    assert_eq!(reads.pull_request_reads.get(), 0);
    assert_eq!(
        *updater.calls.borrow(),
        vec![(None, "lemarier/issue-5".to_owned(), new)]
    );
    Ok(())
}

#[test]
fn every_refusal_stops_the_push_before_any_update() -> TestResult {
    let setup = pushing()?;
    let head = commit('d')?;
    let intent = update_intent(Some(head.clone()))?;
    let pr = |state, head_branch: &str| -> TestResult<Observed<Option<PullRequestView>>> {
        Ok(Observed::Known(Some(view(5, state, head_branch)?)))
    };
    let cases: Vec<(&str, PushIntent, Reads, PushRefusal)> = vec![
        // The live incident: the PR merged and its branch was deleted.
        (
            "merged",
            intent.clone(),
            Reads::new(
                pr(PullRequestState::Merged, "lemarier/issue-5")?,
                Observed::Known(None),
            ),
            PushRefusal::Merged,
        ),
        (
            "closed",
            intent.clone(),
            Reads::new(
                pr(PullRequestState::Closed, "lemarier/issue-5")?,
                Observed::Known(Some(head.clone())),
            ),
            PushRefusal::Closed,
        ),
        (
            "deleted",
            intent.clone(),
            Reads::new(open(5)?, Observed::Known(None)),
            PushRefusal::BranchDeleted,
        ),
        (
            "moved",
            intent.clone(),
            Reads::new(open(5)?, Observed::Known(Some(commit('f')?))),
            PushRefusal::RemoteMoved {
                found: commit('f')?,
            },
        ),
        (
            "unreadable pull request",
            intent.clone(),
            Reads::new(Observed::Unknown, Observed::Known(Some(head.clone()))),
            PushRefusal::Unknown,
        ),
        (
            "unreadable remote",
            intent.clone(),
            Reads::new(open(5)?, Observed::Unknown),
            PushRefusal::Unknown,
        ),
        (
            "missing pull request",
            intent.clone(),
            Reads::new(Observed::Known(None), Observed::Known(Some(head.clone()))),
            PushRefusal::PullRequestMissing,
        ),
        (
            "another pull request",
            intent.clone(),
            Reads::new(open(6)?, Observed::Known(Some(head.clone()))),
            PushRefusal::WrongPullRequest,
        ),
        (
            "renamed head branch",
            intent,
            Reads::new(
                pr(PullRequestState::Open, "orca/lemarier/issue-5")?,
                Observed::Known(Some(head.clone())),
            ),
            PushRefusal::WrongBranch,
        ),
        (
            "first push onto an existing branch",
            first_intent()?,
            Reads::new(Observed::Unknown, Observed::Known(Some(head.clone()))),
            PushRefusal::BranchExists,
        ),
    ];
    for (name, intent, reads, refusal) in cases {
        let updater = Recorder::answering(Ok(()));
        let outcome = boundary(&setup, &reads, &updater).push(
            &setup.task,
            setup.fence,
            &intent,
            &commit('e')?,
        )?;
        assert_eq!(outcome, PushOutcome::Refused(refusal), "{name}");
        assert!(
            updater.calls.borrow().is_empty(),
            "{name} reached the remote"
        );
    }
    Ok(())
}

#[test]
fn a_push_that_already_landed_is_not_sent_again() -> TestResult {
    let landed = commit('e')?;
    let updater = Recorder::answering(Ok(()));
    let reads = Reads::new(open(5)?, Observed::Known(Some(landed.clone())));
    // Its answer was lost, and the head is already the commit.
    for intent in [update_intent(Some(commit('d')?))?, first_intent()?] {
        let setup = pushing()?;
        let outcome =
            boundary(&setup, &reads, &updater).push(&setup.task, setup.fence, &intent, &landed)?;
        assert_eq!(outcome, PushOutcome::AlreadyCurrent);
    }
    assert!(updater.calls.borrow().is_empty());
    let setup = pushing()?;

    // A merged pull request still refuses.
    let merged = Reads::new(
        Observed::Known(Some(view(5, PullRequestState::Merged, "lemarier/issue-5")?)),
        Observed::Known(Some(landed.clone())),
    );
    let outcome = boundary(&setup, &merged, &updater).push(
        &setup.task,
        setup.fence,
        &update_intent(Some(commit('d')?))?,
        &landed,
    )?;
    assert_eq!(outcome, PushOutcome::Refused(PushRefusal::Merged));
    Ok(())
}

#[test]
fn a_lost_race_and_a_lost_answer_are_reported_not_hidden() -> TestResult {
    let setup = pushing()?;
    let head = commit('d')?;
    for (failure, expected) in [
        (UpdateFailure::Rejected, PushOutcome::Stale),
        (UpdateFailure::Uncertain, PushOutcome::Uncertain),
    ] {
        let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
        let updater = Recorder::answering(Err(failure));
        let outcome = boundary(&setup, &reads, &updater).push(
            &setup.task,
            setup.fence,
            &update_intent(Some(head.clone()))?,
            &commit('e')?,
        )?;
        assert_eq!(outcome, expected);
        assert_eq!(updater.calls.borrow().len(), 1);
    }
    Ok(())
}

#[test]
fn the_boundary_authorizes_the_task_before_it_reads_or_sends_anything() -> TestResult {
    let head = commit('d')?;
    let intent = update_intent(Some(head.clone()))?;
    let run =
        |setup: &Pushing, fence: Fence| -> TestResult<(kitchen::Result<PushOutcome>, u32, usize)> {
            let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
            let updater = Recorder::answering(Ok(()));
            let result =
                boundary(setup, &reads, &updater).push(&setup.task, fence, &intent, &commit('e')?);
            Ok((result, reads.total(), updater.calls.borrow().len()))
        };

    // A task delegated no push permission cannot push.
    let list = push_grant_list()?;
    let without: Vec<Grant> = list
        .iter()
        .filter(|grant| grant.permission != Permission::PushBranch)
        .cloned()
        .collect();
    let setup = pushing_with(&house_grants_of(&list)?, without)?;
    let (result, reads, updates) = run(&setup, setup.fence)?;
    assert!(matches!(
        result,
        Err(kitchen::Error::Contract(ContractError::PermissionDenied {
            permission: Permission::PushBranch
        }))
    ));
    assert_eq!((reads, updates), (0, 0));

    // A grant for another repository does not cover this task's repository.
    let elsewhere = Grant::repository(
        Permission::PushBranch,
        Repository::new("origin89hq/other")?,
        github()?,
        common::credential()?,
    );
    let mut with_other = push_grant_list()?;
    with_other.retain(|grant| grant.permission != Permission::PushBranch);
    with_other.push(elsewhere);
    let setup = pushing_with(&house_grants_of(&with_other)?, with_other.clone())?;
    let (result, reads, updates) = run(&setup, setup.fence)?;
    assert!(matches!(
        result,
        Err(kitchen::Error::Contract(
            ContractError::PermissionDenied { .. }
        ))
    ));
    assert_eq!((reads, updates), (0, 0));

    // The task holds the grant, but the house revoked it since.
    let mut setup = pushing()?;
    setup.world.grants = HouseGrants::new(
        common::house()?,
        common::WORKER_PERMISSIONS
            .iter()
            .map(|permission| common::grant(*permission))
            .collect::<TestResult<Vec<_>>>()?,
    );
    let (result, reads, updates) = run(&setup, setup.fence)?;
    assert!(matches!(
        result,
        Err(kitchen::Error::Contract(
            ContractError::AuthorityExpansion { .. }
        ))
    ));
    assert_eq!((reads, updates), (0, 0));

    // The grant names another destination than the one the push targets.
    let mut setup = pushing()?;
    setup.github = BackendId::new("gitlab")?;
    let (result, reads, updates) = run(&setup, setup.fence)?;
    assert!(matches!(
        result,
        Err(kitchen::Error::Contract(
            ContractError::PermissionDenied { .. }
        ))
    ));
    assert_eq!((reads, updates), (0, 0));
    Ok(())
}

#[test]
fn only_the_live_claim_holder_may_push() -> TestResult {
    let head = commit('d')?;
    let intent = update_intent(Some(head.clone()))?;
    let run = |setup: &Pushing, fence: Fence| -> TestResult<(kitchen::Result<PushOutcome>, u32)> {
        let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
        let updater = Recorder::answering(Ok(()));
        let result =
            boundary(setup, &reads, &updater).push(&setup.task, fence, &intent, &commit('e')?);
        Ok((result, reads.total()))
    };
    let setup = pushing()?;
    let (result, _) = run(&setup, setup.fence)?;
    assert!(matches!(result, Ok(PushOutcome::Pushed { .. })));

    // Another claim's fence, and the same fence once its claim expired, are
    // stale and read nothing.
    let ClaimOutcome::Claimed(other) = claim_issue(
        &setup.world.fixture.store,
        &template()?,
        &issue(6)?,
        &common::scheduled("second-holder")?,
        ttl(300)?,
        setup.world.now(),
    )?
    else {
        return Err("second claim failed".into());
    };
    let (result, reads) = run(&setup, other.fence())?;
    assert!(matches!(
        result,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert_eq!(reads, 0);
    setup.world.clock.advance(301);
    let (result, reads) = run(&setup, setup.fence)?;
    let error = result.err().ok_or("an expired claim pushed")?;
    assert!(matches!(
        error,
        kitchen::Error::State(StateError::StaleFence { .. })
    ));
    assert_eq!(error.class(), ErrorClass::Conflict);
    assert_eq!(reads, 0);
    Ok(())
}

#[test]
fn a_stacked_layer_is_never_pushed_on_the_plain_path_when_a_stack_tool_is_configured() -> TestResult
{
    use kitchen::house::StackTool;
    let head = commit('d')?;
    let push = |setup: &Pushing,
                reads: &Reads,
                tool: Option<StackTool>,
                intent: &PushIntent|
     -> TestResult<(PushOutcome, u32, usize)> {
        let updater = Recorder::answering(Ok(()));
        let outcome = PushBoundary {
            stack_tool: tool,
            ..boundary(setup, reads, &updater)
        }
        .push(&setup.task, setup.fence, intent, &commit('e')?)?;
        let updates = updater.calls.borrow().len();
        Ok((outcome, reads.total(), updates))
    };
    let update = update_intent(Some(head.clone()))?;
    let create = first_intent()?;
    let stack_base = Base::Stack {
        pull_request: number(4)?,
        branch: branch("lemarier/issue-4")?,
        depth: 1,
    };
    let list = push_grant_list()?;
    // Launched as a layer: the record makes it dependent, whatever the
    // writer says, and it is refused before any read.
    let stacked = pushing_on(&house_grants_of(&list)?, list.clone(), stack_base)?;
    for intent in [&update, &create] {
        let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
        assert_eq!(
            push(&stacked, &reads, Some(StackTool::GhStack), intent)?,
            (
                PushOutcome::Refused(PushRefusal::StackToolRequired(StackTool::GhStack)),
                0,
                0
            )
        );
    }
    // Launched on the default branch, but its pull request is based on
    // another branch: dependent by the observed base.
    let setup = pushing()?;
    let on_layer = || -> TestResult<Reads> {
        let mut view = view(5, PullRequestState::Open, "lemarier/issue-5")?;
        view.base_branch = "lemarier/issue-4".to_owned();
        Ok(Reads::new(
            Observed::Known(Some(view)),
            Observed::Known(Some(head.clone())),
        ))
    };
    let (outcome, _, updates) = push(&setup, &on_layer()?, Some(StackTool::GhStack), &update)?;
    assert_eq!(
        outcome,
        PushOutcome::Refused(PushRefusal::StackToolRequired(StackTool::GhStack))
    );
    assert_eq!(updates, 0);
    // An unreadable default branch refuses under a stack tool.
    let mut unknown = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
    unknown.default_branch = Observed::Unknown;
    let (outcome, _, updates) = push(&setup, &unknown, Some(StackTool::GhStack), &update)?;
    assert_eq!(outcome, PushOutcome::Refused(PushRefusal::Unknown));
    assert_eq!(updates, 0);
    // An independent branch, or a house without a stack tool, pushes.
    for (reads, tool) in [
        (
            Reads::new(open(5)?, Observed::Known(Some(head.clone()))),
            Some(StackTool::GhStack),
        ),
        (on_layer()?, None),
    ] {
        let (outcome, _, updates) = push(&setup, &reads, tool, &update)?;
        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                replaced: Some(head.clone())
            }
        );
        assert_eq!(updates, 1);
    }
    let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
    let (outcome, _, _) = push(&stacked, &reads, None, &update)?;
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            replaced: Some(head.clone())
        }
    );
    Ok(())
}

mod github_reads {
    use std::{collections::VecDeque, time::Duration};

    use kitchen::{
        CredentialId, HouseId,
        contracts::{ExternalRef, PostingBudget},
        integrations::github::{
            CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError,
            ReadLimits, ReadRequest,
        },
        workflows::push::GitHubPullRequests,
    };
    use serde_json::json;

    use super::*;

    struct Transport(RefCell<VecDeque<Result<Vec<u8>, IntegrationError>>>);

    impl GitHubReadTransport for Transport {
        fn read(
            &self,
            _: &CredentialRef,
            _: &ReadRequest,
            _: Duration,
            _: usize,
        ) -> Result<Vec<u8>, IntegrationError> {
            self.0
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(IntegrationError::Unavailable))
        }
    }

    fn pull_request(merged: bool, state: &str) -> serde_json::Value {
        json!({
            "number": 5,
            "state": state,
            "draft": false,
            "merged": merged,
            "head": {"sha": "d".repeat(40), "ref": "lemarier/issue-5"},
            "base": {"sha": "e".repeat(40), "ref": "main"},
            "mergeable": null,
            "mergeable_state": "unknown",
        })
    }

    #[test]
    fn the_boundary_reads_the_pull_request_through_the_house_client() -> TestResult {
        let house = HouseId::new("origin89")?;
        let requester = ExternalRef::new("origin89-bot")?;
        let repository = workflows_support::repo()?;
        let scope = HouseScope::new(
            house.clone(),
            [repository.clone()],
            requester.clone(),
            CredentialRef::new(house.clone(), CredentialId::new("github-read")?, requester),
            PostingBudget::new(0)?,
            [Permission::PostComment],
        )?;
        let responses = VecDeque::from([
            Ok(serde_json::to_vec(&pull_request(true, "closed"))?),
            Err(IntegrationError::Timeout),
            Ok(serde_json::to_vec(&pull_request(false, "open"))?),
        ]);
        let client = GitHubClient::new(
            scope,
            Transport(RefCell::new(responses)),
            ReadLimits::new(Duration::from_secs(5), 1, 64 * 1024)?,
        );
        let source = GitHubPullRequests {
            client: &client,
            house: &house,
            repository: &repository,
        };
        let setup = pushing()?;
        let head = commit('d')?;
        let remote = Reads::new(Observed::Unknown, Observed::Known(Some(head.clone())));
        let run = || -> TestResult<(PushOutcome, usize)> {
            let updater = Recorder::answering(Ok(()));
            let outcome = PushBoundary {
                pull_requests: &source,
                ..boundary(&setup, &remote, &updater)
            }
            .push(
                &setup.task,
                setup.fence,
                &update_intent(Some(head.clone()))?,
                &commit('e')?,
            )?;
            Ok((outcome, updater.calls.borrow().len()))
        };
        // Merged: refused. Unreadable: refused. Open: pushed.
        assert_eq!(run()?, (PushOutcome::Refused(PushRefusal::Merged), 0));
        assert_eq!(run()?, (PushOutcome::Refused(PushRefusal::Unknown), 0));
        assert_eq!(
            run()?,
            (
                PushOutcome::Pushed {
                    replaced: Some(head.clone())
                },
                1
            )
        );
        Ok(())
    }
}

#[cfg(unix)]
mod git_remote {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
        time::{Duration, Instant},
    };

    use kitchen::{
        contracts::CommitId,
        workflows::{
            coordination::CoordinationError,
            push::{GitRemote, IsolatedGitConfig, PushSetting},
        },
    };
    use tempfile::TempDir;

    use super::*;

    const GIT: &str = "/usr/bin/git";
    const BRANCH: &str = "lemarier/issue-5";

    fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
        let output = Command::new(GIT)
            .args([
                "-c",
                "user.name=Kitchen Test",
                "-c",
                "user.email=kitchen@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn text(path: &Path) -> TestResult<&str> {
        Ok(path.to_str().ok_or("non-UTF-8 path")?)
    }

    /// A bare remote and two clones of it: the worker's and another person's.
    struct Repos {
        dir: TempDir,
        remote: PathBuf,
        worker: PathBuf,
        other: PathBuf,
    }

    /// The bare remote is the task's repository, `origin89hq/firmware`,
    /// under the temporary directory, which is the remote's URL base.
    fn fresh_repos() -> TestResult<Repos> {
        let dir = tempfile::tempdir()?;
        let remote = dir.path().join("origin89hq").join("firmware.git");
        fs::create_dir_all(&remote)?;
        git(&remote, &["init", "--bare"])?;
        let worker = dir.path().join("worker");
        let other = dir.path().join("other");
        for clone in [&worker, &other] {
            git(dir.path(), &["clone", text(&remote)?, text(clone)?])?;
        }
        git(&worker, &["checkout", "-b", BRANCH])?;
        Ok(Repos {
            dir,
            remote,
            worker,
            other,
        })
    }

    fn commit_in(dir: &Path, message: &str) -> TestResult<CommitId> {
        git(dir, &["commit", "--allow-empty", "-m", message])?;
        Ok(CommitId::new(&git(dir, &["rev-parse", "HEAD"])?)?)
    }

    /// The remote's head for `branch`, straight from the bare repository.
    fn remote_head(repos: &Repos, branch: &str) -> TestResult<Option<String>> {
        let head = git(
            &repos.remote,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/heads/{branch}"),
            ],
        )?;
        Ok((!head.is_empty()).then_some(head))
    }

    /// The other person pushes a commit to `BRANCH`, on top of the remote's
    /// current head when there is one.
    fn other_pushes(repos: &Repos) -> TestResult<CommitId> {
        if remote_head(repos, BRANCH)?.is_some() {
            git(&repos.other, &["fetch", "origin", BRANCH])?;
            git(&repos.other, &["checkout", "-B", BRANCH, "FETCH_HEAD"])?;
        } else {
            git(&repos.other, &["checkout", "-B", BRANCH])?;
        }
        let moved = commit_in(&repos.other, "somebody else")?;
        git(&repos.other, &["push", "origin", BRANCH])?;
        Ok(moved)
    }

    fn url_base(repos: &Repos) -> TestResult<String> {
        Ok(format!("{}/", text(repos.dir.path())?))
    }

    /// The worker's remote, with Kitchen's configuration beside the
    /// repositories, outside the worker's checkout.
    fn remote_for(repos: &Repos) -> TestResult<GitRemote> {
        Ok(GitRemote::new(
            PathBuf::from(GIT),
            repos.worker.clone(),
            "origin",
            workflows_support::isolated_config(repos.dir.path(), &[])?,
            Duration::from_secs(30),
        )?
        .with_url_bases(&[&url_base(repos)?])?)
    }

    /// Reads the remote head, then lets something happen before answering:
    /// the window between the check and the update.
    struct ChangesAfterRead<'a> {
        remote: &'a GitRemote,
        change: &'a dyn Fn() -> TestResult,
        failure: RefCell<Option<String>>,
        done: Cell<bool>,
    }

    impl RemoteBranches for ChangesAfterRead<'_> {
        fn reads_from(&self, repository: &Repository) -> Observed<bool> {
            self.remote.reads_from(repository)
        }

        fn head(&self, branch: &BranchName) -> Observed<Option<CommitId>> {
            let head = self.remote.head(branch);
            if !self.done.replace(true)
                && let Err(error) = (self.change)()
            {
                *self.failure.borrow_mut() = Some(error.to_string());
            }
            head
        }
    }

    fn push_with(
        setup: &Pushing,
        pull_request: Observed<Option<PullRequestView>>,
        remote: &dyn RemoteBranches,
        updater: &dyn RefUpdater,
        intent: &PushIntent,
        commit: &CommitId,
    ) -> TestResult<PushOutcome> {
        let reads = Reads::new(pull_request, Observed::Unknown);
        Ok(PushBoundary {
            remote,
            ..boundary(setup, &reads, updater)
        }
        .push(&setup.task, setup.fence, intent, commit)?)
    }

    #[test]
    fn git_creates_updates_and_recognizes_a_push_that_already_landed() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let remote = remote_for(&repos)?;
        let first = commit_in(&repos.worker, "one")?;
        let created = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &first,
        )?;
        assert_eq!(created, PushOutcome::Pushed { replaced: None });
        assert_eq!(
            remote_head(&repos, BRANCH)?.as_deref(),
            Some(first.as_str())
        );

        let second = commit_in(&repos.worker, "two")?;
        let intent = update_intent(Some(first.clone()))?;
        let updated = push_with(&setup, open(5)?, &remote, &remote, &intent, &second)?;
        assert_eq!(
            updated,
            PushOutcome::Pushed {
                replaced: Some(first)
            }
        );
        assert_eq!(
            remote_head(&repos, BRANCH)?.as_deref(),
            Some(second.as_str())
        );

        // Its answer was lost: the same push finds the commit in place.
        let again = push_with(&setup, open(5)?, &remote, &remote, &intent, &second)?;
        assert_eq!(again, PushOutcome::AlreadyCurrent);
        Ok(())
    }

    #[test]
    fn git_refuses_an_update_when_the_branch_moves_between_check_and_update() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let remote = remote_for(&repos)?;
        let first = commit_in(&repos.worker, "one")?;
        push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &first,
        )?;
        let second = commit_in(&repos.worker, "two")?;

        let moved = RefCell::new(None);
        let change = || -> TestResult {
            *moved.borrow_mut() = Some(other_pushes(&repos)?);
            Ok(())
        };
        let racing = ChangesAfterRead {
            remote: &remote,
            change: &change,
            failure: RefCell::new(None),
            done: Cell::new(false),
        };
        let outcome = push_with(
            &setup,
            open(5)?,
            &racing,
            &remote,
            &update_intent(Some(first))?,
            &second,
        )?;
        assert_eq!(racing.failure.borrow().as_deref(), None);
        // The check passed against the head it read, yet the update failed
        // instead of overwriting the other person's commit.
        assert_eq!(outcome, PushOutcome::Stale);
        let moved = moved.borrow().clone().ok_or("the branch never moved")?;
        assert_eq!(
            remote_head(&repos, BRANCH)?.as_deref(),
            Some(moved.as_str())
        );
        assert_ne!(
            Some(second.as_str()),
            remote_head(&repos, BRANCH)?.as_deref()
        );
        Ok(())
    }

    #[test]
    fn git_never_recreates_a_branch_after_its_pull_request_merged() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let remote = remote_for(&repos)?;
        let first = commit_in(&repos.worker, "one")?;
        push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &first,
        )?;
        // The pull request merges and the forge deletes the branch.
        git(&repos.remote, &["branch", "-D", BRANCH])?;
        assert_eq!(remote_head(&repos, BRANCH)?, None);

        let next = commit_in(&repos.worker, "after the merge")?;
        let intent = update_intent(Some(first))?;
        let merged = Observed::Known(Some(view(5, PullRequestState::Merged, BRANCH)?));
        for (pull_request, refusal) in [
            (merged, PushRefusal::Merged),
            (Observed::Unknown, PushRefusal::Unknown),
            // Even a stale "open" answer cannot recreate the deleted branch.
            (open(5)?, PushRefusal::BranchDeleted),
        ] {
            let outcome = push_with(&setup, pull_request, &remote, &remote, &intent, &next)?;
            assert_eq!(outcome, PushOutcome::Refused(refusal));
            assert_eq!(remote_head(&repos, BRANCH)?, None);
        }
        // A stale intent that claims a first push cannot recreate it either:
        // the boundary recorded that it published the branch.
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &next,
        )?;
        assert_eq!(outcome, PushOutcome::Refused(PushRefusal::BranchDeleted));
        assert_eq!(remote_head(&repos, BRANCH)?, None);
        Ok(())
    }

    #[test]
    fn git_pushes_only_to_the_granted_repository() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let elsewhere = repos.dir.path().join("elsewhere").join("firmware.git");
        fs::create_dir_all(&elsewhere)?;
        git(&elsewhere, &["init", "--bare"])?;
        let mine = commit_in(&repos.worker, "mine")?;
        let granted = text(&repos.remote)?.to_owned();
        let other = text(&elsewhere)?.to_owned();
        let redirects: [(&str, [&str; 4], String); 3] = [
            // The checkout's origin points at another repository.
            (
                "set-url",
                ["remote", "set-url", "origin", &other],
                "remote.origin.url".to_owned(),
            ),
            // The URL is right, but a rewrite sends fetches elsewhere.
            (
                "insteadOf",
                ["config", &format!("url.{other}.insteadOf"), &granted, ""],
                format!("url.{other}.insteadof"),
            ),
            // Or only pushes.
            (
                "pushInsteadOf",
                [
                    "config",
                    &format!("url.{other}.pushInsteadOf"),
                    &granted,
                    "",
                ],
                format!("url.{other}.pushinsteadof"),
            ),
        ];
        for (name, args, key) in &redirects {
            let args: Vec<&str> = args.iter().copied().filter(|arg| !arg.is_empty()).collect();
            git(&repos.worker, &args)?;
            let remote = remote_for(&repos)?;
            let outcome = push_with(
                &setup,
                Observed::Unknown,
                &remote,
                &remote,
                &first_intent()?,
                &mine,
            )?;
            let PushOutcome::Refused(PushRefusal::CheckoutRedirect(found)) = outcome else {
                return Err(format!("{name} not refused: {outcome:?}").into());
            };
            assert_eq!(found.as_str(), key, "{name}");
            // Nothing reached either repository.
            assert_eq!(remote_head(&repos, BRANCH)?, None, "{name}");
            let landed = git(&elsewhere, &["for-each-ref", "--format=%(refname)"])?;
            assert!(landed.is_empty(), "{name} pushed elsewhere");
            // Undo the redirect for the next case.
            git(&repos.worker, &["remote", "set-url", "origin", &granted])?;
            let _ = git(
                &repos.worker,
                &["config", "--remove-section", &format!("url.{other}")],
            );
        }
        Ok(())
    }

    /// A second `pushurl` names another repository: a push by remote name
    /// would send the ref to both, so the check must read every push URL.
    #[test]
    fn git_refuses_a_second_push_url_that_names_another_repository() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let elsewhere = repos.dir.path().join("elsewhere").join("firmware.git");
        fs::create_dir_all(&elsewhere)?;
        git(&elsewhere, &["init", "--bare"])?;
        let mine = commit_in(&repos.worker, "mine")?;
        let granted = text(&repos.remote)?.to_owned();
        let other = text(&elsewhere)?.to_owned();
        git(
            &repos.worker,
            &["config", "--add", "remote.origin.pushurl", &granted],
        )?;
        git(
            &repos.worker,
            &["config", "--add", "remote.origin.pushurl", &other],
        )?;
        let remote = remote_for(&repos)?;
        assert_eq!(
            remote.pushes_to(&Repository::new("origin89hq/firmware")?),
            Observed::Known(false)
        );
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(
            outcome,
            PushOutcome::Refused(PushRefusal::CheckoutRedirect(key_named(
                &remote,
                "remote.origin.pushurl"
            )?))
        );
        assert_eq!(remote_head(&repos, BRANCH)?, None);
        assert!(git(&elsewhere, &["for-each-ref"])?.is_empty());

        // A second fetch URL is refused as well.
        git(
            &repos.worker,
            &["config", "--unset-all", "remote.origin.pushurl"],
        )?;
        git(
            &repos.worker,
            &["config", "--add", "remote.origin.url", &other],
        )?;
        assert_eq!(
            remote.reads_from(&Repository::new("origin89hq/firmware")?),
            Observed::Known(false)
        );
        Ok(())
    }

    /// Updates the remote's config after the check passed and before the
    /// update runs: the window issue #92 describes.
    struct ChangesBeforeUpdate<'a> {
        remote: &'a GitRemote,
        change: &'a dyn Fn() -> TestResult,
        failure: RefCell<Option<String>>,
    }

    impl RefUpdater for ChangesBeforeUpdate<'_> {
        fn pushes_to(&self, repository: &Repository) -> Observed<bool> {
            self.remote.pushes_to(repository)
        }

        fn redirect(&self, repository: &Repository) -> Observed<Option<GitConfigKey>> {
            self.remote.redirect(repository)
        }

        fn update(
            &self,
            permit: &PushPermit,
            branch: &BranchName,
            commit: &CommitId,
        ) -> Result<(), UpdateFailure> {
            if let Err(error) = (self.change)() {
                *self.failure.borrow_mut() = Some(error.to_string());
            }
            self.remote.update(permit, branch, commit)
        }
    }

    #[test]
    fn git_does_not_follow_a_config_change_between_the_check_and_the_push() -> TestResult {
        for (name, args) in [
            ("pushurl", ["remote", "set-url", "--push", "origin"]),
            ("url", ["remote", "set-url", "origin", ""]),
        ] {
            let repos = fresh_repos()?;
            let setup = pushing()?;
            let elsewhere = repos.dir.path().join("elsewhere").join("firmware.git");
            fs::create_dir_all(&elsewhere)?;
            git(&elsewhere, &["init", "--bare"])?;
            let mine = commit_in(&repos.worker, "mine")?;
            let other = text(&elsewhere)?.to_owned();
            let remote = remote_for(&repos)?;
            let change = || -> TestResult {
                let mut args: Vec<&str> = args.to_vec();
                args.retain(|arg| !arg.is_empty());
                args.push(&other);
                git(&repos.worker, &args)?;
                Ok(())
            };
            let racing = ChangesBeforeUpdate {
                remote: &remote,
                change: &change,
                failure: RefCell::new(None),
            };
            let outcome = push_with(
                &setup,
                Observed::Unknown,
                &remote,
                &racing,
                &first_intent()?,
                &mine,
            )?;
            assert_eq!(racing.failure.borrow().as_deref(), None, "{name}");
            assert_ne!(
                outcome,
                PushOutcome::Pushed { replaced: None },
                "{name}: reported a push that went elsewhere"
            );
            assert!(
                git(&elsewhere, &["for-each-ref"])?.is_empty(),
                "{name}: the push followed the changed config"
            );
        }
        Ok(())
    }

    /// The explicit-URL push works when the push URL is set apart from the
    /// fetch URL and both name the granted repository.
    #[test]
    fn git_pushes_through_a_separate_push_url_that_names_the_granted_repository() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let mine = commit_in(&repos.worker, "mine")?;
        let granted = text(&repos.remote)?.to_owned();
        git(
            &repos.worker,
            &["config", "--add", "remote.origin.pushurl", &granted],
        )?;
        let remote = remote_for(&repos)?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(outcome, PushOutcome::Pushed { replaced: None });
        assert_eq!(remote_head(&repos, BRANCH)?.as_deref(), Some(mine.as_str()));
        Ok(())
    }

    #[test]
    fn git_never_runs_the_checkouts_hooks() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let marker = repos.dir.path().join("hook-ran");
        let hooks = repos.dir.path().join("hooks");
        fs::create_dir_all(&hooks)?;
        for dir in [hooks.clone(), repos.worker.join(".git").join("hooks")] {
            let hook = dir.join("pre-push");
            common::executable::write_executable(
                &hook,
                format!("#!/bin/sh\ntouch '{}'\nexit 0\n", text(&marker)?),
            )?;
        }
        git(&repos.worker, &["config", "core.hooksPath", text(&hooks)?])?;
        let mine = commit_in(&repos.worker, "mine")?;
        let remote = remote_for(&repos)?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(outcome, PushOutcome::Pushed { replaced: None });
        assert!(
            !marker.exists(),
            "a worker-controlled hook ran under Kitchen"
        );
        Ok(())
    }

    #[test]
    fn git_pushes_the_named_branch_and_nothing_the_checkouts_config_adds() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let mine = commit_in(&repos.worker, "mine")?;
        // The worker's Git configuration follows tags, recurses into
        // submodules, and mirrors every ref, and an annotated tag points at
        // the commit being pushed.
        git(&repos.worker, &["tag", "-a", "-m", "release", "v1"])?;
        for (key, value) in [
            ("push.followTags", "true"),
            ("push.recurseSubmodules", "on-demand"),
            ("remote.origin.mirror", "true"),
        ] {
            git(&repos.worker, &["config", key, value])?;
        }
        let remote = remote_for(&repos)?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(outcome, PushOutcome::Pushed { replaced: None });
        let refs = git(&repos.remote, &["for-each-ref", "--format=%(refname)"])?;
        assert_eq!(refs, format!("refs/heads/{BRANCH}"), "only the branch");
        Ok(())
    }

    #[test]
    fn git_first_push_never_overwrites_a_branch_that_appears_meanwhile() -> TestResult {
        // The branch exists before the check.
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let remote = remote_for(&repos)?;
        let theirs = other_pushes(&repos)?;
        let mine = commit_in(&repos.worker, "mine")?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(outcome, PushOutcome::Refused(PushRefusal::BranchExists));
        assert_eq!(
            remote_head(&repos, BRANCH)?.as_deref(),
            Some(theirs.as_str())
        );

        // It appears between the check and the update: the empty compare
        // value means the branch must not exist.
        let repos = fresh_repos()?;
        let remote = remote_for(&repos)?;
        let mine = commit_in(&repos.worker, "mine")?;
        let created = RefCell::new(None);
        let change = || -> TestResult {
            *created.borrow_mut() = Some(other_pushes(&repos)?);
            Ok(())
        };
        let racing = ChangesAfterRead {
            remote: &remote,
            change: &change,
            failure: RefCell::new(None),
            done: Cell::new(false),
        };
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &racing,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(racing.failure.borrow().as_deref(), None);
        assert_eq!(outcome, PushOutcome::Stale);
        let created = created
            .borrow()
            .clone()
            .ok_or("the branch never appeared")?;
        assert_eq!(
            remote_head(&repos, BRANCH)?.as_deref(),
            Some(created.as_str())
        );
        Ok(())
    }

    #[test]
    fn git_heads_match_only_the_exact_ref() -> TestResult {
        let repos = fresh_repos()?;
        let remote = remote_for(&repos)?;
        // `ls-remote` patterns match by suffix: this ref ends in the
        // requested ref's full name.
        git(
            &repos.other,
            &["checkout", "-b", &format!("x/refs/heads/{BRANCH}")],
        )?;
        commit_in(&repos.other, "lookalike")?;
        git(
            &repos.other,
            &["push", "origin", &format!("x/refs/heads/{BRANCH}")],
        )?;
        assert_eq!(remote.head(&branch(BRANCH)?), Observed::Known(None));
        let mine = commit_in(&repos.worker, "mine")?;
        git(&repos.worker, &["push", "origin", BRANCH])?;
        assert_eq!(remote.head(&branch(BRANCH)?), Observed::Known(Some(mine)));
        Ok(())
    }

    #[test]
    fn an_unreachable_remote_is_unknown_and_never_reported_as_pushed() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let mine = commit_in(&repos.worker, "mine")?;
        // The remote is the granted repository, but it cannot be reached.
        fs::remove_dir_all(&repos.remote)?;
        let remote = remote_for(&repos)?;
        assert_eq!(remote.head(&branch(BRANCH)?), Observed::Unknown);
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(outcome, PushOutcome::Refused(PushRefusal::Unknown));
        let update = remote.update(&permit_for(&setup)?, &branch(BRANCH)?, &mine);
        assert_eq!(update, Err(UpdateFailure::Uncertain));
        Ok(())
    }

    /// A permit obtained the only way there is: through a passing check.
    fn permit_for(setup: &Pushing) -> TestResult<PushPermit> {
        struct Capture(RefCell<Option<PushPermit>>);
        impl RefUpdater for Capture {
            fn pushes_to(&self, _: &Repository) -> Observed<bool> {
                Observed::Known(true)
            }

            fn redirect(&self, _: &Repository) -> Observed<Option<GitConfigKey>> {
                Observed::Known(None)
            }

            fn update(
                &self,
                permit: &PushPermit,
                _: &BranchName,
                _: &CommitId,
            ) -> Result<(), UpdateFailure> {
                *self.0.borrow_mut() = Some(permit.clone());
                Ok(())
            }
        }
        let capture = Capture(RefCell::new(None));
        let reads = Reads::new(Observed::Unknown, Observed::Known(None));
        boundary(setup, &reads, &capture).push(
            &setup.task,
            setup.fence,
            &first_intent()?,
            &commit('e')?,
        )?;
        let permit = capture.0.borrow().clone();
        permit.ok_or_else(|| "no permit was issued".into())
    }

    #[test]
    fn a_git_remote_needs_absolute_paths_a_plain_name_and_a_deadline() -> TestResult {
        let kitchen = tempfile::tempdir()?;
        let config = workflows_support::isolated_config(kitchen.path(), &[])?;
        // A checkout that does not contain Kitchen's configuration; the
        // system temp directory does on Linux, where both live under `/tmp`.
        let checkout = tempfile::tempdir()?;
        let work = checkout
            .path()
            .to_str()
            .ok_or("temp directory path is not UTF-8")?;
        let good = |git: &str, worktree: &str, remote: &str, deadline: Duration| {
            GitRemote::new(
                PathBuf::from(git),
                PathBuf::from(worktree),
                remote,
                config.clone(),
                deadline,
            )
        };
        assert!(good(GIT, work, "origin", Duration::from_secs(1)).is_ok());
        assert!(good(GIT, work, "up-stream_2.x", Duration::from_secs(1)).is_ok());
        for (git, worktree, remote, deadline) in [
            ("git", work, "origin", Duration::from_secs(1)),
            (GIT, "worktree", "origin", Duration::from_secs(1)),
            (GIT, work, "", Duration::from_secs(1)),
            (GIT, work, "-origin", Duration::from_secs(1)),
            (GIT, work, "--upload-pack=x", Duration::from_secs(1)),
            (GIT, work, "or igin", Duration::from_secs(1)),
            (GIT, work, "origin/../x", Duration::from_secs(1)),
            (
                GIT,
                work,
                &"a".repeat(GitRemote::MAX_REMOTE_BYTES + 1),
                Duration::from_secs(1),
            ),
            (GIT, work, "origin", Duration::ZERO),
        ] {
            let error = good(git, worktree, remote, deadline)
                .err()
                .ok_or("invalid remote accepted")?;
            assert_eq!(
                kitchen::Error::from(error).class(),
                ErrorClass::InvalidInput
            );
        }
        let remote = || good(GIT, work, "origin", Duration::from_secs(1));
        assert!(remote()?.with_url_bases(&["https://git.example/"]).is_ok());
        let long = "a".repeat(513);
        for bases in [
            &[][..],
            &[""],
            &["https://git.example/ x/"],
            &["a\nb"],
            &[long.as_str()],
        ] {
            let error = remote()?
                .with_url_bases(bases)
                .err()
                .ok_or("invalid URL base accepted")?;
            assert_eq!(
                kitchen::Error::from(error).class(),
                ErrorClass::InvalidInput
            );
        }
        // Kitchen's configuration must not live in the checkout the worker
        // controls, whether named directly or through a symbolic link.
        let link = kitchen.path().join("link");
        std::os::unix::fs::symlink(kitchen.path(), &link)?;
        for worktree in [kitchen.path().to_path_buf(), link] {
            assert_eq!(
                GitRemote::new(
                    PathBuf::from(GIT),
                    worktree,
                    "origin",
                    config.clone(),
                    Duration::from_secs(1),
                )
                .err(),
                Some(CoordinationError::InvalidGitConfig)
            );
        }
        Ok(())
    }

    /// A bare repository `name` beside the granted one, which nothing may
    /// reach.
    fn elsewhere(repos: &Repos, name: &str) -> TestResult<PathBuf> {
        let path = repos.dir.path().join(name).join("firmware.git");
        fs::create_dir_all(&path)?;
        git(&path, &["init", "--bare"])?;
        Ok(path)
    }

    /// The checkout's origin is an alias that one rewrite turns into the
    /// granted URL, and a second rewrite turns the granted URL into another
    /// repository. Resolving the remote yields the granted URL, but a push to
    /// that URL would be rewritten again.
    #[test]
    fn git_refuses_a_chained_rewrite_of_the_granted_url() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let elsewhere = elsewhere(&repos, "elsewhere")?;
        let mine = commit_in(&repos.worker, "mine")?;
        let granted = text(&repos.remote)?.to_owned();
        let other = text(&elsewhere)?.to_owned();
        git(&repos.worker, &["remote", "set-url", "origin", "alias:"])?;
        git(
            &repos.worker,
            &["config", &format!("url.{granted}.insteadOf"), "alias:"],
        )?;
        git(
            &repos.worker,
            &["config", &format!("url.{other}.insteadOf"), &granted],
        )?;
        let remote = remote_for(&repos)?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        let PushOutcome::Refused(PushRefusal::CheckoutRedirect(key)) = outcome else {
            return Err(format!("chained rewrite not refused: {outcome:?}").into());
        };
        assert_eq!(key.as_str(), format!("url.{granted}.insteadof"));
        assert!(git(&elsewhere, &["for-each-ref"])?.is_empty());
        assert_eq!(remote_head(&repos, BRANCH)?, None);
        Ok(())
    }

    /// No rewrite is allowed in the checkout's configuration, not even a
    /// `pushInsteadOf` that lands on the granted repository, and a remote URL
    /// naming another repository is refused by its key even for a remote the
    /// push does not use.
    #[test]
    fn git_refuses_any_rewrite_or_foreign_remote_in_the_checkouts_config() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let elsewhere = elsewhere(&repos, "elsewhere")?;
        let mine = commit_in(&repos.worker, "mine")?;
        let granted = text(&repos.remote)?.to_owned();
        let bare = granted
            .strip_suffix(".git")
            .ok_or("no .git suffix")?
            .to_owned();
        git(&repos.worker, &["remote", "set-url", "origin", &bare])?;
        git(
            &repos.worker,
            &["config", &format!("url.{granted}.pushInsteadOf"), &bare],
        )?;
        let remote = remote_for(&repos)?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        let PushOutcome::Refused(PushRefusal::CheckoutRedirect(key)) = outcome else {
            return Err(format!("pushInsteadOf not refused: {outcome:?}").into());
        };
        assert_eq!(key.as_str(), format!("url.{granted}.pushinsteadof"));
        assert_eq!(remote_head(&repos, BRANCH)?, None);

        git(
            &repos.worker,
            &["config", "--remove-section", &format!("url.{granted}")],
        )?;
        git(&repos.worker, &["remote", "set-url", "origin", &granted])?;
        git(
            &repos.worker,
            &["remote", "add", "upstream", text(&elsewhere)?],
        )?;
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(
            outcome,
            PushOutcome::Refused(PushRefusal::CheckoutRedirect(key_named(
                &remote,
                "remote.upstream.url"
            )?))
        );
        assert_eq!(remote_head(&repos, BRANCH)?, None);
        Ok(())
    }

    /// The key [`RefUpdater::redirect`] reports, checked against `expected`.
    fn key_named(remote: &GitRemote, expected: &str) -> TestResult<GitConfigKey> {
        let Observed::Known(Some(key)) = remote.redirect(&Repository::new("origin89hq/firmware")?)
        else {
            return Err("no redirecting key".into());
        };
        assert_eq!(key.as_str(), expected);
        Ok(key)
    }

    /// A rewrite added after the boundary's check and before the update is
    /// caught by the update's own check: nothing is sent.
    #[test]
    fn git_refuses_a_rewrite_added_between_the_check_and_the_push() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let elsewhere = elsewhere(&repos, "elsewhere")?;
        let mine = commit_in(&repos.worker, "mine")?;
        let granted = text(&repos.remote)?.to_owned();
        let other = text(&elsewhere)?.to_owned();
        let remote = remote_for(&repos)?;
        let change = || -> TestResult {
            git(
                &repos.worker,
                &["config", &format!("url.{other}.insteadOf"), &granted],
            )?;
            Ok(())
        };
        let racing = ChangesBeforeUpdate {
            remote: &remote,
            change: &change,
            failure: RefCell::new(None),
        };
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &racing,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(racing.failure.borrow().as_deref(), None);
        let PushOutcome::Refused(PushRefusal::CheckoutRedirect(key)) = outcome else {
            return Err(format!("late rewrite not refused: {outcome:?}").into());
        };
        assert_eq!(key.as_str(), format!("url.{other}.insteadof"));
        assert!(git(&elsewhere, &["for-each-ref"])?.is_empty());
        Ok(())
    }

    /// A clean checkout pushes to a local bare remote with Kitchen's own
    /// configuration carrying a credential helper, and the push's Git reads
    /// no system configuration and no user configuration but Kitchen's file.
    #[test]
    fn git_pushes_from_a_clean_checkout_under_kitchens_configuration() -> TestResult {
        let repos = fresh_repos()?;
        let setup = pushing()?;
        let mine = commit_in(&repos.worker, "mine")?;
        let kitchen = tempfile::tempdir()?;
        let config = workflows_support::isolated_config(
            kitchen.path(),
            &[
                PushSetting::CredentialHelper {
                    url: Some("https://github.com".to_owned()),
                    helper: "!gh auth git-credential".to_owned(),
                },
                PushSetting::UserName("Kitchen".to_owned()),
            ],
        )?;
        let written = fs::read_to_string(config.path())?;
        assert_eq!(
            fs::metadata(config.path())?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            written,
            "[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n\
             [user]\n\tname = Kitchen\n"
        );
        let remote = GitRemote::new(
            PathBuf::from(GIT),
            repos.worker.clone(),
            "origin",
            config,
            Duration::from_secs(30),
        )?
        .with_url_bases(&[&url_base(&repos)?])?;
        assert_eq!(
            remote.redirect(&Repository::new("origin89hq/firmware")?),
            Observed::Known(None)
        );
        let outcome = push_with(
            &setup,
            Observed::Unknown,
            &remote,
            &remote,
            &first_intent()?,
            &mine,
        )?;
        assert_eq!(outcome, PushOutcome::Pushed { replaced: None });
        assert_eq!(remote_head(&repos, BRANCH)?.as_deref(), Some(mine.as_str()));
        Ok(())
    }

    #[test]
    fn kitchens_git_configuration_takes_only_plain_settings() -> TestResult {
        let kitchen = tempfile::tempdir()?;
        let dir = kitchen.path();
        let create = |path: PathBuf, settings: &[PushSetting]| {
            IsolatedGitConfig::create(Path::new(GIT), path, settings, Duration::from_secs(10))
        };
        // No settings is an empty file; creating again replaces the file.
        fs::write(dir.join("config"), "[url \"x\"]\n\tinsteadOf = y\n")?;
        let empty = create(dir.join("config"), &[])?;
        assert_eq!(fs::read_to_string(empty.path())?, "");
        let helper = |helper: &str| PushSetting::CredentialHelper {
            url: None,
            helper: helper.to_owned(),
        };
        let too_many = vec![helper("store"); IsolatedGitConfig::MAX_SETTINGS + 1];
        let cases: [(PathBuf, Vec<PushSetting>); 6] = [
            (PathBuf::from("relative"), vec![]),
            (dir.join("config"), vec![helper("")]),
            (dir.join("config"), vec![helper("store\n[url \"x\"]")]),
            (
                dir.join("config"),
                vec![PushSetting::CredentialHelper {
                    url: Some("https://a b".to_owned()),
                    helper: "store".to_owned(),
                }],
            ),
            (dir.join("config"), vec![helper(&"a".repeat(1025))]),
            (dir.join("config"), too_many),
        ];
        for (path, settings) in cases {
            assert_eq!(
                create(path, &settings).err(),
                Some(CoordinationError::InvalidGitConfig)
            );
        }
        // A directory that does not exist cannot hold the file.
        assert_eq!(
            create(dir.join("missing").join("config"), &[]).err(),
            Some(CoordinationError::GitConfigUnwritten)
        );
        // At the limit, every setting is written in order.
        let full = create(
            dir.join("full"),
            &vec![helper("store"); IsolatedGitConfig::MAX_SETTINGS],
        )?;
        // A write that fails leaves the previous file as it was.
        let failing = dir.join("failing-git");
        common::executable::write_executable(&failing, "#!/bin/sh\nexit 1\n")?;
        assert_eq!(
            IsolatedGitConfig::create(
                &failing,
                full.path().to_path_buf(),
                &[helper("other")],
                Duration::from_secs(10)
            )
            .err(),
            Some(CoordinationError::GitConfigUnwritten)
        );
        let written = fs::read_to_string(full.path())?;
        assert_eq!(
            written.matches("helper = store").count(),
            IsolatedGitConfig::MAX_SETTINGS
        );
        Ok(())
    }

    #[test]
    fn git_calls_are_bounded_by_the_deadline() -> TestResult {
        let dir = tempfile::tempdir()?;
        let slow = dir.path().join("git");
        common::executable::write_executable(&slow, "#!/bin/sh\nexec sleep 30\n")?;
        let kitchen = tempfile::tempdir()?;
        let remote = GitRemote::new(
            slow,
            dir.path().to_path_buf(),
            "origin",
            workflows_support::isolated_config(kitchen.path(), &[])?,
            Duration::from_millis(300),
        )?;
        let started = Instant::now();
        assert_eq!(remote.head(&branch(BRANCH)?), Observed::Unknown);
        assert_eq!(
            remote.update(&permit_for(&pushing()?)?, &branch(BRANCH)?, &commit('e')?),
            Err(UpdateFailure::Uncertain)
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        Ok(())
    }
}

#[test]
fn a_task_with_a_pending_cancellation_does_not_push() -> TestResult {
    let setup = pushing()?;
    setup.world.fixture.store.request_cancel(
        &setup.task,
        &common::holder("person")?,
        setup.world.now(),
    )?;
    let head = commit('d')?;
    let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
    let updater = Recorder::answering(Ok(()));
    let error = boundary(&setup, &reads, &updater)
        .push(
            &setup.task,
            setup.fence,
            &update_intent(Some(head))?,
            &commit('e')?,
        )
        .err()
        .ok_or("a cancelled task pushed")?;
    assert!(matches!(
        error,
        kitchen::Error::State(StateError::CancelRequested)
    ));
    assert_eq!((reads.total(), updater.calls.borrow().len()), (0, 0));
    Ok(())
}

#[test]
fn a_push_is_bound_to_the_tasks_recorded_branch() -> TestResult {
    use kitchen::contracts::{WorkerBackend, WorkerState};
    use kitchen::workflows::coordination::{Supervision, SupervisionInput, supervise};
    use kitchen::workflows::recovery::{PromptState, RecoverySignals, TerminalHolder};
    let setup = pushing()?;
    let head = commit('d')?;
    let intent = update_intent(Some(head.clone()))?;
    let push = |setup: &Pushing| -> TestResult<(PushOutcome, Vec<String>, usize)> {
        let reads = Reads::new(open(5)?, Observed::Known(Some(head.clone())));
        let updater = Recorder::answering(Ok(()));
        let outcome = boundary(setup, &reads, &updater).push(
            &setup.task,
            setup.fence,
            &intent,
            &commit('e')?,
        )?;
        let asked = reads.heads_asked.borrow().clone();
        let updates = updater.calls.borrow().len();
        Ok((outcome, asked, updates))
    };
    // A person takes the worker's terminal over, and it goes idle.
    let record = setup.world.fixture.store.task(&setup.task)?;
    let worker = kitchen::workflows::coordination::current_worker(&record)
        .ok_or("no worker")?
        .worker;
    setup
        .world
        .backend
        .set_worker_state(&worker, WorkerState::UserTakeover);
    let last = setup.world.now();
    setup.world.clock.advance(241);
    let idle = RecoverySignals {
        prompt: PromptState::Idle,
        terminal: TerminalHolder::Person,
        ..workflows_support::signals(&worker, Some(last))
    };
    let replaced = supervise(
        &setup.world.ctx(),
        &setup.task,
        setup.fence,
        &workflows_support::supervision()?,
        &SupervisionInput {
            signals: Some(&idle),
            ..SupervisionInput::default()
        },
    )?;
    assert!(matches!(replaced, Supervision::Replace { .. }));
    // The person's branch is never pushed, before anything is read.
    let (outcome, asked, updates) = push(&setup)?;
    assert_eq!(outcome, PushOutcome::Refused(PushRefusal::BranchHeld));
    assert!(asked.is_empty());
    assert_eq!(updates, 0);
    // The replacement works on a new branch, and pushes go there only.
    launch(
        &setup,
        &WorkerBrief {
            branch: branch("lemarier/issue-5-replacement")?,
            ..brief(5)?
        },
    )?;
    let mut renamed = view(5, PullRequestState::Open, "lemarier/issue-5-replacement")?;
    renamed.number = number(5)?;
    let reads = Reads::new(
        Observed::Known(Some(renamed)),
        Observed::Known(Some(head.clone())),
    );
    let updater = Recorder::answering(Ok(()));
    let outcome = boundary(&setup, &reads, &updater).push(
        &setup.task,
        setup.fence,
        &intent,
        &commit('e')?,
    )?;
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            replaced: Some(head.clone())
        }
    );
    assert_eq!(
        *reads.heads_asked.borrow(),
        vec!["lemarier/issue-5-replacement"]
    );
    assert_eq!(
        updater.calls.borrow().first().map(|call| call.1.clone()),
        Some("lemarier/issue-5-replacement".to_owned())
    );
    assert_eq!(
        setup.world.backend.observe_worker(&worker)?,
        WorkerState::UserTakeover
    );
    Ok(())
}

#[test]
fn a_task_without_a_launched_branch_pushes_nothing() -> TestResult {
    let mut world = World::new()?;
    let list = push_grant_list()?;
    world.grants = house_grants_of(&list)?;
    let mut spec: TaskTemplate = template()?;
    spec.authority = TaskAuthority::delegate(&world.grants, list)?;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        &world.fixture.store,
        &spec,
        &issue(5)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let setup = Pushing {
        task: issue_task_id(&issue(5)?)?,
        fence: lease.fence(),
        github: github()?,
        world,
    };
    let reads = Reads::new(open(5)?, Observed::Known(None));
    let updater = Recorder::answering(Ok(()));
    let outcome = boundary(&setup, &reads, &updater).push(
        &setup.task,
        setup.fence,
        &first_intent()?,
        &commit('e')?,
    )?;
    assert_eq!(outcome, PushOutcome::Refused(PushRefusal::NoBranch));
    assert_eq!((reads.total(), updater.calls.borrow().len()), (0, 0));
    Ok(())
}

#[test]
fn a_checked_pull_request_and_a_published_branch_bind_later_pushes() -> TestResult {
    let setup = pushing()?;
    let (first, second) = (commit('d')?, commit('e')?);
    // The first push creates the branch.
    let reads = Reads::new(Observed::Unknown, Observed::Known(None));
    let updater = Recorder::answering(Ok(()));
    assert_eq!(
        boundary(&setup, &reads, &updater).push(
            &setup.task,
            setup.fence,
            &first_intent()?,
            &first
        )?,
        PushOutcome::Pushed { replaced: None }
    );
    // An update names the pull request, which is checked and recorded.
    let reads = Reads::new(open(5)?, Observed::Known(Some(first.clone())));
    assert_eq!(
        boundary(&setup, &reads, &updater).push(
            &setup.task,
            setup.fence,
            &update_intent(Some(first.clone()))?,
            &second,
        )?,
        PushOutcome::Pushed {
            replaced: Some(first.clone())
        }
    );
    // The pull request merged and the forge kept the branch. An intent that
    // omits the pull request cannot skip the merged check.
    let merged = Reads::new(
        Observed::Known(Some(view(5, PullRequestState::Merged, "lemarier/issue-5")?)),
        Observed::Known(Some(second.clone())),
    );
    let omitted = PushIntent {
        pull_request: None,
        expected_remote: Some(second.clone()),
    };
    assert_eq!(
        boundary(&setup, &merged, &updater).push(
            &setup.task,
            setup.fence,
            &omitted,
            &commit('f')?
        )?,
        PushOutcome::Refused(PushRefusal::PullRequestRequired)
    );
    assert_eq!(merged.total(), 0);
    // A stale first-push intent cannot recreate the branch once it is gone.
    let first_again = PushIntent {
        pull_request: Some(number(5)?),
        expected_remote: None,
    };
    let gone_open = Reads::new(open(5)?, Observed::Known(None));
    assert_eq!(
        boundary(&setup, &gone_open, &updater).push(
            &setup.task,
            setup.fence,
            &first_again,
            &commit('f')?
        )?,
        PushOutcome::Refused(PushRefusal::BranchDeleted)
    );
    assert_eq!(updater.calls.borrow().len(), 2);
    Ok(())
}

#[test]
fn a_remote_that_is_not_the_granted_repository_is_refused_before_any_read() -> TestResult {
    let setup = pushing()?;
    for (bound, refusal) in [
        (Observed::Known(false), PushRefusal::RemoteMismatch),
        (Observed::Unknown, PushRefusal::Unknown),
    ] {
        let mut reads = Reads::new(open(5)?, Observed::Known(None));
        reads.bound = bound;
        let updater = Recorder::answering(Ok(()));
        let outcome = boundary(&setup, &reads, &updater).push(
            &setup.task,
            setup.fence,
            &first_intent()?,
            &commit('e')?,
        )?;
        assert_eq!(outcome, PushOutcome::Refused(refusal));
        assert_eq!((reads.total(), updater.calls.borrow().len()), (0, 0));
    }
    Ok(())
}
