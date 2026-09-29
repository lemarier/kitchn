//! Proactive ready-to-merge reports.
//!
//! The coordinator reports a pull request as ready to merge as soon as its
//! final review is clean and its required checks are green, both at the
//! exact head and base under review, so the owner never has to ask. A
//! durable marker keyed by that exact subject makes the report once per
//! head: it is recorded as pending before the report is sent and confirmed
//! after, so a crash in between sends it again rather than never. A moved
//! head or base is a new key and arms a new report.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::{
    WorkflowId,
    contracts::{
        Claimant, CommitId, EvidenceSubject, EvidenceVerdict, ExternalRef, IssueNumber, Repository,
        Timestamp,
    },
    state::{HouseStore, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, StateError, WorkItem},
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Evidence about one exact subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadEvidence {
    /// The result.
    pub verdict: EvidenceVerdict,
    /// The head and base it is about.
    pub subject: EvidenceSubject,
    /// Where it can be read.
    pub source: ExternalRef,
}

/// What readiness is decided on for one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReadiness {
    /// The repository.
    pub repository: Repository,
    /// The pull request.
    pub pull_request: IssueNumber,
    /// Its current head and base.
    pub subject: EvidenceSubject,
    /// The final review: `Pass` only when it is clean with nothing
    /// outstanding.
    pub review: HeadEvidence,
    /// The required checks: `Pass` only when every one is green.
    pub checks: HeadEvidence,
}

impl MergeReadiness {
    /// Every reason the pull request is not ready at its current head and
    /// base; empty when it is.
    #[must_use]
    pub fn not_ready(&self) -> Vec<NotReady> {
        let mut reasons = Vec::new();
        if self.review.verdict != EvidenceVerdict::Pass {
            reasons.push(NotReady::ReviewNotClean);
        }
        if self.review.subject != self.subject {
            reasons.push(NotReady::ReviewStale);
        }
        if self.checks.verdict != EvidenceVerdict::Pass {
            reasons.push(NotReady::ChecksNotGreen);
        }
        if self.checks.subject != self.subject {
            reasons.push(NotReady::ChecksStale);
        }
        reasons
    }
}

/// Why a pull request is not ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotReady {
    /// The final review is not clean, or could not be read.
    ReviewNotClean,
    /// The review is about another head or base.
    ReviewStale,
    /// A required check is not green, or could not be read.
    ChecksNotGreen,
    /// The checks are about another head or base.
    ChecksStale,
}

/// A ready-to-merge report for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyReport {
    /// The repository.
    pub repository: Repository,
    /// The pull request.
    pub pull_request: IssueNumber,
    /// The exact head that is ready.
    pub head: CommitId,
    /// The base it was checked against.
    pub base: Option<CommitId>,
    /// The clean final review.
    pub review: ExternalRef,
    /// The green required checks.
    pub checks: ExternalRef,
}

impl ReadyReport {
    /// The one-line message for the owner.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "{}#{} is ready to merge at {}: final review clean ({}) and required checks green ({}) at this head.",
            self.repository,
            self.pull_request.get(),
            self.head,
            self.review,
            self.checks
        )
    }
}

/// What the readiness check decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyDecision {
    /// Send this report, then call [`confirm_ready_reported`].
    Report(ReadyReport),
    /// The report for this exact head was already delivered.
    AlreadyReported,
    /// Not ready, for every listed reason.
    NotReady(Vec<NotReady>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Delivery {
    Pending,
    Delivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadyFact {
    delivery: Delivery,
}

fn schema() -> std::result::Result<MarkerSchema, StateError> {
    MarkerSchema::new("ready-report", NonZeroU32::MIN)
}

fn fact(delivery: Delivery) -> Result<MarkerFact> {
    Ok(MarkerFact::workflow(schema()?, &ReadyFact { delivery })?)
}

fn key(
    repository: &Repository,
    pull_request: IssueNumber,
    subject: &EvidenceSubject,
) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new("ready-report")?,
        item: WorkItem::PullRequest {
            repository: repository.clone(),
            number: NonZeroU64::new(pull_request.get()).ok_or(StateError::MarkerPayloadInvalid)?,
        },
        subject: MarkerSubject::Git(subject.clone()),
    })
}

/// Decide whether `readiness` is ready to report, at most once per exact
/// head and base. A ready pull request whose report is not yet confirmed is
/// reported (again); one confirmed for this subject is not.
///
/// # Errors
/// Returns store and marker failures, such as a full marker store.
pub fn ready_to_merge(
    store: &HouseStore,
    claimant: &Claimant,
    readiness: &MergeReadiness,
    now: Timestamp,
) -> Result<ReadyDecision> {
    let reasons = readiness.not_ready();
    if !reasons.is_empty() {
        return Ok(ReadyDecision::NotReady(reasons));
    }
    let key = key(
        &readiness.repository,
        readiness.pull_request,
        &readiness.subject,
    )?;
    if let Some(marker) = store.marker(&key)? {
        let recorded: ReadyFact = marker.fact().decode(&schema()?)?;
        if recorded.delivery == Delivery::Delivered {
            return Ok(ReadyDecision::AlreadyReported);
        }
    } else {
        store.record_marker(key, fact(Delivery::Pending)?, claimant, now)?;
    }
    Ok(ReadyDecision::Report(ReadyReport {
        repository: readiness.repository.clone(),
        pull_request: readiness.pull_request,
        head: readiness.subject.head.clone(),
        base: readiness.subject.base.clone(),
        review: readiness.review.source.clone(),
        checks: readiness.checks.source.clone(),
    }))
}

/// Record that `report` reached the owner, so the same head is not
/// reported again.
///
/// # Errors
/// Returns [`StateError::MarkerNotFound`] when no pending report is
/// recorded for this head, and other store failures.
pub fn confirm_ready_reported(
    store: &HouseStore,
    claimant: &Claimant,
    report: &ReadyReport,
    now: Timestamp,
) -> Result<()> {
    let subject = EvidenceSubject {
        head: report.head.clone(),
        base: report.base.clone(),
    };
    let key = key(&report.repository, report.pull_request, &subject)?;
    let delivered = fact(Delivery::Delivered)?;
    if store
        .marker(&key)?
        .is_some_and(|marker| marker.fact() == &delivered)
    {
        return Ok(());
    }
    store.supersede_marker(&key, &fact(Delivery::Pending)?, delivered, claimant, now)?;
    Ok(())
}
