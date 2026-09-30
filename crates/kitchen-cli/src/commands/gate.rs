//! Record an exact-revision review in the house store for the scheduled gate.

use std::{num::NonZeroU64, path::PathBuf};

use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    contracts::{Clock, IssueNumber, Repository, SystemClock},
    workflows::run::attest_gate_review,
};

use super::run::Opened;

#[derive(Args)]
#[command(
    after_help = "--registry defaults to KITCHN_HOME or ~/.kitchn; --house uses this checkout's stored repository binding unless specified."
)]
pub struct GateArgs {
    #[command(subcommand)]
    command: GateCommand,
}

#[derive(Subcommand)]
enum GateCommand {
    /// Record an approved forge review with a kitchen-attestation block.
    Attest(AttestArgs),
}

#[derive(Args)]
struct AttestArgs {
    #[arg(long)]
    registry: PathBuf,
    #[arg(long)]
    house: HouseId,
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long)]
    repository: Option<Repository>,
    #[arg(long)]
    pull_request: u64,
    /// The forge review ID containing the attestation block.
    #[arg(long)]
    review_id: NonZeroU64,
}

pub fn run(args: GateArgs) -> Result<(String, bool), kitchen::Error> {
    let GateCommand::Attest(args) = args.command;
    let pull_request = IssueNumber::new(args.pull_request)?;
    let opened = Opened::open_parts(args.registry, &args.house, args.store, args.repository)?;
    let forge = opened.forge()?;
    let attestation = attest_gate_review(
        &opened.store,
        &forge,
        &opened.repository,
        pull_request,
        args.review_id,
        SystemClock.now(),
    )?;
    Ok((
        format!(
            "recorded gate attestation for {} #{} at {} on {} by {} (review {})",
            attestation.repository,
            attestation.pull_request.get(),
            attestation.head,
            attestation.base,
            attestation.forge_review.reviewer,
            attestation.forge_review.id
        ),
        true,
    ))
}
