//! A house's private forge binding and the apply hook for approved writes.
//!
//! A [`ForgeBinding`] names the forge, backend namespace, requester identity,
//! credential name, and per-task posting budget a house writes with. It is
//! stored at `private/<house>/forge.json` in the house registry, outside every
//! working tree, and holds no secret. The credential itself is a token file the
//! person places at [`credential_path`]; Kitchen never writes or copies it and
//! reads it only when a write runs, after every other check has passed. It is
//! opened from the registry root one name at a time without following links,
//! checked on the opened descriptor, and read from that same descriptor.
//!
//! [`apply_approved`] is the one entry point for writing an approved preview,
//! such as an issue draft or a decomposition. It refuses a claimant without a
//! person present, a house without a binding, and an approval that does not
//! name the preview's current digest, all before any credential is read.
//! Posting once, resuming after an interruption, and never duplicating a write
//! are the [`ApprovedWrite`] implementation's duty, through the core task store.

use std::{fmt, fs::File, path::PathBuf};

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
    Ok(match open_credential(registry, binding)? {
        Ok(_) => CredentialStatus::Ready,
        Err(status) => status,
    })
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
) -> Result<Option<rustix::fd::OwnedFd>, ForgeError> {
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
        Err(error) => return Err(HouseError::from(std::io::Error::from(error)).into()),
    };
    let opened = File::from(descriptor);
    let actual = opened.metadata().map_err(HouseError::from)?;
    if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
        return Ok(None);
    }
    Ok(Some(opened.into()))
}

/// Open the token file at [`credential_path`] from the registry root, one
/// name at a time, refusing a link at every step below the canonical root.
/// Path components above the canonical registry root are trusted system or
/// user configuration. Ownership, type, and mode
/// are checked on the opened descriptor, which the caller reads the token
/// from, so replacing the file after the check cannot change what is read.
#[cfg(unix)]
fn open_credential(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<Result<File, CredentialStatus>, ForgeError> {
    use rustix::{
        fs::{Mode, OFlags, openat},
        io::Errno,
    };
    use std::os::unix::fs::MetadataExt;

    fn failed(error: Errno) -> ForgeError {
        HouseError::from(std::io::Error::from(error)).into()
    }

    // Loads the house and refuses a private directory inside a repository.
    registry.private_path(&binding.house)?;
    let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let Some(mut parent) = open_registry_root(registry.root(), || {})? else {
        return Ok(Err(CredentialStatus::Redirected));
    };
    for name in ["private", binding.house.as_str(), CREDENTIALS] {
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
    let file = match openat(&parent, binding.credential.as_str(), token, Mode::empty()) {
        Ok(descriptor) => File::from(descriptor),
        Err(Errno::NOENT) => return Ok(Err(CredentialStatus::Missing)),
        Err(Errno::LOOP) => return Ok(Err(CredentialStatus::NotRegularFile)),
        Err(error) => return Err(failed(error)),
    };
    let metadata = file.metadata().map_err(HouseError::from)?;
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
fn open_credential(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<Result<File, CredentialStatus>, ForgeError> {
    registry.private_path(&binding.house)?;
    Err(HouseError::Io(std::io::ErrorKind::Unsupported).into())
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
/// missing, redirected, foreign, or exposed token file. `connect` then builds
/// the transport, such as [`GhCli::new`] with the GitHub CLI, over the token
/// file already opened and checked.
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
    let file = match open_credential(registry, &binding)? {
        Ok(file) => file,
        Err(status) => {
            return Err(ForgeError::CredentialUnavailable {
                house: house.clone(),
                credential: binding.credential,
                status,
            }
            .into());
        }
    };
    let credential = CredentialFile::opened(binding.credential_ref(), file);
    let transport = connect(credential).map_err(ForgeError::Integration)?;
    let executor = GitHubExecutor::new(binding.backend, scope, transport, ReadLimits::default());
    write.apply(&executor, approved, claimant)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        HolderId,
        contracts::{CommitId, EffectFailure, NotAppliedReason, Repository},
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

    struct Approved;
    impl ApprovedWrite for Approved {
        type Digest = &'static str;
        type Report = ();
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
            &Approved,
            &"sha256:aaaa",
            &claimant,
            |file| {
                replace().map_err(|_| IntegrationError::Unavailable)?;
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
