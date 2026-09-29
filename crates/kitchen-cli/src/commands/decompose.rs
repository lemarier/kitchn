//! `kitchen decompose preview`: validate a proposed project decomposition
//! and show the exact issues, owned paths, acceptance criteria, blocked-by
//! edges, and ownership overlaps a person approves, with the digest that
//! binds the approval. Reads only the proposal file; writes nothing and
//! contacts no forge. Writing happens through the library's
//! `workflows::decomposition::apply` with a person's approval of that digest.
//!
//! `kitchen decompose acknowledge` releases a repository that an earlier
//! decomposition still holds after settling without success. It opens the
//! house store and records the acknowledgement; the CLI holds no forge
//! credential, so it cannot re-read the forge and says so.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use kitchen::{
    HolderId, HouseId, TaskId,
    adoption::{decode, encode},
    contracts::{Claimant, SystemClock, Text},
    house::HouseError,
    state::{HouseStore, StoreOptions},
    workflows::decomposition::{AcknowledgeReport, Proposal, acknowledge, preview},
};

#[derive(Args)]
pub struct DecomposeArgs {
    #[command(subcommand)]
    command: DecomposeCommand,
}

#[derive(Subcommand)]
enum DecomposeCommand {
    /// Show the preview and its digest. Exits 0 when it can be approved, 1
    /// while ownership overlaps are unordered, and 2 for an invalid proposal
    /// such as a dependency cycle.
    Preview {
        /// The proposal (JSON).
        #[arg(long)]
        proposal: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Release the repository from an earlier decomposition that settled
    /// without success after writing, or possibly writing, to the forge.
    /// Records who acknowledged, when, and why. Run it only after checking
    /// the forge for that task's issues: this command cannot re-read the
    /// forge, so every write not already proven applied is recorded as
    /// unproven. Nothing is released without it, and a scheduled run cannot
    /// do it.
    Acknowledge {
        /// The house's initialized state store.
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        house: HouseId,
        /// The settled decomposition task, as `apply` reported it.
        #[arg(long)]
        task: TaskId,
        /// Your session, recorded as who acknowledged.
        #[arg(long)]
        holder: HolderId,
        /// Why you are content to proceed. Recorded with the acknowledgement.
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: DecomposeArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        DecomposeCommand::Preview { proposal, json } => {
            let proposal: Proposal = decode(&proposal)?;
            let preview = preview(&proposal)?;
            let output = if json {
                String::from_utf8(encode(&preview)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                preview.render()
            };
            Ok((output, preview.ready()))
        }
        DecomposeCommand::Acknowledge {
            store,
            house,
            task,
            holder,
            reason,
            json,
        } => {
            let store = HouseStore::open(store, house, StoreOptions::default())?;
            let report = acknowledge(
                &store,
                None,
                &task,
                &Claimant::interactive(holder),
                &Text::new(&reason)?,
                &SystemClock,
            )?;
            let output = if json {
                String::from_utf8(encode(&report)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                render_acknowledgement(&report)
            };
            Ok((output, true))
        }
    }
}

fn render_acknowledgement(report: &AcknowledgeReport) -> String {
    let mut text = if report.already_acknowledged {
        format!(
            "Task {} was already acknowledged; nothing changed.",
            report.task
        )
    } else {
        format!(
            "Acknowledged the writes of task {} ({}). The repository is released for a new decomposition.",
            report.task, report.settlement
        )
    };
    if !report.already_acknowledged {
        text.push_str("\nThe forge was not re-read.");
    }
    for name in &report.unresolved {
        text.push_str("\nunproven write accepted: ");
        text.push_str(name.as_str());
    }
    text
}
