//! `kitchn trust`: how full the house trust ledger is, and its archival.
//!
//! Archival previews by default. With --apply it moves the observations,
//! bindings, and inspections no grant needs into the ledger directory's
//! archive file, in one ledger transaction; grants and their evidence stay.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    adoption::encode,
    contracts::{Clock, SystemClock},
    house::HouseError,
    trust::{Archival, ArchiveReport, Capacity, Ledger},
};
use serde::Serialize;

#[derive(Args)]
pub struct TrustArgs {
    #[command(subcommand)]
    command: TrustCommand,
}

#[derive(Subcommand)]
enum TrustCommand {
    /// Report how full the trust ledger is and what was archived. Reads only.
    Capacity(LedgerArgs),
    /// Preview the settled observations, their bindings, and the finished
    /// inspections no grant needs; with --apply, archive them.
    Archive {
        #[command(flatten)]
        ledger: LedgerArgs,
        /// Archive what the preview lists. Without it nothing is written.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Args)]
struct LedgerArgs {
    #[arg(long)]
    house: HouseId,
    /// Absolute path of the house's initialized trust ledger directory.
    #[arg(long)]
    ledger: PathBuf,
    #[arg(long)]
    json: bool,
}

/// What `capacity` reports.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityOutput {
    capacity: Capacity,
    archivals: Vec<Archival>,
}

/// What `archive` reports.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveOutput {
    applied: bool,
    archive: ArchiveReport,
    capacity: Capacity,
}

pub fn run(args: TrustArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        TrustCommand::Capacity(args) => {
            let ledger = open(&args.ledger, args.house)?;
            let output = CapacityOutput {
                capacity: ledger.capacity()?,
                archivals: ledger.archivals()?,
            };
            let healthy = !output.capacity.near_limit();
            let text = if args.json {
                json_text(&output)?
            } else {
                capacity_text(&output.capacity, &output.archivals)
            };
            Ok((text, healthy))
        }
        TrustCommand::Archive {
            ledger: args,
            apply,
        } => {
            let ledger = open(&args.ledger, args.house)?;
            let now = SystemClock.now();
            let archive = if apply {
                ledger.archive(now)?
            } else {
                ledger.preview_archive(now)?
            };
            let output = ArchiveOutput {
                applied: apply,
                archive,
                capacity: ledger.capacity()?,
            };
            let text = if args.json {
                json_text(&output)?
            } else {
                archive_text(&output, &ledger.archivals()?)
            };
            Ok((text, true))
        }
    }
}

fn open(ledger: &Path, house: HouseId) -> Result<Ledger, kitchen::Error> {
    if !ledger.is_absolute() {
        return Err(HouseError::InvalidInput.into());
    }
    Ok(Ledger::open(ledger, house)?)
}

fn capacity_text(capacity: &Capacity, archivals: &[Archival]) -> String {
    let mut text = format!(
        "Entries: {} of {}\nBytes: {} of {}\n",
        capacity.entries, capacity.max_entries, capacity.bytes, capacity.max_bytes,
    );
    if capacity.near_limit() {
        text.push_str(
            "Near the limit: ordinary writes stop when either limit is reached. Run `kitchn trust archive` to preview what can leave.\n",
        );
    }
    // A loaded ledger has already refused counts that overflow.
    let records = archivals
        .iter()
        .filter_map(Archival::records)
        .fold(0_usize, usize::saturating_add);
    let _ = writeln!(
        text,
        "Archived: {records} record(s) in {} archival(s)",
        archivals.len()
    );
    for archival in archivals {
        let _ = writeln!(
            text,
            "  {} at {} ms: {} observation(s), {} binding(s), {} inspection(s)",
            archival.digest,
            archival.at.as_unix_millis(),
            archival.observations,
            archival.bindings,
            archival.inspections,
        );
    }
    text
}

fn archive_text(output: &ArchiveOutput, archivals: &[Archival]) -> String {
    let archive = &output.archive;
    let revisions: usize = archive.streams.iter().map(|s| s.revisions).sum();
    let bindings = archive.streams.iter().filter(|s| s.binding).count();
    let mut text = format!(
        "{} {} observation stream(s) ({revisions} revision(s)), {bindings} binding(s), and {} inspection(s).\n",
        if output.applied {
            "Archived"
        } else {
            "Would archive"
        },
        archive.streams.len(),
        archive.inspections.len(),
    );
    for stream in &archive.streams {
        let _ = writeln!(
            text,
            "  stream {} (task {}): {} revision(s){}",
            stream.id,
            stream.task,
            stream.revisions,
            if stream.binding { ", with binding" } else { "" }
        );
    }
    for inspection in &archive.inspections {
        let _ = writeln!(text, "  inspection {inspection}");
    }
    let kept = archive.kept;
    let _ = writeln!(
        text,
        "Kept: {} grant audit(s), {} stream(s) a grant cites, {} stream(s) under inspection, {} binding(s) without a recorded stream, {} open inspection(s).",
        kept.grant_audits,
        kept.streams_cited_by_grants,
        kept.streams_under_inspection,
        kept.unobserved_bindings,
        kept.open_inspections,
    );
    if let Some(archival) = &archive.archival {
        let _ = writeln!(text, "Archive digest: {}", archival.digest);
    }
    if !output.applied && !archive.is_empty() {
        text.push_str("Preview only; rerun with --apply to archive them.\n");
    }
    text.push('\n');
    text.push_str(&capacity_text(&output.capacity, archivals));
    text
}

fn json_text(value: &impl Serialize) -> Result<String, kitchen::Error> {
    Ok(String::from_utf8(encode(value)?).map_err(|_| HouseError::InvalidInput)?)
}
