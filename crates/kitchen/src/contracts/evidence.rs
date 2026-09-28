//! Evidence bound to an exact subject revision.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::contracts::{CommitId, ExternalRef, Timestamp};

/// A per-task counter that increases whenever the evidence subject changes,
/// for example when a pull-request head or its base moves. Decisions record the revision
/// they were made at; effects decided at an older revision are rejected.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct EvidenceRevision(u64);

impl EvidenceRevision {
    /// The revision before any evidence is recorded.
    pub const INITIAL: Self = Self(0);

    pub(crate) const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The raw counter.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EvidenceRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "evidence revision {}", self.0)
    }
}

/// What an evidence item attests. Workflow owners add the kinds their
/// policies evaluate, such as reviews or approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EvidenceKind {
    /// A CI or local check result.
    Check,
    /// A worker's own completion report.
    WorkerReport,
}

/// The exact revision evidence is about: a head commit and, for a change
/// proposed against a base, that base. Moving either makes earlier evidence
/// stale.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceSubject {
    /// The head commit.
    pub head: CommitId,
    /// The base commit the head is compared against, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<CommitId>,
}

/// Whether the evidence supports or contradicts the subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceVerdict {
    /// The attested condition holds.
    Pass,
    /// The attested condition failed.
    Fail,
    /// The source could not establish a result; never treated as a pass.
    Unavailable,
}

/// One piece of evidence about an exact subject revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Evidence {
    /// What is attested.
    pub kind: EvidenceKind,
    /// The result.
    pub verdict: EvidenceVerdict,
    /// The exact revision the evidence is about.
    pub subject: EvidenceSubject,
    /// Where the evidence can be read.
    pub source: ExternalRef,
    /// When the source produced it.
    pub observed_at: Timestamp,
}
