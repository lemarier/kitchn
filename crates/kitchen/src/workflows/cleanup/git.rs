//! A bounded, read-only Git reader for worktree preservation evidence.
//!
//! Every call runs `git` with a deadline and an output limit, without a
//! terminal prompt, optional locks, or the file-system monitor, and with the
//! caller's `GIT_DIR`-style overrides removed so the path alone selects the
//! repository. Nothing here writes to the repository.

use std::{
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::contracts::CommitId;

/// Environment variables that would redirect Git away from the given path.
const REDIRECTING_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

/// Longest poll interval while waiting for `git` to exit.
const MAX_POLL: Duration = Duration::from_millis(20);

/// Bounds for one worktree inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLimits {
    /// The `git` executable.
    pub program: PathBuf,
    /// Deadline for each `git` call.
    pub call_timeout: Duration,
    /// Largest standard output accepted from one call.
    pub max_output_bytes: usize,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            call_timeout: Duration::from_secs(10),
            max_output_bytes: 1024 * 1024,
        }
    }
}

/// Why a worktree could not be inspected. Any of these retains the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, thiserror::Error)]
#[serde(rename_all = "kebab-case")]
pub enum GitReadError {
    /// The path is not absolute or not a directory.
    #[error("worktree path is not an absolute directory")]
    InvalidPath,
    /// `git` could not be started.
    #[error("git could not be started")]
    Spawn,
    /// A call exceeded its deadline and was killed.
    #[error("git call timed out")]
    Timeout,
    /// A call produced more output than allowed.
    #[error("git output exceeded its limit")]
    OutputTooLarge,
    /// A call exited unsuccessfully.
    #[error("git call failed")]
    Failed,
    /// Output did not have the expected shape.
    #[error("git output was malformed")]
    Malformed,
    /// The path is inside a checkout but is not its top level.
    #[error("path is not the top level of a checkout")]
    NotCheckoutRoot,
}

/// What Git reports about one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeState {
    /// The checked-out commit.
    pub head: CommitId,
    /// Whether this is a linked worktree rather than a repository's main checkout.
    pub linked: bool,
    /// Whether the worktree is locked (`git worktree lock`).
    pub locked: bool,
    /// Tracked paths with staged or unstaged changes, including conflicts.
    pub tracked_changes: u32,
    /// Untracked, non-ignored paths.
    pub untracked_files: u32,
    /// Whether `HEAD` has commits that no remote-tracking ref contains.
    pub unpushed_commits: bool,
}

/// Inspect the worktree whose top level is `path`.
///
/// # Errors
/// Returns a [`GitReadError`] when any bounded call fails; callers must treat
/// that as "retain", never as clean.
pub fn inspect_worktree(path: &Path, limits: &GitLimits) -> Result<WorktreeState, GitReadError> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(GitReadError::InvalidPath);
    }
    let layout = run(
        path,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
        limits,
    )?;
    let mut lines = layout.lines();
    let (Some(git_dir), Some(common_dir), Some(top_level), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return Err(GitReadError::Malformed);
    };
    let canonical = |value: &Path| value.canonicalize().map_err(|_| GitReadError::Malformed);
    if canonical(Path::new(top_level))? != canonical(path)? {
        return Err(GitReadError::NotCheckoutRoot);
    }
    let git_dir = canonical(Path::new(git_dir))?;
    let linked = git_dir != canonical(Path::new(common_dir))?;
    // A lock marker that cannot be read counts as a failure, not as unlocked.
    let locked = linked
        && match git_dir.join("locked").symlink_metadata() {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(GitReadError::Failed),
        };

    let head = run(
        path,
        ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        limits,
    )?;
    let head = CommitId::new(head.trim_end()).map_err(|_| GitReadError::Malformed)?;

    let status = run(
        path,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        limits,
    )?;
    let (tracked_changes, untracked_files) = count_status(&status)?;

    let unpushed = run(
        path,
        ["rev-list", "--max-count=1", "HEAD", "--not", "--remotes"],
        limits,
    )?;
    Ok(WorktreeState {
        head,
        linked,
        locked,
        tracked_changes,
        untracked_files,
        unpushed_commits: !unpushed.trim().is_empty(),
    })
}

/// Count tracked and untracked entries in `git status --porcelain=v1 -z`
/// output. Renames and copies carry their source path as an extra field.
fn count_status(output: &str) -> Result<(u32, u32), GitReadError> {
    let mut tracked: u32 = 0;
    let mut untracked: u32 = 0;
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    while let Some(entry) = fields.next() {
        let code = entry.get(..2).ok_or(GitReadError::Malformed)?;
        if entry.get(2..3) != Some(" ") {
            return Err(GitReadError::Malformed);
        }
        if code == "??" {
            untracked = untracked.saturating_add(1);
            continue;
        }
        tracked = tracked.saturating_add(1);
        if code.contains(['R', 'C']) && fields.next().is_none() {
            return Err(GitReadError::Malformed);
        }
    }
    Ok((tracked, untracked))
}

/// Whether `name`, a top-level entry of the checkout at `dir`, is ignored
/// by Git and contains no tracked files.
///
/// # Errors
/// Returns a [`GitReadError`] when a call fails; callers must treat that as
/// "not build output".
pub(super) fn ignored_untracked(
    dir: &Path,
    name: &str,
    limits: &GitLimits,
) -> Result<bool, GitReadError> {
    let entry = format!("{name}/");
    let (ignored, _) = run_raw(dir, ["check-ignore", "--quiet", "--", &entry], limits)?;
    match ignored.code() {
        Some(0) => {}
        Some(1) => return Ok(false),
        _ => return Err(GitReadError::Failed),
    }
    let tracked = run(dir, ["ls-files", "-z", "--", &entry], limits)?;
    Ok(tracked.is_empty())
}

/// Run one read-only `git` call in `dir` that must succeed.
fn run<const N: usize>(
    dir: &Path,
    args: [&str; N],
    limits: &GitLimits,
) -> Result<String, GitReadError> {
    let (status, output) = run_raw(dir, args, limits)?;
    if !status.success() {
        return Err(GitReadError::Failed);
    }
    Ok(output)
}

/// Run one read-only `git` call in `dir` within the limits and return its
/// exit status with its output.
fn run_raw<const N: usize>(
    dir: &Path,
    args: [&str; N],
    limits: &GitLimits,
) -> Result<(ExitStatus, String), GitReadError> {
    let mut command = Command::new(&limits.program);
    command
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("-C")
        .arg(dir)
        .args(args.iter().map(OsStr::new))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in REDIRECTING_ENV {
        command.env_remove(name);
    }
    let mut child = command.spawn().map_err(|_| GitReadError::Spawn)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(GitReadError::Spawn);
    };
    let limit = u64::try_from(limits.max_output_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    // The reader reports through a channel so that waiting for it is bounded
    // too: a grandchild process can keep the pipe open after `git` exits.
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let read = stdout.take(limit).read_to_end(&mut buffer).map(|_| buffer);
        let _ = sender.send(read);
    });
    let started = Instant::now();
    let mut poll = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() >= limits.call_timeout => {
                break Err(GitReadError::Timeout);
            }
            Ok(None) => {
                thread::sleep(poll);
                poll = poll.saturating_mul(2).min(MAX_POLL);
            }
            Err(_) => break Err(GitReadError::Failed),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = status?;
    let remaining = limits.call_timeout.saturating_sub(started.elapsed());
    let output = match receiver.recv_timeout(remaining) {
        Ok(Ok(output)) => output,
        Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => return Err(GitReadError::Failed),
        // The detached reader ends when the last writer closes the pipe.
        Err(RecvTimeoutError::Timeout) => return Err(GitReadError::Timeout),
    };
    if output.len() > limits.max_output_bytes {
        return Err(GitReadError::OutputTooLarge);
    }
    let output = String::from_utf8(output).map_err(|_| GitReadError::Malformed)?;
    Ok((status, output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_counts_tracked_untracked_and_renames() {
        assert_eq!(count_status(""), Ok((0, 0)));
        assert_eq!(
            count_status(" M a.rs\0?? new.txt\0R  b.rs\0old b.rs\0UU c.rs\0"),
            Ok((3, 1))
        );
    }

    #[test]
    fn malformed_status_is_an_error() {
        assert_eq!(count_status("M"), Err(GitReadError::Malformed));
        assert_eq!(count_status("MMx.rs\0"), Err(GitReadError::Malformed));
        // A rename without its source path.
        assert_eq!(count_status("R  b.rs\0"), Err(GitReadError::Malformed));
    }

    #[test]
    fn relative_and_missing_paths_are_refused_without_running_git() {
        let limits = GitLimits {
            program: PathBuf::from("/nonexistent/git"),
            ..GitLimits::default()
        };
        assert_eq!(
            inspect_worktree(Path::new("relative"), &limits),
            Err(GitReadError::InvalidPath)
        );
        assert_eq!(
            inspect_worktree(Path::new("/nonexistent/worktree"), &limits),
            Err(GitReadError::InvalidPath)
        );
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure() {
        let limits = GitLimits {
            program: PathBuf::from("/nonexistent/git"),
            ..GitLimits::default()
        };
        assert_eq!(
            inspect_worktree(&std::env::temp_dir(), &limits),
            Err(GitReadError::Spawn)
        );
    }
}
