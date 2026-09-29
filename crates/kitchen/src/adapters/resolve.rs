//! The one place a command builds a house's worker backend.
//!
//! [`resolve_backend`] reads the house's [`BackendBinding`], refuses a house
//! without one or bound to a backend this Kitchen does not know, connects,
//! and refuses a backend that does not fully support what the caller's
//! workflow requires. Commands never connect to a backend themselves.

use std::{path::PathBuf, time::Duration};

use super::orca::{OrcaBackend, OrcaConfig, OrcaError, OrcaRunner};
use crate::{
    ErrorClass, HouseId,
    contracts::{BranchName, Capability, ContractError, EffectExecutor, ExternalRef},
    house::{BackendBinding, BackendKind, BackendName, HouseConfig},
    scheduling::AgentFamily,
};

/// What one command's Orca calls name, and where this host keeps Orca state.
/// The backend namespace and credential come from the house's binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaSession {
    /// See [`OrcaConfig::run`].
    pub run: ExternalRef,
    /// See [`OrcaConfig::coordinator`].
    pub coordinator: ExternalRef,
    /// See [`OrcaConfig::repo`].
    pub repo: ExternalRef,
    /// See [`OrcaConfig::base_branch`].
    pub base_branch: Option<ExternalRef>,
    /// See [`OrcaConfig::branch_prefix`].
    pub branch_prefix: Option<BranchName>,
    /// See [`OrcaConfig::agent`].
    pub agent: AgentFamily,
    /// See [`OrcaConfig::call_timeout`].
    pub call_timeout: Duration,
    /// See [`OrcaConfig::launch_timeout`].
    pub launch_timeout: Duration,
    /// See [`OrcaConfig::runtime_dir`].
    pub runtime_dir: PathBuf,
    /// See [`OrcaConfig::reservation_timeout`].
    pub reservation_timeout: Duration,
}

/// Why no backend was built for a house.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// The house declares no worker backend.
    #[error(
        "house {house} has no worker backend binding; rerun `kitchn house init` with the same answers, or add \"backend\": {{\"kind\": \"orca\", \"backend\": <namespace>, \"credential\": <name>}} to its configuration"
    )]
    Unbound {
        /// The house.
        house: HouseId,
    },
    /// The house is bound to a backend this Kitchen cannot build.
    #[error(
        "house {house} is bound to worker backend `{name}`, which this Kitchen does not support (supported: orca)"
    )]
    Unknown {
        /// The house.
        house: HouseId,
        /// The backend name its binding stores.
        name: BackendName,
    },
    /// A backend namespace or credential the caller named is not the one the
    /// house is bound to.
    #[error(
        "the backend or credential given for house {house} differs from its worker backend binding"
    )]
    BindingMismatch {
        /// The house.
        house: HouseId,
    },
    /// The backend lacks capabilities the workflow requires.
    #[error("worker backend {kind} of house {house} cannot run this workflow: {source}")]
    Unsupported {
        /// The house.
        house: HouseId,
        /// The bound backend.
        kind: BackendKind,
        /// The missing and partial capabilities.
        #[source]
        source: ContractError,
    },
    /// Connecting to Orca failed.
    #[error(transparent)]
    Orca(#[from] OrcaError),
}

impl BackendError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::BindingMismatch { .. } => ErrorClass::InvalidInput,
            Self::Unbound { .. } | Self::Unknown { .. } | Self::Unsupported { .. } => {
                ErrorClass::Refused
            }
            Self::Orca(error) => error.class(),
        }
    }
}

/// The house's worker backend binding, or why it has none Kitchen can build.
///
/// # Errors
/// [`BackendError::Unbound`] and [`BackendError::Unknown`].
pub fn backend_binding(
    house: &HouseConfig,
) -> Result<(&BackendBinding, BackendKind), BackendError> {
    let binding = house
        .backend
        .as_ref()
        .ok_or_else(|| BackendError::Unbound {
            house: house.house.clone(),
        })?;
    let kind = binding.kind.kind().ok_or_else(|| BackendError::Unknown {
        house: house.house.clone(),
        name: binding.kind.clone(),
    })?;
    Ok((binding, kind))
}

/// Build the worker backend `house` is bound to, then refuse it unless it
/// fully supports every capability in `required`. Every command that needs a
/// backend builds it here. The backend enforces the house's schedule limits,
/// when it sets any, on schedule installs and activations.
///
/// # Errors
/// [`BackendError::Unbound`] and [`BackendError::Unknown`] before anything
/// is contacted, [`BackendError::Orca`] when the runtime probe fails, and
/// [`BackendError::Unsupported`] naming every missing and partial capability.
pub fn resolve_backend<R: OrcaRunner>(
    house: &HouseConfig,
    session: OrcaSession,
    runner: R,
    required: &[Capability],
) -> Result<OrcaBackend<R>, BackendError> {
    let (binding, kind) = backend_binding(house)?;
    let backend = match kind {
        BackendKind::Orca => OrcaBackend::connect(
            OrcaConfig {
                backend: binding.backend.clone(),
                house: house.house.clone(),
                credential: binding.credential.clone(),
                run: session.run,
                coordinator: session.coordinator,
                repo: session.repo,
                base_branch: session.base_branch,
                branch_prefix: session.branch_prefix,
                agent: session.agent,
                call_timeout: session.call_timeout,
                launch_timeout: session.launch_timeout,
                runtime_dir: session.runtime_dir,
                reservation_timeout: session.reservation_timeout,
            },
            runner,
        )?,
    };
    let backend = match &house.schedules {
        Some(policy) => backend.with_schedule_policy(policy.clone()),
        None => backend,
    };
    EffectExecutor::descriptor(&backend)
        .capabilities
        .require(required.iter().copied())
        .map_err(|source| BackendError::Unsupported {
            house: house.house.clone(),
            kind,
            source,
        })?;
    Ok(backend)
}
