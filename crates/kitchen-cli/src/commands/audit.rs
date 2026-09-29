//! `kitchn audit`: preview the brigade audit of one house. Reads only.
//!
//! The report and its draft proposals come from
//! [`kitchen::workflows::audit`]. Nothing is filed, and no guidance, grant,
//! or schedule changes.
//!
//! Drafts are proposed only when the house budget and the open proposals
//! are both known. The budget is known from `--orca`: the house's bound
//! backend lists every house schedule. A `--schedule-evidence` file may omit
//! schedules, so with it the preview reports without proposing ("budget
//! unknown"). The open proposals are known when the operator states their
//! complete set, read from open forge issues with
//! [`kitchen::workflows::audit::proposal_key`]: each key with
//! `--open-proposal`, or `--no-open-proposals` when none is open. The
//! preview does not read the forge itself, and never takes a missing set as
//! empty.

use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf};

use clap::{ArgGroup, Args};
use kitchen::{
    HouseId,
    adapters::{
        OrcaSession,
        orca::{
            DEFAULT_CALL_TIMEOUT, DEFAULT_LAUNCH_TIMEOUT, DEFAULT_RESERVATION_TIMEOUT, SystemRunner,
        },
        resolve_backend,
    },
    adoption::{HouseRegistry, decode, encode},
    contracts::{Capability, ExternalRef},
    house::{ForgeError, HouseError, forge_binding},
    scheduling::{AgentFamily, ScheduleEvidence},
    state::{HouseStore, StoreOptions},
    trust::Ledger,
    workflows::{
        WorkflowError,
        audit::{
            self, AuditInputs, AuditPolicy, AuditReport, LinkedEvidence, ListedSchedules,
            ProposalKey, ProposalKind, Publication, ScheduleInventory, ScheduleSignal,
        },
    },
};

#[derive(Args)]
#[command(group(ArgGroup::new("inventory").required(true).args(["orca", "schedule_evidence"])))]
pub struct AuditArgs {
    /// The house registry holding the house configuration and its schedule
    /// policy.
    #[arg(long)]
    registry: PathBuf,
    #[arg(long)]
    house: HouseId,
    /// The house's initialized state store (default: the one `house init`
    /// created in the registry).
    #[arg(long)]
    store: Option<PathBuf>,
    /// Absolute path of the house's initialized trust ledger directory.
    #[arg(long)]
    ledger: PathBuf,
    /// Absolute path of the Orca executable: list every house schedule
    /// through the house's bound backend.
    #[arg(long, requires = "runtime_dir")]
    orca: Option<PathBuf>,
    /// House-scoped Orca runtime storage shared by every caller.
    #[arg(long, requires = "orca")]
    runtime_dir: Option<PathBuf>,
    /// Absolute path of a JSON file of observed schedule runs. It may omit
    /// schedules, so the preview reports without proposing.
    #[arg(long)]
    schedule_evidence: Option<PathBuf>,
    /// Key of a proposal still open on the forge; repeat for each, so that
    /// together they are every open proposal.
    #[arg(long = "open-proposal")]
    open: Vec<String>,
    /// No proposal is open on the forge.
    #[arg(long, conflicts_with = "open")]
    no_open_proposals: bool,
    /// Print the full report, with each draft's body, as JSON.
    #[arg(long)]
    json: bool,
}

pub fn run(args: AuditArgs) -> Result<(String, bool), kitchen::Error> {
    let absolute = |path: &Option<PathBuf>| path.as_ref().is_none_or(|path| path.is_absolute());
    if !args.ledger.is_absolute()
        || !absolute(&args.orca)
        || !absolute(&args.runtime_dir)
        || !absolute(&args.schedule_evidence)
    {
        return Err(HouseError::InvalidInput.into());
    }
    let registry = HouseRegistry::new(&args.registry)?;
    let config = registry.load(&args.house)?;
    // Without a policy there is no budget to spend the run against.
    let schedules = config
        .schedules
        .clone()
        .ok_or(WorkflowError::IncompleteEvidence)?;
    let forge = match forge_binding(&registry, &args.house) {
        Ok(binding) => Some(binding.forge),
        Err(ForgeError::MissingBinding { .. }) => None,
        Err(error) => return Err(error.into()),
    };
    let open = args
        .open
        .iter()
        .map(|key| ProposalKey::parse(key).ok_or(HouseError::InvalidInput))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let open = (args.no_open_proposals || !open.is_empty()).then_some(open);
    let store = HouseStore::open(
        super::house::store_or_default(args.store, Some(&args.registry), &args.house)?,
        args.house.clone(),
        StoreOptions::default(),
    )?;
    let ledger = Ledger::open(&args.ledger, args.house.clone())?;
    let listed;
    let file: ScheduleEvidence;
    let inventory = match (args.orca, args.runtime_dir, args.schedule_evidence) {
        (Some(orca), Some(runtime_dir), _) => {
            let backend = resolve_backend(
                &config,
                OrcaSession {
                    // Schedule calls name no Run, coordinator, or repository.
                    run: ExternalRef::new(audit::WORKFLOW)?,
                    coordinator: ExternalRef::new(audit::WORKFLOW)?,
                    repo: ExternalRef::new(audit::WORKFLOW)?,
                    base_branch: None,
                    branch_prefix: None,
                    agent: AgentFamily::Claude,
                    call_timeout: DEFAULT_CALL_TIMEOUT,
                    launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
                    runtime_dir,
                    reservation_timeout: DEFAULT_RESERVATION_TIMEOUT,
                },
                SystemRunner::new(&orca),
                &[Capability::ScheduleManage],
            )?;
            listed = ListedSchedules::read(&backend)?;
            ScheduleInventory::Listed(&listed)
        }
        (_, _, Some(path)) => {
            file = decode(&path)?;
            ScheduleInventory::Unproven(&file)
        }
        // Clap requires one source, and `--orca` with `--runtime-dir`.
        _ => return Err(HouseError::InvalidInput.into()),
    };
    let report = audit::audit(
        &AuditPolicy::default(),
        &AuditInputs {
            house: &args.house,
            ledger: &ledger,
            store: &store,
            schedules: &schedules,
            inventory,
            open: open.as_ref(),
            publication: Publication {
                forge,
                destinations: &config.posting_destinations,
            },
        },
    )?;
    let text = if args.json {
        json_text(&report)?
    } else {
        report_text(&report)
    };
    Ok((text, true))
}

/// The report and every draft body, for `--json`.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonOutput<'a> {
    report: &'a AuditReport,
    drafts: Vec<String>,
}

fn json_text(report: &AuditReport) -> Result<String, kitchen::Error> {
    let drafts = report
        .proposals
        .iter()
        .map(|proposal| Ok(proposal.draft()?.as_str().to_owned()))
        .collect::<Result<_, kitchen::Error>>()?;
    let output = JsonOutput { report, drafts };
    Ok(String::from_utf8(encode(&output)?).map_err(|_| HouseError::InvalidInput)?)
}

/// `label` with its source count, then each listed link, how many more
/// there are, and how many private sources were left out. Nothing for no
/// sources.
fn evidence_text(text: &mut String, indent: &str, label: &str, evidence: &LinkedEvidence) {
    if evidence.total == 0 {
        return;
    }
    let _ = writeln!(text, "{indent}{label}: {}", evidence.total);
    for link in &evidence.links {
        let _ = writeln!(text, "{indent}  - {link}");
    }
    if evidence.unlisted > 0 {
        let _ = writeln!(text, "{indent}  - and {} more", evidence.unlisted);
    }
    if evidence.private > 0 {
        let _ = writeln!(
            text,
            "{indent}  - {} private, kept in the house and not listed",
            evidence.private
        );
    }
}

fn report_text(report: &AuditReport) -> String {
    let mut text = format!(
        "Brigade audit of {} (preview; nothing is filed or changed)\n",
        report.house
    );
    for reason in &report.withheld {
        let _ = writeln!(text, "No drafts proposed: {reason}.");
    }
    text.push_str("\nStations:\n");
    if report.stations.is_empty() {
        text.push_str("  none recorded\n");
    }
    for station in &report.stations {
        let _ = writeln!(
            text,
            "  {} / {}: {} live and {} simulated deliveries; first pass {} of {}; {} confirmed findings; {} attempts, {} with usage",
            station.station,
            station.work_type,
            station.deliveries,
            station.simulated,
            station.first_pass.accepted,
            station.first_pass.judged,
            station.findings.total,
            station.usage.attempts,
            station.usage.reported,
        );
        evidence_text(&mut text, "    ", "findings", &station.findings);
        evidence_text(
            &mut text,
            "    ",
            "deliveries with findings",
            &station.deliveries_with_findings,
        );
        evidence_text(
            &mut text,
            "    ",
            "rejected on the first pass",
            &station.first_pass_rejected,
        );
    }
    if report.incomplete_streams > 0 {
        let _ = writeln!(
            text,
            "  {} observation streams have revision gaps and were left out",
            report.incomplete_streams
        );
    }
    if report.unattributed_attempts > 0 {
        let _ = writeln!(
            text,
            "  {} attempts have no work type",
            report.unattributed_attempts
        );
    }
    text.push_str("\nSchedules:\n");
    if report.schedules.is_empty() {
        text.push_str("  none with high usage or mostly idle runs\n");
    }
    for schedule in &report.schedules {
        let _ = match schedule.signal {
            ScheduleSignal::HighUsage {
                runs,
                allowed,
                complete,
            } => writeln!(
                text,
                "  {}: {}{runs} of {allowed} runs this window",
                schedule.consumer,
                if complete { "" } else { "at least " },
            ),
            ScheduleSignal::MostlyIdle { runs, idle_runs } => writeln!(
                text,
                "  {}: {idle_runs} of {runs} recent runs idle",
                schedule.consumer,
            ),
        };
    }
    for divergence in &report.divergences {
        let _ = writeln!(
            text,
            "\nDiverged: {} first pass {} of {} on {}, {} of {} on {}",
            divergence.station,
            divergence.leading.1.accepted,
            divergence.leading.1.judged,
            divergence.leading.0,
            divergence.trailing.1.accepted,
            divergence.trailing.1.judged,
            divergence.trailing.0,
        );
        evidence_text(
            &mut text,
            "  ",
            "rejected on the first pass",
            &divergence.evidence,
        );
    }
    text.push_str("\nDraft proposals:\n");
    if report.proposals.is_empty() {
        text.push_str("  none\n");
    }
    for proposal in &report.proposals {
        let what = match &proposal.kind {
            ProposalKind::Guidance { .. } => "guidance change",
            ProposalKind::Split { .. } => "work type or role split",
            ProposalKind::Schedule { .. } => "schedule change",
        };
        let _ = writeln!(
            text,
            "  {}: {what}, {} samples",
            proposal.key, proposal.samples
        );
        evidence_text(&mut text, "    ", "evidence", &proposal.evidence);
    }
    for key in &report.deduplicated {
        let _ = writeln!(text, "  {key}: already open");
    }
    for key in &report.deferred {
        let _ = writeln!(text, "  {key}: deferred past this run's limit");
    }
    for key in &report.withheld_proposals {
        let _ = writeln!(text, "  {key}: withheld");
    }
    text
}

#[cfg(test)]
mod tests {
    use kitchen::{
        contracts::{Role, Timestamp},
        scheduling::UsageWindow,
        selection::WorkType,
        workflows::audit::{Acceptance, Divergence, EvidenceLink, StationRecord, UsageSummary},
    };

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn link(n: u32) -> Result<EvidenceLink, kitchen::contracts::ContractError> {
        Ok(EvidenceLink::Forge(ExternalRef::new(&format!(
            "https://github.com/example/project/pull/{n}"
        ))?))
    }

    fn report(evidence: &LinkedEvidence) -> Result<AuditReport, Box<dyn std::error::Error>> {
        let first_pass = Acceptance {
            accepted: 1,
            judged: 5,
        };
        Ok(AuditReport {
            house: HouseId::new("origin89")?,
            observed_at: Timestamp::from_unix_millis(0),
            window: UsageWindow {
                start: Timestamp::from_unix_millis(0),
                end: Timestamp::from_unix_millis(1),
            },
            stations: vec![StationRecord {
                station: Role::StationCook,
                work_type: WorkType::new("docs")?,
                deliveries: 5,
                simulated: 0,
                first_pass,
                first_pass_rejected: evidence.clone(),
                findings: evidence.clone(),
                deliveries_with_findings: LinkedEvidence::default(),
                usage: UsageSummary::default(),
            }],
            incomplete_streams: 0,
            unattributed_attempts: 0,
            schedules: Vec::new(),
            divergences: vec![Divergence {
                station: Role::StationCook,
                leading: (WorkType::new("implementation")?, first_pass),
                trailing: (WorkType::new("docs")?, first_pass),
                evidence: evidence.clone(),
            }],
            withheld: Vec::new(),
            proposals: Vec::new(),
            deduplicated: Vec::new(),
            deferred: Vec::new(),
            withheld_proposals: Vec::new(),
        })
    }

    #[test]
    fn the_text_report_lists_bounded_evidence_with_counts() -> TestResult {
        let evidence = LinkedEvidence {
            total: 6,
            links: vec![link(1)?, link(2)?],
            unlisted: 3,
            private: 1,
        };
        let text = report_text(&report(&evidence)?);
        let listing = "6\n      - https://github.com/example/project/pull/1\n      - https://github.com/example/project/pull/2\n      - and 3 more\n      - 1 private, kept in the house and not listed\n";
        assert!(text.contains(&format!("    findings: {listing}")), "{text}");
        assert!(
            text.contains(&format!("    rejected on the first pass: {listing}")),
            "{text}"
        );
        assert!(
            text.contains(
                "  rejected on the first pass: 6\n    - https://github.com/example/project/pull/1\n"
            ),
            "{text}"
        );
        // A record without sources lists nothing for them.
        assert!(!text.contains("deliveries with findings"), "{text}");
        Ok(())
    }

    #[test]
    fn a_record_without_sources_prints_only_its_counts() -> TestResult {
        let text = report_text(&report(&LinkedEvidence::default())?);
        assert!(text.contains("1 of 5; 0 confirmed findings"), "{text}");
        assert!(
            !text.contains("findings:") && !text.contains("rejected on"),
            "{text}"
        );
        Ok(())
    }
}
