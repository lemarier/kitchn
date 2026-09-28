//! Evidence inputs and immutable audit records.
use crate::contracts::Settlement;
use crate::{
    HolderId, HouseId, TaskId,
    contracts::{
        AttemptNumber, Evidence, EvidenceSubject, EvidenceVerdict, ExternalRef, Grant, Provenance,
        Repository, Role, TaskSpec, Text, Timestamp,
    },
    state::{AttemptState, EffectRecord, HouseStore, TaskState},
    trust::TrustError,
};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// Evidence collection is bounded independently of external APIs.
pub const MAX_ITEMS: usize = 128;
/// Maximum history entries in one ledger; exhaustion never discards old records.
pub const MAX_HISTORY: usize = 4096;

/// A missing measurement is not a zero or a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Measurement<T> {
    /// No source supplied this measurement.
    Missing,
    /// The source explicitly reports that it was not tested.
    Untested,
    /// Collection failed or the backend cannot supply it.
    Unavailable,
    /// Actual observation, its denominator, and its source.
    Observed {
        /// Measured value.
        value: T,
        /// Number of observations represented, never zero.
        samples: NonZeroU32,
        /// Source for the measurement.
        source: ExternalRef,
    },
}

/// Runtime evidence must never be confused with fixtures or local simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum EvidenceMode {
    /// Sanitized fixture or fake backend.
    Simulated,
    /// Observed on the actual runtime or equipment.
    Live,
}

/// Scope of a station's evidence and autonomy. Names are bounded validated text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationScope {
    /// House-defined station/domain name.
    pub station: Text,
    /// Exact project; house-wide autonomy is deliberately unsupported.
    pub project: Repository,
    /// House-defined work category.
    pub work_type: Text,
}

/// Attribution supplied by the adapter, including explicit unknowns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attribution {
    /// Responsible station, project, and work type.
    pub scope: StationScope,
    /// Worker identity for independent inspection.
    pub agent: Measurement<HolderId>,
    /// Model family and version as reported by the backend.
    pub model: Measurement<Text>,
    /// Reported token use, never inferred from launch success.
    pub tokens: Measurement<u64>,
}

/// A confirmed finding with its attribution evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// Stable source identity, used for deduplication.
    pub source: ExternalRef,
    /// Exact delivered revision investigated.
    pub subject: EvidenceSubject,
    /// Concrete confirmed consequence.
    pub consequence: Text,
}

/// Narrow input boundary for #7; the adapter supplies exact-head evidence.
/// Absence of PR evidence is explicit in [`Observation::pull_request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestEvidence {
    /// Selected credential house.
    pub house: HouseId,
    /// Owning task.
    pub task: TaskId,
    /// Target project.
    pub repository: Repository,
    /// PR source identity.
    pub source: ExternalRef,
    /// Head and base the measurements concern.
    pub subject: EvidenceSubject,
    /// Independent first-pass acceptance; task success does not imply this.
    pub first_pass: Measurement<bool>,
    /// Confirmed review findings, not raw model suggestions.
    pub findings: Measurement<Vec<Finding>>,
    /// Reverts whose attribution was investigated.
    pub reverts: Measurement<Vec<Finding>>,
    /// Regressions whose attribution was investigated.
    pub regressions: Measurement<Vec<Finding>>,
    /// Required checks, including unavailable verdicts.
    pub checks: Measurement<Vec<Evidence>>,
}

/// Bench observation kept separately from CI and worker settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchResult {
    /// Exact revision exercised.
    pub subject: EvidenceSubject,
    /// Whether the specified procedure passed.
    pub passed: bool,
    /// Procedure or test identity.
    pub procedure: Text,
}

/// A versioned observation, including source state captured directly from #4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Stable source stream identity (one stream per task).
    pub id: ExternalRef,
    /// One-based revision; gaps remain unavailable until reconciled.
    pub revision: NonZeroU32,
    /// Required for every subsequent revision, including attribution corrections.
    pub correction: Option<ExternalRef>,
    /// Credential/runtime house.
    pub house: HouseId,
    /// Portable task identity.
    pub task: TaskId,
    /// Observation time, not an ordering key.
    pub observed_at: Timestamp,
    /// Actual versus simulated collection.
    pub mode: EvidenceMode,
    /// Station and agent attribution.
    pub attribution: Attribution,
    /// Pinned Kitchen, house, and repository instruction revisions from the task.
    pub instructions: Provenance,
    /// Task responsibility from the durable store.
    pub role: Role,
    /// Task state, without inferring acceptance.
    pub state: TaskState,
    /// Attempt identities and reported outcomes.
    pub attempts: Vec<(AttemptNumber, AttemptState)>,
    /// Effect outcomes; uncertain launch remains uncertain.
    pub effects: Vec<EffectRecord>,
    /// Current store evidence, with exact subjects and source links.
    pub evidence: Vec<Evidence>,
    /// Explicit missing PR evidence when the adapter has none.
    pub pull_request: Measurement<PullRequestEvidence>,
    /// Physical qualification; never derived from CI.
    pub bench: Measurement<Vec<BenchResult>>,
    /// Whether an escalation was appropriate, based on an investigated source.
    pub appropriate_escalation: Measurement<bool>,
}

impl Observation {
    /// Derive the task, attempt, effect, and evidence portion from the durable store.
    /// Additional measurements start missing and must be supplied explicitly.
    ///
    /// # Errors
    /// Rejects a project mismatch or unavailable task/store.
    pub fn collect(
        store: &HouseStore,
        task: &TaskId,
        id: ExternalRef,
        attribution: Attribution,
        mode: EvidenceMode,
        observed_at: Timestamp,
    ) -> crate::Result<Self> {
        let record = store.task(task)?;
        if record.spec().repository.as_ref() != Some(&attribution.scope.project) {
            return Err(TrustError::Refused.into());
        }
        Ok(Self {
            id,
            revision: NonZeroU32::MIN,
            correction: None,
            house: store.house().clone(),
            task: task.clone(),
            observed_at,
            mode,
            attribution,
            instructions: record.spec().provenance.clone(),
            role: record.spec().role,
            state: record.state().clone(),
            attempts: record
                .attempts()
                .iter()
                .map(|a| (a.number(), a.state()))
                .collect(),
            effects: record.effects().to_vec(),
            evidence: record.evidence().items().to_vec(),
            pull_request: Measurement::Missing,
            bench: Measurement::Missing,
            appropriate_escalation: Measurement::Missing,
        })
    }

    /// Whether this record can support an explicit trust decision. Unknown
    /// agent/model/usage attribution and simulated runs never provide trust.
    #[must_use]
    pub fn trust_eligible(&self) -> bool {
        self.mode == EvidenceMode::Live
            && matches!(self.attribution.agent, Measurement::Observed { .. })
            && matches!(self.attribution.model, Measurement::Observed { .. })
            && matches!(self.attribution.tokens, Measurement::Observed { value, .. } if value > 0)
            && matches!(
                self.state,
                TaskState::Settled {
                    settlement: Settlement::Succeeded,
                    ..
                }
            )
            && matches!(&self.pull_request, Measurement::Observed { value: pr, .. }
                if matches!(pr.first_pass, Measurement::Observed { value: true, .. })
                    && matches!(&pr.findings, Measurement::Observed { value, .. } if value.is_empty())
                    && matches!(&pr.reverts, Measurement::Observed { value, .. } if value.is_empty())
                    && matches!(&pr.regressions, Measurement::Observed { value, .. } if value.is_empty()))
            && matches!(&self.pull_request, Measurement::Observed { value: pr, .. }
                if matches!(&pr.checks, Measurement::Observed { value, .. }
                    if !value.is_empty() && value.iter().all(|check| check.verdict == EvidenceVerdict::Pass)))
            && !matches!(&self.bench, Measurement::Observed { value, .. } if value.iter().any(|result| !result.passed))
            && !matches!(
                &self.appropriate_escalation,
                Measurement::Observed { value: false, .. }
            )
    }

    pub(crate) fn validate(&self) -> Result<(), TrustError> {
        if (self.revision.get() == 1) != self.correction.is_none()
            || self.attempts.len() > MAX_ITEMS
            || self.effects.len() > crate::state::MAX_EFFECTS_PER_TASK
            || self.evidence.len() > MAX_ITEMS
        {
            return Err(TrustError::Invalid);
        }
        if self.effects.iter().any(|effect| {
            effect.request().house() != &self.house || effect.request().task() != &self.task
        }) {
            return Err(TrustError::Refused);
        }
        if let Measurement::Observed { value: pr, .. } = &self.pull_request {
            if pr.house != self.house
                || pr.task != self.task
                || pr.repository != self.attribution.scope.project
            {
                return Err(TrustError::Refused);
            }
            for measurement in [&pr.findings, &pr.reverts, &pr.regressions] {
                if let Measurement::Observed { value, .. } = measurement
                    && (value.len() > MAX_ITEMS
                        || value.iter().any(|f| f.subject != pr.subject)
                        || value.iter().enumerate().any(|(i, f)| {
                            value[..i]
                                .iter()
                                .any(|previous| previous.source == f.source)
                        }))
                {
                    return Err(TrustError::Invalid);
                }
            }
            if let Measurement::Observed { value, .. } = &pr.checks
                && (value.len() > MAX_ITEMS || value.iter().any(|e| e.subject != pr.subject))
            {
                return Err(TrustError::Invalid);
            }
        }
        if let Measurement::Observed { value, .. } = &self.bench
            && value.len() > MAX_ITEMS
        {
            return Err(TrustError::Invalid);
        }
        Ok(())
    }
}

/// Explicit operator decision, never synthesized from a score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutonomyGrant {
    /// Stable grant identity; revoked identities cannot be reused.
    pub id: ExternalRef,
    /// Granting house.
    pub house: HouseId,
    /// Exact station/project/work category.
    pub scope: StationScope,
    /// Core permission/destination/credential tuple.
    pub claim: Grant,
    /// Decision author.
    pub approved_by: HolderId,
    /// Decision source bound to this grant.
    pub decision: ExternalRef,
    /// Evidence stream and revision explicitly considered by the approver.
    pub evidence: Vec<(ExternalRef, NonZeroU32)>,
    /// Original unapproved proposal, retained after an approval transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<AutonomyProposal>,
    /// Approval time.
    pub at: Timestamp,
}

/// A requested standing grant that has no approval authority yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutonomyProposal {
    /// Stable identity used by the later approval decision.
    pub id: ExternalRef,
    /// Owning house.
    pub house: HouseId,
    /// Station, project, and work category.
    pub scope: StationScope,
    /// Proposed exact core grant.
    pub claim: Grant,
    /// Evidence explicitly presented for review.
    pub evidence: Vec<(ExternalRef, NonZeroU32)>,
    /// Proposal source.
    pub source: ExternalRef,
    /// Proposal time.
    pub at: Timestamp,
}

/// Write-once adapter attribution for a prospective task. Core authority is
/// delegated after this binding and is checked separately at execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBinding {
    /// Prospective task specification before earned authority is delegated.
    pub spec: TaskSpec,
    /// Station and work category declared by the house adapter.
    pub scope: StationScope,
    /// The model identity the adapter resolved, compared verbatim with the
    /// observed model. Trust only matches it; choosing a model per role and
    /// work type belongs to house agent-selection policy (issue #42).
    pub model: Text,
    /// Auditable source for the adapter decision.
    pub source: ExternalRef,
}

impl TaskBinding {
    pub(crate) fn matches(&self, spec: &TaskSpec) -> bool {
        self.spec.id == spec.id
            && self.spec.role == spec.role
            && self.spec.repository == spec.repository
            && self.spec.retry == spec.retry
            && self.spec.provenance == spec.provenance
            && self.spec.resources == spec.resources
            && self.spec.requires == spec.requires
    }
}

/// Grant decision state. Revocation replaces the current entry while retaining
/// the original proposal or approval and the revocation decision together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum GrantAudit {
    /// Proposal without authority.
    Proposed(AutonomyProposal),
    /// Explicit issuance.
    Issued(AutonomyGrant),
    /// Revoked approval; the original decision remains auditable in place.
    Revoked {
        /// Original approved grant.
        grant: AutonomyGrant,
        /// Decision author.
        by: HolderId,
        /// Auditable reason.
        decision: ExternalRef,
        /// Decision time.
        at: Timestamp,
    },
    /// Revoked proposal, which was never active authority.
    RevokedProposal {
        /// Original proposal retained for audit.
        proposal: AutonomyProposal,
        /// Decision author.
        by: HolderId,
        /// Revocation source.
        decision: ExternalRef,
        /// Decision time.
        at: Timestamp,
    },
}
