//! Repository readiness for autonomous merge.
//!
//! Readiness is diagnostic evidence about the repository's own checks and
//! instructions. House policy can hold a merge grant back until a work type
//! reaches a level, but no level grants merge authority: that still requires
//! an explicit `Merge` grant.
use super::{HouseConfig, HouseError, RepositoryConfig, validate_names};
use crate::{
    HolderId, HouseId,
    contracts::{
        CommitId, ExternalRef, GrantScope, HouseGrants, Permission, Repository, Text, Timestamp,
    },
    integrations::github::{
        CheckConclusion, CheckRun, CheckStatus, CommitStatus, RequiredChecks, StatusState,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Largest check history accepted in one observation.
pub const MAX_CHECK_RECORDS: usize = 4096;
/// Largest number of work types in an observation or policy.
pub const MAX_WORK_TYPES: usize = 64;
/// Largest number of forge-required checks accepted in one observation.
pub const MAX_REQUIRED_CHECKS: usize = 512;

/// Ordered readiness levels; each level includes the ones below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessLevel {
    /// Required checks or repository instructions are absent or unknown.
    Unready,
    /// The forge requires at least one check, including every configured
    /// check, and the repository has instructions.
    Checked,
    /// Every required check also has known recent history with a pass and no
    /// head that both passed and failed.
    Reliable,
    /// The work type also has a known acceptance check that the forge requires.
    Covered,
}

impl ReadinessLevel {
    /// Stable report name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unready => "unready",
            Self::Checked => "checked",
            Self::Reliable => "reliable",
            Self::Covered => "covered",
        }
    }
}

/// Result of one check run or status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckOutcome {
    /// Completed successfully.
    Passed,
    /// Completed with a failure or timeout.
    Failed,
    /// Pending, cancelled, skipped, neutral, stale, or unrecognized.
    Inconclusive,
}

/// One observed run of a check at an exact head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckRunRecord {
    /// Check-run name or status context.
    pub check: String,
    /// Commit the run tested.
    pub head: CommitId,
    /// Run result.
    pub outcome: CheckOutcome,
}

impl CheckRunRecord {
    /// Record a GitHub check run. Only a completed success passes.
    #[must_use]
    pub fn from_check_run(run: &CheckRun) -> Self {
        let outcome = match (run.status, run.conclusion) {
            (CheckStatus::Completed, Some(CheckConclusion::Success)) => CheckOutcome::Passed,
            (
                CheckStatus::Completed,
                Some(CheckConclusion::Failure | CheckConclusion::TimedOut),
            ) => CheckOutcome::Failed,
            (
                CheckStatus::Completed,
                Some(
                    CheckConclusion::Cancelled
                    | CheckConclusion::Neutral
                    | CheckConclusion::Skipped
                    | CheckConclusion::ActionRequired
                    | CheckConclusion::Stale
                    | CheckConclusion::Unknown,
                )
                | None,
            )
            | (CheckStatus::Queued | CheckStatus::InProgress | CheckStatus::Unknown, _) => {
                CheckOutcome::Inconclusive
            }
        };
        Self {
            check: run.name.clone(),
            head: run.head_sha.clone(),
            outcome,
        }
    }

    /// Record a GitHub commit status.
    #[must_use]
    pub fn from_commit_status(status: &CommitStatus) -> Self {
        let outcome = match status.state {
            StatusState::Success => CheckOutcome::Passed,
            StatusState::Failure | StatusState::Error => CheckOutcome::Failed,
            StatusState::Pending | StatusState::Unknown => CheckOutcome::Inconclusive,
        };
        Self {
            check: status.context.clone(),
            head: status.sha.clone(),
            outcome,
        }
    }
}

/// Every check name that branch protection requires.
#[must_use]
pub fn required_check_names(required: &RequiredChecks) -> BTreeSet<String> {
    required
        .contexts
        .iter()
        .cloned()
        .chain(required.checks.iter().map(|check| check.context.clone()))
        .collect()
}

/// Read-only repository observations. `None` means not observed, which is
/// reported as unknown; an empty collection means observed and absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadinessEvidence {
    /// Checks the forge requires on the default branch.
    #[serde(default)]
    pub required_checks: Option<BTreeSet<String>>,
    /// Recent check runs on the default branch and its pull requests.
    #[serde(default)]
    pub check_history: Option<Vec<CheckRunRecord>>,
    /// Repository instruction files present at the default branch head.
    #[serde(default)]
    pub instruction_files: Option<BTreeSet<String>>,
    /// Checks that establish acceptance for each work type. A work type
    /// absent from a supplied map is unknown.
    #[serde(default)]
    pub acceptance_checks: Option<BTreeMap<Text, BTreeSet<String>>>,
}

impl ReadinessEvidence {
    fn validate(&self) -> Result<(), HouseError> {
        if let Some(files) = &self.instruction_files {
            validate_names(files)?;
        }
        if let Some(required) = &self.required_checks
            && (required.len() > MAX_REQUIRED_CHECKS
                || required.iter().any(|name| !valid_name(name)))
        {
            return Err(HouseError::InvalidInput);
        }
        if let Some(history) = &self.check_history
            && (history.len() > MAX_CHECK_RECORDS
                || history.iter().any(|record| !valid_name(&record.check)))
        {
            return Err(HouseError::InvalidInput);
        }
        if let Some(work_types) = &self.acceptance_checks {
            if work_types.len() > MAX_WORK_TYPES {
                return Err(HouseError::InvalidInput);
            }
            for (work_type, checks) in work_types {
                validate_work_type(work_type)?;
                validate_names(checks)?;
            }
        }
        Ok(())
    }
}

/// An assessed fact: known from evidence, or unknown and therefore not a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "kebab-case")]
pub enum Assessed<T> {
    /// Observed.
    Known(T),
    /// Not observed or without samples.
    Unknown,
}

/// Recent results for one check. Only built from at least one record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckHistory {
    /// Passing runs.
    pub passed: u32,
    /// Failing runs.
    pub failed: u32,
    /// Runs without a pass or fail result.
    pub inconclusive: u32,
    /// Heads where the check both passed and failed.
    pub flaky_heads: u32,
}

/// A readiness gap. Unknown evidence is a gap, never a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ReadinessGap {
    /// Branch protection was not observed.
    RequiredChecksUnknown,
    /// The forge requires no check.
    NoRequiredChecks,
    /// A house or repository check is not required by the forge.
    CheckNotRequired {
        /// Configured check name.
        check: String,
    },
    /// Repository instruction files were not observed.
    InstructionsUnknown,
    /// The repository has no instruction files.
    NoInstructions,
    /// No recent run of a required check was observed.
    HistoryUnknown {
        /// Check name.
        check: String,
    },
    /// Recent runs of a required check never passed.
    NeverPassed {
        /// Check name.
        check: String,
    },
    /// A required check both passed and failed at the same head.
    Flaky {
        /// Check name.
        check: String,
        /// Affected heads.
        heads: u32,
    },
    /// Acceptance checks for a work type were not observed.
    AcceptanceUnknown {
        /// Work type.
        work_type: Text,
    },
    /// A work type has no acceptance check.
    NoAcceptanceCheck {
        /// Work type.
        work_type: Text,
    },
    /// A work type's acceptance check is not required by the forge.
    AcceptanceNotRequired {
        /// Work type.
        work_type: Text,
        /// Check name.
        check: String,
    },
}

impl ReadinessGap {
    /// Human-readable diagnosis.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::RequiredChecksUnknown => {
                "Required checks: unknown (branch protection not observed).".into()
            }
            Self::NoRequiredChecks => "Required checks: none required by the forge.".into(),
            Self::CheckNotRequired { check } => {
                format!("Configured check {check} is not required by the forge.")
            }
            Self::InstructionsUnknown => "Repository instructions: unknown.".into(),
            Self::NoInstructions => "Repository instructions: none present.".into(),
            Self::HistoryUnknown { check } => {
                format!("Check {check}: recent history unknown.")
            }
            Self::NeverPassed { check } => {
                format!("Check {check}: no recent run passed.")
            }
            Self::Flaky { check, heads } => {
                format!("Check {check}: flaky; passed and failed at {heads} head(s).")
            }
            Self::AcceptanceUnknown { work_type } => {
                format!(
                    "Work type {}: acceptance check unknown.",
                    work_type.as_str()
                )
            }
            Self::NoAcceptanceCheck { work_type } => {
                format!("Work type {}: no acceptance check.", work_type.as_str())
            }
            Self::AcceptanceNotRequired { work_type, check } => format!(
                "Work type {}: acceptance check {check} is not required by the forge.",
                work_type.as_str()
            ),
        }
    }

    /// Exact next step to close the gap.
    #[must_use]
    pub const fn next_step(&self) -> &'static str {
        match self {
            Self::RequiredChecksUnknown | Self::InstructionsUnknown => {
                "Read the default branch through the house-scoped forge integration and rerun doctor with that observation."
            }
            Self::HistoryUnknown { .. } | Self::AcceptanceUnknown { .. } => {
                "Supply recent check history and acceptance checks from a house-scoped observation and rerun doctor."
            }
            Self::NoRequiredChecks | Self::CheckNotRequired { .. } => {
                "Ask the repository owner to require the check in branch protection; Kitchen does not change repository settings."
            }
            Self::NoInstructions => {
                "Add repository instructions such as AGENTS.md or CONTRIBUTING.md through a normal reviewed change."
            }
            Self::NeverPassed { .. } | Self::Flaky { .. } => {
                "Fix the check until it passes reliably, then rerun doctor with fresh history."
            }
            Self::NoAcceptanceCheck { .. } | Self::AcceptanceNotRequired { .. } => {
                "Add a required check that establishes acceptance for this work type, or keep its merges manual."
            }
        }
    }
}

/// Readiness assessment for one repository. Diagnostic only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryReadiness {
    /// Assessed house.
    pub house: HouseId,
    /// Assessed repository.
    pub repository: Repository,
    /// Checks the forge requires.
    pub required_checks: Assessed<BTreeSet<String>>,
    /// History per required or configured check.
    pub check_history: BTreeMap<String, Assessed<CheckHistory>>,
    /// Repository instruction files.
    pub instruction_files: Assessed<BTreeSet<String>>,
    /// Acceptance checks per work type in house policy or evidence.
    pub acceptance_checks: BTreeMap<Text, Assessed<BTreeSet<String>>>,
    /// Level for work without a coverage requirement; at most `reliable`.
    pub level: ReadinessLevel,
    /// Every gap found, including unknown evidence.
    pub gaps: Vec<ReadinessGap>,
}

impl RepositoryReadiness {
    /// Level for one work type. A work type without known, required
    /// acceptance checks stays at the repository level.
    #[must_use]
    pub fn level_for(&self, work_type: &Text) -> ReadinessLevel {
        let covered = self.level == ReadinessLevel::Reliable
            && matches!(
                (self.acceptance_checks.get(work_type), &self.required_checks),
                (Some(Assessed::Known(checks)), Assessed::Known(required))
                    if !checks.is_empty() && checks.is_subset(required)
            );
        if covered {
            ReadinessLevel::Covered
        } else {
            self.level
        }
    }
}

/// Assess readiness from observations. Missing observations become unknown
/// facts and gaps, never passes.
///
/// # Errors
/// Returns [`HouseError::InvalidInput`] for invalid or oversized evidence and
/// the repository validation error for a mismatched house.
pub fn assess(
    house: &HouseConfig,
    repository: &RepositoryConfig,
    evidence: Option<&ReadinessEvidence>,
) -> Result<RepositoryReadiness, HouseError> {
    let configured = repository.checks(house)?;
    let empty = ReadinessEvidence::default();
    let evidence = evidence.unwrap_or(&empty);
    evidence.validate()?;
    let mut gaps = Vec::new();

    let required_checks = known(evidence.required_checks.clone());
    let mut checks_enforced = true;
    match &required_checks {
        Assessed::Unknown => {
            checks_enforced = false;
            gaps.push(ReadinessGap::RequiredChecksUnknown);
        }
        Assessed::Known(required) => {
            if required.is_empty() {
                checks_enforced = false;
                gaps.push(ReadinessGap::NoRequiredChecks);
            }
            for check in configured.difference(required) {
                checks_enforced = false;
                gaps.push(ReadinessGap::CheckNotRequired {
                    check: check.clone(),
                });
            }
        }
    }

    let instruction_files = known(evidence.instruction_files.clone());
    let instructed = match &instruction_files {
        Assessed::Unknown => {
            gaps.push(ReadinessGap::InstructionsUnknown);
            false
        }
        Assessed::Known(files) if files.is_empty() => {
            gaps.push(ReadinessGap::NoInstructions);
            false
        }
        Assessed::Known(_) => true,
    };

    let tracked: BTreeSet<&String> = match &required_checks {
        Assessed::Known(required) => required.iter().chain(&configured).collect(),
        Assessed::Unknown => configured.iter().collect(),
    };
    let mut reliable = true;
    let mut check_history = BTreeMap::new();
    for check in tracked {
        let history = summarize(evidence.check_history.as_deref(), check);
        match &history {
            Assessed::Unknown => {
                reliable = false;
                gaps.push(ReadinessGap::HistoryUnknown {
                    check: check.clone(),
                });
            }
            Assessed::Known(history) => {
                if history.passed == 0 {
                    reliable = false;
                    gaps.push(ReadinessGap::NeverPassed {
                        check: check.clone(),
                    });
                }
                if history.flaky_heads > 0 {
                    reliable = false;
                    gaps.push(ReadinessGap::Flaky {
                        check: check.clone(),
                        heads: history.flaky_heads,
                    });
                }
            }
        }
        check_history.insert(check.clone(), history);
    }

    let mut work_types: BTreeSet<&Text> = house.merge_readiness.keys().collect();
    if let Some(supplied) = &evidence.acceptance_checks {
        work_types.extend(supplied.keys());
    }
    let mut acceptance_checks = BTreeMap::new();
    for work_type in work_types {
        let checks = evidence
            .acceptance_checks
            .as_ref()
            .and_then(|supplied| supplied.get(work_type))
            .cloned();
        match (&checks, &required_checks) {
            (None, _) => gaps.push(ReadinessGap::AcceptanceUnknown {
                work_type: work_type.clone(),
            }),
            (Some(checks), _) if checks.is_empty() => {
                gaps.push(ReadinessGap::NoAcceptanceCheck {
                    work_type: work_type.clone(),
                });
            }
            (Some(checks), Assessed::Known(required)) => {
                for check in checks.difference(required) {
                    gaps.push(ReadinessGap::AcceptanceNotRequired {
                        work_type: work_type.clone(),
                        check: check.clone(),
                    });
                }
            }
            // Enforcement is unknown; the required-checks gap reports it.
            (Some(_), Assessed::Unknown) => {}
        }
        acceptance_checks.insert(work_type.clone(), known(checks));
    }

    let level = match (checks_enforced && instructed, reliable) {
        (false, _) => ReadinessLevel::Unready,
        (true, false) => ReadinessLevel::Checked,
        (true, true) => ReadinessLevel::Reliable,
    };
    Ok(RepositoryReadiness {
        house: house.house.clone(),
        repository: repository.repository.clone(),
        required_checks,
        check_history,
        instruction_files,
        acceptance_checks,
        level,
        gaps,
    })
}

fn known<T>(value: Option<T>) -> Assessed<T> {
    value.map_or(Assessed::Unknown, Assessed::Known)
}

fn summarize(records: Option<&[CheckRunRecord]>, check: &str) -> Assessed<CheckHistory> {
    let Some(records) = records else {
        return Assessed::Unknown;
    };
    let mut history = CheckHistory::default();
    let mut heads: BTreeMap<&CommitId, (bool, bool)> = BTreeMap::new();
    for record in records.iter().filter(|record| record.check == check) {
        let head = heads.entry(&record.head).or_default();
        match record.outcome {
            CheckOutcome::Passed => {
                history.passed = history.passed.saturating_add(1);
                head.0 = true;
            }
            CheckOutcome::Failed => {
                history.failed = history.failed.saturating_add(1);
                head.1 = true;
            }
            CheckOutcome::Inconclusive => {
                history.inconclusive = history.inconclusive.saturating_add(1);
            }
        }
    }
    if heads.is_empty() {
        return Assessed::Unknown;
    }
    history.flaky_heads = heads
        .values()
        .filter(|(passed, failed)| *passed && *failed)
        .fold(0, |count: u32, _| count.saturating_add(1));
    Assessed::Known(history)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
}

pub(super) fn validate_work_type(work_type: &Text) -> Result<(), HouseError> {
    if valid_name(work_type.as_str()) {
        Ok(())
    } else {
        Err(HouseError::InvalidInput)
    }
}

/// An owner's decision to allow a merge grant below the required level.
/// It is bound to the exact scope and levels it was made for. Who may decide
/// is house policy: `decided_by` must be listed in [`HouseConfig::owners`],
/// or the decision is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BelowReadinessDecision {
    /// Deciding house.
    pub house: HouseId,
    /// Repository the decision covers.
    pub repository: Repository,
    /// Work type the decision covers.
    pub work_type: Text,
    /// Level the owner saw; a different assessment needs a new decision.
    pub assessed: ReadinessLevel,
    /// Required level the owner accepted missing.
    pub required: ReadinessLevel,
    /// Decision author.
    pub decided_by: HolderId,
    /// Why proceeding below the required level is acceptable.
    pub reason: Text,
    /// Durable decision source.
    pub decision: ExternalRef,
    /// Decision time.
    pub at: Timestamp,
}

/// Readiness outcome for a proposed merge grant. It is a precondition, not
/// authority: the grant itself must still be issued and checked separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[must_use]
pub enum ReadinessClearance {
    /// House policy sets no level for this work type.
    NotRequired {
        /// Assessed level.
        assessed: ReadinessLevel,
    },
    /// The assessed level meets the policy.
    Met {
        /// Required level.
        required: ReadinessLevel,
        /// Assessed level.
        assessed: ReadinessLevel,
    },
    /// An owner accepted the gap; record this decision with the grant.
    AcceptedBelow(BelowReadinessDecision),
}

/// Check house readiness policy before a merge grant for a work type.
///
/// # Errors
/// Returns [`HouseError::BelowReadiness`] when the level is below policy and
/// no matching owner decision is supplied, [`HouseError::ReadinessDecision`]
/// when the decision's scope or levels do not match, and
/// [`HouseError::HouseSelection`] when the assessment belongs to another house.
pub fn merge_readiness(
    house: &HouseConfig,
    readiness: &RepositoryReadiness,
    work_type: &Text,
    decision: Option<&BelowReadinessDecision>,
) -> Result<ReadinessClearance, HouseError> {
    if readiness.house != house.house || !house.repositories.contains(&readiness.repository) {
        return Err(HouseError::HouseSelection);
    }
    let assessed = readiness.level_for(work_type);
    let Some(&required) = house.merge_readiness.get(work_type) else {
        return Ok(ReadinessClearance::NotRequired { assessed });
    };
    if assessed >= required {
        return Ok(ReadinessClearance::Met { required, assessed });
    }
    let Some(decision) = decision else {
        return Err(HouseError::BelowReadiness { required, assessed });
    };
    verify_decision(
        house,
        &readiness.repository,
        work_type,
        assessed,
        required,
        decision,
    )?;
    Ok(ReadinessClearance::AcceptedBelow(decision.clone()))
}

fn decision_in_scope(
    house: &HouseConfig,
    repository: &Repository,
    work_type: &Text,
    decision: &BelowReadinessDecision,
) -> bool {
    decision.house == house.house
        && &decision.repository == repository
        && &decision.work_type == work_type
}

/// A decision counts only for the exact house, repository, work type, and
/// levels it was made for, with a reason, by a holder the house lists as owner.
fn verify_decision(
    house: &HouseConfig,
    repository: &Repository,
    work_type: &Text,
    assessed: ReadinessLevel,
    required: ReadinessLevel,
    decision: &BelowReadinessDecision,
) -> Result<(), HouseError> {
    if !decision_in_scope(house, repository, work_type, decision)
        || decision.assessed != assessed
        || decision.required != required
        || decision.reason.as_str().trim().is_empty()
    {
        return Err(HouseError::ReadinessDecision);
    }
    if !house.owners.contains(&decision.decided_by) {
        return Err(HouseError::ReadinessDeciderNotOwner);
    }
    Ok(())
}

/// House authority issued with the readiness evidence behind any merge grant.
#[derive(Debug, Clone)]
#[must_use]
pub struct IssuedAuthority {
    /// Standing grants, including merge grants that passed the readiness gate.
    pub authority: HouseGrants,
    /// Owner decisions that let a merge grant proceed below the required
    /// level; record each with the grant.
    pub accepted_below: Vec<BelowReadinessDecision>,
}

impl HouseConfig {
    /// Issue house authority, gating every configured [`Permission::Merge`]
    /// grant on readiness. For each repository with a merge grant and each
    /// work type in `merge_readiness`, the repository's assessed level must
    /// meet the requirement, or `decisions` must hold an owner decision bound
    /// to that house, repository, work type, and both levels. A repository
    /// without an assessment counts as unready.
    ///
    /// # Errors
    /// Returns [`HouseError::BelowReadiness`] naming the required and
    /// assessed level, [`HouseError::ReadinessDecision`] or
    /// [`HouseError::ReadinessDeciderNotOwner`] for an unusable decision, and
    /// the validation errors of [`HouseConfig::authority`].
    pub fn issue_authority(
        &self,
        readiness: &[RepositoryReadiness],
        decisions: &[BelowReadinessDecision],
    ) -> Result<IssuedAuthority, HouseError> {
        let authority = self.build_authority()?;
        let mut accepted_below = Vec::new();
        let merge_repositories: BTreeSet<&Repository> = self
            .grants
            .iter()
            .filter(|grant| grant.permission == Permission::Merge)
            .filter_map(|grant| match &grant.scope {
                GrantScope::Repository(repository) => Some(repository),
                GrantScope::House => None,
            })
            .collect();
        for repository in merge_repositories {
            for (work_type, &required) in &self.merge_readiness {
                let assessed = readiness
                    .iter()
                    .filter(|r| r.house == self.house && &r.repository == repository)
                    .map(|r| r.level_for(work_type))
                    .min()
                    .unwrap_or(ReadinessLevel::Unready);
                if assessed >= required {
                    continue;
                }
                let mut refusal = HouseError::BelowReadiness { required, assessed };
                let mut accepted = None;
                for decision in decisions
                    .iter()
                    .filter(|d| decision_in_scope(self, repository, work_type, d))
                {
                    match verify_decision(self, repository, work_type, assessed, required, decision)
                    {
                        Ok(()) => {
                            accepted = Some(decision.clone());
                            break;
                        }
                        Err(error) => refusal = error,
                    }
                }
                accepted_below.push(accepted.ok_or(refusal)?);
            }
        }
        Ok(IssuedAuthority {
            authority,
            accepted_below,
        })
    }
}
