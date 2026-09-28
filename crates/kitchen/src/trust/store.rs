//! Bounded, locked snapshots. Private records never belong in Git.
use crate::{
    HouseId,
    contracts::{Grant, GrantScope, HouseGrants, Permission, Role, TaskSpec},
    state::{HouseStore, StateError, TaskState},
    trust::{
        AutonomyGrant, AutonomyProposal, GrantAudit, MAX_HISTORY, MAX_ITEMS, Measurement,
        Observation, StationScope, TaskBinding, TrustError,
    },
    workflows::inspector::Inspection,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Permissions eligible for earned standing authority. Git publication,
/// release, merge, schedules, equipment, and cleanup require separate policy.
pub const EARNED_AUTONOMY_PERMISSIONS: &[Permission] = &[
    Permission::LaunchWorker,
    Permission::MessageWorker,
    Permission::CancelWorker,
    Permission::AskHuman,
    Permission::PostComment,
    Permission::EditLabels,
    Permission::CreateIssue,
    Permission::EditIssueRelationships,
    Permission::RequestReview,
];
// Each bounded grant can be replaced by a revocation with at most 1 KiB of
// extra audit data. This reserve is unavailable to ordinary writers.
const MAX_PERSISTED_BYTES: u64 = MAX_BYTES + (MAX_HISTORY as u64 * 1024);
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
static REVOCATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct RevocationGate {
    pending: PathBuf,
    temporary: PathBuf,
    _lock: File,
}
struct RemoveOnDrop(Option<PathBuf>);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}
impl RevocationGate {
    fn acquire(dir: &Path) -> Result<Self, TrustError> {
        let temporary = dir.join(format!(
            "revoke-{}-{}.tmp",
            std::process::id(),
            REVOCATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let file = private_file(&temporary, true)?;
        let mut cleanup = RemoveOnDrop(Some(temporary.clone()));
        file.lock()?;
        let pending = dir.join("revoke.pending");
        let start = Instant::now();
        loop {
            match fs::hard_link(&temporary, &pending) {
                Ok(()) => {
                    cleanup.0 = None;
                    return Ok(Self {
                        pending,
                        temporary,
                        _lock: file,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !revocation_pending(dir)? {
                        continue;
                    }
                    if start.elapsed() >= LOCK_TIMEOUT {
                        return Err(TrustError::Busy);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}
impl Drop for RevocationGate {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pending);
        let _ = fs::remove_file(&self.temporary);
    }
}
fn revocation_pending(dir: &Path) -> Result<bool, TrustError> {
    let pending = dir.join("revoke.pending");
    let file = match File::open(&pending) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    check_path(&pending, false)?;
    match file.try_lock() {
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error.into()),
        Ok(()) => {
            fs::remove_file(pending)?;
            Ok(false)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    schema: u32,
    house: HouseId,
    pub(crate) observations: Vec<Observation>,
    #[serde(default)]
    bindings: Vec<TaskBinding>,
    grants: Vec<GrantAudit>,
    pub(crate) inspections: Vec<Inspection>,
}

impl Document {
    fn validate(&self, house: &HouseId) -> Result<(), TrustError> {
        if &self.house != house {
            return Err(TrustError::Refused);
        }
        if self.schema != 1
            || self.observations.len()
                + self.bindings.len()
                + self.grants.len()
                + self.inspections.len()
                > MAX_HISTORY
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
        for (index, binding) in self.bindings.iter().enumerate() {
            if binding.spec.authority.house() != house
                || binding.spec.repository.as_ref() != Some(&binding.scope.project)
                || !role_matches_station(binding.spec.role, &binding.scope)
                || self.bindings[..index]
                    .iter()
                    .any(|old| old.spec.id == binding.spec.id)
            {
                return Err(TrustError::Corrupt);
            }
        }
        for (index, audit) in self.grants.iter().enumerate() {
            let (id, audit_house) = audit_identity(audit);
            if audit_house != house {
                return Err(TrustError::Refused);
            }
            if let GrantAudit::Proposed(proposal) | GrantAudit::RevokedProposal { proposal, .. } =
                audit
            {
                validate_claim(&proposal.claim, &proposal.scope, proposal.evidence.len())?;
                for (source, revision) in &proposal.evidence {
                    let evidence = self
                        .observations
                        .iter()
                        .find(|o| &o.id == source && &o.revision == revision)
                        .ok_or(TrustError::Corrupt)?;
                    if !evidence.trust_eligible()
                        || evidence.attribution.scope != proposal.scope
                        || !evidence_matches_binding(self, evidence)
                    {
                        return Err(TrustError::Corrupt);
                    }
                }
            }
            if let GrantAudit::Issued(grant) | GrantAudit::Revoked { grant, .. } = audit {
                validate_grant(grant)?;
                for (source, revision) in &grant.evidence {
                    let evidence = self
                        .observations
                        .iter()
                        .find(|o| &o.id == source && &o.revision == revision)
                        .ok_or(TrustError::Corrupt)?;
                    if !evidence.trust_eligible()
                        || evidence.attribution.scope != grant.scope
                        || !evidence_matches_binding(self, evidence)
                    {
                        return Err(TrustError::Corrupt);
                    }
                }
            }
            if self.grants[..index]
                .iter()
                .any(|old| audit_identity(old).0 == id)
            {
                return Err(TrustError::Conflict);
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
            bindings: Vec::new(),
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

    /// Bind a prospective task to one station, work type, and selected model.
    /// Bind before delegating earned standing grants, then create the task.
    /// Repeating the same binding is idempotent; it cannot be edited.
    ///
    /// # Errors
    /// Rejects cross-house, project, role, or identity mismatches.
    pub fn bind_task(
        &self,
        spec: &TaskSpec,
        scope: StationScope,
        model: crate::contracts::Text,
        source: crate::contracts::ExternalRef,
    ) -> Result<bool, TrustError> {
        if spec.authority.house() != &self.house
            || spec.repository.as_ref() != Some(&scope.project)
            || !role_matches_station(spec.role, &scope)
        {
            return Err(TrustError::Refused);
        }
        let binding = TaskBinding {
            spec: spec.clone(),
            scope,
            model,
            source,
        };
        self.transact(|doc| {
            if let Some(old) = doc.bindings.iter().find(|old| old.spec.id == spec.id) {
                return if old == &binding {
                    Ok(false)
                } else {
                    Err(TrustError::Conflict)
                };
            }
            doc.bindings.push(binding);
            Ok(true)
        })
    }

    /// Append one immutable revision; identical delivery is a no-op. Reordered
    /// revisions are retained, but the projection stays incomplete until gaps close.
    ///
    /// # Errors
    /// Rejects inconsistent evidence, cross-house writes, and conflicting identities.
    pub fn record(&self, store: &HouseStore, observation: Observation) -> Result<bool, TrustError> {
        if store.house() != &self.house {
            return Err(TrustError::Refused);
        }
        let task = store
            .task(&observation.task)
            .map_err(|_| TrustError::Refused)?;
        if !matches!(task.state(), TaskState::Settled { .. })
            || task.spec().repository.as_ref() != Some(&observation.attribution.scope.project)
            || task.spec().provenance != observation.instructions
            || task.spec().role != observation.role
            || task.state() != &observation.state
            || task
                .attempts()
                .iter()
                .map(|a| (a.number(), a.state()))
                .collect::<Vec<_>>()
                != observation.attempts
            || task.effects() != observation.effects
            || task.evidence().items() != observation.evidence
        {
            return Err(TrustError::Refused);
        }
        self.transact(|doc| {
            observation.validate()?;
            if let Some(binding) = doc.bindings.iter().find(|binding| binding.spec.id == observation.task)
                && (binding.scope != observation.attribution.scope
                    || matches!(&observation.attribution.model, Measurement::Observed { value, .. } if value != &binding.model))
            { return Err(TrustError::Refused); }
            if doc.observations.iter().any(|old| {
                old.id == observation.id
                    && (old.attribution.scope != observation.attribution.scope
                        || old.mode != observation.mode)
            }) {
                return Err(TrustError::Refused);
            }
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
                    || !evidence_matches_binding(doc, evidence)
                {
                    return Err(TrustError::Refused);
                }
            }
            doc.grants.push(audit);
            Ok(true)
        })
    }

    /// Store a proposal within house limits without adding standing authority.
    ///
    /// # Errors
    /// Rejects claims outside policy limits, stale evidence, and reused identities.
    pub fn propose(
        &self,
        proposal: AutonomyProposal,
        current: &HouseGrants,
    ) -> Result<bool, TrustError> {
        if current.house() != &self.house || proposal.house != self.house {
            return Err(TrustError::Refused);
        }
        validate_claim(&proposal.claim, &proposal.scope, proposal.evidence.len())?;
        if current.permitted(
            proposal.claim.permission,
            &proposal.claim.scope,
            &proposal.claim.destination,
        )? != proposal.claim.credential
        {
            return Err(TrustError::Refused);
        }
        self.transact(|doc| {
            let audit = GrantAudit::Proposed(proposal.clone());
            if doc.grants.contains(&audit) {
                return Ok(false);
            }
            if doc
                .grants
                .iter()
                .any(|a| audit_identity(a).0 == &proposal.id)
            {
                return Err(TrustError::Conflict);
            }
            validate_evidence(doc, &proposal.evidence, &proposal.scope)?;
            doc.grants.push(audit);
            Ok(true)
        })
    }

    /// Turn a proposal into standing authority only after an explicit decision.
    ///
    /// # Errors
    /// Rejects stale evidence, withdrawn limits, or an absent proposal.
    pub fn approve(
        &self,
        id: &crate::contracts::ExternalRef,
        approved_by: crate::HolderId,
        decision: crate::contracts::ExternalRef,
        at: crate::contracts::Timestamp,
        current: &HouseGrants,
    ) -> Result<bool, TrustError> {
        if current.house() != &self.house {
            return Err(TrustError::Refused);
        }
        self.transact(|doc| {
            let index = doc
                .grants
                .iter()
                .position(|a| audit_identity(a).0 == id)
                .ok_or(TrustError::Incomplete)?;
            let GrantAudit::Proposed(proposal) = &doc.grants[index] else {
                return Err(TrustError::Conflict);
            };
            if current.permitted(
                proposal.claim.permission,
                &proposal.claim.scope,
                &proposal.claim.destination,
            )? != proposal.claim.credential
            {
                return Err(TrustError::Refused);
            }
            validate_evidence(doc, &proposal.evidence, &proposal.scope)?;
            let grant = AutonomyGrant {
                id: proposal.id.clone(),
                house: proposal.house.clone(),
                scope: proposal.scope.clone(),
                claim: proposal.claim.clone(),
                approved_by,
                decision,
                evidence: proposal.evidence.clone(),
                proposal: Some(proposal.clone()),
                at,
            };
            doc.grants[index] = GrantAudit::Issued(grant);
            Ok(true)
        })
    }

    /// Revoke an existing approval in place. The prior decision remains stored.
    ///
    /// # Errors
    /// Unknown grant identities are refused; storage capacity never blocks a revocation.
    pub fn revoke(
        &self,
        id: &crate::contracts::ExternalRef,
        by: crate::HolderId,
        decision: crate::contracts::ExternalRef,
        at: crate::contracts::Timestamp,
    ) -> Result<bool, TrustError> {
        let _gate = RevocationGate::acquire(&self.dir)?;
        let _lock = self.lock(true)?;
        let mut doc = self.load()?;
        let audit = doc
            .grants
            .iter_mut()
            .find(|audit| audit_identity(audit).0 == id)
            .ok_or(TrustError::NotFound)?;
        match audit {
            GrantAudit::Revoked { .. } | GrantAudit::RevokedProposal { .. } => Ok(false),
            GrantAudit::Proposed(proposal) => {
                *audit = GrantAudit::RevokedProposal {
                    proposal: proposal.clone(),
                    by,
                    decision,
                    at,
                };
                doc.validate(&self.house)?;
                self.write(&doc)?;
                Ok(true)
            }
            GrantAudit::Issued(grant) => {
                *audit = GrantAudit::Revoked {
                    grant: grant.clone(),
                    by,
                    decision,
                    at,
                };
                doc.validate(&self.house)?;
                self.write(&doc)?;
                Ok(true)
            }
        }
    }

    /// Current decisions with original proposal, approval, and revocation sources.
    ///
    /// # Errors
    /// Returns storage failures.
    pub fn grant_history(&self) -> Result<Vec<GrantAudit>, TrustError> {
        self.read(|d| Ok(d.grants.clone()))
    }

    /// Project approved, current evidence into the core house authority for one
    /// bound task. Call before delegation and again before every external effect;
    /// pass the returned grants to the core executor. A plain house policy cannot
    /// acquire the earned grants, and revocation removes them on the next read.
    ///
    /// # Errors
    /// Rejects absent or altered bindings, cross-house tasks, and store failures.
    pub fn standing_for_task(
        &self,
        store: &HouseStore,
        spec: &TaskSpec,
        config: &HouseGrants,
    ) -> Result<HouseGrants, TrustError> {
        if store.house() != &self.house
            || config.house() != &self.house
            || spec.authority.house() != &self.house
        {
            return Err(TrustError::Refused);
        }
        match store.task(&spec.id) {
            Ok(record) if record.spec() != spec => return Err(TrustError::Refused),
            Ok(_) | Err(crate::Error::State(StateError::TaskNotFound(_))) => {}
            Err(_) => return Err(TrustError::Refused),
        }
        self.read(|doc| {
            let binding = doc.bindings.iter().find(|b| b.spec.id == spec.id)
                .ok_or(TrustError::Incomplete)?;
            if !binding.matches(spec) {
                return Err(TrustError::Refused);
            }
            let mut earned: Vec<Grant> = Vec::new();
            for audit in &doc.grants {
                let GrantAudit::Issued(grant) = audit else { continue; };
                if grant.scope != binding.scope { continue; }
                let valid = grant.evidence.iter().all(|(id, revision)| {
                    doc.latest(id).is_ok_and(|observed| {
                        observed.revision == *revision
                            && observed.trust_eligible()
                            && observed.instructions == spec.provenance
                            && observed.attribution.scope == binding.scope
                            && matches!(&observed.attribution.model, Measurement::Observed { value, .. } if value == &binding.model)
                    })
                });
                if !valid { continue; }
                if config.permitted(grant.claim.permission, &grant.claim.scope, &grant.claim.destination)
                    .is_ok_and(|credential| credential == grant.claim.credential)
                {
                    earned.push(grant.claim.clone());
                }
            }
            config.with_added_standing(earned).map_err(Into::into)
        })
    }

    pub(crate) fn transact<T>(
        &self,
        apply: impl FnOnce(&mut Document) -> Result<T, TrustError>,
    ) -> Result<T, TrustError> {
        let _lock = self.lock(false)?;
        let mut doc = self.load()?;
        let before = encode(&doc, MAX_PERSISTED_BYTES)?;
        let result = apply(&mut doc)?;
        if doc.observations.len() + doc.bindings.len() + doc.grants.len() + doc.inspections.len()
            > MAX_HISTORY
        {
            return Err(TrustError::Exhausted);
        }
        doc.validate(&self.house)?;
        if encode(&doc, MAX_BYTES)? != before {
            self.write(&doc)?;
        }
        Ok(result)
    }
    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&Document) -> Result<T, TrustError>,
    ) -> Result<T, TrustError> {
        let _lock = self.lock(false)?;
        f(&self.load()?)
    }
    fn lock(&self, priority: bool) -> Result<File, TrustError> {
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
            if !priority && revocation_pending(&self.dir)? {
                if start.elapsed() >= LOCK_TIMEOUT {
                    return Err(TrustError::Busy);
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            match file.try_lock() {
                Ok(()) => {
                    if !priority && revocation_pending(&self.dir)? {
                        file.unlock()?;
                        continue;
                    }
                    return Ok(file);
                }
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
            .take(MAX_PERSISTED_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PERSISTED_BYTES {
            return Err(TrustError::Exhausted);
        }
        let doc: Document = serde_json::from_slice(&bytes).map_err(|_| TrustError::Corrupt)?;
        doc.validate(&self.house)?;
        Ok(doc)
    }
    fn write(&self, doc: &Document) -> Result<(), TrustError> {
        let bytes = encode(doc, MAX_PERSISTED_BYTES)?;
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
        GrantAudit::Proposed(p) | GrantAudit::RevokedProposal { proposal: p, .. } => {
            (&p.id, &p.house)
        }
        GrantAudit::Issued(g) => (&g.id, &g.house),
        GrantAudit::Revoked { grant, .. } => (&grant.id, &grant.house),
    }
}
fn role_matches_station(role: Role, scope: &StationScope) -> bool {
    !Role::ALL
        .iter()
        .any(|candidate| candidate.as_str() == scope.station.as_str())
        || role.as_str() == scope.station.as_str()
}
fn validate_grant(grant: &AutonomyGrant) -> Result<(), TrustError> {
    validate_claim(&grant.claim, &grant.scope, grant.evidence.len())?;
    if let Some(proposal) = &grant.proposal
        && (proposal.id != grant.id
            || proposal.house != grant.house
            || proposal.scope != grant.scope
            || proposal.claim != grant.claim
            || proposal.evidence != grant.evidence)
    {
        return Err(TrustError::Invalid);
    }
    Ok(())
}
fn validate_claim(
    claim: &crate::contracts::Grant,
    scope: &StationScope,
    evidence_len: usize,
) -> Result<(), TrustError> {
    // Allowlist: new core permissions do not silently become autonomous.
    if !EARNED_AUTONOMY_PERMISSIONS.contains(&claim.permission)
        || claim.scope != GrantScope::Repository(scope.project.clone())
        || evidence_len == 0
        || evidence_len > MAX_ITEMS
    {
        return Err(TrustError::Refused);
    }
    Ok(())
}
fn validate_evidence(
    doc: &Document,
    evidence: &[(crate::contracts::ExternalRef, std::num::NonZeroU32)],
    scope: &StationScope,
) -> Result<(), TrustError> {
    for (id, revision) in evidence {
        let observed = doc.latest(id)?;
        if !observed.trust_eligible()
            || observed.revision != *revision
            || &observed.attribution.scope != scope
            || !evidence_matches_binding(doc, observed)
        {
            return Err(TrustError::Refused);
        }
    }
    Ok(())
}
fn evidence_matches_binding(doc: &Document, observed: &Observation) -> bool {
    doc.bindings.iter().any(|binding| {
        binding.spec.id == observed.task
            && binding.scope == observed.attribution.scope
            && binding.spec.provenance == observed.instructions
            && matches!(&observed.attribution.model, Measurement::Observed { value, .. } if value == &binding.model)
    })
}
fn encode(doc: &Document, limit: u64) -> Result<Vec<u8>, TrustError> {
    let bytes = serde_json::to_vec(doc).map_err(|_| TrustError::Corrupt)?;
    if bytes.len() as u64 > limit {
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
