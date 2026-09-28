//! A bounded, read-only Git reader for worktree preservation evidence.
//!
//! Every call runs `git` with a deadline and an output limit, without a
//! terminal prompt, optional locks, replace refs, or the file-system monitor,
//! and with the caller's `GIT_DIR`-style, configuration, and pathspec
//! overrides removed so the path alone selects the repository. Nothing here
//! writes to the repository.

use std::{
    ffi::OsStr,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{ChildStdout, Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::contracts::CommitId;

/// Environment variables that would redirect Git away from the given path or
/// change what its answers mean.
const REDIRECTING_ENV: [&str; 15] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_REPLACE_REF_BASE",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// Longest poll interval while waiting for `git` to exit.
const MAX_POLL: Duration = Duration::from_millis(20);
/// Most ignored paths one inspection lists; more makes the worktree
/// unreadable, which retains it.
pub const MAX_IGNORED_PATHS: usize = 256;
/// Longest single `git ls-files` record accepted while scanning the index.
const MAX_RECORD_BYTES: u64 = 8192;

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
    /// Paths Git ignores, from every ignore source, as `git ls-files` lists
    /// them: a wholly ignored directory appears once with a trailing `/`.
    pub ignored: Vec<String>,
    /// Tracked files marked assume-unchanged or skip-worktree, whose edits
    /// `git status` does not report.
    pub hidden_tracked: u32,
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
    let ignored = list_ignored(path, limits)?;
    let hidden_tracked = count_hidden_tracked(path, limits)?;

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
        ignored,
        hidden_tracked,
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

/// The paths Git ignores in the checkout at `path`, from `.gitignore` files,
/// `.git/info/exclude`, and the user's global excludes alike. A wholly ignored
/// directory is listed once, so build output does not flood the list.
fn list_ignored(path: &Path, limits: &GitLimits) -> Result<Vec<String>, GitReadError> {
    let listing = run(
        path,
        [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        limits,
    )?;
    let mut ignored: Vec<String> = listing
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    ignored.sort();
    ignored.dedup();
    if ignored.len() > MAX_IGNORED_PATHS {
        return Err(GitReadError::OutputTooLarge);
    }
    Ok(ignored)
}

/// How many tracked files carry the assume-unchanged or skip-worktree flag.
/// `git status` does not report edits to them. The index listing can be far
/// larger than the output limit, so it is filtered while it streams and only
/// the deadline bounds the scan.
fn count_hidden_tracked(path: &Path, limits: &GitLimits) -> Result<u32, GitReadError> {
    let (status, hidden) = run_with(path, ["ls-files", "-v", "-z"], limits, |stdout| {
        count_hidden(stdout)
    })?;
    if !status.success() {
        return Err(GitReadError::Failed);
    }
    Ok(hidden)
}

/// Count `git ls-files -v -z` records whose tag is not `H` (an ordinary
/// cached file): `S` is skip-worktree, lowercase is assume-unchanged, and any
/// unknown tag is treated as hidden rather than as clean.
fn count_hidden(stdout: impl Read) -> io::Result<u32> {
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    let mut hidden: u32 = 0;
    loop {
        record.clear();
        let read = (&mut reader)
            .take(MAX_RECORD_BYTES)
            .read_until(0, &mut record)?;
        if read == 0 {
            return Ok(hidden);
        }
        // A record is `<tag> <path>` ending in NUL; anything else is malformed.
        if record.last() != Some(&0) || record.get(1) != Some(&b' ') {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if record.first() != Some(&b'H') {
            hidden = hidden.saturating_add(1);
        }
    }
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
    // `check-ignore` reads a leading `:` as pathspec magic, so a directory
    // named `:(top)target` would be checked as `target`. `./` prevents that.
    let relative = format!("./{name}/");
    let (ignored, _) = run_raw(dir, ["check-ignore", "--quiet", "--", &relative], limits)?;
    match ignored.code() {
        Some(0) => {}
        Some(1) => return Ok(false),
        _ => return Err(GitReadError::Failed),
    }
    // Literal, so the name is neither magic nor a glob.
    let entry = format!("{name}/");
    let tracked = run(
        dir,
        ["--literal-pathspecs", "ls-files", "-z", "--", &entry],
        limits,
    )?;
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
    let max = limits.max_output_bytes;
    let (status, output) = run_with(dir, args, limits, move |stdout| {
        let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
        let mut buffer = Vec::new();
        stdout.take(limit).read_to_end(&mut buffer)?;
        Ok(buffer)
    })?;
    if output.len() > max {
        return Err(GitReadError::OutputTooLarge);
    }
    let output = String::from_utf8(output).map_err(|_| GitReadError::Malformed)?;
    Ok((status, output))
}

/// Run one read-only `git` call in `dir` under its deadline, handing its
/// standard output to `read` on a separate thread.
fn run_with<const N: usize, T: Send + 'static>(
    dir: &Path,
    args: [&str; N],
    limits: &GitLimits,
    read: impl FnOnce(ChildStdout) -> io::Result<T> + Send + 'static,
) -> Result<(ExitStatus, T), GitReadError> {
    let mut command = Command::new(&limits.program);
    command
        .arg("--no-optional-locks")
        .arg("--no-replace-objects")
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
    // The reader reports through a channel so that waiting for it is bounded
    // too: a grandchild process can keep the pipe open after `git` exits.
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(read(stdout));
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
    fn only_ordinary_cached_files_are_not_hidden() {
        let index = |records: &[u8]| count_hidden(records);
        assert_eq!(index(b"").ok(), Some(0));
        assert_eq!(index(b"H a.rs\0H dir/b.rs\0").ok(), Some(0));
        // Assume-unchanged (lowercase), skip-worktree, and any unknown tag.
        assert_eq!(
            index(b"h a.rs\0S b.rs\0s c.rs\0H d.rs\0X e.rs\0").ok(),
            Some(4)
        );
    }

    #[test]
    fn malformed_index_records_are_errors() {
        let kind = |records: &[u8]| count_hidden(records).map_err(|error| error.kind());
        assert_eq!(kind(b"H"), Err(io::ErrorKind::InvalidData));
        assert_eq!(kind(b"H_a.rs\0"), Err(io::ErrorKind::InvalidData));
        let overlong = [b"H ".as_slice(), &vec![b'a'; 9000], b"\0"].concat();
        assert_eq!(kind(&overlong), Err(io::ErrorKind::InvalidData));
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
