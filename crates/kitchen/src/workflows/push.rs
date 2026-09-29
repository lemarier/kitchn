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
    /// The checkout's own Git configuration rewrites URLs or points a remote
    /// at another repository. The key names the first such entry.
    CheckoutRedirect(GitConfigKey),
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
    repository: Repository,
}

impl PushPermit {
    /// The remote head the update may replace: the compare value of the
    /// compare-and-swap. `None` means the branch must not exist.
    #[must_use]
    pub const fn replaces(&self) -> Option<&CommitId> {
        self.replaces.as_ref()
    }

    /// The repository the check granted the push to. An updater sends to
    /// this repository and nowhere else.
    #[must_use]
    pub const fn repository(&self) -> &Repository {
        &self.repository
    }
}

/// One branch of a [`LayersPermit`]: point `branch` at `commit` only if it
/// still holds `replaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerUpdate {
    branch: BranchName,
    replaces: Option<CommitId>,
    commit: CommitId,
}

impl LayerUpdate {
    pub(crate) const fn new(
        branch: BranchName,
        replaces: Option<CommitId>,
        commit: CommitId,
    ) -> Self {
        Self {
            branch,
            replaces,
            commit,
        }
    }

    /// The branch.
    #[must_use]
    pub const fn branch(&self) -> &BranchName {
        &self.branch
    }

    /// The remote head the update may replace; `None` means the branch must
    /// not exist.
    #[must_use]
    pub const fn replaces(&self) -> Option<&CommitId> {
        self.replaces.as_ref()
    }

    /// The commit the branch points at afterwards. For a layer below the
    /// task's branch it is the head that was checked, so the update only
    /// holds the layer to that head.
    #[must_use]
    pub const fn commit(&self) -> &CommitId {
        &self.commit
    }
}

/// Proof that every layer of a stack push was checked against fresh state,
/// and the head each may replace. Only
/// [`crate::workflows::stack::StackBoundary`] builds one. The update is one
/// compare-and-swap over every branch: if any branch no longer holds its
/// [`LayerUpdate::replaces`], none changes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct LayersPermit {
    repository: Repository,
    updates: Vec<LayerUpdate>,
}

impl LayersPermit {
    pub(crate) const fn new(repository: Repository, updates: Vec<LayerUpdate>) -> Self {
        Self {
            repository,
            updates,
        }
    }

    /// The repository the check granted the push to.
    #[must_use]
    pub const fn repository(&self) -> &Repository {
        &self.repository
    }

    /// The branches, bottom to top.
    #[must_use]
    pub fn updates(&self) -> &[LayerUpdate] {
        &self.updates
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
        (None, None) => Ok(Decision::Update(PushPermit {
            replaces: None,
            repository: bound.repository.clone(),
        })),
        (None, Some(_)) => Err(PushRefusal::BranchExists),
        (Some(_), None) => Err(PushRefusal::BranchDeleted),
        (Some(expected), Some(found)) if expected == found => Ok(Decision::Update(PushPermit {
            replaces: Some(found.clone()),
            repository: bound.repository.clone(),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateFailure {
    /// The remote refused, or the branch no longer held the permit's head.
    /// Nothing changed.
    Rejected,
    /// The checkout's Git configuration gained this redirecting entry after
    /// the check. Nothing was sent.
    Redirected(GitConfigKey),
    /// The remote may or may not have applied the update.
    Uncertain,
}

/// Updates a remote branch with compare-and-swap semantics.
pub trait RefUpdater {
    /// Whether updates go to `repository` and nowhere else. `Unknown`
    /// refuses.
    fn pushes_to(&self, repository: &Repository) -> Observed<bool>;

    /// The first entry of the checkout's own configuration that could send
    /// an update anywhere but `repository`: any URL rewrite, or a remote URL
    /// that is not `repository`. `Known(None)` when there is none; `Unknown`
    /// refuses.
    fn redirect(&self, repository: &Repository) -> Observed<Option<GitConfigKey>>;

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

    /// Apply every [`LayerUpdate`] of `permit` atomically: all of them if
    /// every branch still holds its [`LayerUpdate::replaces`], none
    /// otherwise, with [`UpdateFailure::Rejected`]. A remote that cannot
    /// apply them atomically must refuse, not apply some.
    ///
    /// # Errors
    /// Returns an [`UpdateFailure`] distinguishing "nothing changed" from
    /// "outcome unknown".
    fn update_layers(&self, permit: &LayersPermit) -> std::result::Result<(), UpdateFailure>;
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
        match self.updater.redirect(&binding.repository) {
            Observed::Known(None) => {}
            Observed::Known(Some(key)) => {
                return Ok(PushOutcome::Refused(PushRefusal::CheckoutRedirect(key)));
            }
            Observed::Unknown => return Ok(PushOutcome::Refused(PushRefusal::Unknown)),
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
                Err(UpdateFailure::Redirected(key)) => {
                    PushOutcome::Refused(PushRefusal::CheckoutRedirect(key))
                }
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
/// only if the ref still holds the expected value. Several branches go in
/// one `git push --atomic`, each with its own lease; a remote that does not
/// support atomic pushes fails the push rather than applying part of it.
/// It never prompts. Every call has a deadline; an expired call reports
/// unknown or uncertain, never success.
///
/// Every call runs Git under one environment: no system configuration,
/// Kitchen's own [`IsolatedGitConfig`] in place of the user's global one,
/// hooks disabled, and the SSH command, the remote's pack programs, and a
/// push's tags, submodules, and mirroring pinned. What remains is the
/// checkout's own configuration, which belongs to the worker and is not
/// trusted: before any read or push, and again immediately before the push
/// runs, it must hold no URL rewrite (`url.*.insteadOf`,
/// `url.*.pushInsteadOf`) and no remote URL other than the granted
/// repository ([`RefUpdater::redirect`]). Every fetch URL and every push URL
/// of the remote (`git remote get-url --all`) must also be the granted
/// repository under one of the accepted URL bases (GitHub's HTTPS and SSH
/// forms by default). Credential helpers configured in the checkout still
/// run; a Kitchen-owned clone removes that limit.
///
/// An update does not push by remote name: it pushes to the verified URL,
/// which no configuration Git reads may rewrite. The checks and the push
/// are separate Git processes, so a worker still writing to its checkout's
/// configuration while Kitchen pushes can add a rewrite after the last
/// check; the two reads narrow that to the instants between the final check
/// and Git's own read.
#[derive(Debug, Clone)]
pub struct GitRemote {
    git: PathBuf,
    worktree: PathBuf,
    remote: String,
    config: IsolatedGitConfig,
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
    /// (such as `origin`), the Git configuration Kitchen gives every call,
    /// and the deadline for each call.
    ///
    /// # Errors
    /// Returns [`CoordinationError::InvalidGitRemote`] for a relative
    /// executable or checkout path, a zero deadline, or a remote name that is
    /// empty, too long, starts with `-`, or has characters other than ASCII
    /// letters, digits, and `._-`, and
    /// [`CoordinationError::InvalidGitConfig`] for a configuration file
    /// inside the checkout.
    pub fn new(
        git: PathBuf,
        worktree: PathBuf,
        remote: &str,
        config: IsolatedGitConfig,
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
        if config.inside(&worktree) {
            return Err(CoordinationError::InvalidGitConfig);
        }
        Ok(Self {
            git,
            worktree,
            remote: remote.to_owned(),
            config,
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

    pub(crate) fn run(&self, args: &[&str]) -> Option<(Option<i32>, Vec<u8>)> {
        let env = git_environment(&self.config, &self.remote);
        let env: Vec<(&str, &str)> = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        run_bounded(&self.git, &self.worktree, args, &env, self.deadline)
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

    /// Every URL of the remote after rewrites, for fetching or pushing.
    /// `None` when Git failed, printed nothing, or printed non-UTF-8.
    fn urls(&self, push: bool) -> Option<Vec<String>> {
        let mut args = vec!["remote", "get-url", "--all"];
        if push {
            args.push("--push");
        }
        args.push(&self.remote);
        let (Some(0), stdout) = self.run(&args)? else {
            return None;
        };
        let text = String::from_utf8(stdout).ok()?;
        let urls: Vec<String> = text.lines().map(str::to_owned).collect();
        (!urls.is_empty()).then_some(urls)
    }

    fn bound_to(&self, repository: &Repository, push: bool) -> Observed<bool> {
        self.urls(push).map_or(Observed::Unknown, |urls| {
            Observed::Known(urls.iter().all(|url| self.names(url, repository)))
        })
    }

    /// The first URL rewrite, or remote URL that does not name `repository`,
    /// in the configuration Git reads under [`git_environment`]: the
    /// checkout's local and worktree files and what they include, since the
    /// system file is off and Kitchen's own file holds neither.
    fn redirecting_entry(&self, repository: &Repository) -> Observed<Option<GitConfigKey>> {
        let Some((code, stdout)) = self.run(&["config", "--null", "--get-regexp", REDIRECT_KEYS])
        else {
            return Observed::Unknown;
        };
        match code {
            // `--get-regexp` exits 1 when no key matches.
            Some(1) => Observed::Known(None),
            Some(0) => {
                let Ok(text) = String::from_utf8(stdout) else {
                    return Observed::Unknown;
                };
                // `--null` ends each entry with NUL and puts a newline
                // between its key and value.
                let entries: Vec<(&str, &str)> = text
                    .split_terminator('\0')
                    .map(|entry| entry.split_once('\n').unwrap_or((entry, "")))
                    .collect();
                // A rewrite is named first: it redirects even a verified URL.
                let offending = entries
                    .iter()
                    .find(|(key, _)| key.starts_with("url."))
                    .or_else(|| {
                        entries
                            .iter()
                            .find(|(_, value)| !self.names(value, repository))
                    });
                Observed::Known(offending.map(|(key, _)| GitConfigKey((*key).to_owned())))
            }
            _ => Observed::Unknown,
        }
    }
}

/// Keys that decide where Git sends a push: URL rewrites and remote URLs.
/// Git matches this against keys with the section and variable in lowercase.
const REDIRECT_KEYS: &str = r"^(url\..*\.(insteadof|pushinsteadof)|remote\..*\.(url|pushurl))$";

/// A Git configuration key read from a checkout, such as
/// `url.https://example.com/.insteadof`, with its section and variable in
/// lowercase as Git prints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitConfigKey(String);

impl GitConfigKey {
    /// The key as Git printed it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GitConfigKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One entry of Kitchen's own Git configuration: only what a push or a
/// stack rebase needs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushSetting {
    /// `credential.helper`, or `credential.<url>.helper` for one URL prefix.
    CredentialHelper {
        /// The URL prefix the helper serves; `None` for every URL.
        url: Option<String>,
        /// The helper, as Git's `credential.helper` takes it.
        helper: String,
    },
    /// `user.name`, for commits a stack rebase rewrites.
    UserName(String),
    /// `user.email`, for commits a stack rebase rewrites.
    UserEmail(String),
}

impl PushSetting {
    fn key(&self) -> String {
        match self {
            Self::CredentialHelper { url: None, .. } => "credential.helper".to_owned(),
            Self::CredentialHelper { url: Some(url), .. } => format!("credential.{url}.helper"),
            Self::UserName(_) => "user.name".to_owned(),
            Self::UserEmail(_) => "user.email".to_owned(),
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::CredentialHelper { helper, .. } => helper,
            Self::UserName(value) | Self::UserEmail(value) => value,
        }
    }

    /// A value on one line, and a URL prefix without whitespace.
    fn is_plain(&self) -> bool {
        let line = |text: &str| {
            !text.is_empty()
                && text.len() <= MAX_SETTING_BYTES
                && !text.chars().any(char::is_control)
        };
        let url_ok = match self {
            Self::CredentialHelper { url: Some(url), .. } => {
                line(url) && !url.chars().any(char::is_whitespace)
            }
            Self::CredentialHelper { url: None, .. } | Self::UserName(_) | Self::UserEmail(_) => {
                true
            }
        };
        url_ok && line(self.value())
    }
}

/// Longest [`PushSetting`] value or URL prefix, in bytes.
const MAX_SETTING_BYTES: usize = 1024;

/// The Git configuration file Kitchen writes and gives its Git commands as
/// their global configuration (`GIT_CONFIG_GLOBAL`), with the system file
/// off, so the user's and the system's configuration cannot rewrite where a
/// push goes. It holds only the [`PushSetting`]s it was created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedGitConfig {
    path: PathBuf,
}

impl IsolatedGitConfig {
    /// Most settings one file holds.
    pub const MAX_SETTINGS: usize = 16;

    /// Write `settings`, in order, to `path`, replacing whatever the file
    /// held, with `git` and a deadline for each write. The file must live in
    /// a directory Kitchen owns, outside every checkout it pushes from.
    ///
    /// # Errors
    /// Returns [`CoordinationError::InvalidGitConfig`] for a relative or
    /// non-UTF-8 path, a relative `git`, a zero deadline, more than
    /// [`Self::MAX_SETTINGS`] settings, or a setting that is empty, longer
    /// than 1024 bytes, or has control characters (and whitespace, in a URL
    /// prefix), and [`CoordinationError::GitConfigUnwritten`] when the file
    /// or a setting could not be written.
    pub fn create(
        git: &std::path::Path,
        path: PathBuf,
        settings: &[PushSetting],
        deadline: Duration,
    ) -> std::result::Result<Self, CoordinationError> {
        if path.to_str().is_none() || !path.is_absolute() {
            return Err(CoordinationError::InvalidGitConfig);
        }
        let Some(dir) = path.parent() else {
            return Err(CoordinationError::InvalidGitConfig);
        };
        if !git.is_absolute()
            || deadline.is_zero()
            || settings.len() > Self::MAX_SETTINGS
            || !settings.iter().all(PushSetting::is_plain)
        {
            return Err(CoordinationError::InvalidGitConfig);
        }
        // Write a new file beside the old one and move it into place only
        // once complete, so a failed write never leaves a partial file at
        // `path`. The file is created readable by its owner only.
        let staged = tempfile::NamedTempFile::new_in(dir)
            .map_err(|_| CoordinationError::GitConfigUnwritten)?;
        let Some(staged_text) = staged.path().to_str() else {
            return Err(CoordinationError::GitConfigUnwritten);
        };
        for setting in settings {
            let key = setting.key();
            let written = run_bounded(
                git,
                dir,
                &[
                    "config",
                    "--file",
                    staged_text,
                    "--add",
                    "--",
                    &key,
                    setting.value(),
                ],
                &[
                    ("GIT_CONFIG_NOSYSTEM", "1"),
                    ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ],
                deadline,
            );
            if !matches!(written, Some((Some(0), _))) {
                return Err(CoordinationError::GitConfigUnwritten);
            }
        }
        staged
            .persist(&path)
            .map_err(|_| CoordinationError::GitConfigUnwritten)?;
        Ok(Self { path })
    }

    /// The file's path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Whether the file lies inside `dir`, by its path as given or as the
    /// file system resolves it.
    pub(crate) fn inside(&self, dir: &std::path::Path) -> bool {
        if self.path.starts_with(dir) {
            return true;
        }
        match (self.path.canonicalize(), dir.canonicalize()) {
            (Ok(path), Ok(dir)) => path.starts_with(dir),
            // A missing checkout cannot contain it; an unresolvable file is
            // not trusted.
            (Err(_), Ok(_)) => true,
            (_, Err(_)) => false,
        }
    }
}

/// The Git configuration every Kitchen-run Git command pins over the
/// checkout's own: no hooks, a plain `ssh`, the standard pack programs, and a
/// push that carries only the ref it names (no tags, no submodules, no
/// mirroring), passed to every Kitchen-run Git process through
/// [`git_environment`].
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

/// The environment of every Git process Kitchen starts for a push, directly
/// or through a stack tool: no system configuration
/// (`GIT_CONFIG_NOSYSTEM`), `config` as the global configuration
/// (`GIT_CONFIG_GLOBAL`), no prompts, a plain `ssh`, and
/// [`pinned_git_config`] as `GIT_CONFIG_COUNT` entries, which Git ranks
/// above every configuration file.
pub(crate) fn git_environment(config: &IsolatedGitConfig, remote: &str) -> Vec<(String, String)> {
    let pins = pinned_git_config(remote);
    let mut env = Vec::with_capacity(pins.len() * 2 + 5);
    env.extend([
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        (
            "GIT_CONFIG_GLOBAL".to_owned(),
            config.path.to_string_lossy().into_owned(),
        ),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GIT_SSH_COMMAND".to_owned(), "ssh".to_owned()),
        ("GIT_CONFIG_COUNT".to_owned(), pins.len().to_string()),
    ]);
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
    let mut command = Command::new(program);
    // A caller inside a Git hook must not redirect Git to its own
    // repository, and configuration Kitchen inherited must not reach Git.
    for inherited in INHERITED_GIT_ENV {
        command.env_remove(inherited);
    }
    let mut child = command
        .args(args)
        .current_dir(dir)
        .envs(env.iter().copied())
        .env("LC_ALL", "C")
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

/// Git variables removed from every process [`run_bounded`] starts before
/// its own `env` applies.
const INHERITED_GIT_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

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

    fn redirect(&self, repository: &Repository) -> Observed<Option<GitConfigKey>> {
        self.redirecting_entry(repository)
    }

    fn update(
        &self,
        permit: &PushPermit,
        branch: &BranchName,
        commit: &CommitId,
    ) -> std::result::Result<(), UpdateFailure> {
        let update = LayerUpdate::new(branch.clone(), permit.replaces.clone(), commit.clone());
        self.push_leased(permit.repository(), std::slice::from_ref(&update))
    }

    fn update_layers(&self, permit: &LayersPermit) -> std::result::Result<(), UpdateFailure> {
        self.push_leased(permit.repository(), permit.updates())
    }
}

impl GitRemote {
    /// Push every update to `repository` with its own lease, atomically
    /// when there is more than one: a remote without atomic pushes fails the
    /// push instead of applying part of it.
    fn push_leased(
        &self,
        repository: &Repository,
        updates: &[LayerUpdate],
    ) -> std::result::Result<(), UpdateFailure> {
        // Check the checkout's configuration and resolve the destination
        // once more, as late as possible, then push to that URL, never to
        // the remote's name, which the checkout's config can repoint.
        match self.redirecting_entry(repository) {
            Observed::Known(None) => {}
            Observed::Known(Some(key)) => return Err(UpdateFailure::Redirected(key)),
            Observed::Unknown => return Err(UpdateFailure::Uncertain),
        }
        let Some(urls) = self.urls(true) else {
            return Err(UpdateFailure::Uncertain);
        };
        if !urls.iter().all(|url| self.names(url, repository)) {
            return Err(UpdateFailure::Rejected);
        }
        let Some(destination) = urls.first() else {
            return Err(UpdateFailure::Uncertain);
        };
        let mut args: Vec<String> = [
            "push",
            "--porcelain",
            "--no-follow-tags",
            "--no-recurse-submodules",
            "--receive-pack=git-receive-pack",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if updates.len() > 1 {
            args.push("--atomic".to_owned());
        }
        for update in updates {
            // An empty expected value means the ref must not exist.
            args.push(format!(
                "--force-with-lease=refs/heads/{}:{}",
                update.branch,
                update.replaces().map_or("", CommitId::as_str)
            ));
        }
        args.push(destination.clone());
        args.extend(
            updates
                .iter()
                .map(|update| format!("{}:refs/heads/{}", update.commit, update.branch)),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match self.run(&args) {
            Some((Some(0), _)) => Ok(()),
            // Git reports a refused ref, such as a failed lease, as a line
            // starting with `!` under `--porcelain`: nothing changed, since
            // an atomic push applies no ref when one is refused.
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
