//! Persist and post one exact-subject gate review through the house forge.

use std::{collections::BTreeSet, num::NonZeroU64, time::Duration};

use super::{
    RunError, attest_gate_review,
    attestation::{check_review_independence, parse_review_block},
    gate_attestation,
};
use crate::{
    EffectName, HolderId, TaskId,
    contracts::{
        AttemptOutcome, AttemptStart, BranchName, CapabilityRequirements, Claimant, Clock,
        CommitId, Effect, EffectExecutor, GitHubAction, GitHubMutation, GrantScope, IssueNumber,
        LeaseTtl, Permission, Provenance, Repository, RetryPolicy, ReviewVerdict, Role,
        TaskAuthority, TaskSpec, Text,
    },
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubExecutor, GitHubMutationTransport, IssueState},
    state::{EffectPlan, EffectState, HouseStore, StateError, TaskState, reconcile, run_effect},
    workflows::{
        gate::{RiskClass, SemanticReview},
        known,
        pickup::stable_hash,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Content and exact revision requested by an expediter.
pub struct GateReviewInput {
    /// Destination repository.
    pub repository: Repository,
    /// Pull request number.
    pub pull_request: IssueNumber,
    /// Exact reviewed head.
    pub head: CommitId,
    /// Submitted verdict.
    pub verdict: ReviewVerdict,
    /// Markdown findings from the reviewer.
    pub findings: String,
    /// Semantic inspection result, required for approval.
    pub semantic: Option<SemanticReview>,
    /// Acceptance evidence complete, required for approval.
    pub acceptance: Option<bool>,
    /// Hardware evidence complete, required for approval.
    pub hardware: Option<bool>,
    /// Complete risk classification, required for approval.
    pub risk: Option<Vec<RiskClass>>,
    /// Record the approved review as a gate attestation.
    pub attest: bool,
}

/// A forge review proved by readback.
pub struct GateReview {
    /// Forge review identifier proven by readback.
    pub id: NonZeroU64,
    /// Whether the attestation was recorded.
    pub attested: bool,
}

fn label(complete: bool) -> &'static str {
    if complete { "complete" } else { "incomplete" }
}
fn semantic_name(value: &SemanticReview) -> &'static str {
    match value {
        SemanticReview::Clean => "clean",
        SemanticReview::Findings => "findings",
        SemanticReview::Partial => "partial",
        SemanticReview::Unavailable => "unavailable",
    }
}
fn risk(value: RiskClass) -> &'static str {
    match value {
        RiskClass::EquipmentSafety => "equipment-safety",
        RiskClass::AuthorizationSecrets => "authorization-secrets",
        RiskClass::DurableData => "durable-data",
        RiskClass::PublicContractRelease => "public-contract-release",
        RiskClass::WorkflowRules => "workflow-rules",
        RiskClass::Dependencies => "dependencies",
        RiskClass::WeakenedValidation => "weakened-validation",
        RiskClass::LargeDiff => "large-diff",
    }
}

/// Produce the only attestation block in a review body.
fn body(input: &GateReviewInput, base: &CommitId) -> Result<Text> {
    if input.findings.trim().is_empty() || input.findings.contains("```kitchen-attestation") {
        return Err(RunError::ReviewBodyInvalid.into());
    }
    let (semantic, acceptance, hardware, risks) = match input.verdict {
        ReviewVerdict::Approve => (
            input.semantic.clone().ok_or(RunError::ReviewBodyInvalid)?,
            input.acceptance.ok_or(RunError::ReviewBodyInvalid)?,
            input.hardware.ok_or(RunError::ReviewBodyInvalid)?,
            input
                .risk
                .as_ref()
                .ok_or(RunError::ReviewBodyInvalid)?
                .clone(),
        ),
        ReviewVerdict::RequestChanges => {
            if input.semantic.is_some()
                || input.acceptance.is_some()
                || input.hardware.is_some()
                || input.risk.is_some()
                || input.attest
            {
                return Err(RunError::ReviewClaimsWithoutApproval.into());
            }
            (SemanticReview::Findings, false, false, Vec::new())
        }
    };
    let risks = if risks.is_empty() {
        "none".to_owned()
    } else {
        let mut values = Vec::new();
        for class in risks {
            let name = risk(class);
            if values.contains(&name) {
                return Err(RunError::ReviewBodyInvalid.into());
            }
            values.push(name);
        }
        values.join(",")
    };
    let body = format!(
        "{}\n\n```kitchen-attestation\nhead={}\nbase={}\nsemantic={}\nread_only=true\nacceptance={}\nhardware={}\nrisk={}\n```\n",
        input.findings.trim_end(),
        input.head,
        base,
        semantic_name(&semantic),
        label(acceptance),
        label(hardware),
        risks
    );
    parse_review_block(Some(&body))?;
    Ok(Text::new(&body)?)
}

/// Post one review with durable intent. The forge executor reads back its
/// marker and returns the actual review id. An uncertain write is reconciled
/// and never submitted again while its outcome remains unknown.
///
/// # Errors
/// Refuses a stale head or base, invalid claims, missing house authority,
/// and any review whose outcome cannot be proved by the forge.
pub fn post_gate_review<T: GitHubMutationTransport + Clone>(
    store: &HouseStore,
    house: &HouseConfig,
    forge: &GitHubClient<T>,
    executor: &GitHubExecutor<T>,
    provenance: &Provenance,
    clock: &dyn Clock,
    input: &GateReviewInput,
) -> Result<GateReview> {
    if store.house() != &house.house || forge.scope().house() != store.house() {
        return Err(crate::contracts::ContractError::CrossHouse {
            expected: store.house().clone(),
            found: house.house.clone(),
        }
        .into());
    }
    if forge.scope().credential() != executor.scope().credential()
        || forge.scope().requester() != executor.scope().requester()
    {
        return Err(crate::integrations::github::IntegrationError::ScopeMismatch.into());
    }
    // Validate every caller-supplied claim before durable or external effects.
    if input.verdict == ReviewVerdict::RequestChanges
        && (input.semantic.is_some()
            || input.acceptance.is_some()
            || input.hardware.is_some()
            || input.risk.is_some()
            || input.attest)
    {
        return Err(RunError::ReviewClaimsWithoutApproval.into());
    }
    let pr = known(forge.pull_request(store.house(), &input.repository, input.pull_request))?;
    if pr.state != IssueState::Open || pr.merged {
        return Err(RunError::AttestationClosed.into());
    }
    if pr.head.sha != input.head {
        return Err(RunError::AttestationStaleHead.into());
    }
    if input.verdict == ReviewVerdict::Approve {
        check_review_independence(
            store,
            forge,
            &input.repository,
            input.pull_request,
            &pr,
            executor.scope().requester().as_str(),
        )?;
    }
    let base_branch = BranchName::new(&pr.base.name)?;
    let base = known(forge.branch_tip(store.house(), &input.repository, &base_branch))?;
    let text = body(input, &base)?;
    let mutation = GitHubMutation {
        repository: input.repository.clone(),
        action: GitHubAction::ReviewPullRequest {
            number: input.pull_request,
            expected_head: input.head.clone(),
            expected_base: base_branch,
            expected_base_commit: base,
            verdict: input.verdict,
            body: text,
        },
    };
    let effect = executor.effect(mutation.clone())?;
    let grants = house.issue_authority(&[], &[])?.grants().clone();
    let encoded = serde_json::to_vec(&mutation).map_err(|_| RunError::ReviewBodyInvalid)?;
    let id = TaskId::new(&format!(
        "gate-review-{}-{:016x}",
        input.pull_request.get(),
        stable_hash(&encoded)
    ))?;
    let claimant = Claimant::scheduled(HolderId::new("gate-review")?);
    let spec = TaskSpec {
        id: id.clone(),
        role: Role::Expediter,
        repository: Some(input.repository.clone()),
        authority: TaskAuthority::delegate(
            &grants,
            house
                .grants
                .iter()
                .filter(|grant| {
                    grant.permission == Permission::ReviewPullRequest
                        && grant.scope == GrantScope::Repository(input.repository.clone())
                })
                .cloned(),
        )?,
        retry: RetryPolicy::new(3, Duration::from_secs(7 * 24 * 3600))?,
        provenance: provenance.clone(),
        resources: BTreeSet::new(),
        requires: CapabilityRequirements::new(),
        agent: None,
        work_type: None,
    };
    let conflicting_spec = match store.create_task(spec, &claimant, clock.now()) {
        Ok(_) => false,
        Err(crate::Error::State(StateError::TaskConflict(_))) => true,
        Err(error) => return Err(error),
    };
    let task = store.task(&id)?;
    let expected: Effect = effect.clone().into();
    for persisted in task.effects() {
        let request = persisted.request();
        if request.house() != store.house()
            || request.backend() != &executor.descriptor().backend
            || request.credential() != executor.scope().credential().name()
            || request.task() != &id
            || request.effect() != &expected
        {
            return Err(StateError::TaskConflict(id).into());
        }
    }
    if task.effects().is_empty() && conflicting_spec {
        return Err(StateError::TaskConflict(id).into());
    }
    let applied = |task: &crate::state::TaskRecord| -> Result<Option<NonZeroU64>> {
        match task
            .effects()
            .iter()
            .map(|effect| effect.state())
            .find(|state| matches!(state, EffectState::Applied { .. }))
        {
            Some(EffectState::Applied { receipt, .. }) => Ok(Some(
                NonZeroU64::new(
                    receipt
                        .reference()
                        .as_str()
                        .parse()
                        .map_err(|_| RunError::GateRecords)?,
                )
                .ok_or(RunError::GateRecords)?,
            )),
            _ => Ok(None),
        }
    };
    let review_id = if let Some(id) = applied(&task)? {
        id
    } else {
        if matches!(task.state(), TaskState::Settled { .. }) {
            return Err(RunError::ReviewUncertain.into());
        }
        let lease = match store.claim(
            &id,
            &claimant,
            LeaseTtl::new(Duration::from_secs(600))?,
            clock.now(),
        ) {
            Ok(lease) => lease,
            Err(crate::Error::State(StateError::LeaseExpired { .. })) => store.take_over(
                &id,
                &claimant,
                LeaseTtl::new(Duration::from_secs(600))?,
                clock.now(),
            )?,
            Err(error) => return Err(error),
        };
        let fence = lease.fence();
        let result = (|| -> Result<NonZeroU64> {
            let reconciled = reconcile(store, executor, &id, fence, clock)?;
            if !reconciled.unresolved.is_empty() || !reconciled.foreign.is_empty() {
                return Err(RunError::ReviewUncertain.into());
            }
            if let Some(review_id) = applied(&store.task(&id)?)? {
                if let Some(attempt) = store.continue_attempt(&id, fence, clock.now())? {
                    store.finish_attempt(
                        &id,
                        fence,
                        attempt,
                        AttemptOutcome::Succeeded,
                        clock.now(),
                    )?;
                }
                return Ok(review_id);
            }
            let attempt = match store.start_attempt(&id, fence, clock.now())? {
                AttemptStart::Started(number) | AttemptStart::AlreadyRunning(number) => number,
                AttemptStart::Exhausted => return Err(RunError::ReviewUncertain.into()),
            };
            let record = run_effect(
                store,
                executor,
                &grants,
                EffectPlan {
                    task: id.clone(),
                    fence,
                    name: EffectName::new("review")?,
                    decided_at: store.task(&id)?.evidence().revision(),
                    effect: effect.into(),
                    consent: None,
                    basis: None,
                },
                clock,
            )?;
            if matches!(record.state(), EffectState::NotApplied { .. }) {
                let current = known(forge.pull_request(
                    store.house(),
                    &input.repository,
                    input.pull_request,
                ))?;
                if current.head.sha != input.head {
                    return Err(RunError::AttestationStaleHead.into());
                }
                let GitHubAction::ReviewPullRequest {
                    expected_base,
                    expected_base_commit,
                    ..
                } = &mutation.action
                else {
                    return Err(RunError::GateRecords.into());
                };
                if current.base.name != expected_base.as_str()
                    || known(forge.branch_tip(store.house(), &input.repository, expected_base))?
                        != *expected_base_commit
                {
                    return Err(RunError::AttestationStaleBase.into());
                }
                return Err(RunError::ReviewPostRefused.into());
            }
            let Some(review_id) = applied(&store.task(&id)?)? else {
                return Err(RunError::ReviewUncertain.into());
            };
            if !matches!(record.state(), EffectState::Applied { .. }) {
                return Err(RunError::ReviewUncertain.into());
            }
            store.finish_attempt(&id, fence, attempt, AttemptOutcome::Succeeded, clock.now())?;
            Ok(review_id)
        })();
        if matches!(store.task(&id)?.state(), TaskState::Claimed { lease: held } if held.fence() == fence)
        {
            store.relinquish(&id, fence, clock.now())?;
        }
        result?
    };
    let attested = if input.attest {
        let GitHubAction::ReviewPullRequest {
            expected_base_commit,
            ..
        } = &mutation.action
        else {
            return Err(RunError::GateRecords.into());
        };
        if let Some(existing) = gate_attestation(
            store,
            &input.repository,
            input.pull_request,
            &input.head,
            expected_base_commit,
        )? {
            if existing.attestation.forge_review.id != review_id {
                return Err(RunError::AttestationRecorded.into());
            }
        } else {
            attest_gate_review(
                store,
                forge,
                &input.repository,
                input.pull_request,
                review_id,
                clock.now(),
            )?;
        }
        true
    } else {
        false
    };
    Ok(GateReview {
        id: review_id,
        attested,
    })
}
