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
/// Superseded facts kept per marker; older ones are dropped and counted.
pub const MAX_MARKER_HISTORY: usize = 16;

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<SupersededFact>,
    #[serde(default, skip_serializing_if = "is_zero")]
    dropped_history: u32,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
const fn is_zero(value: &u32) -> bool {
    *value == 0
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

    /// When the current fact was recorded.
    #[must_use]
    pub const fn recorded_at(&self) -> Timestamp {
        self.recorded_at
    }

    /// Superseded facts, oldest first, at most [`MAX_MARKER_HISTORY`].
    #[must_use]
    pub fn history(&self) -> &[SupersededFact] {
        &self.history
    }

    /// How many superseded facts were dropped from the bounded history.
    #[must_use]
    pub const fn dropped_history(&self) -> u32 {
        self.dropped_history
    }
}

/// A fact that a later fact replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupersededFact {
    /// The replaced fact.
    pub fact: MarkerFact,
    /// Who had recorded it.
    pub recorded_by: Claimant,
    /// When it had been recorded.
    pub recorded_at: Timestamp,
    /// When it was replaced.
    pub superseded_at: Timestamp,
}

/// The result of recording a marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerRecording {
    /// The marker is new.
    Recorded(WorkflowMarker),
    /// The same fact was already recorded under this key; nothing changed.
    AlreadyRecorded(WorkflowMarker),
    /// The expected fact was replaced; the prior one is in the history.
    Superseded(WorkflowMarker),
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
    /// No marker is recorded under the key.
    Missing,
    /// The current or new fact is append-only.
    NotSupersedable,
}

impl MarkerFact {
    /// Whether a later fact may replace this one. A question, once asked,
    /// stays recorded so it is never asked again.
    const fn supersedable(&self) -> bool {
        match self {
            Self::Verdict { .. } => true,
            Self::QuestionAsked { .. } => false,
        }
    }
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
            history: Vec::new(),
            dropped_history: 0,
        };
        self.0.push(marker.clone());
        Ok(MarkerRecording::Recorded(marker))
    }

    /// Replace the current fact under `key` if it is still `expected`,
    /// keeping the prior fact in the bounded history. Never refused for
    /// capacity: the oldest history entry is dropped and counted instead.
    pub(super) fn supersede(
        &mut self,
        key: &MarkerKey,
        expected: &MarkerFact,
        fact: MarkerFact,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<MarkerRecording, MarkerRefusal> {
        let Some(marker) = self.0.iter_mut().find(|marker| &marker.key == key) else {
            return Err(MarkerRefusal::Missing);
        };
        if marker.fact == fact {
            return Ok(MarkerRecording::AlreadyRecorded(marker.clone()));
        }
        if &marker.fact != expected {
            return Err(MarkerRefusal::Conflict);
        }
        if !marker.fact.supersedable() || !fact.supersedable() {
            return Err(MarkerRefusal::NotSupersedable);
        }
        if marker.history.len() >= MAX_MARKER_HISTORY {
            marker.history.remove(0);
            marker.dropped_history = marker.dropped_history.saturating_add(1);
        }
        let prior = std::mem::replace(&mut marker.fact, fact);
        marker.history.push(SupersededFact {
            fact: prior,
            recorded_by: std::mem::replace(&mut marker.recorded_by, recorded_by.clone()),
            recorded_at: marker.recorded_at,
            superseded_at: now,
        });
        marker.recorded_at = now;
        Ok(MarkerRecording::Superseded(marker.clone()))
    }

    /// Markers are bounded and keys are unique.
    pub(super) fn validate(&self) -> Result<(), Corruption> {
        if self.0.len() > MAX_MARKERS
            || self
                .0
                .iter()
                .any(|marker| marker.history.len() > MAX_MARKER_HISTORY)
        {
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
