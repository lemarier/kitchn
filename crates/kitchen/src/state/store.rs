//! The file-backed, house-scoped store.
//!
//! Layout inside the caller-supplied directory:
//!
//! - `store.json`: the initialization marker naming the house and the store's
//!   random nonce. Written once by [`HouseStore::initialize`]; its presence
//!   means the store is established, so a missing snapshot is an error.
//! - `state.lock`: an advisory lock file. Writers take an exclusive lock and
//!   readers a shared one, with a bounded wait.
//! - `state.json`: the committed snapshot, replaced atomically by writing
//!   `state.json.tmp`, syncing it, renaming it over the snapshot, and syncing
//!   the directory. A crash leaves either the old or the new snapshot; a stale
//!   temporary file is ignored and overwritten by the next write.
//!
//! Each call is one transaction: lock, read and validate, apply one
//! transition to a copy, write if it changed, unlock. The lock is never held
//! across backend calls. On Unix, newly created directories and files are
//! readable only by their owner because the state holds private briefs.
//!
//! Trust assumptions: the directory is private to the Kitchen user. The store
//! refuses a symlinked store directory and managed files that are symlinks or
//! other non-regular files, checked before every transaction; it cannot
//! prevent a process running as the same user from racing those checks.
//! Snapshot storage suits a house's working set (see [`crate::state::MAX_TASKS`]);
//! every write rewrites the whole snapshot.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    hash::{BuildHasher, RandomState},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, Error, HolderId, HouseId, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, BackendDescriptor, ContractError, Disposition,
        EffectSeq, Evidence, EvidenceRevision, ExternalRef, Fence, HouseGrants, LeaseTtl, TaskSpec,
        Timestamp,
    },
    state::{
        CancelStatus, Consumption, Corruption, Creation, EffectOutcome, EffectPlan, EffectRecord,
        EffectStart, Lease, RecoveryItem, RiskDecision, StateError, StorageOperation, TaskRecord,
        model::{SCHEMA_VERSION, SchemaProbe, StoreState},
    },
};

type Result<T> = std::result::Result<T, Error>;

const MARKER_FILE: &str = "store.json";
const STATE_FILE: &str = "state.json";
const TEMP_FILE: &str = "state.json.tmp";
const LOCK_FILE: &str = "state.lock";
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

/// Durable, house-scoped task ownership state.
///
/// Handles are cheap and hold no open files; separate handles and processes
/// coordinate through the lock file. The directory must be house-scoped
/// runtime storage outside any Git checkout.
#[derive(Debug, Clone)]
pub struct HouseStore {
    dir: PathBuf,
    house: HouseId,
    nonce: u64,
    options: StoreOptions,
}

/// The initialization marker. It never changes after initialization.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoreMarker {
    schema: u64,
    house: HouseId,
    nonce: u64,
}

impl HouseStore {
    /// Create a new, empty store for `house` in `dir`, creating the directory.
    ///
    /// # Errors
    /// Returns [`StateError::AlreadyInitialized`] when `dir` already holds a
    /// store or a snapshot, [`StateError::StorageInsideRepository`] inside a
    /// Git checkout, [`StateError::RedirectedPath`] for a symlinked directory
    /// or managed file, and storage errors.
    pub fn initialize(
        dir: impl AsRef<Path>,
        house: HouseId,
        options: StoreOptions,
    ) -> Result<Self> {
        let dir = dir.as_ref();
        refuse_symlink(dir)?;
        create_private_dir(dir)
            .map_err(|error| StateError::io(StorageOperation::Prepare, error))?;
        let mut store = Self::at(dir, house, 0, options)?;
        let _lock = store.lock(true)?;
        if exists(&store.dir.join(MARKER_FILE))? || exists(&store.dir.join(STATE_FILE))? {
            return Err(StateError::AlreadyInitialized.into());
        }
        store.nonce = fresh_nonce();
        let marker = StoreMarker {
            schema: SCHEMA_VERSION,
            house: store.house.clone(),
            nonce: store.nonce,
        };
        let marker = serde_json::to_vec_pretty(&marker)
            .map_err(|error| StateError::io(StorageOperation::Write, error.into()))?;
        // The marker goes first: if a crash follows, the store is
        // established without a snapshot and fails closed on open.
        store.write_file(MARKER_FILE, &marker)?;
        store.write(&StoreState::new(store.house.clone(), store.nonce))?;
        Ok(store)
    }

    /// Open the established store for `house` in `dir`.
    ///
    /// # Errors
    /// Returns [`StateError::NotInitialized`] when `dir` holds no store,
    /// [`StateError::StateMissing`] when an established store lost its
    /// snapshot, [`ContractError::CrossHouse`] when it belongs to another
    /// house, [`StateError::RedirectedPath`] for symlinked paths, and storage
    /// or corruption errors. It never writes a replacement snapshot.
    pub fn open(dir: impl AsRef<Path>, house: HouseId, options: StoreOptions) -> Result<Self> {
        let dir = dir.as_ref();
        refuse_symlink(dir)?;
        let mut store = Self::at(dir, house, 0, options)?;
        let _lock = store.lock(false)?;
        let marker = store.read_marker()?;
        store.nonce = marker.nonce;
        store.load()?.ok_or(StateError::StateMissing)?;
        Ok(store)
    }

    fn at(dir: &Path, house: HouseId, nonce: u64, options: StoreOptions) -> Result<Self> {
        let dir = fs::canonicalize(dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::State(StateError::NotInitialized)
            } else {
                StateError::io(StorageOperation::Prepare, error).into()
            }
        })?;
        if dir
            .ancestors()
            .any(|ancestor| ancestor.join(".git").symlink_metadata().is_ok())
        {
            return Err(StateError::StorageInsideRepository.into());
        }
        Ok(Self {
            dir,
            house,
            nonce,
            options,
        })
    }

    fn read_marker(&self) -> Result<StoreMarker> {
        let path = self.dir.join(MARKER_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(if exists(&self.dir.join(STATE_FILE))? {
                    StateError::CorruptState(Corruption::Marker).into()
                } else {
                    StateError::NotInitialized.into()
                });
            }
            Err(error) => return Err(StateError::io(StorageOperation::Read, error).into()),
        };
        let marker: StoreMarker = serde_json::from_slice(&bytes)
            .map_err(|_| StateError::CorruptState(Corruption::Marker))?;
        if marker.schema != SCHEMA_VERSION {
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

    /// The house this store serves.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Create a task. Repeating an identical creation is a no-op.
    ///
    /// # Errors
    /// Rejects a foreign-house authority, a reused id with a different
    /// specification, and a full store.
    pub fn create_task(&self, spec: TaskSpec, now: Timestamp) -> Result<Creation> {
        self.transact(|state| state.create_task(spec, now))
    }

    /// Read one task.
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotFound`] or a storage error.
    pub fn task(&self, id: &TaskId) -> Result<TaskRecord> {
        self.read(|state| state.task(id).cloned())?
    }

    /// Read every task, ordered by id.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn tasks(&self) -> Result<Vec<TaskRecord>> {
        self.read(|state| state.tasks().cloned().collect())
    }

    /// Claim an open task. A live or expired claim by anyone, including the
    /// same holder id, is refused; an expired claim needs [`Self::take_over`].
    ///
    /// # Errors
    /// Returns [`StateError::ClaimHeld`], [`StateError::LeaseExpired`], or
    /// [`StateError::TaskSettled`].
    pub fn claim(
        &self,
        id: &TaskId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.claim(id, holder, ttl, now))
    }

    /// Extend a live claim.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] or [`StateError::LeaseExpired`].
    pub fn renew(&self, id: &TaskId, fence: Fence, ttl: LeaseTtl, now: Timestamp) -> Result<Lease> {
        self.transact(|state| state.renew(id, fence, ttl, now))
    }

    /// Give a claim back, interrupting a running attempt. Unresolved effects
    /// stay recorded for the next owner to reconcile.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] when `fence` no longer owns the task.
    pub fn relinquish(&self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<()> {
        self.transact(|state| state.relinquish(id, fence, now))
    }

    /// Explicitly take over an expired claim with a new, larger fence. The
    /// previous owner's running attempt becomes interrupted and its fence stale.
    ///
    /// # Errors
    /// Returns [`StateError::LeaseLive`] while the current lease is live.
    pub fn take_over(
        &self,
        id: &TaskId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.take_over(id, holder, ttl, now))
    }

    /// Start an attempt, or report the running one. Settles the task as
    /// exhausted when the retry budget is spent.
    ///
    /// # Errors
    /// Refuses without a live claim, after a cancellation request, and while
    /// effects are unresolved.
    pub fn start_attempt(&self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<AttemptStart> {
        self.transact(|state| state.start_attempt(id, fence, now))
    }

    /// Finish `attempt`, which must be the running attempt. Repeating the
    /// same report for any earlier attempt replays that attempt's result and
    /// changes nothing.
    ///
    /// # Errors
    /// Refuses an unknown or interrupted attempt, unresolved effects, and a
    /// report that contradicts the recorded one.
    pub fn finish_attempt(
        &self,
        id: &TaskId,
        fence: Fence,
        attempt: AttemptNumber,
        outcome: AttemptOutcome,
        now: Timestamp,
    ) -> Result<Disposition> {
        self.transact(|state| state.finish_attempt(id, fence, attempt, outcome, now))
    }

    /// Request cancellation. Needs no claim; an open task without unresolved
    /// effects settles immediately, otherwise its owner must settle it.
    ///
    /// # Errors
    /// Returns [`StateError::TaskNotFound`] or a storage error.
    pub fn request_cancel(
        &self,
        id: &TaskId,
        requested_by: &HolderId,
        now: Timestamp,
    ) -> Result<CancelStatus> {
        self.transact(|state| state.request_cancel(id, requested_by, now))
    }

    /// Settle the owned task as cancelled after its effects are resolved.
    /// Cancellation does not roll back applied effects.
    ///
    /// # Errors
    /// Refuses a stale fence and unresolved effects.
    pub fn settle_cancelled(&self, id: &TaskId, fence: Fence, now: Timestamp) -> Result<()> {
        self.transact(|state| state.settle_cancelled(id, fence, now))
    }

    /// Persist the intent for one effect on `backend` before it is executed.
    ///
    /// Checks, in one transaction: the grants' and backend's house, the
    /// backend's capabilities, live ownership, no pending cancellation, a
    /// running attempt, the decision's evidence revision, task authority
    /// against the house's current grants, and that no other effect is
    /// unresolved. The intent records the backend namespace; a repeated
    /// request for the same logical effect must come from that backend.
    /// An uncertain effect is resubmitted with its key only when the backend
    /// declares [`crate::contracts::Capability::EffectIdempotentRequests`].
    ///
    /// # Errors
    /// Returns the first failed check.
    pub fn begin_effect(
        &self,
        plan: EffectPlan,
        grants: &HouseGrants,
        backend: &BackendDescriptor,
        now: Timestamp,
    ) -> Result<EffectStart> {
        self.transact(|state| state.begin_effect(plan, grants, backend, now))
    }

    /// Record what is known about an effect. Only the current fence may
    /// record; repeating a consistent report is a no-op.
    ///
    /// # Errors
    /// Returns [`StateError::ConflictingOutcome`] when the report contradicts
    /// a resolved outcome.
    pub fn record_effect_outcome(
        &self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        outcome: EffectOutcome,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| state.record_effect_outcome(id, fence, seq, outcome, now))
    }

    /// Record what the backend returned for submission number `submission`
    /// of an effect (see [`EffectRecord::submissions`]). A not-applied or
    /// uncertain result from an older submission is ignored, since a newer
    /// submission may still apply; a receipt is accepted from any submission.
    ///
    /// # Errors
    /// As [`Self::record_effect_outcome`].
    pub fn record_submission_outcome(
        &self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        submission: u32,
        outcome: EffectOutcome,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| {
            state.record_submission_outcome(id, fence, seq, submission, outcome, now)
        })
    }

    /// Record a scoped decision about a handed-over effect (one recorded as
    /// [`EffectOutcome::Unresolvable`]). The decision must name the effect's
    /// idempotency key and the task's current evidence revision. Repeating
    /// the same decision is a no-op.
    ///
    /// # Errors
    /// Returns [`StateError::DecisionScope`] for another effect,
    /// [`StateError::StaleDecision`] for an older revision, and
    /// [`StateError::NotHandedOver`] unless the effect is handed over.
    pub fn accept_risk(
        &self,
        id: &TaskId,
        fence: Fence,
        seq: EffectSeq,
        decision: RiskDecision,
        now: Timestamp,
    ) -> Result<EffectRecord> {
        self.transact(|state| state.accept_risk(id, fence, seq, decision, now))
    }

    /// Record evidence. A new subject revision starts a new evidence revision
    /// and drops superseded evidence, invalidating decisions made earlier.
    ///
    /// # Errors
    /// Refuses a stale fence and a full evidence log.
    pub fn record_evidence(
        &self,
        id: &TaskId,
        fence: Fence,
        evidence: Evidence,
        now: Timestamp,
    ) -> Result<EvidenceRevision> {
        self.transact(|state| state.record_evidence(id, fence, evidence, now))
    }

    /// Mark an inbound message as consumed, reporting duplicates.
    ///
    /// # Errors
    /// Refuses without live ownership and when the bounded set is full.
    pub fn consume_message(
        &self,
        id: &TaskId,
        fence: Fence,
        message: &ExternalRef,
        now: Timestamp,
    ) -> Result<Consumption> {
        self.transact(|state| state.consume_message(id, fence, message, now))
    }

    /// Acquire the single-consumer lease for a workflow scope.
    ///
    /// # Errors
    /// Returns [`StateError::ClaimHeld`] while another lease is live and
    /// [`StateError::LeaseExpired`] when an expired lease needs a takeover.
    pub fn acquire_consumer(
        &self,
        consumer: &ConsumerId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.acquire_consumer(consumer, holder, ttl, now))
    }

    /// Extend a live consumer lease.
    ///
    /// # Errors
    /// Refuses a missing lease, a stale fence, and an expired lease.
    pub fn renew_consumer(
        &self,
        consumer: &ConsumerId,
        fence: Fence,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.renew_consumer(consumer, fence, ttl, now))
    }

    /// Release a consumer lease. Releasing an absent lease is a no-op.
    ///
    /// # Errors
    /// Returns [`StateError::StaleFence`] when another fence holds the lease.
    pub fn release_consumer(&self, consumer: &ConsumerId, fence: Fence) -> Result<()> {
        self.transact(|state| state.release_consumer(consumer, fence))
    }

    /// Explicitly take over an expired consumer lease.
    ///
    /// # Errors
    /// Returns [`StateError::LeaseLive`] while the lease is live.
    pub fn take_over_consumer(
        &self,
        consumer: &ConsumerId,
        holder: &HolderId,
        ttl: LeaseTtl,
        now: Timestamp,
    ) -> Result<Lease> {
        self.transact(|state| state.take_over_consumer(consumer, holder, ttl, now))
    }

    /// Read a consumer lease.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn consumer(&self, consumer: &ConsumerId) -> Result<Option<Lease>> {
        self.read(|state| state.consumer(consumer).cloned())
    }

    /// Work that needs an explicit recovery decision: expired owners,
    /// unowned unresolved effects, and unowned pending cancellations.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn recovery_queue(&self, now: Timestamp) -> Result<Vec<RecoveryItem>> {
        self.read(|state| state.recovery_queue(now))
    }

    fn transact<T>(&self, apply: impl FnOnce(&mut StoreState) -> Result<T>) -> Result<T> {
        let _lock = self.lock(true)?;
        let (mut state, before) = self.load()?.ok_or(StateError::StateMissing)?;
        let value = apply(&mut state)?;
        let after = serialize(&state)?;
        if after != before {
            self.write_bytes(&after)?;
        }
        Ok(value)
    }

    fn read<T>(&self, view: impl FnOnce(&StoreState) -> T) -> Result<T> {
        let _lock = self.lock(false)?;
        let (state, _) = self.load()?.ok_or(StateError::StateMissing)?;
        Ok(view(&state))
    }

    fn lock(&self, exclusive: bool) -> Result<File> {
        for name in [MARKER_FILE, LOCK_FILE, STATE_FILE, TEMP_FILE] {
            refuse_redirected(&self.dir.join(name))?;
        }
        let file = open_lock_file(&self.dir.join(LOCK_FILE))
            .map_err(|error| StateError::io(StorageOperation::Lock, error))?;
        let started = Instant::now();
        let mut backoff = Duration::from_millis(2);
        loop {
            let attempt = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match attempt {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    return Err(StateError::io(StorageOperation::Lock, error).into());
                }
            }
            let waited = started.elapsed();
            let Some(remaining) = self
                .options
                .lock_timeout
                .checked_sub(waited)
                .filter(|left| !left.is_zero())
            else {
                return Err(StateError::LockTimeout {
                    waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                }
                .into());
            };
            thread::sleep(backoff.min(remaining));
            backoff = backoff.saturating_mul(2).min(MAX_LOCK_BACKOFF);
        }
    }

    fn load(&self) -> Result<Option<(StoreState, Vec<u8>)>> {
        let file = match File::open(self.dir.join(STATE_FILE)) {
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

    fn parse(&self, bytes: &[u8]) -> Result<StoreState> {
        let probe: SchemaProbe = serde_json::from_slice(bytes).map_err(syntax)?;
        if probe.schema != SCHEMA_VERSION {
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
        let state: StoreState = serde_json::from_slice(bytes).map_err(syntax)?;
        state.validate().map_err(StateError::CorruptState)?;
        if state.nonce() != self.nonce {
            return Err(StateError::CorruptState(Corruption::StoreIdentity).into());
        }
        Ok(state)
    }

    fn write(&self, state: &StoreState) -> Result<()> {
        self.write_bytes(&serialize(state)?)
    }

    fn write_bytes(&self, bytes: &[u8]) -> Result<()> {
        let limit = self.options.max_state_bytes;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(StateError::StateTooLarge { limit_bytes: limit }.into());
        }
        self.write_file(STATE_FILE, bytes)
    }

    /// Atomically replace `name` with `bytes` through the temporary file.
    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let io = |error| StateError::io(StorageOperation::Write, error);
        let temp = self.dir.join(TEMP_FILE);
        let mut file = create_private_file(&temp).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        fs::rename(&temp, self.dir.join(name)).map_err(io)?;
        sync_dir(&self.dir).map_err(io)?;
        Ok(())
    }
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

fn serialize(state: &StoreState) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(state)
        .map_err(|error| StateError::io(StorageOperation::Write, error.into()).into())
}

fn syntax(error: serde_json::Error) -> Error {
    StateError::CorruptState(Corruption::Syntax {
        line: error.line(),
        column: error.column(),
    })
    .into()
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

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StateError::io(StorageOperation::Prepare, error).into()),
    }
}

/// Refuse a store directory that is itself a symlink.
fn refuse_symlink(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StateError::RedirectedPath.into()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::io(StorageOperation::Prepare, error).into()),
    }
}

/// Refuse a managed file that exists but is not a regular file, such as a
/// symlink redirecting writes elsewhere.
fn refuse_redirected(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(StateError::RedirectedPath.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::io(StorageOperation::Prepare, error).into()),
    }
}

fn fresh_nonce() -> u64 {
    // RandomState is seeded from OS randomness per process; mixing in the
    // time distinguishes stores created in the same process.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    RandomState::new().hash_one((std::process::id(), now))
}
