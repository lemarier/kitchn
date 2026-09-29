//! Bounded subprocess boundary with isolated credentials and redacted failures.

use super::{CredentialRef, GitHubReadTransport, IntegrationError, ReadRequest};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Explicit private credential-file binding. Never falls back to host CLI login.
#[derive(Clone)]
pub struct CredentialFile {
    reference: CredentialRef,
    source: Source,
}

/// Where a token is read from.
#[derive(Clone)]
enum Source {
    /// A caller-selected path, opened on each load.
    Path(PathBuf),
    /// A descriptor its owner opened and checked; read in place, never
    /// reopened by path.
    #[cfg(unix)]
    Opened(std::sync::Arc<File>),
}

/// Largest accepted token file, in bytes.
const TOKEN_LIMIT: u16 = 16 * 1024;

#[cfg(all(test, unix))]
mod isolation_tests {
    use super::*;

    #[test]
    fn child_receives_only_selected_environment_and_private_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let output = run(
            Path::new("/usr/bin/env"),
            &[],
            &[],
            &[("GH_TOKEN", "fixture-secret")],
            Duration::from_secs(2),
            65536,
        )?;
        assert_eq!(output.code, Some(0));
        let vars = String::from_utf8(output.stdout)?;
        let lines: Vec<_> = vars.lines().collect();
        assert!(lines.contains(&"GH_TOKEN=fixture-secret"));
        assert!(lines.contains(&"NO_COLOR=1"));
        assert!(!lines.iter().any(|line| line.starts_with("PATH=")
            || line.starts_with("ROGER_TOKEN=")
            || line.starts_with("USER=")));
        let home = lines
            .iter()
            .find_map(|line| line.strip_prefix("HOME="))
            .ok_or("missing HOME")?;
        let config = lines
            .iter()
            .find_map(|line| line.strip_prefix("GH_CONFIG_DIR="))
            .ok_or("missing GH_CONFIG_DIR")?;
        assert_eq!(home, config);
        assert_ne!(Some(home), std::env::var("HOME").ok().as_deref());
        assert!(!Path::new(home).exists());
        Ok(())
    }
}
impl std::fmt::Debug for CredentialFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialFile([private])")
    }
}
impl CredentialFile {
    /// Select a credential file from private house configuration.
    ///
    /// # Errors
    /// Requires an absolute path; reading remains deferred until scope validation.
    pub fn new(reference: CredentialRef, path: PathBuf) -> Result<Self, IntegrationError> {
        if !path.is_absolute() {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(Self {
            reference,
            source: Source::Path(path),
        })
    }
    /// Bind a token file the house layer already opened and checked. Every
    /// load reads this descriptor from its start, so replacing the file at
    /// its path afterwards does not change the token.
    #[cfg(unix)]
    pub(crate) fn opened(reference: CredentialRef, file: File) -> Self {
        Self {
            reference,
            source: Source::Opened(std::sync::Arc::new(file)),
        }
    }
    /// Bound reference, without the credential or private path.
    #[must_use]
    pub const fn reference(&self) -> &CredentialRef {
        &self.reference
    }
    pub(crate) fn load(&self, requested: &CredentialRef) -> Result<String, IntegrationError> {
        if requested != &self.reference {
            return Err(IntegrationError::ScopeMismatch);
        }
        let bytes = match &self.source {
            Source::Path(path) => {
                let file = File::open(path).map_err(|_| IntegrationError::Unavailable)?;
                if !file
                    .metadata()
                    .map_err(|_| IntegrationError::Unavailable)?
                    .is_file()
                {
                    return Err(IntegrationError::InvalidInput);
                }
                let mut bytes = Vec::new();
                file.take(u64::from(TOKEN_LIMIT) + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| IntegrationError::Unavailable)?;
                bytes
            }
            #[cfg(unix)]
            Source::Opened(file) => read_from_start(file)?,
        };
        if bytes.len() > usize::from(TOKEN_LIMIT) {
            return Err(IntegrationError::LimitExceeded);
        }
        let token = String::from_utf8(bytes).map_err(|_| IntegrationError::InvalidInput)?;
        let token = token.trim();
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(token.into())
    }
}

/// Read up to one byte past [`TOKEN_LIMIT`] from offset zero with positioned
/// reads, so clones sharing the descriptor never move each other's offset.
#[cfg(unix)]
fn read_from_start(file: &File) -> Result<Vec<u8>, IntegrationError> {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0; usize::from(TOKEN_LIMIT) + 1];
    let mut filled = 0;
    while let Some(rest) = bytes.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        let offset = u64::try_from(filled).map_err(|_| IntegrationError::LimitExceeded)?;
        match file.read_at(rest, offset) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(IntegrationError::Unavailable),
        }
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// Installed GitHub CLI, pinned by absolute executable path.
#[derive(Debug, Clone)]
pub struct GhCli {
    executable: PathBuf,
    credential: CredentialFile,
}
impl GhCli {
    /// Select the binary and private credential binding without invoking them.
    ///
    /// # Errors
    /// Refuses a relative executable path.
    pub fn new(executable: PathBuf, credential: CredentialFile) -> Result<Self, IntegrationError> {
        if !executable.is_absolute() {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(Self {
            executable,
            credential,
        })
    }
    pub(crate) fn call(
        &self,
        reference: &CredentialRef,
        args: &[String],
        input: &[u8],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<ProcessOutput, IntegrationError> {
        let started = Instant::now();
        let token = self.verified_token(reference, timeout)?;
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(IntegrationError::Timeout)?;
        self.run_with_token(&token, args, input, remaining, max_bytes)
    }
    pub(crate) fn verified_token(
        &self,
        reference: &CredentialRef,
        timeout: Duration,
    ) -> Result<String, IntegrationError> {
        let token = self.credential.load(reference)?;
        let env = [
            ("GH_TOKEN", token.as_str()),
            ("GH_HOST", "github.com"),
            ("GH_PROMPT_DISABLED", "1"),
        ];
        let identity = run(
            &self.executable,
            &[
                "api".into(),
                "--hostname".into(),
                "github.com".into(),
                "user".into(),
            ],
            &[],
            &env,
            timeout,
            16 * 1024,
        )?;
        if identity.code != Some(0) {
            return Err(IntegrationError::Unavailable);
        }
        let user: super::User =
            serde_json::from_slice(&identity.stdout).map_err(|_| IntegrationError::Unknown)?;
        if !user
            .login
            .eq_ignore_ascii_case(reference.requester().as_str())
        {
            return Err(IntegrationError::ScopeMismatch);
        }
        Ok(token)
    }
    pub(crate) fn run_with_token(
        &self,
        token: &str,
        args: &[String],
        input: &[u8],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<ProcessOutput, IntegrationError> {
        let env = [
            ("GH_TOKEN", token),
            ("GH_HOST", "github.com"),
            ("GH_PROMPT_DISABLED", "1"),
        ];
        run(&self.executable, args, input, &env, timeout, max_bytes)
    }
}
impl GitHubReadTransport for GhCli {
    fn read(
        &self,
        credential: &CredentialRef,
        request: &ReadRequest,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        let mut args = vec![
            "api".into(),
            "--hostname".into(),
            "github.com".into(),
            "--method".into(),
            if request.graphql.is_some() {
                "POST".into()
            } else {
                "GET".into()
            },
            request.endpoint.clone(),
        ];
        let input = if let Some(body) = &request.graphql {
            args.extend(["--input".into(), "-".into()]);
            serde_json::to_vec(body).map_err(|_| IntegrationError::InvalidInput)?
        } else {
            Vec::new()
        };
        let output = self.call(credential, &args, &input, timeout, max_bytes)?;
        if output.code != Some(0) {
            return Err(IntegrationError::Unavailable);
        }
        Ok(output.stdout)
    }
}

pub(crate) struct ProcessOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
}

/// File-backed output avoids a pipe-reader thread remaining blocked after timeout.
/// No ambient token, HOME, CLI config, proxy, shell, or pager is inherited.
pub(crate) fn run(
    executable: &Path,
    args: &[String],
    input: &[u8],
    environment: &[(&str, &str)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<ProcessOutput, IntegrationError> {
    if timeout.is_zero()
        || timeout > Duration::from_secs(60)
        || max_bytes == 0
        || max_bytes > 8 * 1024 * 1024
        || input.len() > 128 * 1024
    {
        return Err(IntegrationError::InvalidInput);
    }
    let private = tempfile::tempdir().map_err(|_| IntegrationError::Unavailable)?;
    let mut stdin = tempfile::tempfile().map_err(|_| IntegrationError::Unavailable)?;
    stdin
        .write_all(input)
        .and_then(|()| stdin.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|_| IntegrationError::Unavailable)?;
    let mut stdout = tempfile::tempfile().map_err(|_| IntegrationError::Unavailable)?;
    let stderr = tempfile::tempfile().map_err(|_| IntegrationError::Unavailable)?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .env("HOME", private.path())
        .env("GH_CONFIG_DIR", private.path())
        .env("NO_COLOR", "1")
        .envs(environment.iter().copied())
        .current_dir(private.path())
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(
            stdout
                .try_clone()
                .map_err(|_| IntegrationError::Unavailable)?,
        ))
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .map_err(|_| IntegrationError::Unavailable)?,
        ));
    let started = Instant::now();
    let mut child = command.spawn().map_err(|_| IntegrationError::Unavailable)?;
    let outcome = loop {
        let size = stdout
            .metadata()
            .and_then(|a| stderr.metadata().map(|b| a.len().saturating_add(b.len())));
        match size {
            Ok(size) if size > max_bytes as u64 => break Err(IntegrationError::LimitExceeded),
            Err(_) => break Err(IntegrationError::Unavailable),
            Ok(_) => {}
        }
        if started.elapsed() >= timeout {
            break Err(IntegrationError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.code()),
            Ok(None) => thread::sleep(
                Duration::from_millis(5).min(timeout.saturating_sub(started.elapsed())),
            ),
            Err(_) => break Err(IntegrationError::Unavailable),
        }
    };
    if outcome.is_err() {
        // Always attempt both termination and reap; failure cannot become success.
        let killed = child.kill();
        let reaped = child.wait();
        if killed.is_err() && reaped.is_err() {
            return Err(IntegrationError::Unavailable);
        }
    }
    let code = outcome?;
    stdout
        .seek(SeekFrom::Start(0))
        .map_err(|_| IntegrationError::Unavailable)?;
    let mut bytes = Vec::new();
    stdout
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| IntegrationError::Unavailable)?;
    if bytes.len() > max_bytes {
        return Err(IntegrationError::LimitExceeded);
    }
    Ok(ProcessOutput {
        code,
        stdout: bytes,
    })
}
