//! Follow-ups held in the house store for a worker that cannot receive them:
//! a person holds its terminal, or its attempt ended.
//!
//! Each held follow-up is a workflow marker on the task, keyed by its
//! follow-up id, so it survives a coordinator restart. It is delivered once:
//! by message when the person released the terminal and the worker runs
//! again, or in the next worker's brief. A message is an effect named by the
//! follow-up id, and a follow-up with any effect of that name is never held
//! for delivery again; one carried in a brief is marked as briefed before the
//! launch runs, so it is never also sent by message. Until a completion
//! addresses it, a briefed follow-up stays in each later brief, like one
//! whose message the worker refused.
//!
//! A task holds at most [`MAX_HELD_FOLLOW_UPS`] follow-ups of at most
//! [`MAX_HELD_FOLLOW_UP_BYTES`] each; more is refused
//! ([`CoordinationError::FollowUpsFull`],
//! [`CoordinationError::FollowUpTooLarge`]), never dropped. Markers are
//! retired once their follow-up was addressed or sent, and when the task
//! settles. A follow-up is held only under the task's current live claim,
//! checked in the same store transaction as the write, so none is held for
//! a task that settled; store retention also retires the markers of a
//! settled task.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{
    TaskId, WorkflowId,
    contracts::{Claimant, ExternalRef, Fence, Text, Timestamp},
    state::{
        EffectState, HouseStore, MarkerAttempt, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject,
        StateError, TaskRecord, TaskState, WorkItem, WorkflowMarker,
    },
    workflows::{coordination::CoordinationError, recovery::QueuedFollowUp},
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Follow-ups a task may hold at once.
pub const MAX_HELD_FOLLOW_UPS: usize = 8;
/// Bytes of one held follow-up's request text. The request must also fit one
/// store marker once encoded, which text full of escaped characters may not.
pub const MAX_HELD_FOLLOW_UP_BYTES: usize = 2048;

const WORKFLOW: &str = "held-follow-up";
const SCHEMA: &str = "coordination.held-follow-up";

/// Where a held follow-up is in its delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Delivery {
    /// Not delivered yet: the next worker receives it by message or brief.
    Waiting,
    /// Carried in a launch's brief; never sent by message.
    Briefed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Stored {
    id: ExternalRef,
    body: Text,
    delivery: Delivery,
}

/// A follow-up held for the task and not yet addressed or sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held {
    key: MarkerKey,
    fact: MarkerFact,
    stored: Stored,
}

impl Held {
    pub(crate) const fn id(&self) -> &ExternalRef {
        &self.stored.id
    }

    pub(crate) const fn body(&self) -> &Text {
        &self.stored.body
    }

    pub(crate) const fn delivery(&self) -> Delivery {
        self.stored.delivery
    }

    pub(crate) fn queued(&self) -> QueuedFollowUp {
        QueuedFollowUp {
            id: self.stored.id.clone(),
            body: self.stored.body.clone(),
        }
    }
}

fn workflow() -> Result<WorkflowId> {
    Ok(WorkflowId::new(WORKFLOW)?)
}

fn schema() -> Result<MarkerSchema> {
    Ok(MarkerSchema::new(SCHEMA, NonZeroU32::MIN)?)
}

fn key(task: &TaskId, id: &ExternalRef) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: workflow()?,
        item: WorkItem::Task { task: task.clone() },
        subject: MarkerSubject::Observation(id.clone()),
    })
}

fn fact(stored: &Stored) -> Result<MarkerFact> {
    MarkerFact::workflow(schema()?, stored).map_err(|error| match error {
        StateError::MarkerPayloadInvalid => CoordinationError::FollowUpTooLarge.into(),
        other => other.into(),
    })
}

/// The claimant owning the task at `fence`, refusing a stale or expired
/// claim. A consumer lease it acts under is checked again when a marker is
/// written.
fn owner(record: &TaskRecord, fence: Fence, now: Timestamp) -> Result<Claimant> {
    match record.state() {
        TaskState::Claimed { lease } if lease.fence() == fence => {
            if !lease.is_live(now) {
                return Err(StateError::LeaseExpired {
                    expired_at: lease.expires_at(),
                }
                .into());
            }
            Ok(Claimant {
                holder: lease.holder().clone(),
                trigger: lease.trigger().clone(),
                consumer: lease.consumer().cloned(),
            })
        }
        TaskState::Open | TaskState::Claimed { .. } | TaskState::Settled { .. } => {
            Err(StateError::StaleFence { presented: fence }.into())
        }
    }
}

fn markers_of<'a>(
    markers: &'a [WorkflowMarker],
    task: &'a TaskId,
) -> impl Iterator<Item = &'a WorkflowMarker> + 'a {
    markers.iter().filter(
        move |marker| matches!(&marker.key().item, WorkItem::Task { task: held } if held == task),
    )
}

/// Hold `body` under `id` for the task's next worker. Holding the same
/// follow-up again changes nothing.
///
/// # Errors
/// Returns [`CoordinationError::FollowUpTooLarge`] for a body over
/// [`MAX_HELD_FOLLOW_UP_BYTES`], [`CoordinationError::FollowUpsFull`] when
/// the task already holds [`MAX_HELD_FOLLOW_UPS`], a claim that is stale,
/// expired, or taken over, a settled task
/// ([`crate::state::StateError::TaskSettled`]), a different request held
/// under the same id, and store failures.
pub(crate) fn hold(
    store: &HouseStore,
    record: &TaskRecord,
    fence: Fence,
    id: &ExternalRef,
    body: &Text,
    now: Timestamp,
) -> Result<()> {
    if body.as_str().len() > MAX_HELD_FOLLOW_UP_BYTES {
        return Err(CoordinationError::FollowUpTooLarge.into());
    }
    let task = &record.spec().id;
    // Follow-ups addressed or sent since the last supervision step do not
    // count against the bound.
    prune(store, task)?;
    let stored = Stored {
        id: id.clone(),
        body: body.clone(),
        delivery: Delivery::Waiting,
    };
    // The claim is checked with the bound and the write: an owner taken over
    // or settled since `record` was read holds nothing that no one delivers.
    let attempt = store.record_task_marker_unless(
        key(task, id)?,
        fact(&stored)?,
        task,
        fence,
        now,
        |markers| {
            let held = markers
                .iter()
                .filter(|marker| {
                    matches!(&marker.key().item, WorkItem::Task { task: held } if held == task)
                })
                .count();
            Ok((held >= MAX_HELD_FOLLOW_UPS).then_some(CoordinationError::FollowUpsFull))
        },
    )?;
    match attempt {
        MarkerAttempt::Recorded(_) | MarkerAttempt::AlreadyRecorded(_) => Ok(()),
        MarkerAttempt::Blocked(refusal) => Err(refusal.into()),
    }
}

/// The follow-ups held for the task that are still outstanding: not
/// addressed by a completion and never sent by message. Oldest first.
///
/// # Errors
/// Returns store failures and a held marker that does not decode.
pub(crate) fn held(store: &HouseStore, record: &TaskRecord) -> Result<Vec<Held>> {
    let markers = store.markers(&workflow()?)?;
    let schema = schema()?;
    markers_of(&markers, &record.spec().id)
        .map(|marker| {
            Ok(Held {
                key: marker.key().clone(),
                fact: marker.fact().clone(),
                stored: marker.fact().decode(&schema)?,
            })
        })
        .filter(|held: &Result<Held>| {
            held.as_ref().map_or(true, |held| {
                !record.has_consumed(held.id()) && !sent(record, held.id())
            })
        })
        .collect()
}

/// Whether an effect named `id` is on record, whatever its outcome.
fn sent(record: &TaskRecord, id: &ExternalRef) -> bool {
    record
        .effects()
        .iter()
        .any(|effect| effect.name().as_str() == id.as_str())
}

/// Mark the waiting follow-ups among `ids` as carried in a brief, before its
/// launch runs, so none is also sent by message.
///
/// # Errors
/// Returns a stale or expired claim and store failures.
pub(crate) fn mark_briefed(
    store: &HouseStore,
    record: &TaskRecord,
    fence: Fence,
    ids: &[ExternalRef],
    now: Timestamp,
) -> Result<()> {
    let waiting: Vec<Held> = held(store, record)?
        .into_iter()
        .filter(|held| held.delivery() == Delivery::Waiting && ids.contains(held.id()))
        .collect();
    if waiting.is_empty() {
        return Ok(());
    }
    let claimant = owner(record, fence, now)?;
    for held in waiting {
        let briefed = fact(&Stored {
            delivery: Delivery::Briefed,
            ..held.stored
        })?;
        store.supersede_marker(&held.key, &held.fact, briefed, &claimant, now)?;
    }
    Ok(())
}

/// Retire the task's held follow-ups that no longer matter: every one once
/// the task settled, otherwise those addressed by a completion or sent.
///
/// # Errors
/// Returns store failures.
pub(crate) fn prune(store: &HouseStore, task: &TaskId) -> Result<()> {
    let record = store.task(task)?;
    let settled = matches!(record.state(), TaskState::Settled { .. });
    let markers = store.markers(&workflow()?)?;
    let schema = schema()?;
    let done: Vec<(MarkerKey, MarkerFact)> = markers_of(&markers, task)
        .filter(|marker| {
            settled
                || marker.fact().decode::<Stored>(&schema).is_ok_and(|stored| {
                    record.has_consumed(&stored.id) || resolved(&record, &stored.id)
                })
        })
        .map(|marker| (marker.key().clone(), marker.fact().clone()))
        .collect();
    if !done.is_empty() {
        store.retire_markers(&done)?;
    }
    Ok(())
}

/// Whether an effect named `id` reached a known outcome. Its effect record
/// then carries the follow-up; an unresolved one keeps the marker until it
/// is reconciled.
fn resolved(record: &TaskRecord, id: &ExternalRef) -> bool {
    record.effects().iter().any(|effect| {
        effect.name().as_str() == id.as_str()
            && matches!(
                effect.state(),
                EffectState::Applied { .. } | EffectState::NotApplied { .. }
            )
    })
}
