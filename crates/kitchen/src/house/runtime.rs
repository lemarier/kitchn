//! A house's private runtime configuration: the host facts its worker
//! backend needs on every tick.
//!
//! A [`RuntimeConfig`] names where Orca runs on this host (executable,
//! runtime directory, Run, coordinator, repository selector), where `curl`
//! is for an HTTP backend, and which repository a multi-repository house's
//! passes serve. It is stored at `private/<house>/runtime.json` in the house
//! registry, outside every working tree, so the printed trigger stays
//! `kitchn tick --registry <registry> --house <house>` and the tick reads the
//! rest. It holds paths and identifiers only: unknown fields are refused, so
//! a token cannot be added to it, and credentials stay in their own files.
//!
//! Reading validates the file: a regular file the owner alone can access, of
//! bounded size, for this house, with absolute paths. Anything else is a
//! [`RuntimeError`] and no backend is contacted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{HouseConfig, HouseError};
use crate::{
    ErrorClass, HouseId,
    adoption::{FileMode, HouseRegistry, NewFile, RelativePath, decode, encode},
    contracts::{ExternalRef, Repository},
};

/// Schema of a stored [`RuntimeConfig`].
pub const RUNTIME_SCHEMA: u32 = 1;
/// The file's name in the house's private registry directory.
const RUNTIME_FILE: &str = "runtime.json";

/// Where Orca runs on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrcaHost {
    /// Absolute path of the Orca executable.
    pub executable: PathBuf,
    /// House-scoped Orca runtime storage shared by every caller.
    pub runtime_dir: PathBuf,
    /// The Orca Run that owns the house's workers and mailbox.
    pub run: ExternalRef,
    /// The coordinator terminal handle calls are attributed to.
    pub coordinator: ExternalRef,
    /// The Orca repository selector for worker workspaces.
    pub repo: ExternalRef,
}

/// Host facts for one house. Contains no credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Schema version; currently [`RUNTIME_SCHEMA`].
    pub schema: u32,
    /// The house these facts belong to.
    pub house: HouseId,
    /// Orca on this host, for a house bound to Orca.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca: Option<OrcaHost>,
    /// Absolute path of `curl`, for a house bound to an HTTP backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curl: Option<PathBuf>,
    /// The repository a multi-repository house's passes serve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
}

/// A runtime configuration was refused. Paths and values are never echoed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// The file is damaged, for another house or schema, or names a relative
    /// path or a repository outside the house.
    #[error(
        "the house's runtime configuration is invalid; store it again with `kitchn tick trigger`"
    )]
    Invalid,
    /// Other users can access the file, or it is not a regular file.
    #[error("the house's runtime configuration must be a regular file only its owner can access")]
    NotPrivate,
    /// House loading and storage failures.
    #[error(transparent)]
    House(#[from] HouseError),
}

impl RuntimeError {
    /// The broad handling class of this error.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Invalid => ErrorClass::InvalidInput,
            Self::NotPrivate => ErrorClass::Refused,
            Self::House(error) => error.class(),
        }
    }
}

impl RuntimeConfig {
    /// Check the schema, the house, that every path is absolute, and that the
    /// repository is one of the house's.
    ///
    /// # Errors
    /// [`RuntimeError::Invalid`] for any of those failing.
    pub fn validate(&self, house: &HouseConfig) -> Result<(), RuntimeError> {
        let absolute = self
            .orca
            .iter()
            .flat_map(|orca| [&orca.executable, &orca.runtime_dir])
            .chain(&self.curl)
            .all(|path| path.is_absolute());
        let repository = self
            .repository
            .as_ref()
            .is_none_or(|repository| house.repositories.contains(repository));
        if self.schema == RUNTIME_SCHEMA && self.house == house.house && absolute && repository {
            Ok(())
        } else {
            Err(RuntimeError::Invalid)
        }
    }
}

/// Whether [`store_runtime`] wrote the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeOutcome {
    /// No configuration existed; it was created.
    Created,
    /// A different valid configuration was replaced.
    Replaced,
    /// The stored configuration was identical.
    Unchanged,
}

/// Read the house's runtime configuration; `None` when none is stored.
///
/// # Errors
/// [`RuntimeError::NotPrivate`] for a file others can access or a link,
/// [`RuntimeError::Invalid`] for a damaged or mismatched one, and house
/// loading failures.
pub fn runtime_config(
    registry: &HouseRegistry,
    house: &HouseId,
) -> Result<Option<RuntimeConfig>, RuntimeError> {
    let config = registry.load(house)?;
    let path = registry.private_path(house)?.join(RUNTIME_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !private_file(&metadata) => return Err(RuntimeError::NotPrivate),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HouseError::from(error).into()),
    }
    let stored: RuntimeConfig = decode(&path).map_err(|error| match error {
        HouseError::InvalidInput => RuntimeError::Invalid,
        other => other.into(),
    })?;
    stored.validate(&config)?;
    Ok(Some(stored))
}

/// Store the house's runtime configuration, owner-only, replacing a
/// different valid one. A damaged or shared file is refused and kept.
///
/// # Errors
/// The errors of [`runtime_config`] for the existing file,
/// [`RuntimeError::Invalid`] for a configuration that fails validation, and
/// storage failures.
pub fn store_runtime(
    registry: &HouseRegistry,
    runtime: &RuntimeConfig,
) -> Result<RuntimeOutcome, RuntimeError> {
    let house = registry.load(&runtime.house)?;
    runtime.validate(&house)?;
    let existing = runtime_config(registry, &runtime.house)?;
    if existing.as_ref() == Some(runtime) {
        return Ok(RuntimeOutcome::Unchanged);
    }
    let contents = encode(runtime)?;
    if existing.is_none() {
        let path = RelativePath::new(&format!("private/{}/{RUNTIME_FILE}", runtime.house))?;
        crate::adoption::install_private_files(
            registry.root(),
            &[NewFile {
                path: &path,
                contents: &contents,
                mode: FileMode::Regular,
            }],
        )
        .map_err(|error| match error {
            HouseError::Conflicts(_) | HouseError::Conflict => RuntimeError::Invalid,
            other => other.into(),
        })?;
        return Ok(RuntimeOutcome::Created);
    }
    replace_private(
        &registry.private_path(&runtime.house)?.join(RUNTIME_FILE),
        &contents,
    )?;
    Ok(RuntimeOutcome::Replaced)
}

/// Replace the validated file by renaming an owner-only temporary over it,
/// so a reader sees the old or the new contents, never a partial file.
fn replace_private(path: &Path, contents: &[u8]) -> Result<(), HouseError> {
    use std::io::Write as _;
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&temporary).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()
    });
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, path)) {
        // The temporary is this call's own file: create_new made it.
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn private_file(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}
