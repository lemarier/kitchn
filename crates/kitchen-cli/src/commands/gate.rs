//! Record an exact-revision review in the house store for the scheduled gate.

use std::{num::NonZeroU64, path::PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    HolderId, HouseId,
    contracts::{Claimant, Clock, CommitId, IssueNumber, Repository, SystemClock},
    workflows::{
        gate::{RiskClass, SemanticReview},
        run::{ForgeReview, GateAttestation, RunError, attest_gate_review},
    },
};

use super::run::Opened;

#[derive(Args)]
pub struct GateArgs {
    #[command(subcommand)]
    command: GateCommand,
}

#[derive(Subcommand)]
enum GateCommand {
    /// Record a read-only review of the exact committed base and head.
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
    #[arg(long)]
    head: CommitId,
    #[arg(long)]
    base: CommitId,
    /// The forge reviewer's login.
    #[arg(long)]
    reviewer: String,
    /// The holder or worker handle recording the review.
    #[arg(long)]
    recorder: HolderId,
    /// The forge review ID.
    #[arg(long)]
    review_id: NonZeroU64,
    #[arg(long, value_enum)]
    result: ReviewResult,
    #[arg(long, action = clap::ArgAction::Set)]
    read_only: bool,
    #[arg(long, value_enum)]
    acceptance: Completion,
    #[arg(long, value_enum)]
    hardware: Completion,
    /// Complete risk classification. Pass `none` when no class applies.
    #[arg(long, value_enum, required = true, num_args = 1..)]
    risk: Vec<Risk>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ReviewResult {
    Clean,
    Findings,
    Partial,
    Unavailable,
}

#[derive(Clone, Copy, ValueEnum)]
enum Completion {
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Risk {
    None,
    EquipmentSafety,
    AuthorizationSecrets,
    DurableData,
    PublicContractRelease,
    WorkflowRules,
    Dependencies,
    WeakenedValidation,
    LargeDiff,
}

impl Risk {
    fn class(self) -> Option<RiskClass> {
        Some(match self {
            Self::None => return None,
            Self::EquipmentSafety => RiskClass::EquipmentSafety,
            Self::AuthorizationSecrets => RiskClass::AuthorizationSecrets,
            Self::DurableData => RiskClass::DurableData,
            Self::PublicContractRelease => RiskClass::PublicContractRelease,
            Self::WorkflowRules => RiskClass::WorkflowRules,
            Self::Dependencies => RiskClass::Dependencies,
            Self::WeakenedValidation => RiskClass::WeakenedValidation,
            Self::LargeDiff => RiskClass::LargeDiff,
        })
    }
}

pub fn run(args: GateArgs) -> Result<(String, bool), kitchen::Error> {
    let GateCommand::Attest(args) = args.command;
    if (args.risk.contains(&Risk::None) && args.risk.len() != 1)
        || args
            .risk
            .iter()
            .enumerate()
            .any(|(index, risk)| args.risk[..index].contains(risk))
    {
        return Err(RunError::AttestationRiskInvalid.into());
    }
    let opened = Opened::open_parts(args.registry, &args.house, args.store, args.repository)?;
    let forge = opened.forge()?;
    let risk_classes = args.risk.iter().filter_map(|risk| risk.class()).collect();
    let attestation = GateAttestation {
        house: args.house,
        repository: opened.repository,
        pull_request: IssueNumber::new(args.pull_request)?,
        head: args.head,
        base: args.base,
        forge_review: ForgeReview {
            id: args.review_id,
            reviewer: args.reviewer,
        },
        review: match args.result {
            ReviewResult::Clean => SemanticReview::Clean,
            ReviewResult::Findings => SemanticReview::Findings,
            ReviewResult::Partial => SemanticReview::Partial,
            ReviewResult::Unavailable => SemanticReview::Unavailable,
        },
        read_only: args.read_only,
        acceptance_met: matches!(args.acceptance, Completion::Complete),
        hardware_complete: matches!(args.hardware, Completion::Complete),
        risk_classes,
    };
    attest_gate_review(
        &opened.store,
        &forge,
        &attestation,
        &Claimant::interactive(args.recorder),
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
