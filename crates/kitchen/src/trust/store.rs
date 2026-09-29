//! The house trust ledger: one bounded snapshot on the shared state engine.
//! Private records never belong in Git.
use crate::{
    HouseId,
    contracts::{ExternalRef, Grant, GrantScope, HouseGrants, Permission, Role, TaskSpec},
    state::{
        HouseStore, StateError, StoreOptions, TaskState,
        snapshot::{Snapshot, SnapshotStore, StoreLayout},
    },
    trust::{
        AutonomyGrant, AutonomyProposal, GrantAudit, MAX_HISTORY, MAX_ITEMS, Measurement,
        Observation, StationScope, TaskBinding, TrustError,
    },
    workflows::inspector::Inspection,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::Path,
    time::Duration,
};

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
/// Largest snapshot an ordinary write may produce.
const MAX_BYTES: u64 = 8 * 1024 * 1024;
// Each bounded grant can be replaced by a revocation with at most 1 KiB of
// extra audit data (`revocation_growth_stays_inside_the_per_grant_reserve`
// measures the worst case). This reserve is unavailable to ordinary writers.
// `usize` widens to `u64` on every supported target; `TryFrom` is not const.
const REVOCATION_RESERVE: u64 = MAX_HISTORY as u64 * 1024;
const OPTIONS: StoreOptions = StoreOptions {
    lock_timeout: Duration::from_secs(2),
    max_state_bytes: MAX_BYTES + REVOCATION_RESERVE,
};
const LAYOUT: StoreLayout = StoreLayout {
    marker: "store.json",
    snapshot: "ledger.json",
    temporary: "ledger.tmp",
    lock: "ledger.lock",
    pretty: false,
    require_private: true,
    // Held by a revocation so ordinary writers yield to it.
    priority_intent: Some("revoke.pending"),
    priority_reserve_bytes: REVOCATION_RESERVE,
};
// Older stores are refused as unsupported before decoding; there is no
// migration. Version 2 added the inspector task and fence to inspections;
// version 3 requires every binding's model to come from its agent selection.
const SCHEMA: u64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Document {
    schema: u64,
    house: HouseId,
    nonce: u64,
    pub(crate) observations: Vec<Observation>,
    #[serde(default)]
    bindings: Vec<TaskBinding>,
    grants: Vec<GrantAudit>,
    pub(crate) inspections: Vec<Inspection>,
}

impl Document {
    /// Checks every entry once against hash indexes, so a full ledger
    /// validates in time linear in its entries and evidence references.
    fn validate(&self, house: &HouseId) -> Result<(), TrustError> {
        if &self.house != house {
            return Err(TrustError::Refused);
        }
        if self.schema != SCHEMA || self.entries() > MAX_HISTORY {
            return Err(TrustError::Corrupt);
        }
        let mut revisions = HashMap::with_capacity(self.observations.len());
        let mut stream_tasks = HashMap::with_capacity(self.observations.len());
        let mut task_streams = HashMap::with_capacity(self.observations.len());
        for observation in &self.observations {
            observation.validate()?;
            if &observation.house != house {
                return Err(TrustError::Refused);
            }
            // One stream per task and one task per stream; each revision once.
            if *stream_tasks
                .entry(&observation.id)
                .or_insert(&observation.task)
                != &observation.task
                || *task_streams
                    .entry(&observation.task)
                    .or_insert(&observation.id)
                    != &observation.id
                || revisions
                    .insert((&observation.id, observation.revision), observation)
                    .is_some()
            {
                return Err(TrustError::Conflict);
            }
        }
        let mut bindings = HashMap::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            if binding.spec.authority.house() != house
                || binding.spec.repository.as_ref() != Some(&binding.scope.project)
                || !role_matches_station(binding.spec.role, &binding.scope)
                || selected_model(&binding.spec).as_ref() != Some(&binding.model)
                || bindings.insert(&binding.spec.id, binding).is_some()
            {
                return Err(TrustError::Corrupt);
            }
        }
        let evidence_holds =
            |scope: &StationScope, (source, revision): &(ExternalRef, NonZeroU32)| {
                revisions.get(&(source, *revision)).is_some_and(|evidence| {
                    evidence.trust_eligible()
                        && &evidence.attribution.scope == scope
                        && bindings
                            .get(&evidence.task)
                            .is_some_and(|binding| binding_covers(binding, evidence))
                })
            };
        let mut grant_ids = HashSet::with_capacity(self.grants.len());
        for audit in &self.grants {
            let (id, audit_house) = audit_identity(audit);
            if audit_house != house {
                return Err(TrustError::Refused);
            }
            let (scope, evidence) = match audit {
                GrantAudit::Proposed(proposal) | GrantAudit::RevokedProposal { proposal, .. } => {
                    validate_claim(&proposal.claim, &proposal.scope, &proposal.evidence)?;
                    (&proposal.scope, &proposal.evidence)
                }
                GrantAudit::Issued(grant) | GrantAudit::Revoked { grant, .. } => {
                    validate_grant(grant)?;
                    (&grant.scope, &grant.evidence)
                }
            };
            if !evidence.iter().all(|item| evidence_holds(scope, item)) {
                return Err(TrustError::Corrupt);
            }
            if !grant_ids.insert(id) {
                return Err(TrustError::Conflict);
            }
        }
        let mut inspection_ids = HashSet::with_capacity(self.inspections.len());
        for inspection in &self.inspections {
            inspection.validate(house)?;
            let observation = revisions
                .get(&(&inspection.plan().observation, inspection.revision()))
                .ok_or(TrustError::Corrupt)?;
            inspection.validate_observation(observation)?;
            if !inspection_ids.insert(inspection.id()) {
                return Err(TrustError::Conflict);
            }
        }
        Ok(())
    }

    /// History entries counted against [`MAX_HISTORY`].
    fn entries(&self) -> usize {
        self.observations.len() + self.bindings.len() + self.grants.len() + self.inspections.len()
    }

    pub(crate) fn latest(
        &self,
        id: &crate::contracts::ExternalRef,
    ) -> Result<&Observation, TrustError> {
        let mut versions: Vec<_> = self.observations.iter().filter(|o| &o.id == id).collect();
        versions.sort_by_key(|o| o.revision);
        for (index, observation) in versions.iter().enumerate() {
            if u32::try_from(index + 1).ok() != Some(observation.revision.get()) {
                return Err(TrustError::Incomplete);
            }
        }
        versions.last().copied().ok_or(TrustError::Incomplete)
    }
}

impl Snapshot for Document {
    const SCHEMA: u64 = SCHEMA;
    type Error = TrustError;

    fn empty(house: HouseId, nonce: u64) -> Self {
        Self {
            schema: SCHEMA,
            house,
            nonce,
            observations: Vec::new(),
            bindings: Vec::new(),
            grants: Vec::new(),
            inspections: Vec::new(),
        }
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Runs on every load. A stored snapshot that fails any check is corrupt
    /// whatever class the write-time check would give it: a caller must not see
    /// `Conflict`, `Refused`, `Invalid`, or `Exhausted` for a file it cannot
    /// fix by retrying or changing its input. Writes validate through the
    /// inherent method and keep those classes.
    fn validate(&self, house: &HouseId) -> Result<(), TrustError> {
        Self::validate(self, house).map_err(|_| TrustError::Corrupt)
    }
}

/// How full a ledger is. History is never discarded: once either limit is
/// reached, `record`, `bind_task`, and other ordinary writes fail and earned
/// standing stops applying to new tasks. Revocation stays possible because it
/// uses a byte reserve outside [`Self::max_bytes`] and adds no entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// Stored observations, task bindings, grant audits, and inspections.
    pub entries: usize,
    /// Entry limit for ordinary writes.
    pub max_entries: usize,
    /// Size of the stored snapshot.
    pub bytes: u64,
    /// Snapshot size limit for ordinary writes.
    pub max_bytes: u64,
}

impl Capacity {
    /// Percentage of either limit at which [`Self::near_limit`] reports.
    pub const WARNING_PERCENT: u64 = 80;

    /// Whether either limit is at least [`Self::WARNING_PERCENT`] used, so an
    /// operator can act before ordinary writes stop.
    #[must_use]
    pub fn near_limit(&self) -> bool {
        let reached = |used: u64, limit: u64| {
            u128::from(used) * 100 >= u128::from(limit) * u128::from(Self::WARNING_PERCENT)
        };
        reached(
            u64::try_from(self.entries).unwrap_or(u64::MAX),
            u64::try_from(self.max_entries).unwrap_or(u64::MAX),
        ) || reached(self.bytes, self.max_bytes)
    }
}

/// House-scoped runtime ledger. Each operation reloads under a bounded lock.
/// The directory and its files must be private to the process user; same-user
/// hostile filesystem races are outside this boundary, as with the core house
/// store.
#[derive(Debug, Clone)]
pub struct Ledger {
    engine: SnapshotStore<Document>,
}
impl Ledger {
    /// Initialize a new ledger in `path`, creating the directory and missing
    /// parents. An existing marker or snapshot, including a partial setup, is
    /// never replaced. Unix directories/files are created with 0700/0600 modes.
    ///
    /// # Errors
    /// Rejects existing stores, repositories, redirected or nonprivate storage.
    pub fn initialize(path: impl AsRef<Path>, house: HouseId) -> Result<Self, TrustError> {
        Ok(Self {
            engine: SnapshotStore::initialize(path, house, OPTIONS, LAYOUT)?,
        })
    }

    /// Open an established ledger; missing or invalid history is an error.
    /// Unlike the core house store, every open requires owner-only storage:
    /// the directory and each managed file must have no group or other
    /// permissions on Unix.
    ///
    /// # Errors
    /// Rejects cross-house, redirected, public, missing, or corrupted storage.
    pub fn open(path: impl AsRef<Path>, house: HouseId) -> Result<Self, TrustError> {
        Ok(Self {
            engine: SnapshotStore::open(path, house, OPTIONS, LAYOUT)?,
        })
    }

    /// House selected when this handle was opened.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        self.engine.house()
    }

    /// Bind a prospective task to one station and work type. The model trust
    /// compares against observations is derived from the task's resolved
    /// agent selection ([`AgentSelection::attribution_model`]); the station
    /// and work type are still declared by the caller. Bind before delegating
    /// earned standing grants, then create the task. Repeating the same
    /// binding is idempotent; it cannot be edited.
    ///
    /// [`AgentSelection::attribution_model`]: crate::selection::AgentSelection::attribution_model
    ///
    /// # Errors
    /// Rejects cross-house, project, role, or identity mismatches, and a task
    /// without a resolved agent selection.
    pub fn bind_task(
        &self,
        spec: &TaskSpec,
        scope: StationScope,
        source: crate::contracts::ExternalRef,
    ) -> Result<bool, TrustError> {
        if spec.authority.house() != self.house()
            || spec.repository.as_ref() != Some(&scope.project)
            || !role_matches_station(spec.role, &scope)
        {
            return Err(TrustError::Refused);
        }
        let model = selected_model(spec).ok_or(TrustError::Refused)?;
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
    /// The task must be settled and the observation must equal its stored
    /// facts. When core holds evidence for the task, PR evidence must describe
    /// that exact head and base; where core holds none, the adapter's PR
    /// evidence is the only source. A core item that is not a pass keeps the
    /// record out of trust (see [`Observation::trust_eligible`]).
    ///
    /// # Errors
    /// Rejects inconsistent evidence, PR evidence for another head, a task
    /// binding that differs from the stored task, cross-house writes, and
    /// conflicting identities. An absent task is `Refused`; any other failure
    /// reading the core store is reported as `Storage`.
    pub fn record(&self, store: &HouseStore, observation: Observation) -> Result<bool, TrustError> {
        if store.house() != self.house() {
            return Err(TrustError::Refused);
        }
        let task = store.task(&observation.task).map_err(store_error)?;
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
        // Adapter PR evidence must describe the exact head core recorded for the
        // task. Where core holds no evidence, the adapter's is the only source.
        if let Measurement::Observed { value: pr, .. } = &observation.pull_request
            && task
                .evidence()
                .subject()
                .is_some_and(|core| core != &pr.subject)
        {
            return Err(TrustError::Refused);
        }
        self.transact(|doc| {
            observation.validate()?;
            if let Some(binding) = doc
                .bindings
                .iter()
                .find(|binding| binding.spec.id == observation.task)
                && (!binding.matches(task.spec())
                    || binding.scope != observation.attribution.scope
                    || matches!(&observation.attribution.model, Measurement::Observed { value, .. } if value != &binding.model))
            {
                return Err(TrustError::Refused);
            }
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

    /// Store a proposal within house limits without adding standing authority.
    /// Delivering the same proposal again is a no-op, including after its
    /// approval; the same identity with different content is a conflict.
    ///
    /// # Errors
    /// Rejects claims outside policy limits, stale evidence, and reused identities.
    pub fn propose(
        &self,
        proposal: AutonomyProposal,
        current: &HouseGrants,
    ) -> Result<bool, TrustError> {
        if current.house() != self.house() || &proposal.house != self.house() {
            return Err(TrustError::Refused);
        }
        validate_claim(&proposal.claim, &proposal.scope, &proposal.evidence)?;
        self.transact(|doc| {
            if let Some(existing) = doc
                .grants
                .iter()
                .find(|audit| audit_identity(audit).0 == &proposal.id)
            {
                return match existing {
                    GrantAudit::Proposed(old) if old == &proposal => Ok(false),
                    GrantAudit::Issued(grant) if grant.proposal.as_ref() == Some(&proposal) => {
                        Ok(false)
                    }
                    GrantAudit::Proposed(_)
                    | GrantAudit::Issued(_)
                    | GrantAudit::Revoked { .. }
                    | GrantAudit::RevokedProposal { .. } => Err(TrustError::Conflict),
                };
            }
            if current.permitted(
                proposal.claim.permission,
                &proposal.claim.scope,
                &proposal.claim.destination,
            )? != proposal.claim.credential
            {
                return Err(TrustError::Refused);
            }
            validate_evidence(doc, &proposal.evidence, &proposal.scope)?;
            doc.grants.push(GrantAudit::Proposed(proposal));
            Ok(true)
        })
    }

    /// Turn a proposal into standing authority only after an explicit decision.
    /// Repeating an approval by the same approver with the same decision is a
    /// no-op that returns `false`, whatever its timestamp; a different approver
    /// or decision, or a revoked entry, is a conflict.
    ///
    /// # Errors
    /// Rejects stale evidence, withdrawn limits, a conflicting decision, and an
    /// unknown identity (`NotFound`).
    pub fn approve(
        &self,
        id: &crate::contracts::ExternalRef,
        approved_by: crate::HolderId,
        decision: crate::contracts::ExternalRef,
        at: crate::contracts::Timestamp,
        current: &HouseGrants,
    ) -> Result<bool, TrustError> {
        if current.house() != self.house() {
            return Err(TrustError::Refused);
        }
        self.transact(|doc| {
            let index = doc
                .grants
                .iter()
                .position(|a| audit_identity(a).0 == id)
                .ok_or(TrustError::NotFound)?;
            let proposal = match &doc.grants[index] {
                GrantAudit::Proposed(proposal) => proposal,
                GrantAudit::Issued(grant)
                    if grant.approved_by == approved_by && grant.decision == decision =>
                {
                    return Ok(false);
                }
                GrantAudit::Issued(_)
                | GrantAudit::Revoked { .. }
                | GrantAudit::RevokedProposal { .. } => return Err(TrustError::Conflict),
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
        let house = self.house();
        self.engine.transact_priority(|doc| {
            let audit = doc
                .grants
                .iter_mut()
                .find(|audit| audit_identity(audit).0 == id)
                .ok_or(TrustError::NotFound)?;
            *audit = match audit {
                GrantAudit::Revoked { .. } | GrantAudit::RevokedProposal { .. } => {
                    return Ok(false);
                }
                GrantAudit::Proposed(proposal) => GrantAudit::RevokedProposal {
                    proposal: proposal.clone(),
                    by,
                    decision,
                    at,
                },
                GrantAudit::Issued(grant) => GrantAudit::Revoked {
                    grant: grant.clone(),
                    by,
                    decision,
                    at,
                },
            };
            doc.validate(house)?;
            Ok(true)
        })
    }

    /// Current use of the entry and byte limits.
    ///
    /// # Errors
    /// Returns storage failures, including a corrupt snapshot.
    pub fn capacity(&self) -> Result<Capacity, TrustError> {
        self.engine.read_sized(|doc, bytes| Capacity {
            entries: doc.entries(),
            max_entries: MAX_HISTORY,
            bytes,
            max_bytes: MAX_BYTES,
        })
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
    /// A grant applies only when every evidence task has the acting task's
    /// station scope, role, and bound model, and exactly its instruction pins
    /// ([`Provenance`](crate::contracts::Provenance) is compared for equality).
    /// Any change to the Kitchen, house-guidance, or repository-instruction pin
    /// therefore voids earned standing until new evidence is earned under the
    /// new pins. This fails closed; policy-based re-evaluation per guidance
    /// revision belongs to the graduation work in #44.
    ///
    /// # Errors
    /// Rejects absent or altered bindings and cross-house tasks. A failure
    /// reading the core store is reported as `Storage`.
    pub fn standing_for_task(
        &self,
        store: &HouseStore,
        spec: &TaskSpec,
        config: &HouseGrants,
    ) -> Result<HouseGrants, TrustError> {
        if store.house() != self.house()
            || config.house() != self.house()
            || spec.authority.house() != self.house()
        {
            return Err(TrustError::Refused);
        }
        match store.task(&spec.id) {
            Ok(record) if record.spec() != spec => return Err(TrustError::Refused),
            Ok(_) | Err(crate::Error::State(StateError::TaskNotFound(_))) => {}
            Err(error) => return Err(store_error(error)),
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
                // A stale, incomplete, or mismatched stream adds no grant.
                let valid = grant.evidence.iter().all(|(id, revision)| {
                    doc.latest(id).is_ok_and(|observed| {
                        observed.revision == *revision
                            && observed.trust_eligible()
                            && observed.instructions == spec.provenance
                            && observed.role == spec.role
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
        let house = self.house();
        self.engine.transact(|doc| {
            let result = apply(doc)?;
            if doc.entries() > MAX_HISTORY {
                return Err(TrustError::Exhausted);
            }
            doc.validate(house)?;
            Ok(result)
        })
    }
    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&Document) -> Result<T, TrustError>,
    ) -> Result<T, TrustError> {
        self.engine.read(f)?
    }
}
/// An absent task is a policy refusal; any other core-store failure keeps its
/// own class so a caller can tell a transient fault from a refusal.
pub(crate) fn store_error(error: crate::Error) -> TrustError {
    match error {
        crate::Error::State(StateError::TaskNotFound(_)) => TrustError::Refused,
        crate::Error::State(error) => TrustError::Storage(error),
        crate::Error::Contract(error) => TrustError::Authority(error),
        crate::Error::Trust(error) => error,
        // A task read produces none of these; refuse rather than guess.
        crate::Error::Identifier(_)
        | crate::Error::House(_)
        | crate::Error::Integration(_)
        | crate::Error::Scaffold(_)
        | crate::Error::Orca(_)
        | crate::Error::Cleanup(_)
        | crate::Error::Decomposition(_)
        | crate::Error::Workflow(_)
        | crate::Error::Selection(_)
        | crate::Error::Event(_)
        | crate::Error::Verification(_)
        | crate::Error::Coordination(_)
        | crate::Error::Budget(_)
        | crate::Error::Intake(_)
        | crate::Error::HouseInit(_)
        | crate::Error::Forge(_)
        | crate::Error::Deliberation(_) => TrustError::Refused,
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
/// The model identity a task's resolved agent selection is attributed to.
fn selected_model(spec: &TaskSpec) -> Option<crate::contracts::Text> {
    spec.agent
        .as_ref()
        .and_then(|agent| agent.selection.attribution_model().ok())
}
/// A station named after a role binds only tasks of that role. Any other
/// station name is a house-defined domain that accepts every role; earned
/// standing still requires the acting task's role to equal the evidence task's.
fn role_matches_station(role: Role, scope: &StationScope) -> bool {
    !Role::ALL
        .iter()
        .any(|candidate| candidate.as_str() == scope.station.as_str())
        || role.as_str() == scope.station.as_str()
}
fn validate_grant(grant: &AutonomyGrant) -> Result<(), TrustError> {
    validate_claim(&grant.claim, &grant.scope, &grant.evidence)?;
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
    evidence: &[(crate::contracts::ExternalRef, std::num::NonZeroU32)],
) -> Result<(), TrustError> {
    // Allowlist: new core permissions do not silently become autonomous.
    if !EARNED_AUTONOMY_PERMISSIONS.contains(&claim.permission)
        || claim.scope != GrantScope::Repository(scope.project.clone())
        || evidence.is_empty()
        || evidence.len() > MAX_ITEMS
    {
        return Err(TrustError::Refused);
    }
    // One entry per evidence stream: a repeated stream adds no evidence.
    if evidence
        .iter()
        .enumerate()
        .any(|(index, (id, _))| evidence[..index].iter().any(|(earlier, _)| earlier == id))
    {
        return Err(TrustError::Invalid);
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
    doc.bindings
        .iter()
        .any(|binding| binding.spec.id == observed.task && binding_covers(binding, observed))
}
/// Whether `observed` ran with the station, pins, and model its task was bound to.
fn binding_covers(binding: &TaskBinding, observed: &Observation) -> bool {
    binding.scope == observed.attribution.scope
        && binding.spec.provenance == observed.instructions
        && matches!(&observed.attribution.model, Measurement::Observed { value, .. } if value == &binding.model)
}
