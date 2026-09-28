//! The push boundary: every branch update a worker makes for a task goes
//! through [`PushBoundary::push`], which authorizes the task, reads the pull
//! request and the remote branch head of the one branch the task owns,
//! refuses stale state, and updates the remote ref only if it still holds the
//! head that was checked.
//!
//! The decision and the update cannot be separated. The only value that lets
//! a [`RefUpdater`] act is the [`PushPermit`] the boundary builds from
//! observations it took itself, and the update is a compare-and-swap on that
//! permit: a branch that changes between the check and the update makes the
//! update fail instead of overwriting the change. [`GitRemote`] is the Git
//! implementation of both remote traits.
//!
//! The boundary covers pushes made through it. A worker whose own Git
//! credentials can still push directly is a limit of credential isolation
//! (#6, #13), not of this module.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    BackendId, HouseId, TaskId,
    contracts::{
        Clock, CommitId, Fence, GrantScope, HouseGrants, IssueNumber, Permission, Repository,
    },
    integrations::github::{GitHubClient, GitHubReadTransport},
    state::{HouseStore, StateError, TaskState},
    workflows::{
        coordination::CoordinationError,
        pickup::BranchName,
        repair::{Observed, PullRequestState, PullRequestView, observe_pull_request},
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// What a writer is about to push to the branch its boundary is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushIntent {
    /// The pull request the branch belongs to, once opened.
    pub pull_request: Option<IssueNumber>,
    /// The remote head the writer last saw; `None` before the first push.
    pub expected_remote: Option<CommitId>,
}

/// State read immediately before the push.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PushObservation {
    /// The pull request, when the intent names one.
    pull_request: Observed<Option<PullRequestView>>,
    /// The remote branch head; `None` when the branch does not exist.
    remote_head: Observed<Option<CommitId>>,
}

/// Why a push must not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushRefusal {
    /// The pull request merged; pushing would recreate a deleted branch.
    Merged,
    /// The pull request closed.
    Closed,
    /// The branch was deleted.
    BranchDeleted,
    /// Someone else moved the branch.
    RemoteMoved {
        /// The head found.
        found: CommitId,
    },
    /// The branch already exists though the writer expected to create it.
    BranchExists,
    /// The pull request's head branch is not this branch.
    WrongBranch,
    /// The named pull request does not exist.
    PullRequestMissing,
    /// The observation is about another pull request.
    WrongPullRequest,
    /// State could not be read.
    Unknown,
}

/// Proof that a push was checked against fresh state, and the remote head it
/// may replace. Only [`PushBoundary::push`] builds one. It describes the state
/// at that check and is not a standing right: replaying it later is safe only
/// because the update compares against [`PushPermit::replaces`], so a branch
/// that has changed since makes the update fail.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct PushPermit {
    replaces: Option<CommitId>,
}

impl PushPermit {
    /// The remote head the update may replace: the compare value of the
    /// compare-and-swap. `None` means the branch must not exist.
    #[must_use]
    pub const fn replaces(&self) -> Option<&CommitId> {
        self.replaces.as_ref()
    }
}

/// What the check decided.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Update the remote ref under this permit.
    Update(PushPermit),
    /// The remote already holds the commit.
    Current,
}

/// Check PR and branch state immediately before a push. Every push needs a
/// fresh check; a stale permit proves nothing.
fn decide(
    branch: &BranchName,
    intent: &PushIntent,
    observed: &PushObservation,
    commit: &CommitId,
) -> std::result::Result<Decision, PushRefusal> {
    if let Some(number) = intent.pull_request {
        let Observed::Known(pull_request) = &observed.pull_request else {
            return Err(PushRefusal::Unknown);
        };
        let Some(pull_request) = pull_request else {
            return Err(PushRefusal::PullRequestMissing);
        };
        if pull_request.number != number {
            return Err(PushRefusal::WrongPullRequest);
        }
        match pull_request.state {
            PullRequestState::Open => {}
            PullRequestState::Merged => return Err(PushRefusal::Merged),
            PullRequestState::Closed => return Err(PushRefusal::Closed),
        }
        if branch.verify_observed(&pull_request.head_branch).is_err() {
            return Err(PushRefusal::WrongBranch);
        }
    }
    let Observed::Known(remote) = &observed.remote_head else {
        return Err(PushRefusal::Unknown);
    };
    match (&intent.expected_remote, remote) {
        // An earlier push of this commit landed and its answer was lost.
        (_, Some(found)) if found == commit => Ok(Decision::Current),
        (None, None) => Ok(Decision::Update(PushPermit { replaces: None })),
        (None, Some(_)) => Err(PushRefusal::BranchExists),
        (Some(_), None) => Err(PushRefusal::BranchDeleted),
        (Some(expected), Some(found)) if expected == found => Ok(Decision::Update(PushPermit {
            replaces: Some(found.clone()),
        })),
        (Some(_), Some(found)) => Err(PushRefusal::RemoteMoved {
            found: found.clone(),
        }),
    }
}

/// Reads the pull request a push belongs to.
pub trait PullRequests {
    /// The pull request `number`, `Known(None)` when it does not exist, and
    /// `Unknown` when it could not be read.
    fn pull_request(&self, number: IssueNumber) -> Observed<Option<PullRequestView>>;
}

/// Reads remote branch heads.
pub trait RemoteBranches {
    /// The head of `branch`, `Known(None)` when the branch does not exist,
    /// and `Unknown` when the remote could not be read.
    fn head(&self, branch: &BranchName) -> Observed<Option<CommitId>>;
}

/// Why a ref update did not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateFailure {
    /// The remote refused, or the branch no longer held the permit's head.
    /// Nothing changed.
    Rejected,
    /// The remote may or may not have applied the update.
    Uncertain,
}

/// Updates a remote branch with compare-and-swap semantics.
pub trait RefUpdater {
    /// Point `branch` at `commit` only if it still holds
    /// [`PushPermit::replaces`] (or does not exist, for `None`). A branch
    /// that changed since the check must make this fail with
    /// [`UpdateFailure::Rejected`], never overwrite it.
    ///
    /// # Errors
    /// Returns an [`UpdateFailure`] distinguishing "nothing changed" from
    /// "outcome unknown".
    fn update(
        &self,
        permit: &PushPermit,
        branch: &BranchName,
        commit: &CommitId,
    ) -> std::result::Result<(), UpdateFailure>;
}

/// What a push attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushOutcome {
    /// The branch now points at the commit.
    Pushed {
        /// The head the update replaced; `None` when it created the branch.
        replaced: Option<CommitId>,
    },
    /// The branch already pointed at the commit; nothing was sent.
    AlreadyCurrent,
    /// The check refused the push; nothing was sent.
    Refused(PushRefusal),
    /// The branch changed between the check and the update, or the remote
    /// refused it. Nothing changed; check again before another attempt.
    Stale,
    /// The update may or may not have applied. The next push re-reads the
    /// branch: a head equal to the commit is [`PushOutcome::AlreadyCurrent`].
    Uncertain,
}

/// What a push acts through.
#[derive(Clone, Copy)]
pub struct PushBoundary<'a> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The house's current grants.
    pub grants: &'a HouseGrants,
    /// The backend namespace the task's push grant names.
    pub destination: &'a BackendId,
    /// The branch the task owns, from its durable record and never from the
    /// pushing worker: the only branch this boundary reads and updates.
    pub branch: &'a BranchName,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// Pull request reads.
    pub pull_requests: &'a dyn PullRequests,
    /// Remote head reads.
    pub remote: &'a dyn RemoteBranches,
    /// The compare-and-swap ref update.
    pub updater: &'a dyn RefUpdater,
}

impl PushBoundary<'_> {
    /// Push `commit` to the bound branch for `task`.
    ///
    /// Nothing is read or sent until the task's live claim at `fence` and its
    /// delegated [`Permission::PushBranch`] for its repository are checked
    /// against the house's current grants. The pull request and the remote
    /// head are then read, the push is checked against them, and the update
    /// is a compare-and-swap on the head that was read.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] when `fence` no longer holds a live
    /// claim on the task, [`StateError::CancelRequested`] when the task's
    /// cancellation is pending, and contract errors such as
    /// [`crate::contracts::ContractError::PermissionDenied`] when the task
    /// lacks the grant or the house revoked it. A refused, stale, or
    /// uncertain push is an [`PushOutcome`], not an error.
    pub fn push(
        &self,
        task: &TaskId,
        fence: Fence,
        intent: &PushIntent,
        commit: &CommitId,
    ) -> Result<PushOutcome> {
        let record = self.store.task(task)?;
        match record.state() {
            TaskState::Claimed { lease }
                if lease.fence() == fence && lease.is_live(self.clock.now()) => {}
            TaskState::Claimed { .. } | TaskState::Open | TaskState::Settled { .. } => {
                return Err(StateError::StaleFence { presented: fence }.into());
            }
        }
        // Like every other effect, nothing starts once cancellation is pending.
        if record.cancel_request().is_some() {
            return Err(StateError::CancelRequested.into());
        }
        let repository = record
            .spec()
            .repository
            .clone()
            .ok_or(CoordinationError::MissingRepository)?;
        record.spec().authority.authorize(
            self.grants,
            Permission::PushBranch,
            &GrantScope::Repository(repository),
            self.destination,
        )?;

        let observed = PushObservation {
            pull_request: match intent.pull_request {
                Some(number) => self.pull_requests.pull_request(number),
                None => Observed::Known(None),
            },
            remote_head: self.remote.head(self.branch),
        };
        let permit = match decide(self.branch, intent, &observed, commit) {
            Ok(Decision::Update(permit)) => permit,
            Ok(Decision::Current) => return Ok(PushOutcome::AlreadyCurrent),
            Err(refusal) => return Ok(PushOutcome::Refused(refusal)),
        };
        Ok(match self.updater.update(&permit, self.branch, commit) {
            Ok(()) => PushOutcome::Pushed {
                replaced: permit.replaces,
            },
            Err(UpdateFailure::Rejected) => PushOutcome::Stale,
            Err(UpdateFailure::Uncertain) => PushOutcome::Uncertain,
        })
    }
}

/// Pull request reads through #7's house-scoped GitHub client. The client
/// reports a missing pull request as unreadable, so it is `Unknown` here,
/// which refuses the push all the same.
pub struct GitHubPullRequests<'a, T: GitHubReadTransport> {
    /// The house-scoped client.
    pub client: &'a GitHubClient<T>,
    /// The house the read is scoped to.
    pub house: &'a HouseId,
    /// The repository the pull request lives in.
    pub repository: &'a Repository,
}

impl<T: GitHubReadTransport> PullRequests for GitHubPullRequests<'_, T> {
    fn pull_request(&self, number: IssueNumber) -> Observed<Option<PullRequestView>> {
        match observe_pull_request(self.client, self.house, self.repository, number) {
            Observed::Known(view) => Observed::Known(Some(view)),
            Observed::Unknown => Observed::Unknown,
        }
    }
}

/// Longest Git output read, in bytes.
const MAX_GIT_OUTPUT: u64 = 64 * 1024;

/// How often a running Git process is checked against its deadline.
const GIT_POLL: Duration = Duration::from_millis(10);

/// A Git remote reached through the `git` executable in a worker's
/// checkout: reads branch heads with `git ls-remote` and updates them with
/// `git push --force-with-lease=<ref>:<expected>`, which the remote applies
/// only if the ref still holds the expected value. It uses the credentials
/// the checkout's Git already has and never prompts. Every call has a
/// deadline; an expired call reports unknown or uncertain, never success.
#[derive(Debug, Clone)]
pub struct GitRemote {
    git: PathBuf,
    worktree: PathBuf,
    remote: String,
    deadline: Duration,
}

impl GitRemote {
    /// Longest remote name accepted, in bytes.
    pub const MAX_REMOTE_BYTES: usize = 64;

    /// Bind the `git` executable, the checkout it runs in, the remote name
    /// (such as `origin`), and the deadline for each call.
    ///
    /// # Errors
    /// Returns [`CoordinationError::InvalidGitRemote`] for a relative
    /// executable or checkout path, a zero deadline, or a remote name that is
    /// empty, too long, starts with `-`, or has characters other than ASCII
    /// letters, digits, and `._-`.
    pub fn new(
        git: PathBuf,
        worktree: PathBuf,
        remote: &str,
        deadline: Duration,
    ) -> std::result::Result<Self, CoordinationError> {
        let plain = !remote.is_empty()
            && remote.len() <= Self::MAX_REMOTE_BYTES
            && !remote.starts_with('-')
            && remote
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !git.is_absolute() || !worktree.is_absolute() || deadline.is_zero() || !plain {
            return Err(CoordinationError::InvalidGitRemote);
        }
        Ok(Self {
            git,
            worktree,
            remote: remote.to_owned(),
            deadline,
        })
    }

    /// Run `git` with `args` and return its exit code and bounded stdout.
    /// `None` means the process did not complete: it could not start, ran
    /// past the deadline, or produced too much output.
    fn run(&self, args: &[&str]) -> Option<(Option<i32>, Vec<u8>)> {
        let mut output = tempfile::tempfile().ok()?;
        let mut child = Command::new(&self.git)
            .args(args)
            .current_dir(&self.worktree)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            // A caller inside a Git hook must not redirect Git to its own repository.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().ok()?))
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < self.deadline => thread::sleep(GIT_POLL),
                Ok(None) | Err(_) => {
                    // Best effort: the process may already have exited.
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        Some((status.code(), read_bounded(&mut output)?))
    }
}

/// Read `file` from the start, refusing more than [`MAX_GIT_OUTPUT`] bytes.
fn read_bounded(file: &mut File) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_GIT_OUTPUT.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (u64::try_from(bytes.len()).ok()? <= MAX_GIT_OUTPUT).then_some(bytes)
}

impl RemoteBranches for GitRemote {
    fn head(&self, branch: &BranchName) -> Observed<Option<CommitId>> {
        let reference = format!("refs/heads/{branch}");
        let Some((Some(0), stdout)) = self.run(&["ls-remote", &self.remote, &reference]) else {
            return Observed::Unknown;
        };
        let Ok(stdout) = String::from_utf8(stdout) else {
            return Observed::Unknown;
        };
        // `ls-remote` matches patterns by suffix; only the exact ref counts.
        let mut heads = stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter(|(_, name)| *name == reference);
        match (heads.next(), heads.next()) {
            (None, _) => Observed::Known(None),
            (Some((id, _)), None) => match CommitId::new(id) {
                Ok(id) => Observed::Known(Some(id)),
                Err(_) => Observed::Unknown,
            },
            (Some(_), Some(_)) => Observed::Unknown,
        }
    }
}

impl RefUpdater for GitRemote {
    fn update(
        &self,
        permit: &PushPermit,
        branch: &BranchName,
        commit: &CommitId,
    ) -> std::result::Result<(), UpdateFailure> {
        let reference = format!("refs/heads/{branch}");
        // An empty expected value means the ref must not exist.
        let lease = format!(
            "--force-with-lease={reference}:{}",
            permit.replaces().map_or("", CommitId::as_str)
        );
        let refspec = format!("{commit}:{reference}");
        match self.run(&["push", "--porcelain", &lease, &self.remote, &refspec]) {
            Some((Some(0), _)) => Ok(()),
            // Git reports a refused ref, such as a failed lease, as a line
            // starting with `!` under `--porcelain`: nothing changed.
            Some((Some(1), stdout))
                if stdout
                    .split(|byte| *byte == b'\n')
                    .any(|line| line.starts_with(b"!")) =>
            {
                Err(UpdateFailure::Rejected)
            }
            Some(_) | None => Err(UpdateFailure::Uncertain),
        }
    }
}
