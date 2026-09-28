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

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
};

use crate::{
    BackendId, CredentialId, EffectName, HouseId,
    contracts::{
        ContractError, ExternalRef, GitHubAction, GitHubMutation, GrantScope, HouseGrants,
        IssueNumber, Permission, Repository, TaskAuthority, Text, Timestamp,
    },
    integrations::github::IssueState,
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
/// Longest source or problem key, in bytes. Short enough that a problem key
/// fits in an [`EffectName`] with its prefix.
pub const MAX_KEY_BYTES: usize = 48;

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
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// A report's identity: its source and source-native id. It is kept in
/// house-scoped state and never written to public issue text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReportKey {
    /// Declared source.
    pub source: SourceId,
    /// Source-native id.
    pub id: ExternalRef,
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

/// An issue already tracking a problem, with the reports it has counted.
/// `counted` comes from the house's private intake state, not from issue text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownIssue {
    /// Issue number.
    pub number: IssueNumber,
    /// Its lifecycle; unknown is never treated as open or closed.
    pub state: IssueState,
    /// Reports already counted on it.
    pub counted: BTreeSet<ReportKey>,
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
        /// Reports counted on the issue once these are recorded.
        total: usize,
        /// The comment to post.
        mutation: GitHubMutation,
    },
    /// Create a draft issue for a new problem, for the gardener to specify.
    DraftIssue {
        /// Problem.
        problem: ProblemKey,
        /// Reports it groups, to record once the issue is confirmed.
        reports: Vec<ReportKey>,
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

    /// A stable name for submitting [`Self::mutation`] as a task effect, so a
    /// rerun after an uncertain submission reuses the same effect instead of
    /// posting twice. A draft is named after its problem; a count comment
    /// after its issue and the new total, which a rerun computes the same way
    /// until the new reports are recorded as counted.
    #[must_use]
    pub fn effect_name(&self) -> Option<EffectName> {
        let name = match self {
            Self::DraftIssue { problem, .. } => format!("intake-draft-{problem}"),
            Self::AddReports { issue, total, .. } => {
                format!("intake-{}-{total}", issue.get())
            }
            Self::ClosedMatch { .. } | Self::MissingGrant { .. } => return None,
        };
        EffectName::new(&name).ok()
    }
}

/// Group `reports` by problem and propose one result per problem with new
/// reports. Reports already counted on a known issue, and redelivered copies
/// of one report, count once. Problems with nothing new produce nothing.
///
/// # Errors
/// Refuses reports from another house, authority for another repository,
/// more than 500 reports, one report classified under two problems, a known
/// issue in an unknown state, and output that exceeds forge bounds.
pub fn plan(
    house: &HouseId,
    repository: &Repository,
    reports: &[Classified],
    known: &BTreeMap<ProblemKey, KnownIssue>,
    authority: &PostingAuthority,
) -> Result<Vec<Proposal>, IntakeError> {
    if &authority.repository != repository {
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
        let issue = known.get(problem);
        if let Some(issue) = issue {
            grouped.retain(|key, _| !issue.counted.contains(key));
        }
        if grouped.is_empty() {
            continue;
        }
        let keys: Vec<ReportKey> = grouped.keys().cloned().collect();
        let listed: Vec<&Report> = grouped.into_values().collect();
        let proposal = match issue {
            Some(issue) => match issue.state {
                IssueState::Unknown => return Err(IntakeError::IncompleteEvidence),
                IssueState::Closed => Proposal::ClosedMatch {
                    problem: problem.clone(),
                    issue: issue.number,
                    reports: keys,
                },
                IssueState::Open if authority.post_comment => {
                    let total = issue.counted.len().saturating_add(keys.len());
                    let body = render_comment(problem, total, &listed)?;
                    Proposal::AddReports {
                        problem: problem.clone(),
                        issue: issue.number,
                        reports: keys,
                        total,
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

fn render_comment(
    problem: &ProblemKey,
    total: usize,
    reports: &[&Report],
) -> Result<Text, IntakeError> {
    let mut body = marker(problem);
    let _ = write!(
        body,
        "\n{} new external report{} for this problem; {total} counted in total.\n",
        reports.len(),
        plural(reports.len()),
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
}
