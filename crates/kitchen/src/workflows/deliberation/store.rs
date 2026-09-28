//! Threads, records, and pins as workflow markers in the house store.
//!
//! Keys: every marker is recorded under [`DELIBERATION_WORKFLOW`] with the
//! task as its work item. A thread entry's subject is `thread/<id>/<seq>`, a
//! record's is `record/<id>`, and a pin's is `pin/<record>`.
//!
//! Writes: an entry is written with [`HouseStore::record_marker_unless`]. Its
//! guard replays the thread from the markers in the same transaction and
//! blocks the write when the same entry is already recorded (a retry) or
//! another writer appended first. The entry was admitted against the thread
//! at the same revision, and entries are append-only, so a thread at the same
//! revision is the same thread. Records and pins are guarded the same way.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{
    Error, TaskId, WorkflowId,
    contracts::{
        AskKind, AskRisk, BackendDescriptor, Claimant, ContractError, DecisionBinding, Effect,
        ExternalRef, RogerAsk, Text, Timestamp,
    },
    integrations::roger::DecisionStatus,
    state::{
        EffectState, HouseStore, MAX_MARKER_PAYLOAD_BYTES, MarkerAttempt, MarkerFact, MarkerKey,
        MarkerSchema, MarkerSubject, StateError, WorkItem, WorkflowMarker,
    },
};

use super::{
    DELIBERATION_WORKFLOW, DeliberationError, RecordId, ThreadId, brief, check_backend,
    record::{ContextRecord, MAX_PINS_PER_TASK, PinnedRecord, RecordDraft, RecordRef, TaskContext},
    thread::{
        Entry, HumanAnswer, MAX_ANSWER_BYTES, MessageSeq, Participant, Thread, ThreadSpec,
        ThreadStatus, Turn,
    },
};

type Result<T> = std::result::Result<T, Error>;

const ENTRY_SCHEMA: &str = "deliberation.entry";
const RECORD_SCHEMA: &str = "deliberation.record";
const PIN_SCHEMA: &str = "deliberation.pin";

fn schema(name: &str) -> Result<MarkerSchema> {
    Ok(MarkerSchema::new(name, NonZeroU32::MIN)?)
}

/// One persisted thread entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredEntry {
    thread: ThreadId,
    seq: MessageSeq,
    entry: Entry,
}

/// The result of appending an entry to a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posting {
    /// The entry was appended at `seq`.
    Recorded {
        /// The entry's position.
        seq: MessageSeq,
        /// The thread including the entry.
        thread: Thread,
    },
    /// The same entry (for a turn, the same delivery key) was already
    /// recorded at `seq`; nothing was written.
    Duplicate {
        /// The earlier entry's position.
        seq: MessageSeq,
        /// The thread as recorded.
        thread: Thread,
    },
}

impl Posting {
    /// The thread after the posting.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        match self {
            Self::Recorded { thread, .. } | Self::Duplicate { thread, .. } => thread,
        }
    }

    /// The entry's position.
    #[must_use]
    pub const fn seq(&self) -> MessageSeq {
        match self {
            Self::Recorded { seq, .. } | Self::Duplicate { seq, .. } => *seq,
        }
    }
}

/// A question a thread puts to a person through Roger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanQuestion {
    /// The Ask's scope: the thread's house and task, owner
    /// [`crate::contracts::DecisionOwner::Task`], action
    /// [`crate::contracts::Permission::AskHuman`], target `task:<task>`, and
    /// the task's current evidence revision and subject.
    pub binding: DecisionBinding,
    /// The task effect name that will submit the Ask.
    pub effect: crate::EffectName,
    /// Consequence level the operator selected.
    pub risk: AskRisk,
    /// One-line title, at most 120 characters.
    pub title: Text,
    /// The question, at most 2048 bytes. Quoted as untrusted in the Ask.
    pub question: Text,
}

/// What recording a person's answer did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// Nobody answered yet; nothing changed.
    Waiting,
    /// The answer was recorded and the thread resumed.
    Resumed(Posting),
    /// The Ask expired or was withdrawn; the thread ended as cut off.
    Ended(Posting),
}

/// Deliberation threads, records, and pins of one house store.
#[derive(Debug, Clone)]
pub struct Deliberations<'a> {
    store: &'a HouseStore,
    workflow: WorkflowId,
    recorded_by: &'a Claimant,
}

/// Why the guard of an entry write blocked it.
enum Blocked {
    Duplicate(MessageSeq),
    Moved,
}

/// Why the guard of a pin blocked it.
enum PinBlock {
    /// The task already reaches the record through an earlier pin.
    Covered,
    Refused(DeliberationError),
}

impl<'a> Deliberations<'a> {
    /// Work with `store`'s deliberations, recording as `recorded_by`. When
    /// the claimant acts under a consumer lease, the store checks it on every
    /// write.
    ///
    /// # Errors
    /// Never in practice; the workflow id is a valid constant.
    pub fn new(store: &'a HouseStore, recorded_by: &'a Claimant) -> Result<Self> {
        Ok(Self {
            store,
            workflow: WorkflowId::new(DELIBERATION_WORKFLOW)?,
            recorded_by,
        })
    }

    /// Open a thread for an existing task. Opening the same thread again
    /// returns it as recorded.
    ///
    /// # Errors
    /// Refuses a backend without worker messaging or from another house, a
    /// specification from another house or outside its bounds, an unknown
    /// task, and a different thread under the same identity.
    pub fn open(
        &self,
        spec: ThreadSpec,
        backend: &BackendDescriptor,
        now: Timestamp,
    ) -> Result<Thread> {
        let house = self.store.house();
        check_backend(backend, house)?;
        if &spec.house != house {
            return Err(ContractError::CrossHouse {
                expected: house.clone(),
                found: spec.house,
            }
            .into());
        }
        self.store.task(&spec.task)?;
        let task = spec.task.clone();
        let id = spec.id.clone();
        Thread::open(spec.clone())?;
        let key = self.entry_key(&task, &id, MessageSeq::new(0))?;
        let fact = entry_fact(&id, MessageSeq::new(0), Entry::Opened { spec })?;
        match self.store.record_marker(key, fact, self.recorded_by, now) {
            Ok(_) => {}
            Err(Error::State(StateError::MarkerConflict)) => {
                return Err(DeliberationError::ThreadExists.into());
            }
            Err(error) => return Err(error),
        }
        self.thread(&task, &id)
    }

    /// Read a thread.
    ///
    /// # Errors
    /// Returns [`DeliberationError::UnknownThread`], corruption, or a storage
    /// error.
    pub fn thread(&self, task: &TaskId, id: &ThreadId) -> Result<Thread> {
        let markers = self.store.markers(&self.workflow)?;
        let refs: Vec<&WorkflowMarker> = markers.iter().collect();
        self.replay(&refs, task, id)?
            .ok_or_else(|| DeliberationError::UnknownThread.into())
    }

    /// Record a participant's turn. A turn whose key is already recorded is
    /// reported as a duplicate and not written again, even after the thread
    /// ended.
    ///
    /// # Errors
    /// Refuses a non-participant, a turn held by a mentioned participant, a
    /// thread waiting for a person or ended, invalid mentions or content, and
    /// a concurrent append ([`DeliberationError::ThreadMoved`]).
    pub fn post(
        &self,
        task: &TaskId,
        id: &ThreadId,
        turn: Turn,
        now: Timestamp,
    ) -> Result<Posting> {
        self.append(task, id, Entry::Turn(turn), now)
    }

    /// Invite a participant. Inviting beyond the participant bound ends the
    /// thread as cut off.
    ///
    /// # Errors
    /// As [`Self::post`], and refuses a role that already takes part.
    pub fn invite(
        &self,
        task: &TaskId,
        id: &ThreadId,
        participant: Participant,
        now: Timestamp,
    ) -> Result<Posting> {
        self.append(task, id, Entry::Invited { participant }, now)
    }

    /// Record that the thread asks a person, and return the Roger Ask to
    /// submit through the task's effect named in `question`. The thread
    /// waits until [`Self::answer`] records the answer. Asking again with the
    /// same question returns the same Ask without a second entry.
    ///
    /// # Errors
    /// Refuses a binding outside the thread's house and task or not a
    /// question, a thread that is not open, more than
    /// [`super::MAX_HUMAN_QUESTIONS`], and invalid Ask content.
    pub fn ask_human(
        &self,
        task: &TaskId,
        id: &ThreadId,
        question: HumanQuestion,
        now: Timestamp,
    ) -> Result<(Posting, RogerAsk)> {
        if question.question.as_str().len() > super::MAX_TURN_BYTES {
            return Err(DeliberationError::InvalidContent.into());
        }
        let thread = self.thread(task, id)?;
        let ask = RogerAsk {
            binding: question.binding.clone(),
            kind: AskKind::Question,
            risk: question.risk,
            title: question.title,
            body: Text::new(&brief::question_body(&thread, question.question.as_str()))?,
            supersedes: None,
        };
        ask.validate()?;
        let entry = Entry::HumanAsked {
            binding: question.binding,
            effect: question.effect,
        };
        let posting = self.append(task, id, entry, now)?;
        Ok((posting, ask))
    }

    /// Record a person's answer to the thread's pending question. `status`
    /// must come from [`crate::integrations::roger::validate_answer`] for
    /// `ask` and `binding`. The thread resumes only when `binding` is exactly
    /// the pending question's, `ask` is the Ask the task's named effect
    /// created, and the task's evidence has not moved since. An answer
    /// records instructions only; approvals are refused, because
    /// deliberation grants nothing.
    ///
    /// # Errors
    /// Returns [`DeliberationError::DecisionMismatch`] for another question,
    /// Ask, or an approval decision, [`DeliberationError::StaleAnswer`] when
    /// the task's evidence moved, and [`DeliberationError::NotAwaitingHuman`]
    /// when no question is pending.
    pub fn answer(
        &self,
        task: &TaskId,
        id: &ThreadId,
        ask: &ExternalRef,
        binding: &DecisionBinding,
        status: DecisionStatus,
        now: Timestamp,
    ) -> Result<AnswerOutcome> {
        let thread = self.thread(task, id)?;
        if let Some(outcome) = recorded_answer(&thread, ask) {
            return Ok(outcome);
        }
        let ThreadStatus::AwaitingHuman {
            binding: pending,
            effect,
        } = thread.status()
        else {
            return Err(match thread.status() {
                ThreadStatus::Closed(_) => DeliberationError::ThreadClosed,
                ThreadStatus::Open | ThreadStatus::AwaitingHuman { .. } => {
                    DeliberationError::NotAwaitingHuman
                }
            }
            .into());
        };
        if **pending != *binding {
            return Err(DeliberationError::DecisionMismatch.into());
        }
        let record = self.store.task(task)?;
        let created = record.effects().iter().any(|candidate| {
            candidate.name() == effect
                && matches!(candidate.request().effect(), Effect::Roger(roger)
                    if roger.ask.binding == **pending && roger.ask.kind == AskKind::Question)
                && matches!(candidate.state(), EffectState::Applied { receipt, .. }
                    if receipt.reference() == ask)
        });
        if !created {
            return Err(DeliberationError::DecisionMismatch.into());
        }
        let evidence = record.evidence();
        if evidence.revision() != binding.revision || evidence.subject() != binding.subject.as_ref()
        {
            return Err(DeliberationError::StaleAnswer.into());
        }
        let entry = match status {
            DecisionStatus::Unanswered => return Ok(AnswerOutcome::Waiting),
            DecisionStatus::Instructions(text) => {
                let (instructions, truncated) = match text {
                    Some(text) => {
                        let (kept, truncated) = truncate(text.as_str(), MAX_ANSWER_BYTES);
                        (Some(Text::new(kept)?), truncated)
                    }
                    None => (None, false),
                };
                Entry::HumanAnswered(HumanAnswer {
                    ask: ask.clone(),
                    instructions,
                    truncated,
                })
            }
            DecisionStatus::Expired | DecisionStatus::Closed => {
                let posting =
                    self.append(task, id, Entry::HumanUnanswered { ask: ask.clone() }, now)?;
                return Ok(AnswerOutcome::Ended(posting));
            }
            DecisionStatus::Approved | DecisionStatus::Rejected => {
                return Err(DeliberationError::DecisionMismatch.into());
            }
        };
        Ok(AnswerOutcome::Resumed(self.append(task, id, entry, now)?))
    }

    /// End the thread with a conclusion.
    ///
    /// # Errors
    /// Refuses a thread that is waiting for a person or already ended.
    pub fn conclude(&self, task: &TaskId, id: &ThreadId, now: Timestamp) -> Result<Posting> {
        self.append(task, id, Entry::Concluded, now)
    }

    /// Stop the thread before a conclusion; it ends as cut off.
    ///
    /// # Errors
    /// Refuses a thread that already ended.
    pub fn stop(&self, task: &TaskId, id: &ThreadId, now: Timestamp) -> Result<Posting> {
        self.append(task, id, Entry::Stopped, now)
    }

    /// Publish the record of an ended thread and pin it to the thread's task.
    /// The record carries the thread's revision and how it ended. Publishing
    /// the same draft again returns the same record.
    ///
    /// # Errors
    /// Refuses a thread that has not ended, sources that are not turns or
    /// human answers of the thread, oversized content, a different record
    /// under a published identity ([`DeliberationError::RecordImmutable`]),
    /// and a correction that does not supersede the current record
    /// ([`DeliberationError::NotCurrentRecord`]).
    pub fn publish(
        &self,
        task: &TaskId,
        draft: RecordDraft,
        now: Timestamp,
    ) -> Result<ContextRecord> {
        let thread = self.thread(task, &draft.thread)?;
        let record = ContextRecord::summarize(&thread, draft)?;
        let key = self.key(task, &format!("record/{}", record.id))?;
        let fact = workflow_fact(&schema(RECORD_SCHEMA)?, &record)?;
        let attempt =
            self.store
                .record_marker_unless(key, fact, self.recorded_by, now, |markers| {
                    Ok(check_publication(&records(markers)?, &record))
                });
        match attempt {
            Ok(MarkerAttempt::Recorded(_) | MarkerAttempt::AlreadyRecorded(_)) => {}
            Ok(MarkerAttempt::Blocked(refusal)) => return Err(refusal.into()),
            Err(Error::State(StateError::MarkerConflict)) => {
                return Err(DeliberationError::RecordImmutable.into());
            }
            Err(error) => return Err(error),
        }
        self.pin(task, &record.reference(), now)?;
        Ok(record)
    }

    /// Read a published record by reference.
    ///
    /// # Errors
    /// Returns [`ContractError::CrossHouse`] for another house's record and
    /// [`DeliberationError::UnknownRecord`] when none is published.
    pub fn record(&self, reference: &RecordRef) -> Result<ContextRecord> {
        reference.check_house(self.store.house())?;
        let markers = self.store.markers(&self.workflow)?;
        let refs: Vec<&WorkflowMarker> = markers.iter().collect();
        records(&refs)?
            .into_iter()
            .find(|record| record.id == reference.record)
            .ok_or_else(|| DeliberationError::UnknownRecord.into())
    }

    /// Pin a published record to `task`, so the task's cook receives it.
    /// Pinning a record the task already reaches through an earlier pin
    /// changes nothing.
    ///
    /// # Errors
    /// Returns [`ContractError::CrossHouse`] for another house's record,
    /// [`DeliberationError::UnknownRecord`], an unknown task, and
    /// [`DeliberationError::PinBound`].
    pub fn pin(&self, task: &TaskId, reference: &RecordRef, now: Timestamp) -> Result<()> {
        reference.check_house(self.store.house())?;
        self.store.task(task)?;
        let key = self.key(task, &format!("pin/{}", reference.record))?;
        let fact = workflow_fact(&schema(PIN_SCHEMA)?, reference)?;
        let item = task_item(task);
        let attempt =
            self.store
                .record_marker_unless(key, fact, self.recorded_by, now, |markers| {
                    let published = records(markers)?;
                    let Some(record) = published.iter().find(|one| one.id == reference.record)
                    else {
                        return Ok(Some(PinBlock::Refused(DeliberationError::UnknownRecord)));
                    };
                    let pinned = pins(markers, &item)?;
                    let target = current(&published, record)?;
                    for pin in &pinned {
                        let Some(earlier) = published.iter().find(|one| one.id == pin.record)
                        else {
                            return Err(DeliberationError::Corrupt.into());
                        };
                        if current(&published, earlier)?.id == target.id {
                            return Ok(Some(PinBlock::Covered));
                        }
                    }
                    if pinned.len() >= MAX_PINS_PER_TASK {
                        return Ok(Some(PinBlock::Refused(DeliberationError::PinBound)));
                    }
                    Ok(None)
                })?;
        match attempt {
            MarkerAttempt::Blocked(PinBlock::Refused(refusal)) => Err(refusal.into()),
            MarkerAttempt::Recorded(_)
            | MarkerAttempt::AlreadyRecorded(_)
            | MarkerAttempt::Blocked(PinBlock::Covered) => Ok(()),
        }
    }

    /// The task with its pinned records; see [`task_context`].
    ///
    /// # Errors
    /// As [`task_context`].
    pub fn task_context(&self, task: &TaskId) -> Result<TaskContext> {
        task_context(self.store, task)
    }

    fn append(
        &self,
        task: &TaskId,
        id: &ThreadId,
        entry: Entry,
        now: Timestamp,
    ) -> Result<Posting> {
        let thread = self.thread(task, id)?;
        if let Some(seq) = duplicate_of(&thread, &entry) {
            return Ok(Posting::Duplicate { seq, thread });
        }
        let mut next = thread.clone();
        next.push(entry.clone())?;
        let seq = MessageSeq::new(thread.revision());
        let key = self.entry_key(task, id, seq)?;
        let fact = entry_fact(id, seq, entry.clone())?;
        let attempt =
            self.store
                .record_marker_unless(key, fact, self.recorded_by, now, |markers| {
                    let Some(fresh) = self.replay(markers, task, id)? else {
                        return Err(DeliberationError::Corrupt.into());
                    };
                    if let Some(earlier) = duplicate_of(&fresh, &entry) {
                        return Ok(Some(Blocked::Duplicate(earlier)));
                    }
                    if fresh.revision() != seq.get() {
                        return Ok(Some(Blocked::Moved));
                    }
                    Ok(None)
                });
        match attempt {
            Ok(MarkerAttempt::Recorded(_)) => Ok(Posting::Recorded { seq, thread: next }),
            Ok(MarkerAttempt::AlreadyRecorded(_)) => Ok(Posting::Duplicate { seq, thread: next }),
            Ok(MarkerAttempt::Blocked(Blocked::Duplicate(earlier))) => Ok(Posting::Duplicate {
                seq: earlier,
                thread: self.thread(task, id)?,
            }),
            Ok(MarkerAttempt::Blocked(Blocked::Moved))
            | Err(Error::State(StateError::MarkerConflict)) => {
                Err(DeliberationError::ThreadMoved.into())
            }
            Err(error) => Err(error),
        }
    }

    /// Replay one thread from the workflow's markers; `None` when it has no
    /// entries.
    fn replay(
        &self,
        markers: &[&WorkflowMarker],
        task: &TaskId,
        id: &ThreadId,
    ) -> Result<Option<Thread>> {
        let entry_schema = schema(ENTRY_SCHEMA)?;
        let item = task_item(task);
        let mut stored = Vec::new();
        for marker in markers.iter().filter(|marker| marker.key().item == item) {
            if !has_schema(marker.fact(), &entry_schema) {
                continue;
            }
            let entry: StoredEntry = marker.fact().decode(&entry_schema)?;
            if &entry.thread != id {
                continue;
            }
            if marker.key() != &self.entry_key(task, id, entry.seq)? {
                return Err(DeliberationError::Corrupt.into());
            }
            stored.push(entry);
        }
        if stored.is_empty() {
            return Ok(None);
        }
        stored.sort_by_key(|entry| entry.seq);
        let contiguous = stored
            .iter()
            .enumerate()
            .all(|(index, entry)| u32::try_from(index).is_ok_and(|index| index == entry.seq.get()));
        if !contiguous {
            return Err(DeliberationError::Corrupt.into());
        }
        let entries = stored.into_iter().map(|entry| entry.entry).collect();
        Ok(Some(Thread::replay(entries)?))
    }

    fn entry_key(&self, task: &TaskId, id: &ThreadId, seq: MessageSeq) -> Result<MarkerKey> {
        self.key(task, &format!("thread/{id}/{}", seq.get()))
    }

    fn key(&self, task: &TaskId, subject: &str) -> Result<MarkerKey> {
        Ok(MarkerKey {
            workflow: self.workflow.clone(),
            item: task_item(task),
            subject: MarkerSubject::Observation(ExternalRef::new(subject)?),
        })
    }
}

/// The task with its pinned records, each resolved to its current
/// correction: what the station cook receives with its task. Reading needs
/// no claimant.
///
/// # Errors
/// Returns an unknown task, [`ContractError::CrossHouse`] for a persisted pin
/// naming another house, and corruption or storage errors.
pub fn task_context(store: &HouseStore, task: &TaskId) -> Result<TaskContext> {
    let record = store.task(task)?;
    let markers = store.markers(&WorkflowId::new(DELIBERATION_WORKFLOW)?)?;
    let refs: Vec<&WorkflowMarker> = markers.iter().collect();
    let published = records(&refs)?;
    let mut resolved: Vec<PinnedRecord> = Vec::new();
    for pin in pins(&refs, &task_item(task))? {
        pin.check_house(store.house())?;
        let pinned = published
            .iter()
            .find(|one| one.id == pin.record)
            .ok_or(DeliberationError::Corrupt)?;
        let latest = current(&published, pinned)?;
        if resolved.iter().all(|one| one.current.id != latest.id) {
            resolved.push(PinnedRecord {
                pinned: pin.record,
                current: latest.clone(),
            });
        }
    }
    Ok(TaskContext {
        task: record,
        records: resolved,
    })
}

fn task_item(task: &TaskId) -> WorkItem {
    WorkItem::Task { task: task.clone() }
}

fn has_schema(fact: &MarkerFact, expected: &MarkerSchema) -> bool {
    matches!(fact, MarkerFact::Workflow { schema, .. } if schema == expected)
}

fn entry_fact(id: &ThreadId, seq: MessageSeq, entry: Entry) -> Result<MarkerFact> {
    let stored = StoredEntry {
        thread: id.clone(),
        seq,
        entry,
    };
    workflow_fact(&schema(ENTRY_SCHEMA)?, &stored)
}

/// Encode a fact, reporting content beyond one marker as invalid content.
fn workflow_fact<T: Serialize>(schema: &MarkerSchema, value: &T) -> Result<MarkerFact> {
    let encoded = serde_json::to_string(value).map_err(|_| DeliberationError::InvalidContent)?;
    if encoded.len() > MAX_MARKER_PAYLOAD_BYTES {
        return Err(DeliberationError::InvalidContent.into());
    }
    Ok(MarkerFact::workflow(schema.clone(), value)?)
}

/// An entry the thread already holds: for a turn, one with the same delivery
/// key; otherwise an identical entry.
fn duplicate_of(thread: &Thread, entry: &Entry) -> Option<MessageSeq> {
    let index = match entry {
        Entry::Turn(turn) => return thread.turn_with_key(&turn.key),
        other => thread
            .entries()
            .iter()
            .position(|recorded| recorded == other)?,
    };
    u32::try_from(index).ok().map(MessageSeq::new)
}

fn records(markers: &[&WorkflowMarker]) -> Result<Vec<ContextRecord>> {
    let record_schema = schema(RECORD_SCHEMA)?;
    markers
        .iter()
        .filter(|marker| has_schema(marker.fact(), &record_schema))
        .map(|marker| Ok(marker.fact().decode(&record_schema)?))
        .collect()
}

fn pins(markers: &[&WorkflowMarker], item: &WorkItem) -> Result<Vec<RecordRef>> {
    let pin_schema = schema(PIN_SCHEMA)?;
    markers
        .iter()
        .filter(|marker| &marker.key().item == item && has_schema(marker.fact(), &pin_schema))
        .map(|marker| Ok(marker.fact().decode(&pin_schema)?))
        .collect()
}

/// Follow corrections from `record` to the newest one. Each record is
/// superseded at most once, so the chain is linear and at most as long as
/// the list of records.
fn current<'r>(
    published: &'r [ContextRecord],
    record: &'r ContextRecord,
) -> Result<&'r ContextRecord> {
    let mut latest = record;
    for _ in 0..published.len() {
        let mut successors = published
            .iter()
            .filter(|one| one.supersedes.as_ref() == Some(&latest.id));
        match (successors.next(), successors.next()) {
            (None, _) => return Ok(latest),
            (Some(next), None) => latest = next,
            (Some(_), Some(_)) => return Err(DeliberationError::Corrupt.into()),
        }
    }
    Err(DeliberationError::Corrupt.into())
}

/// Why `record` may not be published next to `published`, if any.
fn check_publication(
    published: &[ContextRecord],
    record: &ContextRecord,
) -> Option<DeliberationError> {
    if published.iter().any(|one| one.id == record.id) {
        return Some(DeliberationError::RecordImmutable);
    }
    let superseded = |id: &RecordId| {
        published
            .iter()
            .any(|one| one.supersedes.as_ref() == Some(id))
    };
    if let Some(previous) = &record.supersedes {
        if published.iter().all(|one| &one.id != previous) {
            return Some(DeliberationError::UnknownRecord);
        }
        if superseded(previous) {
            return Some(DeliberationError::NotCurrentRecord);
        }
    }
    let thread_head = published
        .iter()
        .find(|one| one.task == record.task && one.thread == record.thread && !superseded(&one.id));
    match thread_head {
        Some(head) if record.supersedes.as_ref() != Some(&head.id) => {
            Some(DeliberationError::NotCurrentRecord)
        }
        Some(_) | None => None,
    }
}

/// Cut `text` to at most `limit` bytes on a character boundary.
fn truncate(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (text.get(..end).unwrap_or_default(), true)
}

/// The outcome of an answer to `ask` the thread already recorded, if any.
fn recorded_answer(thread: &Thread, ask: &ExternalRef) -> Option<AnswerOutcome> {
    thread
        .entries()
        .iter()
        .enumerate()
        .find_map(|(index, entry)| {
            let seq = MessageSeq::new(u32::try_from(index).ok()?);
            let posting = || Posting::Duplicate {
                seq,
                thread: thread.clone(),
            };
            match entry {
                Entry::HumanAnswered(answer) if &answer.ask == ask => {
                    Some(AnswerOutcome::Resumed(posting()))
                }
                Entry::HumanUnanswered { ask: closed } if closed == ask => {
                    Some(AnswerOutcome::Ended(posting()))
                }
                Entry::Opened { .. }
                | Entry::Turn(_)
                | Entry::Invited { .. }
                | Entry::HumanAsked { .. }
                | Entry::HumanAnswered(_)
                | Entry::HumanUnanswered { .. }
                | Entry::Concluded
                | Entry::Stopped => None,
            }
        })
}
