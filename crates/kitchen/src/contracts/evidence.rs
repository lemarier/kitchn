//! Evidence bound to an exact subject revision.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::contracts::{CommitId, ExternalRef, Timestamp};

/// A per-task counter that increases whenever the evidence subject changes,
/// for example when a pull-request head moves. Decisions record the revision
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

/// What an evidence item attests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EvidenceKind {
    /// A CI or local check result.
    Check,
    /// A code review.
    Review,
    /// A scoped human or policy approval.
    Approval,
    /// A worker's own completion report.
    WorkerReport,
    /// A bench or hardware result.
    Bench,
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
    pub subject: CommitId,
    /// Where the evidence can be read.
    pub source: ExternalRef,
    /// When the source produced it.
    pub observed_at: Timestamp,
}
