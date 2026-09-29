//! Shared snapshot persistence for house-scoped stores.
//!
//! A store is one directory holding an initialization marker, one snapshot
//! file, a temporary file for atomic replacement, and a lock file. Each
//! operation takes the lock, reloads the snapshot within a size bound, and
//! replaces it atomically only when the content changed. The marker binds the
//! snapshot to one house and one store identity (nonce), so a snapshot copied
//! from another store fails closed.
//!
//! [`crate::state::HouseStore`] and the trust ledger use this engine with
//! their own [`StoreLayout`] and payload type.

use std::{
    cmp::Ordering,
    fs::{self, File, OpenOptions, TryLockError},
    hash::{BuildHasher, RandomState},
    io::{Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    HouseId,
    contracts::ContractError,
    state::{Corruption, StateError, StorageOperation},
};

const MAX_LOCK_BACKOFF: Duration = Duration::from_millis(50);

/// Bounds for store I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreOptions {
    /// Longest wait for the store lock before [`StateError::LockTimeout`].
    pub lock_timeout: Duration,
    /// Largest state file the store reads or writes.
    pub max_state_bytes: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(5),
            max_state_bytes: 8 * 1024 * 1024,
        }
    }
}

/// The payload one store persists.
pub(crate) trait Snapshot: Serialize + DeserializeOwned {
    /// The persisted schema version, checked in the marker and the snapshot.
    const SCHEMA: u64;
    /// The payload's error type; engine failures convert into it.
    type Error: From<StateError> + From<ContractError>;
    /// The empty payload written at initialization.
    fn empty(house: HouseId, nonce: u64) -> Self;
    /// The store identity recorded in the payload.
    fn nonce(&self) -> u64;
    /// Check payload invariants after decoding.
    fn validate(&self, house: &HouseId) -> Result<(), Self::Error>;
}

/// File names and policy for one store within its own directory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoreLayout {
    pub marker: &'static str,
    pub snapshot: &'static str,
    pub temporary: &'static str,
    pub lock: &'static str,
    /// Write the snapshot as indented JSON.
    pub pretty: bool,
    /// Refuse a store directory or managed file readable by other users.
    /// Enforced on Unix; other platforms have no mode bits to check.
    pub require_private: bool,
    /// A persistent file whose exclusive lock, held by a priority writer,
    /// makes ordinary lockers yield to it. It is never removed, so a locker
    /// always probes the file a priority writer locks. `None` disables
    /// priority writes.
    pub priority_intent: Option<&'static str>,
    /// Bytes of [`StoreOptions::max_state_bytes`] only priority writes may use.
    pub priority_reserve_bytes: u64,
}

/// Handles are cheap and hold no open files; separate handles and processes
/// coordinate through the lock file.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotStore<S> {
    dir: PathBuf,
    house: HouseId,
    nonce: u64,
    options: StoreOptions,
    layout: StoreLayout,
    payload: PhantomData<fn() -> S>,
}

/// The initialization marker. It never changes after initialization.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoreMarker {
    schema: u64,
    house: HouseId,
    nonce: u64,
}

/// The fields checked before the full payload is decoded.
#[derive(Deserialize)]
struct SchemaProbe {
    schema: u64,
    house: HouseId,
}

impl<S: Snapshot> SnapshotStore<S> {
    /// Create a new store in `dir`, creating the directory and missing parents.
    pub(crate) fn initialize(
        dir: impl AsRef<Path>,
        house: HouseId,
        options: StoreOptions,
        layout: StoreLayout,
    ) -> Result<Self, S::Error> {
        let dir = dir.as_ref();
        refuse_symlink(dir)?;
        // Refuse before creating anything, so a refusal leaves no directory
        // behind in a working tree; `at` rechecks the created directory.
        if inside_repository(dir)
            .map_err(|error| StateError::io(StorageOperation::Prepare, error))?
        {
            return Err(StateError::StorageInsideRepository.into());
        }
        create_private_dir(dir)
            .map_err(|error| StateError::io(StorageOperation::Prepare, error))?;
        let mut store = Self::at(dir, house, 0, options, layout)?;
        let _lock = store.lock(true, false)?;
        if exists(&store.dir.join(layout.marker))? || exists(&store.dir.join(layout.snapshot))? {
            return Err(StateError::AlreadyInitialized.into());
        }
        store.nonce = fresh_nonce();
        let marker = StoreMarker {
            schema: S::SCHEMA,
            house: store.house.clone(),
            nonce: store.nonce,
        };
        let marker = serde_json::to_vec_pretty(&marker)
            .map_err(|error| StateError::io(StorageOperation::Write, error.into()))?;
        // The marker goes first: if a crash follows, the store is
        // established without a snapshot and fails closed on open.
        store.write_file(layout.marker, &marker)?;
        let empty = store.serialize(&S::empty(store.house.clone(), store.nonce))?;
        store.write_bytes(&empty, false)?;
        Ok(store)
    }

    /// Open the established store in `dir`. It never writes a replacement
    /// snapshot.
    pub(crate) fn open(
        dir: impl AsRef<Path>,
        house: HouseId,
        options: StoreOptions,
        layout: StoreLayout,
    ) -> Result<Self, S::Error> {
        let dir = dir.as_ref();
        refuse_symlink(dir)?;
        let mut store = Self::at(dir, house, 0, options, layout)?;
        let _lock = store.lock(false, false)?;
        let marker = store.read_marker()?;
        store.nonce = marker.nonce;
        store.load()?.ok_or(StateError::StateMissing)?;
        Ok(store)
    }

    fn at(
        dir: &Path,
        house: HouseId,
        nonce: u64,
        options: StoreOptions,
        layout: StoreLayout,
    ) -> Result<Self, StateError> {
        let dir = fs::canonicalize(dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StateError::NotInitialized
            } else {
                StateError::io(StorageOperation::Prepare, error)
            }
        })?;
        if inside_repository(&dir)
            .map_err(|error| StateError::io(StorageOperation::Prepare, error))?
        {
            return Err(StateError::StorageInsideRepository);
        }
        Ok(Self {
            dir,
            house,
            nonce,
            options,
            layout,
            payload: PhantomData,
        })
    }

    /// The house this store serves.
    pub(crate) const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Apply `apply` to the snapshot under the exclusive lock, and replace
    /// the snapshot if the content changed. An error from `apply` writes
    /// nothing.
    pub(crate) fn transact<T>(
        &self,
        apply: impl FnOnce(&mut S) -> Result<T, S::Error>,
    ) -> Result<T, S::Error> {
        self.transact_inner(false, apply)
    }

    /// Like [`Self::transact`], but ordinary lockers yield to this writer and
    /// it may use [`StoreLayout::priority_reserve_bytes`].
    pub(crate) fn transact_priority<T>(
        &self,
        apply: impl FnOnce(&mut S) -> Result<T, S::Error>,
    ) -> Result<T, S::Error> {
        self.transact_inner(true, apply)
    }

    fn transact_inner<T>(
        &self,
        priority: bool,
        apply: impl FnOnce(&mut S) -> Result<T, S::Error>,
    ) -> Result<T, S::Error> {
        let _intent = match (priority, self.layout.priority_intent) {
            (true, Some(name)) => Some(PriorityIntent::acquire(
                &self.dir,
                name,
                self.options.lock_timeout,
            )?),
            (true, None) | (false, _) => None,
        };
        let _lock = self.lock(true, priority)?;
        let (mut state, before) = self.load()?.ok_or(StateError::StateMissing)?;
        let value = apply(&mut state)?;
        let after = self.serialize(&state)?;
        if after != before {
            self.write_bytes(&after, priority)?;
        }
        Ok(value)
    }

    /// Read the snapshot under the shared lock.
    pub(crate) fn read<T>(&self, view: impl FnOnce(&S) -> T) -> Result<T, S::Error> {
        let _lock = self.lock(false, false)?;
        let (state, _) = self.load()?.ok_or(StateError::StateMissing)?;
        Ok(view(&state))
    }

    /// Like [`Self::read`], also passing the stored snapshot's size in bytes.
    pub(crate) fn read_sized<T>(&self, view: impl FnOnce(&S, u64) -> T) -> Result<T, S::Error> {
        let _lock = self.lock(false, false)?;
        let (state, bytes) = self.load()?.ok_or(StateError::StateMissing)?;
        Ok(view(&state, u64::try_from(bytes.len()).unwrap_or(u64::MAX)))
    }

    fn read_marker(&self) -> Result<StoreMarker, S::Error> {
        let path = self.dir.join(self.layout.marker);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(if exists(&self.dir.join(self.layout.snapshot))? {
                    StateError::CorruptState(Corruption::Marker).into()
                } else {
                    StateError::NotInitialized.into()
                });
            }
            Err(error) => return Err(StateError::io(StorageOperation::Read, error).into()),
        };
        let marker: StoreMarker = serde_json::from_slice(&bytes)
            .map_err(|_| StateError::CorruptState(Corruption::Marker))?;
        if marker.schema != S::SCHEMA {
            return Err(StateError::UnsupportedSchema {
                found: marker.schema,
            }
            .into());
        }
        if marker.house != self.house {
            return Err(ContractError::CrossHouse {
                expected: self.house.clone(),
                found: marker.house,
            }
            .into());
        }
        Ok(marker)
    }

    fn lock(&self, exclusive: bool, priority: bool) -> Result<File, StateError> {
        if self.layout.require_private {
            require_private(&self.dir)?;
        }
        for name in [
            Some(self.layout.marker),
            Some(self.layout.lock),
            Some(self.layout.snapshot),
            Some(self.layout.temporary),
            self.layout.priority_intent,
        ]
        .into_iter()
        .flatten()
        {
            let path = self.dir.join(name);
            refuse_redirected(&path)?;
            if self.layout.require_private && exists(&path)? {
                require_private(&path)?;
            }
        }
        let file = open_lock_file(&self.dir.join(self.layout.lock))
            .map_err(|error| StateError::io(StorageOperation::Lock, error))?;
        let started = Instant::now();
        let mut backoff = Duration::from_millis(2);
        loop {
            if !priority && self.priority_pending()? {
                self.wait_for_lock(started, &mut backoff)?;
                continue;
            }
            let attempt = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match attempt {
                // A priority writer may have announced itself while this
                // locker waited; yield to it before touching the snapshot.
                Ok(()) if !priority && self.priority_pending()? => {
                    file.unlock()
                        .map_err(|error| StateError::io(StorageOperation::Lock, error))?;
                }
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(StateError::io(StorageOperation::Lock, error));
                }
            }
            self.wait_for_lock(started, &mut backoff)?;
        }
    }

    fn wait_for_lock(&self, started: Instant, backoff: &mut Duration) -> Result<(), StateError> {
        let waited = started.elapsed();
        let Some(remaining) = self
            .options
            .lock_timeout
            .checked_sub(waited)
            .filter(|left| !left.is_zero())
        else {
            return Err(StateError::LockTimeout {
                waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
            });
        };
        thread::sleep((*backoff).min(remaining));
        *backoff = backoff.saturating_mul(2).min(MAX_LOCK_BACKOFF);
        Ok(())
    }

    /// Whether a priority writer currently holds its intent. The probe takes
    /// a shared lock and releases it at once; a crashed writer's lock is
    /// released by the operating system, so nothing needs cleaning up.
    fn priority_pending(&self) -> Result<bool, StateError> {
        let Some(name) = self.layout.priority_intent else {
            return Ok(false);
        };
        let lock = |error| StateError::io(StorageOperation::Lock, error);
        let path = self.dir.join(name);
        refuse_redirected(&path)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(lock(error)),
        };
        match file.try_lock_shared() {
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(error)) => Err(lock(error)),
            Ok(()) => {
                file.unlock().map_err(lock)?;
                Ok(false)
            }
        }
    }

    fn load(&self) -> Result<Option<(S, Vec<u8>)>, S::Error> {
        let file = match File::open(self.dir.join(self.layout.snapshot)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(StateError::io(StorageOperation::Read, error).into()),
        };
        let limit = self.options.max_state_bytes;
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| StateError::io(StorageOperation::Read, error))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(StateError::StateTooLarge { limit_bytes: limit }.into());
        }
        let state = self.parse(&bytes)?;
        Ok(Some((state, bytes)))
    }

    fn parse(&self, bytes: &[u8]) -> Result<S, S::Error> {
        let probe: SchemaProbe = serde_json::from_slice(bytes).map_err(syntax)?;
        if probe.schema != S::SCHEMA {
            return Err(StateError::UnsupportedSchema {
                found: probe.schema,
            }
            .into());
        }
        if probe.house != self.house {
            return Err(ContractError::CrossHouse {
                expected: self.house.clone(),
                found: probe.house,
            }
            .into());
        }
        let state: S = serde_json::from_slice(bytes).map_err(syntax)?;
        state.validate(&self.house)?;
        if state.nonce() != self.nonce {
            return Err(StateError::CorruptState(Corruption::StoreIdentity).into());
        }
        Ok(state)
    }

    fn serialize(&self, state: &S) -> Result<Vec<u8>, StateError> {
        if self.layout.pretty {
            serde_json::to_vec_pretty(state)
        } else {
            serde_json::to_vec(state)
        }
        .map_err(|error| StateError::io(StorageOperation::Write, error.into()))
    }

    fn write_bytes(&self, bytes: &[u8], priority: bool) -> Result<(), StateError> {
        let limit = if priority {
            self.options.max_state_bytes
        } else {
            self.options
                .max_state_bytes
                .saturating_sub(self.layout.priority_reserve_bytes)
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(StateError::StateTooLarge { limit_bytes: limit });
        }
        self.write_file(self.layout.snapshot, bytes)
    }

    /// Append `bytes` to the append-only file `name` beside the snapshot and
    /// sync it, creating it owner-only. `committed` is the length the
    /// snapshot records for the file: bytes past it are the remains of an
    /// append whose snapshot commit never happened, and are cut off first.
    /// Call inside [`Self::transact`], so appends are serialized by the store
    /// lock and `committed` cannot change underneath.
    ///
    /// Type, link count, and mode are checked on the opened descriptor, which
    /// is the one written to, so the path cannot be swapped after the check.
    ///
    /// # Errors
    /// A symlink, non-regular, or hard-linked file is
    /// [`StateError::RedirectedPath`]; a nonprivate one in a private store is
    /// [`StateError::PublicPath`]; a file shorter than `committed` is
    /// [`Corruption::TruncatedAppend`]. Nothing is cut off or appended in
    /// those cases, though a missing file is created empty.
    pub(crate) fn append_private(
        &self,
        name: &str,
        committed: u64,
        bytes: &[u8],
    ) -> Result<(), StateError> {
        self.append_with(name, committed, bytes, |file, bytes| file.write_all(bytes))
    }

    /// [`Self::append_private`] with the write step injected, so tests can
    /// fail it partway or act between the checks and the write.
    fn append_with(
        &self,
        name: &str,
        committed: u64,
        bytes: &[u8],
        write: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), StateError> {
        let io = |error| StateError::io(StorageOperation::Write, error);
        let mut file = open_append_file(&self.dir.join(name))?;
        let metadata = file.metadata().map_err(io)?;
        if !metadata.is_file() || links(&metadata) != 1 {
            return Err(StateError::RedirectedPath);
        }
        if self.layout.require_private && is_public(&metadata) {
            return Err(StateError::PublicPath);
        }
        match metadata.len().cmp(&committed) {
            Ordering::Less => return Err(StateError::CorruptState(Corruption::TruncatedAppend)),
            Ordering::Greater => file.set_len(committed).map_err(io)?,
            Ordering::Equal => {}
        }
        write(&mut file, bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        if committed == 0 {
            // The file may be new; make its directory entry durable too.
            sync_dir(&self.dir).map_err(io)?;
        }
        Ok(())
    }

    /// Atomically replace `name` with `bytes` through the temporary file.
    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<(), StateError> {
        let io = |error| StateError::io(StorageOperation::Write, error);
        let temp = self.dir.join(self.layout.temporary);
        let mut file = create_private_file(&temp).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        fs::rename(&temp, self.dir.join(name)).map_err(io)?;
        sync_dir(&self.dir).map_err(io)?;
        Ok(())
    }
}

/// A priority writer's exclusive lock on the persistent intent file. Ordinary
/// lockers see it held and wait; dropping the handle releases it.
struct PriorityIntent {
    _lock: File,
}

impl PriorityIntent {
    /// Lock the intent file, creating it like the lock file if needed and
    /// waiting for another priority writer within `timeout`.
    fn acquire(dir: &Path, name: &str, timeout: Duration) -> Result<Self, StateError> {
        let lock = |error| StateError::io(StorageOperation::Lock, error);
        let path = dir.join(name);
        refuse_redirected(&path)?;
        let file = open_lock_file(&path).map_err(lock)?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _lock: file }),
                // Another priority writer, or an ordinary locker's brief probe.
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(lock(error)),
            }
            let waited = started.elapsed();
            if waited >= timeout {
                return Err(StateError::LockTimeout {
                    waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                });
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn syntax(error: serde_json::Error) -> StateError {
    StateError::CorruptState(Corruption::Syntax {
        line: error.line(),
        column: error.column(),
    })
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Open `path` for appending without following a final symlink, creating
/// it owner-only. Non-blocking, so a FIFO placed there cannot stall the open.
#[cfg(unix)]
fn open_append_file(path: &Path) -> Result<File, StateError> {
    use rustix::{
        fs::{Mode, OFlags, open},
        io::Errno,
    };
    let flags = OFlags::WRONLY
        | OFlags::APPEND
        | OFlags::CREATE
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | OFlags::NOCTTY
        | OFlags::CLOEXEC;
    match open(path, flags, Mode::RUSR | Mode::WUSR) {
        Ok(descriptor) => Ok(File::from(descriptor)),
        // A final symlink, or a FIFO with no reader.
        Err(Errno::LOOP | Errno::NXIO) => Err(StateError::RedirectedPath),
        Err(error) => Err(StateError::io(
            StorageOperation::Write,
            std::io::Error::from(error),
        )),
    }
}

#[cfg(not(unix))]
fn open_append_file(path: &Path) -> Result<File, StateError> {
    refuse_redirected(path)?;
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|error| StateError::io(StorageOperation::Write, error))
}

#[cfg(unix)]
fn links(metadata: &fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::nlink(metadata)
}

/// Link counts are not available on this platform; treat the file as unshared.
#[cfg(not(unix))]
fn links(_metadata: &fs::Metadata) -> u64 {
    1
}

#[cfg(unix)]
fn is_public(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 != 0
}

#[cfg(not(unix))]
fn is_public(_metadata: &fs::Metadata) -> bool {
    false
}

/// Create the store directory (and missing parents) readable only by the owner.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

/// Create or truncate a state file readable only by the owner.
#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    File::create(path)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    // Directory handles cannot be synced on this platform; rename is still atomic.
    Ok(())
}

fn exists(path: &Path) -> Result<bool, StateError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StateError::io(StorageOperation::Prepare, error)),
    }
}

/// Refuse a store directory that is itself a symlink.
fn refuse_symlink(dir: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StateError::RedirectedPath),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::io(StorageOperation::Prepare, error)),
    }
}

/// Refuse a managed file that exists but is not a regular file, such as a
/// symlink redirecting writes elsewhere.
fn refuse_redirected(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(StateError::RedirectedPath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::io(StorageOperation::Prepare, error)),
    }
}

/// Refuse a path that other users can read or write.
#[cfg(unix)]
fn require_private(path: &Path) -> Result<(), StateError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| StateError::io(StorageOperation::Prepare, error))?;
    if is_public(&metadata) {
        return Err(StateError::PublicPath);
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_private(_path: &Path) -> Result<(), StateError> {
    Ok(())
}

fn fresh_nonce() -> u64 {
    // RandomState is seeded from OS randomness per process; mixing in the
    // time distinguishes stores created in the same process.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    RandomState::new().hash_one((std::process::id(), now))
}

/// Whether `path`, or the directory it would be created in, is inside a Git
/// checkout. Symbolic links in the existing part of `path` are resolved first,
/// so a link cannot hide a checkout. Runtime storage refuses such locations.
///
/// # Errors
/// Returns an I/O error when no ancestor of `path` can be resolved.
pub(crate) fn inside_repository(path: &Path) -> std::io::Result<bool> {
    let absolute = std::path::absolute(path)?;
    let mut existing = absolute.as_path();
    let canonical = loop {
        match fs::canonicalize(existing) {
            Ok(canonical) => break canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    };
    Ok(canonical
        .ancestors()
        .any(|ancestor| ancestor.join(".git").symlink_metadata().is_ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[derive(Debug, Serialize, Deserialize)]
    struct Toy {
        schema: u64,
        house: HouseId,
        nonce: u64,
        items: Vec<u32>,
    }

    impl Snapshot for Toy {
        const SCHEMA: u64 = 7;
        type Error = crate::Error;

        fn empty(house: HouseId, nonce: u64) -> Self {
            Self {
                schema: Self::SCHEMA,
                house,
                nonce,
                items: Vec::new(),
            }
        }

        fn nonce(&self) -> u64 {
            self.nonce
        }

        fn validate(&self, _house: &HouseId) -> Result<(), crate::Error> {
            Ok(())
        }
    }

    const RESERVE: u64 = 16;
    const LAYOUT: StoreLayout = StoreLayout {
        marker: "store.json",
        snapshot: "toy.json",
        temporary: "toy.tmp",
        lock: "toy.lock",
        pretty: false,
        require_private: true,
        priority_intent: Some("toy.priority"),
        priority_reserve_bytes: RESERVE,
    };

    fn house() -> std::result::Result<HouseId, crate::IdentifierError> {
        HouseId::new("example")
    }

    /// A store whose ordinary writes may reach exactly the encoded size of
    /// `fits` and no more.
    fn bounded(
        dir: &Path,
        fits: &[u32],
    ) -> std::result::Result<SnapshotStore<Toy>, Box<dyn std::error::Error>> {
        let store =
            SnapshotStore::<Toy>::initialize(dir, house()?, StoreOptions::default(), LAYOUT)?;
        let nonce = store.nonce;
        let at_limit = serde_json::to_vec(&Toy {
            items: fits.to_vec(),
            ..Toy::empty(house()?, nonce)
        })?;
        let options = StoreOptions {
            max_state_bytes: u64::try_from(at_limit.len())? + RESERVE,
            ..StoreOptions::default()
        };
        Ok(SnapshotStore::open(dir, house()?, options, LAYOUT)?)
    }

    #[test]
    fn ordinary_writes_stop_at_the_reserve_and_priority_writes_use_it() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[1, 2, 3])?;
        store.transact(|toy| {
            toy.items.extend([1, 2, 3]);
            Ok(())
        })?;
        let refused = store.transact(|toy| {
            toy.items.push(4);
            Ok(())
        });
        assert!(matches!(
            refused,
            Err(crate::Error::State(StateError::StateTooLarge { .. }))
        ));
        assert_eq!(store.read(|toy| toy.items.clone())?, [1, 2, 3]);
        store.transact_priority(|toy| {
            toy.items.push(4);
            Ok(())
        })?;
        assert_eq!(store.read(|toy| toy.items.clone())?, [1, 2, 3, 4]);
        // The intent stays in place, released, and private.
        assert!(!store.priority_pending()?);
        require_private(&path.join("toy.priority"))?;
        Ok(())
    }

    #[test]
    fn a_failed_transaction_writes_nothing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[1])?;
        let before = fs::read(path.join("toy.json"))?;
        let failed: Result<(), crate::Error> = store.transact(|toy| {
            toy.items.push(9);
            Err(StateError::StateMissing.into())
        });
        assert!(failed.is_err());
        assert_eq!(fs::read(path.join("toy.json"))?, before);
        Ok(())
    }

    #[test]
    fn a_released_priority_intent_does_not_block_ordinary_writers() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[1, 2])?;
        // The intent file a finished or crashed priority writer leaves behind.
        drop(PriorityIntent::acquire(
            &store.dir,
            "toy.priority",
            Duration::from_secs(1),
        )?);
        store.transact(|toy| {
            toy.items.push(1);
            Ok(())
        })?;
        assert!(path.join("toy.priority").exists());
        assert_eq!(store.read(|toy| toy.items.clone())?, [1]);
        Ok(())
    }

    /// Lockers never unlink or replace the intent, so an ordinary locker
    /// always probes the file a later priority writer locks.
    #[cfg(unix)]
    #[test]
    fn the_intent_file_is_never_replaced() -> TestResult {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[1, 2])?;
        let intent = path.join("toy.priority");
        store.transact_priority(|toy| {
            toy.items.push(1);
            Ok(())
        })?;
        let inode = fs::metadata(&intent)?.ino();
        store.transact(|toy| {
            toy.items.push(2);
            Ok(())
        })?;
        store.read(|toy| toy.items.len())?;
        store.transact_priority(|toy| {
            toy.items.clear();
            Ok(())
        })?;
        assert_eq!(fs::metadata(&intent)?.ino(), inode);
        let held = PriorityIntent::acquire(&store.dir, "toy.priority", Duration::from_secs(1))?;
        assert!(store.priority_pending()?);
        // A probe that finds the intent held leaves it held.
        assert!(store.priority_pending()?);
        drop(held);
        assert!(!store.priority_pending()?);
        assert_eq!(fs::metadata(&intent)?.ino(), inode);
        Ok(())
    }

    #[test]
    fn a_held_priority_intent_makes_another_priority_writer_time_out() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        bounded(&path, &[])?;
        let _held = PriorityIntent::acquire(&path, "toy.priority", Duration::from_secs(1))?;
        assert!(matches!(
            PriorityIntent::acquire(&path, "toy.priority", Duration::from_millis(30)),
            Err(StateError::LockTimeout { .. })
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_redirected_or_public_intent_is_refused() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[])?;
        let intent = path.join("toy.priority");
        let elsewhere = dir.path().join("elsewhere");
        fs::write(&elsewhere, b"")?;
        std::os::unix::fs::symlink(&elsewhere, &intent)?;
        assert!(matches!(
            store.read(|toy| toy.items.len()),
            Err(crate::Error::State(StateError::RedirectedPath))
        ));
        assert!(matches!(
            store.transact_priority(|_| Ok(())),
            Err(crate::Error::State(StateError::RedirectedPath))
        ));
        fs::remove_file(&intent)?;
        store.transact_priority(|_| Ok(()))?;
        fs::set_permissions(&intent, fs::Permissions::from_mode(0o644))?;
        assert!(matches!(
            store.read(|toy| toy.items.len()),
            Err(crate::Error::State(StateError::PublicPath))
        ));
        Ok(())
    }

    #[test]
    fn a_partial_append_is_cut_off_before_the_retry() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[])?;
        store.append_private("log", 0, b"one\n")?;
        // The write stops partway, as a full disk or a crash would leave it.
        let failed = store.append_with("log", 4, b"two\n", |file, bytes| {
            file.write_all(bytes.get(..2).unwrap_or_default())?;
            Err(std::io::Error::other("injected write failure"))
        });
        assert!(matches!(
            failed,
            Err(StateError::Io {
                operation: StorageOperation::Write,
                ..
            })
        ));
        assert_eq!(fs::read(path.join("log"))?, b"one\ntw");
        store.append_private("log", 4, b"two\n")?;
        assert_eq!(fs::read(path.join("log"))?, b"one\ntwo\n");
        // A whole line the snapshot never committed is cut off the same way.
        store.append_private("log", 4, b"three\n")?;
        assert_eq!(fs::read(path.join("log"))?, b"one\nthree\n");
        Ok(())
    }

    #[test]
    fn an_append_file_shorter_than_committed_is_refused_unchanged() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[])?;
        store.append_private("log", 0, b"one\n")?;
        assert!(matches!(
            store.append_private("log", 5, b"two\n"),
            Err(StateError::CorruptState(Corruption::TruncatedAppend))
        ));
        assert_eq!(fs::read(path.join("log"))?, b"one\n");
        Ok(())
    }

    /// The checks read the opened descriptor, and the write goes to that
    /// descriptor, so a file swapped in after the checks never receives it.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_in_after_the_checks_is_not_written() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[])?;
        let other = dir.path().join("other");
        fs::write(&other, b"other\n")?;
        let log = path.join("log");
        store.append_with("log", 0, b"one\n", |file, bytes| {
            fs::remove_file(&log)?;
            fs::hard_link(&other, &log)?;
            file.write_all(bytes)
        })?;
        assert_eq!(fs::read(&other)?, b"other\n");
        // The swapped-in file now has two links, so the next append refuses it.
        assert!(matches!(
            store.append_private("log", 0, b"two\n"),
            Err(StateError::RedirectedPath)
        ));
        assert_eq!(fs::read(&other)?, b"other\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_append_file_is_refused_without_blocking() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = bounded(&path, &[])?;
        let made = std::process::Command::new("mkfifo")
            .arg(path.join("log"))
            .status()?;
        assert!(made.success());
        assert!(matches!(
            store.append_private("log", 0, b"one\n"),
            Err(StateError::RedirectedPath)
        ));
        Ok(())
    }

    #[test]
    fn a_held_priority_intent_makes_ordinary_lockers_time_out() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("store");
        let store = SnapshotStore::<Toy>::open(
            {
                bounded(&path, &[])?;
                &path
            },
            house()?,
            StoreOptions {
                lock_timeout: Duration::from_millis(30),
                ..StoreOptions::default()
            },
            LAYOUT,
        )?;
        let _held = PriorityIntent::acquire(&store.dir, "toy.priority", Duration::from_secs(1))?;
        assert!(matches!(
            store.read(|toy| toy.items.len()),
            Err(crate::Error::State(StateError::LockTimeout { .. }))
        ));
        Ok(())
    }
}
