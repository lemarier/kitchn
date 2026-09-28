//! Bounded post-delivery inspection policy, with durable reservations.
//!
//! This module schedules no workers and posts nothing. The adapter executes one
//! reserved sample with its token ceiling and deadline; uncertain execution keeps
//! that reservation spent. Confirmed findings become scoped issue/test/guidance
//! follow-ups for existing workflows, which still need their own authority.
//!
//! An inspection runs as a core [`Role::Inspector`] task. Starting,
//! reserving, finishing, and cancelling each present that task's fence; the
//! ledger accepts them only while [`InspectionPlan::inspector`] holds a live
//! claim with that fence, and never after a newer fence has acted. Every
//! operation reads the time from the supplied clock once, and a clock that
//! runs backwards is refused. At most [`MAX_OPEN_INSPECTIONS`] inspections
//! are open at once. A recorded result is a routing intent, never authority.
//!
//! The claim is read from the core store inside the ledger transaction, after
//! the ledger lock is taken. A takeover that commits before that read is
//! refused; one that commits after it waits behind the ledger write, as if the
//! write had finished first. The core lock is taken only for that read and is
//! never held while waiting for the ledger lock.
use crate::{
    HolderId, HouseId, TaskId,
    contracts::{Clock, EvidenceSubject, ExternalRef, Fence, Role, Settlement, Text, Timestamp},
    state::{HouseStore, TaskState},
    trust::{Finding, Ledger, Measurement, TrustError},
};
use serde::{Deserialize, Serialize};

/// Inspections that may be open at once in one ledger. An inspection is open
/// until it is cancelled, passes its deadline, or has every sample reserved
/// and finished.
pub const MAX_OPEN_INSPECTIONS: usize = 32;

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
    /// It must hold the claim on [`Self::task`] for every operation.
    pub inspector: HolderId,
    /// The core [`Role::Inspector`] task that runs the samples.
    pub task: TaskId,
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
    /// Newest inspector-task fence that acted; older fences are refused.
    fence: Fence,
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

    /// Whether this inspection still counts against [`MAX_OPEN_INSPECTIONS`].
    fn is_open(&self, now: Timestamp) -> bool {
        !self.cancelled
            && now < self.plan.deadline
            && !(u32::try_from(self.samples.len()).is_ok_and(|len| len >= self.plan.max_samples)
                && self.samples.iter().all(|sample| sample.result.is_some()))
    }

    /// Accept `fence` if it is not older than the newest fence that acted.
    fn advance_fence(&mut self, fence: Fence) -> Result<(), TrustError> {
        if fence < self.fence {
            return Err(TrustError::Refused);
        }
        self.fence = fence;
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
            let earliest = index
                .checked_sub(1)
                .and_then(|previous| self.samples.get(previous))
                .map_or(self.started_at, |previous| previous.reserved_at);
            if sample.tokens == 0
                || u32::try_from(index + 1).ok() != Some(sample.number)
                || sample.reserved_at < earliest
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
    /// Refuse unless `plan.inspector` holds a live claim on the inspector
    /// task with `fence` at `now`. Call it inside the ledger transaction that
    /// the claim authorizes, so no takeover can commit between the two.
    fn check_inspector(
        &self,
        store: &HouseStore,
        plan: &InspectionPlan,
        fence: Fence,
        now: Timestamp,
    ) -> Result<(), TrustError> {
        if store.house() != self.house() {
            return Err(TrustError::Refused);
        }
        let task = store.task(&plan.task).map_err(crate::trust::store_error)?;
        match task.state() {
            TaskState::Claimed { lease }
                if task.spec().role == Role::Inspector
                    && lease.fence() == fence
                    && lease.holder() == &plan.inspector
                    && lease.is_live(now) =>
            {
                Ok(())
            }
            TaskState::Open | TaskState::Claimed { .. } | TaskState::Settled { .. } => {
                Err(TrustError::Refused)
            }
        }
    }

    /// Start an inspection only for positively delivered work and an exact PR
    /// head, under the inspector task's live claim. Repeating an identical plan
    /// returns its existing state without resetting budgets.
    ///
    /// # Errors
    /// Refuses non-delivery, missing PR/agent evidence, failed independence,
    /// an inspector task that is not claimed by the plan's inspector with
    /// `fence`, and bounds. More than [`MAX_OPEN_INSPECTIONS`] open is
    /// `Exhausted`. A failure reading the core store is `Storage`.
    pub fn start_inspection(
        &self,
        store: &HouseStore,
        plan: InspectionPlan,
        fence: Fence,
        clock: &dyn Clock,
    ) -> Result<Inspection, TrustError> {
        self.transact(|doc| {
            let now = clock.now();
            self.check_inspector(store, &plan, fence, now)?;
            if let Some(old) = doc.inspections.iter_mut().find(|i| i.id() == &plan.id) {
                if old.plan != plan {
                    return Err(TrustError::Conflict);
                }
                old.advance_fence(fence)?;
                return Ok(old.clone());
            }
            let observation = doc.latest(&plan.observation)?;
            if !matches!(
                observation.state,
                TaskState::Settled {
                    settlement: Settlement::Succeeded,
                    ..
                }
            ) || observation.task == plan.task
            {
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
            if doc.inspections.iter().filter(|i| i.is_open(now)).count() >= MAX_OPEN_INSPECTIONS {
                return Err(TrustError::Exhausted);
            }
            let inspection = Inspection {
                plan,
                revision: observation.revision,
                subject,
                started_at: now,
                samples: Vec::new(),
                cancelled: false,
                fence,
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
    /// Refuses exhausted/deadline/cancelled/stale inspections, uncertain
    /// samples, a stale or foreign inspector claim, and a clock earlier than
    /// the last reservation.
    pub fn reserve_sample(
        &self,
        store: &HouseStore,
        id: &ExternalRef,
        fence: Fence,
        number: u32,
        tokens: u64,
        clock: &dyn Clock,
    ) -> Result<SampleReservation, TrustError> {
        self.transact(|doc| {
            let now = clock.now();
            let index = doc
                .inspections
                .iter()
                .position(|i| i.id() == id)
                .ok_or(TrustError::Incomplete)?;
            self.check_inspector(store, &doc.inspections[index].plan, fence, now)?;
            doc.inspections[index].advance_fence(fence)?;
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
            let earliest = inspection
                .samples
                .last()
                .map_or(inspection.started_at, |last| last.reserved_at);
            if now < earliest {
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
    /// Rejects conflicting outcomes, stale subjects, nonexistent reservations,
    /// and a stale or foreign inspector claim.
    pub fn finish_sample(
        &self,
        store: &HouseStore,
        id: &ExternalRef,
        fence: Fence,
        number: u32,
        result: SampleResult,
        clock: &dyn Clock,
    ) -> Result<bool, TrustError> {
        self.transact(|doc| {
            let inspection = doc
                .inspections
                .iter_mut()
                .find(|i| i.id() == id)
                .ok_or(TrustError::Incomplete)?;
            self.check_inspector(store, &inspection.plan, fence, clock.now())?;
            inspection.advance_fence(fence)?;
            let sample = inspection
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
    /// Returns missing inspection, a stale or foreign inspector claim, or
    /// storage errors.
    pub fn cancel_inspection(
        &self,
        store: &HouseStore,
        id: &ExternalRef,
        fence: Fence,
        clock: &dyn Clock,
    ) -> Result<(), TrustError> {
        self.transact(|doc| {
            let inspection = doc
                .inspections
                .iter_mut()
                .find(|i| i.id() == id)
                .ok_or(TrustError::Incomplete)?;
            self.check_inspector(store, &inspection.plan, fence, clock.now())?;
            inspection.advance_fence(fence)?;
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
