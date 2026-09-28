//! Bounded Roger CLI access with explicit requester identity evidence.
use super::{DecisionBinding, DecisionStatus, validate_answer};
use crate::contracts::{ExternalRef, Text};
use crate::integrations::github::{
    CredentialFile, CredentialRef, HouseScope, IntegrationError, ReadLimits, process::run,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// Human decision kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskKind {
    /// Exact action approval with approve/reject options.
    Approval,
    /// Instructions only; answers cannot approve an action.
    Question,
}
/// Explicit operator-selected consequence level; never inferred downward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskRisk {
    /// Reversible routine work.
    Routine,
    /// Secrets, money, authorization, or data loss.
    Sensitive,
    /// Irreversible production, release, or equipment effects.
    Irreversible,
}
/// Payload to be persisted before asking a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RogerAsk {
    /// Exact decision scope.
    pub binding: DecisionBinding,
    /// Approval or question.
    pub kind: AskKind,
    /// Consequence level.
    pub risk: AskRisk,
    /// One-line title.
    pub title: Text,
    /// Sanitized question context.
    pub body: Text,
    /// Existing open request replaced after a revision change.
    pub supersedes: Option<ExternalRef>,
}
impl RogerAsk {
    /// Validate input before persistence and any effect.
    ///
    /// # Errors
    /// Refuses invalid binding, titles, body size, and Ask references.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        self.binding.decision_key()?;
        if self.title.as_str().chars().count() > 120
            || self.title.as_str().chars().any(char::is_control)
            || self.body.as_str().len() > 16 * 1024
        {
            return Err(IntegrationError::InvalidInput);
        }
        if let Some(id) = &self.supersedes {
            validate_id(id)?;
        }
        Ok(())
    }
}

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

/// Roger binary and verified requester probe, selected from private configuration.
/// A probe is a pre-existing Ask owned by this requester. If no probe is known,
/// provisioning remains incomplete; this adapter never guesses a token's owner.
#[derive(Debug, Clone)]
pub struct RogerCli {
    executable: PathBuf,
    credential: CredentialFile,
    probe: ExternalRef,
}
impl RogerCli {
    /// Configure bounded Roger calls. No network request occurs here.
    ///
    /// # Errors
    /// Requires an absolute binary path and a valid Ask id.
    pub fn new(
        executable: PathBuf,
        credential: CredentialFile,
        probe: ExternalRef,
    ) -> Result<Self, IntegrationError> {
        if !executable.is_absolute() {
            return Err(IntegrationError::InvalidInput);
        }
        validate_id(&probe)?;
        Ok(Self {
            executable,
            credential,
            probe,
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
        let token = self.credential.load(reference)?;
        run(
            &self.executable,
            args,
            input,
            &[
                ("ROGER_TOKEN", &token),
                ("ROGER_URL", "https://roger.origin89.com"),
            ],
            timeout,
            max_bytes,
        )
    }
    pub(crate) fn verify_requester(
        &self,
        reference: &CredentialRef,
        timeout: Duration,
    ) -> Result<(), IntegrationError> {
        let output = self.call(
            reference,
            &["get".into(), "--".into(), self.probe.as_str().into()],
            &[],
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
        Ok(())
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
        let started = Instant::now();
        self.verify_requester(credential, timeout)?;
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(IntegrationError::Timeout)?;
        let output = self.call(
            credential,
            &["get".into(), "--".into(), ask.as_str().into()],
            &[],
            remaining,
            max_bytes,
        )?;
        if !matches!(output.code, Some(0 | 10 | 11 | 20 | 21 | 22)) {
            return Err(IntegrationError::Unavailable);
        }
        Ok(output.stdout)
    }
}
fn validate_id(id: &ExternalRef) -> Result<(), IntegrationError> {
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
