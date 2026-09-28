//! Typed recovery observations for one worker.
//!
//! A coordinator that has to recover without a person must tell apart a launch
//! that never took, an agent waiting at its prompt, a terminal a person now
//! holds, a provider that refuses the agent, and a Dispatch that no longer
//! accepts messages. [`OrcaBackend::observe_signals`] reads these from one
//! `worker-show` and one bounded `worker-read`.
//!
//! Each signal is evidence with an explicit "cannot tell". Absence is never
//! promoted to a fact: an unrecognized or missing value reads as unknown, and
//! nothing here settles, stops, retries, or releases a worker. Deciding what to
//! do is the coordinator's job; Orca's own rule stands that only positive
//! proof (an `exited` liveness, a settled report) authorizes acting.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::{
    adapters::orca::{
        OrcaBackend, OrcaError, OrcaRunner,
        backend::{ProjectionStage, TerminalResource, WorkerShow},
        inspect::liveness,
        wire,
    },
    contracts::{Liveness, ResourceRef, Timestamp},
};

/// Transcript messages or terminal lines read for one observation. The count
/// in [`TranscriptProgress`] is a lower bound past this.
pub const SIGNAL_WINDOW_ROWS: usize = 50;

/// Most bytes of provider output scanned for an error, taken from the end.
const MAX_SCAN_BYTES: usize = 8 * 1024;

/// Newest transcript messages scanned for a provider error.
const SCANNED_MESSAGES: usize = 3;

/// Whether the agent behind a launch began work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartOutcome {
    /// Orca accepted the launch input, the agent has shown no turn yet, and
    /// the start window is still open.
    Accepted,
    /// The agent began a turn: Orca's agent status reports work, a wait, or a
    /// finished turn, the transcript holds an agent message, or the agent
    /// reported an outcome.
    TurnObserved,
    /// No turn was seen. Orca recorded the launch as failed before the agent
    /// was ready, or it accepted the input and the start window closed with
    /// nothing from the agent (the shape of a prompt that swallowed the
    /// launch). This is missing evidence, not proof the agent is gone: read
    /// [`WorkerSignals::liveness`] before acting.
    NeverObserved,
    /// Not enough to say: still starting, stopped before it showed a turn, or
    /// a stage Kitchen does not recognize.
    Unknown,
}

/// The time a launch has to show a first agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartWindow {
    launched_at: Timestamp,
    now: Timestamp,
    deadline: Duration,
}

impl StartWindow {
    /// A window opened when the launch was recorded at `launched_at`, judged
    /// at `now`. Kitchen's own clock supplies both, so the decision is
    /// repeatable.
    #[must_use]
    pub const fn new(launched_at: Timestamp, now: Timestamp, deadline: Duration) -> Self {
        Self {
            launched_at,
            now,
            deadline,
        }
    }

    fn closed(&self) -> bool {
        self.now.saturating_since(self.launched_at) >= self.deadline
    }
}

/// What the agent is doing, from Orca's agent status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentPrompt {
    /// The agent is working a turn.
    Working,
    /// The agent finished its turn or has not started one, and sits at its
    /// prompt. Without a `worker_done` report this is the stalled-agent
    /// shape, not completion.
    AtPrompt,
    /// The agent is parked on a prompt only a person can answer, such as a
    /// permission request. A waiting worker is healthy, not failed.
    AwaitingHuman,
    /// Orca reports nothing, or something Kitchen does not recognize.
    Unknown,
}

/// Who holds the worker's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalOwner {
    /// Orca's supervised worker owns it.
    Supervised,
    /// A person took it over. It is theirs: Kitchen sends it no work without
    /// asking.
    Person,
    /// Orca did not create the terminal.
    External,
    /// The terminal was released.
    Released,
    /// Not reported or not recognized.
    Unknown,
}

/// Whether the Dispatch still takes part in coordination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatchActivity {
    /// The Dispatch is this Run's live attempt: dispatched and not fenced.
    Active,
    /// The Dispatch completed, failed, or was fenced. Orca refuses a message
    /// to it with `dispatch_inactive`, which the adapter reports as not
    /// applied.
    Ended,
    /// The Dispatch belongs to another Run. The adapter acts on none of them.
    OutsideRun,
    /// Not reported, transitional, or not recognized.
    Unknown,
}

/// The kind of failure a provider reported. Only the class is kept: the text
/// it came from can carry secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorClass {
    /// The provider rejected the credentials: a missing, invalid, or expired
    /// login or key.
    Auth,
    /// The account's allowance is used up: a usage limit, a spending limit,
    /// or exhausted credit. Retrying does not help until it resets.
    Quota,
    /// The provider throttled requests. Retrying later can help.
    RateLimit,
    /// The provider reported an API failure of another kind.
    Other,
}

/// How far the agent's transcript has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptProgress {
    /// Messages in the newest window Orca returned.
    pub messages: usize,
    /// Whether Orca returned the whole transcript, so `messages` is exact.
    /// Otherwise it is a lower bound: the window is [`SIGNAL_WINDOW_ROWS`] messages.
    pub complete: bool,
    /// When the newest message was written. Progress between two
    /// observations is this advancing.
    pub last_activity: Option<Timestamp>,
    /// Whether any message came from the agent or its tools, as opposed to
    /// the prompt sender.
    pub agent_spoke: bool,
}

/// The recovery signals for one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSignals {
    /// The observed worker.
    pub worker: ResourceRef,
    /// Whether the Dispatch takes part in coordination.
    pub dispatch: DispatchActivity,
    /// Orca's liveness verdict. Only `Exited` (or `Live`) is a positive
    /// fact; `Unverifiable` is absence.
    pub liveness: Liveness,
    /// Whether the agent began work.
    pub start: StartOutcome,
    /// What the agent is doing.
    pub prompt: AgentPrompt,
    /// The transcript's progress, when Orca has a proven provider transcript.
    /// `None` is absence: terminal output is not a transcript.
    pub transcript: Option<TranscriptProgress>,
    /// Who holds the terminal.
    pub terminal: TerminalOwner,
    /// A provider failure seen in the worker's start error, terminal, or
    /// newest agent messages. A heuristic over text: pair it with
    /// [`Self::prompt`] and [`Self::start`].
    pub provider_error: Option<ProviderErrorClass>,
}

impl WorkerSignals {
    /// Whether a message to the worker would be accepted: the Dispatch is
    /// active and no person holds the terminal. This is the adapter's own
    /// rule, so a `false` here means the adapter would send nothing.
    #[must_use]
    pub fn accepts_messages(&self) -> bool {
        self.dispatch == DispatchActivity::Active && self.terminal != TerminalOwner::Person
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkerRead {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    content_complete: Option<bool>,
    #[serde(default)]
    transcript: Option<Transcript>,
    #[serde(default)]
    terminal: Option<TerminalTail>,
}

#[derive(Deserialize)]
struct Transcript {
    #[serde(default)]
    messages: Vec<TranscriptMessage>,
    #[serde(default)]
    limited: Option<bool>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    blocks: Vec<Block>,
    #[serde(default)]
    timestamp: Option<u64>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TerminalTail {
    #[serde(default)]
    tail: Vec<Value>,
}

impl WorkerRead {
    /// Progress of a proven provider transcript; terminal output is not one.
    fn progress(&self) -> Option<TranscriptProgress> {
        if self.source.as_deref() != Some("transcript") {
            return None;
        }
        let transcript = self.transcript.as_ref()?;
        Some(TranscriptProgress {
            messages: transcript.messages.len(),
            complete: self.content_complete == Some(true) && transcript.limited != Some(true),
            last_activity: transcript
                .messages
                .iter()
                .filter_map(|message| message.timestamp)
                .max()
                .map(Timestamp::from_unix_millis),
            agent_spoke: transcript
                .messages
                .iter()
                .any(|message| matches!(message.role.as_deref(), Some("assistant" | "tool"))),
        })
    }

    /// Output that can carry a provider error: the terminal tail, or the text
    /// the agent wrote in its newest messages. Tool output and the prompt
    /// sender's text are left out, since they quote errors that are not the
    /// provider's.
    fn provider_text(&self) -> Vec<&str> {
        let mut texts = Vec::new();
        if let Some(terminal) = &self.terminal {
            texts.extend(terminal.tail.iter().filter_map(Value::as_str));
        }
        if let Some(transcript) = &self.transcript {
            let start = transcript.messages.len().saturating_sub(SCANNED_MESSAGES);
            for message in transcript.messages.iter().skip(start) {
                if !matches!(message.role.as_deref(), Some("assistant" | "system")) {
                    continue;
                }
                texts.extend(
                    message
                        .blocks
                        .iter()
                        .filter(|block| block.kind.as_deref() == Some("text"))
                        .filter_map(|block| block.text.as_deref()),
                );
            }
        }
        texts
    }
}

/// The last `max` bytes of `text`, cut at a character boundary.
fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    text.get(start..).unwrap_or_default()
}

/// The text of an Orca error value: a string, or the strings of an object.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .values()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => String::new(),
    }
}

/// Phrases that name a provider's own failure. Matched on lowercased text.
const QUOTA: &[&str] = &[
    "insufficient_quota",
    "exceeded your current quota",
    "quota exceeded",
    "credit balance",
    "out of credits",
    "usage limit",
    "billing hard limit",
];
const AUTH: &[&str] = &[
    "authentication_error",
    "invalid api key",
    "invalid x-api-key",
    "please run /login",
    "not logged in",
    "token has expired",
    "oauth token",
    "login expired",
];
const RATE_LIMIT: &[&str] = &["rate_limit_error", "rate limit", "too many requests"];
/// Weaker phrases that count only beside a provider error frame, because
/// ordinary tool output prints them too.
const AUTH_WEAK: &[&str] = &["unauthorized", "forbidden"];
const AUTH_STATUS: &[&str] = &["401", "403"];
const RATE_LIMIT_STATUS: &[&str] = &["429"];
const QUOTA_WEAK: &[&str] = &["limit reached", "limit will reset"];
/// Markers that the text reports a provider or API failure.
const FRAMES: &[&str] = &[
    "api error",
    "api_error",
    "stream error",
    "unexpected status",
    "status code",
    "http error",
    "request failed",
    "\"type\":\"error\"",
    "overloaded_error",
];

fn any(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| text.contains(phrase))
}

/// Whether `text` holds `status` as a number of its own, not inside a longer one.
fn has_status(text: &str, status: &str) -> bool {
    text.match_indices(status).any(|(at, _)| {
        let before = text.get(..at).and_then(|head| head.chars().next_back());
        let after = text
            .get(at + status.len()..)
            .and_then(|rest| rest.chars().next());
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Classify a provider failure in `text`, or `None` when it reports none.
///
/// Only the class leaves this function. Specific provider phrases decide on
/// their own; status codes and generic words such as `unauthorized` count only
/// beside an API error frame, since tool output prints them for other
/// reasons. A quota phrase wins over a throttle, because providers report an
/// exhausted quota with the throttle status.
pub(crate) fn classify_provider_error(text: &str) -> Option<ProviderErrorClass> {
    let lower = tail(text, MAX_SCAN_BYTES).to_ascii_lowercase();
    let framed = any(&lower, FRAMES);
    if any(&lower, QUOTA) {
        return Some(ProviderErrorClass::Quota);
    }
    if any(&lower, AUTH)
        || (framed && (any(&lower, AUTH_WEAK) || AUTH_STATUS.iter().any(|s| has_status(&lower, s))))
    {
        return Some(ProviderErrorClass::Auth);
    }
    if any(&lower, RATE_LIMIT)
        || (framed && RATE_LIMIT_STATUS.iter().any(|s| has_status(&lower, s)))
    {
        return Some(ProviderErrorClass::RateLimit);
    }
    if any(&lower, QUOTA_WEAK) && framed {
        return Some(ProviderErrorClass::Quota);
    }
    framed.then_some(ProviderErrorClass::Other)
}

/// What the agent is doing, from Orca's activity and its wait probe.
///
/// A recorded wait comes first: it names a worker parked on a prompt only a
/// person can answer, with the evidence that proved it.
pub(crate) fn prompt_of(activity: Option<&str>, waiting: bool) -> AgentPrompt {
    if waiting {
        return AgentPrompt::AwaitingHuman;
    }
    match activity {
        Some("working") => AgentPrompt::Working,
        Some("done" | "idle") => AgentPrompt::AtPrompt,
        Some("blocked" | "waiting") => AgentPrompt::AwaitingHuman,
        Some(_) | None => AgentPrompt::Unknown,
    }
}

/// Whether Orca's activity shows the agent took a turn: working, blocked,
/// waiting, or done. `idle` and `unknown` are not.
fn turn_shown(activity: Option<&str>) -> bool {
    matches!(activity, Some("working" | "blocked" | "waiting" | "done"))
}

/// Whether the agent began work.
///
/// A turn seen anywhere is `TurnObserved`. Without one, `NeverObserved` needs
/// positive evidence: Orca recorded a start-stage failure, or the input was
/// accepted and the window closed. Kitchen's own stop leaves it unknown.
pub(crate) fn start_outcome(
    stage: Option<&ProjectionStage>,
    agent_spoke: bool,
    window: &StartWindow,
) -> StartOutcome {
    let Some(stage) = stage else {
        return if agent_spoke {
            StartOutcome::TurnObserved
        } else {
            StartOutcome::Unknown
        };
    };
    if agent_spoke || turn_shown(stage.activity.as_deref()) {
        return StartOutcome::TurnObserved;
    }
    match (
        stage.worker.as_deref(),
        stage.dispatch.as_deref(),
        stage.detail.as_deref(),
    ) {
        // The agent reported an outcome, so it ran.
        (Some("succeeded"), _, _) | (Some("failed"), _, Some("settled")) => {
            StartOutcome::TurnObserved
        }
        // Orca recorded the launch as failed at a start stage, before the
        // agent was ready.
        (Some("failed"), Some("failed"), Some(detail)) if detail != "process_stopped" => {
            StartOutcome::NeverObserved
        }
        (_, _, Some("input_accepted")) if window.closed() => StartOutcome::NeverObserved,
        (_, _, Some("input_accepted")) => StartOutcome::Accepted,
        _ => StartOutcome::Unknown,
    }
}

/// Whether the Dispatch takes part in coordination in `run`.
pub(crate) fn dispatch_activity(shown: &WorkerShow, run: &str) -> DispatchActivity {
    let Some(dispatch) = &shown.dispatch else {
        return DispatchActivity::Unknown;
    };
    match dispatch.run_id.as_deref() {
        None => return DispatchActivity::Unknown,
        Some(found) if found != run => return DispatchActivity::OutsideRun,
        Some(_) => {}
    }
    let fenced = dispatch
        .capability_revoked_at
        .as_ref()
        .is_some_and(|at| !at.is_null());
    match (
        dispatch.status.as_deref(),
        shown.worker.state.as_str(),
        fenced,
    ) {
        (_, _, true)
        | (Some("completed" | "failed"), _, _)
        | (_, "stopped" | "failed" | "succeeded", _) => DispatchActivity::Ended,
        (Some("dispatched"), "starting" | "ready", false) => DispatchActivity::Active,
        _ => DispatchActivity::Unknown,
    }
}

/// Who holds the terminal.
pub(crate) fn terminal_owner(resource: Option<&TerminalResource>) -> TerminalOwner {
    let Some(resource) = resource else {
        return TerminalOwner::Unknown;
    };
    if resource.person_owns() {
        return TerminalOwner::Person;
    }
    match resource.ownership_state.as_deref() {
        Some("owned") => TerminalOwner::Supervised,
        Some("external") => TerminalOwner::External,
        Some("released") => TerminalOwner::Released,
        Some(_) | None => TerminalOwner::Unknown,
    }
}

/// A provider failure in the start error, the terminal preview, or the
/// newest output, in that order.
///
/// Each source is classified on its own, so a long output cannot push Orca's
/// recorded start error out of the scanned window.
fn provider_error(shown: &WorkerShow, read: Option<&WorkerRead>) -> Option<ProviderErrorClass> {
    let last_error = shown.worker.last_error.as_ref().map(value_text);
    let preview = shown
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.preview.clone());
    // Lines can split one error across entries, so classify the whole output.
    let output = read.map(|read| read.provider_text().join("\n"));
    [last_error, preview, output]
        .into_iter()
        .flatten()
        .find_map(|text| classify_provider_error(&text))
}

impl<R: OrcaRunner> OrcaBackend<R> {
    /// Read the recovery signals for `worker`.
    ///
    /// Makes two read-only calls: `worker-show`, then a bounded `worker-read`
    /// of the newest [`SIGNAL_WINDOW_ROWS`] transcript messages (or terminal lines when
    /// Orca has no proven transcript). A `worker-read` Orca refuses leaves
    /// [`WorkerSignals::transcript`] empty; the other signals still hold.
    /// `Ok(None)` means Orca has no record of the worker.
    ///
    /// # Errors
    /// Call and parse failures of `worker-show`, and non-refusal failures of
    /// `worker-read`.
    pub fn observe_signals(
        &self,
        worker: &ResourceRef,
        window: &StartWindow,
    ) -> Result<Option<WorkerSignals>, OrcaError> {
        let Some(dispatch) = self.dispatch_of(worker) else {
            return Ok(None);
        };
        let Some(shown) = self.show(dispatch)? else {
            return Ok(None);
        };
        let read = self.read_output(dispatch)?;
        let transcript = read.as_ref().and_then(WorkerRead::progress);
        let stage = shown.projection.stage.as_ref();
        let agent_spoke = transcript.is_some_and(|progress| progress.agent_spoke);
        let waiting = shown
            .observation
            .as_ref()
            .and_then(|observation| observation.agent_wait.as_ref())
            .is_some_and(|wait| !wait.is_null());
        Ok(Some(WorkerSignals {
            worker: worker.clone(),
            dispatch: dispatch_activity(&shown, self.config().run.as_str()),
            liveness: liveness(&shown.projection.liveness.verdict),
            start: start_outcome(stage, agent_spoke, window),
            prompt: prompt_of(stage.and_then(|stage| stage.activity.as_deref()), waiting),
            transcript,
            terminal: terminal_owner(shown.terminal_resource.as_ref()),
            provider_error: provider_error(&shown, read.as_ref()),
        }))
    }

    /// The newest output of `dispatch`: its proven transcript, or the
    /// terminal tail when there is none. A refusal reads as no output.
    fn read_output(&self, dispatch: &str) -> Result<Option<WorkerRead>, OrcaError> {
        let args = wire::Args::command(&["orchestration", "worker-read"])
            .value("dispatch", dispatch)
            .value("source", "auto")
            .value("limit", &SIGNAL_WINDOW_ROWS.to_string())
            .json();
        match self.call(args, self.config().call_timeout) {
            Ok(value) => wire::typed(value, "worker read").map(Some),
            Err(OrcaError::Refused { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn at(millis: u64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    /// Launched at 0, judged at `now` ms, with a 60 s window.
    fn window(now: u64) -> StartWindow {
        StartWindow::new(at(0), at(now), Duration::from_secs(60))
    }

    fn stage(
        worker: &str,
        dispatch: &str,
        detail: Option<&str>,
        activity: &str,
    ) -> ProjectionStage {
        ProjectionStage {
            worker: Some(worker.to_owned()),
            dispatch: Some(dispatch.to_owned()),
            detail: detail.map(str::to_owned),
            activity: Some(activity.to_owned()),
        }
    }

    fn shown(dispatch: &Value, worker_state: &str) -> TestResult<WorkerShow> {
        Ok(serde_json::from_value(json!({
            "dispatch": dispatch,
            "worker": {"state": worker_state},
            "projection": {"outcome": "in_progress", "liveness": {"verdict": "live"}},
        }))?)
    }

    fn resource(value: &Value) -> TestResult<TerminalResource> {
        Ok(serde_json::from_value(value.clone())?)
    }

    #[test]
    fn provider_failures_are_classified() {
        let cases = [
            (
                "API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\"}}",
                ProviderErrorClass::Auth,
            ),
            (
                "Invalid API key · Please run /login",
                ProviderErrorClass::Auth,
            ),
            (
                "OAuth token has expired. Please run /login",
                ProviderErrorClass::Auth,
            ),
            (
                "stream error: unexpected status 401 Unauthorized",
                ProviderErrorClass::Auth,
            ),
            ("API Error: 403 Forbidden", ProviderErrorClass::Auth),
            (
                "Claude usage limit reached. Your limit will reset at 3pm",
                ProviderErrorClass::Quota,
            ),
            ("You've hit your usage limit.", ProviderErrorClass::Quota),
            (
                // Providers report an exhausted quota with the throttle status.
                "stream error: 429 Too Many Requests {\"code\":\"insufficient_quota\"}",
                ProviderErrorClass::Quota,
            ),
            (
                "API Error: Credit balance is too low",
                ProviderErrorClass::Quota,
            ),
            (
                "API Error: 5-hour limit reached ∙ resets 3pm",
                ProviderErrorClass::Quota,
            ),
            (
                "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
                ProviderErrorClass::RateLimit,
            ),
            (
                "stream error: unexpected status 429",
                ProviderErrorClass::RateLimit,
            ),
            ("429 Too Many Requests", ProviderErrorClass::RateLimit),
            (
                "Rate limit reached for requests, retrying in 20s",
                ProviderErrorClass::RateLimit,
            ),
            (
                "API Error: 500 internal server error",
                ProviderErrorClass::Other,
            ),
            ("API Error: overloaded_error", ProviderErrorClass::Other),
        ];
        for (text, expected) in cases {
            assert_eq!(classify_provider_error(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn ordinary_output_is_not_a_provider_failure() {
        for text in [
            "",
            "All tests passed. 429 rows imported.",
            "test login_returns_401_unauthorized ... ok",
            "Compiling kitchen v0.1.0 (14290 files)",
            "error[E0432]: unresolved import",
            "the limit reached its default of 10",
        ] {
            assert_eq!(classify_provider_error(text), None, "{text}");
        }
        // A status inside a longer number is not that status.
        assert_eq!(
            classify_provider_error("API error: request of 14290 tokens failed"),
            Some(ProviderErrorClass::Other)
        );
    }

    #[test]
    fn only_the_newest_output_is_scanned_and_cuts_respect_characters() {
        let filler = "é".repeat(MAX_SCAN_BYTES);
        // An old failure scrolled out of the window is not current.
        assert_eq!(
            classify_provider_error(&format!("You've hit your usage limit.\n{filler}")),
            None
        );
        assert_eq!(
            classify_provider_error(&format!("{filler}\nYou've hit your usage limit.")),
            Some(ProviderErrorClass::Quota)
        );
    }

    #[test]
    fn a_first_turn_is_seen_or_missing_evidence_stays_missing() {
        let accepted =
            |activity: &str| stage("ready", "dispatched", Some("input_accepted"), activity);
        let cases = [
            // Within the window, nothing from the agent yet.
            (accepted("unknown"), false, 59_999, StartOutcome::Accepted),
            // The window closes at its deadline, not after it.
            (
                accepted("unknown"),
                false,
                60_000,
                StartOutcome::NeverObserved,
            ),
            // An idle prompt is not a turn.
            (accepted("idle"), false, 90_000, StartOutcome::NeverObserved),
            // Any agent status that shows a turn wins over a closed window.
            (
                accepted("working"),
                false,
                90_000,
                StartOutcome::TurnObserved,
            ),
            (
                accepted("blocked"),
                false,
                90_000,
                StartOutcome::TurnObserved,
            ),
            (
                accepted("waiting"),
                false,
                90_000,
                StartOutcome::TurnObserved,
            ),
            (accepted("done"), false, 90_000, StartOutcome::TurnObserved),
            // So does an agent message in the transcript.
            (
                accepted("unknown"),
                true,
                90_000,
                StartOutcome::TurnObserved,
            ),
            // Orca recorded a start-stage failure.
            (
                stage("failed", "failed", Some("agent_readiness"), "unknown"),
                false,
                1,
                StartOutcome::NeverObserved,
            ),
            // An agent's own report means it ran.
            (
                stage("failed", "failed", Some("settled"), "unknown"),
                false,
                1,
                StartOutcome::TurnObserved,
            ),
            (
                stage("succeeded", "completed", Some("settled"), "unknown"),
                false,
                1,
                StartOutcome::TurnObserved,
            ),
            // Kitchen's own stop says nothing about whether a turn happened.
            (
                stage("stopped", "failed", Some("process_stopped"), "unknown"),
                false,
                90_000,
                StartOutcome::Unknown,
            ),
            (
                stage("failed", "failed", Some("process_stopped"), "unknown"),
                false,
                90_000,
                StartOutcome::Unknown,
            ),
            // A failure the Dispatch does not confirm, and stages Kitchen does
            // not know, stay unknown.
            (
                stage("failed", "dispatched", Some("agent_readiness"), "unknown"),
                false,
                1,
                StartOutcome::Unknown,
            ),
            (
                stage("starting", "dispatched", None, "unknown"),
                false,
                90_000,
                StartOutcome::Unknown,
            ),
            (
                stage("ready", "dispatched", Some("something_new"), "unknown"),
                false,
                90_000,
                StartOutcome::Unknown,
            ),
        ];
        for (index, (stage, spoke, now, expected)) in cases.iter().enumerate() {
            assert_eq!(
                start_outcome(Some(stage), *spoke, &window(*now)),
                *expected,
                "case {index}"
            );
        }
        // An older host reports no stage: only the transcript can show a turn.
        assert_eq!(
            start_outcome(None, false, &window(90_000)),
            StartOutcome::Unknown
        );
        assert_eq!(
            start_outcome(None, true, &window(90_000)),
            StartOutcome::TurnObserved
        );
        // A launch judged before it was recorded has no elapsed time.
        let early = StartWindow::new(at(10_000), at(0), Duration::ZERO);
        assert!(early.closed(), "a zero deadline is already closed");
        let early = StartWindow::new(at(10_000), at(0), Duration::from_secs(1));
        assert!(!early.closed());
    }

    #[test]
    fn the_prompt_reads_from_the_status_and_a_recorded_wait() {
        let cases = [
            (Some("working"), false, AgentPrompt::Working),
            (Some("done"), false, AgentPrompt::AtPrompt),
            (Some("idle"), false, AgentPrompt::AtPrompt),
            (Some("blocked"), false, AgentPrompt::AwaitingHuman),
            (Some("waiting"), false, AgentPrompt::AwaitingHuman),
            // A recorded wait wins over a stale status.
            (Some("working"), true, AgentPrompt::AwaitingHuman),
            (Some("unknown"), false, AgentPrompt::Unknown),
            (Some("something_new"), false, AgentPrompt::Unknown),
            (None, false, AgentPrompt::Unknown),
        ];
        for (activity, waiting, expected) in cases {
            assert_eq!(
                prompt_of(activity, waiting),
                expected,
                "{activity:?} {waiting}"
            );
        }
    }

    #[test]
    fn a_dispatch_is_active_only_while_it_is_the_live_attempt() -> TestResult {
        let live = json!({"runId": "run_sim", "status": "dispatched", "capabilityRevokedAt": null});
        let cases = [
            (live.clone(), "ready", DispatchActivity::Active),
            (live.clone(), "starting", DispatchActivity::Active),
            (
                json!({"runId": "run_sim", "status": "completed"}),
                "succeeded",
                DispatchActivity::Ended,
            ),
            (
                json!({"runId": "run_sim", "status": "failed"}),
                "failed",
                DispatchActivity::Ended,
            ),
            (live.clone(), "stopped", DispatchActivity::Ended),
            // A fenced Dispatch refuses messages whatever else it reports.
            (
                json!({"runId": "run_sim", "status": "dispatched",
                       "capabilityRevokedAt": "2026-09-28T16:00:00Z"}),
                "ready",
                DispatchActivity::Ended,
            ),
            (
                json!({"runId": "run_other", "status": "dispatched"}),
                "ready",
                DispatchActivity::OutsideRun,
            ),
            (
                json!({"status": "dispatched"}),
                "ready",
                DispatchActivity::Unknown,
            ),
            (
                json!({"runId": "run_sim", "status": "paused"}),
                "ready",
                DispatchActivity::Unknown,
            ),
            (live, "stopping", DispatchActivity::Unknown),
        ];
        for (index, (dispatch, worker, expected)) in cases.iter().enumerate() {
            assert_eq!(
                dispatch_activity(&shown(dispatch, worker)?, "run_sim"),
                *expected,
                "case {index}"
            );
        }
        let no_dispatch: WorkerShow = serde_json::from_value(json!({
            "worker": {"state": "ready"},
            "projection": {"outcome": "in_progress", "liveness": {"verdict": "live"}},
        }))?;
        assert_eq!(
            dispatch_activity(&no_dispatch, "run_sim"),
            DispatchActivity::Unknown
        );
        Ok(())
    }

    #[test]
    fn a_terminal_a_person_holds_is_theirs() -> TestResult {
        let cases = [
            (
                json!({"ownershipState": "owned"}),
                TerminalOwner::Supervised,
            ),
            (
                json!({"ownershipState": "user_owned"}),
                TerminalOwner::Person,
            ),
            (
                json!({"ownershipState": "owned", "retainedReason": "user_takeover"}),
                TerminalOwner::Person,
            ),
            (
                json!({"ownershipState": "external"}),
                TerminalOwner::External,
            ),
            (
                json!({"ownershipState": "released"}),
                TerminalOwner::Released,
            ),
            (
                json!({"ownershipState": "something_new"}),
                TerminalOwner::Unknown,
            ),
            (json!({}), TerminalOwner::Unknown),
        ];
        for (wire, expected) in cases {
            assert_eq!(terminal_owner(Some(&resource(&wire)?)), expected, "{wire}");
        }
        assert_eq!(terminal_owner(None), TerminalOwner::Unknown);
        Ok(())
    }

    #[test]
    fn messages_are_accepted_by_an_active_dispatch_nobody_took_over() -> TestResult {
        let signals = |dispatch, terminal| -> TestResult<WorkerSignals> {
            Ok(WorkerSignals {
                worker: ResourceRef {
                    kind: crate::contracts::ResourceKind::Worker,
                    backend: crate::BackendId::new("orca-local")?,
                    handle: crate::contracts::ExternalRef::new("ctx_1")?,
                },
                dispatch,
                liveness: Liveness::Live,
                start: StartOutcome::TurnObserved,
                prompt: AgentPrompt::Working,
                transcript: None,
                terminal,
                provider_error: None,
            })
        };
        for (dispatch, terminal, expected) in [
            (DispatchActivity::Active, TerminalOwner::Supervised, true),
            (DispatchActivity::Active, TerminalOwner::External, true),
            (DispatchActivity::Active, TerminalOwner::Person, false),
            (DispatchActivity::Ended, TerminalOwner::Supervised, false),
            (
                DispatchActivity::OutsideRun,
                TerminalOwner::Supervised,
                false,
            ),
            (DispatchActivity::Unknown, TerminalOwner::Supervised, false),
        ] {
            assert_eq!(
                signals(dispatch, terminal)?.accepts_messages(),
                expected,
                "{dispatch:?} {terminal:?}"
            );
        }
        Ok(())
    }

    fn read(value: &Value) -> TestResult<WorkerRead> {
        Ok(serde_json::from_value(value.clone())?)
    }

    #[test]
    fn transcript_progress_counts_the_window_and_dates_the_newest_message() -> TestResult {
        let whole = read(&json!({
            "source": "transcript",
            "contentComplete": true,
            "transcript": {"limited": false, "messages": [
                {"role": "user", "timestamp": 1_000, "blocks": []},
                {"role": "assistant", "timestamp": 3_000, "blocks": []},
                {"role": "tool", "timestamp": 2_000, "blocks": []},
            ]},
        }))?;
        assert_eq!(
            whole.progress(),
            Some(TranscriptProgress {
                messages: 3,
                complete: true,
                last_activity: Some(at(3_000)),
                agent_spoke: true,
            })
        );
        // A clipped window is a lower bound.
        let clipped = read(&json!({
            "source": "transcript",
            "contentComplete": false,
            "transcript": {"limited": true, "messages": [
                {"role": "user", "timestamp": 1_000, "blocks": []},
            ]},
        }))?;
        assert_eq!(
            clipped.progress(),
            Some(TranscriptProgress {
                messages: 1,
                complete: false,
                last_activity: Some(at(1_000)),
                agent_spoke: false,
            })
        );
        // Nothing said about completeness is not completeness.
        let silent = read(&json!({"source": "transcript", "transcript": {"messages": []}}))?;
        assert_eq!(
            silent.progress(),
            Some(TranscriptProgress {
                messages: 0,
                complete: false,
                last_activity: None,
                agent_spoke: false,
            })
        );
        Ok(())
    }

    #[test]
    fn terminal_output_is_not_a_transcript() -> TestResult {
        let terminal = read(&json!({
            "source": "terminal",
            "terminal": {"tail": ["> hello", "API Error: 429 rate_limit_error"]},
        }))?;
        assert_eq!(terminal.progress(), None);
        // It can still show a provider failure.
        assert_eq!(
            classify_provider_error(&terminal.provider_text().join("\n")),
            Some(ProviderErrorClass::RateLimit)
        );
        Ok(())
    }

    #[test]
    fn only_the_newest_agent_text_is_read_for_provider_failures() -> TestResult {
        let messages = read(&json!({
            "source": "transcript",
            "transcript": {"messages": [
                {"role": "assistant", "blocks": [{"type": "text", "text": "old: usage limit"}]},
                {"role": "assistant", "blocks": [{"type": "text", "text": "second"}]},
                {"role": "user", "blocks": [{"type": "text", "text": "user: 429"}]},
                {"role": "tool", "blocks": [{"type": "text", "text": "tool: 401 Unauthorized"}]},
                {"role": "assistant", "blocks": [
                    {"type": "tool-call", "text": "call: rate limit"},
                    {"type": "text", "text": "newest"},
                ]},
            ]},
        }))?;
        // The three newest messages are the user's, the tool's, and the
        // assistant's; only the assistant's text block counts.
        assert_eq!(messages.provider_text(), vec!["newest"]);
        Ok(())
    }
}
