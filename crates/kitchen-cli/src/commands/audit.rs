//! `kitchn audit`: preview the brigade audit of one house. Reads only.
//!
//! The report and its draft proposals come from
//! [`kitchen::workflows::audit`]. Nothing is filed, and no guidance, grant,
//! or schedule changes. Schedule evidence is the house's observed schedule
//! runs as JSON (a [`ScheduleEvidence`]); open proposals are the keys read
//! back from open issues with [`kitchen::workflows::audit::proposal_key`].

use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf};

use clap::Args;
use kitchen::{
    HouseId,
    adoption::{HouseRegistry, decode, encode},
    house::HouseError,
    scheduling::ScheduleEvidence,
    state::{HouseStore, StoreOptions},
    trust::Ledger,
    workflows::{
        WorkflowError,
        audit::{
            AuditInputs, AuditPolicy, AuditReport, ProposalKey, ProposalKind, ScheduleSignal, audit,
        },
    },
};

#[derive(Args)]
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
    /// JSON file of the house's observed schedule runs.
    #[arg(long)]
    schedule_evidence: PathBuf,
    /// Key of a proposal still open on the forge; repeat for each.
    #[arg(long = "open-proposal")]
    open: Vec<String>,
    /// Print the full report, with each draft's body, as JSON.
    #[arg(long)]
    json: bool,
}

pub fn run(args: AuditArgs) -> Result<(String, bool), kitchen::Error> {
    if !args.ledger.is_absolute() || !args.schedule_evidence.is_absolute() {
        return Err(HouseError::InvalidInput.into());
    }
    let config = HouseRegistry::new(&args.registry)?.load(&args.house)?;
    // Without a policy there is no budget to spend the run against.
    let schedules = config.schedules.ok_or(WorkflowError::IncompleteEvidence)?;
    let store = HouseStore::open(
        super::house::store_or_default(args.store, Some(&args.registry), &args.house)?,
        args.house.clone(),
        StoreOptions::default(),
    )?;
    let ledger = Ledger::open(&args.ledger, args.house.clone())?;
    let evidence: ScheduleEvidence = decode(&args.schedule_evidence)?;
    let open = args
        .open
        .iter()
        .map(|key| ProposalKey::parse(key).ok_or(HouseError::InvalidInput))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let report = audit(
        &AuditPolicy::default(),
        &AuditInputs {
            house: &args.house,
            ledger: &ledger,
            store: &store,
            schedules: &schedules,
            evidence: &evidence,
            open: &open,
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

fn report_text(report: &AuditReport) -> String {
    let mut text = format!(
        "Brigade audit of {} (preview; nothing is filed or changed)\n",
        report.house
    );
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
            station.findings.len(),
            station.usage.attempts,
            station.usage.reported,
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
                "  {}: {}{runs} of {allowed} runs this window ({})",
                schedule.consumer,
                if complete { "" } else { "at least " },
                schedule.schedule.handle,
            ),
            ScheduleSignal::MostlyIdle { runs, idle_runs } => writeln!(
                text,
                "  {}: {idle_runs} of {runs} recent runs idle ({})",
                schedule.consumer, schedule.schedule.handle,
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
            "  {}: {what}, {} samples, {} evidence links",
            proposal.key,
            proposal.samples,
            proposal.evidence.len()
        );
    }
    for key in &report.deduplicated {
        let _ = writeln!(text, "  {key}: already open");
    }
    for key in &report.deferred {
        let _ = writeln!(text, "  {key}: deferred past this run's limit");
    }
    text
}
