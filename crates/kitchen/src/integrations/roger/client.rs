//! Bounded Roger CLI access with explicit requester identity evidence.
use super::{DecisionBinding, DecisionStatus, validate_answer};
use crate::contracts::ExternalRef;
use crate::integrations::github::{
    CredentialFile, CredentialRef, HouseScope, IntegrationError, ReadLimits, process::run,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Roger read boundary for deterministic offline tests and bounded CLI use.
pub trait RogerReadTransport {
    /// Read a known Ask. The caller validates all returned decision bindings.
    ///
    /// # Errors
    /// Offline/auth/time-limit errors do not imply an answer or a missing Ask.
    fn get(
        &self,
        credential: &CredentialRef,
        ask: &ExternalRef,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, IntegrationError>;
}
/// Scoped reader for a persisted Ask receipt. Polling does not submit a new Ask.
pub struct RogerClient<T> {
    scope: HouseScope,
    transport: T,
    limits: ReadLimits,
}
impl<T: RogerReadTransport> RogerClient<T> {
    /// Select a private house boundary before any access.
    pub const fn new(scope: HouseScope, transport: T, limits: ReadLimits) -> Self {
        Self {
            scope,
            transport,
            limits,
        }
    }
    /// Poll after restart using the persisted receipt and original binding.
    ///
    /// # Errors
    /// Refuses cross-house, stale, malformed, or unavailable evidence.
    pub fn poll(
        &self,
        binding: &DecisionBinding,
        ask: &ExternalRef,
    ) -> Result<DecisionStatus, IntegrationError> {
        binding.validate(&self.scope)?;
        validate_id(ask)?;
        let bytes = self.transport.get(
            self.scope.credential(),
            ask,
            self.limits.timeout(),
            self.limits.bytes().min(128 * 1024),
        )?;
        validate_answer(&self.scope, binding, ask, &bytes)
    }
}

/// Local optional Roger capability. Detection never reads credentials or contacts Roger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RogerAvailability {
    /// The local CLI advertises the bounded adapter's required arguments.
    Available,
    /// No binary exists at the explicitly selected path.
    NotInstalled,
    /// A binary exists but does not advertise the required interface.
    Unsupported,
}

/// Roger binary and verified requester probe, selected from private configuration.
/// A probe is a pre-existing Ask owned by this requester. If no probe is known,
/// provisioning remains incomplete; this adapter never guesses a token's owner.
#[derive(Debug, Clone)]
pub struct RogerCli {
    executable: PathBuf,
    credential: CredentialFile,
    probe: ExternalRef,
    base_url: String,
}
impl RogerCli {
    /// Detect the optional local CLI without credentials, network calls, or installation.
    ///
    /// # Errors
    /// Invalid paths, unreadable files, failed help calls and deadlines remain errors.
    pub fn detect(executable: &Path) -> Result<RogerAvailability, IntegrationError> {
        if !executable.is_absolute() {
            return Err(IntegrationError::InvalidInput);
        }
        match std::fs::metadata(executable) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RogerAvailability::NotInstalled);
            }
            Err(_) => return Err(IntegrationError::Unavailable),
            Ok(metadata) if !metadata.is_file() => return Ok(RogerAvailability::Unsupported),
            Ok(_) => {}
        }
        let output = run(
            executable,
            &["ask".into(), "--help".into()],
            &[],
            &[],
            Duration::from_secs(2),
            32 * 1024,
        )?;
        if output.code != Some(0) {
            return Err(IntegrationError::Unavailable);
        }
        let help = std::str::from_utf8(&output.stdout).map_err(|_| IntegrationError::Unknown)?;
        Ok(
            if [
                "--idem",
                "--decision-key",
                "--action-rev",
                "--action-target",
                "--action-limits",
                "--resume-task",
                "--resume-rev",
                "--body-file",
            ]
            .iter()
            .all(|flag| help.contains(flag))
            {
                RogerAvailability::Available
            } else {
                RogerAvailability::Unsupported
            },
        )
    }
    /// Configure a house-selected HTTPS deployment.
    ///
    /// # Errors
    /// Refuses malformed endpoints and unsupported CLI installations.
    pub fn new(
        executable: PathBuf,
        credential: CredentialFile,
        probe: ExternalRef,
        base_url: String,
    ) -> Result<Self, IntegrationError> {
        let host = base_url
            .strip_prefix("https://")
            .ok_or(IntegrationError::InvalidInput)?;
        let (name, port) = host
            .split_once(':')
            .map_or((host, None), |(name, port)| (name, Some(port)));
        if name.is_empty()
            || name.split('.').any(|label| {
                label.is_empty()
                    || !label
                        .bytes()
                        .next()
                        .is_some_and(|byte| byte.is_ascii_alphanumeric())
                    || !label
                        .bytes()
                        .last()
                        .is_some_and(|byte| byte.is_ascii_alphanumeric())
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            || port.is_some_and(|port| port.parse::<u16>().map_or(true, |value| value == 0))
        {
            return Err(IntegrationError::InvalidInput);
        }
        if !executable.is_absolute() {
            return Err(IntegrationError::InvalidInput);
        }
        validate_id(&probe)?;
        if Self::detect(&executable)? != RogerAvailability::Available {
            return Err(IntegrationError::Unavailable);
        }
        Ok(Self {
            executable,
            credential,
            probe,
            base_url,
        })
    }
    pub(crate) fn call(
        &self,
        reference: &CredentialRef,
        args: &[String],
        input: &[u8],
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<super::super::github::process::ProcessOutput, IntegrationError> {
        let started = Instant::now();
        let token = self.credential.load(reference)?;
        let env = [
            ("ROGER_TOKEN", token.as_str()),
            ("ROGER_URL", self.base_url.as_str()),
        ];
        let output = run(
            &self.executable,
            &["get".into(), "--".into(), self.probe.as_str().into()],
            &[],
            &env,
            timeout,
            128 * 1024,
        )?;
        if !matches!(output.code, Some(0 | 10 | 11 | 20 | 21 | 22)) {
            return Err(IntegrationError::Unavailable);
        }
        #[derive(Deserialize)]
        struct Probe {
            id: ExternalRef,
            requester: ExternalRef,
        }
        let probe: Probe =
            serde_json::from_slice(&output.stdout).map_err(|_| IntegrationError::Unknown)?;
        if probe.id != self.probe || &probe.requester != reference.requester() {
            return Err(IntegrationError::ScopeMismatch);
        }
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(IntegrationError::Timeout)?;
        run(&self.executable, args, input, &env, remaining, max_bytes)
    }
}
impl RogerReadTransport for RogerCli {
    fn get(
        &self,
        credential: &CredentialRef,
        ask: &ExternalRef,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        validate_id(ask)?;
        let output = self.call(
            credential,
            &["get".into(), "--".into(), ask.as_str().into()],
            &[],
            timeout,
            max_bytes,
        )?;
        if !matches!(output.code, Some(0 | 10 | 11 | 20 | 21 | 22)) {
            return Err(IntegrationError::Unavailable);
        }
        Ok(output.stdout)
    }
}
pub(crate) fn validate_id(id: &ExternalRef) -> Result<(), IntegrationError> {
    // Roger ids are ULIDs; reject flags, paths, and ambiguous positional input.
    if id.as_str().len() != 26
        || !id
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase() && !b"ILOU".contains(&b))
    {
        return Err(IntegrationError::InvalidInput);
    }
    Ok(())
}
