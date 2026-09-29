//! Context records: the immutable summary a closed thread publishes, and the
//! references tasks pin.

use serde::{Deserialize, Serialize};

use crate::{
    HouseId, TaskId,
    contracts::{ContractError, Text},
    state::TaskRecord,
};

use super::{
    DeliberationError, RecordId, ThreadId,
    thread::{Closure, Entry, MessageSeq, Thread, ThreadStatus},
};

/// Items of each kind (decisions, rejected options, open questions) a record
/// may hold.
pub const MAX_RECORD_ITEMS: usize = 6;
/// Largest text of one record item, in bytes. The encoded record must also
/// fit one marker.
pub const MAX_RECORD_ITEM_BYTES: usize = 320;
/// Records one task may pin, so its cook's brief stays bounded.
pub const MAX_PINS_PER_TASK: usize = 4;
/// Messages one decision or rejected option may cite.
const MAX_SOURCES: usize = 8;

/// A decision and the messages it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordDecision {
    /// The decision.
    pub decision: Text,
    /// The turns or human answers it came from; at least one.
    pub sources: Vec<MessageSeq>,
}

/// An option the thread rejected, why, and the messages it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RejectedOption {
    /// The option.
    pub option: Text,
    /// Why it was rejected.
    pub reason: Text,
    /// The turns or human answers it came from; at least one.
    pub sources: Vec<MessageSeq>,
}

/// What the summarizer supplies. The thread supplies the rest: its house,
/// task, revision, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDraft {
    /// Record identity, unique within the house.
    pub id: RecordId,
    /// The closed thread the record summarizes.
    pub thread: ThreadId,
    /// Decisions, each linked to its source messages.
    pub decisions: Vec<RecordDecision>,
    /// Rejected options, each linked to its source messages.
    pub rejected: Vec<RejectedOption>,
    /// Questions the thread left open.
    pub open_questions: Vec<Text>,
    /// The current record this one corrects, if any.
    pub supersedes: Option<RecordId>,
}

/// A published, immutable context record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRecord {
    /// Record identity, unique within the house.
    pub id: RecordId,
    /// The house it belongs to.
    pub house: HouseId,
    /// The task whose thread it summarizes.
    pub task: TaskId,
    /// The thread it summarizes.
    pub thread: ThreadId,
    /// The thread revision it summarizes: every entry of the closed thread.
    pub thread_revision: u32,
    /// How the thread ended; a cut-off thread's record says so.
    pub outcome: Closure,
    /// Decisions, each linked to its source messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<RecordDecision>,
    /// Rejected options, each linked to its source messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedOption>,
    /// Questions the thread left open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<Text>,
    /// The record this one corrects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<RecordId>,
}

impl ContextRecord {
    /// Build the record for `draft` from the closed `thread`.
    pub(super) fn summarize(
        thread: &Thread,
        draft: RecordDraft,
    ) -> Result<Self, DeliberationError> {
        let ThreadStatus::Closed(outcome) = thread.status() else {
            return Err(DeliberationError::ThreadOpen);
        };
        if draft.thread != thread.spec().id {
            return Err(DeliberationError::UnknownThread);
        }
        let record = Self {
            id: draft.id,
            house: thread.spec().house.clone(),
            task: thread.spec().task.clone(),
            thread: draft.thread,
            thread_revision: thread.revision(),
            outcome: *outcome,
            decisions: draft.decisions,
            rejected: draft.rejected,
            open_questions: draft.open_questions,
            supersedes: draft.supersedes,
        };
        record.validate(thread)?;
        Ok(record)
    }

    /// Check bounds and that every source is a turn or human answer of the
    /// summarized thread.
    fn validate(&self, thread: &Thread) -> Result<(), DeliberationError> {
        let items_fit = self.decisions.len() <= MAX_RECORD_ITEMS
            && self.rejected.len() <= MAX_RECORD_ITEMS
            && self.open_questions.len() <= MAX_RECORD_ITEMS;
        let texts_fit = self
            .decisions
            .iter()
            .map(|item| &item.decision)
            .chain(
                self.rejected
                    .iter()
                    .flat_map(|item| [&item.option, &item.reason]),
            )
            .chain(&self.open_questions)
            .all(|text| text.as_str().len() <= MAX_RECORD_ITEM_BYTES);
        if !items_fit || !texts_fit || self.supersedes.as_ref() == Some(&self.id) {
            return Err(DeliberationError::InvalidContent);
        }
        let cited = self
            .decisions
            .iter()
            .map(|item| &item.sources)
            .chain(self.rejected.iter().map(|item| &item.sources));
        for sources in cited {
            if sources.is_empty() || sources.len() > MAX_SOURCES {
                return Err(DeliberationError::InvalidContent);
            }
            for seq in sources {
                match thread.entry(*seq) {
                    Some(Entry::Turn(_) | Entry::HumanAnswered(_)) => {}
                    Some(
                        Entry::Opened { .. }
                        | Entry::Invited { .. }
                        | Entry::HumanAsked { .. }
                        | Entry::HumanUnanswered { .. }
                        | Entry::Concluded
                        | Entry::Stopped,
                    )
                    | None => return Err(DeliberationError::UnknownSource(*seq)),
                }
            }
        }
        Ok(())
    }

    /// This record's reference.
    #[must_use]
    pub fn reference(&self) -> RecordRef {
        RecordRef {
            house: self.house.clone(),
            record: self.id.clone(),
        }
    }
}

/// A reference to a context record by identity, naming its house so a record
/// from another house is refused rather than looked up.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordRef {
    /// The record's house.
    pub house: HouseId,
    /// The record.
    pub record: RecordId,
}

impl RecordRef {
    /// Refuse a reference from another house.
    pub(super) fn check_house(&self, house: &HouseId) -> Result<(), ContractError> {
        if &self.house == house {
            Ok(())
        } else {
            Err(ContractError::CrossHouse {
                expected: house.clone(),
                found: self.house.clone(),
            })
        }
    }
}

/// A record pinned to a task, resolved to its current correction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRecord {
    /// The record the task pinned.
    pub pinned: RecordId,
    /// The current record: the pinned one, or the newest record that
    /// supersedes it.
    pub current: ContextRecord,
}

impl PinnedRecord {
    /// Whether a correction replaced the pinned record.
    #[must_use]
    pub fn superseded(&self) -> bool {
        self.pinned != self.current.id
    }
}

/// A task with the context records pinned to it, as the station cook
/// receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskContext {
    /// The task.
    pub task: TaskRecord,
    /// Pinned records in pinning order, each resolved to its current
    /// correction; a record reached through two pins appears once.
    pub records: Vec<PinnedRecord>,
}
