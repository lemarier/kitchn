//! The thread contract and its replay: one ordered list of entries, and the
//! state derived from it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    EffectName, HouseId, TaskId,
    contracts::{
        DecisionBinding, DecisionOwner, ExternalRef, Permission, ResourceKind, ResourceRef, Role,
        Text,
    },
};

use super::{DeliberationError, ThreadId};

/// Largest turn bound a thread may declare.
pub const MAX_TURNS: u32 = 48;
/// Largest participant bound a thread may declare.
pub const MAX_PARTICIPANTS: u32 = 8;
/// Human questions one thread may ask.
pub const MAX_HUMAN_QUESTIONS: u32 = 3;
/// Largest topic, in bytes.
pub const MAX_TOPIC_BYTES: usize = 1024;
/// Largest turn body, in bytes. The encoded entry must also fit one marker.
pub const MAX_TURN_BYTES: usize = 2048;
/// Human answer text kept in the thread; longer answers are truncated and
/// marked as such.
pub(super) const MAX_ANSWER_BYTES: usize = 1024;
/// Entries one thread can hold: its opening, every turn and invitation, each
/// question and answer, and its ending.
pub const MAX_THREAD_ENTRIES: usize =
    (1 + MAX_TURNS + MAX_PARTICIPANTS + 2 * MAX_HUMAN_QUESTIONS + 1) as usize;

/// The zero-based position of an entry in its thread. Entry 0 opens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageSeq(u32);

impl MessageSeq {
    /// Build from a zero-based position.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The zero-based position.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for MessageSeq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "message {}", self.0)
    }
}

/// A role agent taking part in a thread, and the worker its turns go to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Participant {
    /// The participant's role; unique within the thread.
    pub role: Role,
    /// The worker that receives the participant's turns.
    pub worker: ResourceRef,
}

/// A thread's bounds. Reaching any of them ends the thread as cut off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadBounds {
    /// Participant turns, 1 to [`MAX_TURNS`].
    pub max_turns: u32,
    /// Participants, including invited ones, 2 to [`MAX_PARTICIPANTS`].
    pub max_participants: u32,
    /// Tokens all turns may use together; at least 1. A turn whose usage the
    /// backend cannot report ends the thread, since the bound can no longer
    /// be enforced.
    pub max_tokens: u64,
}

impl ThreadBounds {
    const fn valid(self) -> bool {
        self.max_turns >= 1
            && self.max_turns <= MAX_TURNS
            && self.max_participants >= 2
            && self.max_participants <= MAX_PARTICIPANTS
            && self.max_tokens >= 1
    }
}

/// Everything fixed when a thread opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSpec {
    /// Thread identity, unique within the task.
    pub id: ThreadId,
    /// The house the thread belongs to.
    pub house: HouseId,
    /// The task the thread deliberates about.
    pub task: TaskId,
    /// What the participants discuss, at most [`MAX_TOPIC_BYTES`].
    pub topic: Text,
    /// Participants at the start, at least two and within the bound.
    pub participants: Vec<Participant>,
    /// The thread's bounds.
    pub bounds: ThreadBounds,
}

impl ThreadSpec {
    fn validate(&self) -> Result<(), DeliberationError> {
        let count = u32::try_from(self.participants.len()).unwrap_or(u32::MAX);
        let valid = self.bounds.valid()
            && self.topic.as_str().len() <= MAX_TOPIC_BYTES
            && count >= 2
            && count <= self.bounds.max_participants
            && self.participants.iter().all(worker_participant)
            && self.participants.iter().enumerate().all(|(index, one)| {
                self.participants
                    .iter()
                    .skip(index.saturating_add(1))
                    .all(|other| other.role != one.role)
            });
        if valid {
            Ok(())
        } else {
            Err(DeliberationError::InvalidSpec)
        }
    }
}

fn worker_participant(participant: &Participant) -> bool {
    participant.worker.kind == ResourceKind::Worker
}

/// Usage a backend reported for one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "tokens", rename_all = "kebab-case")]
pub enum TurnUsage {
    /// Tokens the turn used.
    Tokens(u64),
    /// The backend could not report usage.
    Unknown,
}

/// One participant's contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Turn {
    /// The delivery's identity, such as the backend's reply reference. A
    /// retry of the same turn carries the same key and is not written again.
    pub key: ExternalRef,
    /// The participant that spoke.
    pub author: Role,
    /// What it said, at most [`MAX_TURN_BYTES`]. Untrusted.
    pub body: Text,
    /// Participants it mentioned, in order; the first one holds the next turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Role>,
    /// Usage the backend reported for the turn.
    pub usage: TurnUsage,
}

/// What a person answered to a thread's question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HumanAnswer {
    /// The Roger Ask that was answered.
    pub ask: ExternalRef,
    /// The person's instructions, at most 1024 bytes. Untrusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Text>,
    /// Whether longer instructions were cut to fit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// One entry of a thread, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Entry {
    /// The thread opened; always entry 0.
    Opened {
        /// The thread's specification.
        spec: ThreadSpec,
    },
    /// A participant spoke.
    Turn(Turn),
    /// A participant joined. Inviting one beyond the participant bound ends
    /// the thread instead.
    Invited {
        /// The new participant.
        participant: Participant,
    },
    /// The thread asked a person, through the named Roger effect of its task.
    HumanAsked {
        /// The Ask's exact decision scope.
        binding: DecisionBinding,
        /// The task effect that submits the Ask.
        effect: EffectName,
    },
    /// A person answered the pending question.
    HumanAnswered(HumanAnswer),
    /// The pending question expired or was withdrawn without an answer.
    HumanUnanswered {
        /// The Roger Ask that closed.
        ask: ExternalRef,
    },
    /// The coordinator ended the thread with a conclusion.
    Concluded,
    /// The coordinator stopped the thread before a conclusion.
    Stopped,
}

/// Why a thread ended without a conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CutOffReason {
    /// Participants took the maximum number of turns.
    TurnBound,
    /// An invitation would exceed the participant bound.
    ParticipantBound,
    /// Turns used the token bound.
    UsageBound,
    /// A turn's usage was unknown, so the usage bound cannot be enforced.
    UsageUnknown,
    /// A human question expired or was withdrawn.
    HumanUnanswered,
    /// The coordinator stopped the thread.
    Stopped,
}

/// How a thread ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "reason", rename_all = "kebab-case")]
pub enum Closure {
    /// The participants concluded.
    Concluded,
    /// A bound or a stop ended the thread early.
    CutOff(CutOffReason),
}

/// Where a thread stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Participants may take turns.
    Open,
    /// Waiting for a person to answer the pending Ask.
    AwaitingHuman {
        /// The Ask's exact decision scope.
        binding: Box<DecisionBinding>,
        /// The task effect that submits the Ask.
        effect: EffectName,
    },
    /// The thread ended.
    Closed(Closure),
}

/// Who may take the next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextTurn<'a> {
    /// A mentioned participant holds the turn.
    Mentioned(&'a Participant),
    /// Any participant may reply.
    Anyone,
    /// Nobody: the thread waits for a person.
    AwaitingHuman,
    /// Nobody: the thread ended.
    Closed(Closure),
}

/// A thread replayed from its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    spec: ThreadSpec,
    participants: Vec<Participant>,
    entries: Vec<Entry>,
    turns: u32,
    tokens: u64,
    questions: u32,
    queue: Vec<Role>,
    status: ThreadStatus,
}

impl Thread {
    /// Start a thread from its specification.
    pub(super) fn open(spec: ThreadSpec) -> Result<Self, DeliberationError> {
        spec.validate()?;
        Ok(Self {
            participants: spec.participants.clone(),
            entries: vec![Entry::Opened { spec: spec.clone() }],
            spec,
            turns: 0,
            tokens: 0,
            questions: 0,
            queue: Vec::new(),
            status: ThreadStatus::Open,
        })
    }

    /// Replay persisted entries in order. Any entry that the thread would not
    /// admit now means the persisted markers are inconsistent.
    pub(super) fn replay(entries: Vec<Entry>) -> Result<Self, DeliberationError> {
        let mut entries = entries.into_iter();
        let Some(Entry::Opened { spec }) = entries.next() else {
            return Err(DeliberationError::Corrupt);
        };
        let mut thread = Self::open(spec).map_err(|_| DeliberationError::Corrupt)?;
        for entry in entries {
            thread.push(entry).map_err(|_| DeliberationError::Corrupt)?;
        }
        Ok(thread)
    }

    /// The thread's specification.
    #[must_use]
    pub const fn spec(&self) -> &ThreadSpec {
        &self.spec
    }

    /// Current participants, in joining order.
    #[must_use]
    pub fn participants(&self) -> &[Participant] {
        &self.participants
    }

    /// The participant with `role`, if it takes part.
    #[must_use]
    pub fn participant(&self, role: Role) -> Option<&Participant> {
        self.participants.iter().find(|one| one.role == role)
    }

    /// Every entry, in order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry at `seq`.
    #[must_use]
    pub fn entry(&self, seq: MessageSeq) -> Option<&Entry> {
        self.entries.get(usize::try_from(seq.get()).ok()?)
    }

    /// The thread revision: how many entries it holds. A record carries the
    /// revision it summarizes.
    #[must_use]
    pub fn revision(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }

    /// Participant turns taken.
    #[must_use]
    pub const fn turns(&self) -> u32 {
        self.turns
    }

    /// Tokens the turns reported.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Where the thread stands.
    #[must_use]
    pub const fn status(&self) -> &ThreadStatus {
        &self.status
    }

    /// Who may take the next turn.
    #[must_use]
    pub fn next_turn(&self) -> NextTurn<'_> {
        match &self.status {
            ThreadStatus::Closed(closure) => NextTurn::Closed(*closure),
            ThreadStatus::AwaitingHuman { .. } => NextTurn::AwaitingHuman,
            ThreadStatus::Open => self
                .queue
                .first()
                .and_then(|role| self.participant(*role))
                .map_or(NextTurn::Anyone, NextTurn::Mentioned),
        }
    }

    /// The position of the turn delivered as `key`, if it was recorded.
    #[must_use]
    pub fn turn_with_key(&self, key: &ExternalRef) -> Option<MessageSeq> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Entry::Turn(turn) if &turn.key == key))
            .and_then(|index| u32::try_from(index).ok())
            .map(MessageSeq::new)
    }

    /// Whether `role` may take the next turn.
    ///
    /// # Errors
    /// Returns why it may not.
    pub fn may_speak(&self, role: Role) -> Result<(), DeliberationError> {
        self.require_open()?;
        if self.participant(role).is_none() {
            return Err(DeliberationError::NotParticipant(role));
        }
        match self.queue.first() {
            Some(expected) if *expected != role => Err(DeliberationError::NotYourTurn {
                expected: *expected,
            }),
            Some(_) | None => Ok(()),
        }
    }

    fn require_open(&self) -> Result<(), DeliberationError> {
        match self.status {
            ThreadStatus::Open => Ok(()),
            ThreadStatus::AwaitingHuman { .. } => Err(DeliberationError::AwaitingHuman),
            ThreadStatus::Closed(_) => Err(DeliberationError::ThreadClosed),
        }
    }

    /// Admit `entry` and apply it, or refuse it and change nothing.
    pub(super) fn push(&mut self, entry: Entry) -> Result<(), DeliberationError> {
        if self.entries.len() >= MAX_THREAD_ENTRIES {
            return Err(DeliberationError::Corrupt);
        }
        match &entry {
            Entry::Opened { .. } => return Err(DeliberationError::Corrupt),
            Entry::Turn(turn) => self.apply_turn(turn)?,
            Entry::Invited { participant } => self.apply_invite(participant)?,
            Entry::HumanAsked { binding, effect } => self.apply_question(binding, effect)?,
            Entry::HumanAnswered(answer) => {
                self.require_awaiting()?;
                if answer
                    .instructions
                    .as_ref()
                    .is_some_and(|text| text.as_str().len() > MAX_ANSWER_BYTES)
                {
                    return Err(DeliberationError::InvalidContent);
                }
                self.status = ThreadStatus::Open;
            }
            Entry::HumanUnanswered { .. } => {
                self.require_awaiting()?;
                self.status = ThreadStatus::Closed(Closure::CutOff(CutOffReason::HumanUnanswered));
            }
            Entry::Concluded => {
                self.require_open()?;
                self.status = ThreadStatus::Closed(Closure::Concluded);
            }
            Entry::Stopped => {
                if matches!(self.status, ThreadStatus::Closed(_)) {
                    return Err(DeliberationError::ThreadClosed);
                }
                self.status = ThreadStatus::Closed(Closure::CutOff(CutOffReason::Stopped));
            }
        }
        self.entries.push(entry);
        Ok(())
    }

    fn require_awaiting(&self) -> Result<(), DeliberationError> {
        match self.status {
            ThreadStatus::AwaitingHuman { .. } => Ok(()),
            ThreadStatus::Open => Err(DeliberationError::NotAwaitingHuman),
            ThreadStatus::Closed(_) => Err(DeliberationError::ThreadClosed),
        }
    }

    fn apply_turn(&mut self, turn: &Turn) -> Result<(), DeliberationError> {
        self.may_speak(turn.author)?;
        if turn.body.as_str().len() > MAX_TURN_BYTES {
            return Err(DeliberationError::InvalidContent);
        }
        for (index, mention) in turn.mentions.iter().enumerate() {
            if self.participant(*mention).is_none() {
                return Err(DeliberationError::NotParticipant(*mention));
            }
            if *mention == turn.author
                || turn
                    .mentions
                    .get(..index)
                    .is_some_and(|earlier| earlier.contains(mention))
            {
                return Err(DeliberationError::InvalidContent);
            }
        }
        if self.queue.first() == Some(&turn.author) {
            self.queue.remove(0);
        }
        for mention in &turn.mentions {
            if !self.queue.contains(mention) {
                self.queue.push(*mention);
            }
        }
        self.turns = self.turns.saturating_add(1);
        let bounds = self.spec.bounds;
        let cut_off = match turn.usage {
            TurnUsage::Unknown => Some(CutOffReason::UsageUnknown),
            TurnUsage::Tokens(used) => {
                self.tokens = self.tokens.saturating_add(used);
                if self.tokens >= bounds.max_tokens {
                    Some(CutOffReason::UsageBound)
                } else if self.turns >= bounds.max_turns {
                    Some(CutOffReason::TurnBound)
                } else {
                    None
                }
            }
        };
        if let Some(reason) = cut_off {
            self.status = ThreadStatus::Closed(Closure::CutOff(reason));
        }
        Ok(())
    }

    fn apply_invite(&mut self, participant: &Participant) -> Result<(), DeliberationError> {
        self.require_open()?;
        if !worker_participant(participant) {
            return Err(DeliberationError::InvalidSpec);
        }
        if self.participant(participant.role).is_some() {
            return Err(DeliberationError::AlreadyParticipant(participant.role));
        }
        let count = u32::try_from(self.participants.len()).unwrap_or(u32::MAX);
        if count >= self.spec.bounds.max_participants {
            self.status = ThreadStatus::Closed(Closure::CutOff(CutOffReason::ParticipantBound));
        } else {
            self.participants.push(participant.clone());
        }
        Ok(())
    }

    fn apply_question(
        &mut self,
        binding: &DecisionBinding,
        effect: &EffectName,
    ) -> Result<(), DeliberationError> {
        self.require_open()?;
        if self.questions >= MAX_HUMAN_QUESTIONS {
            return Err(DeliberationError::QuestionBound);
        }
        let own_task = format!("task:{}", self.spec.task);
        let scoped = binding.house == self.spec.house
            && binding.task == self.spec.task
            && binding.owner == DecisionOwner::Task
            && binding.action == Permission::AskHuman
            && binding.target.as_str() == own_task
            && binding.subject.is_some()
            && binding.decision_key().is_ok();
        if !scoped {
            return Err(DeliberationError::DecisionMismatch);
        }
        self.questions = self.questions.saturating_add(1);
        self.status = ThreadStatus::AwaitingHuman {
            binding: Box::new(binding.clone()),
            effect: effect.clone(),
        };
        Ok(())
    }
}
