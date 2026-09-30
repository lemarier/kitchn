//! Graduation from supervised to unattended runs, on evidence.
//!
//! House policy sets thresholds per work type ([`GraduationPolicy`]). The
//! eligibility report ([`Ledger::eligibility`]) counts live, supervised runs of
//! one station scope on the house's current guidance revision and changes
//! nothing. Only an owner's [`GraduationDecision`] adds unattended authority;
//! it expires, can be revoked, and never exceeds the house's interactive
//! limits or reaches beyond [`EARNED_AUTONOMY_PERMISSIONS`], so merge,
//! publication, schedule activation, and equipment authority stay separate.
//!
//! A confirmed revert or regression observed after a decision demotes it and
//! reports it for review ([`Ledger::graduation_reviews`]); under a
//! [`RegressionResponse::PauseSchedule`] policy the review carries the schedule
//! pause as a planned effect for the caller to submit under its own authority.
//!
//! Reads of the core store happen outside the ledger lock, as the inspector
//! requires: evidence is re-checked inside the write.
use crate::{
    HolderId, HouseId, TaskId,
    contracts::{
        CommitId, ExternalRef, Grant, GrantScope, HouseGrants, ResourceKind, ResourceRef,
        ScheduleEffect, TaskSpec, Timestamp, Trigger,
    },
    house::HouseConfig,
    scheduling::ScheduleState,
    state::{HouseStore, OwnershipEvent, StateError},
    trust::{
        Document, EARNED_AUTONOMY_PERMISSIONS, EvidenceMode, Ledger, MAX_ITEMS, Measurement,
        Observation, StationScope, TrustError, store_error,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::{NonZeroU16, NonZeroU32},
    time::Duration,
};

/// Longest measurement window a policy may set.
pub const MAX_WINDOW_DAYS: u16 = 365;
/// Longest term of one owner decision; renewal is a new decision.
pub const MAX_DECISION_TERM: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// What a change of house guidance revision does to an earlier decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuidanceChange {
    /// The decision applies only to tasks pinned to the revision it was made
    /// on. A new revision needs new evidence and a new decision.
    Reset,
    /// The decision also applies to tasks on the house's current revision
    /// while the report on that revision is eligible; until then it is
    /// suspended. Without a decision, eligibility still grants nothing.
    ReEvaluate,
}

/// What a confirmed regression after graduation does beyond demotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegressionResponse {
    /// Report the decision for review.
    Report,
    /// Report it and plan a pause of the decision's schedule.
    PauseSchedule,
}

/// Thresholds for one work type, all measured on one guidance revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraduationPolicy {
    /// Supervised runs with a first-pass measurement required in the window,
    /// at most [`MAX_ITEMS`] so one decision can cite them all.
    pub min_supervised_runs: NonZeroU32,
    /// Accepted-on-first-pass share of those runs, in percent (0–100).
    pub min_first_pass_percent: u8,
    /// Days before the evaluation time that count, at most [`MAX_WINDOW_DAYS`].
    pub window_days: NonZeroU16,
    /// Handling of a guidance revision change.
    pub on_guidance_change: GuidanceChange,
    /// Handling of a confirmed regression after graduation.
    pub on_regression: RegressionResponse,
}

impl GraduationPolicy {
    /// Check the bounds.
    ///
    /// # Errors
    /// [`TrustError::Invalid`] for a percentage above 100, a window longer than
    /// [`MAX_WINDOW_DAYS`], or more required runs than [`MAX_ITEMS`].
    pub fn validate(&self) -> Result<(), TrustError> {
        if self.min_first_pass_percent > 100
            || self.window_days.get() > MAX_WINDOW_DAYS
            || usize::try_from(self.min_supervised_runs.get()).map_or(true, |n| n > MAX_ITEMS)
        {
            return Err(TrustError::Invalid);
        }
        Ok(())
    }

    fn window_start(&self, now: Timestamp) -> Timestamp {
        let window = DAY.saturating_mul(u32::from(self.window_days.get()));
        let millis = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        Timestamp::from_unix_millis(now.as_unix_millis().saturating_sub(millis))
    }
}

/// One supervised run with a first-pass measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisedRun {
    /// Evidence stream.
    pub stream: ExternalRef,
    /// Latest reconciled revision of that stream.
    pub revision: NonZeroU32,
    /// The run's task.
    pub task: TaskId,
    /// When the run was observed.
    pub observed_at: Timestamp,
    /// Whether it was accepted on first pass.
    pub accepted: bool,
    /// Source of the first-pass measurement.
    pub source: ExternalRef,
}

/// Runs on a guidance revision other than the one evaluated. Shown, never counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionEvidence {
    /// The older (or other) guidance revision.
    pub guidance: CommitId,
    /// Its supervised runs in the window.
    pub runs: Vec<SupervisedRun>,
}

/// Why a stream in scope was not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExclusionReason {
    /// Revision gaps: the latest state is unknown.
    IncompleteStream,
    /// Fixture or fake-backend evidence.
    Simulated,
    /// The core store no longer has the task.
    TaskUnavailable,
    /// A scheduled or event-started claim took part in the run.
    Unsupervised,
    /// Observed before the window or after the evaluation time.
    OutsideWindow,
    /// No first-pass measurement.
    FirstPassMissing,
}

/// A stream in scope that was not counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exclusion {
    /// Evidence stream.
    pub stream: ExternalRef,
    /// Why.
    pub reason: ExclusionReason,
}

/// The report's verdict. Never authority by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Eligibility {
    /// The house sets no thresholds for this work type.
    NoPolicy,
    /// Fewer measured runs than required.
    InsufficientSamples {
        /// Counted runs.
        runs: u32,
        /// Required runs.
        required: NonZeroU32,
    },
    /// Enough runs, too few accepted on first pass.
    BelowAcceptance {
        /// Accepted runs.
        accepted: u32,
        /// Counted runs.
        runs: u32,
        /// Required percentage.
        required_percent: u8,
    },
    /// Thresholds met; the owner may record a decision.
    Eligible,
}

/// Read-only eligibility of one station scope on one guidance revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EligibilityReport {
    /// Evaluated house.
    pub house: HouseId,
    /// Station, project, and work type.
    pub scope: StationScope,
    /// The guidance revision the counted runs share.
    pub guidance: CommitId,
    /// Thresholds applied, if the house sets any.
    pub policy: Option<GraduationPolicy>,
    /// Evaluation time.
    pub evaluated_at: Timestamp,
    /// Start of the window; `None` without a policy.
    pub window_start: Option<Timestamp>,
    /// Counted runs: the sample and its sources.
    pub runs: Vec<SupervisedRun>,
    /// Runs on other guidance revisions, grouped by revision.
    pub other_revisions: Vec<RevisionEvidence>,
    /// Streams in scope that were not counted.
    pub excluded: Vec<Exclusion>,
    /// Verdict.
    pub verdict: Eligibility,
}

impl EligibilityReport {
    /// Runs accepted on first pass.
    #[must_use]
    pub fn accepted(&self) -> usize {
        self.runs.iter().filter(|run| run.accepted).count()
    }

    fn evidence(&self) -> BTreeSet<(&ExternalRef, NonZeroU32)> {
        self.runs.iter().map(|r| (&r.stream, r.revision)).collect()
    }
}

/// An owner's explicit, expiring decision to allow unattended runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraduationDecision {
    /// Stable identity; a revoked identity cannot be reused.
    pub id: ExternalRef,
    /// Owning house.
    pub house: HouseId,
    /// Station, project, and work type.
    pub scope: StationScope,
    /// Guidance revision the eligibility was measured on.
    pub guidance: CommitId,
    /// Unattended authority, each within the house's interactive limits.
    pub claims: BTreeSet<Grant>,
    /// Exactly the runs the eligibility report counted.
    pub evidence: Vec<(ExternalRef, NonZeroU32)>,
    /// Schedule running this work unattended, paused on regression by policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ResourceRef>,
    /// Decision author.
    pub approved_by: HolderId,
    /// Decision source.
    pub source: ExternalRef,
    /// Decision time; eligibility is evaluated at this time.
    pub at: Timestamp,
    /// The decision stops applying at this time.
    pub expires_at: Timestamp,
}

impl GraduationDecision {
    fn validate(&self) -> Result<(), TrustError> {
        if self.expires_at <= self.at
            || self.expires_at > self.at.saturating_add(MAX_DECISION_TERM)
            || self.claims.is_empty()
            || self.claims.len() > MAX_ITEMS
            || self.evidence.is_empty()
            || self.evidence.len() > MAX_ITEMS
            || self
                .schedule
                .as_ref()
                .is_some_and(|s| s.kind != ResourceKind::Schedule)
        {
            return Err(TrustError::Invalid);
        }
        let project = GrantScope::Repository(self.scope.project.clone());
        if self
            .claims
            .iter()
            .any(|c| !EARNED_AUTONOMY_PERMISSIONS.contains(&c.permission) || c.scope != project)
        {
            return Err(TrustError::Refused);
        }
        Ok(())
    }

    /// Whether the decision's term covers `now`: from its time, inclusive, to
    /// its expiry, exclusive.
    fn in_effect(&self, now: Timestamp) -> bool {
        self.at <= now && now < self.expires_at
    }
}

/// Decision state. Revocation keeps the original decision in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum GraduationAudit {
    /// Recorded decision.
    Decided(GraduationDecision),
    /// Revoked decision.
    Revoked {
        /// Original decision.
        decision: GraduationDecision,
        /// Revocation author.
        by: HolderId,
        /// Revocation source.
        source: ExternalRef,
        /// Revocation time.
        at: Timestamp,
    },
}

impl GraduationAudit {
    /// The decision, whether or not it was revoked.
    pub(crate) fn decision(&self) -> &GraduationDecision {
        match self {
            Self::Decided(decision) | Self::Revoked { decision, .. } => decision,
        }
    }
}

/// A decision demoted by confirmed regressions observed after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraduationReview {
    /// The demoted decision.
    pub decision: ExternalRef,
    /// Its scope.
    pub scope: StationScope,
    /// Sources of the confirmed reverts and regressions.
    pub findings: Vec<ExternalRef>,
    /// Planned schedule pause under a [`RegressionResponse::PauseSchedule`]
    /// policy. Kitchen does not submit it; it needs `manage-schedule`.
    pub pause: Option<ScheduleEffect>,
}

/// One stream's place in a report.
enum Classified {
    /// A counted run on its guidance revision.
    Run(CommitId, SupervisedRun),
    /// Not counted.
    Excluded(ExclusionReason),
}

/// Checks run on every ledger load and write.
pub(crate) fn validate_audits(
    audits: &[GraduationAudit],
    house: &HouseId,
    revisions: &HashMap<(&ExternalRef, NonZeroU32), &Observation>,
) -> Result<(), TrustError> {
    let mut ids = BTreeSet::new();
    for audit in audits {
        let decision = audit.decision();
        if &decision.house != house {
            return Err(TrustError::Refused);
        }
        decision.validate()?;
        if !decision.evidence.iter().all(|(id, revision)| {
            revisions
                .get(&(id, *revision))
                .is_some_and(|o| o.attribution.scope == decision.scope)
        }) {
            return Err(TrustError::Corrupt);
        }
        if !ids.insert(&decision.id) {
            return Err(TrustError::Conflict);
        }
    }
    Ok(())
}

/// Latest revision per stream in `scope`, or `None` for a stream with gaps,
/// in one pass. Every revision of a stream shares its scope.
fn streams_in_scope<'a>(
    doc: &'a Document,
    scope: &StationScope,
) -> BTreeMap<&'a ExternalRef, Option<&'a Observation>> {
    let mut grouped: BTreeMap<&ExternalRef, Vec<&Observation>> = BTreeMap::new();
    for observation in doc
        .observations
        .iter()
        .filter(|o| &o.attribution.scope == scope)
    {
        grouped
            .entry(&observation.id)
            .or_default()
            .push(observation);
    }
    grouped
        .into_iter()
        .map(|(id, versions)| {
            // Revisions are unique per stream, so they run from one without a
            // gap exactly when the highest equals the count.
            let latest = versions.iter().copied().max_by_key(|o| o.revision);
            let complete = latest
                .is_some_and(|o| usize::try_from(o.revision.get()).ok() == Some(versions.len()));
            (id, latest.filter(|_| complete))
        })
        .collect()
}

/// Every recorded revision in a scope, compared before a write so a run or
/// correction recorded after the report was read cannot slip past it. Every
/// revision counts, including one in a stream with a gap, whose report entry
/// does not change when another revision arrives. Revisions are immutable, so
/// the set identifies the scope's evidence.
type Revisions = BTreeSet<(ExternalRef, NonZeroU32)>;

fn revisions(doc: &Document, scope: &StationScope) -> Revisions {
    doc.observations
        .iter()
        .filter(|o| &o.attribution.scope == scope)
        .map(|o| (o.id.clone(), o.revision))
        .collect()
}

/// Confirmed live reverts and regressions in `decision`'s scope observed after
/// it. Every recorded revision counts, including one behind a revision gap, so
/// demotion fails closed.
fn regressions_after(doc: &Document, decision: &GraduationDecision) -> Vec<ExternalRef> {
    let mut found = BTreeSet::new();
    for observation in &doc.observations {
        if observation.attribution.scope != decision.scope
            || observation.mode != EvidenceMode::Live
            || observation.observed_at <= decision.at
        {
            continue;
        }
        if let Measurement::Observed { value: pr, .. } = &observation.pull_request {
            for list in [&pr.reverts, &pr.regressions] {
                if let Measurement::Observed { value, .. } = list {
                    found.extend(value.iter().map(|finding| &finding.source));
                }
            }
        }
    }
    found.into_iter().cloned().collect()
}

/// Whether every claim on the task was made with a person present.
fn supervised(events: &[OwnershipEvent]) -> bool {
    let triggers: Vec<&Trigger> = events
        .iter()
        .filter_map(|event| match event {
            OwnershipEvent::Claimed { trigger, .. }
            | OwnershipEvent::Adopted { trigger, .. }
            | OwnershipEvent::TakenOver { trigger, .. } => Some(trigger),
            OwnershipEvent::Relinquished { .. } | OwnershipEvent::Released { .. } => None,
        })
        .collect();
    !triggers.is_empty() && triggers.iter().all(|t| matches!(t, Trigger::Interactive))
}

/// The house's interactive limits, with no standing grants.
fn interactive_limits(config: &HouseConfig) -> Result<HouseGrants, TrustError> {
    Ok(HouseGrants::with_limits(
        config.house.clone(),
        config.policy_limits.iter().cloned(),
        [],
    )?)
}

fn within(limits: &HouseGrants, claim: &Grant) -> bool {
    limits
        .permitted(claim.permission, &claim.scope, &claim.destination)
        .is_ok_and(|credential| credential == claim.credential)
}

impl Ledger {
    /// Evaluate graduation eligibility for `scope` on the house's current
    /// guidance revision at `now`. Reads only; never creates a grant.
    ///
    /// Counts the latest revision of each live stream in scope whose task was
    /// claimed only interactively and that has a first-pass measurement in
    /// the policy window. Runs on another guidance revision are listed in
    /// [`EligibilityReport::other_revisions`]; everything else in scope is
    /// listed with its [`ExclusionReason`].
    ///
    /// # Errors
    /// Refuses a store or configuration of another house, or a project the
    /// house does not serve. A failure reading the core store is `Storage`.
    pub fn eligibility(
        &self,
        store: &HouseStore,
        config: &HouseConfig,
        scope: &StationScope,
        now: Timestamp,
    ) -> Result<EligibilityReport, TrustError> {
        Ok(self.evaluate(store, config, scope, now)?.0)
    }

    fn evaluate(
        &self,
        store: &HouseStore,
        config: &HouseConfig,
        scope: &StationScope,
        now: Timestamp,
    ) -> Result<(EligibilityReport, Revisions), TrustError> {
        if store.house() != self.house()
            || &config.house != self.house()
            || !config.repositories.contains(&scope.project)
        {
            return Err(TrustError::Refused);
        }
        let policy = config.graduation.get(&scope.work_type).copied();
        let window_start = policy.map(|p| p.window_start(now));
        let (streams, seen) = self.read(|doc| {
            let streams = streams_in_scope(doc, scope);
            let owned: Vec<(ExternalRef, Option<Observation>)> = streams
                .iter()
                .map(|(id, latest)| ((*id).clone(), latest.cloned()))
                .collect();
            Ok((owned, revisions(doc, scope)))
        })?;
        let mut report = EligibilityReport {
            house: self.house().clone(),
            scope: scope.clone(),
            guidance: config.guidance.clone(),
            policy,
            evaluated_at: now,
            window_start,
            runs: Vec::new(),
            other_revisions: Vec::new(),
            excluded: Vec::new(),
            verdict: Eligibility::NoPolicy,
        };
        let mut others: BTreeMap<CommitId, Vec<SupervisedRun>> = BTreeMap::new();
        for (stream, latest) in streams {
            let classified = match latest {
                None => Classified::Excluded(ExclusionReason::IncompleteStream),
                Some(observation) => self.classify(store, observation, window_start, now)?,
            };
            match classified {
                Classified::Run(guidance, run) if guidance == config.guidance => {
                    report.runs.push(run);
                }
                Classified::Run(guidance, run) => others.entry(guidance).or_default().push(run),
                Classified::Excluded(reason) => report.excluded.push(Exclusion { stream, reason }),
            }
        }
        report.other_revisions = others
            .into_iter()
            .map(|(guidance, runs)| RevisionEvidence { guidance, runs })
            .collect();
        report.verdict = verdict(policy, &report);
        Ok((report, seen))
    }

    /// A run with its guidance revision, or why it is not counted.
    fn classify(
        &self,
        store: &HouseStore,
        observation: Observation,
        window_start: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<Classified, TrustError> {
        if observation.mode != EvidenceMode::Live {
            return Ok(Classified::Excluded(ExclusionReason::Simulated));
        }
        let task = match store.task(&observation.task) {
            Ok(task) => task,
            Err(crate::Error::State(StateError::TaskNotFound(_))) => {
                return Ok(Classified::Excluded(ExclusionReason::TaskUnavailable));
            }
            Err(error) => return Err(store_error(error)),
        };
        if !supervised(task.ownership()) {
            return Ok(Classified::Excluded(ExclusionReason::Unsupervised));
        }
        if observation.observed_at > now
            || window_start.is_some_and(|s| observation.observed_at < s)
        {
            return Ok(Classified::Excluded(ExclusionReason::OutsideWindow));
        }
        let Measurement::Observed { value: pr, .. } = &observation.pull_request else {
            return Ok(Classified::Excluded(ExclusionReason::FirstPassMissing));
        };
        let Measurement::Observed { value, source, .. } = &pr.first_pass else {
            return Ok(Classified::Excluded(ExclusionReason::FirstPassMissing));
        };
        Ok(Classified::Run(
            observation.instructions.house_guidance.clone(),
            SupervisedRun {
                stream: observation.id.clone(),
                revision: observation.revision,
                task: observation.task.clone(),
                observed_at: observation.observed_at,
                accepted: *value,
                source: source.clone(),
            },
        ))
    }

    /// Record an owner's decision at the caller's current time `now`. The
    /// report at `decision.at` must be eligible on the current guidance
    /// revision and `decision.evidence` must be exactly its counted runs.
    /// Repeating an identical decision is a no-op.
    ///
    /// # Errors
    /// `Refused` for another house, stale guidance, an ineligible report,
    /// different evidence, any evidence in scope recorded or removed while the
    /// report was read, a claim beyond the house's interactive limits, or
    /// merge, publication, schedule, equipment, or cleanup authority;
    /// `Invalid` for an empty or unbounded decision, a schedule reference that
    /// is not a schedule, a decision time after `now`, or a term that is not
    /// positive and at most [`MAX_DECISION_TERM`]; `Conflict` for a reused
    /// identity.
    pub fn graduate(
        &self,
        store: &HouseStore,
        config: &HouseConfig,
        decision: GraduationDecision,
        now: Timestamp,
    ) -> Result<bool, TrustError> {
        if &decision.house != self.house() || decision.guidance != config.guidance {
            return Err(TrustError::Refused);
        }
        decision.validate()?;
        // A future-dated decision would grant authority before it was made.
        if decision.at > now {
            return Err(TrustError::Invalid);
        }
        let limits = interactive_limits(config)?;
        if !decision.claims.iter().all(|claim| within(&limits, claim)) {
            return Err(TrustError::Refused);
        }
        let (report, seen) = self.evaluate(store, config, &decision.scope, decision.at)?;
        let cited: BTreeSet<_> = decision.evidence.iter().map(|(s, r)| (s, *r)).collect();
        if report.verdict != Eligibility::Eligible
            || cited.len() != decision.evidence.len()
            || cited != report.evidence()
        {
            return Err(TrustError::Refused);
        }
        #[cfg(feature = "test-hooks")]
        test_hooks::before_graduation_write();
        self.transact(|doc| {
            if let Some(old) = doc
                .graduations
                .iter()
                .find(|a| a.decision().id == decision.id)
            {
                return match old {
                    GraduationAudit::Decided(old) if old == &decision => Ok(false),
                    GraduationAudit::Decided(_) | GraduationAudit::Revoked { .. } => {
                        Err(TrustError::Conflict)
                    }
                };
            }
            // Any revision recorded or archived in scope since the report was
            // read, including one in a stream with a gap, refuses the decision.
            if revisions(doc, &decision.scope) != seen {
                return Err(TrustError::Refused);
            }
            doc.graduations.push(GraduationAudit::Decided(decision));
            Ok(true)
        })
    }

    /// Revoke a decision in place; the original stays auditable. Uses the
    /// revocation reserve, so a full ledger never blocks it.
    ///
    /// # Errors
    /// `NotFound` for an unknown identity.
    pub fn revoke_graduation(
        &self,
        id: &ExternalRef,
        by: HolderId,
        source: ExternalRef,
        at: Timestamp,
    ) -> Result<bool, TrustError> {
        self.transact_priority(|doc| {
            let audit = doc
                .graduations
                .iter_mut()
                .find(|audit| &audit.decision().id == id)
                .ok_or(TrustError::NotFound)?;
            let GraduationAudit::Decided(decision) = audit else {
                return Ok(false);
            };
            *audit = GraduationAudit::Revoked {
                decision: decision.clone(),
                by,
                source,
                at,
            };
            Ok(true)
        })
    }

    /// All decisions, including revoked ones.
    ///
    /// # Errors
    /// Returns storage failures.
    pub fn graduation_history(&self) -> Result<Vec<GraduationAudit>, TrustError> {
        self.read(|doc| Ok(doc.graduations.clone()))
    }

    /// Add the unattended authority of current decisions to `current` for a
    /// bound task at `now`. A decision applies while it is recorded, not
    /// revoked, in its term (`at <= now < expires_at`), not demoted by a later
    /// regression, its work type still has a policy, and its guidance rule
    /// holds (see [`GuidanceChange`]). Each claim must still be within the house's
    /// interactive limits and `current`'s limits. Call again before each effect.
    ///
    /// # Errors
    /// Rejects another house, an absent (`Incomplete`) or altered binding. A
    /// failure reading the core store is `Storage`.
    pub fn graduated_standing(
        &self,
        store: &HouseStore,
        config: &HouseConfig,
        spec: &TaskSpec,
        current: &HouseGrants,
        now: Timestamp,
    ) -> Result<HouseGrants, TrustError> {
        if store.house() != self.house()
            || &config.house != self.house()
            || current.house() != self.house()
            || spec.authority.house() != self.house()
        {
            return Err(TrustError::Refused);
        }
        let (scope, decisions) = self.read(|doc| {
            let binding = doc
                .bindings
                .iter()
                .find(|b| b.spec.id == spec.id)
                .ok_or(TrustError::Incomplete)?;
            if !binding.matches(spec) {
                return Err(TrustError::Refused);
            }
            let decisions: Vec<GraduationDecision> = doc
                .graduations
                .iter()
                .filter_map(|audit| match audit {
                    GraduationAudit::Decided(d) => Some(d),
                    GraduationAudit::Revoked { .. } => None,
                })
                .filter(|d| {
                    d.scope == binding.scope
                        && d.in_effect(now)
                        && regressions_after(doc, d).is_empty()
                })
                .cloned()
                .collect();
            Ok((binding.scope.clone(), decisions))
        })?;
        let Some(policy) = config.graduation.get(&scope.work_type) else {
            return Ok(current.clone());
        };
        let guidance = &spec.provenance.house_guidance;
        let mut re_evaluated = None;
        let limits = interactive_limits(config)?;
        let mut earned = Vec::new();
        for decision in decisions {
            let applies = if &decision.guidance == guidance {
                true
            } else if policy.on_guidance_change == GuidanceChange::ReEvaluate
                && guidance == &config.guidance
            {
                if re_evaluated.is_none() {
                    let report = self.eligibility(store, config, &scope, now)?;
                    re_evaluated = Some(report.verdict == Eligibility::Eligible);
                }
                re_evaluated == Some(true)
            } else {
                false
            };
            if applies {
                earned.extend(
                    decision
                        .claims
                        .into_iter()
                        .filter(|claim| within(&limits, claim) && within(current, claim)),
                );
            }
        }
        Ok(current.with_added_standing(earned)?)
    }

    /// Unrevoked decisions in their term with confirmed live reverts or
    /// regressions observed after them. Each is demoted until the owner
    /// revokes it or records a new decision; under a
    /// [`RegressionResponse::PauseSchedule`] policy the review plans a pause
    /// of the decision's schedule. Nothing is submitted.
    ///
    /// # Errors
    /// Refuses a configuration of another house; returns storage failures.
    pub fn graduation_reviews(
        &self,
        config: &HouseConfig,
        now: Timestamp,
    ) -> Result<Vec<GraduationReview>, TrustError> {
        if &config.house != self.house() {
            return Err(TrustError::Refused);
        }
        self.read(|doc| {
            let mut reviews = Vec::new();
            for audit in &doc.graduations {
                let GraduationAudit::Decided(decision) = audit else {
                    continue;
                };
                if !decision.in_effect(now) {
                    continue;
                }
                let findings = regressions_after(doc, decision);
                if findings.is_empty() {
                    continue;
                }
                let pause = match config.graduation.get(&decision.scope.work_type) {
                    Some(p) if p.on_regression == RegressionResponse::PauseSchedule => decision
                        .schedule
                        .clone()
                        .map(|schedule| ScheduleEffect::SetState {
                            schedule,
                            state: ScheduleState::Paused,
                            requires: None,
                        }),
                    Some(_) | None => None,
                };
                reviews.push(GraduationReview {
                    decision: decision.id.clone(),
                    scope: decision.scope.clone(),
                    findings,
                    pause,
                });
            }
            Ok(reviews)
        })
    }
}

fn verdict(policy: Option<GraduationPolicy>, report: &EligibilityReport) -> Eligibility {
    let Some(policy) = policy else {
        return Eligibility::NoPolicy;
    };
    // Both counts are bounded by the ledger's history limit.
    let runs = u32::try_from(report.runs.len()).unwrap_or(u32::MAX);
    let accepted = u32::try_from(report.accepted()).unwrap_or(u32::MAX);
    if runs < policy.min_supervised_runs.get() {
        return Eligibility::InsufficientSamples {
            runs,
            required: policy.min_supervised_runs,
        };
    }
    if u64::from(accepted) * 100 < u64::from(policy.min_first_pass_percent) * u64::from(runs) {
        return Eligibility::BelowAcceptance {
            accepted,
            runs,
            required_percent: policy.min_first_pass_percent,
        };
    }
    Eligibility::Eligible
}

/// Pause point for tests that need to act between the eligibility read and the
/// ledger write in [`Ledger::graduate`]. Enabled only by the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub mod test_hooks {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        static BEFORE_WRITE: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run `hook` once on this thread, after the next [`graduate`] reads its
    /// eligibility report and before it takes the ledger write lock.
    ///
    /// [`graduate`]: crate::trust::Ledger::graduate
    pub fn on_next_graduation_write(hook: impl FnOnce() + 'static) {
        BEFORE_WRITE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn before_graduation_write() {
        let hook = BEFORE_WRITE.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
}
