//! Record an exact-revision review in the house store for the scheduled gate.

use std::{io::Read, num::NonZeroU64, path::PathBuf};

use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    contracts::{Clock, CommitId, IssueNumber, Repository, ReviewVerdict, SystemClock},
    house::forge_binding,
    integrations::github::{GitHubExecutor, ReadLimits, TokenScope},
    workflows::{
        gate::{RiskClass, SemanticReview},
        run::{GateReviewInput, attest_gate_review, post_gate_review},
    },
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
    /// Post a review through the house forge binding at one exact head.
    Review(ReviewArgs),
}

#[derive(Args)]
struct ReviewArgs {
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
    #[arg(long, value_parser = ["approve", "request-changes"])]
    verdict: String,
    #[arg(long)]
    body_file: PathBuf,
    #[arg(long, value_parser = ["clean", "findings", "partial", "unavailable"])]
    semantic: Option<String>,
    #[arg(long, value_parser = ["complete", "incomplete"])]
    acceptance: Option<String>,
    #[arg(long, value_parser = ["complete", "incomplete"])]
    hardware: Option<String>,
    /// Comma-separated risk classes or `none`.
    #[arg(long)]
    risk: Option<String>,
    #[arg(long)]
    attest: bool,
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
    match args.command {
        GateCommand::Attest(args) => attest(args),
        GateCommand::Review(args) => review(args),
    }
}

fn attest(args: AttestArgs) -> Result<(String, bool), kitchen::Error> {
    let pull_request = IssueNumber::new(args.pull_request)?;
    let opened = Opened::open_parts(args.registry, &args.house, args.store, args.repository)?;
    let forge = opened
        .forge()?
        .with_read_access(TokenScope::for_review(opened.repository.clone()))?;
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

fn review(args: ReviewArgs) -> Result<(String, bool), kitchen::Error> {
    let opened = Opened::open_parts(args.registry, &args.house, args.store, args.repository)?;
    let binding = forge_binding(&opened.registry, &opened.config.house)?;
    let forge = opened
        .forge()?
        .with_read_access(TokenScope::for_review(opened.repository.clone()))?;
    let executor = GitHubExecutor::new(
        binding.backend.clone(),
        binding.scope(&opened.config)?,
        forge.transport().clone(),
        ReadLimits::default(),
    );
    let verdict = match args.verdict.as_str() {
        "approve" => ReviewVerdict::Approve,
        "request-changes" => ReviewVerdict::RequestChanges,
        _ => return Err(kitchen::workflows::run::RunError::ReviewBodyInvalid.into()),
    };
    let semantic = args
        .semantic
        .as_deref()
        .map(|value| match value {
            "clean" => Ok(SemanticReview::Clean),
            "findings" => Ok(SemanticReview::Findings),
            "partial" => Ok(SemanticReview::Partial),
            "unavailable" => Ok(SemanticReview::Unavailable),
            _ => Err(kitchen::workflows::run::RunError::ReviewBodyInvalid),
        })
        .transpose()?;
    let completeness = |value: Option<String>| value.map(|value| value == "complete");
    let risk = args.risk.as_deref().map(parse_risks).transpose()?;
    let file =
        std::fs::File::open(args.body_file).map_err(|source| kitchen::state::StateError::Io {
            operation: kitchen::state::StorageOperation::Read,
            source,
        })?;
    const MAX_BODY_FILE_BYTES: usize = 60 * 1024;
    let mut bytes = Vec::new();
    file.take((MAX_BODY_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| kitchen::state::StateError::Io {
            operation: kitchen::state::StorageOperation::Read,
            source,
        })?;
    if bytes.len() > MAX_BODY_FILE_BYTES {
        return Err(kitchen::workflows::run::RunError::ReviewBodyInvalid.into());
    }
    let findings = String::from_utf8(bytes)
        .map_err(|_| kitchen::workflows::run::RunError::ReviewBodyInvalid)?;
    let input = GateReviewInput {
        repository: opened.repository.clone(),
        pull_request: IssueNumber::new(args.pull_request)?,
        head: args.head,
        verdict,
        findings,
        semantic,
        acceptance: completeness(args.acceptance),
        hardware: completeness(args.hardware),
        risk,
        attest: args.attest,
    };
    let provenance = super::run::instructions(&opened)?.provenance;
    let result = post_gate_review(
        &opened.store,
        &opened.config,
        &forge,
        &executor,
        &provenance,
        &SystemClock,
        &input,
    )?;
    Ok((
        format!(
            "posted gate review {} for {} #{} at {}{}",
            result.id,
            input.repository,
            input.pull_request.get(),
            input.head,
            if result.attested {
                " and recorded attestation"
            } else {
                ""
            }
        ),
        true,
    ))
}

fn parse_risks(value: &str) -> Result<Vec<RiskClass>, kitchen::Error> {
    if value == "none" {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|name| match name {
            "equipment-safety" => Ok(RiskClass::EquipmentSafety),
            "authorization-secrets" => Ok(RiskClass::AuthorizationSecrets),
            "durable-data" => Ok(RiskClass::DurableData),
            "public-contract-release" => Ok(RiskClass::PublicContractRelease),
            "workflow-rules" => Ok(RiskClass::WorkflowRules),
            "dependencies" => Ok(RiskClass::Dependencies),
            "weakened-validation" => Ok(RiskClass::WeakenedValidation),
            "large-diff" => Ok(RiskClass::LargeDiff),
            _ => Err(kitchen::workflows::run::RunError::ReviewBodyInvalid.into()),
        })
        .collect()
}
