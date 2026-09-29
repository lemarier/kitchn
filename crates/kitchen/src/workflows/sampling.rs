//! Post-merge inspection sampling: how often merged work is inspected, and
//! which merges are picked.
//!
//! Once a work type merges without a person reviewing it first, the
//! inspector's samples are the remaining human-visible check. A
//! [`SamplingPolicy`] sets a rate per work type from [`RateInputs`]: how long
//! the merge grant has stood, how many clean deliveries the station has in the
//! trust ledger since the grant or its latest confirmed finding, and whether a
//! confirmed finding or attributed revert is recent. The rate falls from
//! `initial` toward `floor` only as both the age and the clean record mature,
//! never reaches zero, and stays at least `afterFinding` while a finding is
//! recent. A finding also restarts the clean record, so the rate climbs back
//! down from `initial` once the finding ages out.
//!
//! Selection is deterministic: a SHA-256 digest of the house, the merged pull
//! request with its exact head and base, and the policy revision gives a draw
//! from 0 to 999, and the merge is picked when the draw is below the rate in
//! thousandths. The decision keeps its inputs, rate, and draw, and is recorded
//! once as a house-store marker, so [`SamplingDecision::replay`] can show why
//! a merge was skipped.
//!
//! A picked merge spends the house usage budget. While that budget is
//! exhausted the decision is [`Outcome::BudgetExhausted`] and carries an
//! [`OwnerReport`]; exhaustion never changes the rate or the floor.
//!
//! This module launches no inspection and posts nothing:
//! [`crate::workflows::inspector`] runs samples, and callers deliver
//! [`OwnerReport`]s through their own reporting authority.
use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    ErrorClass, HouseId, WorkflowId,
    contracts::{Claimant, EvidenceSubject, ExternalRef, Timestamp},
    house::MergeSubject,
    scheduling::{BudgetAssessment, Exhausted},
    selection::WorkType,
    state::{
        HouseStore, MarkerFact, MarkerKey, MarkerRecording, MarkerSchema, MarkerSubject, WorkItem,
    },
    trust::{EvidenceMode, Ledger, Measurement, Observation, StationScope, TrustError},
    workflows::inspector::SampleResult,
};

/// The workflow name sampling markers are recorded under.
pub const WORKFLOW: &str = "inspection-sampling";
/// Work types with their own rates in one policy.
pub const MAX_WORK_TYPE_RATES: usize = 64;
/// Longest accepted maturity or finding window, in days.
pub const MAX_DAYS: u16 = 3650;

const DECISION_SCHEMA: &str = "inspection-sampling.decision";
const RAISE_SCHEMA: &str = "inspection-sampling.rate-raise";
/// Separates this digest from any other use of the same fields.
const DRAW_DOMAIN: &str = "kitchen.inspection-sampling.draw/1";
const DAY_MILLIS: u128 = 86_400_000;
const PER_MILLE: u16 = 1000;

/// Sampling input or evidence failure. Private finding content is never included.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SamplingError {
    /// A rate is out of range, the floor is above the initial rate, or the
    /// initial rate is above the rate after a finding.
    #[error("invalid sampling policy")]
    InvalidPolicy,
    /// The merge, scope, or recorded times disagree.
    #[error("invalid sampling input")]
    InvalidInput,
    /// A rate raise names a finding the scope's record does not hold.
    #[error("the finding is not in the scope's record")]
    UnknownFinding,
    /// The house budget assessment is for another window than the decision.
    #[error("the house budget assessment does not cover the decision time")]
    StaleBudget,
    /// The house budget assessment does not reach back to its window start,
    /// so it cannot show that budget remains.
    #[error("the house budget assessment is incomplete")]
    IncompleteBudget,
    /// A recorded decision or store belongs to another house.
    #[error("sampling house mismatch")]
    HouseMismatch,
    /// A decision does not follow from its recorded inputs under the policy.
    #[error("the sampling decision does not match its recorded inputs")]
    NotReproducible,
    /// Reading the trust ledger failed.
    #[error(transparent)]
    Trust(#[from] TrustError),
}

impl SamplingError {
    /// Handling class shared by CLI callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidPolicy | Self::InvalidInput | Self::UnknownFinding => {
                ErrorClass::InvalidInput
            }
            Self::StaleBudget | Self::IncompleteBudget | Self::HouseMismatch => ErrorClass::Refused,
            Self::NotReproducible => ErrorClass::Conflict,
            Self::Trust(error) => error.class(),
        }
    }
}

/// Chance that a merge is inspected, in thousandths: 1 to 1000. Zero is not
/// a rate, so no policy can stop sampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct Rate(u16);

impl Rate {
    /// Validate a rate in thousandths.
    ///
    /// # Errors
    /// [`SamplingError::InvalidPolicy`] outside 1 to 1000.
    pub const fn new(per_mille: u16) -> Result<Self, SamplingError> {
        if per_mille == 0 || per_mille > PER_MILLE {
            return Err(SamplingError::InvalidPolicy);
        }
        Ok(Self(per_mille))
    }

    /// The rate in thousandths.
    #[must_use]
    pub const fn per_mille(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for Rate {
    type Error = SamplingError;

    fn try_from(per_mille: u16) -> Result<Self, Self::Error> {
        Self::new(per_mille)
    }
}

impl From<Rate> for u16 {
    fn from(rate: Rate) -> Self {
        rate.0
    }
}

/// How one work type's rate moves with its record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateSchedule {
    /// Rate for a new grant, and after a finding restarts the clean record.
    pub initial: Rate,
    /// Lowest rate, reached once both the age and the clean record mature.
    pub floor: Rate,
    /// Least rate while a confirmed finding is recent; at least `initial`.
    pub after_finding: Rate,
    /// Days since the grant or latest finding before the age matures.
    pub mature_after_days: NonZeroU16,
    /// Clean deliveries since the grant or latest finding before the record matures.
    pub mature_after_merges: NonZeroU32,
    /// Days a confirmed finding or attributed revert counts as recent.
    pub finding_window_days: NonZeroU16,
}

impl RateSchedule {
    fn validate(&self) -> Result<(), SamplingError> {
        if self.floor > self.initial
            || self.initial > self.after_finding
            || self.mature_after_days.get() > MAX_DAYS
            || self.finding_window_days.get() > MAX_DAYS
        {
            return Err(SamplingError::InvalidPolicy);
        }
        Ok(())
    }

    /// The rate for `inputs`. Maturity is the lesser of the age and the clean
    /// record, each as a share of its maturity threshold; the rate falls
    /// linearly from `initial` to `floor` with it, rounding toward `initial`.
    #[must_use]
    pub fn rate(&self, inputs: &RateInputs) -> Rate {
        // At most `u64::MAX` milliseconds, so the products below cannot overflow.
        let age = inputs.at.saturating_since(inputs.clean_since).as_millis();
        let by_age =
            age * u128::from(PER_MILLE) / (u128::from(self.mature_after_days.get()) * DAY_MILLIS);
        let by_merges = u128::from(inputs.clean_merges) * u128::from(PER_MILLE)
            / u128::from(self.mature_after_merges.get());
        let maturity = by_age.min(by_merges).min(u128::from(PER_MILLE));
        let span = u128::from(self.initial.0.saturating_sub(self.floor.0));
        let lowered = span * maturity / u128::from(PER_MILLE);
        // `lowered` is at most `span`, so the result lies between the floor
        // and the initial rate and is never zero.
        let base = u16::try_from(lowered).map_or(self.initial, |lowered| {
            Self::clamp(self.initial.0 - lowered)
        });
        if inputs.recent_findings > 0 {
            base.max(self.after_finding)
        } else {
            base
        }
    }

    const fn clamp(per_mille: u16) -> Rate {
        if per_mille == 0 {
            Rate(1)
        } else {
            Rate(per_mille)
        }
    }

    fn finding_window(&self) -> Duration {
        Duration::from_secs(u64::from(self.finding_window_days.get()) * 86_400)
    }
}

/// A house's inspection sampling rates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SamplingPolicy {
    /// Recorded with every decision; change it whenever a rate changes, so an
    /// old decision replays against the policy it used.
    pub revision: NonZeroU32,
    /// Rates for a work type without its own entry.
    pub default: RateSchedule,
    /// Rates for individual work types.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub work_types: BTreeMap<WorkType, RateSchedule>,
}

impl SamplingPolicy {
    /// Check every schedule and the number of work types.
    ///
    /// # Errors
    /// [`SamplingError::InvalidPolicy`] for more than [`MAX_WORK_TYPE_RATES`]
    /// work types, a floor above the initial rate, an initial rate above the
    /// rate after a finding, or a window longer than [`MAX_DAYS`].
    pub fn validate(&self) -> Result<(), SamplingError> {
        if self.work_types.len() > MAX_WORK_TYPE_RATES {
            return Err(SamplingError::InvalidPolicy);
        }
        self.default.validate()?;
        self.work_types
            .values()
            .try_for_each(RateSchedule::validate)
    }

    /// The schedule for `work_type`.
    #[must_use]
    pub fn schedule(&self, work_type: &WorkType) -> &RateSchedule {
        self.work_types.get(work_type).unwrap_or(&self.default)
    }
}

/// The recorded inputs a rate is computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateInputs {
    /// When the merge grant for this scope took effect.
    pub granted_at: Timestamp,
    /// Start of the current clean record: the grant, or the latest confirmed
    /// finding after it.
    pub clean_since: Timestamp,
    /// Clean live deliveries observed since `clean_since`.
    pub clean_merges: u32,
    /// Confirmed findings and attributed reverts inside the finding window.
    pub recent_findings: u32,
    /// When the rate was judged.
    pub at: Timestamp,
}

impl RateInputs {
    fn validate(&self) -> Result<(), SamplingError> {
        if self.granted_at <= self.clean_since && self.clean_since <= self.at {
            Ok(())
        } else {
            Err(SamplingError::InvalidInput)
        }
    }
}

/// A confirmed finding or attributed revert and when it was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordedFinding {
    /// Stable finding source, used for deduplication.
    pub source: ExternalRef,
    /// When the observation or inspection sample recorded it.
    pub at: Timestamp,
}

/// What the trust ledger holds about one station scope since its merge grant.
/// Only live evidence counts; simulated runs neither lower nor raise a rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRecord {
    /// Station, project, and work type.
    pub scope: StationScope,
    /// When the merge grant for this scope took effect.
    pub granted_at: Timestamp,
    /// When each clean live delivery at or after `granted_at` was observed.
    pub clean: Vec<Timestamp>,
    /// Confirmed findings, reverts, and regressions attributed to the scope,
    /// one per source.
    pub findings: Vec<RecordedFinding>,
}

impl ScopeRecord {
    /// Read the scope's record from the trust ledger. The latest revision of
    /// each observation stream counts: a stream attributed to `scope` with
    /// any confirmed finding, revert, or regression contributes those
    /// findings, and a trust-eligible stream without them, observed at or
    /// after `granted_at`, is one clean delivery. Confirmed inspection
    /// samples of the scope's deliveries are findings at their reservation
    /// time. Records an operator archived no longer count.
    ///
    /// # Errors
    /// Ledger read failures, and a stream in the scope whose revisions have
    /// a gap ([`TrustError::Incomplete`]): missing evidence is not a clean
    /// record.
    pub fn from_ledger(
        ledger: &Ledger,
        scope: &StationScope,
        granted_at: Timestamp,
    ) -> Result<Self, SamplingError> {
        let mut record = Self {
            scope: scope.clone(),
            granted_at,
            clean: Vec::new(),
            findings: Vec::new(),
        };
        ledger.read(|doc| {
            let streams: BTreeSet<&ExternalRef> = doc
                .observations
                .iter()
                .filter(|observation| &observation.attribution.scope == scope)
                .map(|observation| &observation.id)
                .collect();
            for stream in &streams {
                let observation = doc.latest(stream)?;
                if !counts(observation, scope) {
                    continue;
                }
                let Measurement::Observed { value: pr, .. } = &observation.pull_request else {
                    continue;
                };
                for list in [&pr.findings, &pr.reverts, &pr.regressions] {
                    if let Measurement::Observed { value, .. } = list {
                        for finding in value {
                            record.add_finding(&finding.source, observation.observed_at);
                        }
                    }
                }
                // Eligibility already requires every finding list to be empty.
                if observation.trust_eligible() && observation.observed_at >= granted_at {
                    record.clean.push(observation.observed_at);
                }
            }
            for inspection in &doc.inspections {
                if !streams.contains(&inspection.plan().observation) {
                    continue;
                }
                let delivered = doc.latest(&inspection.plan().observation)?;
                if !counts(delivered, scope) {
                    continue;
                }
                for sample in inspection.samples() {
                    if let Some(SampleResult::Confirmed { finding, .. }) = &sample.result {
                        record.add_finding(&finding.source, sample.reserved_at);
                    }
                }
            }
            Ok(())
        })?;
        Ok(record)
    }

    fn add_finding(&mut self, source: &ExternalRef, at: Timestamp) {
        match self
            .findings
            .iter_mut()
            .find(|found| &found.source == source)
        {
            Some(found) => found.at = found.at.min(at),
            None => self.findings.push(RecordedFinding {
                source: source.clone(),
                at,
            }),
        }
    }

    /// The rate inputs at `now` under `schedule`. Records after `now` are
    /// ignored, so a replay at the same time sees the same inputs.
    ///
    /// # Errors
    /// [`SamplingError::InvalidInput`] when `now` is before the grant.
    pub fn inputs(
        &self,
        schedule: &RateSchedule,
        now: Timestamp,
    ) -> Result<RateInputs, SamplingError> {
        self.inputs_without(schedule, now, None)
    }

    fn inputs_without(
        &self,
        schedule: &RateSchedule,
        now: Timestamp,
        excluded: Option<&ExternalRef>,
    ) -> Result<RateInputs, SamplingError> {
        if now < self.granted_at {
            return Err(SamplingError::InvalidInput);
        }
        let findings = || {
            self.findings
                .iter()
                .filter(|finding| finding.at <= now && Some(&finding.source) != excluded)
        };
        let clean_since = findings()
            .map(|finding| finding.at)
            .max()
            .map_or(self.granted_at, |latest| latest.max(self.granted_at));
        let window = schedule.finding_window();
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Ok(RateInputs {
            granted_at: self.granted_at,
            clean_since,
            clean_merges: count(
                self.clean
                    .iter()
                    .filter(|at| clean_since <= **at && **at <= now)
                    .count(),
            ),
            recent_findings: count(
                findings()
                    .filter(|finding| now.saturating_since(finding.at) < window)
                    .count(),
            ),
            at: now,
        })
    }
}

/// Whether `observation` is live evidence attributed to `scope`.
fn counts(observation: &Observation, scope: &StationScope) -> bool {
    observation.mode == EvidenceMode::Live && &observation.attribution.scope == scope
}

/// What happened to one merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Outcome {
    /// The draw was at or above the rate; the merge is not inspected.
    Skipped,
    /// The draw was below the rate and budget remains; inspect it.
    Selected,
    /// The draw was below the rate but the house budget is exhausted: not
    /// inspected, reported to the owner, and the rate is unchanged. Nothing
    /// retries it; the owner decides whether to inspect it by hand.
    BudgetExhausted {
        /// The exhausted house limit.
        exhausted: Exhausted,
    },
}

/// The recorded part of a decision; the merge is its marker key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionRecord {
    /// Policy revision the rate came from.
    pub policy_revision: NonZeroU32,
    /// Station, project, and work type of the merged work.
    pub scope: StationScope,
    /// The inputs the rate was computed from.
    pub inputs: RateInputs,
    /// The rate in effect.
    pub rate: Rate,
    /// The deterministic draw, 0 to 999.
    pub draw: u16,
    /// What happened to the merge.
    pub outcome: Outcome,
}

/// A sampling decision for one merged pull request at an exact head and base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingDecision {
    /// Owning house.
    pub house: HouseId,
    /// The merged pull request.
    pub merge: MergeSubject,
    /// Inputs, rate, draw, and outcome.
    pub record: DecisionRecord,
}

/// Decide whether to inspect `merge`, delivered under `record`'s scope.
/// `budget` is the house usage assessment at `now`; it matters only when the
/// draw picks the merge.
///
/// # Errors
/// [`SamplingError::InvalidPolicy`], [`SamplingError::InvalidInput`] for a
/// scope in another repository or a grant after `now`, and for a picked
/// merge [`SamplingError::StaleBudget`] or [`SamplingError::IncompleteBudget`]
/// when the budget evidence cannot show whether budget remains.
pub fn select(
    policy: &SamplingPolicy,
    house: &HouseId,
    merge: &MergeSubject,
    record: &ScopeRecord,
    budget: &BudgetAssessment,
    now: Timestamp,
) -> Result<SamplingDecision, SamplingError> {
    policy.validate()?;
    if record.scope.project != merge.repository {
        return Err(SamplingError::InvalidInput);
    }
    let schedule = policy.schedule(&record.scope.work_type);
    let inputs = record.inputs(schedule, now)?;
    let rate = schedule.rate(&inputs);
    let draw = draw(house, merge, policy.revision);
    let outcome = if draw >= rate.0 {
        Outcome::Skipped
    } else if !budget.window.contains(now) {
        return Err(SamplingError::StaleBudget);
    } else if let Some(exhausted) = budget.house_exhausted {
        Outcome::BudgetExhausted { exhausted }
    } else if !budget.house.complete {
        return Err(SamplingError::IncompleteBudget);
    } else {
        Outcome::Selected
    };
    Ok(SamplingDecision {
        house: house.clone(),
        merge: merge.clone(),
        record: DecisionRecord {
            policy_revision: policy.revision,
            scope: record.scope.clone(),
            inputs,
            rate,
            draw,
            outcome,
        },
    })
}

/// The draw for `merge`: a SHA-256 digest of length-prefixed fields, reduced
/// to 0..1000.
fn draw(house: &HouseId, merge: &MergeSubject, revision: NonZeroU32) -> u16 {
    let number = u64::from(merge.number).to_string();
    let revision = revision.to_string();
    let mut hasher = Sha256::new();
    for field in [
        DRAW_DOMAIN,
        house.as_str(),
        merge.repository.as_str(),
        &number,
        merge.head.as_str(),
        merge.base.as_str(),
        &revision,
    ] {
        hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    let prefix = digest
        .as_slice()
        .first_chunk::<8>()
        .copied()
        .unwrap_or_default();
    u16::try_from(u64::from_be_bytes(prefix) % u64::from(PER_MILLE)).unwrap_or(0)
}

impl SamplingDecision {
    /// Recompute the rate and draw from the recorded inputs under `policy`
    /// and check that they, and the outcome, match.
    ///
    /// # Errors
    /// [`SamplingError::NotReproducible`] for another policy revision or any
    /// mismatch, [`SamplingError::InvalidInput`] for inconsistent inputs, and
    /// [`SamplingError::InvalidPolicy`].
    pub fn replay(&self, policy: &SamplingPolicy) -> Result<(), SamplingError> {
        policy.validate()?;
        let record = &self.record;
        record.inputs.validate()?;
        if policy.revision != record.policy_revision
            || record.scope.project != self.merge.repository
        {
            return Err(SamplingError::NotReproducible);
        }
        let rate = policy
            .schedule(&record.scope.work_type)
            .rate(&record.inputs);
        let draw = draw(&self.house, &self.merge, policy.revision);
        let picked = draw < rate.0;
        let recorded_pick = match record.outcome {
            Outcome::Skipped => false,
            Outcome::Selected | Outcome::BudgetExhausted { .. } => true,
        };
        if rate != record.rate || draw != record.draw || picked != recorded_pick {
            return Err(SamplingError::NotReproducible);
        }
        Ok(())
    }

    /// The report the owner must receive for this decision, if any.
    #[must_use]
    pub fn report(&self) -> Option<OwnerReport> {
        match self.record.outcome {
            Outcome::BudgetExhausted { exhausted } => Some(OwnerReport::BudgetExhausted {
                scope: self.record.scope.clone(),
                merge: self.merge.clone(),
                rate: self.record.rate,
                exhausted,
            }),
            Outcome::Skipped | Outcome::Selected => None,
        }
    }

    /// Record this decision once in `store`. Recording the same decision
    /// again is [`MarkerRecording::AlreadyRecorded`]; a different decision
    /// for the same merge is refused, so the first one stands.
    ///
    /// # Errors
    /// [`SamplingError::HouseMismatch`] for another house's store,
    /// `StateError::MarkerConflict` for a different recorded decision, and
    /// store failures.
    pub fn record(
        &self,
        store: &HouseStore,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> crate::Result<MarkerRecording> {
        if store.house() != &self.house {
            return Err(SamplingError::HouseMismatch.into());
        }
        let fact = MarkerFact::workflow(decision_schema()?, &self.record)?;
        store.record_marker(decision_key(&self.merge)?, fact, recorded_by, now)
    }

    /// The decision recorded for `merge`, if any.
    ///
    /// # Errors
    /// Store failures, and a marker that does not decode.
    pub fn load(store: &HouseStore, merge: &MergeSubject) -> crate::Result<Option<Self>> {
        let Some(marker) = store.marker(&decision_key(merge)?)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            house: store.house().clone(),
            merge: merge.clone(),
            record: marker.fact().decode(&decision_schema()?)?,
        }))
    }
}

fn decision_schema() -> crate::Result<MarkerSchema> {
    Ok(MarkerSchema::new(DECISION_SCHEMA, NonZeroU32::MIN)?)
}

fn decision_key(merge: &MergeSubject) -> crate::Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(WORKFLOW)?,
        item: WorkItem::PullRequest {
            repository: merge.repository.clone(),
            number: NonZeroU64::new(u64::from(merge.number)).ok_or(SamplingError::InvalidInput)?,
        },
        subject: MarkerSubject::Git(EvidenceSubject {
            head: merge.head.clone(),
            base: Some(merge.base.clone()),
        }),
    })
}

/// The rate change a confirmed finding or attributed revert caused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateRaise {
    /// Policy revision the rates came from.
    pub policy_revision: NonZeroU32,
    /// Station, project, and work type whose rate changed.
    pub scope: StationScope,
    /// The finding's source.
    pub finding: ExternalRef,
    /// The rate without this finding.
    pub from: Rate,
    /// The rate with it; never lower than `from`.
    pub to: Rate,
    /// When the change was judged.
    pub at: Timestamp,
}

impl RateRaise {
    /// The rate change `finding` causes for `record`'s scope at `now`.
    /// `record` must already hold the finding, for example from
    /// [`ScopeRecord::from_ledger`] after the finding was recorded.
    ///
    /// # Errors
    /// [`SamplingError::UnknownFinding`] when `record` lacks the finding,
    /// [`SamplingError::InvalidInput`] for a grant after `now`, and
    /// [`SamplingError::InvalidPolicy`].
    pub fn of(
        policy: &SamplingPolicy,
        record: &ScopeRecord,
        finding: &ExternalRef,
        now: Timestamp,
    ) -> Result<Self, SamplingError> {
        policy.validate()?;
        if !record
            .findings
            .iter()
            .any(|found| &found.source == finding && found.at <= now)
        {
            return Err(SamplingError::UnknownFinding);
        }
        let schedule = policy.schedule(&record.scope.work_type);
        let from = schedule.rate(&record.inputs_without(schedule, now, Some(finding))?);
        let to = schedule.rate(&record.inputs(schedule, now)?);
        Ok(Self {
            policy_revision: policy.revision,
            scope: record.scope.clone(),
            finding: finding.clone(),
            from,
            to,
            at: now,
        })
    }

    /// Record this report once per finding in `store`. The caller delivers
    /// [`OwnerReport::RateRaised`] when this returns
    /// [`MarkerRecording::Recorded`], and not again on
    /// [`MarkerRecording::AlreadyRecorded`].
    ///
    /// # Errors
    /// `StateError::MarkerConflict` when the finding was already reported
    /// with other rates, and store failures.
    pub fn record(
        &self,
        store: &HouseStore,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> crate::Result<MarkerRecording> {
        let key = MarkerKey {
            workflow: WorkflowId::new(WORKFLOW)?,
            item: WorkItem::Repository {
                repository: self.scope.project.clone(),
            },
            subject: MarkerSubject::Observation(self.finding.clone()),
        };
        let fact = MarkerFact::workflow(MarkerSchema::new(RAISE_SCHEMA, NonZeroU32::MIN)?, self)?;
        store.record_marker(key, fact, recorded_by, now)
    }
}

/// Something the house owner must be told. Delivery is the caller's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnerReport {
    /// A confirmed finding or attributed revert raised the scope's rate.
    RateRaised(RateRaise),
    /// A picked merge was not inspected because the house budget is exhausted.
    BudgetExhausted {
        /// Station, project, and work type of the merged work.
        scope: StationScope,
        /// The merge left uninspected.
        merge: MergeSubject,
        /// The unchanged rate.
        rate: Rate,
        /// The exhausted house limit.
        exhausted: Exhausted,
    },
}
