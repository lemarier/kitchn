//! Bounded Orca subprocesses.
//!
//! Every call runs the Orca executable directly with separate arguments (no
//! shell), a cleared environment plus an allowlist, no stdin, discarded
//! stderr, a deadline, and a stdout byte limit. A call that exceeds either
//! bound is killed and reported as such; it never counts as success.

use std::{
    ffi::OsString,
    io::{self, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::adapters::orca::OrcaError;

/// Environment variables passed through to Orca. Terminal identity variables
/// such as `ORCA_TERMINAL_HANDLE` are deliberately excluded so the adapter
/// never borrows the identity of the terminal it happens to run in.
pub const ENV_ALLOWLIST: [&str; 9] = [
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "ORCA_USER_DATA_PATH",
];

/// Default stdout limit: automation listings carry full prompts.
pub const DEFAULT_MAX_STDOUT: usize = 8 * 1024 * 1024;

/// How often a running call is polled for exit.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long to wait for the stdout reader after the process exits.
const READER_GRACE: Duration = Duration::from_secs(2);

/// One Orca call: arguments after the executable, and a deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    args: Vec<String>,
    deadline: Duration,
}

impl Invocation {
    /// Build an invocation.
    #[must_use]
    pub const fn new(args: Vec<String>, deadline: Duration) -> Self {
        Self { args, deadline }
    }

    /// The arguments, one element per argv entry.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The deadline after which the call is killed.
    #[must_use]
    pub const fn deadline(&self) -> Duration {
        self.deadline
    }
}

/// A finished call's exit code and stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawOutput {
    /// The exit code; `None` when the process ended by signal.
    pub exit_code: Option<i32>,
    /// Captured stdout, within the runner's limit.
    pub stdout: Vec<u8>,
}

/// Runs Orca invocations. Tests substitute a simulated runtime.
pub trait OrcaRunner: Send + Sync {
    /// Run one invocation to completion or its deadline.
    ///
    /// # Errors
    /// [`OrcaError::Spawn`] when nothing started; [`OrcaError::Timeout`],
    /// [`OrcaError::OutputLimit`], or [`OrcaError::Io`] after it started.
    fn run(&self, invocation: &Invocation) -> Result<RawOutput, OrcaError>;
}

impl<T: OrcaRunner + ?Sized> OrcaRunner for &T {
    fn run(&self, invocation: &Invocation) -> Result<RawOutput, OrcaError> {
        (**self).run(invocation)
    }
}

/// Runs the real Orca executable.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
    max_stdout: usize,
}

impl SystemRunner {
    /// Run `program` with the allowlisted variables of the current environment.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let env = ENV_ALLOWLIST
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
            .collect();
        Self {
            program: program.into(),
            env,
            max_stdout: DEFAULT_MAX_STDOUT,
        }
    }

    /// Use a different stdout limit.
    #[must_use]
    pub const fn with_max_stdout(mut self, max_stdout: usize) -> Self {
        self.max_stdout = max_stdout;
        self
    }
}

enum ReadResult {
    Complete(Vec<u8>),
    Overflow,
    Failed(io::ErrorKind),
}

fn read_bounded(mut source: impl Read, limit: usize) -> ReadResult {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match source.read(&mut chunk) {
            Ok(0) => return ReadResult::Complete(buffer),
            Ok(read) => {
                let Some(bytes) = chunk.get(..read) else {
                    return ReadResult::Failed(io::ErrorKind::InvalidData);
                };
                if buffer.len().saturating_add(bytes.len()) > limit {
                    return ReadResult::Overflow;
                }
                buffer.extend_from_slice(bytes);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return ReadResult::Failed(error.kind()),
        }
    }
}

impl OrcaRunner for SystemRunner {
    fn run(&self, invocation: &Invocation) -> Result<RawOutput, OrcaError> {
        let started = Instant::now();
        let mut child = Command::new(&self.program)
            .args(invocation.args())
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| OrcaError::Spawn(error.kind()))?;
        let Some(stdout) = child.stdout.take() else {
            // The pipe was requested above, so this does not happen; still,
            // stop the process rather than wait on output nobody reads.
            let _ = child.kill();
            let _ = child.wait();
            return Err(OrcaError::Io(io::ErrorKind::BrokenPipe));
        };
        let limit = self.max_stdout;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(read_bounded(stdout, limit));
        });
        let mut early: Option<ReadResult> = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(OrcaError::Io(error.kind()));
                }
            }
            if early.is_none() {
                match receiver.try_recv() {
                    Ok(ReadResult::Overflow) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(OrcaError::OutputLimit { limit });
                    }
                    Ok(ReadResult::Failed(kind)) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(OrcaError::Io(kind));
                    }
                    // Output ended before the process exited; keep waiting.
                    Ok(complete @ ReadResult::Complete(_)) => early = Some(complete),
                    Err(_) => {}
                }
            }
            if started.elapsed() >= invocation.deadline() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OrcaError::Timeout);
            }
            thread::sleep(POLL_INTERVAL);
        };
        let result = match early {
            Some(result) => result,
            None => receiver
                .recv_timeout(READER_GRACE)
                .map_err(|_| OrcaError::Timeout)?,
        };
        match result {
            ReadResult::Complete(stdout) => Ok(RawOutput {
                exit_code: status.code(),
                stdout,
            }),
            ReadResult::Overflow => Err(OrcaError::OutputLimit { limit }),
            ReadResult::Failed(kind) => Err(OrcaError::Io(kind)),
        }
    }
}
