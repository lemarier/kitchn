//! `kitchn decompose preview`: validate a proposed project decomposition
//! and show the exact issues, owned paths, acceptance criteria, blocked-by
//! edges, and ownership overlaps a person approves, with the digest that
//! binds the approval. Reads only the proposal file; writes nothing and
//! contacts no forge.
//!
//! `kitchen decompose apply --approve <digest>` writes that exact preview
//! with the house's forge binding, as the person present. A rerun resumes an
//! interrupted write without duplicating issues or links; a changed proposal
//! needs a new approval.
//!
//! `kitchn decompose acknowledge` releases a repository that an earlier
//! decomposition still holds after settling without success. Given the
//! registry of a house with a forge binding, it first re-reads the forge for
//! writes whose outcome is unknown; then it records the acknowledgement.

use std::{fmt::Write as _, path::PathBuf, time::Duration};

use clap::{Args, Subcommand};
use kitchen::{
    HolderId, HouseId, TaskId,
    adoption::{HouseRegistry, decode, encode},
    contracts::{Claimant, LeaseTtl, Provenance, Settlement, SystemClock, Text},
    house::{HouseError, apply_approved},
    state::{HouseStore, StoreOptions},
    workflows::decomposition::{
        AcknowledgeReport, ApplyOptions, ApplyOutcome, ApplyReport, ApprovedDecomposition, Blocker,
        PreviewDigest, Proposal, WriteKind, acknowledge, preview,
    },
};

use super::forge::{Reread, connect_gh, not_applied};

/// Lease on the decomposition task while one `apply` call writes.
const APPLY_LEASE: Duration = Duration::from_secs(15 * 60);

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
    /// Write the approved preview to the forge with the house's forge
    /// binding, as the person present. Exits 0 once every write is applied,
    /// and 1 when the run stopped early; rerun the same command to resume.
    Apply {
        /// The proposal (JSON) whose preview you approved.
        #[arg(long)]
        proposal: PathBuf,
        /// The digest of the preview you approved.
        #[arg(long)]
        approve: PreviewDigest,
        /// The house registry holding the forge binding.
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        house: HouseId,
        /// The house's initialized state store.
        #[arg(long)]
        store: PathBuf,
        /// You, the person approving.
        #[arg(long)]
        holder: HolderId,
        #[arg(long)]
        json: bool,
    },
    /// Release the repository from an earlier decomposition that settled
    /// without success after writing, or possibly writing, to the forge.
    /// Records who acknowledged, when, and why. With --registry and a house
    /// forge binding it first re-reads the forge for writes whose outcome is
    /// unknown. A write still not proven, including any when the forge is not
    /// read, refuses the release unless --accept-unknown is given; then it is
    /// recorded as unproven. Check the forge for that task's issues first.
    /// Nothing is released without it, and a scheduled run cannot do it.
    Acknowledge {
        /// The house's initialized state store.
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        house: HouseId,
        /// The house registry; its forge binding is used to re-read the
        /// forge. Without it nothing is re-read.
        #[arg(long)]
        registry: Option<PathBuf>,
        /// Do not re-read the forge even though the house has a binding.
        #[arg(long)]
        without_forge: bool,
        /// The settled decomposition task, as `apply` reported it.
        #[arg(long)]
        task: TaskId,
        /// Your session, recorded as who acknowledged.
        #[arg(long)]
        holder: HolderId,
        /// Why you are content to proceed. Recorded with the acknowledgement.
        #[arg(long)]
        reason: String,
        /// Release the repository even though a write's outcome is still
        /// unknown.
        #[arg(long)]
        accept_unknown: bool,
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
        DecomposeCommand::Apply {
            proposal,
            approve,
            registry,
            house,
            store,
            holder,
            json,
        } => {
            let proposal: Proposal = decode(&proposal)?;
            let registry = HouseRegistry::new(super::house::canonical_root(registry)?)?;
            let config = registry.load(&house)?;
            let store = HouseStore::open(store, house.clone(), StoreOptions::default())?;
            let grants = config.authority()?;
            let options = ApplyOptions {
                provenance: Provenance {
                    kitchen: config.kitchen.clone(),
                    house_guidance: config.guidance.clone(),
                    repository_instructions: None,
                },
                lease: LeaseTtl::new(APPLY_LEASE)?,
            };
            let write = ApprovedDecomposition {
                proposal: &proposal,
                store: &store,
                grants: &grants,
                clock: &SystemClock,
                options: &options,
            };
            let report = apply_approved(
                &registry,
                &house,
                &write,
                &approve,
                &Claimant::interactive(holder),
                connect_gh,
            )?;
            let done = matches!(
                report.outcome,
                ApplyOutcome::Completed | ApplyOutcome::Settled(Settlement::Succeeded)
            );
            let output = if json {
                String::from_utf8(encode(&report)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                render_apply(&report)
            };
            Ok((output, done))
        }
        DecomposeCommand::Acknowledge {
            store,
            house,
            registry,
            without_forge,
            task,
            holder,
            reason,
            accept_unknown,
            json,
        } => {
            let reread = match registry {
                Some(registry) => Reread::open(
                    &HouseRegistry::new(super::house::canonical_root(registry)?)?,
                    &house,
                    without_forge,
                )?,
                None => Reread::NoRegistry,
            };
            let store = HouseStore::open(store, house.clone(), StoreOptions::default())?;
            let report = acknowledge(
                &store,
                reread.executor(),
                &task,
                &Claimant::interactive(holder),
                &Text::new(&reason)?,
                accept_unknown,
                &SystemClock,
            )?;
            let output = if json {
                String::from_utf8(encode(&report)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                render_acknowledgement(&report, &reread, &house)
            };
            Ok((output, true))
        }
    }
}

fn render_apply(report: &ApplyReport) -> String {
    let mut text = String::new();
    for written in &report.written {
        let what = match &written.write {
            WriteKind::Create => "created".to_owned(),
            WriteKind::Parent => "linked to the parent".to_owned(),
            WriteKind::BlockedBy {
                blocker: Blocker::Proposed(key),
            } => format!("blocked by {key}"),
            WriteKind::BlockedBy {
                blocker: Blocker::Existing(number),
            } => format!("blocked by #{}", number.get()),
        };
        let how = if written.reused { " (earlier run)" } else { "" };
        let _ = writeln!(
            text,
            "{} {what}: {}{how}",
            written.issue,
            written.reference.as_str()
        );
    }
    let task = report
        .task
        .as_ref()
        .map_or_else(String::new, |task| format!(" (task {task})"));
    let _ = match &report.outcome {
        ApplyOutcome::Completed => write!(
            text,
            "Every write of digest {} is applied{task}.",
            report.preview.digest
        ),
        ApplyOutcome::StaleApproval => write!(
            text,
            "The approval does not name this preview; its digest is {}. Nothing was written.",
            report.preview.digest
        ),
        ApplyOutcome::NotReady => write!(
            text,
            "Ownership overlaps are still unordered. Nothing was written."
        ),
        ApplyOutcome::OverBudget { needed, limit } => write!(
            text,
            "The preview needs {needed} writes; the house allows {limit} per task. Nothing was written."
        ),
        ApplyOutcome::EarlierUnfinished { task } => write!(
            text,
            "Decomposition task {task} of this repository is unfinished. Rerun its approved proposal first. Nothing was written."
        ),
        ApplyOutcome::EarlierSettledWithWrites {
            task,
            settlement,
            writes,
        } => write!(
            text,
            "Decomposition task {task} settled ({settlement}) after writing {}. Check those writes on the forge, then run `kitchen decompose acknowledge --task {task}`. Nothing was written.",
            names(writes)
        ),
        ApplyOutcome::HeldElsewhere => write!(
            text,
            "Another run is writing this decomposition{task}. Nothing was written."
        ),
        ApplyOutcome::Uncertain { effect } => write!(
            text,
            "The outcome of write {effect} is unknown{task}; nothing after it was submitted. Rerun to reconcile it."
        ),
        ApplyOutcome::NotApplied { effect, reason } => write!(
            text,
            "The forge refused write {effect} ({}){task}. Rerun to retry the remaining writes.",
            not_applied(*reason)
        ),
        ApplyOutcome::Settled(settlement) => {
            write!(
                text,
                "The decomposition had already settled ({settlement}){task}."
            )
        }
        // An outcome this build does not know wrote nothing it can report.
        _ => write!(text, "The decomposition stopped{task}."),
    };
    text
}

fn names(writes: &[kitchen::EffectName]) -> String {
    writes
        .iter()
        .map(kitchen::EffectName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_acknowledgement(report: &AcknowledgeReport, reread: &Reread, house: &HouseId) -> String {
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
        match reread.skipped(house) {
            Some(skipped) => {
                text.push('\n');
                text.push_str(&skipped);
            }
            None => {
                for name in &report.applied {
                    text.push_str("\nre-read, applied: ");
                    text.push_str(name.as_str());
                }
                for name in &report.absent {
                    text.push_str("\nre-read, not applied: ");
                    text.push_str(name.as_str());
                }
            }
        }
    }
    for name in &report.unresolved {
        text.push_str("\nunproven write accepted: ");
        text.push_str(name.as_str());
    }
    text
}
