//! Admitting event-started and polled work into the house store.

use std::{collections::BTreeSet, fmt::Write as _, num::NonZeroU32};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ConsumerId, Error, Result, TaskId, WorkflowId,
    contracts::{
        BackendDescriptor, Capability, Claimant, ContractError, EventOrigin, Repository, TaskSpec,
        Timestamp, Trigger,
    },
    events::{EventError, ForgeEvent, ForgeEventKind, PolledWork},
    house::HouseConfig,
    state::{
        Creation, HouseStore, MarkerAttempt, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject,
        StateError, WorkItem, WorkflowMarker,
    },
};

/// Name of the marker schema recording admitted work, version 1.
pub const ADMISSION_SCHEMA: &str = "event.admission";

const TASK_PREFIX: &str = "work-";

/// A workflow that starts from events: which event kinds start it, and the
/// consumer lease its event receiver and fallback schedule share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRoute {
    /// The workflow the admitted work belongs to. Admission markers are
    /// recorded under this id, keyed by item and revision, so the workflow
    /// records its own facts under a different id.
    pub workflow: WorkflowId,
    /// The single-consumer scope. The event receiver and any fallback
    /// schedule acquire this same lease, so they never consume the scope at
    /// the same time.
    pub consumer: ConsumerId,
    /// The event kinds that start work. Other kinds are ignored.
    pub kinds: BTreeSet<ForgeEventKind>,
}

/// The work an admission refers to: the workflow, item, and exact revision,
/// and the task id derived from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkOrder {
    /// The workflow.
    pub workflow: WorkflowId,
    /// The issue or pull request.
    pub item: WorkItem,
    /// The item's repository, bound to the house.
    pub repository: Repository,
    /// The revision the event or poll observed.
    pub subject: MarkerSubject,
    /// The task for this work. Every admission of the same work names it.
    pub task: TaskId,
}

/// The result of offering an event or a polled observation to the intake.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Admission {
    /// New work: the task was created now. This includes finishing an
    /// admission a restart interrupted after its key was recorded.
    Admitted(TaskId),
    /// The same work was already admitted, by this event's earlier delivery,
    /// another event about the same revision, or the fallback poll. Its task
    /// exists; claim it through the store as usual.
    Duplicate(TaskId),
    /// This revision's admission was interrupted, and the store received
    /// another revision of the same item since, so finishing it starts
    /// nothing. Names the newer work's task.
    Stale(TaskId),
    /// The route does not start work for this event kind. Nothing was read or
    /// written.
    Ignored,
}

/// How work was admitted, for audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Via {
    /// A delivered event.
    Event { origin: EventOrigin },
    /// The fallback schedule's poll.
    Poll,
}

/// The marker payload recorded for admitted work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionRecord {
    task: TaskId,
    /// Source time of the event, or the poll's observation time. Recorded
    /// for audit only: a provider or sender supplies an event's time, so it
    /// never orders or deduplicates admissions.
    at: Timestamp,
    via: Via,
}

/// The intake for one event-started workflow in one house.
#[derive(Debug)]
pub struct EventIntake<'a> {
    store: &'a HouseStore,
    house: &'a HouseConfig,
    source: &'a BackendDescriptor,
    route: EventRoute,
    schema: MarkerSchema,
}

impl<'a> EventIntake<'a> {
    /// Activate `route` for `house` with events delivered by `source`.
    ///
    /// # Errors
    /// Returns [`ContractError::UnsupportedCapabilities`] when `source` does
    /// not fully support [`Capability::EventDelivery`],
    /// [`ContractError::CrossHouse`] when the house configuration or source
    /// serves another house than `store`, and [`EventError::EmptyRoute`] when
    /// the route names no event kind.
    pub fn new(
        store: &'a HouseStore,
        house: &'a HouseConfig,
        source: &'a BackendDescriptor,
        route: EventRoute,
    ) -> Result<Self> {
        for found in [&house.house, &source.house] {
            if found != store.house() {
                return Err(ContractError::CrossHouse {
                    expected: store.house().clone(),
                    found: found.clone(),
                }
                .into());
            }
        }
        source.capabilities.require([Capability::EventDelivery])?;
        if route.kinds.is_empty() {
            return Err(EventError::EmptyRoute.into());
        }
        let schema = MarkerSchema::new(ADMISSION_SCHEMA, NonZeroU32::MIN)?;
        Ok(Self {
            store,
            house,
            source,
            route,
            schema,
        })
    }

    /// The route this intake serves.
    #[must_use]
    pub const fn route(&self) -> &EventRoute {
        &self.route
    }

    /// Admit a delivered event. Every refusal of the event or claimant
    /// happens before any state is written. `plan` builds the task for new
    /// work; it must use the order's task id and repository.
    ///
    /// # Errors
    /// Returns [`ContractError::CrossHouse`] for an event delivered for
    /// another house, [`EventError::UnknownSource`] for another source,
    /// [`EventError::RepositoryNotBound`], [`EventError::TriggerMismatch`]
    /// unless the claimant acts under this event's own trigger,
    /// [`EventError::ConsumerRequired`] without the route's consumer lease,
    /// [`EventError::PlanMismatch`], [`StateError::StaleFence`] or
    /// [`StateError::LeaseExpired`] when the consumer lease is no longer
    /// current and live, and other store errors.
    pub fn admit_event(
        &self,
        event: &ForgeEvent,
        claimant: &Claimant,
        plan: impl FnOnce(&WorkOrder) -> Result<TaskSpec>,
        now: Timestamp,
    ) -> Result<Admission> {
        let origin = event.origin();
        if &origin.house != self.store.house() {
            return Err(ContractError::CrossHouse {
                expected: self.store.house().clone(),
                found: origin.house.clone(),
            }
            .into());
        }
        if origin.source != self.source.backend {
            return Err(EventError::UnknownSource.into());
        }
        self.check_bound(event.repository())?;
        if !matches!(&claimant.trigger, Trigger::Event(own) if own == origin) {
            return Err(EventError::TriggerMismatch.into());
        }
        self.check_consumer(claimant)?;
        if !self.route.kinds.contains(&event.kind()) {
            return Ok(Admission::Ignored);
        }
        self.admit(
            event.item(),
            event.repository(),
            event.subject(),
            AdmissionRecord {
                task: self.task_id(event.item(), event.subject())?,
                at: event.occurred_at(),
                via: Via::Event {
                    origin: origin.clone(),
                },
            },
            claimant,
            plan,
            now,
        )
    }

    /// Admit work the fallback schedule's poll observed. It shares the work
    /// identity of events, so a revision already admitted from an event is a
    /// [`Admission::Duplicate`], and the reverse.
    ///
    /// # Errors
    /// As [`Self::admit_event`]; the claimant must be scheduled.
    pub fn admit_polled(
        &self,
        work: &PolledWork,
        claimant: &Claimant,
        plan: impl FnOnce(&WorkOrder) -> Result<TaskSpec>,
        now: Timestamp,
    ) -> Result<Admission> {
        self.check_bound(work.repository())?;
        if claimant.trigger != Trigger::Scheduled {
            return Err(EventError::TriggerMismatch.into());
        }
        self.check_consumer(claimant)?;
        self.admit(
            work.item(),
            work.repository(),
            work.subject(),
            AdmissionRecord {
                task: self.task_id(work.item(), work.subject())?,
                at: work.observed_at(),
                via: Via::Poll,
            },
            claimant,
            plan,
            now,
        )
    }

    fn check_bound(&self, repository: &Repository) -> Result<()> {
        if self.house.repositories.contains(repository) {
            Ok(())
        } else {
            Err(EventError::RepositoryNotBound(repository.clone()).into())
        }
    }

    fn check_consumer(&self, claimant: &Claimant) -> Result<()> {
        match &claimant.consumer {
            Some(fence) if fence.consumer == self.route.consumer => Ok(()),
            Some(_) | None => Err(EventError::ConsumerRequired.into()),
        }
    }

    /// Refuse a claimant whose consumer lease is no longer current and live,
    /// even when the admission would write nothing. The store checks the
    /// lease again on every write.
    fn check_consumer_live(&self, claimant: &Claimant, now: Timestamp) -> Result<()> {
        let Some(fence) = &claimant.consumer else {
            return Err(EventError::ConsumerRequired.into());
        };
        let record = self
            .store
            .consumer(&fence.consumer)?
            .ok_or_else(|| StateError::ConsumerNotFound(fence.consumer.clone()))?;
        match record.lease() {
            Some(lease) if lease.fence() == fence.fence => {
                if lease.is_live(now) {
                    Ok(())
                } else {
                    Err(StateError::LeaseExpired {
                        expired_at: lease.expires_at(),
                    }
                    .into())
                }
            }
            Some(_) | None => Err(StateError::StaleFence {
                presented: fence.fence,
            }
            .into()),
        }
    }

    /// Record the work key first, then create its task. A restart between the
    /// two leaves the key, and the next admission of the same work creates
    /// the task.
    #[expect(
        clippy::too_many_arguments,
        reason = "one private step shared by both admission paths"
    )]
    fn admit(
        &self,
        item: &WorkItem,
        repository: &Repository,
        subject: &MarkerSubject,
        record: AdmissionRecord,
        claimant: &Claimant,
        plan: impl FnOnce(&WorkOrder) -> Result<TaskSpec>,
        now: Timestamp,
    ) -> Result<Admission> {
        self.check_consumer_live(claimant, now)?;
        let key = MarkerKey {
            workflow: self.route.workflow.clone(),
            item: item.clone(),
            subject: subject.clone(),
        };
        let order = WorkOrder {
            workflow: key.workflow.clone(),
            item: item.clone(),
            repository: repository.clone(),
            subject: subject.clone(),
            task: record.task.clone(),
        };
        // The stale scan and the write are one store transaction, so the
        // store's receipt order is the order the scan sees.
        let fact = MarkerFact::workflow(self.schema.clone(), &record)?;
        // A redelivery that finishes an interrupted admission is checked too:
        // another revision may have been received since the key was recorded.
        let recorded_now = match self.store.record_marker_unless_created(
            key.clone(),
            fact,
            claimant,
            now,
            &order.task,
            |markers| self.newer_receipt(markers, &key),
        ) {
            Ok(MarkerAttempt::Recorded(_)) => true,
            Ok(MarkerAttempt::AlreadyRecorded(_)) => false,
            Ok(MarkerAttempt::Blocked(newer)) => return Ok(Admission::Stale(newer)),
            // Another admission of the same work recorded a different fact first.
            Err(Error::State(StateError::MarkerConflict)) => false,
            Err(error) => return Err(error),
        };
        match self.store.task(&order.task) {
            Ok(_) => return Ok(Admission::Duplicate(order.task)),
            Err(Error::State(StateError::TaskNotFound(_))) => {}
            Err(error) => return Err(error),
        }
        let spec = plan(&order)?;
        if spec.id != order.task || spec.repository.as_ref() != Some(&order.repository) {
            return Err(EventError::PlanMismatch.into());
        }
        match self.store.create_task(spec, claimant, now)? {
            Creation::Created => Ok(Admission::Admitted(order.task)),
            Creation::AlreadyExists if recorded_now => Ok(Admission::Admitted(order.task)),
            Creation::AlreadyExists => Ok(Admission::Duplicate(order.task)),
        }
    }

    /// The task of the newest admission the store received for the same item
    /// at another revision after it first received `key`. Ordering uses only
    /// the store's receipt order (markers are kept oldest first), never a
    /// provider or sender time, so a skewed or hostile event time cannot make
    /// real work stale. A revision the store has not received before is
    /// therefore never stale; only finishing an interrupted admission can be.
    fn newer_receipt(
        &self,
        markers: &[&WorkflowMarker],
        key: &MarkerKey,
    ) -> Result<Option<TaskId>> {
        let Some(position) = markers.iter().position(|marker| marker.key() == key) else {
            return Ok(None);
        };
        let mut newest = None;
        for marker in markers.iter().skip(position.saturating_add(1)) {
            let other = marker.key();
            let admission = matches!(
                marker.fact(),
                MarkerFact::Workflow { schema, .. } if schema == &self.schema
            );
            if admission && other.item == key.item && other.subject != key.subject {
                let seen: AdmissionRecord = marker.fact().decode(&self.schema)?;
                newest = Some(seen.task);
            }
        }
        Ok(newest)
    }

    /// The task id for work on `item` at `subject` in this workflow. Stable
    /// across processes and restarts.
    fn task_id(&self, item: &WorkItem, subject: &MarkerSubject) -> Result<TaskId> {
        let encoded = serde_json::to_vec(&(&self.route.workflow, item, subject))
            .map_err(|_| EventError::Encoding)?;
        let mut digest = Sha256::new();
        digest.update(b"kitchen-event-work-v1\0");
        digest.update(self.store.house().as_str().as_bytes());
        digest.update(b"\0");
        digest.update(&encoded);
        let mut id = String::with_capacity(TASK_PREFIX.len() + 48);
        id.push_str(TASK_PREFIX);
        for byte in digest.finalize().iter().take(24) {
            write!(id, "{byte:02x}").map_err(|_| EventError::Encoding)?;
        }
        Ok(TaskId::new(&id)?)
    }
}
