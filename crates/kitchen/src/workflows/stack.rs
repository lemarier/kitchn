//! Dependent branches and stacked pull requests.
//!
//! When a house configures a stack tool ([`StackTool`]), a dependent branch
//! is created, rebased, retargeted, and pushed only through that tool. The
//! push boundary ([`crate::workflows::push`]) refuses a branch launched as a
//! layer or whose pull request is based on another branch than the default;
//! other plain paths (a plain rebase, a pull-request base edit) call
//! [`check_plain`] and are refused. The tool runs through [`StackBoundary`], which authorizes the task, binds
//! the command to the branch its durable record names, applies the push
//! boundary's pull-request and remote-head checks before anything reaches
//! the remote, and never rewrites layers another writer is working on, as
//! derived from the tool's own view of the stack and the house's tasks.
//! [`GhStack`] runs `gh stack` with non-interactive flags and an explicit
//! remote.
//!
//! [`plan_retarget`] turns a merged base pull request into the steps its
//! dependents need, addressed to each branch's own writer.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::{
    BackendId, TaskId,
    contracts::{
        BranchName, Clock, CommitId, Fence, HouseGrants, IssueNumber, Permission, Repository,
    },
    house::{StackTool, StackToolStatus},
    state::{HouseStore, TaskRecord, TaskState},
    workflows::{
        coordination::{CoordinationError, held_branches, task_branch},
        pickup::is_shell_safe,
        push::{
            Decision, GitRemote, PullRequests, PushIntent, PushRefusal, RefUpdater, RemoteBranches,
            bind, decide, git_config_env, observe, record_landed, run_bounded,
        },
        repair::Observed,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Most layers one command may name.
pub const MAX_STACK_LAYERS: usize = 8;

/// Where a task's branch sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchLayer {
    /// Based on the default branch.
    Independent,
    /// A layer on another branch: its pull request depends on the parent's.
    Dependent {
        /// The branch it is based on.
        parent: BranchName,
    },
}

/// A branch operation on a plain path, outside the stack tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BranchOperation {
    /// Create the branch on the remote.
    Create,
    /// Rebase the branch, such as `git rebase --onto`.
    Rebase,
    /// Change its pull request's base, such as `gh pr edit --base`.
    Retarget,
    /// Push new commits.
    Push,
}

/// Why a branch operation must not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StackRefusal {
    /// The house configures a stack tool and the branch is a dependent
    /// layer: only the tool may perform the operation.
    #[error("a dependent branch must go through the configured stack tool")]
    StackToolRequired {
        /// The configured tool.
        tool: StackTool,
        /// The refused plain operation.
        operation: BranchOperation,
    },
    /// The command names a branch other than the task's own.
    #[error("the command names a branch the task does not own")]
    ForeignBranch,
    /// The command would rewrite or push layers above the task's branch
    /// while another writer works on them, or that could not be established.
    #[error("another writer may be working on a layer above")]
    UpstackBusy,
    /// The command names no layer, too many, or one twice.
    #[error("the stack layers are empty, too many, repeated, or not linear")]
    InvalidLayers,
    /// A branch name has characters that are unsafe in an instruction a
    /// writer runs in a shell.
    #[error("a branch name is unsafe in a shell instruction")]
    UnsafeBranchName,
    /// The push boundary's checks refused the task's branch, its pull
    /// request, or its remote head.
    #[error("the push boundary refused the stack command")]
    Push(PushRefusal),
}

/// Check a plain branch operation. With a configured stack tool, a
/// dependent layer is refused on every plain path.
///
/// # Errors
/// Returns [`StackRefusal::StackToolRequired`].
pub fn check_plain(
    tool: Option<StackTool>,
    layer: &BranchLayer,
    operation: BranchOperation,
) -> std::result::Result<(), StackRefusal> {
    match (tool, layer) {
        (Some(tool), BranchLayer::Dependent { .. }) => {
            Err(StackRefusal::StackToolRequired { tool, operation })
        }
        (None, _) | (Some(_), BranchLayer::Independent) => Ok(()),
    }
}

/// A stack-tool command. Every variant has a non-interactive form; commands
/// that only exist interactively (a bare `view`, `modify`, `switch`) cannot
/// be expressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackCommand {
    /// Adopt an existing chain of branches, bottom to top, onto `trunk`,
    /// rather than rebuilding it by hand.
    Adopt {
        /// The branch the bottom layer is based on.
        trunk: BranchName,
        /// The layers, bottom to top.
        branches: Vec<BranchName>,
    },
    /// Add the task's branch as a new layer on top of the current one.
    Add {
        /// The new layer.
        branch: BranchName,
    },
    /// Rebase the task's layer and every layer above it.
    RebaseUpstack,
    /// Push every layer of the stack.
    Push,
    /// Push and create or update the stack's pull requests with generated
    /// titles, as drafts unless `ready`.
    Submit {
        /// Mark the pull requests ready for review.
        ready: bool,
    },
    /// Read the stack.
    View,
}

impl StackCommand {
    /// Whether the command rewrites or pushes layers above the task's.
    #[must_use]
    pub const fn touches_upstack(&self) -> bool {
        match self {
            Self::RebaseUpstack | Self::Push | Self::Submit { .. } => true,
            Self::Adopt { .. } | Self::Add { .. } | Self::View => false,
        }
    }

    /// The permissions the command exercises: every command acts on the
    /// task's branch, and a submission also opens pull requests and, when
    /// `ready`, asks for their review.
    #[must_use]
    pub const fn permissions(&self) -> &'static [Permission] {
        match self {
            Self::Submit { ready: false } => &[Permission::PushBranch, Permission::OpenPullRequest],
            Self::Submit { ready: true } => &[
                Permission::PushBranch,
                Permission::OpenPullRequest,
                Permission::RequestReview,
            ],
            Self::Adopt { .. }
            | Self::Add { .. }
            | Self::RebaseUpstack
            | Self::Push
            | Self::View => &[Permission::PushBranch],
        }
    }
}

/// One layer as the stack tool reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackLayerView {
    /// The branch.
    pub name: BranchName,
    /// Its pull request merged.
    #[serde(default)]
    pub is_merged: bool,
    /// Its parent's tip is no longer in its history.
    #[serde(default)]
    pub needs_rebase: bool,
    /// Its pull request, if one exists.
    #[serde(default)]
    pub pr: Option<StackPullRequest>,
}

/// A layer's pull request as the stack tool reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct StackPullRequest {
    /// Its number.
    pub number: IssueNumber,
}

/// A stack as the tool reports it, bottom to top.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackView {
    /// The trunk branch.
    pub trunk: BranchName,
    /// The layers, bottom to top.
    pub branches: Vec<StackLayerView>,
}

/// What a stack-tool command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackResult {
    /// The command succeeded.
    Done,
    /// The stack as the tool reports it.
    Viewed(StackView),
    /// A rebase stopped on a conflict; the writer resolves it and continues.
    Conflict,
    /// Another rebase is in progress.
    RebaseInProgress,
    /// Another stack-tool process holds the stack; retry later.
    Locked,
    /// The branch is not part of a stack; adopt it first.
    NotInStack,
    /// The tool refused the command; nothing is known to have changed.
    Rejected,
    /// The command may or may not have changed branches or pull requests.
    Uncertain,
}

/// Runs stack-tool commands in one checkout.
pub trait StackRunner {
    /// Run `command`.
    fn run(&self, command: &StackCommand) -> StackResult;
}

/// The `gh stack` extension of the GitHub CLI, run in one checkout against
/// one explicit remote.
#[derive(Debug, Clone)]
pub struct GhStack {
    gh: PathBuf,
    checkout: PathBuf,
    remote: String,
    deadline: Duration,
}

/// Longest remote name accepted, in bytes.
const MAX_REMOTE_BYTES: usize = 64;

/// Longest version text accepted from the tool, in bytes.
const MAX_VERSION_BYTES: usize = 64;

impl GhStack {
    /// Bind the `gh` executable, the checkout, the remote name, and the
    /// deadline for each call.
    ///
    /// # Errors
    /// Returns [`CoordinationError::InvalidGitRemote`] for a relative path, a
    /// zero deadline, or a remote name that is not 1–64 ASCII letters,
    /// digits, or `._-` without a leading `-`.
    pub fn new(
        gh: PathBuf,
        checkout: PathBuf,
        remote: &str,
        deadline: Duration,
    ) -> std::result::Result<Self, CoordinationError> {
        let plain = !remote.is_empty()
            && remote.len() <= MAX_REMOTE_BYTES
            && !remote.starts_with('-')
            && remote
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !gh.is_absolute() || !checkout.is_absolute() || deadline.is_zero() || !plain {
            return Err(CoordinationError::InvalidGitRemote);
        }
        Ok(Self {
            gh,
            checkout,
            remote: remote.to_owned(),
            deadline,
        })
    }

    /// The exact arguments for `command`: always non-interactive, and with
    /// `--remote` on every command that fetches or pushes.
    #[must_use]
    pub fn args(&self, command: &StackCommand) -> Vec<String> {
        let mut args = vec!["stack".to_owned()];
        match command {
            StackCommand::Adopt { trunk, branches } => {
                args.extend(["init".to_owned(), "--base".to_owned(), trunk.to_string()]);
                args.extend(branches.iter().map(ToString::to_string));
            }
            StackCommand::Add { branch } => {
                args.extend(["add".to_owned(), branch.to_string()]);
            }
            StackCommand::RebaseUpstack => {
                args.extend(["rebase".to_owned(), "--upstack".to_owned()]);
                args.extend(["--remote".to_owned(), self.remote.clone()]);
            }
            StackCommand::Push => {
                args.extend([
                    "push".to_owned(),
                    "--remote".to_owned(),
                    self.remote.clone(),
                ]);
            }
            StackCommand::Submit { ready } => {
                args.extend(["submit".to_owned(), "--auto".to_owned()]);
                if *ready {
                    args.push("--open".to_owned());
                }
                args.extend(["--remote".to_owned(), self.remote.clone()]);
            }
            StackCommand::View => {
                args.extend(["view".to_owned(), "--json".to_owned()]);
            }
        }
        args
    }

    /// A [`GitRemote`] for the same checkout and remote name this tool
    /// pushes with, so the boundary's URL checks read the remote the tool
    /// uses.
    ///
    /// # Errors
    /// Returns [`CoordinationError::InvalidGitRemote`] for a relative `git`
    /// path or a zero deadline.
    pub fn git_remote(
        &self,
        git: PathBuf,
        deadline: Duration,
    ) -> std::result::Result<GitRemote, CoordinationError> {
        GitRemote::new(git, self.checkout.clone(), &self.remote, deadline)
    }

    /// Probe whether `gh stack` is installed, for doctor. `None` means the
    /// probe did not finish, which is not evidence either way.
    #[must_use]
    pub fn detect(gh: &Path, dir: &Path, deadline: Duration) -> Option<StackToolStatus> {
        if !gh.is_file() {
            return Some(StackToolStatus::Missing);
        }
        let (code, stdout) = run_bounded(gh, dir, &["stack", "--version"], &NO_PROMPTS, deadline)?;
        if code != Some(0) {
            // `gh` reports an unknown command when the extension is absent.
            return Some(StackToolStatus::Missing);
        }
        let text = String::from_utf8(stdout).ok()?;
        let version = text.trim().rsplit(' ').next()?;
        (!version.is_empty()
            && version.len() <= MAX_VERSION_BYTES
            && version.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| StackToolStatus::Installed {
            version: version.to_owned(),
        })
    }
}

/// Environment that keeps `gh` and Git from prompting or opening an editor.
const NO_PROMPTS: [(&str, &str); 6] = [
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("NO_COLOR", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
];

impl StackRunner for GhStack {
    fn run(&self, command: &StackCommand) -> StackResult {
        let args = self.args(command);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // `gh stack` runs Git in the worker's checkout: pin what its
        // configuration may change about a push or rebase.
        let mut env: Vec<(String, String)> = NO_PROMPTS
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        env.push(("GIT_SSH_COMMAND".to_owned(), "ssh".to_owned()));
        env.extend(git_config_env(&self.remote));
        let env: Vec<(&str, &str)> = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let Some((code, stdout)) =
            run_bounded(&self.gh, &self.checkout, &args, &env, self.deadline)
        else {
            return StackResult::Uncertain;
        };
        // Exit codes documented by gh-stack.
        match code {
            Some(0) => match command {
                StackCommand::View => serde_json::from_slice(&stdout)
                    .map_or(StackResult::Uncertain, StackResult::Viewed),
                StackCommand::Adopt { .. }
                | StackCommand::Add { .. }
                | StackCommand::RebaseUpstack
                | StackCommand::Push
                | StackCommand::Submit { .. } => StackResult::Done,
            },
            Some(2) => StackResult::NotInStack,
            Some(3) => StackResult::Conflict,
            Some(7) => StackResult::RebaseInProgress,
            Some(8) => StackResult::Locked,
            Some(5 | 6 | 9) => StackResult::Rejected,
            // A generic or API failure may follow a partial push.
            _ if command.touches_upstack() => StackResult::Uncertain,
            _ => StackResult::Rejected,
        }
    }
}

/// Whether writers are working on the layers above the task's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstack {
    /// The task's branch is the top layer.
    Top,
    /// No writer works on any layer above it.
    Idle,
    /// A writer or a person works on a layer above it.
    Busy,
    /// Could not be established.
    Unknown,
}

/// Derive who works above `branch` from the stack tool's `view` and the
/// house's tasks: a layer whose task is still open, or whose worker a person
/// holds, is busy; a layer no task of this house owns is unknown, since a
/// person may be working on it.
///
/// # Errors
/// Returns store read failures.
pub fn upstack(store: &HouseStore, view: &StackView, branch: &BranchName) -> Result<Upstack> {
    let Some(position) = view.branches.iter().position(|layer| &layer.name == branch) else {
        return Ok(Upstack::Unknown);
    };
    let above: Vec<&StackLayerView> = view
        .branches
        .iter()
        .skip(position.saturating_add(1))
        .filter(|layer| !layer.is_merged)
        .collect();
    if above.is_empty() {
        return Ok(Upstack::Top);
    }
    let tasks = store.tasks()?;
    let mut state = Upstack::Idle;
    for layer in above {
        // A settled pickup task and a later repair task can both name the
        // layer's branch: every one must be settled for the layer to be idle.
        let owners: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|record| task_branch(record).as_ref() == Some(&layer.name))
            .collect();
        let layer_state = if owners.is_empty() {
            Upstack::Unknown
        } else if owners.iter().any(|record| {
            held_branches(record).contains(&layer.name)
                || match record.state() {
                    TaskState::Settled { .. } => false,
                    TaskState::Open | TaskState::Claimed { .. } => true,
                }
        }) {
            Upstack::Busy
        } else {
            Upstack::Idle
        };
        state = match (state, layer_state) {
            (Upstack::Busy, _) | (_, Upstack::Busy) => Upstack::Busy,
            (Upstack::Unknown, _) | (_, Upstack::Unknown) => Upstack::Unknown,
            (Upstack::Idle | Upstack::Top, Upstack::Idle | Upstack::Top) => Upstack::Idle,
        };
    }
    Ok(state)
}

/// What a stack-tool command acts through, bound to one task's branch.
#[derive(Clone, Copy)]
pub struct StackBoundary<'a> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The house's current grants.
    pub grants: &'a HouseGrants,
    /// The backend namespace the task's push grant names.
    pub destination: &'a BackendId,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// The stack tool.
    pub runner: &'a dyn StackRunner,
    /// Pull request reads.
    pub pull_requests: &'a dyn PullRequests,
    /// Remote head reads, through the remote the tool pushes to.
    pub remote: &'a dyn RemoteBranches,
    /// The push side of that same remote: only its
    /// [`RefUpdater::pushes_to`] is used, to check the effective push URL.
    pub updater: &'a dyn RefUpdater,
}

/// What a stack-tool command through the boundary did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackOutcome {
    /// The boundary refused; nothing ran.
    Refused(StackRefusal),
    /// The tool ran.
    Ran(StackResult),
}

impl StackBoundary<'_> {
    /// Run `command` for `task`. Nothing runs until the task's live claim at
    /// `fence` and every permission the command exercises
    /// ([`StackCommand::permissions`]) for its repository are checked
    /// against the house's current grants. The task's branch comes from its
    /// durable record, and a branch a person holds is refused. The command
    /// must name only that branch. Before a command that fetches, rewrites,
    /// or pushes, the remote must be the granted repository, and the pull
    /// request and remote head are checked exactly as the push boundary
    /// checks them against `intent`: a merged or closed pull request, a
    /// deleted or moved branch, or an unreadable state refuses. Such a
    /// command also needs every layer above free of other writers, derived
    /// through [`upstack`] from the tool's view of the stack.
    ///
    /// # Errors
    /// Returns [`crate::state::StateError::StaleFence`] without a live claim
    /// at `fence`, [`crate::state::StateError::CancelRequested`] while
    /// cancellation is pending, contract errors when the task lacks a grant
    /// or the house revoked it, and store read failures.
    pub fn run(
        &self,
        task: &TaskId,
        fence: Fence,
        command: &StackCommand,
        intent: &PushIntent,
    ) -> Result<StackOutcome> {
        let (_, binding) = bind(
            self.store,
            self.grants,
            self.destination,
            self.clock,
            task,
            fence,
            command.permissions(),
        )?;
        let binding = match binding {
            Ok(binding) => binding,
            Err(refusal) => return Ok(StackOutcome::Refused(StackRefusal::Push(refusal))),
        };
        if let Err(refusal) = check_layers(command, &binding.branch) {
            return Ok(StackOutcome::Refused(refusal));
        }
        if !command.touches_upstack() {
            return Ok(StackOutcome::Ran(self.runner.run(command)));
        }
        if let Some(refusal) = self.remote_refusal(&binding.repository) {
            return Ok(refused(refusal));
        }
        let observed = match observe(&binding, intent, self.pull_requests, self.remote, false) {
            Ok((observed, _)) => observed,
            Err(refusal) => return Ok(refused(refusal)),
        };
        match decide(&binding, intent, &observed, None) {
            Ok(Decision::Update(_) | Decision::Current) => {}
            Err(refusal) => return Ok(refused(refusal)),
        }
        let above = match self.runner.run(&StackCommand::View) {
            StackResult::Viewed(view) => upstack(self.store, &view, &binding.branch)?,
            StackResult::Done
            | StackResult::Conflict
            | StackResult::RebaseInProgress
            | StackResult::Locked
            | StackResult::NotInStack
            | StackResult::Rejected
            | StackResult::Uncertain => Upstack::Unknown,
        };
        match above {
            Upstack::Top | Upstack::Idle => {}
            Upstack::Busy | Upstack::Unknown => {
                return Ok(StackOutcome::Refused(StackRefusal::UpstackBusy));
            }
        }
        // The tool takes the remote by name, so it resolves the URLs itself
        // and this check cannot bind them: read them again as late as
        // possible. What remains is the interval between this read and the
        // tool's own (#92).
        if let Some(refusal) = self.remote_refusal(&binding.repository) {
            return Ok(refused(refusal));
        }
        let result = self.runner.run(command);
        if result == StackResult::Done
            && matches!(command, StackCommand::Push | StackCommand::Submit { .. })
        {
            record_landed(self.store, self.clock, task, fence, &binding, intent)?;
        }
        Ok(StackOutcome::Ran(result))
    }
}

impl StackBoundary<'_> {
    /// Why the remote is not the granted repository, if it is not. The tool
    /// fetches from and pushes to the remote's own URLs, and a worker can set
    /// the push URLs apart from the fetch URLs, so every one of each must
    /// name it.
    fn remote_refusal(&self, repository: &Repository) -> Option<PushRefusal> {
        match (
            self.remote.reads_from(repository),
            self.updater.pushes_to(repository),
        ) {
            (Observed::Known(true), Observed::Known(true)) => None,
            (Observed::Known(false), _) | (_, Observed::Known(false)) => {
                Some(PushRefusal::RemoteMismatch)
            }
            (Observed::Unknown, _) | (_, Observed::Unknown) => Some(PushRefusal::Unknown),
        }
    }
}

const fn refused(refusal: PushRefusal) -> StackOutcome {
    StackOutcome::Refused(StackRefusal::Push(refusal))
}

/// The command may name only the task's own branch.
fn check_layers(
    command: &StackCommand,
    branch: &BranchName,
) -> std::result::Result<(), StackRefusal> {
    match command {
        StackCommand::Adopt { trunk, branches } => {
            let mut seen: Vec<&BranchName> = Vec::with_capacity(branches.len());
            for layer in branches {
                if layer == trunk || seen.contains(&layer) {
                    return Err(StackRefusal::InvalidLayers);
                }
                seen.push(layer);
            }
            if branches.is_empty() || branches.len() > MAX_STACK_LAYERS {
                return Err(StackRefusal::InvalidLayers);
            }
            // Adopting records the chain; it rewrites nothing, but the task
            // must own one of its layers.
            if !branches.contains(branch) {
                return Err(StackRefusal::ForeignBranch);
            }
            Ok(())
        }
        StackCommand::Add { branch: added } if added != branch => Err(StackRefusal::ForeignBranch),
        StackCommand::Add { .. }
        | StackCommand::RebaseUpstack
        | StackCommand::Push
        | StackCommand::Submit { .. }
        | StackCommand::View => Ok(()),
    }
}

/// A base pull request that merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedBase {
    /// The merged pull request.
    pub pull_request: IssueNumber,
    /// Its branch, which dependents are still based on.
    pub branch: BranchName,
    /// The branch it merged into.
    pub into: BranchName,
    /// Its branch head before the merge. After a squash merge this commit is
    /// not in the target's history, so dependents replay only their own
    /// commits on top of it.
    pub head: CommitId,
}

/// An open dependent pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    /// Its number.
    pub pull_request: IssueNumber,
    /// Its branch.
    pub branch: BranchName,
    /// The branch it is based on.
    pub base: BranchName,
    /// Its current head.
    pub head: CommitId,
}

/// One step after a base merged, bottom-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetargetStep {
    /// Change the pull request's base, before the merged branch is deleted.
    Retarget {
        /// The dependent pull request.
        pull_request: IssueNumber,
        /// The merged branch it was based on.
        from: BranchName,
        /// Its new base.
        to: BranchName,
    },
    /// An instruction for the branch's own writer: rebase only this branch
    /// with `git rebase --onto <onto> <upstream> <branch>`.
    RebaseOnto {
        /// The branch; only its writer runs this.
        branch: BranchName,
        /// The new base.
        onto: BranchName,
        /// The old base head: commits up to it are not the branch's own.
        upstream: CommitId,
    },
    /// An instruction for the writer of the lowest dependent layer: run
    /// these stack-tool commands through [`StackBoundary`], which refuses
    /// while another writer works above it.
    StackTool {
        /// The lowest dependent layer.
        branch: BranchName,
        /// The commands, in order.
        commands: Vec<StackCommand>,
    },
}

impl RetargetStep {
    /// The command for the branch's writer, for a
    /// [`RetargetStep::RebaseOnto`] step, as separate arguments to run
    /// without a shell.
    #[must_use]
    pub fn args(&self) -> Option<Vec<String>> {
        match self {
            Self::RebaseOnto {
                branch,
                onto,
                upstream,
            } => Some(vec![
                "git".to_owned(),
                "rebase".to_owned(),
                "--onto".to_owned(),
                onto.to_string(),
                upstream.to_string(),
                branch.to_string(),
            ]),
            Self::Retarget { .. } | Self::StackTool { .. } => None,
        }
    }

    /// The instruction text for the branch's writer, for a
    /// [`RetargetStep::RebaseOnto`] step. `None` unless every name is safe
    /// to read as one shell word ([`is_shell_safe`]); [`plan_retarget`]
    /// refuses other names before any step exists.
    #[must_use]
    pub fn instruction(&self) -> Option<String> {
        match self {
            Self::RebaseOnto {
                branch,
                onto,
                upstream,
            } if is_shell_safe(branch) && is_shell_safe(onto) => {
                Some(format!("git rebase --onto {onto} {upstream} {branch}"))
            }
            Self::RebaseOnto { .. } | Self::Retarget { .. } | Self::StackTool { .. } => None,
        }
    }
}

/// The steps after a base merged, and the branch that may be deleted once
/// they are done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetargetPlan {
    /// The steps, bottom-up; each waits for the one before it.
    pub steps: Vec<RetargetStep>,
    /// The merged branch. Delete it only after every retarget applied;
    /// deleting it first closes the dependents' pull requests.
    pub delete_after: BranchName,
}

/// Plan the dependents of a merged base, bottom-up. Without a stack tool,
/// each dependent pull request is retargeted before the merged branch is
/// deleted and each branch's writer gets its own `rebase --onto`
/// instruction; Kitchen rewrites no branch. With a stack tool, the lowest
/// dependent's writer runs the tool, which retargets and rebases the layers
/// in one pass through [`StackBoundary`].
///
/// Dependents that are not on the merged branch's chain are ignored.
///
/// # Errors
/// Returns [`StackRefusal::InvalidLayers`] when the chain branches (two
/// dependents on one base) or is longer than [`MAX_STACK_LAYERS`], and
/// [`StackRefusal::UnsafeBranchName`] when a branch in the plan, read from
/// pull-request data, is not [`is_shell_safe`].
pub fn plan_retarget(
    tool: Option<StackTool>,
    merged: &MergedBase,
    dependents: &[Dependent],
) -> std::result::Result<RetargetPlan, StackRefusal> {
    let mut chain: Vec<&Dependent> = Vec::new();
    let mut base = &merged.branch;
    loop {
        let mut on_base = dependents
            .iter()
            .filter(|dependent| &dependent.base == base);
        let Some(next) = on_base.next() else {
            break;
        };
        if on_base.next().is_some() || chain.len() >= MAX_STACK_LAYERS {
            return Err(StackRefusal::InvalidLayers);
        }
        chain.push(next);
        base = &next.branch;
    }
    let names = [&merged.branch, &merged.into].into_iter().chain(
        chain
            .iter()
            .flat_map(|dependent| [&dependent.branch, &dependent.base]),
    );
    for name in names {
        if !is_shell_safe(name) {
            return Err(StackRefusal::UnsafeBranchName);
        }
    }
    let steps = match (tool, chain.first()) {
        (_, None) => Vec::new(),
        (Some(_), Some(lowest)) => vec![RetargetStep::StackTool {
            branch: lowest.branch.clone(),
            commands: vec![
                StackCommand::RebaseUpstack,
                StackCommand::Submit { ready: false },
            ],
        }],
        (None, Some(_)) => {
            let mut steps = Vec::with_capacity(chain.len().saturating_add(1));
            let mut upstream = &merged.head;
            let mut onto = &merged.into;
            for (index, dependent) in chain.iter().enumerate() {
                if index == 0 {
                    steps.push(RetargetStep::Retarget {
                        pull_request: dependent.pull_request,
                        from: merged.branch.clone(),
                        to: merged.into.clone(),
                    });
                }
                steps.push(RetargetStep::RebaseOnto {
                    branch: dependent.branch.clone(),
                    onto: onto.clone(),
                    upstream: upstream.clone(),
                });
                upstream = &dependent.head;
                onto = &dependent.branch;
            }
            steps
        }
    };
    Ok(RetargetPlan {
        steps,
        delete_after: merged.branch.clone(),
    })
}
