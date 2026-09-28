//! Validated schedule definitions.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId,
    contracts::{ResourceRef, Text},
};

/// A rejected schedule value. Input text is never echoed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// A cron expression must have five fields of digits and `* , / -`.
    #[error("invalid cron expression")]
    Cron,
    /// Hours run 0–23 and minutes 0–59.
    #[error("invalid time of day")]
    TimeOfDay,
    /// Time zones are `UTC` or IANA `Area/Location` names.
    #[error("invalid time zone")]
    Timezone,
    /// A precheck needs a bounded timeout.
    #[error("precheck timeout must be between 1 and {max_seconds} seconds")]
    PrecheckTimeout {
        /// Longest accepted timeout.
        max_seconds: u64,
    },
    /// A precheck needs a program and a bounded number of arguments.
    #[error("precheck must have 1 to {max} arguments")]
    PrecheckArgs {
        /// Most accepted arguments, including the program.
        max: usize,
    },
    /// The missed-run grace window is bounded.
    #[error("missed-run grace must be at most {max} minutes")]
    Grace {
        /// Longest accepted grace window.
        max: u16,
    },
    /// Only a run in an existing workspace can reuse a previous session.
    #[error("session reuse requires an existing workspace")]
    ReuseNeedsExistingWorkspace,
    /// Workflow names are 1–64 ASCII letters, digits, `-` or `_`, starting
    /// with a letter or digit.
    #[error("invalid workflow name")]
    WorkflowName,
}

/// The Kitchen workflow a schedule runs, such as `pickup` or `gardener`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkflowName(String);

impl WorkflowName {
    /// Validate a workflow name.
    ///
    /// # Errors
    /// Returns [`ScheduleError::WorkflowName`] for empty, oversized, or
    /// non-identifier input.
    pub fn new(value: &str) -> Result<Self, ScheduleError> {
        let valid = (1..=64).contains(&value.len())
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(ScheduleError::WorkflowName)
        }
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WorkflowName {
    type Error = ScheduleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<WorkflowName> for String {
    fn from(name: WorkflowName) -> Self {
        name.0
    }
}

/// A wall-clock time used by daily and weekly triggers, written `HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TimeOfDay {
    hour: u8,
    minute: u8,
}

impl TimeOfDay {
    /// Validate an hour (0–23) and minute (0–59).
    ///
    /// # Errors
    /// Returns [`ScheduleError::TimeOfDay`] outside those ranges.
    pub const fn new(hour: u8, minute: u8) -> Result<Self, ScheduleError> {
        if hour > 23 || minute > 59 {
            return Err(ScheduleError::TimeOfDay);
        }
        Ok(Self { hour, minute })
    }

    /// The hour, 0–23.
    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// The minute, 0–59.
    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }
}

impl TryFrom<String> for TimeOfDay {
    type Error = ScheduleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let (hour, minute) = value.split_once(':').ok_or(ScheduleError::TimeOfDay)?;
        if hour.len() != 2 || minute.len() != 2 {
            return Err(ScheduleError::TimeOfDay);
        }
        let hour = hour.parse().map_err(|_| ScheduleError::TimeOfDay)?;
        let minute = minute.parse().map_err(|_| ScheduleError::TimeOfDay)?;
        Self::new(hour, minute)
    }
}

impl From<TimeOfDay> for String {
    fn from(time: TimeOfDay) -> Self {
        time.to_string()
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// A day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    /// Sunday.
    Sunday,
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
}

impl Weekday {
    /// The day number with Sunday as 0, as cron uses it.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Sunday => 0,
            Self::Monday => 1,
            Self::Tuesday => 2,
            Self::Wednesday => 3,
            Self::Thursday => 4,
            Self::Friday => 5,
            Self::Saturday => 6,
        }
    }
}

/// Longest accepted cron expression in bytes.
const MAX_CRON_BYTES: usize = 128;

/// A five-field cron expression using only digits and `* , / -`, with
/// fields separated by exactly one space.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CronExpr(String);

impl TryFrom<String> for CronExpr {
    type Error = ScheduleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<CronExpr> for String {
    fn from(expr: CronExpr) -> Self {
        expr.0
    }
}

impl CronExpr {
    /// Validate a five-field cron expression.
    ///
    /// # Errors
    /// Returns [`ScheduleError::Cron`] for another field count, empty or
    /// oversized input, or characters outside digits and `* , / -`.
    pub fn new(value: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = value.split(' ').collect();
        let valid = value.len() <= MAX_CRON_BYTES
            && fields.len() == 5
            && fields.iter().all(|field| {
                !field.is_empty()
                    && field.bytes().all(|byte| {
                        byte.is_ascii_digit() || matches!(byte, b'*' | b',' | b'/' | b'-')
                    })
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(ScheduleError::Cron)
        }
    }

    /// Borrow the expression.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// When a schedule fires.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "at", rename_all = "kebab-case")]
pub enum Recurrence {
    /// Every hour.
    Hourly,
    /// Every day at a time.
    Daily(TimeOfDay),
    /// Monday to Friday at a time.
    Weekdays(TimeOfDay),
    /// One day a week at a time.
    Weekly(Weekday, TimeOfDay),
    /// A cron expression.
    Cron(CronExpr),
}

/// Longest accepted time zone name in bytes.
const MAX_TIMEZONE_BYTES: usize = 64;

/// A time zone: `UTC` or an IANA `Area/Location` name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timezone(String);

impl TryFrom<String> for Timezone {
    type Error = ScheduleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<Timezone> for String {
    fn from(zone: Timezone) -> Self {
        zone.0
    }
}

impl Timezone {
    /// Validate a time zone name. Existence is checked by the backend.
    ///
    /// # Errors
    /// Returns [`ScheduleError::Timezone`] for malformed names.
    pub fn new(value: &str) -> Result<Self, ScheduleError> {
        let segment_valid = |segment: &str| {
            segment
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'-'))
        };
        let valid = value.len() <= MAX_TIMEZONE_BYTES
            && (value == "UTC" || (value.contains('/') && value.split('/').all(segment_valid)));
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(ScheduleError::Timezone)
        }
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The agent family a scheduled run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentFamily {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl AgentFamily {
    /// The stable lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// A precheck's bounded run time (1 second to 5 minutes), stored in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct PrecheckTimeout(Duration);

impl TryFrom<u64> for PrecheckTimeout {
    type Error = ScheduleError;

    fn try_from(millis: u64) -> Result<Self, Self::Error> {
        Self::new(Duration::from_millis(millis))
    }
}

impl From<PrecheckTimeout> for u64 {
    fn from(timeout: PrecheckTimeout) -> Self {
        Self::try_from(timeout.0.as_millis()).unwrap_or(Self::MAX)
    }
}

impl PrecheckTimeout {
    /// Longest accepted precheck timeout.
    pub const MAX: Duration = Duration::from_secs(300);

    /// Validate a precheck timeout.
    ///
    /// # Errors
    /// Returns [`ScheduleError::PrecheckTimeout`] below one second or above [`Self::MAX`].
    pub fn new(duration: Duration) -> Result<Self, ScheduleError> {
        if duration < Duration::from_secs(1) || duration > Self::MAX {
            return Err(ScheduleError::PrecheckTimeout {
                max_seconds: Self::MAX.as_secs(),
            });
        }
        Ok(Self(duration))
    }

    /// Whole seconds, rounded up so the bound is never shortened.
    #[must_use]
    pub fn whole_seconds(self) -> u64 {
        let seconds = self.0.as_secs();
        if self.0.subsec_nanos() > 0 {
            seconds.saturating_add(1)
        } else {
            seconds
        }
    }
}

/// Most arguments a precheck may have, including the program.
pub const MAX_PRECHECK_ARGS: usize = 32;

/// A command run before each scheduled run, as an argument vector.
///
/// Kitchen prechecks report through their exit status; see [`PrecheckOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawPrecheck", into = "RawPrecheck")]
pub struct Precheck {
    argv: Vec<Text>,
    timeout: PrecheckTimeout,
}

impl Precheck {
    /// Build a precheck from a program and its arguments.
    ///
    /// # Errors
    /// Returns [`ScheduleError::PrecheckArgs`] for an empty or oversized vector.
    pub fn new(argv: Vec<Text>, timeout: PrecheckTimeout) -> Result<Self, ScheduleError> {
        if argv.is_empty() || argv.len() > MAX_PRECHECK_ARGS {
            return Err(ScheduleError::PrecheckArgs {
                max: MAX_PRECHECK_ARGS,
            });
        }
        Ok(Self { argv, timeout })
    }

    /// The program and its arguments.
    #[must_use]
    pub fn argv(&self) -> &[Text] {
        &self.argv
    }

    /// The run-time bound.
    #[must_use]
    pub const fn timeout(&self) -> PrecheckTimeout {
        self.timeout
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPrecheck {
    argv: Vec<Text>,
    timeout: PrecheckTimeout,
}

impl TryFrom<RawPrecheck> for Precheck {
    type Error = ScheduleError;

    fn try_from(raw: RawPrecheck) -> Result<Self, Self::Error> {
        Self::new(raw.argv, raw.timeout)
    }
}

impl From<Precheck> for RawPrecheck {
    fn from(precheck: Precheck) -> Self {
        Self {
            argv: precheck.argv,
            timeout: precheck.timeout,
        }
    }
}

/// A precheck's typed result, decoded from its exit status.
///
/// Exit 0 means there is work, exit 1 means there is none, and anything else,
/// including a signal or timeout, is an error that must be reported rather
/// than treated as idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrecheckOutcome {
    /// There is work to do.
    Actionable,
    /// Nothing to do.
    Idle,
    /// The precheck itself failed.
    Error,
}

impl PrecheckOutcome {
    /// Decode an exit code; `None` means the process ended without one.
    #[must_use]
    pub const fn from_exit_code(code: Option<i32>) -> Self {
        match code {
            Some(0) => Self::Actionable,
            Some(1) => Self::Idle,
            Some(_) | None => Self::Error,
        }
    }
}

/// The largest missed-run grace window in minutes (one day).
const MAX_GRACE_MINUTES: u16 = 24 * 60;

/// How long after a missed fire time a run may still start.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(try_from = "u16", into = "u16")]
pub struct GraceMinutes(u16);

impl TryFrom<u16> for GraceMinutes {
    type Error = ScheduleError;

    fn try_from(minutes: u16) -> Result<Self, Self::Error> {
        Self::new(minutes)
    }
}

impl From<GraceMinutes> for u16 {
    fn from(grace: GraceMinutes) -> Self {
        grace.0
    }
}

impl GraceMinutes {
    /// Validate a grace window of at most one day.
    ///
    /// # Errors
    /// Returns [`ScheduleError::Grace`] above 1440 minutes.
    pub const fn new(minutes: u16) -> Result<Self, ScheduleError> {
        if minutes > MAX_GRACE_MINUTES {
            return Err(ScheduleError::Grace {
                max: MAX_GRACE_MINUTES,
            });
        }
        Ok(Self(minutes))
    }

    /// The window in minutes.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Where each scheduled run works.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "resource", rename_all = "kebab-case")]
pub enum ScheduleWorkspace {
    /// A fresh workspace per run.
    NewPerRun,
    /// One existing workspace for every run.
    Existing(ResourceRef),
}

/// Whether a schedule fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleState {
    /// Installed but not firing.
    Paused,
    /// Firing on its trigger.
    Active,
}

/// A portable recurring workflow definition.
///
/// It names the workflow it runs and the consumer scope that workflow claims
/// through Kitchen's single-consumer lease; at most one schedule serves a
/// consumer. Backends install every schedule paused; activation is a
/// separate step with its own authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawScheduleSpec", into = "RawScheduleSpec")]
pub struct ScheduleSpec {
    workflow: WorkflowName,
    consumer: ConsumerId,
    recurrence: Recurrence,
    timezone: Timezone,
    prompt: Text,
    agent: AgentFamily,
    precheck: Option<Precheck>,
    workspace: ScheduleWorkspace,
    missed_run_grace: GraceMinutes,
    reuse_session: bool,
}

impl ScheduleSpec {
    /// A schedule with no precheck, a fresh workspace per run, no missed-run
    /// grace, and no session reuse.
    #[must_use]
    pub const fn new(
        workflow: WorkflowName,
        consumer: ConsumerId,
        recurrence: Recurrence,
        timezone: Timezone,
        prompt: Text,
        agent: AgentFamily,
    ) -> Self {
        Self {
            workflow,
            consumer,
            recurrence,
            timezone,
            prompt,
            agent,
            precheck: None,
            workspace: ScheduleWorkspace::NewPerRun,
            missed_run_grace: GraceMinutes(0),
            reuse_session: false,
        }
    }

    /// Run `precheck` before each run.
    #[must_use]
    pub fn with_precheck(mut self, precheck: Precheck) -> Self {
        self.precheck = Some(precheck);
        self
    }

    /// Run in `workspace`. Switching to a fresh workspace per run also
    /// disables session reuse.
    #[must_use]
    pub fn with_workspace(mut self, workspace: ScheduleWorkspace) -> Self {
        if workspace == ScheduleWorkspace::NewPerRun {
            self.reuse_session = false;
        }
        self.workspace = workspace;
        self
    }

    /// Allow a missed run to start within `grace`.
    #[must_use]
    pub const fn with_missed_run_grace(mut self, grace: GraceMinutes) -> Self {
        self.missed_run_grace = grace;
        self
    }

    /// Submit later runs to the previous live session.
    ///
    /// # Errors
    /// Returns [`ScheduleError::ReuseNeedsExistingWorkspace`] unless the
    /// workspace is [`ScheduleWorkspace::Existing`].
    pub fn with_session_reuse(mut self) -> Result<Self, ScheduleError> {
        if self.workspace == ScheduleWorkspace::NewPerRun {
            return Err(ScheduleError::ReuseNeedsExistingWorkspace);
        }
        self.reuse_session = true;
        Ok(self)
    }

    /// The workflow each run executes.
    #[must_use]
    pub const fn workflow(&self) -> &WorkflowName {
        &self.workflow
    }

    /// The consumer scope the workflow claims; at most one schedule serves it.
    #[must_use]
    pub const fn consumer(&self) -> &ConsumerId {
        &self.consumer
    }

    /// When it fires.
    #[must_use]
    pub const fn recurrence(&self) -> &Recurrence {
        &self.recurrence
    }

    /// The time zone for the trigger.
    #[must_use]
    pub const fn timezone(&self) -> &Timezone {
        &self.timezone
    }

    /// The brief each run receives.
    #[must_use]
    pub const fn prompt(&self) -> &Text {
        &self.prompt
    }

    /// The agent family each run starts.
    #[must_use]
    pub const fn agent(&self) -> AgentFamily {
        self.agent
    }

    /// The precheck, if any.
    #[must_use]
    pub const fn precheck(&self) -> Option<&Precheck> {
        self.precheck.as_ref()
    }

    /// Where runs work.
    #[must_use]
    pub const fn workspace(&self) -> &ScheduleWorkspace {
        &self.workspace
    }

    /// The missed-run grace window.
    #[must_use]
    pub const fn missed_run_grace(&self) -> GraceMinutes {
        self.missed_run_grace
    }

    /// Whether later runs reuse the previous live session.
    #[must_use]
    pub const fn reuse_session(&self) -> bool {
        self.reuse_session
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawScheduleSpec {
    workflow: WorkflowName,
    consumer: ConsumerId,
    recurrence: Recurrence,
    timezone: Timezone,
    prompt: Text,
    agent: AgentFamily,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    precheck: Option<Precheck>,
    workspace: ScheduleWorkspace,
    missed_run_grace: GraceMinutes,
    reuse_session: bool,
}

impl TryFrom<RawScheduleSpec> for ScheduleSpec {
    type Error = ScheduleError;

    fn try_from(raw: RawScheduleSpec) -> Result<Self, Self::Error> {
        let spec = Self {
            workflow: raw.workflow,
            consumer: raw.consumer,
            recurrence: raw.recurrence,
            timezone: raw.timezone,
            prompt: raw.prompt,
            agent: raw.agent,
            precheck: raw.precheck,
            workspace: raw.workspace,
            missed_run_grace: raw.missed_run_grace,
            reuse_session: false,
        };
        if raw.reuse_session {
            spec.with_session_reuse()
        } else {
            Ok(spec)
        }
    }
}

impl From<ScheduleSpec> for RawScheduleSpec {
    fn from(spec: ScheduleSpec) -> Self {
        Self {
            workflow: spec.workflow,
            consumer: spec.consumer,
            recurrence: spec.recurrence,
            timezone: spec.timezone,
            prompt: spec.prompt,
            agent: spec.agent,
            precheck: spec.precheck,
            workspace: spec.workspace,
            missed_run_grace: spec.missed_run_grace,
            reuse_session: spec.reuse_session,
        }
    }
}
