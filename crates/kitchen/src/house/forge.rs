//! A house's private forge binding and the apply hook for approved writes.
//!
//! A [`ForgeBinding`] names the forge, backend namespace, requester identity,
//! credential name, and per-task posting budget a house writes with. It is
//! stored at `private/<house>/forge.json` in the house registry, outside every
//! working tree, and holds no secret. The credential itself is a file the
//! person places at [`credential_path`]; Kitchen never writes or copies it and
//! reads it only when a write runs, after every other check has passed. It is
//! opened from the registry root one name at a time without following links,
//! checked on the opened descriptor, and read from that same descriptor.
//!
//! The [`CredentialKind`] says what that file holds: a person's token, or the
//! PEM private key of a GitHub App whose ID and installation the binding
//! names. An app writes as `<app-slug>[bot]` with installation tokens minted
//! per effect for its repository and permissions; see
//! [`AppTokens`](crate::integrations::github::AppTokens). Before an approved
//! write, [`apply_approved`] refuses an app that is not installed on the
//! write's repository, naming it.
//!
//! [`apply_approved`] is the one entry point for writing an approved preview,
//! such as an issue draft or a decomposition. It refuses a claimant without a
//! person present, a house without a binding, and an approval that does not
//! name the preview's current digest, all before any credential is read.
//! Posting once, resuming after an interruption, and never duplicating a write
//! are the [`ApprovedWrite`] implementation's duty, through the core task store.
//! [`forge_reader`] builds the same house-scoped executor for re-reading the
//! forge, such as when a person acknowledges a settled write; it submits
//! nothing.

use std::{fmt, fs::File, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{HouseConfig, HouseError};
use crate::{
    BackendId, CredentialId, ErrorClass, HouseId,
    adoption::{FileMode, HouseRegistry, NewFile, RelativePath, decode, encode},
    contracts::Repository,
    contracts::{
        BackendDescriptor, BackendUnavailable, Claimant, EffectExecutor, EffectFailure,
        EffectRequest, ExternalRef, Lookup, NotAppliedReason, PostingBudget, Receipt, Trigger,
    },
    integrations::github::{
        CredentialFile, CredentialRef, GitHubApp, GitHubExecutor, GitHubMutationTransport,
        HouseScope, Installed, IntegrationError, ReadLimits,
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
    /// 39 letters, digits, and inner single hyphens or underscores (an
    /// Enterprise Managed User login is `handle_shortcode`), optionally
    /// ending in `[bot]` for an app.
    #[must_use]
    pub fn accepts_requester(self, requester: &ExternalRef) -> bool {
        match self {
            Self::GitHub => {
                let login = requester.as_str();
                let name = login.strip_suffix("[bot]").unwrap_or(login);
                (1..=39).contains(&name.len())
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                    && !name.starts_with(['-', '_'])
                    && !name.ends_with(['-', '_'])
                    && !["--", "__", "-_", "_-"]
                        .iter()
                        .any(|pair| name.contains(pair))
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

/// What the binding's credential file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum CredentialKind {
    /// A token for the requester, checked through `/user` before each call.
    #[default]
    Token,
    /// The PEM private key of this GitHub App; the requester is the app's
    /// `<app-slug>[bot]` login.
    GitHubApp(GitHubApp),
}

impl CredentialKind {
    /// Whether this is the default token kind, which a stored binding omits.
    #[must_use]
    pub const fn is_token(&self) -> bool {
        matches!(self, Self::Token)
    }
}

/// The checked credential file handed to `connect` in [`apply_approved`] and
/// [`forge_reader`], with what it holds.
#[derive(Debug)]
pub enum ForgeCredential {
    /// A token file.
    Token(CredentialFile),
    /// A GitHub App's private key file.
    App {
        /// The bound app and installation.
        app: GitHubApp,
        /// The private key file.
        key: CredentialFile,
    },
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
    /// What the credential file holds; a token when absent.
    #[serde(default, skip_serializing_if = "CredentialKind::is_token")]
    pub credential_kind: CredentialKind,
    /// GitHub's numeric bot user ID for an app requester. GitHub uses this
    /// ID in the commit email; token bindings do not have a bot user ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_user_id: Option<u64>,
    /// Most logical writes one task may make.
    pub posting_budget: PostingBudget,
}

impl ForgeBinding {
    /// Check the schema, the requester's login syntax, that an app's
    /// requester is a `[bot]` login, and that the binding belongs to `house`.
    ///
    /// # Errors
    /// [`HouseError::InvalidInput`] for another schema, a requester the
    /// forge cannot issue, or an app requester without `[bot]`, and
    /// [`HouseError::HouseSelection`] for another house.
    pub fn validate(&self, house: &HouseConfig) -> Result<(), HouseError> {
        let app_login = match self.credential_kind {
            CredentialKind::Token => self.bot_user_id.is_none(),
            CredentialKind::GitHubApp(_) => {
                self.requester.as_str().ends_with("[bot]")
                    && self.bot_user_id.is_none_or(|id| id > 0)
            }
        };
        if self.schema != FORGE_BINDING_SCHEMA
            || !self.forge.accepts_requester(&self.requester)
            || !app_login
        {
            return Err(HouseError::InvalidInput);
        }
        if self.house != house.house {
            return Err(HouseError::HouseSelection);
        }
        Ok(())
    }

    /// The exact author and committer a worker must use for this binding.
    /// App bindings use GitHub's documented bot noreply address.
    #[must_use]
    pub fn writer_identity(&self) -> Option<(String, String)> {
        let id = self.bot_user_id?;
        let login = self.requester.as_str();
        Some((
            login.to_owned(),
            format!("{id}+{login}@users.noreply.github.com"),
        ))
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
    /// A legacy app binding gained its verified bot user ID.
    Updated,
}

/// Whether the credential file is ready to use. Checked without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// A regular file the current user owns and only they can access.
    Ready,
    /// No file is there yet.
    Missing,
    /// Something other than a regular file, such as a link or directory.
    NotRegularFile,
    /// A directory on its path, such as `credentials` or the house's private
    /// directory, is a link or not a directory.
    Redirected,
    /// Another user owns it.
    NotOwned,
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
        "house {house} has no forge binding, so kitchn cannot write to its forge; bind one with `kitchn forge bind --house {house}` or `kitchn house init`"
    )]
    MissingBinding {
        /// The house.
        house: HouseId,
    },
    /// A different binding is already stored; it was kept.
    #[error(
        "house {house} already has a different forge binding; it was kept (see `kitchn forge show --house {house}`)"
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
        "credential {credential} of house {house} is {status}; `kitchn forge show --house {house}` prints where its token file belongs"
    )]
    CredentialUnavailable {
        /// The house.
        house: HouseId,
        /// The credential name.
        credential: CredentialId,
        /// Why it cannot be used.
        status: CredentialStatus,
    },
    /// The GitHub CLI, which Kitchen writes and reads through, is not
    /// installed. Nothing was read or written.
    #[error("the GitHub CLI (`gh`) was not found on PATH; install it to write to GitHub")]
    GhNotFound,
    /// `curl`, which mints GitHub App installation tokens, is not installed.
    /// Nothing was read or written.
    #[error("`curl` was not found on PATH; install it to write to GitHub as an app")]
    CurlNotFound,
    /// The bound GitHub App is not installed on the write's repository
    /// through the bound installation. Nothing was written.
    #[error(
        "the GitHub App of house {house} is not installed on {repository} through its bound installation; install the app there, or bind the installation that covers it"
    )]
    AppNotInstalled {
        /// The house.
        house: HouseId,
        /// The repository the write targets.
        repository: Repository,
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
            Self::Redirected => "behind a link or non-directory on its path",
            Self::NotOwned => "owned by another user",
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
            | Self::CredentialUnavailable { .. }
            | Self::AppNotInstalled { .. } => ErrorClass::Refused,
            Self::GhNotFound | Self::CurlNotFound => ErrorClass::Execution,
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
        Ok(existing)
            if existing.bot_user_id.is_none()
                && binding.bot_user_id.is_some()
                && (ForgeBinding {
                    bot_user_id: None,
                    ..binding.clone()
                }) == existing =>
        {
            registry.update_private_document(&binding.house, BINDING_FILE, &existing, binding)?;
            return Ok(BindOutcome::Updated);
        }
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

/// Inspect the binding's token file without reading it, following a link,
/// or leaving the house's private directory.
///
/// # Errors
/// House loading failures, a private directory inside a repository, and
/// filesystem failures other than absence or redirection.
pub fn credential_status(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<CredentialStatus, ForgeError> {
    Ok(
        match open_credential(registry, &binding.house, &binding.credential)? {
            Ok(_) => CredentialStatus::Ready,
            Err(status) => status,
        },
    )
}

/// Open the registry root as a directory descriptor, or `None` when the path
/// no longer names the directory it resolved to.
///
/// The root is canonicalized once, so links above it, such as `/var` on macOS
/// or a symlinked `~/.local/share`, resolve as system or user configuration
/// that Kitchen trusts. The canonical path is then opened without following a
/// final link, and the descriptor's (device, inode) must equal the canonical
/// path's, so a root swapped for a link or another directory in between is
/// refused. `after_resolve` runs between the two steps for race tests.
#[cfg(unix)]
fn open_registry_root(
    root: &std::path::Path,
    after_resolve: impl FnOnce(),
) -> Result<Option<rustix::fd::OwnedFd>, HouseError> {
    use rustix::{
        fs::{Mode, OFlags, open},
        io::Errno,
    };
    use std::os::unix::fs::MetadataExt;

    let canonical = root.canonicalize().map_err(HouseError::from)?;
    let expected = std::fs::metadata(&canonical).map_err(HouseError::from)?;
    after_resolve();
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let descriptor = match open(&canonical, flags, Mode::empty()) {
        Ok(descriptor) => descriptor,
        Err(Errno::LOOP | Errno::NOTDIR | Errno::NOENT) => return Ok(None),
        Err(error) => return Err(HouseError::from(std::io::Error::from(error))),
    };
    let opened = File::from(descriptor);
    let actual = opened.metadata().map_err(HouseError::from)?;
    if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
        return Ok(None);
    }
    Ok(Some(opened.into()))
}

/// Open the credential file `private/<house>/credentials/<credential>`, the
/// forge binding's [`credential_path`] or a worker backend's token, from the
/// registry root, one name at a time, refusing a link at every step below
/// the canonical root.
/// Path components above the canonical registry root are trusted system or
/// user configuration. Ownership, type, and mode
/// are checked on the opened descriptor, which the caller reads the token
/// from, so replacing the file after the check cannot change what is read.
#[cfg(unix)]
pub(crate) fn open_credential(
    registry: &HouseRegistry,
    house: &HouseId,
    credential: &CredentialId,
) -> Result<Result<File, CredentialStatus>, HouseError> {
    use rustix::{
        fs::{Mode, OFlags, openat},
        io::Errno,
    };
    use std::os::unix::fs::MetadataExt;

    fn failed(error: Errno) -> HouseError {
        HouseError::from(std::io::Error::from(error))
    }

    // Loads the house and refuses a private directory inside a repository.
    registry.private_path(house)?;
    let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let Some(mut parent) = open_registry_root(registry.root(), || {})? else {
        return Ok(Err(CredentialStatus::Redirected));
    };
    for name in ["private", house.as_str(), CREDENTIALS] {
        parent = match openat(&parent, name, directory | OFlags::NOFOLLOW, Mode::empty()) {
            Ok(child) => child,
            Err(Errno::NOENT) => return Ok(Err(CredentialStatus::Missing)),
            Err(Errno::LOOP | Errno::NOTDIR) => return Ok(Err(CredentialStatus::Redirected)),
            Err(error) => return Err(failed(error)),
        };
    }
    // Non-blocking, so a FIFO placed there cannot stall the open.
    let token =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let file = match openat(&parent, credential.as_str(), token, Mode::empty()) {
        Ok(descriptor) => File::from(descriptor),
        Err(Errno::NOENT) => return Ok(Err(CredentialStatus::Missing)),
        Err(Errno::LOOP) => return Ok(Err(CredentialStatus::NotRegularFile)),
        Err(error) => return Err(failed(error)),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(Err(CredentialStatus::NotRegularFile));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Ok(Err(CredentialStatus::NotOwned));
    }
    if metadata.mode() & 0o077 != 0 {
        return Ok(Err(CredentialStatus::Exposed));
    }
    Ok(Ok(file))
}

/// Without no-follow opens and owner checks, no token file is trusted.
#[cfg(not(unix))]
pub(crate) fn open_credential(
    registry: &HouseRegistry,
    house: &HouseId,
    _credential: &CredentialId,
) -> Result<Result<File, CredentialStatus>, HouseError> {
    registry.private_path(house)?;
    Err(HouseError::Io(std::io::ErrorKind::Unsupported))
}

/// A previewed forge write that a person approves by its digest, such as an
/// issue draft or a decomposition.
pub trait ApprovedWrite {
    /// The preview digest an approval names.
    type Digest: PartialEq + fmt::Display;
    /// What the write reports.
    type Report;

    /// The repository the write targets.
    fn repository(&self) -> &Repository;

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
/// missing, redirected, foreign, or exposed credential file. `connect` then
/// builds the transport, such as [`GhCli::new`] or [`GhCli::app`] with the
/// GitHub CLI, over the credential file already opened and checked. Before
/// writing, a GitHub App that is not installed on the write's repository is
/// refused as [`ForgeError::AppNotInstalled`].
///
/// # Errors
/// The refusals above as [`ForgeError`]s, `connect` failures, and the
/// write's own errors.
///
/// [`GhCli::new`]: crate::integrations::github::GhCli::new
/// [`GhCli::app`]: crate::integrations::github::GhCli::app
pub fn apply_approved<W, T>(
    registry: &HouseRegistry,
    house: &HouseId,
    write: &W,
    approved: &W::Digest,
    claimant: &Claimant,
    connect: impl FnOnce(ForgeCredential) -> Result<T, ForgeError>,
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
    let credential = binding.credential_ref();
    let executor = executor(registry, &config, binding, connect)?;
    let repository = write.repository();
    let installed = executor
        .transport()
        .installed(&credential, repository, ReadLimits::default().timeout())
        .map_err(ForgeError::Integration)?;
    match installed {
        Installed::Yes => write.apply(&executor, approved, claimant),
        Installed::No => Err(ForgeError::AppNotInstalled {
            house: house.clone(),
            repository: repository.clone(),
        }
        .into()),
    }
}

/// The house-scoped executor over its checked credential file.
fn executor<T: GitHubMutationTransport>(
    registry: &HouseRegistry,
    config: &HouseConfig,
    binding: ForgeBinding,
    connect: impl FnOnce(ForgeCredential) -> Result<T, ForgeError>,
) -> Result<GitHubExecutor<T>, ForgeError> {
    let scope = binding.scope(config)?;
    let credential = checked_forge_credential(registry, &binding)?;
    let transport = connect(credential)?;
    Ok(GitHubExecutor::new(
        binding.backend,
        scope,
        transport,
        ReadLimits::default(),
    ))
}

/// Open the binding's credential with the house directory and file checks.
///
/// # Errors
/// Refuses missing, redirected, or unsafe house credentials.
pub fn checked_forge_credential(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<ForgeCredential, ForgeError> {
    let file = match open_credential(registry, &binding.house, &binding.credential)? {
        Ok(file) => file,
        Err(status) => {
            return Err(ForgeError::CredentialUnavailable {
                house: binding.house.clone(),
                credential: binding.credential.clone(),
                status,
            });
        }
    };
    let file = CredentialFile::opened(binding.credential_ref(), file);
    Ok(match binding.credential_kind {
        CredentialKind::Token => ForgeCredential::Token(file),
        CredentialKind::GitHubApp(app) => ForgeCredential::App { app, key: file },
    })
}

/// The house's forge, for looking up what earlier writes did. Every
/// submission is refused before it reaches the forge; writes go through
/// [`apply_approved`].
pub struct ForgeReader<T>(GitHubExecutor<T>);

impl<T: GitHubMutationTransport> EffectExecutor for ForgeReader<T> {
    fn descriptor(&self) -> &BackendDescriptor {
        self.0.descriptor()
    }

    fn execute(&self, _: &EffectRequest) -> Result<Receipt, EffectFailure> {
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.0.lookup(request)
    }
}

/// Build a [`ForgeReader`] with the house's forge binding, for re-reading
/// the outcome of earlier writes.
///
/// Refuses, before reading any credential, a house without a forge binding
/// or posting destinations, and a missing, redirected, foreign, or exposed
/// token file. `connect` then builds the transport over the checked token
/// file, as for [`apply_approved`].
///
/// # Errors
/// The refusals above and `connect` failures.
pub fn forge_reader<T: GitHubMutationTransport>(
    registry: &HouseRegistry,
    house: &HouseId,
    connect: impl FnOnce(ForgeCredential) -> Result<T, ForgeError>,
) -> Result<ForgeReader<T>, ForgeError> {
    let config = registry.load(house)?;
    let binding = forge_binding(registry, house)?;
    executor(registry, &config, binding, connect).map(ForgeReader)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        HolderId,
        contracts::{CommitId, EffectFailure, NotAppliedReason},
        integrations::github::{GitHubReadTransport, MutationRequest, ReadRequest},
    };
    use std::{cell::RefCell, collections::BTreeSet, fs, os::unix::fs::PermissionsExt, path::Path};

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    const CHECKED: &str = "checked-fixture-token";

    struct Offline;
    impl GitHubReadTransport for Offline {
        fn read(
            &self,
            _: &CredentialRef,
            _: &ReadRequest,
            _: std::time::Duration,
            _: usize,
        ) -> Result<Vec<u8>, IntegrationError> {
            Err(IntegrationError::Unavailable)
        }
    }
    impl GitHubMutationTransport for Offline {
        fn submit(
            &self,
            _: &CredentialRef,
            _: &MutationRequest,
            _: std::time::Duration,
            _: usize,
        ) -> Result<Vec<u8>, EffectFailure> {
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        }
    }

    struct Approved(Repository);
    impl ApprovedWrite for Approved {
        type Digest = &'static str;
        type Report = ();
        fn repository(&self) -> &Repository {
            &self.0
        }
        fn digest(&self) -> crate::Result<&'static str> {
            Ok("sha256:aaaa")
        }
        fn apply<T: GitHubMutationTransport>(
            &self,
            _: &GitHubExecutor<T>,
            _: &&'static str,
            _: &Claimant,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    fn write_token(path: &Path, token: &str, mode: u32) -> TestResult {
        fs::write(path, token)?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    /// A registry with house `acme` bound to credential `github` and a
    /// ready token file.
    fn bound(
        root: &Path,
    ) -> Result<(HouseRegistry, ForgeBinding, PathBuf), Box<dyn std::error::Error>> {
        let app = Repository::new("acme/app")?;
        let kitchen = CommitId::new("4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c")?;
        let config = HouseConfig {
            schema: 1,
            house: HouseId::new("acme")?,
            kitchen: kitchen.clone(),
            guidance: kitchen,
            repositories: [app.clone()].into(),
            posting_destinations: [app].into(),
            required_reviewers: BTreeSet::new(),
            required_checks: BTreeSet::new(),
            policy_limits: BTreeSet::new(),
            grants: BTreeSet::new(),
            agents: None,
            stack_tool: None,
            schedules: None,
            merge_readiness: Default::default(),
            disk_pressure: None,
            follow_up: None,
            backend: None,
            graduation: Default::default(),
            tick: None,
        };
        let registry = HouseRegistry::new(root.join("registry"))?;
        registry.initialize(&config)?;
        let binding = ForgeBinding {
            schema: FORGE_BINDING_SCHEMA,
            house: config.house,
            forge: ForgeKind::GitHub,
            backend: BackendId::new("github")?,
            requester: ExternalRef::new("acme-bot")?,
            credential: CredentialId::new("github")?,
            credential_kind: CredentialKind::Token,
            bot_user_id: None,
            posting_budget: PostingBudget::new(5)?,
        };
        bind_forge(&registry, &binding)?;
        let path = credential_path(&registry, &binding)?;
        fs::create_dir_all(path.parent().ok_or("no parent")?)?;
        write_token(&path, CHECKED, 0o600)?;
        Ok((registry, binding, path))
    }

    /// Run the hook; `replace` runs in `connect`, after the checks and
    /// before any token is read. Returns the token the transport would load.
    fn apply_then_load(
        registry: &HouseRegistry,
        binding: &ForgeBinding,
        replace: impl FnOnce() -> TestResult,
    ) -> Result<Result<String, IntegrationError>, Box<dyn std::error::Error>> {
        let handed = RefCell::new(None);
        let claimant = Claimant {
            holder: HolderId::new("session-1")?,
            trigger: Trigger::Interactive,
            consumer: None,
        };
        apply_approved(
            registry,
            &binding.house,
            &Approved(Repository::new("acme/app").map_err(crate::Error::from)?),
            &"sha256:aaaa",
            &claimant,
            |credential| {
                replace().map_err(|_| ForgeError::Integration(IntegrationError::Unavailable))?;
                let ForgeCredential::Token(file) = credential else {
                    return Err(ForgeError::Integration(IntegrationError::ScopeMismatch));
                };
                handed.replace(Some(file));
                Ok(Offline)
            },
        )?;
        let file = handed.take().ok_or("connect was not reached")?;
        Ok(file.load(&binding.credential_ref()))
    }

    #[test]
    fn a_token_replaced_after_the_check_is_never_read() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (registry, binding, path) = bound(&root)?;

        // An exposed file renamed over the checked one.
        let loaded = apply_then_load(&registry, &binding, || {
            let exposed = root.join("exposed");
            write_token(&exposed, "exposed-fixture-token", 0o644)?;
            fs::rename(&exposed, &path)?;
            Ok(())
        })?;
        assert_eq!(loaded, Ok(CHECKED.to_owned()));
        assert_eq!(
            credential_status(&registry, &binding)?,
            CredentialStatus::Exposed
        );

        // A link to another house's token put in its place.
        write_token(&path, CHECKED, 0o600)?;
        let loaded = apply_then_load(&registry, &binding, || {
            let other = root.join("registry/private/other/credentials/github");
            fs::create_dir_all(other.parent().ok_or("no parent")?)?;
            write_token(&other, "other-house-fixture-token", 0o600)?;
            fs::remove_file(&path)?;
            std::os::unix::fs::symlink(&other, &path)?;
            Ok(())
        })?;
        assert_eq!(loaded, Ok(CHECKED.to_owned()));
        Ok(())
    }

    #[test]
    fn a_checked_token_emptied_in_place_is_refused() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (registry, binding, path) = bound(&root)?;
        // Truncating the same file leaves nothing to read, so loading fails
        // rather than falling back to another path.
        let loaded = apply_then_load(&registry, &binding, || {
            fs::write(&path, "")?;
            Ok(())
        })?;
        assert_eq!(loaded, Err(IntegrationError::InvalidInput));
        Ok(())
    }

    #[test]
    fn an_oversized_checked_token_is_refused_without_its_contents() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (registry, binding, path) = bound(&root)?;
        write_token(&path, &"t".repeat(16 * 1024 + 1), 0o600)?;
        let loaded = apply_then_load(&registry, &binding, || Ok(()))?;
        assert_eq!(loaded, Err(IntegrationError::LimitExceeded));
        // The largest accepted token still loads.
        write_token(&path, &"t".repeat(16 * 1024), 0o600)?;
        let loaded = apply_then_load(&registry, &binding, || Ok(()))?;
        assert_eq!(loaded.map(|token| token.len()), Ok(16 * 1024));
        Ok(())
    }

    #[test]
    fn a_registry_root_swapped_for_a_link_is_refused() -> TestResult {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let (registry, binding, _) = bound(&base)?;
        let root = base.join("registry");

        // Another registry holding a token of the same name.
        let (_, _, other_token) = bound(&base.join("elsewhere"))?;
        let other_root = base.join("elsewhere/registry");
        write_token(&other_token, "other-registry-fixture-token", 0o600)?;

        let refused = open_registry_root(&root, || {
            fs::rename(&root, base.join("moved")).ok();
            std::os::unix::fs::symlink(&other_root, &root).ok();
        })?;
        assert!(refused.is_none());
        // With the link in place the registry's own path check refuses too.
        assert!(matches!(
            credential_status(&registry, &binding),
            Err(ForgeError::House(HouseError::RedirectedPath))
        ));
        Ok(())
    }

    #[test]
    fn a_registry_root_replaced_by_another_directory_is_refused() -> TestResult {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let (_registry, _binding, _) = bound(&base)?;
        let root = base.join("registry");
        let refused = open_registry_root(&root, || {
            fs::rename(&root, base.join("moved")).ok();
            fs::create_dir(&root).ok();
        })?;
        assert!(refused.is_none());
        Ok(())
    }

    #[test]
    fn links_above_the_registry_root_still_resolve() -> TestResult {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        bound(&base)?;
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&base, &alias)?;
        assert!(open_registry_root(&alias.join("registry"), || {})?.is_some());
        assert!(open_registry_root(&base.join("absent"), || {}).is_err());
        Ok(())
    }
}
