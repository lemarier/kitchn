//! Attested gate evidence: the independent review, acceptance, hardware,
//! and risk facts the merge gate needs besides the forge's own
//! ([`crate::workflows::gate::GateSupplement`]), recorded in the house store
//! as a `gate.attestation/1` workflow marker keyed by the pull request and
//! its exact head and base.
//!
//! Only [`record_gate_attestation`] writes one. It refuses a reviewer who
//! wrote the branch: a person who is the pull request's author, or a worker
//! that a launch of the branch created. A recorded attestation is never
//! rewritten: a different one for the same subject is refused, and a moved
//! head or base needs a new attestation. The scheduled gate reads it back
//! with [`gate_attestation`] and checks independence again against the
//! author the forge reports.
//!
//! Nothing in Kitchen records an attestation yet, so until a reviewer
//! workflow does, the scheduled gate reports every pull request as
//! unattested and merges nothing.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use super::RunError;
use crate::{
    HouseId, WorkflowId,
    contracts::{
        BranchName, Claimant, CommitId, ContractError, EvidenceSubject, ExternalRef, IssueNumber,
        Repository, ResourceRef, Timestamp, ValueKind,
    },
    state::{
        HouseStore, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, StateError, TaskRecord,
        WorkItem,
    },
    workflows::{
        coordination::launched_workers,
        gate::{RiskClass, SemanticReview},
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Workflow id under which gate attestations are recorded.
pub const GATE_ATTESTATION_WORKFLOW: &str = "merge-gate-attestation";

const SCHEMA: &str = "gate.attestation";

/// Who performed the attested review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Reviewer {
    /// A person, by forge login.
    Person {
        /// The login.
        login: String,
    },
    /// A Kitchen worker, by its backend resource.
    Worker {
        /// The worker.
        worker: ResourceRef,
    },
}

/// What an independent reviewer attests about one pull request at one exact
/// head and base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateAttestation {
    /// The house the attestation belongs to.
    pub house: HouseId,
    /// The repository.
    pub repository: Repository,
    /// The pull request.
    pub pull_request: IssueNumber,
    /// The exact head reviewed.
    pub head: CommitId,
    /// The exact base the diff was compared against.
    pub base: CommitId,
    /// Who reviewed.
    pub reviewer: Reviewer,
    /// Where the review record can be read.
    pub source: ExternalRef,
    /// The review's result.
    pub review: SemanticReview,
    /// The review read committed content without running the pull
    /// request's code with credentials.
    pub read_only: bool,
    /// The linked issue's acceptance evidence is complete.
    pub acceptance_met: bool,
    /// Required hardware work is complete, or none is required.
    pub hardware_complete: bool,
    /// The complete risk classification of the diff. A class needs a human
    /// approval this record does not carry, so the gate will not merge it.
    pub risk_classes: Vec<RiskClass>,
}

/// Record `attestation` for its exact subject. `author` is the pull
/// request's author and `branch` its head branch, as the forge reports
/// them; the reviewer must be neither the author nor a worker a launch of
/// `branch` created. Recording the same attestation again is a no-op.
///
/// # Errors
/// [`ContractError::CrossHouse`] for another house,
/// [`RunError::AttestationNotIndependent`] for a reviewer who wrote the
/// branch or an empty login, [`RunError::AttestationRecorded`] when a
/// different attestation exists for the subject, and store errors.
pub fn record_gate_attestation(
    store: &HouseStore,
    attestation: &GateAttestation,
    author: &str,
    branch: &BranchName,
    recorded_by: &Claimant,
    now: Timestamp,
) -> Result<()> {
    if &attestation.house != store.house() {
        return Err(ContractError::CrossHouse {
            expected: store.house().clone(),
            found: attestation.house.clone(),
        }
        .into());
    }
    let writers = branch_writers(&store.tasks()?, branch);
    if !independent(&attestation.reviewer, Some(author), &writers) {
        return Err(RunError::AttestationNotIndependent.into());
    }
    let key = key(
        &attestation.repository,
        attestation.pull_request,
        &attestation.head,
        &attestation.base,
    )?;
    let fact = MarkerFact::workflow(schema()?, attestation)?;
    match store.record_marker(key, fact, recorded_by, now) {
        Ok(_) => Ok(()),
        Err(crate::Error::State(StateError::MarkerConflict)) => {
            Err(RunError::AttestationRecorded.into())
        }
        Err(error) => Err(error),
    }
}

/// The attestation recorded for exactly this pull request, head, and base,
/// if any.
///
/// # Errors
/// [`StateError::MarkerPayloadInvalid`] for a payload that describes
/// another subject or house, and store errors.
pub fn gate_attestation(
    store: &HouseStore,
    repository: &Repository,
    pull_request: IssueNumber,
    head: &CommitId,
    base: &CommitId,
) -> Result<Option<GateAttestation>> {
    let Some(marker) = store.marker(&key(repository, pull_request, head, base)?)? else {
        return Ok(None);
    };
    let attestation: GateAttestation = marker.fact().decode(&schema()?)?;
    if &attestation.house != store.house()
        || &attestation.repository != repository
        || attestation.pull_request != pull_request
        || &attestation.head != head
        || &attestation.base != base
    {
        return Err(StateError::MarkerPayloadInvalid.into());
    }
    Ok(Some(attestation))
}

/// Whether `reviewer` did not write the branch. A person must differ from
/// the pull request's `author`, which must be known; a worker must not be
/// one of the branch's `writers`.
pub(super) fn independent(
    reviewer: &Reviewer,
    author: Option<&str>,
    writers: &[ResourceRef],
) -> bool {
    match reviewer {
        Reviewer::Person { login } => {
            !login.is_empty() && author.is_some_and(|author| !author.eq_ignore_ascii_case(login))
        }
        Reviewer::Worker { worker } => !writers.contains(worker),
    }
}

/// Every worker an applied launch of any task created on `branch`.
pub(super) fn branch_writers(tasks: &[TaskRecord], branch: &BranchName) -> Vec<ResourceRef> {
    tasks
        .iter()
        .flat_map(launched_workers)
        .filter(|view| view.branch.as_ref() == Some(branch))
        .map(|view| view.worker)
        .collect()
}

fn schema() -> std::result::Result<MarkerSchema, StateError> {
    MarkerSchema::new(SCHEMA, NonZeroU32::MIN)
}

fn key(
    repository: &Repository,
    pull_request: IssueNumber,
    head: &CommitId,
    base: &CommitId,
) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(GATE_ATTESTATION_WORKFLOW)?,
        item: WorkItem::PullRequest {
            repository: repository.clone(),
            number: NonZeroU64::new(pull_request.get()).ok_or(ContractError::InvalidValue {
                kind: ValueKind::Text,
            })?,
        },
        subject: MarkerSubject::Git(EvidenceSubject {
            head: head.clone(),
            base: Some(base.clone()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::{Reviewer, independent};
    use crate::{
        BackendId,
        contracts::{ExternalRef, ResourceKind, ResourceRef},
    };

    fn worker(handle: &str) -> Result<ResourceRef, Box<dyn std::error::Error>> {
        Ok(ResourceRef {
            kind: ResourceKind::Worker,
            backend: BackendId::new("fake")?,
            handle: ExternalRef::new(handle)?,
        })
    }

    #[test]
    fn a_person_must_differ_from_a_known_author() {
        let person = Reviewer::Person {
            login: "Reviewer".to_owned(),
        };
        assert!(independent(&person, Some("kitchen-bot"), &[]));
        assert!(!independent(&person, Some("reviewer"), &[]));
        assert!(!independent(&person, None, &[]));
        let empty = Reviewer::Person {
            login: String::new(),
        };
        assert!(!independent(&empty, Some("kitchen-bot"), &[]));
    }

    #[test]
    fn a_worker_must_not_have_written_the_branch() -> Result<(), Box<dyn std::error::Error>> {
        let writer = worker("worker-1")?;
        let reviewer = Reviewer::Worker {
            worker: worker("worker-2")?,
        };
        assert!(independent(&reviewer, None, std::slice::from_ref(&writer)));
        let same = Reviewer::Worker { worker: writer };
        let writers = [worker("worker-1")?];
        assert!(!independent(&same, Some("kitchen-bot"), &writers));
        Ok(())
    }
}
