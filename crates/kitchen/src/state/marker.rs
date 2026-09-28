//! Durable workflow markers: facts a workflow records about one work item at
//! one exact evidence subject, such as a report-only verdict for a pull
//! request head or a question already asked about an issue.
//!
//! Markers record facts only. They grant nothing and are separate from
//! effects: a report-only run performs no effect but still records a marker,
//! so the next run can skip the same head or avoid repeating a question.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::{
    WorkflowId,
    contracts::{Claimant, EvidenceSubject, EvidenceVerdict, ExternalRef, Repository, Timestamp},
    state::Corruption,
};

/// Markers per house store.
pub const MAX_MARKERS: usize = 4096;

/// The work item a marker is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WorkItem {
    /// An issue.
    Issue {
        /// The repository.
        repository: Repository,
        /// The issue number.
        number: NonZeroU64,
    },
    /// A pull request.
    PullRequest {
        /// The repository.
        repository: Repository,
        /// The pull-request number.
        number: NonZeroU64,
    },
}

/// What a marker is keyed by: the workflow, the work item, and the exact
/// evidence subject. A moved head or base is a different key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarkerKey {
    /// The workflow that records the fact.
    pub workflow: WorkflowId,
    /// The work item.
    pub item: WorkItem,
    /// The exact head and optional base the fact is about.
    pub subject: EvidenceSubject,
}

/// The fact a marker records.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MarkerFact {
    /// A verdict was reached, for example by a report-only gate.
    Verdict {
        /// The verdict.
        verdict: EvidenceVerdict,
    },
    /// A question was asked; the reference identifies it.
    QuestionAsked {
        /// The question's reference, such as a decision request id.
        question: ExternalRef,
    },
}

/// One recorded marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowMarker {
    key: MarkerKey,
    fact: MarkerFact,
    recorded_by: Claimant,
    recorded_at: Timestamp,
}

impl WorkflowMarker {
    /// The key.
    #[must_use]
    pub const fn key(&self) -> &MarkerKey {
        &self.key
    }

    /// The recorded fact.
    #[must_use]
    pub const fn fact(&self) -> &MarkerFact {
        &self.fact
    }

    /// Who recorded it, and under which trigger.
    #[must_use]
    pub const fn recorded_by(&self) -> &Claimant {
        &self.recorded_by
    }

    /// When it was first recorded.
    #[must_use]
    pub const fn recorded_at(&self) -> Timestamp {
        self.recorded_at
    }
}

/// The result of recording a marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerRecording {
    /// The marker is new.
    Recorded(WorkflowMarker),
    /// The same fact was already recorded under this key; nothing changed.
    AlreadyRecorded(WorkflowMarker),
}

/// The persisted markers of one house.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Markers(Vec<WorkflowMarker>);

/// Why a marker could not be recorded.
pub(super) enum MarkerRefusal {
    /// A different fact is recorded under the key.
    Conflict,
    /// The store holds [`MAX_MARKERS`].
    Full,
}

impl Markers {
    pub(super) const fn new() -> Self {
        Self(Vec::new())
    }

    pub(super) fn get(&self, key: &MarkerKey) -> Option<&WorkflowMarker> {
        self.0.iter().find(|marker| &marker.key == key)
    }

    pub(super) fn for_workflow<'a>(
        &'a self,
        workflow: &'a WorkflowId,
    ) -> impl Iterator<Item = &'a WorkflowMarker> {
        self.0
            .iter()
            .filter(move |marker| &marker.key.workflow == workflow)
    }

    pub(super) fn record(
        &mut self,
        key: MarkerKey,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<MarkerRecording, MarkerRefusal> {
        if let Some(existing) = self.get(&key) {
            return if existing.fact == fact {
                Ok(MarkerRecording::AlreadyRecorded(existing.clone()))
            } else {
                Err(MarkerRefusal::Conflict)
            };
        }
        if self.0.len() >= MAX_MARKERS {
            return Err(MarkerRefusal::Full);
        }
        let marker = WorkflowMarker {
            key,
            fact,
            recorded_by: recorded_by.clone(),
            recorded_at: now,
        };
        self.0.push(marker.clone());
        Ok(MarkerRecording::Recorded(marker))
    }

    /// Markers are bounded and keys are unique.
    pub(super) fn validate(&self) -> Result<(), Corruption> {
        if self.0.len() > MAX_MARKERS {
            return Err(Corruption::LimitExceeded);
        }
        let mut keys = std::collections::BTreeSet::new();
        if self.0.iter().all(|marker| keys.insert(&marker.key)) {
            Ok(())
        } else {
            Err(Corruption::DuplicateWorkflowMarker)
        }
    }
}
