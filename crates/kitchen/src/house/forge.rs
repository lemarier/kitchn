//! A house's private forge binding and the apply hook for approved writes.
//!
//! A [`ForgeBinding`] names the forge, backend namespace, requester identity,
//! credential name, and per-task posting budget a house writes with. It is
//! stored at `private/<house>/forge.json` in the house registry, outside every
//! working tree, and holds no secret. The credential itself is a token file the
//! person places at [`credential_path`]; Kitchen never writes or copies it and
//! reads it only when a write runs, after every other check has passed.
//!
//! [`apply_approved`] is the one entry point for writing an approved preview,
//! such as an issue draft or a decomposition. It refuses a claimant without a
//! person present, a house without a binding, and an approval that does not
//! name the preview's current digest, all before any credential is read.
//! Posting once, resuming after an interruption, and never duplicating a write
//! are the [`ApprovedWrite`] implementation's duty, through the core task store.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{HouseConfig, HouseError};
use crate::{
    BackendId, CredentialId, ErrorClass, HouseId,
    adoption::{FileMode, HouseRegistry, NewFile, RelativePath, decode, encode},
    contracts::{Claimant, ExternalRef, PostingBudget, Trigger},
    integrations::github::{
        CredentialFile, CredentialRef, GitHubExecutor, GitHubMutationTransport, HouseScope,
        IntegrationError, ReadLimits,
    },
};

/// Schema of a stored [`ForgeBinding`].
pub const FORGE_BINDING_SCHEMA: u32 = 1;
/// The binding's file name in the house's private registry directory.
const BINDING_FILE: &str = "forge.json";
/// Directory, in the house's private registry directory, holding token files.
const CREDENTIALS: &str = "credentials";

/// The forge a binding writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeKind {
    /// github.com, through the GitHub CLI.
    #[serde(rename = "github")]
    GitHub,
}

impl ForgeKind {
    /// Whether `requester` is a login this forge can issue: for GitHub, 1 to
    /// 39 letters, digits, and inner single hyphens, optionally ending in
    /// `[bot]` for an app.
    #[must_use]
    pub fn accepts_requester(self, requester: &ExternalRef) -> bool {
        match self {
            Self::GitHub => {
                let login = requester.as_str();
                let name = login.strip_suffix("[bot]").unwrap_or(login);
                (1..=39).contains(&name.len())
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && !name.starts_with('-')
                    && !name.ends_with('-')
                    && !name.contains("--")
            }
        }
    }
}

impl fmt::Display for ForgeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GitHub => "GitHub",
        })
    }
}

/// How a house writes to its forge. Contains no credential value or path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgeBinding {
    /// Schema version; currently [`FORGE_BINDING_SCHEMA`].
    pub schema: u32,
    /// The house this binding belongs to.
    pub house: HouseId,
    /// The forge.
    pub forge: ForgeKind,
    /// Backend namespace; house grants for forge effects must name it.
    pub backend: BackendId,
    /// The forge login the credential must authenticate as.
    pub requester: ExternalRef,
    /// Credential name; house grants for forge effects must name it.
    pub credential: CredentialId,
    /// Most logical writes one task may make.
    pub posting_budget: PostingBudget,
}

impl ForgeBinding {
    /// Check the schema, the requester's login syntax, and that the binding
    /// belongs to `house`.
    ///
    /// # Errors
    /// [`HouseError::InvalidInput`] for another schema or a requester the
    /// forge cannot issue, and [`HouseError::HouseSelection`] for another
    /// house.
    pub fn validate(&self, house: &HouseConfig) -> Result<(), HouseError> {
        if self.schema != FORGE_BINDING_SCHEMA || !self.forge.accepts_requester(&self.requester) {
            return Err(HouseError::InvalidInput);
        }
        if self.house != house.house {
            return Err(HouseError::HouseSelection);
        }
        Ok(())
    }

    /// The credential reference core effects carry.
    #[must_use]
    pub fn credential_ref(&self) -> CredentialRef {
        CredentialRef::new(
            self.house.clone(),
            self.credential.clone(),
            self.requester.clone(),
        )
    }

    /// The integration scope: the house's posting destinations, and the
    /// permissions its policy limits allow on this backend with this
    /// credential. Standing grants still decide what a task may do.
    ///
    /// # Errors
    /// [`ForgeError::NoPostingDestinations`] when the house allows posting
    /// nowhere, and validation failures.
    pub fn scope(&self, house: &HouseConfig) -> Result<HouseScope, ForgeError> {
        self.validate(house)?;
        if house.posting_destinations.is_empty() {
            return Err(ForgeError::NoPostingDestinations {
                house: house.house.clone(),
            });
        }
        let permitted = house
            .policy_limits
            .iter()
            .filter(|limit| {
                limit.destination == self.backend && limit.credential == self.credential
            })
            .map(|limit| limit.permission);
        HouseScope::new(
            house.house.clone(),
            house.posting_destinations.iter().cloned(),
            self.requester.clone(),
            self.credential_ref(),
            self.posting_budget,
            permitted,
        )
        .map_err(ForgeError::Integration)
    }
}

/// What [`bind_forge`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// The binding was stored.
    Created,
    /// The identical binding was already stored.
    Unchanged,
}

/// Whether the credential file is ready to use. Checked without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// A regular file only its owner can access.
    Ready,
    /// No file is there yet.
    Missing,
    /// Something other than a regular file, such as a link or directory.
    NotRegularFile,
    /// Group or others can access it.
    Exposed,
}

/// Forge binding and approved-write failures. Messages name the house and
/// credential, never a credential value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForgeError {
    /// The house has no forge binding. Nothing was written.
    #[error(
        "house {house} has no forge binding, so kitchen cannot write to its forge; bind one with `kitchen forge bind --house {house}` or `kitchen house init`"
    )]
    MissingBinding {
        /// The house.
        house: HouseId,
    },
    /// A different binding is already stored; it was kept.
    #[error(
        "house {house} already has a different forge binding; it was kept (see `kitchen forge show --house {house}`)"
    )]
    BindingConflict {
        /// The house.
        house: HouseId,
    },
    /// The house allows posting nowhere.
    #[error("house {house} has no posting destinations, so nothing may be written to its forge")]
    NoPostingDestinations {
        /// The house.
        house: HouseId,
    },
    /// Writing an approved preview needs a person present.
    #[error("an approved write needs an interactive session with a person present")]
    NeedsPerson,
    /// The approval names another digest than the preview's current one.
    /// Nothing was written.
    #[error(
        "the approval does not name the current preview (digest {current}); review the preview again and approve its digest"
    )]
    StaleApproval {
        /// The current preview's digest.
        current: String,
    },
    /// The credential file is not usable. Nothing was read or written.
    #[error(
        "credential {credential} of house {house} is {status}; `kitchen forge show --house {house}` prints where its token file belongs"
    )]
    CredentialUnavailable {
        /// The house.
        house: HouseId,
        /// The credential name.
        credential: CredentialId,
        /// Why it cannot be used.
        status: CredentialStatus,
    },
    /// Registry storage or validation failed.
    #[error(transparent)]
    House(#[from] HouseError),
    /// The forge integration refused or failed.
    #[error(transparent)]
    Integration(IntegrationError),
}

impl fmt::Display for CredentialStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::NotRegularFile => "not a regular file",
            Self::Exposed => "readable by other users (chmod 600 it)",
        })
    }
}

impl ForgeError {
    /// Common CLI/recovery handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::MissingBinding { .. }
            | Self::NoPostingDestinations { .. }
            | Self::NeedsPerson
            | Self::CredentialUnavailable { .. } => ErrorClass::Refused,
            Self::BindingConflict { .. } | Self::StaleApproval { .. } => ErrorClass::Conflict,
            Self::House(error) => error.class(),
            Self::Integration(error) => error.class(),
        }
    }
}

/// Read the house's forge binding.
///
/// # Errors
/// [`ForgeError::MissingBinding`] when none is stored; a damaged binding, one
/// for another house, and house loading failures otherwise.
pub fn forge_binding(
    registry: &HouseRegistry,
    house: &HouseId,
) -> Result<ForgeBinding, ForgeError> {
    let config = registry.load(house)?;
    let path = registry.private_path(house)?.join(BINDING_FILE);
    let binding: ForgeBinding = match decode(&path) {
        Ok(binding) => binding,
        Err(HouseError::Io(std::io::ErrorKind::NotFound)) => {
            return Err(ForgeError::MissingBinding {
                house: house.clone(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    binding.validate(&config)?;
    Ok(binding)
}

/// Store a forge binding for its house, create-only. An identical binding is
/// left unchanged, so a rerun resumes; a different one is kept and refused.
///
/// # Errors
/// [`ForgeError::BindingConflict`] when a different binding exists; house
/// loading, validation, and storage failures.
pub fn bind_forge(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<BindOutcome, ForgeError> {
    let house = registry.load(&binding.house)?;
    binding.validate(&house)?;
    let conflict = || ForgeError::BindingConflict {
        house: binding.house.clone(),
    };
    match forge_binding(registry, &binding.house) {
        Ok(existing) if existing == *binding => return Ok(BindOutcome::Unchanged),
        Ok(_) => return Err(conflict()),
        Err(ForgeError::MissingBinding { .. }) => {}
        // A damaged file is kept, never overwritten.
        Err(ForgeError::House(HouseError::InvalidInput)) => return Err(conflict()),
        Err(error) => return Err(error),
    }
    let path = RelativePath::new(&format!("private/{}/{BINDING_FILE}", binding.house))?;
    let contents = encode(binding)?;
    match crate::adoption::install_private_files(
        registry.root(),
        &[NewFile {
            path: &path,
            contents: &contents,
            mode: FileMode::Regular,
        }],
    ) {
        Ok(_) => Ok(BindOutcome::Created),
        // Another writer stored a binding first.
        Err(HouseError::Conflicts(_) | HouseError::Conflict) => {
            match forge_binding(registry, &binding.house)? {
                existing if existing == *binding => Ok(BindOutcome::Unchanged),
                _ => Err(conflict()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Where the binding's token file belongs: in the house's private registry
/// directory, never in a working tree or the binding itself.
///
/// # Errors
/// House loading failures and a private directory inside a repository.
pub fn credential_path(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<PathBuf, ForgeError> {
    Ok(registry
        .private_path(&binding.house)?
        .join(CREDENTIALS)
        .join(binding.credential.as_str()))
}

/// Inspect the token file at `path` without reading it or following a link.
///
/// # Errors
/// Filesystem failures other than absence.
pub fn credential_status(path: &std::path::Path) -> Result<CredentialStatus, HouseError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CredentialStatus::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Ok(CredentialStatus::NotRegularFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Ok(CredentialStatus::Exposed);
        }
    }
    Ok(CredentialStatus::Ready)
}

/// A previewed forge write that a person approves by its digest, such as an
/// issue draft or a decomposition.
pub trait ApprovedWrite {
    /// The preview digest an approval names.
    type Digest: PartialEq + fmt::Display;
    /// What the write reports.
    type Report;

    /// Recompute the preview and return its digest. Reads nothing remote.
    ///
    /// # Errors
    /// An invalid or unreadable draft.
    fn digest(&self) -> crate::Result<Self::Digest>;

    /// Write the approved preview through `forge`, or complete an earlier
    /// partial write of it. Implementations persist intent in the core task
    /// store before each write, so a rerun reuses applied writes and
    /// reconciles uncertain ones instead of posting again.
    ///
    /// # Errors
    /// Store, contract, and executor failures, which leave interrupted work
    /// for the next run.
    fn apply<T: GitHubMutationTransport>(
        &self,
        forge: &GitHubExecutor<T>,
        approved: &Self::Digest,
        claimant: &Claimant,
    ) -> crate::Result<Self::Report>;
}

/// Write an approved preview with the house's forge binding.
///
/// Refuses, before reading any credential: a claimant without a person
/// present, a house without a forge binding, an approval that does not name
/// the preview's current digest, a house with no posting destinations, and a
/// missing or exposed token file. `connect` then builds the transport over
/// the house's credential file, such as [`GhCli::new`] with the GitHub CLI.
///
/// # Errors
/// The refusals above as [`ForgeError`]s, `connect` failures, and the
/// write's own errors.
///
/// [`GhCli::new`]: crate::integrations::github::GhCli::new
pub fn apply_approved<W, T>(
    registry: &HouseRegistry,
    house: &HouseId,
    write: &W,
    approved: &W::Digest,
    claimant: &Claimant,
    connect: impl FnOnce(CredentialFile) -> Result<T, IntegrationError>,
) -> crate::Result<W::Report>
where
    W: ApprovedWrite,
    T: GitHubMutationTransport,
{
    match claimant.trigger {
        Trigger::Interactive => {}
        Trigger::Scheduled | Trigger::Event(_) => return Err(ForgeError::NeedsPerson.into()),
    }
    let config = registry.load(house)?;
    let binding = forge_binding(registry, house)?;
    let current = write.digest()?;
    if current != *approved {
        return Err(ForgeError::StaleApproval {
            current: current.to_string(),
        }
        .into());
    }
    let scope = binding.scope(&config)?;
    let path = credential_path(registry, &binding)?;
    match credential_status(&path)? {
        CredentialStatus::Ready => {}
        status @ (CredentialStatus::Missing
        | CredentialStatus::NotRegularFile
        | CredentialStatus::Exposed) => {
            return Err(ForgeError::CredentialUnavailable {
                house: house.clone(),
                credential: binding.credential,
                status,
            }
            .into());
        }
    }
    let file =
        CredentialFile::new(binding.credential_ref(), path).map_err(ForgeError::Integration)?;
    let transport = connect(file).map_err(ForgeError::Integration)?;
    let executor = GitHubExecutor::new(binding.backend, scope, transport, ReadLimits::default());
    write.apply(&executor, approved, claimant)
}
