//! Bounded post-delivery inspection policy, with durable reservations.
//!
//! This module schedules no workers and posts nothing. The adapter executes one
//! reserved sample with its token ceiling and deadline; uncertain execution keeps
//! that reservation spent. Confirmed findings become scoped issue/test/guidance
//! follow-ups for existing workflows, which still need their own authority.
//!
//! The ledger records inspections; it does not run or fence them. Reserving,
//! finishing, and cancelling take no claim, so the identity that reports a
//! result is not compared with [`InspectionPlan::inspector`]. Independence is
//! checked once, when the inspection starts, against the delivering agent the
//! adapter attested. Deadlines use the caller's clock, and only the ledger's
//! history limit bounds how many inspections start. A recorded result is a
//! routing intent, never authority.
use crate::{
    HolderId, HouseId,
    contracts::{EvidenceSubject, ExternalRef, Settlement, Text, Timestamp},
    state::TaskState,
    trust::{Finding, Ledger, Measurement, TrustError},
};
use serde::{Deserialize, Serialize};

/// Bounds accepted by the inspector, independent of backend capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectionPlan {
    /// Idempotent inspection identity.
    pub id: ExternalRef,
    /// Credential/runtime house.
    pub house: HouseId,
    /// Delivered observation stream.
    pub observation: ExternalRef,
    /// Concrete question to answer about the delivered revision.
    pub question: Text,
    /// Inspector identity, distinct from the delivering worker when required.
    /// The ledger does not verify who runs the samples.
    pub inspector: HolderId,
    /// Require positive evidence of a different delivering agent. Checked when
    /// the inspection starts, against the attested agent of the observation.
    pub independent: bool,
    /// Maximum samples, 1 through 32.
    pub max_samples: u32,
    /// Total reserved tokens, 1 through 1,000,000.
    pub max_tokens: u64,
    /// Absolute deadline, at most one hour after creation.
    pub deadline: Timestamp,
}

/// Where a confirmed finding should be consumed. This is intent, not a post.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum FollowUpRoute {
    /// Existing issue/triage workflow.
    Issue,
    /// Regression test work through the task coordinator.
    Test,
    /// House guidance change through its review workflow.
    Guidance,
}

/// Sample outcome. A missing result is distinct from an unavailable result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum SampleResult {
    /// Question checked with no confirmed finding; not general acceptance.
    NoFinding {
        /// Evidence answering the question.
        source: ExternalRef,
    },
    /// Could not establish an answer.
    Unavailable,
    /// Investigated finding at the exact delivered revision.
    Confirmed {
        /// Confirmed finding and source.
        finding: Finding,
        /// Existing workflow that should consume it.
        route: FollowUpRoute,
    },
}

/// Durable reservation returned before launching any inspection work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sample {
    /// One-based sample number, stable across restart.
    pub number: u32,
    /// Maximum token budget charged before execution; never refunded on uncertainty.
    pub tokens: u64,
    /// Time of reservation.
    pub reserved_at: Timestamp,
    /// Result supplied by the adapter, or none for an interrupted/in-flight sample.
    pub result: Option<SampleResult>,
}

/// Whether this call created a reservation or merely recovered one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleReservation {
    /// Persisted now; execute at most once using the inspection/sample identity.
    Reserved(Sample),
    /// Already reserved: reconcile its backend identity; never launch again.
    Existing(Sample),
}

/// Audit record for one bounded inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inspection {
    plan: InspectionPlan,
    revision: std::num::NonZeroU32,
    subject: EvidenceSubject,
    started_at: Timestamp,
    samples: Vec<Sample>,
    cancelled: bool,
}
impl Inspection {
    /// Inspection idempotency identity.
    #[must_use]
    pub const fn id(&self) -> &ExternalRef {
        &self.plan.id
    }
    /// Plan, including concrete question, inspector, and bounds.
    #[must_use]
    pub const fn plan(&self) -> &InspectionPlan {
        &self.plan
    }
    /// Reserved samples and their known outcomes.
    #[must_use]
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }
    /// Confirmed findings routed to existing workflows. Consumers use inspection
    /// identity plus sample number as their durable idempotency key and append a
    /// sourced correction to the station record after investigating attribution.
    pub fn follow_ups(&self) -> impl Iterator<Item = (u32, &Finding, FollowUpRoute)> {
        self.samples
            .iter()
            .filter_map(|sample| match &sample.result {
                Some(SampleResult::Confirmed { finding, route }) => {
                    Some((sample.number, finding, *route))
                }
                Some(SampleResult::NoFinding { .. } | SampleResult::Unavailable) | None => None,
            })
    }
    /// Observation revision the inspection was started against.
    pub(crate) const fn revision(&self) -> std::num::NonZeroU32 {
        self.revision
    }
    /// Check the inspection against its observation revision, looked up by
    /// the caller from [`Self::plan`] and [`Self::revision`].
    pub(crate) fn validate_observation(
        &self,
        observation: &crate::trust::Observation,
    ) -> Result<(), TrustError> {
        if !matches!(
            observation.state,
            TaskState::Settled {
                settlement: Settlement::Succeeded,
                ..
            }
        ) || !matches!(&observation.pull_request, Measurement::Observed { value, .. } if value.subject == self.subject)
            || (self.plan.independent
                && !matches!(&observation.attribution.agent, Measurement::Observed { value, .. } if value != &self.plan.inspector))
        {
            return Err(TrustError::Corrupt);
        }
        Ok(())
    }

    pub(crate) fn validate(&self, house: &HouseId) -> Result<(), TrustError> {
        if &self.plan.house != house {
            return Err(TrustError::Refused);
        }
        if !(1..=32).contains(&self.plan.max_samples)
            || !(1..=1_000_000).contains(&self.plan.max_tokens)
            || self.plan.deadline <= self.started_at
            || self.plan.deadline.saturating_since(self.started_at)
                > std::time::Duration::from_secs(3600)
            || u32::try_from(self.samples.len()).map_or(true, |len| len > self.plan.max_samples)
        {
            return Err(TrustError::Invalid);
        }
        let mut spent = 0u64;
        for (index, sample) in self.samples.iter().enumerate() {
            spent = spent
                .checked_add(sample.tokens)
                .ok_or(TrustError::Exhausted)?;
            if sample.tokens == 0
                || u32::try_from(index + 1).ok() != Some(sample.number)
                || sample.reserved_at < self.started_at
                || sample.reserved_at >= self.plan.deadline
            {
                return Err(TrustError::Invalid);
            }
            if let Some(SampleResult::Confirmed { finding, .. }) = &sample.result {
                let duplicate_source = self.samples[..index].iter().any(|old| {
                    matches!(&old.result, Some(SampleResult::Confirmed { finding: prior, .. }) if prior.source == finding.source)
                });
                if finding.subject != self.subject || duplicate_source {
                    return Err(TrustError::Refused);
                }
            }
        }
        if spent > self.plan.max_tokens {
            return Err(TrustError::Exhausted);
        }
        Ok(())
    }
}

impl Ledger {
    /// Start an inspection only for positively delivered work and an exact PR head.
    /// Repeating an identical plan returns its existing state without resetting budgets.
    ///
    /// # Errors
    /// Refuses non-delivery, missing PR/agent evidence, failed independence, and bounds.
    pub fn start_inspection(
        &self,
        plan: InspectionPlan,
        now: Timestamp,
    ) -> Result<Inspection, TrustError> {
        self.transact(|doc| {
            if let Some(old) = doc.inspections.iter().find(|i| i.id() == &plan.id) {
                return if old.plan == plan {
                    Ok(old.clone())
                } else {
                    Err(TrustError::Conflict)
                };
            }
            let observation = doc.latest(&plan.observation)?;
            if !matches!(
                observation.state,
                TaskState::Settled {
                    settlement: Settlement::Succeeded,
                    ..
                }
            ) {
                return Err(TrustError::Refused);
            }
            if plan.independent {
                match &observation.attribution.agent {
                    Measurement::Observed { value, .. } if value != &plan.inspector => {}
                    _ => return Err(TrustError::Refused),
                }
            }
            let subject = match &observation.pull_request {
                Measurement::Observed { value, .. } => value.subject.clone(),
                _ => return Err(TrustError::Incomplete),
            };
            let inspection = Inspection {
                plan,
                revision: observation.revision,
                subject,
                started_at: now,
                samples: Vec::new(),
                cancelled: false,
            };
            inspection.validate(self.house())?;
            doc.inspections.push(inspection.clone());
            Ok(inspection)
        })
    }

    /// Reserve one sample before execution. A caller-supplied one-based number
    /// makes retries idempotent; a pending sample must be reconciled before another.
    ///
    /// # Errors
    /// Refuses exhausted/deadline/cancelled/stale inspections and uncertain samples.
    pub fn reserve_sample(
        &self,
        id: &ExternalRef,
        number: u32,
        tokens: u64,
        now: Timestamp,
    ) -> Result<SampleReservation, TrustError> {
        self.transact(|doc| {
            let index = doc
                .inspections
                .iter()
                .position(|i| i.id() == id)
                .ok_or(TrustError::Incomplete)?;
            let inspection = &doc.inspections[index];
            if let Some(old) = inspection.samples.iter().find(|s| s.number == number) {
                return if old.tokens == tokens {
                    Ok(SampleReservation::Existing(old.clone()))
                } else {
                    Err(TrustError::Conflict)
                };
            }
            if inspection.cancelled {
                return Err(TrustError::Refused);
            }
            if now < inspection.started_at {
                return Err(TrustError::Invalid);
            }
            if now >= inspection.plan.deadline {
                return Err(TrustError::Exhausted);
            }
            let current = doc.latest(&inspection.plan.observation)?;
            if current.revision != inspection.revision
                || !matches!(&current.pull_request, Measurement::Observed { value, .. } if value.subject == inspection.subject)
            {
                return Err(TrustError::Refused);
            }
            if inspection.samples.iter().any(|s| s.result.is_none()) {
                return Err(TrustError::Incomplete);
            }
            if u32::try_from(inspection.samples.len() + 1).ok() != Some(number) {
                return Err(TrustError::Invalid);
            }
            if u32::try_from(inspection.samples.len())
                .map_or(true, |len| len >= inspection.plan.max_samples)
                || tokens == 0
                || tokens
                    > inspection
                        .plan
                        .max_tokens
                        .saturating_sub(inspection.samples.iter().map(|s| s.tokens).sum())
            {
                return Err(TrustError::Exhausted);
            }
            let sample = Sample {
                number,
                tokens,
                reserved_at: now,
                result: None,
            };
            doc.inspections[index].samples.push(sample.clone());
            Ok(SampleReservation::Reserved(sample))
        })
    }

    /// Record one result once. Late results are retained after cancellation/deadline;
    /// they grant no new execution. A changed observation requires a new inspection.
    ///
    /// # Errors
    /// Rejects conflicting outcomes, stale subjects, and nonexistent reservations.
    pub fn finish_sample(
        &self,
        id: &ExternalRef,
        number: u32,
        result: SampleResult,
    ) -> Result<bool, TrustError> {
        self.transact(|doc| {
            let index = doc
                .inspections
                .iter()
                .position(|i| i.id() == id)
                .ok_or(TrustError::Incomplete)?;
            let sample = doc.inspections[index]
                .samples
                .iter_mut()
                .find(|s| s.number == number)
                .ok_or(TrustError::Incomplete)?;
            if let Some(old) = &sample.result {
                return if old == &result {
                    Ok(false)
                } else {
                    Err(TrustError::Conflict)
                };
            }
            sample.result = Some(result);
            Ok(true)
        })
    }

    /// Prevent new samples while retaining uncertain reservations and all findings.
    ///
    /// # Errors
    /// Returns missing inspection or storage errors.
    pub fn cancel_inspection(&self, id: &ExternalRef) -> Result<(), TrustError> {
        self.transact(|doc| {
            let inspection = doc
                .inspections
                .iter_mut()
                .find(|i| i.id() == id)
                .ok_or(TrustError::Incomplete)?;
            inspection.cancelled = true;
            Ok(())
        })
    }

    /// Read persisted inspection and its pending routing intents after restart.
    ///
    /// # Errors
    /// Returns missing inspection or storage errors.
    pub fn inspection(&self, id: &ExternalRef) -> Result<Inspection, TrustError> {
        self.read(|doc| {
            doc.inspections
                .iter()
                .find(|i| i.id() == id)
                .cloned()
                .ok_or(TrustError::Incomplete)
        })
    }
}
