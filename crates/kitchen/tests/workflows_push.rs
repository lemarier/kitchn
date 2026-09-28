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
        CommitId, ContractError, Fence, Grant, HouseGrants, IssueNumber, Permission, Repository,
        TaskAuthority,
    },
    state::StateError,
    workflows::{
        pickup::{BranchName, ClaimOutcome, TaskTemplate, claim_issue, issue_task_id},
        push::{
            PullRequests, PushBoundary, PushIntent, PushOutcome, PushPermit, PushRefusal,
            RefUpdater, RemoteBranches, UpdateFailure,
        },
        repair::{Mergeability, Observed, PullRequestState, PullRequestView},
    },
};
use workflows_support::{World, branch, issue, template, under_consumer};

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

/// A claimed pickup task whose authority includes `PushBranch` when `grants`
/// delegates it.
struct Pushing {
    world: World,
    task: TaskId,
    fence: Fence,
    github: BackendId,
    branch: BranchName,
}

fn pushing_with(grants: &HouseGrants, requested: Vec<Grant>) -> TestResult<Pushing> {
    let mut world = World::new()?;
    world.grants = grants.clone();
    let mut base: TaskTemplate = template()?;
    base.authority = TaskAuthority::delegate(grants, requested)?;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        &world.fixture.store,
        &base,
        &issue(5)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    Ok(Pushing {
        task: issue_task_id(&issue(5)?)?,
        fence: lease.fence(),
        github: github()?,
        branch: branch("lemarier/issue-5")?,
        world,
    })
}

fn pushing() -> TestResult<Pushing> {
    let list = push_grant_list()?;
    pushing_with(&house_grants_of(&list)?, list)
}

/// Scripted reads that count how often they are asked.
struct Reads {
    pull_request: Observed<Option<PullRequestView>>,
    remote: Observed<Option<CommitId>>,
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
}

impl RemoteBranches for Reads {
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
        self.result
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
        branch: &setup.branch,
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
    let setup = pushing()?;
    let landed = commit('e')?;
    let updater = Recorder::answering(Ok(()));
    let reads = Reads::new(open(5)?, Observed::Known(Some(landed.clone())));
    // Its answer was lost, and the head is already the commit.
    for intent in [update_intent(Some(commit('d')?))?, first_intent()?] {
        let outcome =
            boundary(&setup, &reads, &updater).push(&setup.task, setup.fence, &intent, &landed)?;
        assert_eq!(outcome, PushOutcome::AlreadyCurrent);
    }
    assert!(updater.calls.borrow().is_empty());

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

    use kitchen::{contracts::CommitId, workflows::push::GitRemote};
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

    fn fresh_repos() -> TestResult<Repos> {
        let dir = tempfile::tempdir()?;
        let remote = dir.path().join("remote.git");
        fs::create_dir(&remote)?;
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

    fn remote_for(repos: &Repos) -> TestResult<GitRemote> {
        Ok(GitRemote::new(
            PathBuf::from(GIT),
            repos.worker.clone(),
            "origin",
            Duration::from_secs(30),
        )?)
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
        git(
            &repos.worker,
            &[
                "remote",
                "set-url",
                "origin",
                text(&repos.dir.path().join("missing.git"))?,
            ],
        )?;
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
        let good = |git: &str, worktree: &str, remote: &str, deadline: Duration| {
            GitRemote::new(
                PathBuf::from(git),
                PathBuf::from(worktree),
                remote,
                deadline,
            )
        };
        assert!(good(GIT, "/tmp", "origin", Duration::from_secs(1)).is_ok());
        assert!(good(GIT, "/tmp", "up-stream_2.x", Duration::from_secs(1)).is_ok());
        for (git, worktree, remote, deadline) in [
            ("git", "/tmp", "origin", Duration::from_secs(1)),
            (GIT, "worktree", "origin", Duration::from_secs(1)),
            (GIT, "/tmp", "", Duration::from_secs(1)),
            (GIT, "/tmp", "-origin", Duration::from_secs(1)),
            (GIT, "/tmp", "--upload-pack=x", Duration::from_secs(1)),
            (GIT, "/tmp", "or igin", Duration::from_secs(1)),
            (GIT, "/tmp", "origin/../x", Duration::from_secs(1)),
            (
                GIT,
                "/tmp",
                &"a".repeat(GitRemote::MAX_REMOTE_BYTES + 1),
                Duration::from_secs(1),
            ),
            (GIT, "/tmp", "origin", Duration::ZERO),
        ] {
            let error = good(git, worktree, remote, deadline)
                .err()
                .ok_or("invalid remote accepted")?;
            assert_eq!(
                kitchen::Error::from(error).class(),
                ErrorClass::InvalidInput
            );
        }
        Ok(())
    }

    #[test]
    fn git_calls_are_bounded_by_the_deadline() -> TestResult {
        let dir = tempfile::tempdir()?;
        let slow = dir.path().join("git");
        fs::write(&slow, "#!/bin/sh\nexec sleep 30\n")?;
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755))?;
        let remote = GitRemote::new(
            slow,
            dir.path().to_path_buf(),
            "origin",
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
