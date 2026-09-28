use super::installer::check_path;
use super::{
    FileMode, InstallReport, InstructionBundle, NewFile, RelativePath, RemoteName,
    ResolvedInstructions, checkout_remotes, checkout_root, install_snapshot, read_bounded,
    resolve_instructions,
};
use crate::{
    HouseId,
    contracts::{CommitId, Repository},
    house::{HouseConfig, HouseError, REPOSITORY_BINDING_SCHEMA, RepositoryConfig},
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// The working-tree binding file older Kitchen versions wrote. Kitchen no
/// longer writes or deletes it: [`HouseRegistry::import_legacy`] copies it into
/// the registry, and doctor reports it so the person can delete it.
pub const LEGACY_REPOSITORY_CONFIG: &str = ".kitchen.json";
/// Registry directory holding one binding per repository.
const BINDINGS: &str = "repositories";
/// Longest wait for the registry lock before reporting it busy.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Held for the duration of one registry mutation.
///
/// The lock is an `flock`-style lock on the open file description, the same
/// kind older Kitchen versions take, so mixed versions still exclude each
/// other. A child forked by another thread shares that description until it
/// execs, so closing the descriptor alone would leave the lock held; dropping
/// this releases it explicitly first.
struct RegistryLock(File);

impl Drop for RegistryLock {
    fn drop(&mut self) {
        // `drop` cannot report a failed unlock. The close that follows
        // still releases the lock once no child shares the description.
        let _unlock_result = self.0.unlock();
    }
}

/// Outcome of one non-blocking attempt on the lock file.
enum Attempt {
    Locked,
    Contended,
}

/// Take the file lock without blocking.
fn try_lock_file(file: &File) -> Result<Attempt, HouseError> {
    match file.try_lock() {
        Ok(()) => Ok(Attempt::Locked),
        Err(fs::TryLockError::WouldBlock) => Ok(Attempt::Contended),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// External registry containing house policy, immutable snapshots, and one
/// binding per repository. It is the only place Kitchen records a repository's
/// house; nothing is kept in working trees. All paths are explicit; opening it
/// does not create private operational state.
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
        super::installer::install_private_files(
            &self.root,
            &[NewFile {
                path: &path,
                contents: &contents,
                mode: FileMode::Regular,
            }],
        )?;
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
    pub fn houses(&self) -> Result<HouseListing, HouseError> {
        ensure_external(&self.root)?;
        let directory = self.root.join("houses");
        check_path(&directory)?;
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HouseListing::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut listing = HouseListing::default();
        for (index, entry) in entries.enumerate() {
            let entry = entry?;
            if index >= 256 {
                return Err(HouseError::InvalidInput);
            }
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|id| HouseId::new(id).ok())
            else {
                continue;
            };
            match self.load(&id) {
                Ok(house) => listing.available.push(house),
                Err(error) => listing.unavailable.push((id, error)),
            }
        }
        listing
            .available
            .sort_by(|left, right| left.house.cmp(&right.house));
        listing
            .unavailable
            .sort_by(|left, right| left.0.cmp(&right.0));
        Ok(listing)
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
        atomic_config(
            &self.config_path(&next.house),
            &current,
            &next,
            super::installer::Visibility::Private,
        )?;
        Ok(resolved)
    }
    /// Resolve and verify pins for a new task in the bound repository whose
    /// checkout contains `start`.
    ///
    /// # Errors
    /// Refuses a checkout that [`Self::resolve_repository`] does not resolve to
    /// a stored binding.
    pub fn resolve(
        &self,
        start: &Path,
        revision: CommitId,
    ) -> Result<ResolvedInstructions, HouseError> {
        let RepositoryMatch::Bound(config) = self.resolve_repository(start)? else {
            return Err(HouseError::HouseSelection);
        };
        let house = self.load(&config.house)?;
        resolve_instructions(&self.root, &house, Some(revision))
    }
    /// The stored binding for `repository`, matched without regard to case.
    ///
    /// # Errors
    /// Refuses a damaged binding, one stored under another repository's key,
    /// and a binding in an older schema.
    pub fn binding(&self, repository: &Repository) -> Result<Option<RepositoryConfig>, HouseError> {
        ensure_external(&self.root)?;
        let config: RepositoryConfig =
            match decode(&self.root.join(binding_path(repository)?.as_path())) {
                Ok(config) => config,
                Err(HouseError::Io(std::io::ErrorKind::NotFound)) => return Ok(None),
                Err(error) => return Err(error),
            };
        if config.schema != REPOSITORY_BINDING_SCHEMA || key(&config.repository) != key(repository)
        {
            return Err(HouseError::InvalidInput);
        }
        Ok(Some(config))
    }
    /// Store a new repository binding in the registry, create-only. Nothing is
    /// written to any working tree. An identical binding is left unchanged.
    ///
    /// # Errors
    /// Refuses a binding its house does not allow, and reports
    /// [`HouseError::Conflicts`] when a different binding already exists.
    pub fn bind_repository(&self, config: &RepositoryConfig) -> Result<InstallReport, HouseError> {
        let _lock = self.lock()?;
        let house = self.load(&config.house)?;
        config.validate(&house)?;
        let path = binding_path(&config.repository)?;
        let contents = encode(config)?;
        super::installer::install_private_files(
            &self.root,
            &[NewFile {
                path: &path,
                contents: &contents,
                mode: FileMode::Regular,
            }],
        )
    }
    /// Change only an already-bound repository's settings after an explicit
    /// selection. The expected binding is rechecked under the registry lock;
    /// house/repository identity cannot change through this operation.
    /// Disabling workflows does not delete any labels, skills or instructions.
    pub fn configure_repository(
        &self,
        expected: &RepositoryConfig,
        next: &RepositoryConfig,
    ) -> Result<(), HouseError> {
        let _lock = self.lock()?;
        let house = self.load(&next.house)?;
        next.validate(&house)?;
        if expected.house != next.house || expected.repository != next.repository {
            return Err(HouseError::HouseSelection);
        }
        if self.binding(&expected.repository)?.as_ref() != Some(expected) {
            return Err(HouseError::Conflict);
        }
        if expected != next {
            atomic_config(
                &self.root.join(binding_path(&next.repository)?.as_path()),
                expected,
                next,
                super::installer::Visibility::Private,
            )?;
        }
        Ok(())
    }
    /// Everything the registry says about the checkout containing `start`:
    /// the stored binding for its identifying remote and the houses whose
    /// allowlists name it. Reads the remotes with bounded `git` calls; writes
    /// nothing.
    ///
    /// # Errors
    /// See [`checkout_remotes`]; a damaged binding is refused, and
    /// [`HouseError::RemotesDisagree`] when another remote belongs to a
    /// different house than the identifying one.
    pub fn claims(&self, start: &Path) -> Result<RepositoryClaims, HouseError> {
        let remotes = checkout_remotes(start)?;
        let listing = self.houses()?;
        let owners = |repository: &Repository| -> Result<Owners, HouseError> {
            let binding = self.binding(repository)?;
            let claims: Vec<(Repository, HouseId)> = listing
                .available
                .iter()
                .flat_map(|house| {
                    house
                        .repositories
                        .iter()
                        .filter(|allowed| key(allowed) == key(repository))
                        .map(|allowed| (allowed.clone(), house.house.clone()))
                })
                .collect();
            Ok(Owners { binding, claims })
        };
        let selected = owners(&remotes.selected.repository)?;
        let selected_houses = selected.houses();
        let mut disagreeing: Vec<RemoteName> = Vec::new();
        for other in &remotes.others {
            if key(&other.repository) == key(&remotes.selected.repository) {
                continue;
            }
            let houses = owners(&other.repository)?.houses();
            if !houses.is_empty()
                && houses != selected_houses
                && !disagreeing.contains(&other.remote)
            {
                disagreeing.push(other.remote.clone());
            }
        }
        if !disagreeing.is_empty() {
            let mut named = vec![remotes.selected.remote];
            named.extend(disagreeing);
            return Err(HouseError::RemotesDisagree { remotes: named });
        }
        Ok(RepositoryClaims {
            binding: selected.binding,
            claims: selected.claims,
            unavailable: listing
                .unavailable
                .into_iter()
                .map(|(house, _)| house)
                .collect(),
        })
    }
    /// Resolve the house for the checkout containing `start`. A stored binding
    /// decides; otherwise exactly one house may claim the checkout. Every
    /// worktree and subdirectory of a repository resolves the same way.
    ///
    /// # Errors
    /// [`HouseError::AmbiguousHouse`] when several houses claim it without a
    /// stored choice, [`HouseError::RemotesDisagree`] when another remote
    /// belongs to a different house, and [`HouseError::HouseSelection`] when
    /// no house claims it, a bound house no longer allows it, or an unreadable
    /// house might also claim it.
    pub fn resolve_repository(&self, start: &Path) -> Result<RepositoryMatch, HouseError> {
        let claims = self.claims(start)?;
        if let Some(binding) = claims.binding {
            let house = self.load(&binding.house)?;
            binding.validate(&house)?;
            return Ok(RepositoryMatch::Bound(binding));
        }
        if !claims.unavailable.is_empty() {
            return Err(HouseError::HouseSelection);
        }
        match claims.claims.as_slice() {
            [] => Err(HouseError::HouseSelection),
            [(repository, house)] => Ok(RepositoryMatch::Unbound {
                repository: repository.clone(),
                house: house.clone(),
            }),
            claims => Err(HouseError::AmbiguousHouse {
                houses: claims.iter().map(|(_, house)| house.clone()).collect(),
            }),
        }
    }
    /// Copy the legacy `.kitchen.json` at the top of the checkout containing
    /// `start` into the registry. With `approved` `None` this only previews.
    /// With `Some`, the binding is stored only if it still has that digest, so
    /// exactly what the person saw is what is stored; a file edited in between
    /// (it is repository content, so a pull request can edit it) is refused.
    /// The file must name the repository this checkout's identifying remote
    /// names; it is never modified or deleted.
    ///
    /// # Errors
    /// [`HouseError::Io`] with `NotFound` when there is no legacy file,
    /// [`HouseError::InvalidInput`] for a file that is not a schema 1 binding,
    /// [`HouseError::HouseSelection`] when its repository is not the
    /// checkout's or its house does not allow it, and
    /// [`HouseError::LegacyChanged`] when `approved` is not its digest.
    pub fn import_legacy(
        &self,
        start: &Path,
        approved: Option<&BindingDigest>,
    ) -> Result<LegacyImport, HouseError> {
        let source = checkout_root(start)?.join(LEGACY_REPOSITORY_CONFIG);
        check_path(&source)?;
        let mut binding: RepositoryConfig = decode(&source)?;
        if binding.schema != 1 {
            return Err(HouseError::InvalidInput);
        }
        binding.schema = REPOSITORY_BINDING_SCHEMA;
        if key(&checkout_remotes(start)?.selected.repository) != key(&binding.repository) {
            return Err(HouseError::HouseSelection);
        }
        binding.validate(&self.load(&binding.house)?)?;
        let digest = BindingDigest::of(&binding)?;
        if approved.is_some_and(|approved| *approved != digest) {
            return Err(HouseError::LegacyChanged);
        }
        let status = match self.binding(&binding.repository)? {
            Some(existing) if existing == binding => LegacyImportStatus::Unchanged,
            Some(_) => LegacyImportStatus::Conflict,
            None if approved.is_some() => {
                self.bind_repository(&binding)?;
                LegacyImportStatus::Created
            }
            None => LegacyImportStatus::WouldCreate,
        };
        Ok(LegacyImport {
            source,
            binding,
            digest,
            status,
        })
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
    fn lock(&self) -> Result<RegistryLock, HouseError> {
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
        // Wait briefly for a writer that is about to release; a longer hold
        // is a real writer.
        let started = std::time::Instant::now();
        let mut backoff = std::time::Duration::from_millis(1);
        loop {
            match try_lock_file(&file)? {
                Attempt::Locked => return Ok(RegistryLock(file)),
                Attempt::Contended if started.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(backoff);
                    backoff = backoff
                        .saturating_mul(2)
                        .min(std::time::Duration::from_millis(20));
                }
                Attempt::Contended => return Err(HouseError::Busy),
            }
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn registry() -> Result<(tempfile::TempDir, HouseRegistry), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let registry = HouseRegistry::new(directory.path().canonicalize()?)?;
        Ok((directory, registry))
    }

    #[test]
    fn a_shared_description_does_not_extend_the_lock() -> TestResult {
        let (_directory, registry) = registry()?;
        let held = registry.lock()?;
        // What a child forked before its exec holds: the same description.
        let inherited = held.0.try_clone()?;
        drop(held);
        let reacquired = registry.lock();
        drop(inherited);
        assert!(reacquired.is_ok(), "a shared description kept the lock");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_child_sharing_the_description_does_not_extend_the_lock() -> TestResult {
        let (_directory, registry) = registry()?;
        let held = registry.lock()?;
        // The child keeps the description past exec, standing in for one
        // forked by another thread that has not exec'd yet.
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .stdin(std::process::Stdio::from(held.0.try_clone()?))
            .spawn()?;
        drop(held);
        let reacquired = registry.lock();
        let alive = child.try_wait()?.is_none();
        child.kill()?;
        child.wait()?;
        assert!(alive, "child must still share the description");
        assert!(reacquired.is_ok(), "child retained the registry lock");
        Ok(())
    }

    #[test]
    fn a_lock_held_on_another_description_is_contended() -> TestResult {
        let (_directory, registry) = registry()?;
        let held = registry.lock()?;
        let other = File::open(registry.root().join("installation.lock"))?;
        assert!(matches!(try_lock_file(&other)?, Attempt::Contended));
        drop(held);
        assert!(matches!(try_lock_file(&other)?, Attempt::Locked));
        Ok(())
    }

    #[test]
    fn an_unopenable_lock_file_is_an_error_not_busy() -> TestResult {
        let (_directory, registry) = registry()?;
        fs::create_dir(registry.root().join("installation.lock"))?;
        assert!(matches!(registry.lock(), Err(HouseError::Io(_))));
        Ok(())
    }
}

/// The legacy `.kitchen.json` at the top of the checkout containing `start`,
/// if one is present. It is only reported, never read into a decision.
///
/// # Errors
/// See [`checkout_root`].
pub fn legacy_binding(start: &Path) -> Result<Option<PathBuf>, HouseError> {
    let path = checkout_root(start)?.join(LEGACY_REPOSITORY_CONFIG);
    Ok(path_present(&path)?.then_some(path))
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
        if path_present(&ancestor.join(".git"))?
            || path_present(&ancestor.join(LEGACY_REPOSITORY_CONFIG))?
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
fn atomic_config<T: Serialize + DeserializeOwned + PartialEq>(
    path: &Path,
    expected: &T,
    next: &T,
    visibility: super::installer::Visibility,
) -> Result<(), HouseError> {
    let bytes = encode(next)?;
    check_path(path)?;
    let parent = path.parent().ok_or(HouseError::InvalidInput)?;
    let temporary = path.with_extension("pending");
    check_path(&temporary)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(visibility.file_mode(FileMode::Regular));
    }
    let mut file = options.open(&temporary).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            HouseError::Conflict
        } else {
            error.into()
        }
    })?;
    // The pending name also fences independent registries targeting the same
    // public binding. Recheck the expected document after taking that slot.
    if decode::<T>(path)? != *expected {
        check_path(&temporary)?;
        if super::installer::same_file(&temporary, &file) && fs::metadata(&temporary)?.len() == 0 {
            fs::remove_file(&temporary)?;
        }
        return Err(HouseError::Conflict);
    }
    // A crash leaves a pending file for explicit inspection; never delete a
    // pre-existing pending file, and never activate a partial snapshot.
    file.write_all(&bytes)?;
    file.sync_all()?;
    check_path(path)?;
    check_path(&temporary)?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Guided selection results; a damaged house never prevents selecting another.
#[derive(Debug, Default)]
pub struct HouseListing {
    /// Valid configurations, sorted by house ID.
    pub available: Vec<HouseConfig>,
    /// Houses requiring individual repair, with their load failures.
    pub unavailable: Vec<(HouseId, HouseError)>,
}

/// How the registry resolved a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryMatch {
    /// A stored binding decides the house.
    Bound(RepositoryConfig),
    /// Exactly one house claims the repository, which is not set up yet.
    Unbound {
        /// The repository as the house allowlist names it.
        repository: Repository,
        /// The only claiming house.
        house: HouseId,
    },
}

/// What the registry holds for a checkout's identifying repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryClaims {
    /// The stored binding for the repository, if any.
    pub binding: Option<RepositoryConfig>,
    /// Each readable house allowing the repository, with the repository as
    /// that house's allowlist names it.
    pub claims: Vec<(Repository, HouseId)>,
    /// Houses that could not be read and might also claim the checkout.
    pub unavailable: Vec<HouseId>,
}
impl RepositoryClaims {
    /// The repository setup should bind, and its stored binding if any.
    ///
    /// # Errors
    /// [`HouseError::HouseSelection`] when neither a binding nor a house
    /// allowlist names the repository.
    pub fn setup_target(&self) -> Result<(Repository, Option<RepositoryConfig>), HouseError> {
        if let Some(binding) = &self.binding {
            return Ok((binding.repository.clone(), Some(binding.clone())));
        }
        self.claims
            .first()
            .map(|(repository, _)| (repository.clone(), None))
            .ok_or(HouseError::HouseSelection)
    }
}
/// Who the registry says owns one repository.
struct Owners {
    binding: Option<RepositoryConfig>,
    claims: Vec<(Repository, HouseId)>,
}
impl Owners {
    /// The stored house, else every house allowing the repository.
    fn houses(&self) -> BTreeSet<HouseId> {
        match &self.binding {
            Some(binding) => BTreeSet::from([binding.house.clone()]),
            None => self.claims.iter().map(|(_, house)| house.clone()).collect(),
        }
    }
}

/// The outcome of a legacy binding import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegacyImportStatus {
    /// Preview: applying would store the binding.
    WouldCreate,
    /// The binding was stored in the registry.
    Created,
    /// The registry already holds the same binding.
    Unchanged,
    /// The registry holds a different binding; nothing was changed.
    Conflict,
}

/// A previewed or applied legacy import. The source file is left in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyImport {
    /// The legacy file that was read.
    pub source: PathBuf,
    /// The binding in the registry schema.
    pub binding: RepositoryConfig,
    /// Digest of `binding`; approving it stores exactly this binding.
    pub digest: BindingDigest,
    /// What happened in the registry.
    pub status: LegacyImportStatus,
}

/// SHA-256 of a repository binding as the registry would store it. Approving a
/// digest approves that content and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingDigest([u8; 32]);
impl BindingDigest {
    /// Digest the canonical encoding of `binding`.
    fn of(binding: &RepositoryConfig) -> Result<Self, HouseError> {
        let mut hash = Sha256::new();
        hash.update(b"kitchen repository binding\n");
        hash.update(encode(binding)?);
        Ok(Self(hash.finalize().into()))
    }
}
impl std::fmt::Display for BindingDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}
impl std::str::FromStr for BindingDigest {
    type Err = HouseError;
    /// Exactly 64 hexadecimal digits.
    fn from_str(text: &str) -> Result<Self, HouseError> {
        if text.len() != 64 || !text.is_ascii() {
            return Err(HouseError::InvalidInput);
        }
        let mut bytes = [0_u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| HouseError::InvalidInput)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| HouseError::InvalidInput)?;
        }
        Ok(Self(bytes))
    }
}
impl Serialize for BindingDigest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Case-insensitive repository key; GitHub owner and name ignore case.
fn key(repository: &Repository) -> String {
    repository.as_str().to_ascii_lowercase()
}
/// Registry path of a repository's binding.
fn binding_path(repository: &Repository) -> Result<RelativePath, HouseError> {
    RelativePath::new(&format!(
        "{BINDINGS}/{}/{}.json",
        repository.owner().to_ascii_lowercase(),
        repository.name().to_ascii_lowercase()
    ))
}
