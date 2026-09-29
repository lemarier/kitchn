//! Brigade audit: a periodic review of how stations, guidance, and schedules
//! perform, ending in draft proposals.
//!
//! The inspector reviews delivered work and the gardener reviews issues; the
//! audit reviews the brigade itself. It reads one house's trust ledger
//! (#12), the attempt usage recorded on its tasks (#194), and the observed
//! runs of its schedules (#40), and reports:
//!
//! - repeated confirmed findings per station and work type: review findings,
//!   reverts, and regressions on live deliveries, and confirmed inspection
//!   samples of them;
//! - schedules with high usage of their run budget, or whose precheck was
//!   mostly idle ([`SchedulePolicy::idle_schedules`]);
//! - stations whose work types have diverged: first-pass acceptance of one
//!   work type far below another's under the same station.
//!
//! Each item links its evidence and states its sample size. Simulated
//! deliveries are counted apart and never support a proposal.
//!
//! The audit runs as an extended inspector mandate, not as a new role: it is
//! a bounded inspection whose subject is the brigade's record instead of one
//! delivery, and the inspector card already routes confirmed findings to
//! tests and guidance. A new role would also change the role-card digest
//! every adopted house pins.
//!
//! Proposals are drafts: a guidance change, a work type or role split, or a
//! schedule change. This module changes no guidance, grant, or schedule and
//! posts nothing. A caller files each [`Proposal::draft`] through the
//! existing issue workflow, and applying it needs the owner's decision. Each
//! draft carries a hidden [`marker`] naming its [`ProposalKey`].
//!
//! Proposals come out of a run only when both of these are known; otherwise
//! the run reports without proposing, and says why ([`Withheld`]):
//!
//! - the house budget: the schedule evidence must be [`ListedSchedules`], the
//!   complete inventory the house's bound schedule backend lists, and its
//!   window must reach back to the window's start. Evidence from anywhere
//!   else may omit schedules whose runs spent the budget.
//! - the open proposals: the caller passes the complete set of keys of open
//!   proposals, read back from the forge with [`proposal_key`]. The audit
//!   does not read the forge itself, and a missing set is never taken as
//!   empty, so an open proposal is not drafted again.
//!
//! A run is refused while the house budget is shown exhausted. At most
//! [`AuditPolicy::max_proposals`] proposals come out of one run; the rest
//! are listed as deferred.
//!
//! The report copies no free text from its inputs: finding consequences,
//! task specifications, and transcripts stay in the house. Evidence sources
//! are listed only when they are public-safe [`EvidenceLink`]s: an `https`
//! link on the house's forge into one of its own repositories, or a Kitchen
//! identifier. Every other source is private: it is counted, never listed.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    num::NonZeroU32,
};

use serde::Serialize;

use crate::{
    ConsumerId, ErrorClass, HouseId,
    contracts::{Capability, ExternalRef, Repository, Role, ScheduleBackend, Text, Timestamp},
    house::ForgeKind,
    scheduling::{Exhausted, ScheduleEvidence, SchedulePolicy, UsageWindow},
    selection::WorkType,
    state::{AttemptUsage, HouseStore},
    trust::{EvidenceMode, Ledger, Measurement, TrustError},
    workflows::inspector::SampleResult,
};

/// The workflow name audit proposals are marked with.
pub const WORKFLOW: &str = "brigade-audit";
/// Most proposals one run may return.
pub const MAX_PROPOSALS: usize = 32;
/// Most evidence links listed for one item; the rest are counted.
pub const MAX_LISTED: usize = 20;
/// Longest [`ProposalKey`] in bytes.
pub const MAX_KEY_BYTES: usize = 200;

const MARKER_PREFIX: &str = "<!-- kitchn:brigade-audit key=";
const MARKER_SUFFIX: &str = " -->";

/// Audit input or evidence failure. Private finding content is never included.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuditError {
    /// A threshold is out of range.
    #[error("invalid audit policy")]
    InvalidPolicy,
    /// The ledger, store, or schedule evidence belongs to another house than
    /// the one the audit runs for.
    #[error("the audit reads only the house it runs for")]
    CrossHouse,
    /// The house usage budget is exhausted in the current window.
    #[error("the house usage budget is exhausted")]
    BudgetExhausted(Exhausted),
    /// The backend does not fully support schedule management, so it cannot
    /// list the house's schedules.
    #[error("the schedule backend cannot list the house's schedules")]
    ListingUnsupported,
}

impl AuditError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidPolicy => ErrorClass::InvalidInput,
            Self::CrossHouse | Self::BudgetExhausted(_) | Self::ListingUnsupported => {
                ErrorClass::Refused
            }
        }
    }
}

/// When the audit judges a record worth a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPolicy {
    /// Distinct confirmed findings on one station and work type before a
    /// guidance change is proposed.
    pub repeated_findings: NonZeroU32,
    /// Share of a schedule's run budget, in percent (1 to 100), used in the
    /// current window at which its usage is high.
    pub high_usage_percent: u8,
    /// Gap in first-pass acceptance, in percentage points (1 to 100),
    /// between two work types of one station at which they have diverged.
    pub divergence_points: u8,
    /// Deliveries with a first-pass verdict a work type needs before its
    /// acceptance is compared.
    pub min_samples: NonZeroU32,
    /// Proposals one run returns, at most [`MAX_PROPOSALS`].
    pub max_proposals: NonZeroU32,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self {
            repeated_findings: NonZeroU32::MIN.saturating_add(1),
            high_usage_percent: 80,
            divergence_points: 30,
            min_samples: NonZeroU32::MIN.saturating_add(4),
            max_proposals: NonZeroU32::MIN.saturating_add(9),
        }
    }
}

impl AuditPolicy {
    /// Check bounds.
    ///
    /// # Errors
    /// [`AuditError::InvalidPolicy`] for a percentage outside 1 to 100 or
    /// more than [`MAX_PROPOSALS`] proposals.
    pub fn validate(&self) -> Result<(), AuditError> {
        let limit = usize::try_from(self.max_proposals.get()).unwrap_or(usize::MAX);
        let percent = |value: u8| (1..=100).contains(&value);
        if limit > MAX_PROPOSALS
            || !percent(self.high_usage_percent)
            || !percent(self.divergence_points)
        {
            return Err(AuditError::InvalidPolicy);
        }
        Ok(())
    }
}

/// Identity of a proposal across runs, carried as a hidden marker in the
/// draft filed for it: `guidance:<station>:<work type>`,
/// `split:<station>:<work type>`, or `schedule:<consumer>:<change>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProposalKey(String);

impl ProposalKey {
    /// Validate a key read back from a filed proposal: 1 to
    /// [`MAX_KEY_BYTES`] ASCII letters, digits, `-`, `_`, `.`, and `:`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= MAX_KEY_BYTES
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            });
        valid.then(|| Self(value.to_owned()))
    }

    /// The key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn of(kind: &ProposalKind) -> Self {
        Self(match kind {
            ProposalKind::Guidance {
                station, work_type, ..
            } => format!("guidance:{station}:{work_type}"),
            ProposalKind::Split {
                station, work_type, ..
            } => format!("split:{station}:{work_type}"),
            ProposalKind::Schedule {
                consumer, change, ..
            } => format!("schedule:{consumer}:{change}"),
        })
    }
}

impl fmt::Display for ProposalKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The hidden marker a filed proposal carries.
#[must_use]
pub fn marker(key: &ProposalKey) -> String {
    format!("{MARKER_PREFIX}{key}{MARKER_SUFFIX}")
}

/// The proposal named by the first audit marker in an issue body, for
/// rebuilding the set of open proposals from the forge. A malformed marker
/// is ignored.
#[must_use]
pub fn proposal_key(body: &str) -> Option<ProposalKey> {
    let start = body.find(MARKER_PREFIX)?;
    let rest = body.get(start.saturating_add(MARKER_PREFIX.len())..)?;
    let end = rest.find(MARKER_SUFFIX)?;
    ProposalKey::parse(rest.get(..end)?)
}

/// Every schedule of one house as its bound schedule backend lists them: the
/// only schedule evidence that can show the house budget remains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSchedules(ScheduleEvidence);

impl ListedSchedules {
    /// List the house's schedules and their recent runs through `backend`.
    ///
    /// # Errors
    /// [`AuditError::ListingUnsupported`] when the backend does not fully
    /// support [`Capability::ScheduleManage`], [`AuditError::CrossHouse`]
    /// when the listing names another house than the backend serves, and the
    /// backend's read failures.
    pub fn read<B: ScheduleBackend>(backend: &B) -> crate::Result<Self> {
        let descriptor = backend.descriptor();
        if !descriptor.capabilities.supports(Capability::ScheduleManage) {
            return Err(AuditError::ListingUnsupported.into());
        }
        let evidence = backend.schedule_evidence().map_err(Into::into)?;
        if evidence.house != descriptor.house {
            return Err(AuditError::CrossHouse.into());
        }
        Ok(Self(evidence))
    }

    /// The listed evidence.
    #[must_use]
    pub const fn evidence(&self) -> &ScheduleEvidence {
        &self.0
    }
}

/// The schedule evidence an audit reads, and whether it is the house's
/// complete schedule inventory.
#[derive(Debug, Clone, Copy)]
pub enum ScheduleInventory<'a> {
    /// The complete listing from the house's schedule backend.
    Listed(&'a ListedSchedules),
    /// Evidence from any other source, such as a file. It may omit
    /// schedules, so it cannot show that the house budget remains, and the
    /// run proposes nothing.
    Unproven(&'a ScheduleEvidence),
}

impl<'a> ScheduleInventory<'a> {
    /// The evidence either way.
    #[must_use]
    pub const fn evidence(&self) -> &'a ScheduleEvidence {
        match self {
            Self::Listed(listed) => listed.evidence(),
            Self::Unproven(evidence) => evidence,
        }
    }
}

/// Where the house's public-safe evidence links point.
#[derive(Debug, Clone, Copy)]
pub struct Publication<'a> {
    /// The forge the house's binding names, if any. Without one no source
    /// is a public forge link.
    pub forge: Option<ForgeKind>,
    /// The house's own repositories.
    pub repositories: &'a BTreeSet<Repository>,
}

impl Publication<'_> {
    fn evidence<'r>(&self, sources: impl IntoIterator<Item = &'r ExternalRef>) -> LinkedEvidence {
        let mut evidence = LinkedEvidence::default();
        for source in sources {
            evidence.total = evidence.total.saturating_add(1);
            match self
                .forge
                .and_then(|forge| EvidenceLink::forge(forge, self.repositories, source))
            {
                Some(link) if evidence.links.len() < MAX_LISTED => evidence.links.push(link),
                Some(_) => evidence.unlisted = evidence.unlisted.saturating_add(1),
                None => evidence.private = evidence.private.saturating_add(1),
            }
        }
        evidence
    }
}

/// What the audit reads. Every source must belong to `house`.
#[derive(Debug, Clone, Copy)]
pub struct AuditInputs<'a> {
    /// The house the audit runs for.
    pub house: &'a HouseId,
    /// The house trust ledger.
    pub ledger: &'a Ledger,
    /// The house store holding attempt usage.
    pub store: &'a HouseStore,
    /// The house schedule policy, for budgets and the idle threshold.
    pub schedules: &'a SchedulePolicy,
    /// Observed runs of the house's schedules; its time is the audit's time.
    pub inventory: ScheduleInventory<'a>,
    /// The complete set of keys of proposals still open on the forge, or
    /// `None` when it is not known. `None` is never taken as empty: the run
    /// then proposes nothing.
    pub open: Option<&'a BTreeSet<ProposalKey>>,
    /// Which evidence sources may be listed.
    pub publication: Publication<'a>,
}

/// A public-safe evidence link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum EvidenceLink {
    /// An `https` link on the house's forge into one of its repositories.
    Forge(ExternalRef),
    /// A Kitchen schedule consumer of the house.
    Consumer(ConsumerId),
}

impl EvidenceLink {
    /// `source` as a forge link when it is public-safe: for GitHub,
    /// `https://github.com/<owner>/<repo>` with `<owner>/<repo>` one of
    /// `repositories`, then only path segments of ASCII letters, digits, `-`,
    /// `_`, and `.` (never `.` or `..` alone), and an optional `#` fragment
    /// of letters, digits, `-`, and `_`. A query, port, credential, or any
    /// other host is private.
    #[must_use]
    pub fn forge(
        forge: ForgeKind,
        repositories: &BTreeSet<Repository>,
        source: &ExternalRef,
    ) -> Option<Self> {
        match forge {
            ForgeKind::GitHub => {
                let rest = source.as_str().strip_prefix("https://github.com/")?;
                let (path, fragment) = match rest.split_once('#') {
                    Some((path, fragment)) => (path, Some(fragment)),
                    None => (rest, None),
                };
                let fragment_safe = fragment.is_none_or(|fragment| {
                    !fragment.is_empty()
                        && fragment
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                });
                let segment_safe = |segment: &str| {
                    !matches!(segment, "" | "." | "..")
                        && segment.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                        })
                };
                let mut segments = path.split('/');
                let (owner, name) = (segments.next()?, segments.next()?);
                let own = repositories.iter().any(|repository| {
                    repository.owner().eq_ignore_ascii_case(owner)
                        && repository.name().eq_ignore_ascii_case(name)
                });
                (own && fragment_safe
                    && segment_safe(owner)
                    && segment_safe(name)
                    && segments.all(segment_safe))
                .then(|| Self::Forge(source.clone()))
            }
        }
    }
}

impl fmt::Display for EvidenceLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forge(link) => write!(formatter, "{link}"),
            Self::Consumer(consumer) => write!(formatter, "schedule consumer `{consumer}`"),
        }
    }
}

/// Distinct evidence sources, listing only public-safe links.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedEvidence {
    /// Distinct sources.
    pub total: u32,
    /// Public-safe links, at most [`MAX_LISTED`].
    pub links: Vec<EvidenceLink>,
    /// Public-safe links beyond the listing.
    pub unlisted: u32,
    /// Private sources: counted, kept in the house, never listed.
    pub private: u32,
}

impl LinkedEvidence {
    fn consumer(consumer: &ConsumerId) -> Self {
        Self {
            total: 1,
            links: vec![EvidenceLink::Consumer(consumer.clone())],
            unlisted: 0,
            private: 0,
        }
    }
}

/// Token and cost use of one station and work type's attempts. Unreported
/// usage is counted, never taken as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    /// Attempts recorded.
    pub attempts: u32,
    /// Attempts with a usage report.
    pub reported: u32,
    /// Tokens of the reports that stated every kind.
    pub tokens: u64,
    /// Reports that stated every token kind.
    pub token_samples: u32,
    /// Cost in millionths of a US dollar, of the reports that stated one.
    pub cost_micros: u64,
    /// Reports that stated a cost.
    pub cost_samples: u32,
}

/// First-pass acceptance of the deliveries that have a verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Acceptance {
    /// Deliveries accepted on the first pass.
    pub accepted: u32,
    /// Deliveries with a first-pass verdict: the sample size.
    pub judged: u32,
}

impl Acceptance {
    /// Accepted share in whole percent, or `None` with no verdicts.
    #[must_use]
    pub fn percent(&self) -> Option<u64> {
        (self.judged > 0).then(|| u64::from(self.accepted) * 100 / u64::from(self.judged))
    }
}

/// One station and work type's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StationRecord {
    /// The station: the task role.
    pub station: Role,
    /// The work type.
    pub work_type: WorkType,
    /// Live deliveries: observation streams at their latest revision.
    pub deliveries: u32,
    /// Simulated deliveries, counted apart and never judged.
    pub simulated: u32,
    /// First-pass acceptance of the live deliveries.
    pub first_pass: Acceptance,
    /// Pull requests of the live deliveries not accepted on the first pass.
    pub first_pass_rejected: LinkedEvidence,
    /// Distinct confirmed findings, reverts, and regressions, including
    /// confirmed inspection samples.
    pub findings: LinkedEvidence,
    /// Pull requests of the live deliveries those findings concern.
    pub deliveries_with_findings: LinkedEvidence,
    /// Attempt usage of the station's tasks of this work type.
    pub usage: UsageSummary,
}

/// Why a schedule is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ScheduleSignal {
    /// Its runs used at least the policy's share of its run budget in the
    /// current window.
    HighUsage {
        /// Agent runs in the window: the sample size.
        runs: u32,
        /// Its run budget.
        allowed: u32,
        /// Whether the observation reached the window's start; if not, `runs`
        /// is a lower bound.
        complete: bool,
    },
    /// Its precheck reported idle for most recent runs.
    MostlyIdle {
        /// Recent runs observed: the sample size.
        runs: u32,
        /// Of those, runs whose precheck reported idle.
        idle_runs: u32,
    },
}

/// A schedule the report lists. The backend's handle for it stays private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRecord {
    /// The consumer scope it serves.
    pub consumer: ConsumerId,
    /// Why it is listed.
    pub signal: ScheduleSignal,
}

/// A station whose work types' first-pass acceptance has diverged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Divergence {
    /// The station.
    pub station: Role,
    /// The work type accepted most often, and its acceptance.
    pub leading: (WorkType, Acceptance),
    /// The work type accepted least often, and its acceptance.
    pub trailing: (WorkType, Acceptance),
    /// Pull requests of the trailing work type not accepted on the first pass.
    pub evidence: LinkedEvidence,
}

/// A schedule change a proposal asks the owner to consider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleChange {
    /// Lengthen the interval or sharpen the precheck of a mostly idle schedule.
    ReduceIdleRuns,
    /// Revisit the allocation or interval of a schedule near its budget.
    RevisitBudget,
}

impl fmt::Display for ScheduleChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReduceIdleRuns => "reduce-idle-runs",
            Self::RevisitBudget => "revisit-budget",
        })
    }
}

/// What a proposal asks the owner to decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProposalKind {
    /// New or changed guidance for a station's work type with repeated
    /// confirmed findings.
    Guidance {
        /// The station.
        station: Role,
        /// The work type.
        work_type: WorkType,
        /// Distinct confirmed findings.
        findings: u32,
    },
    /// A separate work type or role for a station's trailing work type.
    Split {
        /// The station.
        station: Role,
        /// The trailing work type.
        work_type: WorkType,
    },
    /// A schedule change.
    Schedule {
        /// The schedule's consumer scope.
        consumer: ConsumerId,
        /// The change.
        change: ScheduleChange,
    },
}

/// A draft proposal. It carries no authority: filing it goes through the
/// issue workflow, and applying it needs the owner's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    /// Identity across runs.
    pub key: ProposalKey,
    /// What it proposes.
    pub kind: ProposalKind,
    /// Observations behind it: the sample size.
    pub samples: u32,
    /// Its evidence.
    pub evidence: LinkedEvidence,
}

impl Proposal {
    /// The draft issue body: its marker, what it proposes, the sample size,
    /// the listed evidence links, and how many more and private sources
    /// there are. Private sources are never listed.
    ///
    /// # Errors
    /// Returns a [`crate::contracts::ContractError`] if the body exceeds
    /// [`Text`] bounds, which the listing limit prevents.
    pub fn draft(&self) -> crate::Result<Text> {
        let mut body = marker(&self.key);
        body.push('\n');
        let _ = match &self.kind {
            ProposalKind::Guidance {
                station,
                work_type,
                findings,
            } => writeln!(
                body,
                "Proposed guidance change: `{station}` on `{work_type}` work had {findings} distinct confirmed findings."
            ),
            ProposalKind::Split { station, work_type } => writeln!(
                body,
                "Proposed work type or role split: `{station}` first-pass acceptance on `{work_type}` work trails its other work types."
            ),
            ProposalKind::Schedule { consumer, change } => writeln!(
                body,
                "Proposed schedule change for `{consumer}`: {}.",
                match change {
                    ScheduleChange::ReduceIdleRuns =>
                        "lengthen the interval or sharpen the precheck",
                    ScheduleChange::RevisitBudget => "revisit its allocation or interval",
                }
            ),
        };
        let _ = writeln!(
            body,
            "\nSample size: {}. This is a draft from the brigade audit; applying it needs the owner's decision, and the audit changed no guidance, grant, or schedule.\n",
            self.samples
        );
        for link in &self.evidence.links {
            let _ = writeln!(body, "- {link}");
        }
        if self.evidence.unlisted > 0 {
            let _ = writeln!(body, "- and {} more", self.evidence.unlisted);
        }
        if self.evidence.private > 0 {
            let _ = writeln!(
                body,
                "- {} private references are kept in the house and not listed",
                self.evidence.private
            );
        }
        Ok(Text::new(&body)?)
    }
}

/// Why a run proposed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Withheld {
    /// Budget unknown: the schedule evidence is not the complete listing
    /// from the house's schedule backend.
    UnprovenInventory,
    /// Budget unknown: the observed runs do not reach back to the start of
    /// the budget window.
    IncompleteWindow,
    /// The complete set of open proposals was not supplied.
    OpenProposalsUnknown,
}

impl fmt::Display for Withheld {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnprovenInventory => {
                "budget unknown: the schedule evidence is not the complete listing from the house's schedule backend"
            }
            Self::IncompleteWindow => {
                "budget unknown: the observed schedule runs do not reach back to the start of the budget window"
            }
            Self::OpenProposalsUnknown => {
                "open proposals unknown: the complete set of open proposal keys was not supplied"
            }
        })
    }
}

/// The audit's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditReport {
    /// The audited house.
    pub house: HouseId,
    /// When the schedule evidence was observed.
    pub observed_at: Timestamp,
    /// The budget window the run spent.
    pub window: UsageWindow,
    /// Every station and work type with deliveries or attempts.
    pub stations: Vec<StationRecord>,
    /// Observation streams with a revision gap, left out until reconciled.
    pub incomplete_streams: u32,
    /// Attempts whose task recorded no work type.
    pub unattributed_attempts: u32,
    /// Schedules with high usage or mostly idle runs.
    pub schedules: Vec<ScheduleRecord>,
    /// Stations whose work types have diverged.
    pub divergences: Vec<Divergence>,
    /// Why this run proposed nothing; empty when it could propose.
    pub withheld: Vec<Withheld>,
    /// Draft proposals from this run.
    pub proposals: Vec<Proposal>,
    /// Proposals left out because an open proposal already covers them.
    pub deduplicated: Vec<ProposalKey>,
    /// Proposals beyond this run's limit, for a later run.
    pub deferred: Vec<ProposalKey>,
    /// Proposals a run would make once nothing is [`Self::withheld`].
    pub withheld_proposals: Vec<ProposalKey>,
}

/// One station and work type's sources, before publication.
#[derive(Default)]
struct Tally {
    deliveries: u32,
    simulated: u32,
    first_pass: Acceptance,
    rejected: BTreeSet<ExternalRef>,
    findings: BTreeSet<ExternalRef>,
    delivered: BTreeSet<ExternalRef>,
    usage: UsageSummary,
}

type Tallies = BTreeMap<(Role, WorkType), Tally>;

/// Run the audit for `inputs.house`. Reads only.
///
/// # Errors
/// [`AuditError::CrossHouse`] when any source belongs to another house,
/// [`AuditError::BudgetExhausted`] when the house budget is spent,
/// [`AuditError::InvalidPolicy`], and any schedule-evidence, ledger, or
/// store read failure.
pub fn audit(policy: &AuditPolicy, inputs: &AuditInputs<'_>) -> crate::Result<AuditReport> {
    policy.validate()?;
    let house = inputs.house;
    let evidence = inputs.inventory.evidence();
    if inputs.ledger.house() != house || inputs.store.house() != house || &evidence.house != house {
        return Err(AuditError::CrossHouse.into());
    }
    let assessment = inputs.schedules.assess(house, evidence)?;
    if let Some(exhausted) = assessment.house_exhausted {
        return Err(AuditError::BudgetExhausted(exhausted).into());
    }
    let mut withheld = Vec::new();
    match inputs.inventory {
        ScheduleInventory::Listed(_) => {}
        ScheduleInventory::Unproven(_) => withheld.push(Withheld::UnprovenInventory),
    }
    if !assessment.house.complete {
        withheld.push(Withheld::IncompleteWindow);
    }
    if inputs.open.is_none() {
        withheld.push(Withheld::OpenProposalsUnknown);
    }

    let mut tallies = Tallies::new();
    let incomplete_streams = read_ledger(inputs.ledger, &mut tallies)?;
    let mut unattributed_attempts = 0_u32;
    for entry in inputs.store.attempt_usage()? {
        let Some(work_type) = entry.work_type else {
            unattributed_attempts = unattributed_attempts.saturating_add(1);
            continue;
        };
        let usage = &mut tally(&mut tallies, entry.station, &work_type).usage;
        usage.attempts = usage.attempts.saturating_add(1);
        if let AttemptUsage::Reported { report, .. } = &entry.usage {
            usage.reported = usage.reported.saturating_add(1);
            if let Some(tokens) = report.tokens.total() {
                usage.tokens = usage.tokens.saturating_add(tokens);
                usage.token_samples = usage.token_samples.saturating_add(1);
            }
            if let Some(cost) = report.cost {
                usage.cost_micros = usage.cost_micros.saturating_add(cost.amount.0);
                usage.cost_samples = usage.cost_samples.saturating_add(1);
            }
        }
    }
    let publication = &inputs.publication;
    let divergences = divergences(policy, &tallies, publication);

    let mut schedules: Vec<ScheduleRecord> = assessment
        .schedules
        .iter()
        .filter_map(|schedule| {
            let allowed = inputs.schedules.budget_for(&schedule.consumer).runs.get();
            let high = u64::from(schedule.usage.runs) * 100
                >= u64::from(allowed) * u64::from(policy.high_usage_percent);
            high.then(|| ScheduleRecord {
                consumer: schedule.consumer.clone(),
                signal: ScheduleSignal::HighUsage {
                    runs: schedule.usage.runs,
                    allowed,
                    complete: schedule.usage.complete,
                },
            })
        })
        .collect();
    schedules.extend(
        inputs
            .schedules
            .idle_schedules(evidence)
            .into_iter()
            .map(|idle| ScheduleRecord {
                consumer: idle.consumer,
                signal: ScheduleSignal::MostlyIdle {
                    runs: idle.runs,
                    idle_runs: idle.idle_runs,
                },
            }),
    );

    let mut candidates: BTreeMap<ProposalKey, Proposal> = BTreeMap::new();
    let mut add = |kind: ProposalKind, samples: u32, evidence: LinkedEvidence| {
        let key = ProposalKey::of(&kind);
        candidates.entry(key.clone()).or_insert(Proposal {
            key,
            kind,
            samples,
            evidence,
        });
    };
    for ((station, work_type), tally) in &tallies {
        let findings = u32::try_from(tally.findings.len()).unwrap_or(u32::MAX);
        if findings >= policy.repeated_findings.get() {
            add(
                ProposalKind::Guidance {
                    station: *station,
                    work_type: work_type.clone(),
                    findings,
                },
                tally.deliveries,
                publication.evidence(tally.findings.union(&tally.delivered)),
            );
        }
    }
    for divergence in &divergences {
        add(
            ProposalKind::Split {
                station: divergence.station,
                work_type: divergence.trailing.0.clone(),
            },
            divergence
                .leading
                .1
                .judged
                .saturating_add(divergence.trailing.1.judged),
            divergence.evidence.clone(),
        );
    }
    for schedule in &schedules {
        let (change, samples) = match schedule.signal {
            ScheduleSignal::HighUsage { runs, .. } => (ScheduleChange::RevisitBudget, runs),
            ScheduleSignal::MostlyIdle { runs, .. } => (ScheduleChange::ReduceIdleRuns, runs),
        };
        add(
            ProposalKind::Schedule {
                consumer: schedule.consumer.clone(),
                change,
            },
            samples,
            LinkedEvidence::consumer(&schedule.consumer),
        );
    }

    let (deduplicated, fresh): (Vec<Proposal>, Vec<Proposal>) = match inputs.open {
        Some(open) => candidates
            .into_values()
            .partition(|proposal| open.contains(&proposal.key)),
        None => (Vec::new(), candidates.into_values().collect()),
    };
    let keys = |proposals: Vec<Proposal>| -> Vec<ProposalKey> {
        proposals.into_iter().map(|proposal| proposal.key).collect()
    };
    let (proposals, deferred, withheld_proposals) = if withheld.is_empty() {
        let limit = usize::try_from(policy.max_proposals.get()).unwrap_or(MAX_PROPOSALS);
        let mut proposals = fresh;
        let deferred = proposals.split_off(limit.min(proposals.len()));
        (proposals, keys(deferred), Vec::new())
    } else {
        (Vec::new(), Vec::new(), keys(fresh))
    };
    let stations = tallies
        .into_iter()
        .map(|((station, work_type), tally)| StationRecord {
            station,
            work_type,
            deliveries: tally.deliveries,
            simulated: tally.simulated,
            first_pass: tally.first_pass,
            first_pass_rejected: publication.evidence(&tally.rejected),
            findings: publication.evidence(&tally.findings),
            deliveries_with_findings: publication.evidence(&tally.delivered),
            usage: tally.usage,
        })
        .collect();
    Ok(AuditReport {
        house: house.clone(),
        observed_at: evidence.observed_at,
        window: assessment.window,
        stations,
        incomplete_streams,
        unattributed_attempts,
        schedules,
        divergences,
        withheld,
        proposals,
        deduplicated: keys(deduplicated),
        deferred,
        withheld_proposals,
    })
}

fn tally<'a>(tallies: &'a mut Tallies, station: Role, work_type: &WorkType) -> &'a mut Tally {
    tallies.entry((station, work_type.clone())).or_default()
}

/// Fold the latest revision of every observation stream, and the confirmed
/// inspection samples of live deliveries, into `tallies`. Returns how many
/// streams have a revision gap and were left out.
fn read_ledger(ledger: &Ledger, tallies: &mut Tallies) -> Result<u32, TrustError> {
    ledger.read(|doc| {
        let streams: BTreeSet<&ExternalRef> = doc
            .observations
            .iter()
            .map(|observation| &observation.id)
            .collect();
        let mut incomplete = 0_u32;
        let mut live = BTreeMap::new();
        for stream in streams {
            let observation = match doc.latest(stream) {
                Ok(observation) => observation,
                Err(TrustError::Incomplete) => {
                    incomplete = incomplete.saturating_add(1);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let scope = &observation.attribution.scope;
            let entry = tally(tallies, scope.station, &scope.work_type);
            match observation.mode {
                EvidenceMode::Simulated => {
                    entry.simulated = entry.simulated.saturating_add(1);
                    continue;
                }
                EvidenceMode::Live => {}
            }
            entry.deliveries = entry.deliveries.saturating_add(1);
            live.insert(stream, (scope.station, scope.work_type.clone()));
            let Measurement::Observed { value: pr, .. } = &observation.pull_request else {
                continue;
            };
            if let Measurement::Observed { value, .. } = &pr.first_pass {
                entry.first_pass.judged = entry.first_pass.judged.saturating_add(1);
                if *value {
                    entry.first_pass.accepted = entry.first_pass.accepted.saturating_add(1);
                } else {
                    entry.rejected.insert(pr.source.clone());
                }
            }
            for list in [&pr.findings, &pr.reverts, &pr.regressions] {
                if let Measurement::Observed { value, .. } = list {
                    for finding in value {
                        entry.findings.insert(finding.source.clone());
                        entry.delivered.insert(pr.source.clone());
                    }
                }
            }
        }
        for inspection in &doc.inspections {
            let Some((station, work_type)) = live.get(&inspection.plan().observation) else {
                continue;
            };
            let entry = tally(tallies, *station, work_type);
            for sample in inspection.samples() {
                if let Some(SampleResult::Confirmed { finding, .. }) = &sample.result {
                    entry.findings.insert(finding.source.clone());
                }
            }
        }
        Ok(incomplete)
    })
}

/// Per station, the work types with enough verdicts whose acceptance is
/// furthest apart, when the gap reaches the policy's divergence.
fn divergences(
    policy: &AuditPolicy,
    tallies: &Tallies,
    publication: &Publication<'_>,
) -> Vec<Divergence> {
    let mut by_station: BTreeMap<Role, Vec<(&WorkType, &Tally, u64)>> = BTreeMap::new();
    for ((station, work_type), tally) in tallies {
        if tally.first_pass.judged < policy.min_samples.get() {
            continue;
        }
        if let Some(percent) = tally.first_pass.percent() {
            by_station
                .entry(*station)
                .or_default()
                .push((work_type, tally, percent));
        }
    }
    by_station
        .into_iter()
        .filter_map(|(station, types)| {
            let (leading, lead, high) = types.iter().max_by_key(|(_, _, percent)| *percent)?;
            let (trailing, trail, low) = types.iter().min_by_key(|(_, _, percent)| *percent)?;
            (high.saturating_sub(*low) >= u64::from(policy.divergence_points)).then(|| Divergence {
                station,
                leading: ((*leading).clone(), lead.first_pass),
                trailing: ((*trailing).clone(), trail.first_pass),
                evidence: publication.evidence(&trail.rejected),
            })
        })
        .collect()
}
