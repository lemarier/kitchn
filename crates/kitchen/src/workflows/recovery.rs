//! Backend-neutral recovery evidence for one worker, and the classification
//! of environment failures during validation.
//!
//! A backend adapter reads what it can about a worker (for Orca, its
//! signal reader in `adapters::orca`) and maps it to [`RecoverySignals`].
//! Supervision acts only on positive evidence: every field has an explicit
//! "cannot tell", and silence is never promoted to a stall.

use crate::contracts::{ExternalRef, ResourceRef, Text, Timestamp};

/// Whether the agent behind a launch began its first turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartEvidence {
    /// The agent began a turn.
    TurnObserved,
    /// The backend read the worker's output and it holds nothing from the
    /// agent. Missing evidence: alone it proves nothing.
    NoTurn,
    /// Not enough to say.
    Unknown,
}

/// What the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptState {
    /// Working a turn.
    Working,
    /// Sitting at its prompt: a finished turn or one that never started.
    Idle,
    /// Parked on a prompt only a person can answer. Healthy, not stalled.
    AwaitingHuman,
    /// Not reported or not recognized.
    Unknown,
}

/// Who holds the worker's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalHolder {
    /// The supervised agent.
    Agent,
    /// A person took it over; it is theirs.
    Person,
    /// Not reported or not recognized.
    Unknown,
}

/// A provider refusal that stops the agent until the provider works again.
/// Retrying the task does not help, so it never counts as a task failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderInterruption {
    /// The provider rejected the credentials, such as after an account switch.
    Auth,
    /// The account's allowance is used up.
    Quota,
    /// The provider throttled requests.
    RateLimit,
}

impl ProviderInterruption {
    /// A stable lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::RateLimit => "rate-limit",
        }
    }
}

/// How far the agent's transcript has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptProgress {
    /// Whether the backend returned the whole transcript. A truncated one
    /// may hide an earlier agent message, so its silence proves nothing.
    pub complete: bool,
    /// Whether any message came from the agent or its tools.
    pub agent_spoke: bool,
    /// When the newest message was written; progress is this advancing.
    pub last_activity: Option<Timestamp>,
}

/// Recovery evidence about one worker, read by a backend adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverySignals {
    /// The observed worker. Signals for another worker are ignored.
    pub worker: ResourceRef,
    /// Whether the agent began work.
    pub start: StartEvidence,
    /// What the agent is doing.
    pub prompt: PromptState,
    /// The agent's transcript, when the backend has a proven one. `None` is
    /// absence, not an empty transcript.
    pub transcript: Option<TranscriptProgress>,
    /// Who holds the terminal.
    pub terminal: TerminalHolder,
    /// A provider refusal classified from the provider's own error output.
    pub provider: Option<ProviderInterruption>,
}

impl RecoverySignals {
    /// Positive proof that the first turn never started: the whole transcript
    /// was read and the agent never spoke, the backend saw no turn, and the agent
    /// sits idle at its prompt in a terminal known to be the agent's. Anything
    /// less keeps waiting.
    #[must_use]
    pub fn proves_never_started(&self) -> bool {
        self.terminal == TerminalHolder::Agent
            && self.start == StartEvidence::NoTurn
            && self.prompt == PromptState::Idle
            && self
                .transcript
                .is_some_and(|transcript| transcript.complete && !transcript.agent_spoke)
    }

    /// When the agent last made progress, if it now sits idle at its prompt
    /// with a readable transcript. `None` means stall cannot be established,
    /// including when nobody identified the terminal's holder: a terminal
    /// not known to be the agent's is never stopped as stalled.
    #[must_use]
    pub fn idle_since(&self) -> Option<Timestamp> {
        if self.prompt != PromptState::Idle
            || self.provider.is_some()
            || self.terminal == TerminalHolder::Unknown
        {
            return None;
        }
        self.transcript?.last_activity
    }
}

/// Whether the provider was checked and works again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderCheck {
    /// No positive evidence the provider works.
    #[default]
    NotChecked,
    /// The provider answered a probe with the house's current credentials.
    Working,
}

/// An environment fault that broke validation, as opposed to a failing test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvironmentFault {
    /// No space left on the device (`ENOSPC`) or a disk quota (`EDQUOT`).
    NoSpace,
    /// Memory could not be allocated (`ENOMEM`).
    OutOfMemory,
    /// The process ran out of file descriptors (`EMFILE`, `ENFILE`).
    TooManyOpenFiles,
    /// The file system is read-only (`EROFS`).
    ReadOnlyFileSystem,
}

/// Most bytes of validation output scanned, taken from the end.
pub const MAX_VALIDATION_SCAN_BYTES: usize = 16 * 1024;

impl EnvironmentFault {
    /// Classify validation output. Only the last
    /// [`MAX_VALIDATION_SCAN_BYTES`] are scanned for the operating system's
    /// own error names and messages; output without one is not an
    /// environment fault.
    #[must_use]
    pub fn classify(output: &str) -> Option<Self> {
        let mut start = output.len().saturating_sub(MAX_VALIDATION_SCAN_BYTES);
        while !output.is_char_boundary(start) {
            start = start.saturating_add(1);
        }
        let tail = output.get(start..).unwrap_or_default();
        const PATTERNS: [(&str, EnvironmentFault); 11] = [
            ("ENOSPC", EnvironmentFault::NoSpace),
            ("No space left on device", EnvironmentFault::NoSpace),
            ("EDQUOT", EnvironmentFault::NoSpace),
            ("Disk quota exceeded", EnvironmentFault::NoSpace),
            ("ENOMEM", EnvironmentFault::OutOfMemory),
            ("Cannot allocate memory", EnvironmentFault::OutOfMemory),
            ("EMFILE", EnvironmentFault::TooManyOpenFiles),
            ("ENFILE", EnvironmentFault::TooManyOpenFiles),
            ("Too many open files", EnvironmentFault::TooManyOpenFiles),
            ("EROFS", EnvironmentFault::ReadOnlyFileSystem),
            (
                "Read-only file system",
                EnvironmentFault::ReadOnlyFileSystem,
            ),
        ];
        PATTERNS
            .iter()
            .find(|(pattern, _)| tail.contains(pattern))
            .map(|(_, fault)| *fault)
    }
}

/// Why a validation run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationFailure {
    /// A check or test failed on the code.
    Tests,
    /// The environment broke the run; the code was not judged.
    Environment(EnvironmentFault),
}

impl ValidationFailure {
    /// Classify a failed validation run from its output: an environment
    /// fault when [`EnvironmentFault::classify`] finds one, otherwise a test
    /// failure.
    #[must_use]
    pub fn classify(output: &str) -> Self {
        EnvironmentFault::classify(output).map_or(Self::Tests, Self::Environment)
    }
}

/// A failed validation run the worker reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationReport {
    /// Why it failed.
    pub failure: ValidationFailure,
    /// When the run finished. A retry counts only runs after it was sent.
    pub finished_at: Timestamp,
}

/// A request the coordinator sent a worker during an attempt, such as a
/// review follow-up. The worker's completion must address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    /// The coordinator's own id for the request.
    pub id: ExternalRef,
    /// The request text.
    pub body: Text,
}

/// A follow-up recorded for a task and not yet addressed by a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedFollowUp {
    /// The id the worker lists in its report once it addressed the request.
    pub id: ExternalRef,
    /// The request text.
    pub body: Text,
}
