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
    contracts::{BranchName, ExternalRef, Repository},
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

/// The most unsettled pickup tasks a stored capacity may allow.
const MAX_PICKUP_CAPACITY: u32 = 64;
/// The longest label or path a stored pickup setting may hold.
const MAX_PICKUP_TEXT: usize = 256;

/// The scheduled pickup pass's settings for one house.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PickupConfig {
    /// The label that marks an issue ready for an agent.
    pub ready_label: String,
    /// The label that marks an issue needing a specification pass.
    pub needs_spec_label: String,
    /// The label that reserves an issue for a person.
    pub human_label: String,
    /// Most unsettled scheduled pickup tasks in the repository.
    pub capacity: u32,
    /// Workers create `<prefix>/issue-<number>`.
    pub branch_prefix: String,
    /// Where each worker writes its evidence report in its workspace.
    pub report_path: String,
}

impl Default for PickupConfig {
    fn default() -> Self {
        Self {
            ready_label: "ready".to_owned(),
            needs_spec_label: "needs-spec".to_owned(),
            human_label: "human-only".to_owned(),
            capacity: 1,
            branch_prefix: "kitchen".to_owned(),
            report_path: "kitchen-report.md".to_owned(),
        }
    }
}

impl PickupConfig {
    /// Check that every label is plain text, the capacity is between 1 and
    /// 64, the branch prefix is a valid branch name, and the report path is
    /// a plain relative path that stays inside the workspace.
    ///
    /// # Errors
    /// [`RuntimeError::Invalid`] for any of those failing.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let plain = |text: &str| {
            !text.trim().is_empty()
                && text.len() <= MAX_PICKUP_TEXT
                && !text.chars().any(char::is_control)
        };
        let labels = [&self.ready_label, &self.needs_spec_label, &self.human_label];
        let path = &self.report_path;
        let contained = plain(path)
            && !path.starts_with('/')
            && !path.split('/').any(|part| part == ".." || part.is_empty());
        if labels.iter().all(|label| plain(label))
            && (1..=MAX_PICKUP_CAPACITY).contains(&self.capacity)
            && plain(&self.branch_prefix)
            && BranchName::new(&self.branch_prefix).is_ok()
            && contained
        {
            Ok(())
        } else {
            Err(RuntimeError::Invalid)
        }
    }
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
    /// The scheduled pickup pass's settings. Absent means the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pickup: Option<PickupConfig>,
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
    /// Check the schema, the house, that every path is absolute, that the
    /// repository is one of the house's, and the pickup settings.
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
        let pickup = self
            .pickup
            .as_ref()
            .is_none_or(|pickup| pickup.validate().is_ok());
        if self.schema == RUNTIME_SCHEMA
            && self.house == house.house
            && absolute
            && repository
            && pickup
        {
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

/// How many unique temporary names one replacement tries before giving up.
const TEMPORARY_ATTEMPTS: usize = 8;

/// Replace the validated file by renaming an owner-only temporary over it,
/// so a reader sees the old or the new contents, never a partial file.
fn replace_private(path: &Path, contents: &[u8]) -> Result<(), HouseError> {
    replace_private_with(path, contents, temporary_suffix)
}

/// A per-call temporary name part: the process and 64 random bits.
fn temporary_suffix() -> std::io::Result<String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| std::io::Error::other("no entropy"))?;
    Ok(format!(
        "{}.{:016x}",
        std::process::id(),
        u64::from_le_bytes(random)
    ))
}

/// [`replace_private`] with the temporary names supplied. The temporary is
/// created with `create_new` under a name no other writer shares; a name that
/// already exists belongs to someone else and is never opened or removed. Only
/// a temporary this call created is removed, and only when the replacement
/// fails.
fn replace_private_with(
    path: &Path,
    contents: &[u8],
    mut suffix: impl FnMut() -> std::io::Result<String>,
) -> Result<(), HouseError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut created = None;
    for _ in 0..TEMPORARY_ATTEMPTS {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{}.tmp", suffix()?));
        let temporary = PathBuf::from(name);
        match options.open(&temporary) {
            Ok(file) => {
                created = Some((temporary, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some((temporary, mut file)) = created else {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into());
    };
    let written = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            std::fs::rename(&temporary, path)
        });
    if let Err(error) = written {
        // create_new made this file, so it is this call's own to remove.
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn names<'a>(list: &'a [&'a str]) -> impl FnMut() -> std::io::Result<String> + 'a {
        let mut next = list.iter();
        move || Ok((*next.next().unwrap_or(&"exhausted")).to_owned())
    }

    #[test]
    fn a_foreign_temporary_is_kept_and_the_next_name_is_used() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("runtime.json");
        std::fs::write(&path, b"old")?;
        let foreign = dir.path().join("runtime.json.same.tmp");
        std::fs::write(&foreign, b"someone else's write")?;
        replace_private_with(&path, b"new", names(&["same", "other"]))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert_eq!(std::fs::read(&path)?, b"new");
        assert_eq!(std::fs::read(&foreign)?, b"someone else's write");
        assert!(!dir.path().join("runtime.json.other.tmp").exists());
        Ok(())
    }

    #[test]
    fn only_collisions_fail_without_touching_the_foreign_files() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("runtime.json");
        std::fs::write(&path, b"old")?;
        let foreign = dir.path().join("runtime.json.same.tmp");
        std::fs::write(&foreign, b"theirs")?;
        let refused = replace_private_with(&path, b"new", || Ok("same".to_owned()));
        assert!(matches!(
            refused,
            Err(HouseError::Io(std::io::ErrorKind::AlreadyExists))
        ));
        assert_eq!(std::fs::read(&path)?, b"old");
        assert_eq!(std::fs::read(&foreign)?, b"theirs");
        Ok(())
    }

    #[test]
    fn a_failed_rename_removes_only_its_own_temporary() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        // A directory at the destination makes the rename fail.
        let path = dir.path().join("runtime.json");
        std::fs::create_dir(&path)?;
        let foreign = dir.path().join("runtime.json.a.tmp");
        std::fs::write(&foreign, b"theirs")?;
        assert!(replace_private_with(&path, b"new", names(&["a", "b"])).is_err());
        assert!(!dir.path().join("runtime.json.b.tmp").exists());
        assert_eq!(std::fs::read(&foreign)?, b"theirs");
        Ok(())
    }

    #[test]
    fn generated_suffixes_differ() -> std::io::Result<()> {
        assert_ne!(temporary_suffix()?, temporary_suffix()?);
        Ok(())
    }
}
