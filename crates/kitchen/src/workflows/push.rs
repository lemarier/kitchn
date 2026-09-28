//! The push boundary: every branch update a worker makes for a task goes
//! through [`PushBoundary::push`], which authorizes the task, binds the push
//! to the one branch the task's durable record names, checks that the Git
//! remote is the granted repository, reads the pull request and the remote
//! branch head, refuses stale state, and updates the remote ref only if it
//! still holds the head that was checked.
//!
//! The decision and the update cannot be separated. The only value that lets
//! a [`RefUpdater`] act is the [`PushPermit`] the boundary builds from
//! observations it took itself, and the update is a compare-and-swap on that
//! permit: a branch that changes between the check and the update makes the
//! update fail instead of overwriting the change. [`GitRemote`] is the Git
//! implementation of both remote traits.
//!
//! Nothing about the branch comes from the writer: the branch is the task's
//! latest launched branch ([`crate::workflows::coordination::task_branch`]),
//! a branch a person holds is refused, the layer comes from the launch
//! record and the pull request's observed base, and once a push checked the
//! pull request or published the branch, later intents cannot drop the pull
//! request or recreate the branch as a first push.
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
        BranchName, Clock, CommitId, Fence, GrantScope, HouseGrants, IssueNumber, Permission,
        Repository,
    },
    house::StackTool,
    integrations::github::{GitHubClient, GitHubReadTransport, Observation},
    state::{HouseStore, StateError, TaskRecord, TaskState},
    workflows::{
        coordination::{BranchFact, CoordinationError, held_branches, task_branch},
        repair::{Observed, PullRequestState, PullRequestView, observe_pull_request},
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// What a writer is about to push to the branch its task owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushIntent {
    /// The pull request the branch belongs to, once opened. Once a push
    /// checked it, every later intent must name it.
    pub pull_request: Option<IssueNumber>,
    /// The remote head the writer last saw; `None` before the first push.
    /// Once the branch was published through a boundary, `None` is refused.
    pub expected_remote: Option<CommitId>,
}

/// State read immediately before the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushObservation {
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
    /// The branch was deleted, or a first push would recreate a branch this
    /// task already published.
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
    /// An earlier push checked the branch's pull request, and this intent
    /// names none.
    PullRequestRequired,
    /// The branch is a dependent layer and the house configures a stack
    /// tool: push it through [`crate::workflows::stack::StackBoundary`].
    StackToolRequired(StackTool),
    /// The task has launched no worker on a branch: nothing to push.
    NoBranch,
    /// A person took over the terminal of the worker on this branch. The
    /// branch is theirs; the task's replacement works on another one.
    BranchHeld,
    /// The Git remote is not the repository the task's grant names, or it is
    /// redirected elsewhere.
    RemoteMismatch,
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
pub(crate) enum Decision {
    /// Update the remote ref under this permit.
    Update(PushPermit),
    /// The remote already holds the commit.
    Current,
}

/// Check PR and branch state immediately before a push of `commit`, or of
/// whatever a stack tool pushes when `commit` is `None`. Every push needs a
/// fresh check; a stale permit proves nothing.
pub(crate) fn decide(
    bound: &Binding,
    intent: &PushIntent,
    observed: &PushObservation,
    commit: Option<&CommitId>,
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
        if pull_request.head_branch != bound.branch.as_str() {
            return Err(PushRefusal::WrongBranch);
        }
    }
    let Observed::Known(remote) = &observed.remote_head else {
        return Err(PushRefusal::Unknown);
    };
    match (&intent.expected_remote, remote) {
        // An earlier push of this commit landed and its answer was lost.
        (_, Some(found)) if Some(found) == commit => Ok(Decision::Current),
        // A branch this task published and that is gone now was merged or
        // deleted: never recreate it.
        (None, None) if bound.published => Err(PushRefusal::BranchDeleted),
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

    /// The repository's default branch: a pull request based on anything
    /// else is a dependent layer.
    fn default_branch(&self) -> Observed<BranchName>;
}

/// Reads remote branch heads.
pub trait RemoteBranches {
    /// Whether reads go to `repository` and nowhere else. `Unknown` refuses.
    fn reads_from(&self, repository: &Repository) -> Observed<bool>;

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
    /// Whether updates go to `repository` and nowhere else. `Unknown`
    /// refuses.
    fn pushes_to(&self, repository: &Repository) -> Observed<bool>;

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

/// The task's branch as its durable record describes it, after the task's
/// claim and grants were checked.
pub(crate) struct Binding {
    /// The repository the grants name.
    pub(crate) repository: Repository,
    /// The task's branch.
    pub(crate) branch: BranchName,
    /// It was launched as a stack layer.
    pub(crate) stacked: bool,
    /// A push already published it.
    pub(crate) published: bool,
    /// A push already checked its pull request.
    pub(crate) pull_request_bound: bool,
}

/// Check the live claim at `fence`, a pending cancellation, and every
/// permission in `permissions` for the task's repository, then bind the
/// task's branch from its record. Nothing is read or sent before this.
pub(crate) fn bind(
    store: &HouseStore,
    grants: &HouseGrants,
    destination: &BackendId,
    clock: &dyn Clock,
    task: &TaskId,
    fence: Fence,
    permissions: &[Permission],
) -> Result<(TaskRecord, std::result::Result<Binding, PushRefusal>)> {
    let record = store.task(task)?;
    match record.state() {
        TaskState::Claimed { lease } if lease.fence() == fence && lease.is_live(clock.now()) => {}
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
    for permission in permissions {
        record.spec().authority.authorize(
            grants,
            *permission,
            &GrantScope::Repository(repository.clone()),
            destination,
        )?;
    }
    let Some(branch) = task_branch(&record) else {
        return Ok((record, Err(PushRefusal::NoBranch)));
    };
    if held_branches(&record).contains(&branch) {
        return Ok((record, Err(PushRefusal::BranchHeld)));
    }
    let binding = Binding {
        stacked: BranchFact::Stacked.holds(&record, &branch),
        published: BranchFact::Published.holds(&record, &branch),
        pull_request_bound: BranchFact::PullRequestBound.holds(&record, &branch),
        repository,
        branch,
    };
    Ok((record, Ok(binding)))
}

/// The pull request an intent names, read, and, when `layer` is asked for,
/// whether the branch is a dependent layer by its observed base.
pub(crate) fn observe(
    binding: &Binding,
    intent: &PushIntent,
    pull_requests: &dyn PullRequests,
    remote: &dyn RemoteBranches,
    layer: bool,
) -> std::result::Result<(PushObservation, Observed<bool>), PushRefusal> {
    if binding.pull_request_bound && intent.pull_request.is_none() {
        return Err(PushRefusal::PullRequestRequired);
    }
    let pull_request = match intent.pull_request {
        Some(number) => pull_requests.pull_request(number),
        None => Observed::Known(None),
    };
    let dependent = match &pull_request {
        Observed::Known(_) if !layer => Observed::Known(false),
        Observed::Known(Some(view)) => match pull_requests.default_branch() {
            Observed::Known(default) => Observed::Known(view.base_branch != default.as_str()),
            Observed::Unknown => Observed::Unknown,
        },
        Observed::Known(None) => Observed::Known(false),
        Observed::Unknown => Observed::Unknown,
    };
    let observed = PushObservation {
        pull_request,
        remote_head: remote.head(&binding.branch),
    };
    Ok((observed, dependent))
}

/// Record what a landed push established: the branch is published and, when
/// the intent named a checked pull request, later pushes must name it.
pub(crate) fn record_landed(
    store: &HouseStore,
    clock: &dyn Clock,
    task: &TaskId,
    fence: Fence,
    binding: &Binding,
    intent: &PushIntent,
) -> Result<()> {
    let now = clock.now();
    if !binding.published {
        BranchFact::Published.record(store, task, fence, &binding.branch, now)?;
    }
    if intent.pull_request.is_some() && !binding.pull_request_bound {
        BranchFact::PullRequestBound.record(store, task, fence, &binding.branch, now)?;
    }
    Ok(())
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
    /// The house's configured stack tool, if any.
    pub stack_tool: Option<StackTool>,
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
    /// Push `commit` to the task's branch.
    ///
    /// Nothing is read or sent until the task's live claim at `fence` and its
    /// delegated [`Permission::PushBranch`] for its repository are checked
    /// against the house's current grants. The branch is the task's latest
    /// launched branch; a branch a person holds is refused. A branch launched
    /// as a stack layer is refused when the house configures a stack tool.
    /// The remote must be the granted repository. The pull request and the
    /// remote head are then read, a pull request based on another branch
    /// than the default is refused under a stack tool, the push is checked
    /// against both, and the update is a compare-and-swap on the head that
    /// was read. A landed push is recorded, so a later first-push intent
    /// cannot recreate the branch and a later intent must name the checked
    /// pull request.
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
        let (_, binding) = bind(
            self.store,
            self.grants,
            self.destination,
            self.clock,
            task,
            fence,
            &[Permission::PushBranch],
        )?;
        let binding = match binding {
            Ok(binding) => binding,
            Err(refusal) => return Ok(PushOutcome::Refused(refusal)),
        };
        // A branch launched as a layer is a dependent layer: only the stack
        // tool pushes it, before anything is read.
        if binding.stacked
            && let Some(tool) = self.stack_tool
        {
            return Ok(PushOutcome::Refused(PushRefusal::StackToolRequired(tool)));
        }
        match (
            self.remote.reads_from(&binding.repository),
            self.updater.pushes_to(&binding.repository),
        ) {
            (Observed::Known(true), Observed::Known(true)) => {}
            (Observed::Known(false), _) | (_, Observed::Known(false)) => {
                return Ok(PushOutcome::Refused(PushRefusal::RemoteMismatch));
            }
            (Observed::Unknown, _) | (_, Observed::Unknown) => {
                return Ok(PushOutcome::Refused(PushRefusal::Unknown));
            }
        }
        let (observed, dependent) = match observe(
            &binding,
            intent,
            self.pull_requests,
            self.remote,
            self.stack_tool.is_some(),
        ) {
            Ok(observed) => observed,
            Err(refusal) => return Ok(PushOutcome::Refused(refusal)),
        };
        if let Some(tool) = self.stack_tool {
            match dependent {
                Observed::Known(false) => {}
                Observed::Known(true) => {
                    return Ok(PushOutcome::Refused(PushRefusal::StackToolRequired(tool)));
                }
                Observed::Unknown => return Ok(PushOutcome::Refused(PushRefusal::Unknown)),
            }
        }
        let permit = match decide(&binding, intent, &observed, Some(commit)) {
            Ok(Decision::Update(permit)) => permit,
            Ok(Decision::Current) => {
                record_landed(self.store, self.clock, task, fence, &binding, intent)?;
                return Ok(PushOutcome::AlreadyCurrent);
            }
            Err(refusal) => return Ok(PushOutcome::Refused(refusal)),
        };
        Ok(
            match self.updater.update(&permit, &binding.branch, commit) {
                Ok(()) => {
                    record_landed(self.store, self.clock, task, fence, &binding, intent)?;
                    PushOutcome::Pushed {
                        replaced: permit.replaces,
                    }
                }
                Err(UpdateFailure::Rejected) => PushOutcome::Stale,
                Err(UpdateFailure::Uncertain) => PushOutcome::Uncertain,
            },
        )
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

    fn default_branch(&self) -> Observed<BranchName> {
        match self.client.repository(self.house, self.repository) {
            Observation::Known(info) => {
                BranchName::new(&info.default_branch).map_or(Observed::Unknown, Observed::Known)
            }
            Observation::Unavailable(_) | Observation::Unknown => Observed::Unknown,
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
///
/// The checkout belongs to the worker, so its Git configuration is not
/// trusted. Before any read or push, the remote's fetch and push URLs, with
/// `insteadOf` and `pushInsteadOf` rewrites applied, must both be the
/// granted repository under one of the accepted URL bases (GitHub's HTTPS
/// and SSH forms by default). Every call disables hooks and pins the SSH
/// command, the remote's pack programs, and a push's tags, submodules, and
/// mirroring. Credential helpers configured in
/// the checkout still run; a Kitchen-owned clone removes that limit.
#[derive(Debug, Clone)]
pub struct GitRemote {
    git: PathBuf,
    worktree: PathBuf,
    remote: String,
    deadline: Duration,
    url_bases: Vec<String>,
}

/// The URL prefixes of GitHub repositories, followed by `owner/name`.
const GITHUB_URL_BASES: [&str; 3] = [
    "https://github.com/",
    "git@github.com:",
    "ssh://git@github.com/",
];

/// Longest accepted URL base, in bytes.
const MAX_URL_BASE_BYTES: usize = 512;

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
            url_bases: GITHUB_URL_BASES
                .iter()
                .map(|base| (*base).to_owned())
                .collect(),
        })
    }

    /// Accept repositories under these URL bases instead of GitHub's, such
    /// as another forge's `https://host/` or a directory of bare
    /// repositories. The remote's URL must be a base followed by
    /// `owner/name`, optionally with `.git`.
    ///
    /// # Errors
    /// Returns [`CoordinationError::InvalidGitRemote`] for no bases, or a
    /// base that is empty, longer than 512 bytes, or has whitespace or
    /// control characters.
    pub fn with_url_bases(
        mut self,
        bases: &[&str],
    ) -> std::result::Result<Self, CoordinationError> {
        let valid = |base: &&str| {
            !base.is_empty()
                && base.len() <= MAX_URL_BASE_BYTES
                && !base
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        };
        if bases.is_empty() || !bases.iter().all(valid) {
            return Err(CoordinationError::InvalidGitRemote);
        }
        self.url_bases = bases.iter().map(|base| (*base).to_owned()).collect();
        Ok(self)
    }

    fn run(&self, args: &[&str]) -> Option<(Option<i32>, Vec<u8>)> {
        let pins = pinned_git_config(&self.remote);
        // The worker can edit the checkout's configuration: its hooks, SSH
        // command, pack programs, and what a push carries along must not
        // change under Kitchen.
        let pairs: Vec<String> = pins
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let mut full: Vec<&str> = Vec::with_capacity(pairs.len() * 2 + args.len());
        for pair in &pairs {
            full.extend(["-c", pair]);
        }
        full.extend_from_slice(args);
        run_bounded(
            &self.git,
            &self.worktree,
            &full,
            &[("GIT_TERMINAL_PROMPT", "0"), ("GIT_SSH_COMMAND", "ssh")],
            self.deadline,
        )
    }

    /// Whether `url` names `repository` under an accepted base.
    fn names(&self, url: &str, repository: &Repository) -> bool {
        let path = repository.as_str();
        self.url_bases.iter().any(|base| {
            url.strip_prefix(base.as_str()).is_some_and(|rest| {
                let rest = rest.strip_suffix(".git").unwrap_or(rest);
                rest.eq_ignore_ascii_case(path)
            })
        })
    }

    /// The remote's single URL after rewrites, for fetching or pushing.
    fn url(&self, push: bool) -> Option<String> {
        let mut args = vec!["remote", "get-url"];
        if push {
            args.push("--push");
        }
        args.push(&self.remote);
        let (Some(0), stdout) = self.run(&args)? else {
            return None;
        };
        let text = String::from_utf8(stdout).ok()?;
        let mut lines = text.lines();
        match (lines.next(), lines.next()) {
            (Some(url), None) => Some(url.to_owned()),
            _ => None,
        }
    }

    fn bound_to(&self, repository: &Repository, push: bool) -> Observed<bool> {
        self.url(push).map_or(Observed::Unknown, |url| {
            Observed::Known(self.names(&url, repository))
        })
    }
}

/// The Git configuration every Kitchen-run Git command pins over the
/// checkout's own: no hooks, a plain `ssh`, the standard pack programs, and a
/// push that carries only the ref it names (no tags, no submodules, no
/// mirroring). Kitchen's direct Git calls pass these as `-c`; the stack tool's
/// Git reads them from [`git_config_env`].
pub(crate) fn pinned_git_config(remote: &str) -> Vec<(String, String)> {
    [
        ("core.hooksPath".to_owned(), "/dev/null".to_owned()),
        ("core.sshCommand".to_owned(), "ssh".to_owned()),
        (
            format!("remote.{remote}.receivepack"),
            "git-receive-pack".to_owned(),
        ),
        (
            format!("remote.{remote}.uploadpack"),
            "git-upload-pack".to_owned(),
        ),
        (format!("remote.{remote}.mirror"), "false".to_owned()),
        ("push.recurseSubmodules".to_owned(), "no".to_owned()),
        ("push.followTags".to_owned(), "false".to_owned()),
        ("submodule.recurse".to_owned(), "false".to_owned()),
    ]
    .into()
}

/// [`pinned_git_config`] as `GIT_CONFIG_COUNT` environment entries, which
/// Git ranks above every configuration file, for tools that run Git
/// themselves.
pub(crate) fn git_config_env(remote: &str) -> Vec<(String, String)> {
    let pins = pinned_git_config(remote);
    let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), pins.len().to_string())];
    for (index, (key, value)) in pins.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value));
    }
    env
}

/// Run `program` in `dir` with `args` and `env`, stdin closed and stdout
/// captured to a file (never a terminal), and return its exit code and
/// bounded stdout. `None` means the process did not complete: it could not
/// start, ran past `deadline`, or produced too much output. Meant for Git
/// and Git-driven tools: it sets `LC_ALL=C` and drops `GIT_DIR` and
/// `GIT_WORK_TREE`.
pub(crate) fn run_bounded(
    program: &std::path::Path,
    dir: &std::path::Path,
    args: &[&str],
    env: &[(&str, &str)],
    deadline: Duration,
) -> Option<(Option<i32>, Vec<u8>)> {
    let mut output = tempfile::tempfile().ok()?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .envs(env.iter().copied())
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
            Ok(None) if started.elapsed() < deadline => thread::sleep(GIT_POLL),
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
    fn reads_from(&self, repository: &Repository) -> Observed<bool> {
        self.bound_to(repository, false)
    }

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
    fn pushes_to(&self, repository: &Repository) -> Observed<bool> {
        self.bound_to(repository, true)
    }

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
        match self.run(&[
            "push",
            "--porcelain",
            "--no-follow-tags",
            "--no-recurse-submodules",
            &lease,
            &self.remote,
            &refspec,
        ]) {
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
