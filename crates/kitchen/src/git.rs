//! A bounded, read-only `git` runner shared by Kitchen's Git readers.
//!
//! Every call runs `git` with a deadline and an output limit, without a
//! terminal prompt, optional locks, replace refs, or the file-system monitor,
//! and with the caller's `GIT_DIR`-style, configuration, and pathspec
//! overrides removed so the path alone selects the repository. Nothing here
//! writes to the repository.

use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdout, Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

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
/// Run one read-only `git` call in `dir` that must succeed.
pub(crate) fn run<S: AsRef<OsStr>>(
    dir: &Path,
    args: impl IntoIterator<Item = S>,
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
pub(crate) fn run_raw<S: AsRef<OsStr>>(
    dir: &Path,
    args: impl IntoIterator<Item = S>,
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

/// Run a bounded Git query with a bounded stdin payload.
pub(crate) fn run_raw_stdin<S: AsRef<OsStr>>(
    dir: &Path,
    args: impl IntoIterator<Item = S>,
    input: Vec<u8>,
    limits: &GitLimits,
) -> Result<(ExitStatus, String), GitReadError> {
    if input.len() > limits.max_output_bytes {
        return Err(GitReadError::OutputTooLarge);
    }
    let max = limits.max_output_bytes;
    let (status, output) = run_with_input(dir, args, limits, Some(input), move |stdout| {
        let mut buffer = Vec::new();
        stdout
            .take(u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut buffer)?;
        Ok(buffer)
    })?;
    if output.len() > max {
        return Err(GitReadError::OutputTooLarge);
    }
    Ok((
        status,
        String::from_utf8(output).map_err(|_| GitReadError::Malformed)?,
    ))
}

/// Run one read-only `git` call in `dir` under its deadline, handing its
/// standard output to `read` on a separate thread.
pub(crate) fn run_with<S: AsRef<OsStr>, T: Send + 'static>(
    dir: &Path,
    args: impl IntoIterator<Item = S>,
    limits: &GitLimits,
    read: impl FnOnce(ChildStdout) -> io::Result<T> + Send + 'static,
) -> Result<(ExitStatus, T), GitReadError> {
    run_with_input(dir, args, limits, None, read)
}

fn run_with_input<S: AsRef<OsStr>, T: Send + 'static>(
    dir: &Path,
    args: impl IntoIterator<Item = S>,
    limits: &GitLimits,
    input: Option<Vec<u8>>,
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
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in REDIRECTING_ENV {
        command.env_remove(name);
    }
    let mut child = command.spawn().map_err(|_| GitReadError::Spawn)?;
    let writer = if let Some(bytes) = input {
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitReadError::Spawn);
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = sender.send(stdin.write_all(&bytes));
        });
        Some(receiver)
    } else {
        None
    };
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
    if let Some(writer) = writer {
        match writer.recv_timeout(limits.call_timeout.saturating_sub(started.elapsed())) {
            Ok(Ok(())) => {}
            _ => return Err(GitReadError::Failed),
        }
    }
    Ok((status, output))
}
