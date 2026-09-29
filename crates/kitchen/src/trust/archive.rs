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
//! - An inspection whose deadline has passed, whose every reserved sample
//!   has a result, and whose stream leaves with it or is no longer recorded.
//!   Before the deadline an identical plan could restart it with a fresh
//!   budget, and an unanswered sample can still receive a late result, so
//!   those stay. While its stream stays live, for example because a grant
//!   cites it, the inspection stays too: the live record is what makes a
//!   repeated [`start_inspection`](Ledger::start_inspection) return it
//!   instead of starting over with fresh samples and budgets.
//!
//! - Never a stream a graduation decision cites, or one in the scope of an
//!   unrevoked decision observed after it: demotion after a regression reads
//!   those, so archiving them would restore the decision.
//!
//! Grant audits, graduation decisions, and archival summaries never leave, so
//! revocation never needs the archive.
//!
//! The batch is appended and synced before the ledger commits, under the
//! ledger's exclusive lock. Each summary records its line's length, so the
//! ledger knows how long the committed file is. Bytes past that length were
//! usually written by an archival whose append or ledger write failed: it
//! was never applied, and its records are still live. The next archival cuts
//! them off before appending, so every line in the file is one committed
//! batch. Before cutting or appending anything, it reads the committed part
//! once and checks every line's length, final newline, and digest against its
//! summary; a file shorter than its committed length, or any mismatch, is
//! refused as corrupt and changes nothing. It cuts off a tail only when doing
//! so loses no record: a partial line, or a line that is not a batch, or a
//! batch whose every record is still live and identical. A ledger restored
//! from an older copy records fewer batches than the file holds, and the
//! records of those batches may exist nowhere else, so a batch holding any
//! record the ledger lacks is refused as
//! [`Corruption::UnreconciledAppend`] and changes nothing. Recovery is to
//! restore the ledger that committed it, or for an operator to reconcile the
//! file. Recording an archived stream
//! again adds it back to the live ledger; a correction to an archived stream
//! leaves a revision gap, so it reads as incomplete and supports no grant.
use crate::{
    HouseId, TaskId,
    contracts::{ExternalRef, Timestamp},
    state::{Corruption, StateError, StorageOperation},
    trust::{
        GraduationAudit, GrantAudit, Ledger, Observation, TaskBinding, TrustError,
        store::{Document, MAX_STATE_BYTES},
    },
    workflows::inspector::Inspection,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::{ErrorKind, Read},
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
        Self::from_hasher(Sha256::new_with_prefix(bytes))
    }

    fn from_hasher(hasher: Sha256) -> Self {
        let mut hex = String::with_capacity(64);
        for byte in hasher.finalize() {
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
    /// Records the batch moved; `None` when the counts overflow, which only
    /// a corrupt snapshot can hold.
    #[must_use]
    pub const fn records(&self) -> Option<usize> {
        match self.observations.checked_add(self.bindings) {
            Some(sum) => sum.checked_add(self.inspections),
            None => None,
        }
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
    /// Streams a grant audit or graduation decision cites, with their tasks'
    /// bindings.
    pub streams_cited_by_grants: usize,
    /// Streams in an unrevoked graduation decision's scope observed after it.
    pub streams_after_graduation: usize,
    /// Uncited streams with an inspection that must stay.
    pub streams_under_inspection: usize,
    /// Bindings of tasks with no recorded stream: not known to be settled.
    pub unobserved_bindings: usize,
    /// Inspections before their deadline or with an unanswered sample.
    pub open_inspections: usize,
    /// Finished inspections whose stream stays live.
    pub inspections_of_kept_streams: usize,
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
    ///   (proposed, issued, or revoked) or graduation decision cites, that is
    ///   not in an unrevoked decision's scope after it, and that has no inspection
    ///   which must stay, together with its task's binding;
    /// - each inspection past its deadline whose every sample has a result,
    ///   once its stream leaves too or is no longer recorded.
    ///
    /// Grant audits, bindings of tasks without a recorded stream, and
    /// archival summaries stay, so revocation never needs the archive.
    /// Nothing to move writes nothing. Works on a full ledger: it removes at
    /// least one entry for the one it adds, and never grows the snapshot by
    /// more than a summary.
    ///
    /// # Errors
    /// A redirected, hard-linked, or nonprivate archive file, an archive
    /// shorter than its committed batches
    /// ([`Corruption::TruncatedAppend`]), committed lines that differ from
    /// their summaries in length, framing, or digest
    /// ([`Corruption::AppendMismatch`]), an uncommitted batch holding a
    /// record the live ledger lacks ([`Corruption::UnreconciledAppend`]), and
    /// I/O failures are `Storage`; the live ledger and the archive are then
    /// unchanged.
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
            let mut line = serde_json::to_vec(&batch)
                .map_err(|error| StateError::io(StorageOperation::Write, error.into()))?;
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
            let (_, prior) = doc.archivals.split_last().ok_or(TrustError::Corrupt)?;
            self.engine.append_private(
                ARCHIVE_FILE,
                committed,
                |file| verify_committed(prior, file),
                |tail| check_tail(house, doc, &batch, tail),
                &line,
            )?;
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

/// Check that `file`, the committed part of [`ARCHIVE_FILE`], holds exactly
/// the lines `archivals` describe: each has its recorded length, ends in a
/// newline there, and hashes to its digest. Reads the file once, in chunks.
fn verify_committed(archivals: &[Archival], file: &mut dyn Read) -> Result<(), StateError> {
    let mismatch = || StateError::CorruptState(Corruption::AppendMismatch);
    let io = |error| StateError::io(StorageOperation::Read, error);
    let mut chunk = [0_u8; 8192];
    for archival in archivals {
        let mut remaining = archival.bytes.checked_sub(1).ok_or_else(mismatch)?;
        let mut hasher = Sha256::new();
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(chunk.len(), |r| r.min(chunk.len()));
            let buffer = chunk.get_mut(..want).ok_or_else(mismatch)?;
            let read = file.read(buffer).map_err(io)?;
            let content = buffer.get(..read).ok_or_else(mismatch)?;
            if content.is_empty() || content.contains(&b'\n') {
                return Err(mismatch());
            }
            hasher.update(content);
            remaining -= u64::try_from(read).map_err(|_| mismatch())?;
        }
        let mut newline = [0_u8; 1];
        file.read_exact(&mut newline)
            .map_err(|error| match error.kind() {
                ErrorKind::UnexpectedEof => mismatch(),
                _ => io(error),
            })?;
        if newline != *b"\n" || ArchiveDigest::from_hasher(hasher) != archival.digest {
            return Err(mismatch());
        }
    }
    Ok(())
}

/// Longest uncommitted tail read back: one batch holds records taken from
/// one ledger snapshot. A longer tail is refused rather than read.
const MAX_TAIL_BYTES: u64 = MAX_STATE_BYTES;

/// Check that `tail`, the bytes of [`ARCHIVE_FILE`] past its committed
/// length, can be cut off without losing a record. Only a final line
/// without its newline is a write that stopped partway. A complete line must
/// be an [`ArchiveBatch`] that belongs to `house` with every record still
/// live and identical: in `doc`, or in `moving`, the batch this archival
/// takes out of it. An empty line holds nothing. Any other complete line,
/// including one that does not parse, may hold records the ledger lacks, so
/// it is refused.
fn check_tail(
    house: &HouseId,
    doc: &Document,
    moving: &ArchiveBatch,
    tail: &mut dyn Read,
) -> Result<(), StateError> {
    let unreconciled = || StateError::CorruptState(Corruption::UnreconciledAppend);
    let mut bytes = Vec::new();
    tail.take(MAX_TAIL_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| StateError::io(StorageOperation::Read, error))?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_TAIL_BYTES) {
        return Err(unreconciled());
    }
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let Some(content) = line.strip_suffix(b"\n") else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        let Ok(batch) = serde_json::from_slice::<ArchiveBatch>(content) else {
            return Err(unreconciled());
        };
        let live = batch.schema == ARCHIVE_SCHEMA
            && &batch.house == house
            && batch.observations.iter().all(|record| {
                doc.observations.contains(record) || moving.observations.contains(record)
            })
            && batch
                .bindings
                .iter()
                .all(|record| doc.bindings.contains(record) || moving.bindings.contains(record))
            && batch.inspections.iter().all(|record| {
                doc.inspections.contains(record) || moving.inspections.contains(record)
            });
        if !live {
            return Err(unreconciled());
        }
    }
    Ok(())
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
        .chain(
            doc.graduations
                .iter()
                .flat_map(|audit| &audit.decision().evidence),
        )
        .map(|(stream, _)| stream)
        .collect();
    let after_graduation: HashSet<&ExternalRef> = doc
        .observations
        .iter()
        .filter(|observation| {
            doc.graduations.iter().any(|audit| match audit {
                GraduationAudit::Decided(decision) => {
                    decision.scope == observation.attribution.scope
                        && observation.observed_at > decision.at
                }
                GraduationAudit::Revoked { .. } => false,
            })
        })
        .map(|observation| &observation.id)
        .collect();
    let finished = |inspection: &Inspection| {
        now >= inspection.plan().deadline
            && inspection
                .samples()
                .iter()
                .all(|sample| sample.result.is_some())
    };
    let inspected: HashSet<&ExternalRef> = doc
        .inspections
        .iter()
        .filter(|i| !finished(i))
        .map(|i| &i.plan().observation)
        .collect();

    let mut kept = KeptRecords {
        open_inspections: doc.inspections.iter().filter(|i| !finished(i)).count(),
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
        } else if after_graduation.contains(&observation.id) {
            kept.streams_after_graduation += 1;
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
    // A finished inspection leaves only with its stream, or once the stream
    // is gone; while the stream is live, the record blocks a restart.
    let mut inspections = HashSet::new();
    for inspection in doc.inspections.iter().filter(|i| finished(i)) {
        let stream = &inspection.plan().observation;
        if streams.contains(stream) || !seen.contains(stream) {
            inspections.insert(inspection.id().clone());
        } else {
            kept.inspections_of_kept_streams += 1;
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
