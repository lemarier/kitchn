//! `kitchn mailbox`: the house mailbox for workers whose backend carries no
//! worker messages.
//!
//! A worker posts a question, report, or escalation for its own task, naming
//! the task and the fence its brief gives, and reads the answers to its own
//! questions. A person or the coordinator lists open questions and answers
//! them; a person's answer is recorded as human time on the asking attempt.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    HouseId, TaskId,
    contracts::{Clock, ExternalRef, SystemClock, Text},
    house::HouseError,
    state::{
        AnswerState, Answered, Answerer, HouseStore, MailAnswer, MailSender, PostKind,
        ReportedOutcome, StoreOptions, WorkerPost,
    },
};

/// Longest wait for an answer, in seconds.
const MAX_WAIT_SECS: u64 = 15 * 60;
/// How often a wait rereads the store.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Open questions `questions` lists at most.
const MAX_LISTED: usize = 100;

#[derive(Args)]
pub struct MailboxArgs {
    #[command(subcommand)]
    command: MailboxCommand,
}

#[derive(Subcommand)]
enum MailboxCommand {
    /// Worker: ask the coordinator a question, optionally waiting for the answer.
    Ask {
        #[command(flatten)]
        worker: WorkerScope,
        #[command(flatten)]
        message: Message,
        /// Seconds to wait for the answer, at most 900. Without it the
        /// question id is printed and the command returns.
        #[arg(long, default_value_t = 0)]
        wait_secs: u64,
    },
    /// Worker: read the answer to one of this task's questions.
    Answer {
        #[command(flatten)]
        worker: WorkerScope,
        /// The question's id, as `ask` printed it.
        #[arg(long)]
        question: ExternalRef,
        /// Seconds to wait for the answer, at most 900. Without an answer
        /// by then it prints `answer: pending` and exits 0.
        #[arg(long, default_value_t = 0)]
        wait_secs: u64,
    },
    /// Worker: ask the coordinator to act.
    Escalate {
        #[command(flatten)]
        worker: WorkerScope,
        #[command(flatten)]
        message: Message,
    },
    /// Worker: report the task's outcome once, when done.
    Report {
        #[command(flatten)]
        worker: WorkerScope,
        #[command(flatten)]
        message: Message,
        #[arg(long, value_enum)]
        outcome: Outcome,
    },
    /// Person or coordinator: list unanswered questions. Reads only.
    Questions {
        #[command(flatten)]
        house: HouseScope,
    },
    /// Person or coordinator: answer a worker's question.
    Reply {
        #[command(flatten)]
        house: HouseScope,
        /// The question's id.
        #[arg(long)]
        question: ExternalRef,
        /// The answer.
        #[arg(long)]
        body: Text,
        /// Who answers. Say `person` only when a person gave the answer; it
        /// is recorded as their time on the task.
        #[arg(long, value_enum)]
        by: By,
    },
}

#[derive(Args)]
struct HouseScope {
    #[arg(long)]
    house: HouseId,
    /// Absolute path of the house's initialized state store (default: the
    /// one `house init` created in --registry).
    #[arg(long, required_unless_present = "registry")]
    store: Option<PathBuf>,
    /// The house registry, used only to locate the house store.
    #[arg(long)]
    registry: Option<PathBuf>,
}

#[derive(Args)]
struct WorkerScope {
    #[command(flatten)]
    house: HouseScope,
    /// The worker's task, from its brief.
    #[arg(long)]
    task: TaskId,
    /// The fence from the worker's brief.
    #[arg(long)]
    fence: u64,
}

#[derive(Args)]
struct Message {
    /// A one-line subject.
    #[arg(long)]
    subject: Option<Text>,
    /// The message.
    #[arg(long)]
    body: Text,
}

#[derive(Clone, Copy, ValueEnum)]
enum Outcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, ValueEnum)]
enum By {
    Person,
    Coordinator,
}

pub fn run(args: MailboxArgs) -> Result<(String, bool), kitchen::Error> {
    let clock = SystemClock;
    match args.command {
        MailboxCommand::Ask {
            worker,
            message,
            wait_secs,
        } => {
            let wait = wait(wait_secs)?;
            let (store, sender) = worker.open()?;
            let id = post(&store, &sender, PostKind::Question, message, &clock)?;
            // The question is stored; a failed wait must still print its id.
            Ok(match await_answer(&store, &sender, &id, wait) {
                Ok(answer) => (format!("question: {id}\n{}", answer_text(&answer)), true),
                Err(error) => (
                    format!("question: {id}\nanswer: unavailable: {error}"),
                    false,
                ),
            })
        }
        MailboxCommand::Answer {
            worker,
            question,
            wait_secs,
        } => {
            let wait = wait(wait_secs)?;
            let (store, sender) = worker.open()?;
            let answer = await_answer(&store, &sender, &question, wait)?;
            Ok((answer_text(&answer), true))
        }
        MailboxCommand::Escalate { worker, message } => {
            let (store, sender) = worker.open()?;
            let id = post(&store, &sender, PostKind::Escalation, message, &clock)?;
            Ok((format!("escalation: {id}"), true))
        }
        MailboxCommand::Report {
            worker,
            message,
            outcome,
        } => {
            let (store, sender) = worker.open()?;
            let outcome = match outcome {
                Outcome::Succeeded => ReportedOutcome::Succeeded,
                Outcome::Failed => ReportedOutcome::Failed,
            };
            let id = post(
                &store,
                &sender,
                PostKind::Report { outcome },
                message,
                &clock,
            )?;
            Ok((format!("report: {id}"), true))
        }
        MailboxCommand::Questions { house } => {
            let store = house.open()?;
            let questions = store.open_questions(MAX_LISTED)?;
            let usage = store.mailbox_usage()?;
            let mut text = format!("mailbox: {} of {}", usage.used, usage.limit);
            for question in &questions {
                let _ = write!(
                    text,
                    "\n{} task {} attempt {}: {}",
                    question.id,
                    question.task,
                    question.attempt.get(),
                    // Worker text: escaped so it cannot drive the terminal.
                    question
                        .subject
                        .as_ref()
                        .unwrap_or(&question.body)
                        .as_str()
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .escape_debug()
                );
            }
            if questions.is_empty() {
                text.push_str("\nno open questions");
            }
            Ok((text, true))
        }
        MailboxCommand::Reply {
            house,
            question,
            body,
            by,
        } => {
            let store = house.open()?;
            let answer = MailAnswer {
                body,
                by: match by {
                    By::Person => Answerer::Person,
                    By::Coordinator => Answerer::Coordinator,
                },
                at: clock.now(),
            };
            Ok((
                match store.answer_mail(&question, answer)? {
                    Answered::Recorded => format!("answered: {question}"),
                    Answered::Duplicate => format!("already answered: {question}"),
                },
                true,
            ))
        }
    }
}

impl HouseScope {
    fn open(self) -> Result<HouseStore, kitchen::Error> {
        let store =
            super::house::store_or_default(self.store, self.registry.as_deref(), &self.house)?;
        open(&store, self.house)
    }
}

impl WorkerScope {
    fn open(self) -> Result<(HouseStore, MailSender), kitchen::Error> {
        Ok((self.house.open()?, MailSender::new(self.task, self.fence)))
    }
}

fn open(store: &Path, house: HouseId) -> Result<HouseStore, kitchen::Error> {
    if !store.is_absolute() {
        return Err(HouseError::InvalidInput.into());
    }
    HouseStore::open(store, house, StoreOptions::default())
}

fn wait(secs: u64) -> Result<Duration, HouseError> {
    if secs > MAX_WAIT_SECS {
        return Err(HouseError::InvalidInput);
    }
    Ok(Duration::from_secs(secs))
}

fn post(
    store: &HouseStore,
    sender: &MailSender,
    kind: PostKind,
    message: Message,
    clock: &dyn Clock,
) -> Result<ExternalRef, kitchen::Error> {
    store.post_mail(
        sender,
        WorkerPost {
            kind,
            subject: message.subject,
            body: message.body,
        },
        clock.now(),
    )
}

/// Read the answer, rereading the store until it arrives or `wait` passes.
fn await_answer(
    store: &HouseStore,
    sender: &MailSender,
    question: &ExternalRef,
    wait: Duration,
) -> Result<AnswerState, kitchen::Error> {
    let deadline = Instant::now().checked_add(wait);
    loop {
        let answer = store.mail_answer(sender, question)?;
        let left = deadline.map_or(Duration::ZERO, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        if matches!(answer, AnswerState::Answered(_)) || left.is_zero() {
            return Ok(answer);
        }
        thread::sleep(left.min(POLL_INTERVAL));
    }
}

fn answer_text(answer: &AnswerState) -> String {
    match answer {
        AnswerState::Pending => "answer: pending".to_owned(),
        AnswerState::Answered(answer) => format!("answer:\n{}", answer.body.as_str()),
    }
}
