//! Bounded, locked snapshots. Private records never belong in Git.
use crate::{
    CredentialId, HouseId,
    contracts::{GrantScope, HouseGrants, Permission},
    state::TaskRecord,
    trust::{
        AutonomyGrant, GrantAudit, MAX_HISTORY, MAX_ITEMS, Observation, StationScope, TrustError,
    },
    workflows::inspector::Inspection,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    schema: u32,
    house: HouseId,
    pub(crate) observations: Vec<Observation>,
    grants: Vec<GrantAudit>,
    pub(crate) inspections: Vec<Inspection>,
}

impl Document {
    fn validate(&self, house: &HouseId) -> Result<(), TrustError> {
        if &self.house != house {
            return Err(TrustError::Refused);
        }
        if self.schema != 1
            || self.observations.len() + self.grants.len() + self.inspections.len() > MAX_HISTORY
        {
            return Err(TrustError::Corrupt);
        }
        for (index, observation) in self.observations.iter().enumerate() {
            observation.validate()?;
            if &observation.house != house {
                return Err(TrustError::Refused);
            }
            if self.observations[..index].iter().any(|old| {
                (old.id == observation.id
                    && (old.revision == observation.revision || old.task != observation.task))
                    || (old.task == observation.task && old.id != observation.id)
            }) {
                return Err(TrustError::Conflict);
            }
        }
        for (index, audit) in self.grants.iter().enumerate() {
            let (id, audit_house) = audit_identity(audit);
            if audit_house != house {
                return Err(TrustError::Refused);
            }
            if let GrantAudit::Issued(grant) = audit {
                validate_grant(grant)?;
                for (source, revision) in &grant.evidence {
                    let evidence = self
                        .observations
                        .iter()
                        .find(|o| &o.id == source && &o.revision == revision)
                        .ok_or(TrustError::Corrupt)?;
                    if !evidence.trust_eligible() || evidence.attribution.scope != grant.scope {
                        return Err(TrustError::Corrupt);
                    }
                }
                if self.grants[..index]
                    .iter()
                    .any(|old| audit_identity(old).0 == id)
                {
                    return Err(TrustError::Conflict);
                }
            }
        }
        for (index, inspection) in self.inspections.iter().enumerate() {
            inspection.validate(house)?;
            inspection.validate_observation(&self.observations)?;
            if self.inspections[..index]
                .iter()
                .any(|old| old.id() == inspection.id())
            {
                return Err(TrustError::Conflict);
            }
        }
        Ok(())
    }

    pub(crate) fn latest(
        &self,
        id: &crate::contracts::ExternalRef,
    ) -> Result<&Observation, TrustError> {
        let mut versions: Vec<_> = self.observations.iter().filter(|o| &o.id == id).collect();
        versions.sort_by_key(|o| o.revision);
        for (index, observation) in versions.iter().enumerate() {
            if observation.revision.get() as usize != index + 1 {
                return Err(TrustError::Incomplete);
            }
        }
        versions.last().copied().ok_or(TrustError::Incomplete)
    }
}

/// House-scoped runtime ledger. Each operation reloads under a bounded lock.
/// The containing directory is private to the process user; same-user hostile
/// filesystem races are outside this boundary, as with the core house store.
#[derive(Debug, Clone)]
pub struct Ledger {
    dir: PathBuf,
    house: HouseId,
}
impl Ledger {
    /// Initialize a new directory, never replacing existing files or partial setup.
    /// Parent must exist. Unix directories/files are created with 0700/0600 modes.
    ///
    /// # Errors
    /// Rejects existing paths, repositories, redirected or nonprivate storage.
    pub fn initialize(path: impl AsRef<Path>, house: HouseId) -> Result<Self, TrustError> {
        let path = path.as_ref();
        let parent = path.parent().ok_or(TrustError::UnsafePath)?;
        check_outside_repository(&fs::canonicalize(parent)?)?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
        let ledger = Self {
            dir: fs::canonicalize(path)?,
            house,
        };
        private_file(&ledger.dir.join("ledger.lock"), true)?;
        let document = Document {
            schema: 1,
            house: ledger.house.clone(),
            observations: Vec::new(),
            grants: Vec::new(),
            inspections: Vec::new(),
        };
        ledger.write(&document)?;
        Ok(ledger)
    }

    /// Open an established ledger; missing or invalid history is an error.
    ///
    /// # Errors
    /// Rejects cross-house, redirected, public, missing, or corrupted storage.
    pub fn open(path: impl AsRef<Path>, house: HouseId) -> Result<Self, TrustError> {
        check_path(path.as_ref(), true)?;
        let ledger = Self {
            dir: fs::canonicalize(path)?,
            house,
        };
        ledger.read(|_| Ok(()))?;
        Ok(ledger)
    }

    /// House selected when this handle was opened.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Append one immutable revision; identical delivery is a no-op. Reordered
    /// revisions are retained, but the projection stays incomplete until gaps close.
    ///
    /// # Errors
    /// Rejects inconsistent evidence, cross-house writes, and conflicting identities.
    pub fn record(&self, observation: Observation) -> Result<bool, TrustError> {
        self.transact(|doc| {
            observation.validate()?;
            if let Some(old) = doc
                .observations
                .iter()
                .find(|o| o.id == observation.id && o.revision == observation.revision)
            {
                return if old == &observation {
                    Ok(false)
                } else {
                    Err(TrustError::Conflict)
                };
            }
            doc.observations.push(observation);
            Ok(true)
        })
    }

    /// Latest reconciled revision. Use history to investigate an attribution change.
    ///
    /// # Errors
    /// Missing stream or revision gaps are explicitly incomplete.
    pub fn latest(&self, id: &crate::contracts::ExternalRef) -> Result<Observation, TrustError> {
        self.read(|doc| doc.latest(id).cloned())
    }

    /// All immutable observations, including corrections and pending predecessors.
    ///
    /// # Errors
    /// Returns bounded storage failures.
    pub fn history(&self) -> Result<Vec<Observation>, TrustError> {
        self.read(|doc| Ok(doc.observations.clone()))
    }

    /// Record an explicit evidence-backed restriction on existing standing grants.
    /// This never edits policy or creates a task authority from a trust score.
    ///
    /// # Errors
    /// Rejects privileged actions, stale evidence, scope expansion, and conflicts.
    pub fn grant(&self, grant: AutonomyGrant, current: &HouseGrants) -> Result<bool, TrustError> {
        if current.house() != &self.house || !current.covers(&grant.claim) {
            return Err(TrustError::Refused);
        }
        validate_grant(&grant)?;
        self.transact(|doc| {
            let audit = GrantAudit::Issued(grant.clone());
            if doc.grants.contains(&audit) {
                return Ok(false);
            }
            if doc.grants.iter().any(|a| audit_identity(a).0 == &grant.id) {
                return Err(TrustError::Conflict);
            }
            for (id, revision) in &grant.evidence {
                let evidence = doc.latest(id)?;
                if !evidence.trust_eligible()
                    || evidence.revision != *revision
                    || evidence.attribution.scope != grant.scope
                {
                    return Err(TrustError::Refused);
                }
            }
            doc.grants.push(audit);
            Ok(true)
        })
    }

    /// Append a revocation, even if delivery precedes issuance. Never resurrects.
    ///
    /// # Errors
    /// Rejects issuance passed as revocation and cross-house records.
    pub fn revoke(&self, revocation: GrantAudit) -> Result<bool, TrustError> {
        if !matches!(revocation, GrantAudit::Revoked { .. }) {
            return Err(TrustError::Invalid);
        }
        self.transact(|doc| {
            if doc.grants.contains(&revocation) {
                return Ok(false);
            }
            doc.grants.push(revocation);
            Ok(true)
        })
    }

    /// The immutable issuance/revocation trail.
    ///
    /// # Errors
    /// Returns storage failures.
    pub fn grant_history(&self) -> Result<Vec<GrantAudit>, TrustError> {
        self.read(|d| Ok(d.grants.clone()))
    }

    /// Recheck a scoped grant immediately before an action on a current task.
    /// Returns the core-selected credential, not a transferable authority token.
    /// Callers still use the core executor, which rechecks house policy. No API
    /// here persists interactive consent or authorizes merge/publication/equipment.
    ///
    /// # Errors
    /// Rejects revoked grants, corrected evidence, scope/provenance mismatches,
    /// and withdrawn core standing authority.
    pub fn authorize(
        &self,
        id: &crate::contracts::ExternalRef,
        scope: &StationScope,
        task: &TaskRecord,
        current: &HouseGrants,
    ) -> crate::Result<CredentialId> {
        let claim = self.read(|doc| {
            if current.house() != &self.house
                || task.spec().authority.house() != &self.house
                || task.spec().repository.as_ref() != Some(&scope.project)
            {
                return Err(TrustError::Refused);
            }
            if doc
                .grants
                .iter()
                .any(|a| matches!(a, GrantAudit::Revoked { id: revoked, .. } if revoked == id))
            {
                return Err(TrustError::Refused);
            }
            let grant = doc
                .grants
                .iter()
                .find_map(|a| match a {
                    GrantAudit::Issued(g) if &g.id == id => Some(g),
                    _ => None,
                })
                .ok_or(TrustError::Incomplete)?;
            if &grant.scope != scope {
                return Err(TrustError::Refused);
            }
            for (source, revision) in &grant.evidence {
                if doc.latest(source)?.revision != *revision {
                    return Err(TrustError::Refused);
                }
            }
            let observation = doc
                .observations
                .iter()
                .find(|o| o.task == task.spec().id)
                .ok_or(TrustError::Incomplete)?;
            let observation = doc.latest(&observation.id)?;
            if &observation.attribution.scope != scope
                || observation.instructions != task.spec().provenance
            {
                return Err(TrustError::Refused);
            }
            Ok(grant.claim.clone())
        })?;
        let credential = task.spec().authority.authorize(
            current,
            claim.permission,
            &claim.scope,
            &claim.destination,
        )?;
        if credential != claim.credential {
            return Err(TrustError::Refused.into());
        }
        Ok(credential)
    }

    pub(crate) fn transact<T>(
        &self,
        apply: impl FnOnce(&mut Document) -> Result<T, TrustError>,
    ) -> Result<T, TrustError> {
        let _lock = self.lock()?;
        let mut doc = self.load()?;
        let before = encode(&doc)?;
        let result = apply(&mut doc)?;
        doc.validate(&self.house)?;
        if encode(&doc)? != before {
            self.write(&doc)?;
        }
        Ok(result)
    }
    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&Document) -> Result<T, TrustError>,
    ) -> Result<T, TrustError> {
        let _lock = self.lock()?;
        f(&self.load()?)
    }
    fn lock(&self) -> Result<File, TrustError> {
        check_path(&self.dir, true)?;
        check_outside_repository(&self.dir)?;
        for name in ["ledger.lock", "ledger.json"] {
            check_path(&self.dir.join(name), false)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join("ledger.lock"))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::Error(error)) => return Err(error.into()),
                Err(TryLockError::WouldBlock) => {
                    if start.elapsed() >= LOCK_TIMEOUT {
                        return Err(TrustError::Busy);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    fn load(&self) -> Result<Document, TrustError> {
        let mut bytes = Vec::new();
        File::open(self.dir.join("ledger.json"))?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(TrustError::Exhausted);
        }
        let doc: Document = serde_json::from_slice(&bytes).map_err(|_| TrustError::Corrupt)?;
        doc.validate(&self.house)?;
        Ok(doc)
    }
    fn write(&self, doc: &Document) -> Result<(), TrustError> {
        let bytes = encode(doc)?;
        let temp = self.dir.join("ledger.tmp");
        // A stale temporary file is never trusted or truncated through a link.
        if temp.try_exists()? {
            check_path(&temp, false)?;
            fs::remove_file(&temp)?;
        }
        let mut file = private_file(&temp, true)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temp, self.dir.join("ledger.json"))?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
fn audit_identity(audit: &GrantAudit) -> (&crate::contracts::ExternalRef, &HouseId) {
    match audit {
        GrantAudit::Issued(g) => (&g.id, &g.house),
        GrantAudit::Revoked { id, house, .. } => (id, house),
    }
}
fn validate_grant(grant: &AutonomyGrant) -> Result<(), TrustError> {
    // Allowlist: new core permissions do not silently become autonomous.
    if !matches!(
        grant.claim.permission,
        Permission::LaunchWorker
            | Permission::MessageWorker
            | Permission::CancelWorker
            | Permission::AskHuman
            | Permission::PostComment
            | Permission::EditLabels
            | Permission::CreateIssue
            | Permission::EditIssueRelationships
            | Permission::PushBranch
            | Permission::OpenPullRequest
            | Permission::RequestReview
    ) || grant.claim.scope != GrantScope::Repository(grant.scope.project.clone())
        || grant.evidence.is_empty()
        || grant.evidence.len() > MAX_ITEMS
    {
        return Err(TrustError::Refused);
    }
    Ok(())
}
fn encode(doc: &Document) -> Result<Vec<u8>, TrustError> {
    let bytes = serde_json::to_vec(doc).map_err(|_| TrustError::Corrupt)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(TrustError::Exhausted);
    }
    Ok(bytes)
}
fn check_outside_repository(path: &Path) -> Result<(), TrustError> {
    for ancestor in path.ancestors() {
        match ancestor.join(".git").symlink_metadata() {
            Ok(_) => return Err(TrustError::UnsafePath),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn check_path(path: &Path, directory: bool) -> Result<(), TrustError> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink()
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        return Err(TrustError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(TrustError::UnsafePath);
        }
    }
    Ok(())
}
fn private_file(path: &Path, create: bool) -> Result<File, TrustError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
