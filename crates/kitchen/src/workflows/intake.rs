//! Report intake: turn reports from outside the forge into deduplicated issue
//! proposals for the gardener.
//!
//! A house declares each intake source with a credential reference, a read
//! scope, and a privacy class. Reports from undeclared sources, or from
//! channels outside a source's read scope, are refused before grouping.
//! Reports are grouped by the problem an agent classified them under; a
//! problem that already has an open issue gains a count and source links, and
//! a new problem becomes a draft issue proposal. Intake grants no posting:
//! every mutation needs the task's scoped forge grant, and the effect store
//! checks it again on submission.
//!
//! Report text and reporter identities stay in memory within one house. They
//! reach public issue text only when the source's [`PrivacyClass`] allows it.
//! Concrete connectors belong to house configuration; this module defines
//! only the [`ReportConnector`] boundary.
//!
//! Which reports are already counted is durable house state. Before a
//! proposal's mutation is submitted, [`IntakeLedger::reserve`] records a
//! reservation marker holding the task, the effect name, the problem, and
//! digests of the reports; it never holds report ids or text. A reservation
//! counts once its effect is applied, so the effect-outcome transaction is
//! the confirmation and a crash between the two cannot lose a count. Each
//! effect name is a digest of the exact mutation and report set, so a
//! replanned batch never reuses a name for a different mutation.
//!
//! Counted digests are kept in a bounded window. Compaction folds settled
//! reservations into at most [`MAX_COUNTED_CHUNKS`] counted markers per
//! repository, each at most [`crate::state::MAX_MARKER_PAYLOAD_BYTES`]. By
//! encoded size, one marker holds up to about 115 digests, so the window
//! keeps roughly the 1,400 to 3,600 most recently folded reports, depending
//! on batch sizes and problem keys; 32 full batches of [`MAX_PER_PROPOSAL`]
//! fill it exactly. When the markers are full, compaction evicts the oldest
//! chunk and raises the repository's forgotten-through time to the newest
//! `received_at` among the evicted reports. [`plan`] never proposes a report
//! received at or before that time. So a report is proposed only when it is
//! newer than every forgotten report, and every counted report that new is
//! still in the window: dedupe is exact, and a report older than the window
//! is dropped rather than counted twice.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    num::NonZeroU32,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    BackendId, CredentialId, EffectName, HouseId, TaskId, WorkflowId,
    contracts::{
        Claimant, ContractError, ExternalRef, GitHubAction, GitHubMutation, GrantScope,
        HouseGrants, IssueNumber, Permission, Repository, TaskAuthority, Text, Timestamp,
    },
    integrations::github::IssueState,
    state::{
        EffectState, HouseStore, MarkerAttempt, MarkerFact, MarkerKey, MarkerSchema, MarkerSubject,
        MarkerWrite, TaskRecord, TaskState, WorkItem, WorkflowMarker,
    },
};

/// Most sources one house may declare.
pub const MAX_SOURCES: usize = 32;
/// Most channels in one source's read scope.
pub const MAX_CHANNELS: usize = 32;
/// Most reports one fetch or plan accepts. A larger batch is refused, never
/// truncated.
pub const MAX_BATCH: usize = 500;
/// Most reports listed individually in one issue body or comment; the rest
/// are counted.
pub const MAX_LISTED: usize = 50;
/// Longest public quote of report text, in bytes.
pub const MAX_QUOTE_BYTES: usize = 500;
/// Longest source or problem key, in bytes.
pub const MAX_KEY_BYTES: usize = 48;
/// Most new reports one posting proposal carries, so that its reservation
/// fits in one house-store marker. Further new reports for the problem stay
/// uncounted and are proposed by a later plan that includes them.
pub const MAX_PER_PROPOSAL: usize = 100;

const RESERVATION_SCHEMA: &str = "intake.reservation";
const COUNTED_SCHEMA: &str = "intake.counted";
const FORGOTTEN_SCHEMA: &str = "intake.forgotten";
const CHUNK_PREFIX: &str = "counted-";
const FORGOTTEN_SUBJECT: &str = "forgotten";
/// Counted-digest markers kept per repository: the dedupe window. Once they
/// are full, compaction evicts the oldest to fold newer reservations.
pub const MAX_COUNTED_CHUNKS: usize = 32;
/// Digest bytes kept in report digests and effect names (128 bits).
const DIGEST_BYTES: usize = 16;

const MARKER_PREFIX: &str = "<!-- kitchen-intake:";
const MARKER_SUFFIX: &str = " -->";

/// An intake failure. Report text, reporter identities, and credential names
/// are never included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IntakeError {
    /// A source declaration is malformed, duplicated, or over its bounds.
    #[error("invalid intake source declaration")]
    InvalidDeclaration,
    /// The house has not declared the named source.
    #[error("undeclared intake source")]
    UndeclaredSource,
    /// A report came from a channel outside its source's read scope.
    #[error("report outside the source read scope")]
    OutOfScope,
    /// A report or intake input belongs to another house.
    #[error("intake input from another house")]
    CrossHouse,
    /// A report or batch is malformed or over its bounds.
    #[error("invalid intake report")]
    InvalidReport,
    /// One report was classified under two different problems.
    #[error("conflicting report classification")]
    ConflictingClassification,
    /// Issue evidence is incomplete, such as an unknown issue state.
    #[error("incomplete intake evidence")]
    IncompleteEvidence,
    /// The task's posting authority does not match this plan or house.
    #[error("intake posting authority refused")]
    Authority,
    /// The source refused the house credential.
    #[error("intake source refused the credential")]
    SourceUnauthorized,
    /// The source could not be read; this is never an empty batch.
    #[error("intake source unavailable")]
    SourceUnavailable,
    /// Another task's intake effect for this repository is not yet
    /// resolved, so its reports may or may not be counted.
    #[error("another intake effect is unresolved")]
    Unreconciled,
    /// Intake state changed since it was read; read it and plan again.
    #[error("intake state changed since it was read")]
    StaleCount,
}

impl IntakeError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(self) -> crate::ErrorClass {
        match self {
            Self::InvalidDeclaration
            | Self::InvalidReport
            | Self::ConflictingClassification
            | Self::IncompleteEvidence => crate::ErrorClass::InvalidInput,
            Self::UndeclaredSource
            | Self::OutOfScope
            | Self::CrossHouse
            | Self::Authority
            | Self::SourceUnauthorized => crate::ErrorClass::Refused,
            Self::SourceUnavailable => crate::ErrorClass::Execution,
            Self::Unreconciled | Self::StaleCount => crate::ErrorClass::Conflict,
        }
    }
}

fn valid_key(value: &str) -> bool {
    (1..=MAX_KEY_BYTES).contains(&value.len())
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

macro_rules! intake_key {
    ($name:ident, $error:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate a key: 1–48 bytes of lowercase ASCII letters, digits,
            /// and hyphens, not starting with a hyphen.
            ///
            /// # Errors
            /// Refuses other input without echoing it.
            pub fn new(value: &str) -> Result<Self, IntakeError> {
                if valid_key(value) {
                    Ok(Self(value.to_owned()))
                } else {
                    Err($error)
                }
            }

            /// Borrow the key.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IntakeError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(&value)
            }
        }

        impl From<$name> for String {
            fn from(key: $name) -> Self {
                key.0
            }
        }
    };
}

intake_key!(
    SourceId,
    IntakeError::InvalidDeclaration,
    "A house-chosen intake source name, such as `support-inbox`. It appears in public issue text, so it must not identify a reporter."
);
intake_key!(
    ProblemKey,
    IntakeError::InvalidReport,
    "The problem a report was classified under, such as `login-timeout`. It appears in public issue text and markers."
);

/// What a source's reports may reveal in public issue text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyClass {
    /// Reports are public: link, reporter, and a bounded quote may be shown.
    Public,
    /// Only the report link may be shown; reporter and text stay private.
    LinkOnly,
    /// Only the source name and a count may be shown.
    Private,
}

impl PrivacyClass {
    const fn shows_link(self) -> bool {
        match self {
            Self::Public | Self::LinkOnly => true,
            Self::Private => false,
        }
    }

    const fn shows_content(self) -> bool {
        match self {
            Self::Public => true,
            Self::LinkOnly | Self::Private => false,
        }
    }
}

/// The channels a source's credential may read, such as mailboxes or chat
/// rooms named in the source's own terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadScope {
    channels: BTreeSet<ExternalRef>,
}

impl ReadScope {
    /// A scope of 1–32 distinct channels.
    ///
    /// # Errors
    /// Refuses an empty or oversized scope.
    pub fn new(channels: impl IntoIterator<Item = ExternalRef>) -> Result<Self, IntakeError> {
        let channels: BTreeSet<_> = channels.into_iter().collect();
        if channels.is_empty() || channels.len() > MAX_CHANNELS {
            return Err(IntakeError::InvalidDeclaration);
        }
        Ok(Self { channels })
    }

    /// Whether `channel` is within this scope.
    #[must_use]
    pub fn contains(&self, channel: &ExternalRef) -> bool {
        self.channels.contains(channel)
    }

    /// The declared channels.
    pub fn channels(&self) -> impl Iterator<Item = &ExternalRef> {
        self.channels.iter()
    }
}

/// One house-declared intake source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeSource {
    /// Source name.
    pub id: SourceId,
    /// House credential the connector reads with; never the secret itself.
    pub credential: CredentialId,
    /// Channels the connector may read.
    pub scope: ReadScope,
    /// What reports may reveal publicly.
    pub privacy: PrivacyClass,
}

/// A house's declared intake sources. Nothing else is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeSources {
    house: HouseId,
    sources: BTreeMap<SourceId, IntakeSource>,
}

impl IntakeSources {
    /// Declare at most 32 sources with distinct names for `house`.
    ///
    /// # Errors
    /// Refuses a duplicated name or too many sources.
    pub fn new(
        house: HouseId,
        sources: impl IntoIterator<Item = IntakeSource>,
    ) -> Result<Self, IntakeError> {
        let mut declared = BTreeMap::new();
        for source in sources {
            if declared.len() == MAX_SOURCES || declared.contains_key(&source.id) {
                return Err(IntakeError::InvalidDeclaration);
            }
            declared.insert(source.id.clone(), source);
        }
        Ok(Self {
            house,
            sources: declared,
        })
    }

    /// The declaring house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// The declaration for `id`.
    ///
    /// # Errors
    /// Returns [`IntakeError::UndeclaredSource`] for any other name.
    pub fn get(&self, id: &SourceId) -> Result<&IntakeSource, IntakeError> {
        self.sources.get(id).ok_or(IntakeError::UndeclaredSource)
    }

    /// Accept one delivered report from `source`, checking its declaration
    /// and read scope.
    ///
    /// # Errors
    /// Refuses an undeclared source or a channel outside its read scope.
    pub fn accept(&self, source: &SourceId, raw: RawReport) -> Result<Report, IntakeError> {
        let declared = self.get(source)?;
        if !declared.scope.contains(&raw.channel) {
            return Err(IntakeError::OutOfScope);
        }
        Ok(Report {
            house: self.house.clone(),
            source: source.clone(),
            privacy: declared.privacy,
            raw,
        })
    }

    /// Read at most `limit` reports from a declared source through
    /// `connector`. The declaration is checked before the connector is
    /// called, so an undeclared source never reaches a credential.
    ///
    /// # Errors
    /// Refuses an undeclared source, a limit outside 1–500, a connector
    /// failure, a batch larger than `limit`, and any report outside the
    /// source's read scope. A partial batch is never returned.
    pub fn collect(
        &self,
        source: &SourceId,
        connector: &impl ReportConnector,
        limit: usize,
    ) -> Result<Vec<Report>, IntakeError> {
        let declared = self.get(source)?;
        if !(1..=MAX_BATCH).contains(&limit) {
            return Err(IntakeError::InvalidReport);
        }
        let request = FetchRequest {
            house: &self.house,
            source,
            credential: &declared.credential,
            scope: &declared.scope,
            limit,
        };
        let batch = connector.fetch(&request).map_err(|failure| match failure {
            ConnectorFailure::Unauthorized => IntakeError::SourceUnauthorized,
            ConnectorFailure::Unavailable => IntakeError::SourceUnavailable,
            ConnectorFailure::Malformed => IntakeError::InvalidReport,
        })?;
        if batch.len() > limit {
            return Err(IntakeError::InvalidReport);
        }
        batch
            .into_iter()
            .map(|raw| self.accept(source, raw))
            .collect()
    }
}

/// What a connector is allowed to read for one fetch.
#[derive(Debug, Clone, Copy)]
pub struct FetchRequest<'a> {
    /// The house whose credential is used.
    pub house: &'a HouseId,
    /// The declared source.
    pub source: &'a SourceId,
    /// Credential reference resolved by the house's private provider.
    pub credential: &'a CredentialId,
    /// Channels the connector may read.
    pub scope: &'a ReadScope,
    /// Most reports to return.
    pub limit: usize,
}

/// Why a connector could not return a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorFailure {
    /// The service refused the credential.
    Unauthorized,
    /// The service could not be reached or timed out.
    Unavailable,
    /// The service answered with data the connector could not parse.
    Malformed,
}

/// A house-configured reader for one kind of external report source.
///
/// Implementations resolve the credential reference privately, read only the
/// requested channels, stop at `limit`, and bound their own I/O time. An
/// empty batch must mean the source had nothing new, never a failed read.
pub trait ReportConnector {
    /// Read new reports for `request`.
    ///
    /// # Errors
    /// Returns a [`ConnectorFailure`] instead of a partial or empty batch.
    fn fetch(&self, request: &FetchRequest<'_>) -> Result<Vec<RawReport>, ConnectorFailure>;
}

/// A link to a report, restricted to `https://` without Markdown-breaking
/// characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportLink(ExternalRef);

impl ReportLink {
    /// Validate an `https://` link of at most 256 printable ASCII bytes.
    ///
    /// # Errors
    /// Refuses other schemes and `<`, `>`, `` ` ``, `(`, `)`, `[`, or `]`.
    pub fn new(value: &str) -> Result<Self, IntakeError> {
        let rest = value
            .strip_prefix("https://")
            .ok_or(IntakeError::InvalidReport)?;
        if rest.is_empty()
            || value
                .bytes()
                .any(|byte| matches!(byte, b'<' | b'>' | b'`' | b'(' | b')' | b'[' | b']'))
        {
            return Err(IntakeError::InvalidReport);
        }
        ExternalRef::new(value)
            .map(Self)
            .map_err(|_| IntakeError::InvalidReport)
    }

    /// The link.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// One report as a connector read it. Its `Debug` output hides the reporter
/// and text.
#[derive(Clone, PartialEq, Eq)]
pub struct RawReport {
    /// Source-native message identity, stable across redelivery.
    pub id: ExternalRef,
    /// Channel within the source's read scope.
    pub channel: ExternalRef,
    /// Link to the report, when the source has one.
    pub link: Option<ReportLink>,
    /// Reporter handle in the source's own terms.
    pub reporter: ExternalRef,
    /// Report text; untrusted data.
    pub text: Text,
    /// When the source received it.
    pub received_at: Timestamp,
}

impl fmt::Debug for RawReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawReport")
            .field("id", &self.id)
            .field("channel", &self.channel)
            .field("reporter", &"[private]")
            .field("text", &self.text)
            .field("received_at", &self.received_at)
            .finish_non_exhaustive()
    }
}

/// A report accepted from a declared source. It carries its house and the
/// privacy class in force when it was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    house: HouseId,
    source: SourceId,
    privacy: PrivacyClass,
    raw: RawReport,
}

impl Report {
    /// Owning house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// Declared source.
    #[must_use]
    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    /// Privacy class at acceptance.
    #[must_use]
    pub const fn privacy(&self) -> PrivacyClass {
        self.privacy
    }

    /// The report as read.
    #[must_use]
    pub const fn raw(&self) -> &RawReport {
        &self.raw
    }

    /// Identity used to deduplicate redelivery and recounting.
    #[must_use]
    pub fn key(&self) -> ReportKey {
        ReportKey {
            source: self.source.clone(),
            id: self.raw.id.clone(),
        }
    }
}

/// A report's identity: its source and source-native id. It is never
/// written to public issue text; house state keeps only its
/// [`ReportDigest`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReportKey {
    /// Declared source.
    pub source: SourceId,
    /// Source-native id.
    pub id: ExternalRef,
}

impl ReportKey {
    /// The digest recorded in house state for this report.
    #[must_use]
    pub fn digest(&self) -> ReportDigest {
        let mut hasher = Sha256::new();
        hasher.update(b"kitchen-intake-report/1\0");
        hasher.update(self.source.as_str());
        hasher.update([0]);
        hasher.update(self.id.as_str());
        ReportDigest(hex(&hasher.finalize()))
    }
}

/// The first 128 bits of SHA-256 over a report's source and id, as 32
/// lowercase hex digits. Durable intake state records these instead of
/// report ids, which may identify a reporter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReportDigest(String);

impl ReportDigest {
    /// The digest in hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ReportDigest {
    type Error = IntakeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = value.len() == DIGEST_BYTES * 2
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(value))
        } else {
            Err(IntakeError::IncompleteEvidence)
        }
    }
}

impl From<ReportDigest> for String {
    fn from(digest: ReportDigest) -> Self {
        digest.0
    }
}

/// Lowercase hex of the first [`DIGEST_BYTES`] bytes of `digest`.
fn hex(digest: &[u8]) -> String {
    let mut text = String::with_capacity(DIGEST_BYTES * 2);
    for byte in digest.iter().take(DIGEST_BYTES) {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// A report and the problem it was classified under. Classification is a
/// judgment made before planning; the plan only groups and deduplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// The accepted report.
    pub report: Report,
    /// Its problem.
    pub problem: ProblemKey,
}

/// An issue already tracking a problem, found from forge evidence such as
/// [`problem_marker`]. Which reports it counts comes from [`Counted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownIssue {
    /// Issue number.
    pub number: IssueNumber,
    /// Its lifecycle; unknown is never treated as open or closed.
    pub state: IssueState,
}

/// The reports one repository's intake has counted, read from the house
/// store by [`IntakeLedger::counted`]. It also remembers which reservations
/// it saw, so [`IntakeLedger::reserve`] can refuse a plan made from state
/// that has since changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    house: HouseId,
    repository: Repository,
    problems: BTreeMap<ProblemKey, BTreeSet<ReportDigest>>,
    reports: BTreeSet<ReportDigest>,
    observed: BTreeSet<MarkerKey>,
    forgotten: Option<Timestamp>,
}

impl Counted {
    /// Nothing counted. Use it only to plan without a store; reserving
    /// against it is refused once the store holds any reservation for
    /// `repository`.
    #[must_use]
    pub const fn none(house: HouseId, repository: Repository) -> Self {
        Self {
            house,
            repository,
            problems: BTreeMap::new(),
            reports: BTreeSet::new(),
            observed: BTreeSet::new(),
            forgotten: None,
        }
    }

    /// Whether `report` is counted under any problem.
    #[must_use]
    pub fn contains(&self, report: &ReportKey) -> bool {
        self.reports.contains(&report.digest())
    }

    /// How many reports within the dedupe window are counted for `problem`.
    /// Once [`Self::forgotten_through`] is set, earlier reports may have
    /// been counted too.
    #[must_use]
    pub fn total(&self, problem: &ProblemKey) -> usize {
        self.problems.get(problem).map_or(0, BTreeSet::len)
    }

    /// The newest `received_at` among reports evicted from the dedupe
    /// window, if any were. [`plan`] proposes no report received at or
    /// before it.
    #[must_use]
    pub const fn forgotten_through(&self) -> Option<Timestamp> {
        self.forgotten
    }

    fn forgets(&self, report: &Report) -> bool {
        self.forgotten
            .is_some_and(|through| report.raw().received_at <= through)
    }

    fn add(&mut self, problem: ProblemKey, reports: Vec<ReportDigest>) {
        self.reports.extend(reports.iter().cloned());
        self.problems.entry(problem).or_default().extend(reports);
    }
}

/// Which posting permissions the task holds for one repository. Intake
/// itself grants none; this is read from the task's delegated forge grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingAuthority {
    repository: Repository,
    create_issue: bool,
    post_comment: bool,
}

impl PostingAuthority {
    /// No posting: every proposal is reported as blocked on its grant.
    #[must_use]
    pub const fn none(repository: Repository) -> Self {
        Self {
            repository,
            create_issue: false,
            post_comment: false,
        }
    }

    /// Read [`Permission::CreateIssue`] and [`Permission::PostComment`] for
    /// `repository` on `destination` from the task's delegated grants,
    /// rechecked against the house's `current` grants.
    ///
    /// # Errors
    /// Refuses another house's grants, grants the house has since revoked,
    /// and ambiguous credentials. A permission the task simply lacks is not
    /// an error.
    pub fn from_task(
        task: &TaskAuthority,
        current: &HouseGrants,
        repository: &Repository,
        destination: &BackendId,
    ) -> Result<Self, IntakeError> {
        let scope = GrantScope::Repository(repository.clone());
        let held = |permission| match task.authorize(current, permission, &scope, destination) {
            Ok(_) => Ok(true),
            Err(ContractError::PermissionDenied { .. }) => Ok(false),
            Err(_) => Err(IntakeError::Authority),
        };
        Ok(Self {
            repository: repository.clone(),
            create_issue: held(Permission::CreateIssue)?,
            post_comment: held(Permission::PostComment)?,
        })
    }
}

/// One planned intake result per problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proposal {
    /// Add a count and source links to the open issue tracking the problem.
    AddReports {
        /// Problem.
        problem: ProblemKey,
        /// Open issue.
        issue: IssueNumber,
        /// Newly counted reports, to record once the comment is confirmed.
        reports: Vec<ReportKey>,
        /// Reports within the dedupe window counted on the issue once these
        /// are recorded.
        total: usize,
        /// The newest time one of `reports` was received.
        received: Timestamp,
        /// The comment to post.
        mutation: GitHubMutation,
    },
    /// Create a draft issue for a new problem, for the gardener to specify.
    DraftIssue {
        /// Problem.
        problem: ProblemKey,
        /// Reports it groups, to record once the issue is confirmed.
        reports: Vec<ReportKey>,
        /// The newest time one of `reports` was received.
        received: Timestamp,
        /// The issue to create.
        mutation: GitHubMutation,
    },
    /// New reports match a closed issue. Nothing is posted; a person decides
    /// whether it regressed or the reports are stale.
    ClosedMatch {
        /// Problem.
        problem: ProblemKey,
        /// Closed issue.
        issue: IssueNumber,
        /// New reports.
        reports: Vec<ReportKey>,
    },
    /// The task lacks the posting permission this proposal needs. Nothing is
    /// posted.
    MissingGrant {
        /// Problem.
        problem: ProblemKey,
        /// The permission the task would need.
        permission: Permission,
        /// Reports waiting for it.
        reports: Vec<ReportKey>,
    },
}

impl Proposal {
    /// The forge mutation to submit, when this proposal has one.
    #[must_use]
    pub const fn mutation(&self) -> Option<&GitHubMutation> {
        match self {
            Self::AddReports { mutation, .. } | Self::DraftIssue { mutation, .. } => Some(mutation),
            Self::ClosedMatch { .. } | Self::MissingGrant { .. } => None,
        }
    }

    /// The name for submitting [`Self::mutation`] as a task effect: a digest
    /// of the exact mutation and report set. A rerun that plans the same
    /// mutation for the same reports reuses the effect, so an uncertain
    /// submission is reconciled instead of posted twice; any other batch
    /// gets another name.
    #[must_use]
    pub fn effect_name(&self) -> Option<EffectName> {
        let (kind, reports, mutation) = match self {
            Self::DraftIssue {
                reports, mutation, ..
            } => ("draft", reports, mutation),
            Self::AddReports {
                reports, mutation, ..
            } => ("comment", reports, mutation),
            Self::ClosedMatch { .. } | Self::MissingGrant { .. } => return None,
        };
        let mut hasher = Sha256::new();
        hasher.update(b"kitchen-intake-effect/1\0");
        hasher.update(serde_json::to_vec(mutation).ok()?);
        let digests: BTreeSet<ReportDigest> = reports.iter().map(ReportKey::digest).collect();
        for digest in &digests {
            hasher.update([0]);
            hasher.update(digest.as_str());
        }
        EffectName::new(&format!("intake-{kind}-{}", hex(&hasher.finalize()))).ok()
    }

    fn reservation(&self, task: &TaskId) -> Option<Reservation> {
        let (problem, reports, received) = match self {
            Self::DraftIssue {
                problem,
                reports,
                received,
                ..
            }
            | Self::AddReports {
                problem,
                reports,
                received,
                ..
            } => (problem, reports, *received),
            Self::ClosedMatch { .. } | Self::MissingGrant { .. } => return None,
        };
        let digests: BTreeSet<ReportDigest> = reports.iter().map(ReportKey::digest).collect();
        Some(Reservation {
            task: task.clone(),
            effect: self.effect_name()?,
            problem: problem.clone(),
            reports: digests.into_iter().collect(),
            received: Some(received),
        })
    }
}

/// Group `reports` by problem and propose one result per problem with new
/// reports. Reports in `counted`, and redelivered copies of one report,
/// count once. Reports received at or before
/// [`Counted::forgotten_through`] are dropped, since they may have been
/// counted before the window moved past them. Problems with nothing new
/// produce nothing. A posting proposal
/// carries at most [`MAX_PER_PROPOSAL`] reports; the rest wait for a later
/// plan.
///
/// # Errors
/// Refuses reports or counted state from another house, authority or
/// counted state for another repository, more than 500 reports, one report
/// classified under two problems, a known issue in an unknown state, a
/// problem with counted reports but no known issue, and output that exceeds
/// forge bounds.
pub fn plan(
    house: &HouseId,
    repository: &Repository,
    reports: &[Classified],
    known: &BTreeMap<ProblemKey, KnownIssue>,
    counted: &Counted,
    authority: &PostingAuthority,
) -> Result<Vec<Proposal>, IntakeError> {
    if &counted.house != house {
        return Err(IntakeError::CrossHouse);
    }
    if &authority.repository != repository || &counted.repository != repository {
        return Err(IntakeError::Authority);
    }
    if reports.len() > MAX_BATCH {
        return Err(IntakeError::InvalidReport);
    }
    let mut problems: BTreeMap<&ProblemKey, BTreeMap<ReportKey, &Report>> = BTreeMap::new();
    let mut classified: BTreeMap<ReportKey, &ProblemKey> = BTreeMap::new();
    for entry in reports {
        if entry.report.house() != house {
            return Err(IntakeError::CrossHouse);
        }
        let key = entry.report.key();
        match classified.get(&key) {
            Some(problem) if *problem != &entry.problem => {
                return Err(IntakeError::ConflictingClassification);
            }
            Some(_) => {}
            None => {
                classified.insert(key.clone(), &entry.problem);
            }
        }
        problems
            .entry(&entry.problem)
            .or_default()
            .insert(key, &entry.report);
    }

    let mut proposals = Vec::new();
    for (problem, mut grouped) in problems {
        grouped.retain(|key, report| !counted.contains(key) && !counted.forgets(report));
        if grouped.is_empty() {
            continue;
        }
        let issue = known.get(problem);
        let posts = match issue.map(|issue| issue.state) {
            // Counted reports mean intake already created or commented on an
            // issue for this problem; the forge evidence is missing it.
            None if counted.total(problem) > 0 => return Err(IntakeError::IncompleteEvidence),
            None => authority.create_issue,
            Some(IssueState::Open) => authority.post_comment,
            Some(IssueState::Closed | IssueState::Unknown) => false,
        };
        if posts && let Some(first_deferred) = grouped.keys().nth(MAX_PER_PROPOSAL).cloned() {
            grouped.split_off(&first_deferred);
        }
        let keys: Vec<ReportKey> = grouped.keys().cloned().collect();
        let listed: Vec<&Report> = grouped.into_values().collect();
        let Some(received) = listed.iter().map(|report| report.raw().received_at).max() else {
            continue;
        };
        let proposal = match issue {
            Some(issue) => match issue.state {
                IssueState::Unknown => return Err(IntakeError::IncompleteEvidence),
                IssueState::Closed => Proposal::ClosedMatch {
                    problem: problem.clone(),
                    issue: issue.number,
                    reports: keys,
                },
                IssueState::Open if authority.post_comment => {
                    let total = counted.total(problem).saturating_add(keys.len());
                    let body =
                        render_comment(problem, total, counted.forgotten.is_some(), &listed)?;
                    Proposal::AddReports {
                        problem: problem.clone(),
                        issue: issue.number,
                        reports: keys,
                        total,
                        received,
                        mutation: checked(
                            repository,
                            GitHubAction::PostComment {
                                issue: issue.number,
                                body,
                            },
                        )?,
                    }
                }
                IssueState::Open => Proposal::MissingGrant {
                    problem: problem.clone(),
                    permission: Permission::PostComment,
                    reports: keys,
                },
            },
            None if authority.create_issue => {
                let title = Text::new(&format!("Intake: {problem}"))
                    .map_err(|_| IntakeError::InvalidReport)?;
                let body = render_draft(problem, &listed)?;
                Proposal::DraftIssue {
                    problem: problem.clone(),
                    reports: keys,
                    received,
                    mutation: checked(repository, GitHubAction::CreateIssue { title, body })?,
                }
            }
            None => Proposal::MissingGrant {
                problem: problem.clone(),
                permission: Permission::CreateIssue,
                reports: keys,
            },
        };
        proposals.push(proposal);
    }
    Ok(proposals)
}

fn checked(repository: &Repository, action: GitHubAction) -> Result<GitHubMutation, IntakeError> {
    let mutation = GitHubMutation {
        repository: repository.clone(),
        action,
    };
    mutation
        .validate()
        .map_err(|_| IntakeError::InvalidReport)?;
    Ok(mutation)
}

/// What a reservation marker records: digests only, never report ids or
/// text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reservation {
    task: TaskId,
    effect: EffectName,
    problem: ProblemKey,
    reports: Vec<ReportDigest>,
    /// The newest `received_at` among the reports. Reservations recorded
    /// before this field existed use their marker's recorded time instead,
    /// which matches only while the source's clock is not ahead of the
    /// house's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    received: Option<Timestamp>,
}

/// Counted digests folded from applied reservations whose tasks settled,
/// keyed by problem, with the newest time one of their reports was
/// received. One marker holds at most
/// [`crate::state::MAX_MARKER_PAYLOAD_BYTES`] of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CountedChunk {
    problems: BTreeMap<ProblemKey, BTreeSet<ReportDigest>>,
    newest: Timestamp,
}

impl CountedChunk {
    const fn empty() -> Self {
        Self {
            problems: BTreeMap::new(),
            newest: Timestamp::from_unix_millis(0),
        }
    }

    /// Add `reservation`'s digests, received up to `received`, if the
    /// encoded chunk stays within one marker payload; otherwise leave the
    /// chunk unchanged.
    fn fold(&mut self, reservation: &Reservation, received: Timestamp) -> bool {
        let mut next = self.clone();
        next.problems
            .entry(reservation.problem.clone())
            .or_default()
            .extend(reservation.reports.iter().cloned());
        next.newest = next.newest.max(received);
        let fits = serde_json::to_string(&next)
            .is_ok_and(|text| text.len() <= crate::state::MAX_MARKER_PAYLOAD_BYTES);
        if fits {
            *self = next;
        }
        fits
    }
}

/// Where a repository's dedupe window starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Forgotten {
    /// The newest `received_at` among reports evicted from the window.
    through: Timestamp,
}

/// What one intake compaction changed, or would change in a preview.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compaction {
    /// Applied reservations whose digests were folded into counted markers.
    pub folded: usize,
    /// Reservations removed because every attempt at their effect
    /// definitely did not apply, so they count nothing.
    pub dropped: usize,
    /// Reservations kept because their task is unsettled or their outcome
    /// is not established.
    pub kept: usize,
    /// Counted markers evicted from the dedupe window to make room.
    pub evicted: usize,
    /// The repository's forgotten-through time afterwards; see
    /// [`Counted::forgotten_through`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forgotten_through: Option<Timestamp>,
}

/// One repository's intake compaction within a house retention pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerCompaction {
    /// The intake workflow.
    pub workflow: WorkflowId,
    /// The repository whose intake markers these are.
    pub repository: Repository,
    /// What happened to them.
    pub outcome: CompactionOutcome,
}

/// Whether one repository's intake markers could be compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum CompactionOutcome {
    /// Compacted, or would be in a preview.
    Compacted(Compaction),
    /// A marker was malformed or a reserving task is missing, so none of
    /// this repository's intake markers changed.
    Refused,
}

/// The marker changes one compaction makes, applied with
/// [`crate::state::HouseStore::compact_markers`].
#[derive(Debug, Default)]
pub(crate) struct CompactionWrites {
    pub(crate) retire: Vec<(MarkerKey, MarkerFact)>,
    pub(crate) writes: Vec<MarkerWrite>,
}

impl CompactionWrites {
    pub(crate) fn is_empty(&self) -> bool {
        self.retire.is_empty() && self.writes.is_empty()
    }
}

/// Durable, house-scoped intake accounting for one repository, kept as
/// workflow markers in the [`HouseStore`].
///
/// Use it in this order within the task that submits the effect:
/// [`Self::counted`], [`plan`], [`Self::reserve`] for each posting proposal,
/// then [`crate::state::run_effect`] under the returned name. Use one
/// workflow id for a repository's intake: ledgers under different ids do not
/// see each other's reservations. Marker capacity
/// ([`crate::state::MAX_MARKERS`], shared by the house) bounds how many
/// intake effects one store records; [`Self::compact`], which
/// `kitchn store retain` runs for every repository, folds settled
/// reservations into the bounded dedupe window described in the module
/// docs.
#[derive(Debug, Clone)]
pub struct IntakeLedger<'a> {
    store: &'a HouseStore,
    workflow: WorkflowId,
    repository: Repository,
}

impl<'a> IntakeLedger<'a> {
    /// Intake accounting for `repository` under `workflow` in `store`.
    #[must_use]
    pub const fn new(store: &'a HouseStore, workflow: WorkflowId, repository: Repository) -> Self {
        Self {
            store,
            workflow,
            repository,
        }
    }

    fn item(&self) -> WorkItem {
        WorkItem::Repository {
            repository: self.repository.clone(),
        }
    }

    /// Read which reports are counted, for planning in `task`. A
    /// reservation counts only when an effect with its name was applied. A
    /// waived effect does not count: a risk decision is not evidence that
    /// the forge accepted the mutation. It is ignored when its task settled without applying it, and
    /// when it belongs to `task` itself, which resolves its own effects by
    /// planning the same mutation again.
    ///
    /// # Errors
    /// Returns [`IntakeError::Unreconciled`] while another unsettled task
    /// holds an unapplied reservation, [`IntakeError::IncompleteEvidence`]
    /// for a malformed reservation or one whose task is missing, and store
    /// errors.
    pub fn counted(&self, task: &TaskId) -> Result<Counted, crate::Error> {
        let schema = reservation_schema()?;
        let item = self.item();
        let mut counted = Counted::none(self.store.house().clone(), self.repository.clone());
        let markers: Vec<_> = self
            .store
            .markers(&self.workflow)?
            .into_iter()
            .filter(|marker| marker.key().item == item)
            .collect();
        if markers.is_empty() {
            return Ok(counted);
        }
        // Read after the markers, so every reserving task is present and its
        // effects are at least as new as the reservation.
        let tasks: BTreeMap<TaskId, TaskRecord> = self
            .store
            .tasks()?
            .into_iter()
            .map(|record| (record.spec().id.clone(), record))
            .collect();
        let chunk_schema = counted_schema()?;
        let forgotten_schema = forgotten_schema()?;
        for marker in markers {
            counted.observed.insert(marker.key().clone());
            if is_schema(marker.fact(), &forgotten_schema) {
                let forgotten: Forgotten = marker
                    .fact()
                    .decode(&forgotten_schema)
                    .map_err(|_| IntakeError::IncompleteEvidence)?;
                counted.forgotten = counted.forgotten.max(Some(forgotten.through));
                continue;
            }
            if is_schema(marker.fact(), &chunk_schema) {
                let chunk: CountedChunk = marker
                    .fact()
                    .decode(&chunk_schema)
                    .map_err(|_| IntakeError::IncompleteEvidence)?;
                for (problem, reports) in chunk.problems {
                    counted.add(problem, reports.into_iter().collect());
                }
                continue;
            }
            let reservation: Reservation = marker
                .fact()
                .decode(&schema)
                .map_err(|_| IntakeError::IncompleteEvidence)?;
            let record = tasks
                .get(&reservation.task)
                .ok_or(IntakeError::IncompleteEvidence)?;
            if applied(record, &reservation.effect) {
                counted.add(reservation.problem, reservation.reports);
            } else if !matches!(record.state(), TaskState::Settled { .. })
                && &reservation.task != task
            {
                return Err(IntakeError::Unreconciled.into());
            }
        }
        Ok(counted)
    }

    /// Fold the reservations whose tasks settled with their effect applied
    /// into this repository's counted-digest markers, and remove those whose
    /// effect definitely did not apply, in one store transaction. The
    /// counted digests stay, so [`Self::counted`] reports the same reports
    /// afterwards, and the reserving tasks are no longer needed to read them.
    /// A reservation stays while its task is unsettled or any attempt at its
    /// effect has no established outcome, since a later lookup may still
    /// find it applied. When [`MAX_COUNTED_CHUNKS`] markers are full, the
    /// oldest is evicted and the forgotten-through time rises (see the
    /// module docs).
    ///
    /// # Errors
    /// [`IntakeError::IncompleteEvidence`] for a malformed marker or a
    /// reservation whose task is missing; store errors, including a marker
    /// conflict when intake state changed during the call, in which case
    /// nothing changed.
    pub fn compact(
        &self,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<Compaction, crate::Error> {
        let item = self.item();
        let markers: Vec<_> = self
            .store
            .markers(&self.workflow)?
            .into_iter()
            .filter(|marker| marker.key().item == item)
            .collect();
        let tasks: BTreeMap<TaskId, TaskRecord> = self
            .store
            .tasks()?
            .into_iter()
            .map(|record| (record.spec().id.clone(), record))
            .collect();
        let markers: Vec<&WorkflowMarker> = markers.iter().collect();
        let (compaction, changes) = plan_compaction(&self.workflow, &item, &markers, &tasks)?;
        if !changes.is_empty() {
            self.store
                .compact_markers(&changes.retire, changes.writes, recorded_by, now)?;
        }
        Ok(compaction)
    }

    /// Record the reservation for `proposal` in `task` before its mutation is
    /// submitted, and return the effect name to submit it under. Reserving
    /// the same proposal again is a no-op. The write is refused if any
    /// reservation for this repository was recorded after `counted` was
    /// read, so two planners cannot both claim the same reports.
    ///
    /// # Errors
    /// Returns [`IntakeError::StaleCount`] when intake state changed since
    /// `counted`, [`IntakeError::CrossHouse`] or [`IntakeError::Authority`]
    /// for another house's or repository's state or mutation,
    /// [`IntakeError::InvalidReport`] for a proposal without a mutation or
    /// over [`MAX_PER_PROPOSAL`], and store errors such as a full marker
    /// store.
    pub fn reserve(
        &self,
        counted: &Counted,
        proposal: &Proposal,
        task: &TaskId,
        recorded_by: &Claimant,
        now: Timestamp,
    ) -> Result<EffectName, crate::Error> {
        if &counted.house != self.store.house() {
            return Err(IntakeError::CrossHouse.into());
        }
        let mutation = proposal.mutation().ok_or(IntakeError::InvalidReport)?;
        if counted.repository != self.repository || mutation.repository != self.repository {
            return Err(IntakeError::Authority.into());
        }
        let reservation = proposal
            .reservation(task)
            .ok_or(IntakeError::InvalidReport)?;
        if reservation.reports.len() > MAX_PER_PROPOSAL {
            return Err(IntakeError::InvalidReport.into());
        }
        let name = reservation.effect.clone();
        let subject = ExternalRef::new(&format!("{task}/{name}"))?;
        let key = MarkerKey {
            workflow: self.workflow.clone(),
            item: self.item(),
            subject: MarkerSubject::Observation(subject),
        };
        let fact = MarkerFact::workflow(reservation_schema()?, &reservation)?;
        let item = self.item();
        let attempt = self
            .store
            .record_marker_unless(key, fact, recorded_by, now, |markers| {
                let changed = markers.iter().any(|marker| {
                    marker.key().item == item && !counted.observed.contains(marker.key())
                });
                Ok(changed.then_some(()))
            })?;
        match attempt {
            MarkerAttempt::Recorded(_) | MarkerAttempt::AlreadyRecorded(_) => Ok(name),
            MarkerAttempt::Blocked(()) => Err(IntakeError::StaleCount.into()),
        }
    }
}

fn reservation_schema() -> Result<MarkerSchema, crate::Error> {
    Ok(MarkerSchema::new(RESERVATION_SCHEMA, NonZeroU32::MIN)?)
}

fn counted_schema() -> Result<MarkerSchema, crate::Error> {
    Ok(MarkerSchema::new(COUNTED_SCHEMA, NonZeroU32::MIN)?)
}

fn forgotten_schema() -> Result<MarkerSchema, crate::Error> {
    Ok(MarkerSchema::new(FORGOTTEN_SCHEMA, NonZeroU32::MIN)?)
}

fn is_schema(fact: &MarkerFact, expected: &MarkerSchema) -> bool {
    matches!(fact, MarkerFact::Workflow { schema, .. } if schema == expected)
}

fn observation_key(
    workflow: &WorkflowId,
    item: &WorkItem,
    subject: &str,
) -> Result<MarkerKey, crate::Error> {
    Ok(MarkerKey {
        workflow: workflow.clone(),
        item: item.clone(),
        subject: MarkerSubject::Observation(ExternalRef::new(subject)?),
    })
}

fn chunk_key(workflow: &WorkflowId, item: &WorkItem, seq: u64) -> Result<MarkerKey, crate::Error> {
    observation_key(workflow, item, &format!("{CHUNK_PREFIX}{seq}"))
}

/// The sequence number of a counted marker's key.
fn chunk_seq(key: &MarkerKey) -> Option<u64> {
    match &key.subject {
        MarkerSubject::Observation(subject) => {
            subject.as_str().strip_prefix(CHUNK_PREFIX)?.parse().ok()
        }
        MarkerSubject::Git(_) | MarkerSubject::Issue(_) => None,
    }
}

/// Counted chunks by sequence number, each with the fact it was read with
/// (`None` when created by this compaction).
type Chunks = BTreeMap<u64, (Option<MarkerFact>, CountedChunk)>;

/// Plan the compaction of one repository's intake `markers` (under
/// `workflow` and `item`) against `tasks`, both read from one view of the
/// store. Writes nothing.
fn plan_compaction(
    workflow: &WorkflowId,
    item: &WorkItem,
    markers: &[&WorkflowMarker],
    tasks: &BTreeMap<TaskId, TaskRecord>,
) -> Result<(Compaction, CompactionWrites), crate::Error> {
    let reservation_schema = reservation_schema()?;
    let chunk_schema = counted_schema()?;
    let forgotten_schema = forgotten_schema()?;
    let forgotten_key = observation_key(workflow, item, FORGOTTEN_SUBJECT)?;
    let mut chunks = Chunks::new();
    let mut forgotten: Option<(MarkerFact, Forgotten)> = None;
    let mut reservations = Vec::new();
    for marker in markers {
        let fact = marker.fact();
        if is_schema(fact, &chunk_schema) {
            let seq = chunk_seq(marker.key())
                .filter(|seq| chunk_key(workflow, item, *seq).is_ok_and(|key| &key == marker.key()))
                .ok_or(IntakeError::IncompleteEvidence)?;
            let chunk = fact
                .decode(&chunk_schema)
                .map_err(|_| IntakeError::IncompleteEvidence)?;
            chunks.insert(seq, (Some(fact.clone()), chunk));
        } else if is_schema(fact, &forgotten_schema) {
            if marker.key() != &forgotten_key {
                return Err(IntakeError::IncompleteEvidence.into());
            }
            let value = fact
                .decode(&forgotten_schema)
                .map_err(|_| IntakeError::IncompleteEvidence)?;
            forgotten = Some((fact.clone(), value));
        } else {
            let reservation: Reservation = fact
                .decode(&reservation_schema)
                .map_err(|_| IntakeError::IncompleteEvidence)?;
            let received = reservation.received.unwrap_or_else(|| marker.recorded_at());
            reservations.push((*marker, reservation, received));
        }
    }
    let previous = forgotten.as_ref().map(|(_, value)| value.through);
    let mut compaction = Compaction {
        forgotten_through: previous,
        ..Compaction::default()
    };
    let mut changes = CompactionWrites::default();
    let mut evicted = Vec::new();
    for (marker, reservation, received) in reservations {
        let record = tasks
            .get(&reservation.task)
            .ok_or(IntakeError::IncompleteEvidence)?;
        let outcome = if matches!(record.state(), TaskState::Settled { .. }) {
            settled_outcome(record, &reservation.effect)
        } else {
            None
        };
        match outcome {
            Some(true) if fold(&mut chunks, &reservation, received, &mut evicted) => {
                compaction.folded += 1;
            }
            Some(false) => compaction.dropped += 1,
            Some(true) | None => {
                compaction.kept += 1;
                continue;
            }
        }
        changes
            .retire
            .push((marker.key().clone(), marker.fact().clone()));
    }
    if changes.retire.is_empty() {
        return Ok((compaction, changes));
    }
    for (seq, fact, chunk) in evicted {
        compaction.evicted += 1;
        compaction.forgotten_through = compaction.forgotten_through.max(Some(chunk.newest));
        if let Some(fact) = fact {
            changes.retire.push((chunk_key(workflow, item, seq)?, fact));
        }
    }
    for (seq, (expected, chunk)) in chunks {
        let key = chunk_key(workflow, item, seq)?;
        let fact = MarkerFact::workflow(chunk_schema.clone(), &chunk)?;
        changes.writes.push(match expected {
            Some(expected) if expected == fact => continue,
            Some(expected) => MarkerWrite::Supersede {
                key,
                expected,
                fact,
            },
            None => MarkerWrite::Record(key, fact),
        });
    }
    if let Some(through) = compaction.forgotten_through
        && compaction.forgotten_through != previous
    {
        let fact = MarkerFact::workflow(forgotten_schema, &Forgotten { through })?;
        changes.writes.push(match forgotten {
            Some((expected, _)) => MarkerWrite::Supersede {
                key: forgotten_key,
                expected,
                fact,
            },
            None => MarkerWrite::Record(forgotten_key, fact),
        });
    }
    Ok((compaction, changes))
}

/// Plan intake compaction for every repository ledger in one view of the
/// store: each workflow and repository holding intake markers. A ledger
/// whose markers cannot be read is refused and left unchanged; the others
/// still compact. Writes nothing.
pub(crate) fn plan_house_compaction<'a>(
    markers: impl Iterator<Item = &'a WorkflowMarker>,
    tasks: &BTreeMap<TaskId, TaskRecord>,
) -> Vec<(LedgerCompaction, CompactionWrites)> {
    let mut ledgers: BTreeMap<(WorkflowId, Repository), Vec<&WorkflowMarker>> = BTreeMap::new();
    for marker in markers {
        let intake = matches!(
            marker.fact(),
            MarkerFact::Workflow { schema, .. }
                if [RESERVATION_SCHEMA, COUNTED_SCHEMA, FORGOTTEN_SCHEMA].contains(&schema.name())
        );
        if intake && let WorkItem::Repository { repository } = &marker.key().item {
            ledgers
                .entry((marker.key().workflow.clone(), repository.clone()))
                .or_default()
                .push(marker);
        }
    }
    ledgers
        .into_iter()
        .map(|((workflow, repository), markers)| {
            let item = WorkItem::Repository {
                repository: repository.clone(),
            };
            let (outcome, changes) = match plan_compaction(&workflow, &item, &markers, tasks) {
                Ok((compaction, changes)) => (CompactionOutcome::Compacted(compaction), changes),
                Err(_) => (CompactionOutcome::Refused, CompactionWrites::default()),
            };
            let summary = LedgerCompaction {
                workflow,
                repository,
                outcome,
            };
            (summary, changes)
        })
        .collect()
}

/// Fold `reservation` into the newest chunk, or a new one when it does not
/// fit. A new chunk beyond [`MAX_COUNTED_CHUNKS`] moves the oldest into
/// `evicted`. Returns false, changing nothing, only when the reservation
/// cannot fit one chunk or sequence numbers are exhausted.
fn fold(
    chunks: &mut Chunks,
    reservation: &Reservation,
    received: Timestamp,
    evicted: &mut Vec<(u64, Option<MarkerFact>, CountedChunk)>,
) -> bool {
    if let Some((_, (_, last))) = chunks.iter_mut().next_back()
        && last.fold(reservation, received)
    {
        return true;
    }
    let mut chunk = CountedChunk::empty();
    if !chunk.fold(reservation, received) {
        return false;
    }
    let Some(seq) = chunks
        .keys()
        .next_back()
        .map_or(Some(0), |last| last.checked_add(1))
    else {
        return false;
    };
    while chunks.len() >= MAX_COUNTED_CHUNKS {
        let Some((oldest, (fact, old))) = chunks.pop_first() else {
            break;
        };
        evicted.push((oldest, fact, old));
    }
    chunks.insert(seq, (None, chunk));
    true
}

/// Whether a settled task's effect named `name` applied (`Some(true)`),
/// definitely did not in every attempt (`Some(false)`), or is not yet
/// established (`None`).
fn settled_outcome(task: &TaskRecord, name: &EffectName) -> Option<bool> {
    if applied(task, name) {
        return Some(true);
    }
    task.effects()
        .iter()
        .filter(|effect| effect.name() == name)
        .all(|effect| match effect.state() {
            EffectState::NotApplied { .. } => true,
            EffectState::Applied { .. }
            | EffectState::Intended
            | EffectState::Waived { .. }
            | EffectState::Uncertain { .. }
            | EffectState::Unresolvable { .. } => false,
        })
        .then_some(false)
}

/// Whether `task` applied an effect named `name`. A waived effect is a risk
/// decision, not forge evidence: its outcome is still unknown, so it does not
/// count and its reports are proposed again until reconciliation finds the
/// mutation applied.
fn applied(task: &TaskRecord, name: &EffectName) -> bool {
    task.effects()
        .iter()
        .filter(|effect| effect.name() == name)
        .any(|effect| match effect.state() {
            EffectState::Applied { .. } => true,
            EffectState::Intended
            | EffectState::Waived { .. }
            | EffectState::Uncertain { .. }
            | EffectState::NotApplied { .. }
            | EffectState::Unresolvable { .. } => false,
        })
}

/// The hidden marker intake writes into the issues and comments it creates.
#[must_use]
pub fn marker(problem: &ProblemKey) -> String {
    format!("{MARKER_PREFIX}{problem}{MARKER_SUFFIX}")
}

/// The problem named by the first intake marker in an issue body, for
/// rebuilding the known-issue index from forge evidence. A malformed marker
/// is ignored.
#[must_use]
pub fn problem_marker(body: &str) -> Option<ProblemKey> {
    let start = body.find(MARKER_PREFIX)?;
    let rest = body.get(start.saturating_add(MARKER_PREFIX.len())..)?;
    let end = rest.find(MARKER_SUFFIX)?;
    ProblemKey::new(rest.get(..end)?).ok()
}

/// `partial` says earlier reports left the dedupe window, so `total` is a
/// lower bound.
fn render_comment(
    problem: &ProblemKey,
    total: usize,
    partial: bool,
    reports: &[&Report],
) -> Result<Text, IntakeError> {
    let mut body = marker(problem);
    let _ = write!(
        body,
        "\n{} new external report{} for this problem; {}{total} counted in total.\n",
        reports.len(),
        plural(reports.len()),
        if partial { "at least " } else { "" },
    );
    render_reports(&mut body, reports);
    Text::new(&body).map_err(|_| IntakeError::InvalidReport)
}

fn render_draft(problem: &ProblemKey, reports: &[&Report]) -> Result<Text, IntakeError> {
    let mut body = marker(problem);
    let _ = write!(
        body,
        "\nDraft from {} external report{}. Needs a specification before work starts.\n",
        reports.len(),
        plural(reports.len()),
    );
    render_reports(&mut body, reports);
    Text::new(&body).map_err(|_| IntakeError::InvalidReport)
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Public reports are listed individually up to [`MAX_LISTED`]; private
/// reports are only counted per source.
fn render_reports(body: &mut String, reports: &[&Report]) {
    let mut private: BTreeMap<&SourceId, usize> = BTreeMap::new();
    let mut listed = 0_usize;
    let mut unlisted = 0_usize;
    for report in reports {
        match report.privacy {
            PrivacyClass::Private => {
                *private.entry(report.source()).or_default() += 1;
            }
            PrivacyClass::Public | PrivacyClass::LinkOnly if listed == MAX_LISTED => {
                unlisted += 1;
            }
            PrivacyClass::Public | PrivacyClass::LinkOnly => {
                listed += 1;
                render_listed(body, report);
            }
        }
    }
    for (source, count) in private {
        let _ = writeln!(
            body,
            "\n- `{source}`: {count} private report{}",
            plural(count)
        );
    }
    if unlisted > 0 {
        let _ = writeln!(body, "\n- {unlisted} more not listed");
    }
}

fn render_listed(body: &mut String, report: &Report) {
    let raw = report.raw();
    let _ = write!(body, "\n- `{}`", report.source());
    if let (Some(link), true) = (&raw.link, report.privacy.shows_link()) {
        let _ = write!(body, ": <{}>", link.as_str());
    }
    if report.privacy.shows_content() {
        let quote = truncate(raw.text.as_str(), MAX_QUOTE_BYTES);
        let fence = "`".repeat(longest_backtick_run(quote).saturating_add(1).max(3));
        let _ = write!(
            body,
            " from {}\n\n  {fence}text\n  {}\n  {fence}",
            code_span(raw.reporter.as_str()),
            quote.replace('\n', "\n  "),
        );
    }
    body.push('\n');
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default()
}

/// Inline code that a value containing backticks cannot close early.
fn code_span(value: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(value).saturating_add(1));
    if value.contains('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_stays_on_a_character_boundary() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("héllo", 3), "hé");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn backtick_runs_are_measured() {
        assert_eq!(longest_backtick_run("no ticks"), 0);
        assert_eq!(longest_backtick_run("a ``` b ` c"), 3);
    }

    #[test]
    fn code_spans_cannot_be_closed_by_their_content() {
        assert_eq!(code_span("ann"), "`ann`");
        assert_eq!(code_span("a`b"), "`` a`b ``");
        assert_eq!(code_span("``x"), "``` ``x ```");
    }

    #[test]
    fn reservations_recorded_before_received_times_still_decode() -> Result<(), crate::Error> {
        let legacy = serde_json::json!({
            "task": "intake-1",
            "effect": "intake-comment-0",
            "problem": "login-timeout",
            "reports": [],
        });
        let reservation: Reservation =
            serde_json::from_value(legacy.clone()).map_err(|_| IntakeError::InvalidReport)?;
        assert_eq!(reservation.received, None);
        // New reservations carry the time, and it round-trips.
        let current = Reservation {
            received: Some(Timestamp::from_unix_millis(5)),
            ..reservation
        };
        let encoded = serde_json::to_value(&current).map_err(|_| IntakeError::InvalidReport)?;
        assert_eq!(encoded.get("received"), Some(&serde_json::json!(5)));
        let mut unknown = legacy;
        if let Some(fields) = unknown.as_object_mut() {
            fields.insert("extra".to_owned(), serde_json::json!(1));
        }
        assert!(serde_json::from_value::<Reservation>(unknown).is_err());
        Ok(())
    }

    #[test]
    fn counted_marker_sequence_numbers_parse_only_their_own_keys() -> Result<(), crate::Error> {
        let workflow = WorkflowId::new("intake")?;
        let item = WorkItem::Repository {
            repository: Repository::new("lemarier/kitchen")?,
        };
        assert_eq!(chunk_seq(&chunk_key(&workflow, &item, 7)?), Some(7));
        assert_eq!(
            chunk_seq(&chunk_key(&workflow, &item, u64::MAX)?),
            Some(u64::MAX)
        );
        for subject in [FORGOTTEN_SUBJECT, "counted-", "counted-x", "counted"] {
            assert_eq!(
                chunk_seq(&observation_key(&workflow, &item, subject)?),
                None,
                "{subject}"
            );
        }
        // A signed number parses, but its key is not the canonical one, so
        // compaction refuses it.
        let signed = observation_key(&workflow, &item, "counted-+7")?;
        assert_eq!(chunk_seq(&signed), Some(7));
        assert_ne!(chunk_key(&workflow, &item, 7)?, signed);
        Ok(())
    }
}
