//! Workflow policy built on the core contracts and the durable house store.
//!
//! Workflows are trigger-neutral: the same code serves a scheduled run and an
//! interactive session. The [`crate::contracts::Claimant`] passed in decides
//! where effect authority comes from, and both triggers share the same
//! durable claims.

pub mod audit;
pub mod budget;
pub mod cleanup;
pub mod coordination;
pub mod decomposition;
pub mod deliberation;
pub mod follow_up;
pub mod gardener;
pub mod gate;
pub mod inspector;
pub mod intake;
pub mod interactive;
pub mod pickup;
pub mod push;
pub mod ready;
pub mod recovery;
pub mod repair;
pub mod run;
pub mod sampling;
pub mod stack;
pub mod tick;
pub mod train;
pub mod triage;

use crate::{
    ConsumerId,
    contracts::Capability,
    integrations::github::{IntegrationError, Observation},
    scheduling::{PrecheckOutcome, WorkflowName},
};

/// A workflow Kitchen installs as a schedule, with the capabilities its
/// definition requires of the backend that runs it. A backend that can only
/// name the workflow a schedule runs, such as Orca through the automation
/// name, derives the requirements from here; a workflow not listed has none
/// Kitchen can establish, so its schedule is never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledWorkflow {
    /// The house budget tick ([`budget`]).
    Budget,
    /// The daily hygiene pass ([`gardener`]).
    Gardener,
}

impl ScheduledWorkflow {
    /// Every scheduled workflow.
    pub const ALL: [Self; 2] = [Self::Budget, Self::Gardener];

    /// The workflow named `workflow`, or `None` when Kitchen defines no
    /// schedule for it.
    #[must_use]
    pub fn find(workflow: &WorkflowName) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|scheduled| scheduled.as_str() == workflow.as_str())
    }

    /// The workflow name its schedules carry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Budget => budget::WORKFLOW,
            Self::Gardener => gardener::WORKFLOW,
        }
    }

    /// What its definition requires of the backend for scheduled execution.
    #[must_use]
    pub const fn required_capabilities(self) -> &'static [Capability] {
        match self {
            Self::Budget => &budget::REQUIRED_CAPABILITIES,
            Self::Gardener => &gardener::REQUIRED_CAPABILITIES,
        }
    }

    /// The one consumer this workflow's schedule serves, or `None` when the
    /// installer chooses it.
    const fn fixed_consumer(self) -> Option<&'static str> {
        match self {
            Self::Budget => Some(budget::WORKFLOW),
            Self::Gardener => None,
        }
    }

    /// Whether a schedule of this workflow may serve `consumer`: its fixed
    /// consumer, or for a chosen one, any consumer no other workflow fixes.
    #[must_use]
    pub fn serves(self, consumer: &ConsumerId) -> bool {
        match self.fixed_consumer() {
            Some(fixed) => consumer.as_str() == fixed,
            None => !Self::ALL
                .into_iter()
                .any(|other| other.fixed_consumer() == Some(consumer.as_str())),
        }
    }
}

impl std::fmt::Display for ScheduledWorkflow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 50 && !label.chars().any(char::is_control)
}

/// A read that did not complete stops the pass: unavailable is a failure,
/// incomplete or malformed evidence is never treated as empty.
fn known<T>(observation: Observation<T>) -> Result<T, WorkflowError> {
    match observation {
        Observation::Known(value) => Ok(value),
        Observation::Unavailable(source) => Err(WorkflowError::precheck(source)),
        Observation::Unknown => Err(WorkflowError::IncompleteEvidence),
    }
}

/// Workflow input or evidence failure. Private issue content is never included.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// The input is incomplete or inconsistent.
    #[error("incomplete workflow evidence")]
    IncompleteEvidence,
    /// An answered decision has no declared consumer.
    #[error("unknown decision owner")]
    UnknownDecisionOwner,
    /// The decision does not match this workflow's scope or revision.
    #[error("decision scope mismatch")]
    DecisionMismatch,
    /// A precheck read failed.
    #[error("workflow precheck failed: {source}")]
    PrecheckFailed {
        /// The sanitized integration or store failure.
        #[source]
        source: PrecheckCause,
    },
}

/// A precheck's failed read. Both source types omit private issue content and credentials.
#[derive(Debug, thiserror::Error)]
pub enum PrecheckCause {
    /// A house-scoped forge or decision read failed.
    #[error(transparent)]
    Integration(#[from] IntegrationError),
    /// A durable marker read failed.
    #[error(transparent)]
    Store(#[from] Box<crate::Error>),
}

impl PartialEq for WorkflowError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::IncompleteEvidence, Self::IncompleteEvidence)
            | (Self::UnknownDecisionOwner, Self::UnknownDecisionOwner)
            | (Self::DecisionMismatch, Self::DecisionMismatch) => true,
            (Self::PrecheckFailed { source: left }, Self::PrecheckFailed { source: right }) => {
                match (left, right) {
                    (PrecheckCause::Integration(left), PrecheckCause::Integration(right)) => {
                        left == right
                    }
                    (PrecheckCause::Store(left), PrecheckCause::Store(right)) => {
                        left.class() == right.class() && left.to_string() == right.to_string()
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
}

impl Eq for WorkflowError {}

impl WorkflowError {
    /// Preserve an integration read failure for a precheck caller.
    pub fn precheck(source: impl Into<PrecheckCause>) -> Self {
        Self::PrecheckFailed {
            source: source.into(),
        }
    }

    /// Preserve a sanitized house-store read failure for a precheck caller.
    pub fn precheck_store(source: crate::Error) -> Self {
        Self::precheck(Box::new(source))
    }

    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> crate::ErrorClass {
        match self {
            Self::IncompleteEvidence | Self::UnknownDecisionOwner => {
                crate::ErrorClass::InvalidInput
            }
            Self::DecisionMismatch => crate::ErrorClass::Refused,
            Self::PrecheckFailed { .. } => crate::ErrorClass::Execution,
        }
    }
}

/// Typed precheck outcome; read failures cannot masquerade as idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precheck {
    /// There is no changed input to inspect.
    Idle,
    /// At least one item needs inspection.
    Actionable,
}

/// The schedule outcome of a precheck result. Any error is
/// [`PrecheckOutcome::Error`], never idle.
#[must_use]
pub fn precheck_outcome(result: Result<Precheck, WorkflowError>) -> PrecheckOutcome {
    match result {
        Ok(Precheck::Actionable) => PrecheckOutcome::Actionable,
        Ok(Precheck::Idle) => PrecheckOutcome::Idle,
        Err(_) => PrecheckOutcome::Error,
    }
}

/// Durable ownership observation. An unknown claim cannot be treated as free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    /// The house store confirms no active claim.
    Unclaimed,
    /// Another worker owns the item.
    ClaimedByOther,
    /// The store could not establish ownership.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::{PrecheckCause, WorkflowError, known};
    use crate::integrations::github::{IntegrationError, Observation};

    #[test]
    fn failed_reads_keep_every_integration_cause() -> Result<(), Box<dyn std::error::Error>> {
        for cause in [
            IntegrationError::InvalidInput,
            IntegrationError::ScopeMismatch,
            IntegrationError::PermissionDenied,
            IntegrationError::BudgetExhausted,
            IntegrationError::Timeout,
            IntegrationError::Unavailable,
            IntegrationError::NotFound,
            IntegrationError::LimitExceeded,
            IntegrationError::Unknown,
            IntegrationError::StaleDecision,
        ] {
            let Err(WorkflowError::PrecheckFailed {
                source: PrecheckCause::Integration(actual),
            }) = known::<()>(Observation::Unavailable(cause))
            else {
                return Err("read failure did not retain its cause".into());
            };
            assert_eq!(actual, cause);
            assert_eq!(
                WorkflowError::precheck(actual).to_string(),
                format!("workflow precheck failed: {cause}")
            );
        }
        assert!(matches!(
            known::<()>(Observation::Unknown),
            Err(WorkflowError::IncompleteEvidence)
        ));
        assert!(known(Observation::Known(())).is_ok());
        Ok(())
    }
}
