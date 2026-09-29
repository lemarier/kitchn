//! Operator-invoked archival of trust records no grant needs.
//!
//! The ledger is bounded ([`MAX_HISTORY`](crate::trust::MAX_HISTORY) entries
//! and a byte limit). Archival moves records out of the live snapshot into
//! [`ARCHIVE_FILE`], an append-only, owner-only JSON Lines file in the ledger
//! directory, and leaves one [`Archival`] summary (a digest and counts) in
//! the ledger. It never runs on its own.
//!
//! Only records that no grant decision depends on may leave:
//!
//! - An observation stream, all its revisions together, when no grant audit
//!   of any state (proposed, issued, or revoked) cites it and every
//!   inspection of it can leave too. Revoked grants keep their evidence
//!   because the ledger checks every audit's evidence on load, and a pending
//!   proposal still needs its evidence to be approved.
//! - The binding of a task whose stream leaves. A stream is recorded only for
//!   a settled task, so the stream is the proof the task settled; a binding
//!   without a recorded stream stays.
//! - An inspection whose deadline has passed and whose every reserved sample
//!   has a result. Before the deadline an identical plan could restart it
//!   with a fresh budget, and an unanswered sample can still receive a late
//!   result, so those stay.
//!
//! Grant audits and archival summaries never leave, so revocation never
//! needs the archive.
//!
//! The batch is appended and synced before the ledger commits, under the
//! ledger's exclusive lock. Each summary records its line's length, so the
//! ledger knows how long the committed file is. Bytes past that length were
//! written by an archival whose append or ledger write failed: it was never
//! applied, and its records are still live. The next archival cuts them off
//! before appending, so every line in the file is one committed batch. A file
//! shorter than its committed length is refused as corrupt. Recording an archived stream
//! again adds it back to the live ledger; a correction to an archived stream
//! leaves a revision gap, so it reads as incomplete and supports no grant.
use crate::{
    HouseId, TaskId,
    contracts::{ExternalRef, Timestamp},
    trust::{GrantAudit, Ledger, Observation, TaskBinding, TrustError, store::Document},
    workflows::inspector::Inspection,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
};

/// Archive file beside the ledger snapshot, one [`ArchiveBatch`] per line.
pub const ARCHIVE_FILE: &str = "archive.jsonl";
/// Schema of each archived batch.
pub const ARCHIVE_SCHEMA: u64 = 1;

/// SHA-256 of one archived batch line, without its newline, as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArchiveDigest(String);

impl ArchiveDigest {
    fn of(bytes: &[u8]) -> Self {
        let mut hex = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            hex.push(char::from(HEX[usize::from(byte >> 4)]));
            hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(hex)
    }

    /// The digest as lowercase hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

impl TryFrom<String> for ArchiveDigest {
    type Error = TrustError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 64 && value.bytes().all(|byte| HEX.contains(&byte)) {
            Ok(Self(value))
        } else {
            Err(TrustError::Invalid)
        }
    }
}

impl From<ArchiveDigest> for String {
    fn from(digest: ArchiveDigest) -> Self {
        digest.0
    }
}

impl fmt::Display for ArchiveDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One applied archival, kept in the live ledger as one history entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Archival {
    /// Digest of the batch line in [`ARCHIVE_FILE`].
    pub digest: ArchiveDigest,
    /// When the archival was applied.
    pub at: Timestamp,
    /// Observation revisions moved.
    pub observations: usize,
    /// Task bindings moved.
    pub bindings: usize,
    /// Inspections moved.
    pub inspections: usize,
    /// Length of the batch line in [`ARCHIVE_FILE`], newline included.
    pub bytes: u64,
}

impl Archival {
    /// Records the batch moved.
    #[must_use]
    pub const fn records(&self) -> usize {
        self.observations + self.bindings + self.inspections
    }

    /// Committed length of [`ARCHIVE_FILE`]: the sum of every batch line;
    /// `None` on overflow.
    pub(super) fn committed_bytes(archivals: &[Self]) -> Option<u64> {
        archivals
            .iter()
            .try_fold(0_u64, |total, archival| total.checked_add(archival.bytes))
    }
}

/// One line of [`ARCHIVE_FILE`]: the records one archival moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchiveBatch {
    /// [`ARCHIVE_SCHEMA`].
    pub schema: u64,
    /// Owning house.
    pub house: HouseId,
    /// When the archival was applied.
    pub at: Timestamp,
    /// Every revision of each archived stream.
    pub observations: Vec<Observation>,
    /// Bindings of the archived streams' tasks.
    pub bindings: Vec<TaskBinding>,
    /// Finished inspections.
    pub inspections: Vec<Inspection>,
}

/// An observation stream an archival moves, or would move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedStream {
    /// Stream identity.
    pub id: ExternalRef,
    /// The settled task it observed.
    pub task: TaskId,
    /// Revisions moved.
    pub revisions: usize,
    /// Whether the task's binding moves with it.
    pub binding: bool,
}

/// Records an archival keeps live, by the reason they stay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptRecords {
    /// Streams a grant audit cites, with their tasks' bindings.
    pub streams_cited_by_grants: usize,
    /// Uncited streams with an inspection that must stay.
    pub streams_under_inspection: usize,
    /// Bindings of tasks with no recorded stream: not known to be settled.
    pub unobserved_bindings: usize,
    /// Inspections before their deadline or with an unanswered sample.
    pub open_inspections: usize,
    /// Grant audits, which never leave.
    pub grant_audits: usize,
}

/// What an archival moves, or would move, and what it keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveReport {
    /// Streams, with their bindings.
    pub streams: Vec<ArchivedStream>,
    /// Finished inspections.
    pub inspections: Vec<ExternalRef>,
    /// Records that stay live.
    pub kept: KeptRecords,
    /// The summary written to the ledger; `None` for a preview or when
    /// nothing can leave.
    pub archival: Option<Archival>,
}

impl ArchiveReport {
    /// Whether nothing can leave the live ledger.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty() && self.inspections.is_empty()
    }
}

impl Ledger {
    /// What [`Self::archive`] would move at `now`. Writes nothing.
    ///
    /// # Errors
    /// Returns storage failures, including a corrupt snapshot.
    pub fn preview_archive(&self, now: Timestamp) -> Result<ArchiveReport, TrustError> {
        self.read(|doc| Ok(select(doc, now).report))
    }

    /// Move every record no grant needs to [`ARCHIVE_FILE`] and add one
    /// [`Archival`] summary, in one ledger transaction:
    ///
    /// - each observation stream, with all its revisions, that no grant audit
    ///   (proposed, issued, or revoked) cites and that has no inspection
    ///   which must stay, together with its task's binding;
    /// - each inspection past its deadline whose every sample has a result.
    ///
    /// Grant audits, bindings of tasks without a recorded stream, and
    /// archival summaries stay, so revocation never needs the archive. Nothing to move writes nothing. Works on a full ledger:
    /// it removes at least one entry for the one it adds, and never grows
    /// the snapshot by more than a summary.
    ///
    /// # Errors
    /// A redirected, hard-linked, or nonprivate archive file, an archive
    /// shorter than its committed batches, and I/O failures are `Storage`;
    /// the live ledger is then unchanged.
    pub fn archive(&self, now: Timestamp) -> Result<ArchiveReport, TrustError> {
        let house = self.house();
        self.transact(|doc| {
            let Selection {
                mut report,
                streams,
                inspections,
            } = select(doc, now);
            if report.is_empty() {
                return Ok(report);
            }
            let tasks: HashSet<TaskId> = report.streams.iter().map(|s| s.task.clone()).collect();
            let (observations, kept) = doc
                .observations
                .drain(..)
                .partition(|o| streams.contains(&o.id));
            doc.observations = kept;
            let (bindings, kept) = doc
                .bindings
                .drain(..)
                .partition(|b| tasks.contains(&b.spec.id));
            doc.bindings = kept;
            let (moved, kept) = doc
                .inspections
                .drain(..)
                .partition(|i| inspections.contains(i.id()));
            doc.inspections = kept;
            let batch = ArchiveBatch {
                schema: ARCHIVE_SCHEMA,
                house: house.clone(),
                at: now,
                observations,
                bindings,
                inspections: moved,
            };
            let mut line = serde_json::to_vec(&batch).map_err(|error| {
                crate::state::StateError::io(crate::state::StorageOperation::Write, error.into())
            })?;
            let digest = ArchiveDigest::of(&line);
            line.push(b'\n');
            let committed = Archival::committed_bytes(&doc.archivals).ok_or(TrustError::Corrupt)?;
            let archival = Archival {
                digest,
                at: now,
                observations: batch.observations.len(),
                bindings: batch.bindings.len(),
                inspections: batch.inspections.len(),
                bytes: u64::try_from(line.len()).map_err(|_| TrustError::Corrupt)?,
            };
            doc.archivals.push(archival.clone());
            // Check before the append, so a batch is written only for a
            // ledger that will commit.
            doc.validate(house)?;
            self.engine.append_private(ARCHIVE_FILE, committed, &line)?;
            report.archival = Some(archival);
            Ok(report)
        })
    }

    /// Summaries of every applied archival, oldest first.
    ///
    /// # Errors
    /// Returns storage failures.
    pub fn archivals(&self) -> Result<Vec<Archival>, TrustError> {
        self.read(|doc| Ok(doc.archivals.clone()))
    }
}

struct Selection {
    report: ArchiveReport,
    streams: HashSet<ExternalRef>,
    inspections: HashSet<ExternalRef>,
}

fn select(doc: &Document, now: Timestamp) -> Selection {
    let cited: HashSet<&ExternalRef> = doc
        .grants
        .iter()
        .flat_map(|audit| match audit {
            GrantAudit::Proposed(proposal) | GrantAudit::RevokedProposal { proposal, .. } => {
                &proposal.evidence
            }
            GrantAudit::Issued(grant) | GrantAudit::Revoked { grant, .. } => &grant.evidence,
        })
        .map(|(stream, _)| stream)
        .collect();
    let finished = |inspection: &Inspection| {
        now >= inspection.plan().deadline
            && inspection
                .samples()
                .iter()
                .all(|sample| sample.result.is_some())
    };
    let inspections: HashSet<ExternalRef> = doc
        .inspections
        .iter()
        .filter(|i| finished(i))
        .map(|i| i.id().clone())
        .collect();
    let inspected: HashSet<&ExternalRef> = doc
        .inspections
        .iter()
        .filter(|i| !finished(i))
        .map(|i| &i.plan().observation)
        .collect();

    let mut kept = KeptRecords {
        open_inspections: doc.inspections.len().saturating_sub(inspections.len()),
        grant_audits: doc.grants.len(),
        ..KeptRecords::default()
    };
    let mut revisions: HashMap<&ExternalRef, usize> = HashMap::new();
    for observation in &doc.observations {
        *revisions.entry(&observation.id).or_default() += 1;
    }
    let mut seen = HashSet::new();
    let mut streams = HashSet::new();
    let mut report_streams = Vec::new();
    for observation in &doc.observations {
        if !seen.insert(&observation.id) {
            continue;
        }
        if cited.contains(&observation.id) {
            kept.streams_cited_by_grants += 1;
        } else if inspected.contains(&observation.id) {
            kept.streams_under_inspection += 1;
        } else {
            streams.insert(observation.id.clone());
            report_streams.push(ArchivedStream {
                id: observation.id.clone(),
                task: observation.task.clone(),
                revisions: revisions.get(&observation.id).copied().unwrap_or_default(),
                binding: doc.bindings.iter().any(|b| b.spec.id == observation.task),
            });
        }
    }
    let observed: HashSet<&TaskId> = doc.observations.iter().map(|o| &o.task).collect();
    kept.unobserved_bindings = doc
        .bindings
        .iter()
        .filter(|b| !observed.contains(&b.spec.id))
        .count();
    let report = ArchiveReport {
        streams: report_streams,
        inspections: doc
            .inspections
            .iter()
            .filter(|i| inspections.contains(i.id()))
            .map(|i| i.id().clone())
            .collect(),
        kept,
        archival: None,
    };
    Selection {
        report,
        streams,
        inspections,
    }
}
