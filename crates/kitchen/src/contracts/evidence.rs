//! Evidence bound to an exact subject revision.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::contracts::{CommitId, ExternalRef, Timestamp, VerificationAccess, VerificationTarget};

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

/// One fact a worker states about its checkout when it reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckoutFact {
    /// The worker did not say, or its report predates the question.
    #[default]
    Unknown,
    /// The worker stated the fact holds.
    Yes,
    /// The worker stated the fact does not hold.
    No,
}

/// The worker's checkout at report time, as the worker stated it. Nothing
/// here was observed by Kitchen; a report that says nothing is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckoutReport {
    /// The checkout had no uncommitted or untracked changes.
    #[serde(default)]
    pub clean: CheckoutFact,
    /// Every local commit was pushed and the checkout's head was the
    /// remote branch tip.
    #[serde(default)]
    pub pushed: CheckoutFact,
}

impl CheckoutReport {
    /// Whether the worker stated both that its checkout was clean and that
    /// it matched the remote branch.
    #[must_use]
    pub const fn clean_and_pushed(self) -> bool {
        matches!(
            (self.clean, self.pushed),
            (CheckoutFact::Yes, CheckoutFact::Yes)
        )
    }
}

/// What an evidence item attests. Workflow owners add the kinds their
/// policies evaluate, such as reviews or approvals.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", from = "EvidenceKindRecord")]
#[non_exhaustive]
pub enum EvidenceKind {
    /// A CI or local check result.
    Check,
    /// A worker's own completion report, with the checkout it stated. A
    /// report recorded before reports carried their checkout reads as
    /// unknown.
    WorkerReport(CheckoutReport),
    /// The forge confirmed that the task's checked push was merged. The
    /// commit identifies the merge, while the subject identifies its head.
    ForgeMerge(CommitId),
    /// A verification run that names its environment but not the access that
    /// ran it. It never satisfies a verification requirement; it remains so
    /// records written before [`Self::AuthorizedVerification`] stay readable.
    Verification(VerificationTarget),
    /// A run of the changed software under the recorded authorized access to
    /// a verification environment. Distinct from checks: CI, unit, and
    /// simulated results never use it.
    AuthorizedVerification(VerificationAccess),
}

/// How an [`EvidenceKind`] is read from a record: the current form, or a
/// worker report written before reports carried their checkout.
#[derive(Deserialize)]
#[serde(untagged)]
enum EvidenceKindRecord {
    Legacy(LegacyEvidenceKind),
    Current(CurrentEvidenceKind),
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LegacyEvidenceKind {
    WorkerReport,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CurrentEvidenceKind {
    Check,
    WorkerReport(CheckoutReport),
    ForgeMerge(CommitId),
    Verification(VerificationTarget),
    AuthorizedVerification(VerificationAccess),
}

impl From<EvidenceKindRecord> for EvidenceKind {
    fn from(record: EvidenceKindRecord) -> Self {
        match record {
            EvidenceKindRecord::Legacy(LegacyEvidenceKind::WorkerReport) => {
                Self::WorkerReport(CheckoutReport::default())
            }
            EvidenceKindRecord::Current(CurrentEvidenceKind::Check) => Self::Check,
            EvidenceKindRecord::Current(CurrentEvidenceKind::WorkerReport(checkout)) => {
                Self::WorkerReport(checkout)
            }
            EvidenceKindRecord::Current(CurrentEvidenceKind::ForgeMerge(commit)) => {
                Self::ForgeMerge(commit)
            }
            EvidenceKindRecord::Current(CurrentEvidenceKind::Verification(target)) => {
                Self::Verification(target)
            }
            EvidenceKindRecord::Current(CurrentEvidenceKind::AuthorizedVerification(access)) => {
                Self::AuthorizedVerification(access)
            }
        }
    }
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
