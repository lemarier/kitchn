//! The one place a command builds a house's worker backend.
//!
//! [`resolve_backend`] (Orca) and [`resolve_http_backend`] (HTTP) read the
//! house's [`BackendBinding`], refuse a house without one, bound to a backend
//! this Kitchen does not know, or bound to the other kind, connect, and
//! refuse a backend that does not fully support what the caller's workflow
//! requires. Commands never connect to a backend themselves.

use std::{io::Read, path::PathBuf, time::Duration};

use super::{
    http::{HttpBackend, HttpConfig, HttpError},
    orca::{OrcaBackend, OrcaConfig, OrcaError, OrcaRunner},
};
use crate::{
    CredentialId, ErrorClass, HouseId,
    adoption::HouseRegistry,
    contracts::{BranchName, Capability, ContractError, EffectExecutor, ExternalRef},
    house::{
        BackendBinding, BackendKind, BackendName, CredentialStatus, HouseConfig, HouseError,
        open_credential,
    },
    scheduling::AgentFamily,
};

/// Largest accepted token file, in bytes.
const TOKEN_LIMIT: u64 = 16 * 1024;

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

/// What one command's HTTP backend calls name. The endpoint, backend
/// namespace, and credential come from the house's binding; the token is read
/// from the house's private registry directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSession {
    /// See [`HttpConfig::run`].
    pub run: ExternalRef,
    /// See [`HttpConfig::coordinator`].
    pub coordinator: ExternalRef,
    /// See [`HttpConfig::curl`].
    pub curl: PathBuf,
    /// See [`HttpConfig::call_timeout`].
    pub call_timeout: Duration,
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
        "house {house} is bound to worker backend `{name}`, which this Kitchen does not support (supported: orca, http)"
    )]
    Unknown {
        /// The house.
        house: HouseId,
        /// The backend name its binding stores.
        name: BackendName,
    },
    /// The house is bound to another kind of backend than the caller builds.
    #[error("house {house} is bound to worker backend {bound}, but this command needs {needed}")]
    KindMismatch {
        /// The house.
        house: HouseId,
        /// The bound kind.
        bound: BackendKind,
        /// The kind the command builds.
        needed: BackendKind,
    },
    /// An HTTP binding without an endpoint, or another kind's binding with one.
    #[error(
        "the worker backend binding of house {house} is {kind}; only an http binding has an endpoint, and it must"
    )]
    Endpoint {
        /// The house.
        house: HouseId,
        /// The bound kind.
        kind: BackendKind,
    },
    /// The backend's token file is not usable. Nothing was read or sent.
    #[error(
        "credential {credential} of house {house} is {status}; put the HTTP backend's bearer token in private/{house}/credentials/{credential} in the house registry, mode 600"
    )]
    CredentialUnavailable {
        /// The house.
        house: HouseId,
        /// The credential name.
        credential: CredentialId,
        /// Why it cannot be used.
        status: CredentialStatus,
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
    /// Connecting to an HTTP backend failed.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// Reading the house's registry failed.
    #[error(transparent)]
    House(#[from] HouseError),
}

impl BackendError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::BindingMismatch { .. } => ErrorClass::InvalidInput,
            Self::Unbound { .. }
            | Self::Unknown { .. }
            | Self::Unsupported { .. }
            | Self::KindMismatch { .. }
            | Self::Endpoint { .. }
            | Self::CredentialUnavailable { .. } => ErrorClass::Refused,
            Self::Orca(error) => error.class(),
            Self::Http(error) => error.class(),
            Self::House(error) => error.class(),
        }
    }
}

/// The house's worker backend binding, or why it has none Kitchen can build.
///
/// # Errors
/// [`BackendError::Unbound`], [`BackendError::Unknown`], and
/// [`BackendError::Endpoint`].
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
    if (kind == BackendKind::Http) != binding.endpoint.is_some() {
        return Err(BackendError::Endpoint {
            house: house.house.clone(),
            kind,
        });
    }
    Ok((binding, kind))
}

/// Build the worker backend `house` is bound to, then refuse it unless it
/// fully supports every capability in `required`. Every command that needs a
/// backend builds it here. The backend enforces the house's schedule limits,
/// when it sets any, on schedule installs and activations.
///
/// # Errors
/// [`BackendError::Unbound`], [`BackendError::Unknown`], and
/// [`BackendError::KindMismatch`] for a house bound to HTTP before anything
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
        BackendKind::Http => {
            return Err(BackendError::KindMismatch {
                house: house.house.clone(),
                bound: kind,
                needed: BackendKind::Orca,
            });
        }
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

/// Build the HTTP worker backend `house` is bound to, then refuse it unless
/// it fully supports every capability in `required`. The bearer token is
/// read from `private/<house>/credentials/<credential>` in `registry`, with
/// the same no-follow and owner-only checks as a forge token, and never
/// leaves the `Authorization` header.
///
/// # Errors
/// [`BackendError::Unbound`], [`BackendError::Unknown`],
/// [`BackendError::Endpoint`], and [`BackendError::KindMismatch`] for a house
/// bound to Orca, then [`BackendError::CredentialUnavailable`] before
/// anything is contacted; [`BackendError::Http`] when connecting fails, and
/// [`BackendError::Unsupported`] naming every missing and partial capability.
pub fn resolve_http_backend(
    registry: &HouseRegistry,
    house: &HouseConfig,
    session: HttpSession,
    required: &[Capability],
) -> Result<HttpBackend, BackendError> {
    let (binding, kind) = backend_binding(house)?;
    let endpoint = match (kind, &binding.endpoint) {
        (BackendKind::Http, Some(endpoint)) => endpoint.clone(),
        (BackendKind::Http, None) => {
            return Err(BackendError::Endpoint {
                house: house.house.clone(),
                kind,
            });
        }
        (BackendKind::Orca, _) => {
            return Err(BackendError::KindMismatch {
                house: house.house.clone(),
                bound: kind,
                needed: BackendKind::Http,
            });
        }
    };
    let unusable = |status| BackendError::CredentialUnavailable {
        house: house.house.clone(),
        credential: binding.credential.clone(),
        status,
    };
    let file = open_credential(registry, &house.house, &binding.credential)?.map_err(unusable)?;
    // One byte past the limit shows an oversized file, which is refused
    // rather than cut into a different token.
    let mut token = String::new();
    let read = file
        .take(TOKEN_LIMIT.saturating_add(1))
        .read_to_string(&mut token)
        .map_err(|_| HttpError::InvalidConfig)?;
    if u64::try_from(read).map_or(true, |read| read > TOKEN_LIMIT) {
        return Err(HttpError::InvalidConfig.into());
    }
    let backend = HttpBackend::connect(
        HttpConfig {
            endpoint,
            backend: binding.backend.clone(),
            house: house.house.clone(),
            credential: binding.credential.clone(),
            run: session.run,
            coordinator: session.coordinator,
            curl: session.curl,
            call_timeout: session.call_timeout,
        },
        &token,
    )?;
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
