//! Read-only resource inventory and the coordinator mailbox.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::{
    adapters::orca::{OrcaBackend, OrcaError, OrcaRunner, backend, wire},
    contracts::{
        CoordinatorMailbox, Delivery, ExternalRef, Liveness, MAX_MAILBOX_WAIT, MAX_TEXT_BYTES,
        MailMessage, MailboxError, MessageKind, ResourceKind, ResourceRef, Text, WorkerOutcome,
        WorkerState,
    },
};

/// Most `worker-list` pages read for one inventory (100 rows each).
pub const MAX_INVENTORY_PAGES: usize = 10;

/// Rows requested per inventory page.
const PAGE_ROWS: &str = "100";

/// Who owns a worker's terminal and whether it was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalAccounting {
    /// Owned and in use.
    Active,
    /// Settled and owned; release is allowed.
    Reclaimable,
    /// Deliberately kept.
    Retained,
    /// Release requested but not confirmed.
    ReleasePending,
    /// Release outcome unknown.
    ReleaseUnknown,
    /// Released.
    Released,
    /// Not reported or not recognized.
    Unknown,
}

/// Why Orca kept a worker's terminal instead of releasing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetainedReason {
    /// A person took the terminal over. It is theirs now: not a failure,
    /// and Kitchen sends it no work without asking.
    UserTakeover,
    /// A person asked Orca to keep it.
    UserRequested,
    /// Orca did not create the terminal.
    ExternalTerminal,
    /// Ownership moved to another Dispatch.
    OwnershipTransferred,
    /// Orca could not prove the terminal's identity.
    IdentityUnproven,
    /// A reason Kitchen does not recognize.
    Other,
}

impl RetainedReason {
    fn from_wire(value: &str) -> Self {
        match value {
            "user_takeover" => Self::UserTakeover,
            "user_requested" => Self::UserRequested,
            "external_terminal" => Self::ExternalTerminal,
            "ownership_transferred" => Self::OwnershipTransferred,
            "identity_unproven" => Self::IdentityUnproven,
            _ => Self::Other,
        }
    }
}

/// One supervised worker in this backend's Run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecord {
    /// The worker.
    pub worker: ResourceRef,
    /// The idempotency key of the launch that created it, read from its
    /// Orca Task title; `None` for workers Kitchen did not launch.
    pub owner: Option<ExternalRef>,
    /// The mapped worker state. `worker-list` omits Orca's human-wait
    /// probe, so inventory never reports [`WorkerState::AwaitingReply`]; use
    /// [`crate::contracts::WorkerBackend::observe_worker`] for that.
    pub state: WorkerState,
    /// Orca's liveness verdict.
    pub liveness: Liveness,
    /// Terminal ownership accounting.
    pub terminal: TerminalAccounting,
    /// Why the terminal was retained, when Orca says.
    pub retained: Option<RetainedReason>,
}

#[derive(Deserialize)]
struct WorkerList {
    workers: Vec<ListRow>,
    page: Page,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    has_more: bool,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRow {
    dispatch_id: String,
    #[serde(default)]
    task_id: Option<String>,
    worker_state: String,
    #[serde(default)]
    terminal_state: Option<String>,
    #[serde(default)]
    resource: Option<backend::TerminalResource>,
    projection: backend::Projection,
}

/// The contract's type for an Orca message type.
fn message_kind(value: &str) -> MessageKind {
    match value {
        "question" => MessageKind::Question,
        "worker_done" => MessageKind::WorkerDone,
        "escalation" => MessageKind::Escalation,
        "heartbeat" => MessageKind::Heartbeat,
        "status" | "dispatch" | "handoff" | "merge_ready" | "decision_gate" => MessageKind::Status,
        _ => MessageKind::Other,
    }
}

/// A mailbox call failure on the contract: Orca refuses a terminal that
/// lost the Run to an adopting coordinator with `consumer_fenced`.
fn mailbox_failure(error: &OrcaError) -> MailboxError {
    match error {
        OrcaError::Refused { code, .. } if code == "consumer_fenced" => MailboxError::Fenced,
        other => MailboxError::Unavailable(backend::read_failure(other)),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Check {
    #[serde(default)]
    delivery_id: Option<String>,
    #[serde(default)]
    messages: Vec<CheckMessage>,
}

#[derive(Deserialize)]
struct CheckMessage {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportPayload {
    #[serde(default)]
    dispatch_id: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
}

/// Truncate untrusted text to Kitchen's bound at a character boundary.
fn bounded_text(value: Option<&str>) -> Option<Text> {
    let value = value?.replace('\0', "");
    let mut end = value.len().min(MAX_TEXT_BYTES);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.get(..end).and_then(|text| Text::new(text).ok())
}

pub(crate) fn liveness(verdict: &str) -> Liveness {
    match verdict {
        "live" => Liveness::Live,
        "exited" => Liveness::Exited,
        _ => Liveness::Unverifiable,
    }
}

fn terminal(state: Option<&str>) -> TerminalAccounting {
    match state {
        Some("active") => TerminalAccounting::Active,
        Some("reclaimable") => TerminalAccounting::Reclaimable,
        Some("retained") => TerminalAccounting::Retained,
        Some("release_pending") => TerminalAccounting::ReleasePending,
        Some("release_unknown") => TerminalAccounting::ReleaseUnknown,
        Some("released") => TerminalAccounting::Released,
        Some(_) | None => TerminalAccounting::Unknown,
    }
}

impl<R: OrcaRunner> OrcaBackend<R> {
    fn worker_ref(&self, dispatch: &str) -> Option<ResourceRef> {
        ExternalRef::new(dispatch).ok().map(|handle| ResourceRef {
            kind: ResourceKind::Worker,
            backend: self.config().backend.clone(),
            handle,
        })
    }

    /// List every supervised worker in this backend's Run.
    ///
    /// Reads at most [`MAX_INVENTORY_PAGES`] pages. A longer listing is an
    /// error rather than a partial answer, because cleanup must not act on
    /// an incomplete inventory. Orca does not report dirty or unpushed work.
    ///
    /// # Errors
    /// Call, parse, and [`OrcaError::ListingTooLong`] failures.
    pub fn worker_records(&self) -> Result<Vec<WorkerRecord>, OrcaError> {
        let owners = self.launch_owners()?;
        let mut records = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_INVENTORY_PAGES {
            let mut args = wire::Args::command(&["orchestration", "worker-list"])
                .value("run", self.config().run.as_str())
                .value("limit", PAGE_ROWS);
            if let Some(cursor) = &cursor {
                args = args.value("cursor", cursor);
            }
            let list: WorkerList = wire::typed(
                self.call(args.json(), self.config().call_timeout)?,
                "worker list",
            )?;
            for row in list.workers {
                let worker = self
                    .worker_ref(&row.dispatch_id)
                    .ok_or(OrcaError::Malformed {
                        what: "worker list",
                    })?;
                records.push(WorkerRecord {
                    worker,
                    owner: row
                        .task_id
                        .as_ref()
                        .and_then(|task| owners.get(task).cloned()),
                    state: backend::with_takeover(
                        backend::worker_state(
                            &row.worker_state,
                            &row.projection.outcome,
                            &row.projection.liveness.verdict,
                            false,
                        ),
                        row.resource
                            .as_ref()
                            .is_some_and(backend::TerminalResource::person_owns),
                    ),
                    liveness: liveness(&row.projection.liveness.verdict),
                    terminal: terminal(row.terminal_state.as_deref()),
                    retained: row.resource.as_ref().and_then(|resource| {
                        if resource.person_owns() {
                            Some(RetainedReason::UserTakeover)
                        } else {
                            resource
                                .retained_reason
                                .as_deref()
                                .map(RetainedReason::from_wire)
                        }
                    }),
                });
            }
            match (list.page.has_more, list.page.next_cursor) {
                (false, _) => return Ok(records),
                (true, Some(next)) => cursor = Some(next),
                (true, None) => {
                    return Err(OrcaError::Malformed {
                        what: "worker list",
                    });
                }
            }
        }
        Err(OrcaError::ListingTooLong {
            limit: MAX_INVENTORY_PAGES.saturating_mul(100),
        })
    }

    fn delivery(&self, args: Vec<String>) -> Result<Option<Delivery>, OrcaError> {
        self.delivery_within(args, self.config().call_timeout)
    }

    fn delivery_within(
        &self,
        args: Vec<String>,
        deadline: Duration,
    ) -> Result<Option<Delivery>, OrcaError> {
        let check: Check = wire::typed(self.call(args, deadline)?, "mailbox")?;
        let Some(id) = check.delivery_id else {
            return Ok(None);
        };
        let id = ExternalRef::new(&id).map_err(|_| OrcaError::Malformed { what: "mailbox" })?;
        let mut unreadable = 0_usize;
        let messages = check
            .messages
            .into_iter()
            .filter_map(|message| {
                let parsed = self.mail_message(message);
                if parsed.is_none() {
                    unreadable = unreadable.saturating_add(1);
                }
                parsed
            })
            .collect();
        Ok(Some(Delivery {
            id,
            messages,
            unreadable,
        }))
    }

    fn mail_message(&self, message: CheckMessage) -> Option<MailMessage> {
        let id = ExternalRef::new(&message.id).ok()?;
        let kind = message_kind(&message.kind);
        let report = message
            .payload
            .and_then(|payload| serde_json::from_value::<ReportPayload>(payload).ok());
        let worker = report
            .as_ref()
            .and_then(|report| report.dispatch_id.as_deref())
            .and_then(|dispatch| self.worker_ref(dispatch));
        let outcome = match (kind, report.as_ref().and_then(|r| r.outcome.as_deref())) {
            (MessageKind::WorkerDone, Some("succeeded")) => Some(WorkerOutcome::Succeeded),
            (MessageKind::WorkerDone, Some("failed")) => Some(WorkerOutcome::Failed),
            _ => None,
        };
        Some(MailMessage {
            id,
            kind,
            worker,
            outcome,
            subject: bounded_text(message.subject.as_deref()),
            body: bounded_text(message.body.as_deref()),
        })
    }
}

/// The Run mailbox, read by this instance's coordinator terminal.
impl<R: OrcaRunner> CoordinatorMailbox for OrcaBackend<R> {
    /// Bind this instance's coordinator terminal to its Run, so the Run's
    /// mailbox and worker mail reach it.
    ///
    /// Call this only after Kitchen recorded the adoption (a relinquish
    /// followed by an adopt in the state store). Orca records no relinquish,
    /// and binding neither stops nor moves workers; the previous coordinator
    /// terminal loses the mailbox.
    fn adopt_run(&self) -> Result<(), MailboxError> {
        let args = wire::Args::command(&["orchestration", "run-use"])
            .value("id", self.config().run.as_str())
            .value("from", self.config().coordinator.as_str())
            .json();
        self.call(args, self.config().call_timeout)
            .map(|_| ())
            .map_err(|error| mailbox_failure(&error))
    }

    /// Read the oldest unacknowledged mailbox batch without consuming it.
    ///
    /// Orca replays the same batch until [`CoordinatorMailbox::acknowledge`]
    /// names it, so a crash between reading and handling loses nothing.
    fn next_delivery(&self) -> Result<Option<Delivery>, MailboxError> {
        let args = wire::Args::command(&["orchestration", "check"])
            .value("terminal", self.config().coordinator.as_str())
            .value("run", self.config().run.as_str())
            .json();
        self.delivery(args).map_err(|error| mailbox_failure(&error))
    }

    /// Acknowledge a batch after every message in it was handled, and read
    /// the next one.
    ///
    /// Orca 1.4.216 refuses an acknowledgement of a batch id issued before
    /// this terminal adopted the Run with `consumer_fenced`, although the
    /// terminal still holds the Run. A plain read tells the two apart: when
    /// it succeeds, the acknowledgement named a batch that is no longer
    /// current, which consumes nothing, as the contract requires.
    fn acknowledge(&self, delivery: &ExternalRef) -> Result<Option<Delivery>, MailboxError> {
        let args = wire::Args::command(&["orchestration", "check"])
            .value("terminal", self.config().coordinator.as_str())
            .value("run", self.config().run.as_str())
            .value("ack", delivery.as_str())
            .json();
        match self.delivery(args).map_err(|error| mailbox_failure(&error)) {
            Err(MailboxError::Fenced) => self.next_delivery(),
            result @ (Ok(_) | Err(MailboxError::Unavailable(_))) => result,
        }
    }

    /// Wait up to `wait` (at most [`MAX_MAILBOX_WAIT`]) for a question,
    /// report, or escalation, and return the oldest unacknowledged batch.
    ///
    /// Orca returns the whole batch even when it woke for one type, so the
    /// batch can hold heartbeats; use [`Delivery::actionable`] and keep
    /// waiting when it is empty. `None` means the wait ended with nothing
    /// new, which is a checkpoint, never evidence that a worker stopped.
    fn await_delivery(&self, wait: Duration) -> Result<Option<Delivery>, MailboxError> {
        let wait = wait.min(MAX_MAILBOX_WAIT);
        let millis = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        let args = wire::Args::command(&["orchestration", "check"])
            .value("terminal", self.config().coordinator.as_str())
            .value("run", self.config().run.as_str())
            .switch("wait")
            .value("types", "worker_done,escalation,question")
            .value("timeout-ms", &millis.to_string())
            .json();
        let deadline = wait.saturating_add(self.config().call_timeout);
        self.delivery_within(args, deadline)
            .map_err(|error| mailbox_failure(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_is_bounded() {
        assert_eq!(bounded_text(None), None);
        assert_eq!(bounded_text(Some("")), None);
        assert_eq!(bounded_text(Some("\0")), None);
        let long = "é".repeat(MAX_TEXT_BYTES);
        let text = bounded_text(Some(&long));
        assert!(text.is_some_and(|text| text.as_str().len() <= MAX_TEXT_BYTES));
    }

    #[test]
    fn unknown_wire_values_are_not_promoted() {
        assert_eq!(liveness("stale"), Liveness::Unverifiable);
        assert_eq!(terminal(Some("new_state")), TerminalAccounting::Unknown);
        assert_eq!(terminal(None), TerminalAccounting::Unknown);
        assert_eq!(message_kind("mystery"), MessageKind::Other);
    }
}
