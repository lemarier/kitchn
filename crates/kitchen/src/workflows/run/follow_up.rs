//! Durable input for a scheduled review-thread follow-up round.
//!
//! A launch records the exact head and complete set of thread IDs before a
//! worker runs. Coordination validates the report against that set and records
//! each disposition under the round's live fence before settling it. Replayed
//! reports can only record the same facts.

use std::{collections::BTreeSet, num::NonZeroU32};

use serde::{Deserialize, Serialize};

use super::RunError;
use crate::{
    TaskId, WorkflowId,
    contracts::{CommitId, ExternalRef, Fence, IssueNumber, Text, Timestamp},
    integrations::github::FollowUpThread,
    state::{HouseStore, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem},
};

type Result<T> = std::result::Result<T, crate::Error>;

const MAX_THREADS: usize = 32;

/// A worker's decision about one review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FollowUpVerdict {
    /// The finding was fixed in the pushed branch.
    Fixed,
    /// The finding was declined with a reason and needs an owner decision.
    Declined,
}

/// A single durable thread decision, recorded before the task settles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FollowUpDisposition {
    /// GraphQL thread node ID from the launch snapshot.
    pub thread: ExternalRef,
    /// Whether the worker fixed or declined the finding.
    pub verdict: FollowUpVerdict,
    /// The text Kitchen posts as a reply, also sent to the house mailbox
    /// when a finding was declined.
    pub reply: Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FollowUpReport {
    pub source_head: CommitId,
    pub dispositions: Vec<FollowUpDisposition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FollowUpSnapshot {
    pub pull_request: IssueNumber,
    pub source_head: CommitId,
    pub threads: Vec<ThreadTarget>,
    pub reviews: Vec<ReviewTarget>,
}

/// A change-request review and the digest of its text at launch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReviewTarget {
    pub id: u64,
    pub body_digest: u64,
}

/// The latest reviewer comment in a thread at the launched head. A new
/// reviewer comment changes this target even when the thread ID is stable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ThreadTarget {
    pub id: ExternalRef,
    pub latest_reviewer_comment: ExternalRef,
    pub content_digest: u64,
}

pub(crate) fn target(thread: &FollowUpThread, requester: &str) -> Result<ThreadTarget> {
    let latest = thread
        .comments
        .nodes
        .iter()
        .rev()
        .find(|comment| {
            comment
                .author
                .as_ref()
                .is_none_or(|author| author.login != requester)
        })
        .ok_or(RunError::DispositionInvalid)?;
    Ok(ThreadTarget {
        id: ExternalRef::new(&thread.id)?,
        latest_reviewer_comment: ExternalRef::new(&latest.id)?,
        content_digest: super::super::pickup::stable_hash(
            &serde_json::to_vec(&(
                &latest.body,
                &thread.path,
                thread.line,
                thread.original_line,
            ))
            .map_err(|_| RunError::DispositionInvalid)?,
        ),
    })
}

pub(crate) fn record_changed(
    store: &HouseStore,
    task: &TaskId,
    fence: Fence,
    thread: &ExternalRef,
    now: Timestamp,
) -> Result<()> {
    store.record_task_marker_unless(
        changed_key(task, thread)?,
        MarkerFact::workflow(schema()?, &"changed-since-launch")?,
        task,
        fence,
        now,
        |_| Ok(None::<()>),
    )?;
    Ok(())
}

pub(crate) fn changed(store: &HouseStore, task: &TaskId, thread: &ExternalRef) -> Result<bool> {
    Ok(store.marker(&changed_key(task, thread)?)?.is_some())
}

fn changed_key(task: &TaskId, thread: &ExternalRef) -> Result<MarkerKey> {
    let digest = super::super::pickup::stable_hash(thread.as_str().as_bytes());
    key(task, ExternalRef::new(&format!("changed-{digest:016x}"))?)
}

fn workflow() -> Result<WorkflowId> {
    Ok(WorkflowId::new("review-follow-up")?)
}

fn schema() -> Result<MarkerSchema> {
    Ok(MarkerSchema::new("review-follow-up", NonZeroU32::MIN)?)
}

fn key(task: &TaskId, subject: ExternalRef) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: workflow()?,
        item: WorkItem::Task { task: task.clone() },
        subject: MarkerSubject::Observation(subject),
    })
}

fn snapshot_key(task: &TaskId) -> Result<MarkerKey> {
    key(task, ExternalRef::new("snapshot")?)
}

pub(crate) fn snapshot(store: &HouseStore, task: &TaskId) -> Result<Option<FollowUpSnapshot>> {
    store
        .marker(&snapshot_key(task)?)?
        .map(|marker| marker.fact().decode(&schema()?).map_err(Into::into))
        .transpose()
}

pub(crate) fn record_snapshot(
    store: &HouseStore,
    task: &TaskId,
    fence: Fence,
    value: &FollowUpSnapshot,
    now: Timestamp,
) -> Result<()> {
    if (value.threads.is_empty() && value.reviews.is_empty())
        || value.threads.len() > MAX_THREADS
        || value.reviews.len() > MAX_THREADS
        || value
            .threads
            .iter()
            .any(|thread| thread.id.as_str() == "snapshot")
        || value
            .threads
            .iter()
            .map(|thread| &thread.id)
            .collect::<BTreeSet<_>>()
            .len()
            != value.threads.len()
        || value
            .reviews
            .iter()
            .map(|review| review.id)
            .collect::<BTreeSet<_>>()
            .len()
            != value.reviews.len()
    {
        return Err(RunError::DispositionInvalid.into());
    }
    store.record_task_marker_unless(
        snapshot_key(task)?,
        MarkerFact::workflow(schema()?, value)?,
        task,
        fence,
        now,
        |_| Ok(None::<()>),
    )?;
    Ok(())
}

/// Validate a report completely before its first durable write. A crash
/// between marker writes is safe: redelivery writes identical facts, and
/// coordination does not settle the task until all writes succeed.
pub(crate) fn record_report(
    store: &HouseStore,
    task: &TaskId,
    fence: Fence,
    body: Option<&Text>,
    now: Timestamp,
) -> Result<FollowUpReport> {
    let expected = snapshot(store, task)?.ok_or(RunError::DispositionInvalid)?;
    let report: FollowUpReport =
        serde_json::from_str(body.ok_or(RunError::DispositionInvalid)?.as_str())
            .map_err(|_| RunError::DispositionInvalid)?;
    if report.source_head != expected.source_head
        || report.dispositions.len() != expected.threads.len()
    {
        return Err(RunError::DispositionInvalid.into());
    }
    let mut seen = BTreeSet::new();
    for item in &report.dispositions {
        if !seen.insert(&item.thread)
            || !expected
                .threads
                .iter()
                .any(|thread| thread.id == item.thread)
            || item.reply.as_str().trim().is_empty()
        {
            return Err(RunError::DispositionInvalid.into());
        }
    }
    let prepared = report
        .dispositions
        .iter()
        .map(|item| {
            Ok((
                key(task, item.thread.clone())?,
                MarkerFact::workflow(schema()?, item).map_err(|_| RunError::DispositionInvalid)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    for (key, fact) in prepared {
        store.record_task_marker_unless(key, fact, task, fence, now, |_| Ok(None::<()>))?;
    }
    Ok(report)
}
