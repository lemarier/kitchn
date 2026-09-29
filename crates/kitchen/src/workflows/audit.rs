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
//! draft carries a hidden [`marker`] naming its [`ProposalKey`]; the caller
//! reads the keys of open proposals back with [`proposal_key`] and passes
//! them in, and the audit proposes nothing an open proposal already covers.
//!
//! A run spends the house usage budget like any scheduled run: it is refused
//! while that budget is exhausted, or when the schedule evidence cannot show
//! that budget remains. At most [`AuditPolicy::max_proposals`] proposals come
//! out of one run; the rest are listed as deferred.
//!
//! The report copies no free text from its inputs: finding consequences,
//! task specifications, and transcripts stay in the house. It holds typed
//! names, counts, and source references only.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    num::NonZeroU32,
};

use serde::Serialize;

use crate::{
    ConsumerId, ErrorClass, HouseId,
    contracts::{ExternalRef, ResourceRef, Role, Text, Timestamp},
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
/// Most evidence links listed in one draft; the rest are counted.
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
    /// The schedule evidence does not reach back to its window's start, so
    /// it cannot show that the house budget remains.
    #[error("the house budget assessment is incomplete")]
    IncompleteBudget,
}

impl AuditError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidPolicy => ErrorClass::InvalidInput,
            Self::CrossHouse | Self::BudgetExhausted(_) | Self::IncompleteBudget => {
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
    /// Observed runs of every house schedule; its time is the audit's time.
    pub evidence: &'a ScheduleEvidence,
    /// Keys of proposals still open on the forge.
    pub open: &'a BTreeSet<ProposalKey>,
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
    pub first_pass_rejected: BTreeSet<ExternalRef>,
    /// Sources of the distinct confirmed findings, reverts, and regressions,
    /// including confirmed inspection samples.
    pub findings: BTreeSet<ExternalRef>,
    /// Pull requests of the live deliveries those findings concern.
    pub deliveries_with_findings: BTreeSet<ExternalRef>,
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

/// A schedule the report lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRecord {
    /// The consumer scope it serves.
    pub consumer: ConsumerId,
    /// The backend resource: the evidence link.
    pub schedule: ResourceRef,
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
    pub evidence: Vec<ExternalRef>,
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
    /// Evidence links.
    pub evidence: Vec<ExternalRef>,
}

impl Proposal {
    /// The draft issue body: its marker, what it proposes, the sample size,
    /// and at most [`MAX_LISTED`] evidence links.
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
        for link in self.evidence.iter().take(MAX_LISTED) {
            let _ = writeln!(body, "- {link}");
        }
        if let Some(rest) = self
            .evidence
            .len()
            .checked_sub(MAX_LISTED)
            .filter(|n| *n > 0)
        {
            let _ = writeln!(body, "- and {rest} more");
        }
        Ok(Text::new(&body)?)
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
    /// Draft proposals from this run.
    pub proposals: Vec<Proposal>,
    /// Proposals left out because an open proposal already covers them.
    pub deduplicated: Vec<ProposalKey>,
    /// Proposals beyond this run's limit, for a later run.
    pub deferred: Vec<ProposalKey>,
}

/// Run the audit for `inputs.house`. Reads only.
///
/// # Errors
/// [`AuditError::CrossHouse`] when any source belongs to another house,
/// [`AuditError::BudgetExhausted`] or [`AuditError::IncompleteBudget`] when
/// the run cannot spend the house budget, [`AuditError::InvalidPolicy`],
/// and any schedule-evidence, ledger, or store read failure.
pub fn audit(policy: &AuditPolicy, inputs: &AuditInputs<'_>) -> crate::Result<AuditReport> {
    policy.validate()?;
    let house = inputs.house;
    if inputs.ledger.house() != house
        || inputs.store.house() != house
        || &inputs.evidence.house != house
    {
        return Err(AuditError::CrossHouse.into());
    }
    let assessment = inputs.schedules.assess(house, inputs.evidence)?;
    if let Some(exhausted) = assessment.house_exhausted {
        return Err(AuditError::BudgetExhausted(exhausted).into());
    }
    if !assessment.house.complete {
        return Err(AuditError::IncompleteBudget.into());
    }

    let mut stations: BTreeMap<(Role, WorkType), StationRecord> = BTreeMap::new();
    let incomplete_streams = read_ledger(inputs.ledger, &mut stations)?;
    let mut unattributed_attempts = 0_u32;
    for entry in inputs.store.attempt_usage()? {
        let Some(work_type) = entry.work_type else {
            unattributed_attempts = unattributed_attempts.saturating_add(1);
            continue;
        };
        let usage = &mut record(&mut stations, entry.station, &work_type).usage;
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
    let stations: Vec<StationRecord> = stations.into_values().collect();

    let mut schedules: Vec<ScheduleRecord> = assessment
        .schedules
        .iter()
        .filter_map(|schedule| {
            let allowed = inputs.schedules.budget_for(&schedule.consumer).runs.get();
            let high = u64::from(schedule.usage.runs) * 100
                >= u64::from(allowed) * u64::from(policy.high_usage_percent);
            high.then(|| ScheduleRecord {
                consumer: schedule.consumer.clone(),
                schedule: schedule.schedule.clone(),
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
            .idle_schedules(inputs.evidence)
            .into_iter()
            .map(|idle| ScheduleRecord {
                consumer: idle.consumer,
                schedule: idle.schedule,
                signal: ScheduleSignal::MostlyIdle {
                    runs: idle.runs,
                    idle_runs: idle.idle_runs,
                },
            }),
    );
    let divergences = divergences(policy, &stations);

    let mut candidates: BTreeMap<ProposalKey, Proposal> = BTreeMap::new();
    let mut add = |kind: ProposalKind, samples: u32, evidence: Vec<ExternalRef>| {
        let key = ProposalKey::of(&kind);
        candidates.entry(key.clone()).or_insert(Proposal {
            key,
            kind,
            samples,
            evidence,
        });
    };
    for station in &stations {
        let findings = u32::try_from(station.findings.len()).unwrap_or(u32::MAX);
        if findings >= policy.repeated_findings.get() {
            let evidence = station
                .findings
                .iter()
                .chain(&station.deliveries_with_findings)
                .cloned()
                .collect();
            add(
                ProposalKind::Guidance {
                    station: station.station,
                    work_type: station.work_type.clone(),
                    findings,
                },
                station.deliveries,
                evidence,
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
            vec![schedule.schedule.handle.clone()],
        );
    }

    let limit = usize::try_from(policy.max_proposals.get()).unwrap_or(MAX_PROPOSALS);
    let (deduplicated, fresh): (Vec<Proposal>, Vec<Proposal>) = candidates
        .into_values()
        .partition(|proposal| inputs.open.contains(&proposal.key));
    let mut proposals = fresh;
    let deferred = proposals
        .split_off(limit.min(proposals.len()))
        .into_iter()
        .map(|proposal| proposal.key)
        .collect();
    Ok(AuditReport {
        house: house.clone(),
        observed_at: inputs.evidence.observed_at,
        window: assessment.window,
        stations,
        incomplete_streams,
        unattributed_attempts,
        schedules,
        divergences,
        proposals,
        deduplicated: deduplicated
            .into_iter()
            .map(|proposal| proposal.key)
            .collect(),
        deferred,
    })
}

fn record<'a>(
    stations: &'a mut BTreeMap<(Role, WorkType), StationRecord>,
    station: Role,
    work_type: &WorkType,
) -> &'a mut StationRecord {
    stations
        .entry((station, work_type.clone()))
        .or_insert_with(|| StationRecord {
            station,
            work_type: work_type.clone(),
            deliveries: 0,
            simulated: 0,
            first_pass: Acceptance::default(),
            first_pass_rejected: BTreeSet::new(),
            findings: BTreeSet::new(),
            deliveries_with_findings: BTreeSet::new(),
            usage: UsageSummary::default(),
        })
}

/// Fold the latest revision of every observation stream, and the confirmed
/// inspection samples of live deliveries, into `stations`. Returns how many
/// streams have a revision gap and were left out.
fn read_ledger(
    ledger: &Ledger,
    stations: &mut BTreeMap<(Role, WorkType), StationRecord>,
) -> Result<u32, TrustError> {
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
            let entry = record(stations, scope.station, &scope.work_type);
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
                    entry.first_pass_rejected.insert(pr.source.clone());
                }
            }
            for list in [&pr.findings, &pr.reverts, &pr.regressions] {
                if let Measurement::Observed { value, .. } = list {
                    for finding in value {
                        entry.findings.insert(finding.source.clone());
                        entry.deliveries_with_findings.insert(pr.source.clone());
                    }
                }
            }
        }
        for inspection in &doc.inspections {
            let Some((station, work_type)) = live.get(&inspection.plan().observation) else {
                continue;
            };
            let entry = record(stations, *station, work_type);
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
fn divergences(policy: &AuditPolicy, stations: &[StationRecord]) -> Vec<Divergence> {
    let mut by_station: BTreeMap<Role, Vec<(&StationRecord, u64)>> = BTreeMap::new();
    for record in stations {
        if record.first_pass.judged < policy.min_samples.get() {
            continue;
        }
        if let Some(percent) = record.first_pass.percent() {
            by_station
                .entry(record.station)
                .or_default()
                .push((record, percent));
        }
    }
    by_station
        .into_iter()
        .filter_map(|(station, types)| {
            let (leading, high) = types.iter().max_by_key(|(_, percent)| *percent)?;
            let (trailing, low) = types.iter().min_by_key(|(_, percent)| *percent)?;
            (high.saturating_sub(*low) >= u64::from(policy.divergence_points)).then(|| Divergence {
                station,
                leading: (leading.work_type.clone(), leading.first_pass),
                trailing: (trailing.work_type.clone(), trailing.first_pass),
                evidence: trailing.first_pass_rejected.iter().cloned().collect(),
            })
        })
        .collect()
}
