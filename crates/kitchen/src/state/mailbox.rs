//! The house mailbox: worker questions, reports, and escalations kept in the
//! house store, and the answers people or the coordinator give.
//!
//! A backend that carries no worker deliveries still lets a worker reach its
//! coordinator: the worker posts through `kitchn mailbox`, and the
//! coordinator reads the same messages through [`HouseMailbox`], which
//! implements [`CoordinatorMailbox`] on the store. Delivery is at least once
//! per message: the oldest unacknowledged messages form one batch that
//! replays until the coordinator acknowledges it by its batch id.
//!
//! Rules:
//!
//! - A worker posts only for its own task: it names the task and the claim
//!   fence its attempt was launched under, and the task's latest attempt must
//!   still be open. After an adoption the attempt continues under a larger
//!   fence; the launch fence stays valid for that attempt. A fence from
//!   before the previous attempt's claim is refused; a retry under the same
//!   claim shares its fence with the previous attempt, whose worker
//!   coordination has shown stopped before the retry launched. A worker reads
//!   only the answers to its own task's questions.
//! - One coordinator reads the mailbox at a time. The first reader registers
//!   with a current consumer lease; a later coordinator takes over with
//!   [`CoordinatorMailbox::adopt_run`] under a current lease with a larger
//!   fence, and the previous reader's calls then fail with
//!   [`MailboxError::Fenced`]. The adopter gets every unacknowledged message
//!   again under a new batch id. Until the adopter calls `adopt_run`, the
//!   previous reader still reads, so an adopting coordinator calls it first.
//! - The mailbox is bounded ([`MAX_MAILBOX_MESSAGES`],
//!   [`MAX_UNACKNOWLEDGED_PER_TASK`], [`MAX_MAIL_PER_TASK`]) and a full
//!   mailbox refuses the post instead of dropping a message. Retention
//!   ([`crate::state::RetentionPolicy`]) removes acknowledged messages once
//!   their attempt ended or their task settled; unacknowledged messages stay.
//! - A person's answer is recorded as a human reply on the attempt that asked
//!   ([`crate::state::HouseStore::record_attempt_reply`]); a coordinator's
//!   answer records no human time.
//!
//! The store's trust assumptions apply: every process that can open the store
//! runs as the Kitchen user, so the task and fence a worker presents scope its
//! access against mistakes, not against a hostile process of the same user.

use std::{
    collections::VecDeque,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, ErrorClass, TaskId,
    contracts::{
        AttemptNumber, BackendDescriptor, BackendUnavailable, Capability, CheckoutReport, Clock,
        ConsumerFence, ContractError, CoordinatorMailbox, Delivery, Effect, EffectExecutor,
        EffectFailure, EffectRequest, ExternalRef, Fence, Lookup, MAX_MAILBOX_WAIT, MailMessage,
        MailboxError, MessageKind, Operation, Receipt, ResourceKind, ResourceObservation,
        ResourceRef, Support, Text, Timestamp, VerificationEnvironments, WorkerBackend,
        WorkerOutcome, WorkerState,
    },
    state::{AttemptState, EffectState, HouseStore, OwnershipEvent, StateError, TaskRecord},
};

/// Messages the house mailbox holds, acknowledged ones included.
pub const MAX_MAILBOX_MESSAGES: usize = 512;
/// Unacknowledged messages one task may have waiting.
pub const MAX_UNACKNOWLEDGED_PER_TASK: usize = 32;
/// Messages one task may hold, acknowledged ones included.
pub const MAX_MAIL_PER_TASK: usize = 64;
/// Messages in one delivered batch.
pub const MAX_MAIL_BATCH: usize = 16;
/// Longest message body or answer, in bytes.
pub const MAX_MAIL_BODY_BYTES: usize = 8 * 1024;
/// Longest message subject, in bytes.
pub const MAX_MAIL_SUBJECT_BYTES: usize = 256;

/// How often [`HouseMailbox::await_delivery`] rereads the store.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

const MAIL_PREFIX: &str = "house-mail-";
const BATCH_PREFIX: &str = "house-batch-";

/// A house mailbox call was refused. Input text is never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MailError {
    /// The task's latest attempt ended, or it never started one: nothing may
    /// be posted for it or read by its worker.
    #[error("the task has no open attempt")]
    NoOpenAttempt,
    /// The fence is not one the task's open attempt ran under.
    #[error("the fence does not belong to the task's open attempt")]
    NotSender,
    /// No such message for this task, or not in this house's mailbox.
    #[error("no such message for this task")]
    UnknownMessage,
    /// Only a question takes an answer.
    #[error("the message is not a question")]
    NotAQuestion,
    /// The question already has a different answer.
    #[error("the question already has a different answer")]
    AlreadyAnswered,
    /// A person's answer is recorded on the task's owner, and the task has
    /// none now, such as during a handover.
    #[error("the task has no owner to record a person's reply")]
    NoOwner,
    /// The mailbox, or the task's share of it, is full.
    #[error("the house mailbox is full")]
    Full,
    /// A subject or body is longer than the mailbox accepts.
    #[error("the message is longer than the mailbox accepts")]
    TooLarge,
    /// Another coordinator reads the house mailbox.
    #[error("another coordinator reads the house mailbox")]
    Fenced,
}

impl MailError {
    /// Broad handling class.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::UnknownMessage | Self::NotAQuestion | Self::TooLarge => ErrorClass::InvalidInput,
            Self::NoOpenAttempt | Self::NotSender | Self::NoOwner | Self::Full | Self::Fenced => {
                ErrorClass::Refused
            }
            Self::AlreadyAnswered => ErrorClass::Conflict,
        }
    }
}

/// How a worker's task finished, as it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportedOutcome {
    /// The work is done.
    Succeeded,
    /// The worker could not finish.
    Failed,
}

impl From<ReportedOutcome> for WorkerOutcome {
    fn from(outcome: ReportedOutcome) -> Self {
        match outcome {
            ReportedOutcome::Succeeded => Self::Succeeded,
            ReportedOutcome::Failed => Self::Failed,
        }
    }
}

/// What a worker posts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PostKind {
    /// A question that expects an answer.
    Question,
    /// The worker's terminal report.
    Report {
        /// The reported outcome.
        outcome: ReportedOutcome,
        /// The checkout the worker stated; a report posted before reports
        /// carried it reads as unknown.
        #[serde(default)]
        checkout: CheckoutReport,
    },
    /// The worker needs the coordinator to act.
    Escalation,
}

impl PostKind {
    const fn message_kind(self) -> MessageKind {
        match self {
            Self::Question => MessageKind::Question,
            Self::Report { .. } => MessageKind::WorkerDone,
            Self::Escalation => MessageKind::Escalation,
        }
    }
}

/// Who answered a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Answerer {
    /// The coordinator or a scoped agent; no person's time is recorded.
    Coordinator,
    /// A person. The caller states this only on evidence that a person gave
    /// the answer.
    Person,
}

/// The worker a post comes from: its task and the claim fence its attempt
/// was launched under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailSender {
    /// The worker's task.
    pub task: TaskId,
    /// The fence the worker was launched under.
    pub fence: Fence,
}

impl MailSender {
    /// A sender from the raw fence value a worker's brief carries.
    #[must_use]
    pub const fn new(task: TaskId, fence: u64) -> Self {
        Self {
            task,
            fence: Fence::new(fence),
        }
    }
}

/// One post from a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerPost {
    /// What it is.
    pub kind: PostKind,
    /// A one-line subject.
    pub subject: Option<Text>,
    /// The body.
    pub body: Text,
}

/// An answer to a worker question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailAnswer {
    /// The answer.
    pub body: Text,
    /// Who gave it.
    pub by: Answerer,
    /// When it was recorded.
    pub at: Timestamp,
}

/// Whether a question has its answer yet, as its worker reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerState {
    /// Nobody answered yet.
    Pending,
    /// The answer.
    Answered(MailAnswer),
}

/// What recording an answer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answered {
    /// The answer was recorded.
    Recorded,
    /// The same answer was already recorded; nothing changed.
    Duplicate,
}

/// A question still waiting for its answer, as a person or coordinator
/// lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQuestion {
    /// The message id to answer.
    pub id: ExternalRef,
    /// The asking task.
    pub task: TaskId,
    /// The asking attempt.
    pub attempt: AttemptNumber,
    /// The subject.
    pub subject: Option<Text>,
    /// The question.
    pub body: Text,
    /// When it was asked.
    pub asked_at: Timestamp,
}

/// One stored message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredMail {
    seq: u64,
    task: TaskId,
    attempt: AttemptNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker: Option<ResourceRef>,
    kind: PostKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<Text>,
    body: Text,
    posted_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<MailAnswer>,
}

impl StoredMail {
    pub(crate) const fn task(&self) -> &TaskId {
        &self.task
    }

    pub(crate) const fn attempt(&self) -> AttemptNumber {
        self.attempt
    }

    pub(crate) const fn posted_at(&self) -> Timestamp {
        self.posted_at
    }

    pub(crate) fn id(&self) -> Result<ExternalRef, ContractError> {
        mail_id(self.seq)
    }

    fn message(&self) -> Result<MailMessage, ContractError> {
        Ok(MailMessage {
            id: self.id()?,
            kind: self.kind.message_kind(),
            worker: self.worker.clone(),
            outcome: match self.kind {
                PostKind::Report { outcome, .. } => Some(outcome.into()),
                PostKind::Question | PostKind::Escalation => None,
            },
            subject: self.subject.clone(),
            body: Some(self.body.clone()),
            checkout: match self.kind {
                PostKind::Report { checkout, .. } => checkout,
                PostKind::Question | PostKind::Escalation => CheckoutReport::default(),
            },
        })
    }
}

/// The batch the current reader was handed, until acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenBatch {
    seq: u64,
    reader: Fence,
    through: u64,
}

/// The coordinator that reads the mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reader {
    consumer: ConsumerId,
    fence: Fence,
}

/// The persisted house mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Mailbox {
    last_message: u64,
    last_batch: u64,
    acknowledged_through: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reader: Option<Reader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch: Option<OpenBatch>,
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    messages: VecDeque<StoredMail>,
}

impl Mailbox {
    pub(crate) const fn new() -> Self {
        Self {
            last_message: 0,
            last_batch: 0,
            acknowledged_through: 0,
            reader: None,
            batch: None,
            messages: VecDeque::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Stored messages, oldest first, with whether each was acknowledged.
    pub(crate) fn stored(&self) -> impl Iterator<Item = (&StoredMail, bool)> {
        self.messages
            .iter()
            .map(|mail| (mail, mail.seq <= self.acknowledged_through))
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    /// The last message sequence issued, which changes on every post.
    pub(crate) const fn last_posted(&self) -> u64 {
        self.last_message
    }

    /// Remove the acknowledged messages with these sequence numbers.
    pub(crate) fn retire(&mut self, retired: &[ExternalRef]) {
        let acknowledged_through = self.acknowledged_through;
        self.messages.retain(|mail| {
            mail.seq > acknowledged_through
                || !mail
                    .id()
                    .is_ok_and(|id| retired.iter().any(|gone| gone == &id))
        });
    }

    pub(crate) fn post(
        &mut self,
        task: &TaskRecord,
        sender: &MailSender,
        post: WorkerPost,
        now: Timestamp,
    ) -> crate::Result<ExternalRef> {
        if post.body.as_str().len() > MAX_MAIL_BODY_BYTES
            || post
                .subject
                .as_ref()
                .is_some_and(|subject| subject.as_str().len() > MAX_MAIL_SUBJECT_BYTES)
        {
            return Err(MailError::TooLarge.into());
        }
        let (attempt, worker) = seat(task, sender.fence)?;
        let held = self.messages.iter().filter(|mail| mail.task == sender.task);
        let (total, waiting) = held.fold((0_usize, 0_usize), |(total, waiting), mail| {
            (
                total.saturating_add(1),
                waiting.saturating_add(usize::from(mail.seq > self.acknowledged_through)),
            )
        });
        if self.messages.len() >= MAX_MAILBOX_MESSAGES
            || total >= MAX_MAIL_PER_TASK
            || waiting >= MAX_UNACKNOWLEDGED_PER_TASK
        {
            return Err(MailError::Full.into());
        }
        let seq = self.last_message.saturating_add(1);
        let id = mail_id(seq)?;
        self.last_message = seq;
        self.messages.push_back(StoredMail {
            seq,
            task: sender.task.clone(),
            attempt,
            worker,
            kind: post.kind,
            subject: post.subject,
            body: post.body,
            posted_at: now,
            answer: None,
        });
        Ok(id)
    }

    /// The answer to `question`, for the worker that asked it.
    pub(crate) fn answer_for(
        &self,
        task: &TaskRecord,
        sender: &MailSender,
        question: &ExternalRef,
    ) -> crate::Result<AnswerState> {
        let (attempt, _) = seat(task, sender.fence)?;
        let mail = self
            .find(question)
            .filter(|mail| mail.task == sender.task && mail.attempt == attempt)
            .ok_or(MailError::UnknownMessage)?;
        if mail.kind != PostKind::Question {
            return Err(MailError::NotAQuestion.into());
        }
        Ok(mail
            .answer
            .clone()
            .map_or(AnswerState::Pending, AnswerState::Answered))
    }

    /// The question `id` names and whether `answer` repeats its recorded
    /// answer. A different recorded answer is a conflict.
    pub(crate) fn question(
        &self,
        id: &ExternalRef,
        answer: &MailAnswer,
    ) -> crate::Result<(&StoredMail, Answered)> {
        let mail = self.find(id).ok_or(MailError::UnknownMessage)?;
        if mail.kind != PostKind::Question {
            return Err(MailError::NotAQuestion.into());
        }
        match &mail.answer {
            None => Ok((mail, Answered::Recorded)),
            Some(recorded) if recorded.body == answer.body && recorded.by == answer.by => {
                Ok((mail, Answered::Duplicate))
            }
            Some(_) => Err(MailError::AlreadyAnswered.into()),
        }
    }

    pub(crate) fn set_answer(&mut self, id: &ExternalRef, answer: MailAnswer) -> crate::Result<()> {
        let seq = parse_seq(id, MAIL_PREFIX).ok_or(MailError::UnknownMessage)?;
        let mail = self
            .messages
            .iter_mut()
            .find(|mail| mail.seq == seq)
            .ok_or(MailError::UnknownMessage)?;
        mail.answer = Some(answer);
        Ok(())
    }

    /// Questions without an answer, oldest first, at most `limit`.
    pub(crate) fn open_questions(&self, limit: usize) -> crate::Result<Vec<OpenQuestion>> {
        self.messages
            .iter()
            .filter(|mail| mail.kind == PostKind::Question && mail.answer.is_none())
            .take(limit)
            .map(|mail| {
                Ok(OpenQuestion {
                    id: mail.id()?,
                    task: mail.task.clone(),
                    attempt: mail.attempt,
                    subject: mail.subject.clone(),
                    body: mail.body.clone(),
                    asked_at: mail.posted_at,
                })
            })
            .collect()
    }

    /// Whether `fence` is the registered reader. `None` when nobody
    /// registered yet.
    pub(crate) fn reads(&self, fence: Fence) -> Option<bool> {
        self.reader.as_ref().map(|reader| reader.fence == fence)
    }

    /// Make `consumer` at `fence` the reader. The caller checked that the
    /// consumer lease is current; a reader with a larger fence is never
    /// replaced.
    pub(crate) fn register(&mut self, consumer: &ConsumerId, fence: Fence) -> crate::Result<()> {
        match &self.reader {
            Some(reader) if reader.fence > fence => Err(MailError::Fenced.into()),
            Some(_) | None => {
                self.reader = Some(Reader {
                    consumer: consumer.clone(),
                    fence,
                });
                Ok(())
            }
        }
    }

    /// The oldest unacknowledged batch for the reader at `fence`, formed now
    /// if the reader has none. The caller checked that `fence` reads.
    pub(crate) fn delivery(&mut self, fence: Fence) -> crate::Result<Option<Delivery>> {
        let acknowledged = self.acknowledged_through;
        let mut waiting = self
            .messages
            .iter()
            .filter(|mail| mail.seq > acknowledged)
            .peekable();
        if waiting.peek().is_none() {
            self.batch = None;
            return Ok(None);
        }
        let batch = match self.batch {
            Some(batch) if batch.reader == fence => batch,
            Some(_) | None => {
                let through = waiting
                    .take(MAX_MAIL_BATCH)
                    .last()
                    .map_or(acknowledged, |mail| mail.seq);
                let batch = OpenBatch {
                    seq: self.last_batch.saturating_add(1),
                    reader: fence,
                    through,
                };
                self.last_batch = batch.seq;
                self.batch = Some(batch);
                batch
            }
        };
        let messages = self
            .messages
            .iter()
            .filter(|mail| mail.seq > acknowledged && mail.seq <= batch.through)
            .map(StoredMail::message)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Delivery {
            id: batch_id(batch.seq)?,
            messages,
            unreadable: 0,
        }))
    }

    /// Acknowledge the reader's current batch when `id` names it, then
    /// return the next one. Any other id consumes nothing.
    pub(crate) fn acknowledge(
        &mut self,
        fence: Fence,
        id: &ExternalRef,
    ) -> crate::Result<Option<Delivery>> {
        if let Some(batch) = self.batch
            && batch.reader == fence
            && parse_seq(id, BATCH_PREFIX) == Some(batch.seq)
        {
            self.acknowledged_through = batch.through;
            self.batch = None;
        }
        self.delivery(fence)
    }

    fn find(&self, id: &ExternalRef) -> Option<&StoredMail> {
        let seq = parse_seq(id, MAIL_PREFIX)?;
        self.messages.iter().find(|mail| mail.seq == seq)
    }

    /// Stored invariants: bounds, increasing sequences, and cursors that
    /// never pass the last issued number.
    pub(crate) fn validate(&self) -> bool {
        let ordered = self
            .messages
            .iter()
            .zip(self.messages.iter().skip(1))
            .all(|(earlier, later)| earlier.seq < later.seq);
        let bounded = self.messages.len() <= MAX_MAILBOX_MESSAGES
            && self.messages.iter().all(|mail| {
                mail.seq >= 1
                    && mail.seq <= self.last_message
                    && mail.body.as_str().len() <= MAX_MAIL_BODY_BYTES
                    && mail
                        .subject
                        .as_ref()
                        .is_none_or(|subject| subject.as_str().len() <= MAX_MAIL_SUBJECT_BYTES)
                    && mail
                        .answer
                        .as_ref()
                        .is_none_or(|answer| answer.body.as_str().len() <= MAX_MAIL_BODY_BYTES)
            });
        let batch = self.batch.is_none_or(|batch| {
            batch.seq <= self.last_batch
                && batch.through > self.acknowledged_through
                && batch.through <= self.last_message
        });
        ordered && bounded && batch && self.acknowledged_through <= self.last_message
    }
}

/// The attempt a worker at `fence` runs, and its launched worker when the
/// launch already applied.
fn seat(
    task: &TaskRecord,
    fence: Fence,
) -> Result<(AttemptNumber, Option<ResourceRef>), MailError> {
    let attempt = task
        .attempts()
        .last()
        .filter(|attempt| {
            matches!(
                attempt.state(),
                AttemptState::Running | AttemptState::Interrupted { .. }
            )
        })
        .ok_or(MailError::NoOpenAttempt)?;
    // The fences the task's claims issued, from the previous attempt's
    // (a retry may run under the same claim) to this attempt's current one.
    let floor = task
        .attempts()
        .iter()
        .rev()
        .nth(1)
        .map_or(Fence::new(0), |previous| previous.fence());
    let issued = task.ownership().iter().any(|event| match event {
        OwnershipEvent::Claimed { fence: issued, .. }
        | OwnershipEvent::Adopted { fence: issued, .. }
        | OwnershipEvent::TakenOver { fence: issued, .. } => *issued == fence,
        OwnershipEvent::Relinquished { .. } | OwnershipEvent::Released { .. } => false,
    });
    if !issued || fence < floor || fence > attempt.fence() {
        return Err(MailError::NotSender);
    }
    let worker = task
        .effects()
        .iter()
        .rev()
        .filter(|effect| effect.request().attempt() == attempt.number())
        .find_map(|effect| match (effect.request().effect(), effect.state()) {
            (
                Effect::Worker(Operation::LaunchWorker { .. }),
                EffectState::Applied { receipt, .. },
            ) => receipt
                .created()
                .iter()
                .find(|resource| resource.kind == ResourceKind::Worker)
                .cloned(),
            _ => None,
        });
    Ok((attempt.number(), worker))
}

fn mail_id(seq: u64) -> Result<ExternalRef, ContractError> {
    ExternalRef::new(&format!("{MAIL_PREFIX}{seq}"))
}

fn batch_id(seq: u64) -> Result<ExternalRef, ContractError> {
    ExternalRef::new(&format!("{BATCH_PREFIX}{seq}"))
}

fn parse_seq(id: &ExternalRef, prefix: &str) -> Option<u64> {
    let digits = id.as_str().strip_prefix(prefix)?;
    // Reject signs and leading zeros, so one message has one id.
    if digits.starts_with(['+', '0']) {
        return None;
    }
    digits.parse().ok()
}

/// The house store's mailbox as a [`CoordinatorMailbox`] for the
/// coordinator holding the consumer lease `consumer` at `fence`.
///
/// Worker effects still go to `backend`; this wrapper exists because the
/// mailbox contract extends [`WorkerBackend`]. Its descriptor is the
/// backend's with [`Capability::WorkerDeliveries`] and
/// [`Capability::RunTransfer`] added, both provided by the store, so use
/// the backend itself as the executor for effects.
pub struct HouseMailbox<'a> {
    store: &'a HouseStore,
    backend: &'a dyn WorkerBackend,
    clock: &'a dyn Clock,
    descriptor: BackendDescriptor,
    consumer: ConsumerId,
    fence: Fence,
}

impl<'a> HouseMailbox<'a> {
    /// The mailbox of `store` for the coordinator holding `consumer` at
    /// `fence`, whose workers run on `backend`. `clock` decides whether the
    /// consumer lease is live.
    ///
    /// # Errors
    /// [`ContractError::CrossHouse`] when `backend` serves another house.
    pub fn new(
        store: &'a HouseStore,
        backend: &'a dyn WorkerBackend,
        clock: &'a dyn Clock,
        consumer: ConsumerId,
        fence: Fence,
    ) -> crate::Result<Self> {
        let inner = backend.descriptor();
        if &inner.house != store.house() {
            return Err(ContractError::CrossHouse {
                expected: store.house().clone(),
                found: inner.house.clone(),
            }
            .into());
        }
        let mut descriptor = inner.clone();
        descriptor.capabilities = descriptor
            .capabilities
            .with(Capability::WorkerDeliveries, Support::Supported)
            .with(Capability::RunTransfer, Support::Supported);
        Ok(Self {
            store,
            backend,
            clock,
            descriptor,
            consumer,
            fence,
        })
    }

    fn reader(&self) -> ConsumerFence {
        ConsumerFence {
            consumer: self.consumer.clone(),
            fence: self.fence,
        }
    }
}

/// A store failure as the mailbox contract reports it: this reader was
/// fenced, or nothing can be inferred from the mailbox.
fn mailbox_error(error: &crate::Error) -> MailboxError {
    match error {
        crate::Error::Mail(MailError::Fenced)
        | crate::Error::State(
            StateError::StaleFence { .. }
            | StateError::LeaseExpired { .. }
            | StateError::ConsumerNotFound(_),
        ) => MailboxError::Fenced,
        _ => MailboxError::Unavailable(BackendUnavailable::Transport),
    }
}

impl EffectExecutor for HouseMailbox<'_> {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.backend.execute(request)
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.backend.lookup(request)
    }

    fn verification_environments(&self) -> &VerificationEnvironments {
        self.backend.verification_environments()
    }
}

impl WorkerBackend for HouseMailbox<'_> {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.backend.observe_worker(worker)
    }

    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        self.backend.inventory()
    }
}

impl CoordinatorMailbox for HouseMailbox<'_> {
    fn adopt_run(&self) -> Result<(), MailboxError> {
        self.store
            .adopt_mailbox(&self.reader(), self.clock.now())
            .map_err(|error| mailbox_error(&error))
    }

    fn next_delivery(&self) -> Result<Option<Delivery>, MailboxError> {
        self.store
            .mail_delivery(&self.reader(), self.clock.now())
            .map_err(|error| mailbox_error(&error))
    }

    fn acknowledge(&self, delivery: &ExternalRef) -> Result<Option<Delivery>, MailboxError> {
        self.store
            .acknowledge_mail(&self.reader(), delivery, self.clock.now())
            .map_err(|error| mailbox_error(&error))
    }

    fn await_delivery(&self, wait: Duration) -> Result<Option<Delivery>, MailboxError> {
        let deadline = Instant::now().checked_add(wait.min(MAX_MAILBOX_WAIT));
        loop {
            let delivery = self.next_delivery()?;
            if delivery.as_ref().is_some_and(|batch| !batch.is_idle()) {
                return Ok(delivery);
            }
            // Wait under the shared lock until a worker posts, so workers
            // are not held off by a write transaction on every poll.
            let seen = self
                .store
                .mail_last_posted()
                .map_err(|error| mailbox_error(&error))?;
            loop {
                let left = deadline.map_or(Duration::ZERO, |deadline| {
                    deadline.saturating_duration_since(Instant::now())
                });
                if left.is_zero() {
                    return Ok(delivery);
                }
                thread::sleep(left.min(POLL_INTERVAL));
                if self
                    .store
                    .mail_last_posted()
                    .map_err(|error| mailbox_error(&error))?
                    != seen
                {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_only_their_own_canonical_form() -> Result<(), ContractError> {
        assert_eq!(parse_seq(&mail_id(7)?, MAIL_PREFIX), Some(7));
        assert_eq!(parse_seq(&batch_id(7)?, MAIL_PREFIX), None);
        for other in [
            "house-mail-07",
            "house-mail-+7",
            "house-mail-",
            "house-mail-x",
            "mail-7",
        ] {
            assert_eq!(
                parse_seq(&ExternalRef::new(other)?, MAIL_PREFIX),
                None,
                "{other}"
            );
        }
        Ok(())
    }

    #[test]
    fn error_classes_separate_input_refusal_and_conflict() {
        assert_eq!(MailError::TooLarge.class(), ErrorClass::InvalidInput);
        assert_eq!(MailError::NotSender.class(), ErrorClass::Refused);
        assert_eq!(MailError::AlreadyAnswered.class(), ErrorClass::Conflict);
    }
}
