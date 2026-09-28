//! Native Roger idempotency and bounded restart reconciliation.
use super::{AskKind, AskRisk, RogerAsk, RogerCli, RogerReadTransport};
use crate::contracts::{ExternalRef, IdempotencyKey};
use crate::integrations::github::{CredentialRef, IntegrationError};
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Roger mutation transport. Native `--idem` must return the original Ask on replay.
pub trait RogerMutationTransport: RogerReadTransport {
    /// Create or retrieve one exact idempotent request, without polling for an answer.
    ///
    /// # Errors
    /// Any transport error is uncertain; retry only the same persisted key.
    fn submit(
        &self,
        credential: &CredentialRef,
        ask: &RogerAsk,
        key: &IdempotencyKey,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, IntegrationError>;
    /// Read candidate requests for restart reconciliation, without creating an Ask.
    ///
    /// # Errors
    /// Incomplete/offline results remain unknown, not proof of absence.
    fn find(
        &self,
        credential: &CredentialRef,
        ask: &RogerAsk,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Option<ExternalRef>, IntegrationError>;
}
impl RogerMutationTransport for RogerCli {
    fn submit(
        &self,
        credential: &CredentialRef,
        ask: &RogerAsk,
        key: &IdempotencyKey,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        ask.validate()?;
        if key.as_str().len() > 200 {
            return Err(IntegrationError::InvalidInput);
        }
        let binding = &ask.binding;
        let mut args = vec![
            "ask".into(),
            "--kind".into(),
            match ask.kind {
                AskKind::Approval => "approval",
                AskKind::Question => "question",
            }
            .into(),
            "--urgency".into(),
            "later".into(),
            "--risk".into(),
            match ask.risk {
                AskRisk::Routine => "routine",
                AskRisk::Sensitive => "sensitive",
                AskRisk::Irreversible => "irreversible",
            }
            .into(),
            format!("--title={}", ask.title.as_str()),
            "--decision-key".into(),
            binding.decision_key()?,
            "--idem".into(),
            key.as_str().into(),
            "--repo".into(),
            binding.repository.as_str().into(),
            "--action-verb".into(),
            binding.action.as_str().into(),
            "--action-target".into(),
            binding.target.as_str().into(),
            "--action-rev".into(),
            binding.subject.as_str().into(),
            format!("--action-limits={}", binding.limits.as_str()),
            "--resume-task".into(),
            binding.task.as_str().into(),
            "--resume-rev".into(),
            binding.subject.as_str().into(),
            "--body-file".into(),
            "-".into(),
        ];
        match ask.kind {
            AskKind::Approval => args.extend([
                "--option".into(),
                "approve:approve:Approve".into(),
                "--option".into(),
                "reject:reject:Reject".into(),
                "--option".into(),
                "revise:other:Revise".into(),
                "--input-required".into(),
                "revise".into(),
            ]),
            AskKind::Question => args.extend([
                "--option".into(),
                "reply:other:Reply".into(),
                "--input-required".into(),
                "reply".into(),
            ]),
        }
        if let Some(previous) = &ask.supersedes {
            args.extend(["--supersedes".into(), previous.as_str().into()]);
        }
        let output = self.call(
            credential,
            &args,
            ask.body.as_str().as_bytes(),
            timeout,
            max_bytes,
        )?;
        if output.code != Some(0) {
            return Err(IntegrationError::Unavailable);
        }
        Ok(output.stdout)
    }
    fn find(
        &self,
        credential: &CredentialRef,
        ask: &RogerAsk,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Option<ExternalRef>, IntegrationError> {
        let started = Instant::now();
        let mut remaining = max_bytes;
        let mut found = None;
        for state in ["--open", "--answered"] {
            let timeout = timeout
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or(IntegrationError::Timeout)?;
            if remaining == 0 {
                return Err(IntegrationError::LimitExceeded);
            }
            let output = self.call(
                credential,
                &["list".into(), state.into()],
                &[],
                timeout,
                remaining,
            )?;
            if output.code != Some(0) {
                return Err(IntegrationError::Unavailable);
            }
            remaining = remaining
                .checked_sub(output.stdout.len())
                .ok_or(IntegrationError::LimitExceeded)?;
            #[derive(Deserialize)]
            struct List {
                asks: Vec<Value>,
            }
            let list: List =
                serde_json::from_slice(&output.stdout).map_err(|_| IntegrationError::Unknown)?;
            if list.asks.len() > 1000 {
                return Err(IntegrationError::LimitExceeded);
            }
            for candidate in list.asks {
                if candidate.get("decisionKey").and_then(Value::as_str)
                    == Some(ask.binding.decision_key()?.as_str())
                    && candidate.pointer("/action/rev").and_then(Value::as_str)
                        == Some(ask.binding.subject.as_str())
                {
                    let id = validate_receipt(
                        credential,
                        ask,
                        &serde_json::to_vec(&candidate).map_err(|_| IntegrationError::Unknown)?,
                    )?;
                    if found.as_ref().is_some_and(|prior| prior != &id) {
                        return Err(IntegrationError::Unknown);
                    }
                    found = Some(id);
                }
            }
        }
        Ok(found)
    }
}

pub(crate) fn validate_receipt(
    credential: &CredentialRef,
    ask: &RogerAsk,
    bytes: &[u8],
) -> Result<ExternalRef, IntegrationError> {
    if bytes.len() > 128 * 1024 {
        return Err(IntegrationError::LimitExceeded);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| IntegrationError::Unknown)?;
    let binding = &ask.binding;
    let kind = match ask.kind {
        AskKind::Approval => "approval",
        AskKind::Question => "question",
    };
    let pairs = [
        ("/requester", credential.requester().as_str()),
        ("/repo", binding.repository.as_str()),
        ("/resume/task", binding.task.as_str()),
        ("/resume/rev", binding.subject.as_str()),
        ("/action/verb", binding.action.as_str()),
        ("/action/target", binding.target.as_str()),
        ("/action/rev", binding.subject.as_str()),
        ("/action/limits", binding.limits.as_str()),
        ("/kind", kind),
        ("/title", ask.title.as_str()),
        ("/body", ask.body.as_str()),
    ];
    if pairs
        .iter()
        .any(|(path, expected)| value.pointer(path).and_then(Value::as_str) != Some(expected))
        || value.get("decisionKey").and_then(Value::as_str)
            != Some(binding.decision_key()?.as_str())
    {
        return Err(IntegrationError::ScopeMismatch);
    }
    let id = ExternalRef::new(
        value
            .get("id")
            .and_then(Value::as_str)
            .ok_or(IntegrationError::Unknown)?,
    )
    .map_err(|_| IntegrationError::Unknown)?;
    super::client::validate_id(&id)?;
    Ok(id)
}
