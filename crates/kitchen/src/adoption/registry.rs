use super::installer::check_path;
use super::{
    FileMode, InstructionBundle, NewFile, RelativePath, ResolvedInstructions, install_new_files,
    install_snapshot, read_bounded, resolve_instructions,
};
use crate::{
    HouseId,
    contracts::CommitId,
    house::{HouseConfig, HouseError, RepositoryConfig, resolve_house},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// Name of the public, strict repository binding. No house policy or credentials
/// are written here; existing instructions and skills remain untouched.
pub const REPOSITORY_CONFIG: &str = ".kitchen.json";

/// External registry containing house policy and immutable snapshots. All paths
/// are explicit; opening it does not create private operational state.
#[derive(Debug, Clone)]
pub struct HouseRegistry {
    root: PathBuf,
}
impl HouseRegistry {
    /// Select an external directory. Ancestor redirects and repository paths are
    /// rejected before any write. Missing directories are created only by init.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, HouseError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(HouseError::InvalidInput);
        }
        ensure_external(&root)?;
        Ok(Self { root })
    }
    /// The external root; consumers must not use another house's subdirectory.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Create a house configuration without replacing any existing content.
    pub fn initialize(&self, house: &HouseConfig) -> Result<(), HouseError> {
        ensure_external(&self.root)?;
        house.validate()?;
        let path = RelativePath::new(&format!("houses/{}.json", house.house))?;
        let contents = encode(house)?;
        if install_new_files(
            &self.root,
            &[NewFile {
                path: &path,
                contents: &contents,
                mode: FileMode::Regular,
            }],
        )?
        .has_conflicts()
        {
            return Err(HouseError::Conflict);
        }
        Ok(())
    }
    /// Read and validate one exact house. No fallback to another house.
    pub fn load(&self, house: &HouseId) -> Result<HouseConfig, HouseError> {
        ensure_external(&self.root)?;
        let config: HouseConfig = decode(&self.config_path(house))?;
        config.validate()?;
        if config.house != *house {
            return Err(HouseError::HouseSelection);
        }
        Ok(config)
    }
    /// Enumerate bounded house configurations for explicit guided selection.
    pub fn houses(&self) -> Result<Vec<HouseConfig>, HouseError> {
        ensure_external(&self.root)?;
        let directory = self.root.join("houses");
        check_path(&directory)?;
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut houses = Vec::new();
        for entry in entries {
            let entry = entry?;
            if houses.len() >= 256 {
                return Err(HouseError::InvalidInput);
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or(HouseError::InvalidInput)?;
            let id = name.strip_suffix(".json").ok_or(HouseError::InvalidInput)?;
            let id = HouseId::new(id).map_err(|_| HouseError::InvalidInput)?;
            houses.push(self.load(&id)?);
        }
        houses.sort_by(|left, right| left.house.cmp(&right.house));
        Ok(houses)
    }
    /// Install exactly the configured pins, without changing the configuration.
    pub fn sync(
        &self,
        house: &HouseId,
        bundle: &InstructionBundle,
    ) -> Result<ResolvedInstructions, HouseError> {
        let _lock = self.lock()?;
        install_snapshot(&self.root, &self.load(house)?, bundle)
    }
    /// Explicit compare-and-swap update. Verify the complete new snapshot before
    /// replacing pins; retain all old snapshots for active tasks and recovery.
    /// `expected` prevents an old setup session overwriting a newer policy.
    pub fn update(
        &self,
        expected: &HouseConfig,
        bundle: &InstructionBundle,
    ) -> Result<ResolvedInstructions, HouseError> {
        let _lock = self.lock()?;
        let current = self.load(&expected.house)?;
        if current != *expected {
            return Err(HouseError::Conflict);
        }
        if bundle.house != current.house {
            return Err(HouseError::PinMismatch);
        }
        let mut next = current.clone();
        next.kitchen = bundle.kitchen.clone();
        next.guidance = bundle.guidance.clone();
        let resolved = install_snapshot(&self.root, &next, bundle)?;
        atomic_config(&self.config_path(&next.house), &encode(&next)?)?;
        Ok(resolved)
    }
    /// Resolve and verify pins for a new task in an adopted repository.
    pub fn resolve(
        &self,
        repository_root: &Path,
        revision: CommitId,
    ) -> Result<ResolvedInstructions, HouseError> {
        let config = read_repository(repository_root)?;
        let houses = self.houses()?;
        let house = resolve_house(&config, &houses)?;
        resolve_instructions(&self.root, house, Some(revision))
    }
    /// Change only an already-adopted repository's public settings after an
    /// explicit selection. The expected binding is rechecked under the registry
    /// lock; house/repository identity cannot change through this operation.
    /// Disabling workflows does not delete any labels, skills or instructions.
    pub fn configure_repository(
        &self,
        root: &Path,
        expected: &RepositoryConfig,
        next: &RepositoryConfig,
    ) -> Result<(), HouseError> {
        let _lock = self.lock()?;
        let house = self.load(&next.house)?;
        next.validate(&house)?;
        if expected.house != next.house || expected.repository != next.repository {
            return Err(HouseError::HouseSelection);
        }
        if read_repository(root)? != *expected {
            return Err(HouseError::Conflict);
        }
        if expected != next {
            atomic_config(&root.join(REPOSITORY_CONFIG), &encode(next)?)?;
        }
        Ok(())
    }
    /// House-scoped operational storage location. Does not create or open it.
    pub fn private_path(&self, house: &HouseId) -> Result<PathBuf, HouseError> {
        self.load(house)?;
        let path = self.root.join("private").join(house.as_str());
        ensure_external(&path)?;
        Ok(path)
    }
    fn config_path(&self, house: &HouseId) -> PathBuf {
        self.root.join("houses").join(format!("{house}.json"))
    }
    fn lock(&self) -> Result<File, HouseError> {
        ensure_external(&self.root)?;
        if !self.root.is_dir() {
            return Err(HouseError::HouseSelection);
        }
        let path = self.root.join("installation.lock");
        check_path(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        file.try_lock().map_err(|_| HouseError::Busy)?;
        Ok(file)
    }
}

/// Initialize a public repository binding, preserving an existing binding and
/// every local instruction file. Identical reruns succeed; changes need preview.
pub fn adopt_repository(
    root: &Path,
    repository: &RepositoryConfig,
    house: &HouseConfig,
) -> Result<super::InstallReport, HouseError> {
    repository.validate(house)?;
    let path = RelativePath::new(REPOSITORY_CONFIG)?;
    let contents = encode(repository)?;
    install_new_files(
        root,
        &[NewFile {
            path: &path,
            contents: &contents,
            mode: FileMode::Regular,
        }],
    )
}

/// Read an exact repository root's binding; missing binding fails closed.
pub fn read_repository(root: &Path) -> Result<RepositoryConfig, HouseError> {
    decode(&root.join(REPOSITORY_CONFIG)).map_err(|error| match error {
        HouseError::Io(std::io::ErrorKind::NotFound) => HouseError::HouseSelection,
        other => other,
    })
}

/// Find a binding from a repository/worktree subdirectory. Stop at the nearest
/// Git root; never inherit a parent repository's house through a nested checkout.
/// Multiple bindings between the start and root are ambiguous and refused.
pub fn repository_from_path(start: &Path) -> Result<(PathBuf, RepositoryConfig), HouseError> {
    check_path(start)?;
    let mut found = None;
    for root in start.ancestors() {
        let candidate = root.join(REPOSITORY_CONFIG);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => {
                if found.is_some() {
                    return Err(HouseError::HouseSelection);
                }
                found = Some((root.to_path_buf(), read_repository(root)?));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if path_present(&root.join(".git"))? {
            break;
        }
    }
    found.ok_or(HouseError::HouseSelection)
}

/// Deserialize a strict bounded document. Raw JSON errors are suppressed so
/// credentials accidentally supplied in unknown fields cannot reach logs.
pub fn decode<T: DeserializeOwned>(path: &Path) -> Result<T, HouseError> {
    serde_json::from_slice(&read_bounded(path)?).map_err(|_| HouseError::InvalidInput)
}
/// Serialize a public structured report or configuration.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, HouseError> {
    serde_json::to_vec_pretty(value).map_err(|_| HouseError::InvalidInput)
}

pub(crate) fn ensure_external(path: &Path) -> Result<(), HouseError> {
    check_path(path)?;
    for ancestor in path.ancestors() {
        if path_present(&ancestor.join(".git"))? || path_present(&ancestor.join(REPOSITORY_CONFIG))?
        {
            return Err(HouseError::InsideRepository);
        }
    }
    Ok(())
}
fn path_present(path: &Path) -> Result<bool, HouseError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn atomic_config(path: &Path, bytes: &[u8]) -> Result<(), HouseError> {
    check_path(path)?;
    let parent = path.parent().ok_or(HouseError::InvalidInput)?;
    let temporary = path.with_extension("pending");
    check_path(&temporary)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            HouseError::Conflict
        } else {
            error.into()
        }
    })?;
    // A crash leaves a pending file for explicit inspection; never delete a
    // pre-existing pending file, and never activate a partial snapshot.
    file.write_all(bytes)?;
    file.sync_all()?;
    check_path(path)?;
    check_path(&temporary)?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
