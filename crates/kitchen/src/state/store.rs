//! The file-backed, house-scoped store.
//!
//! Layout inside the caller-supplied directory:
//!
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

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    hash::{BuildHasher, RandomState},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    ConsumerId, Error, HolderId, HouseId, TaskId,
    contracts::{
        AttemptOutcome, AttemptStart, ContractError, Disposition, EffectSeq, Evidence,
        EvidenceRevision, ExternalRef, Fence, HouseGrants, LeaseTtl, TaskSpec, Timestamp,
    },
    state::{
        CancelStatus, Consumption, Corruption, Creation, EffectOutcome, EffectPlan, EffectRecord,
        EffectStart, Lease, RecoveryItem, Resubmission, StateError, StorageOperation, TaskRecord,
        model::{SCHEMA_VERSION, SchemaProbe, StoreState},
    },
};

type Result<T> = std::result::Result<T, Error>;

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
    options: StoreOptions,
}

impl HouseStore {
    /// Open or initialize the store for `house` in `dir`, creating the directory.
    ///
    /// # Errors
    /// Returns [`StateError::StorageInsideRepository`] when `dir` is inside a
    /// Git checkout, [`ContractError::CrossHouse`] when the directory belongs to
    /// another house, and storage or corruption errors for unreadable state.
    pub fn open(dir: impl AsRef<Path>, house: HouseId, options: StoreOptions) -> Result<Self> {
        let dir = dir.as_ref();
        create_private_dir(dir)
            .map_err(|error| StateError::io(StorageOperation::Prepare, error))?;
        let dir = fs::canonicalize(dir)
            .map_err(|error| StateError::io(StorageOperation::Prepare, error))?;
        if dir
            .ancestors()
            .any(|ancestor| ancestor.join(".git").symlink_metadata().is_ok())
        {
            return Err(StateError::StorageInsideRepository.into());
        }
        let store = Self {
            dir,
            house,
            options,
        };
        let _lock = store.lock(true)?;
        if store.load()?.is_none() {
            store.write(&StoreState::new(store.house.clone(), fresh_nonce()))?;
        }
        Ok(store)
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

    /// Finish the running attempt. Repeating the same report is a no-op.
    ///
    /// # Errors
    /// Refuses while effects are unresolved and when the report contradicts
    /// an earlier one.
    pub fn finish_attempt(
        &self,
        id: &TaskId,
        fence: Fence,
        outcome: AttemptOutcome,
        now: Timestamp,
    ) -> Result<Disposition> {
        self.transact(|state| state.finish_attempt(id, fence, outcome, now))
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

    /// Persist the intent for one effect before it is executed.
    ///
    /// Checks, in one transaction: the grants' house, live ownership, no
    /// pending cancellation, a running attempt, the decision's evidence
    /// revision, task authority against the house's current grants, and that
    /// no other effect is unresolved.
    ///
    /// # Errors
    /// Returns the first failed check.
    pub fn begin_effect(
        &self,
        plan: EffectPlan,
        grants: &HouseGrants,
        resubmission: Resubmission,
        now: Timestamp,
    ) -> Result<EffectStart> {
        self.transact(|state| state.begin_effect(plan, grants, resubmission, now))
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
        let io = |error| StateError::io(StorageOperation::Write, error);
        let temp = self.dir.join(TEMP_FILE);
        let mut file = create_private_file(&temp).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        fs::rename(&temp, self.dir.join(STATE_FILE)).map_err(io)?;
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

fn fresh_nonce() -> u64 {
    // RandomState is seeded from OS randomness per process; mixing in the
    // time distinguishes stores created in the same process.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    RandomState::new().hash_one((std::process::id(), now))
}
