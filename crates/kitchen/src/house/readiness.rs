//! Repository readiness for autonomous merge.
//!
//! Readiness is diagnostic evidence about the repository's own checks and
//! instructions. House policy can hold a merge grant back until a work type
//! reaches a level, but no level grants merge authority: that still requires
//! an explicit `Merge` grant.
use super::{HouseConfig, HouseError, RepositoryConfig, validate_names};
use crate::{
    HouseId, TaskId,
    contracts::{
        AskKind, AskRisk, CommitId, DecisionBinding, DecisionOwner, Effect, EvidenceRevision,
        EvidenceSubject, ExternalRef, GrantScope, HouseGrants, IssueNumber, Permission, Repository,
        RogerAsk, Text,
    },
    integrations::{
        github::{
            CheckConclusion, CheckRun, CheckStatus, CommitStatus, HouseScope, RequiredChecks,
            StatusState,
        },
        roger::{DecisionStatus, validate_answer},
    },
    state::{EffectState, HouseStore},
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
    /// GitHub App that reported the run; absent for commit statuses and
    /// runs whose app is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<i64>,
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
            app_id: run.app.as_ref().map(|app| app.id),
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
            app_id: None,
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

/// The GitHub App each required check must come from, where branch
/// protection names one. A check that accepts any source is absent.
#[must_use]
pub fn required_check_apps(required: &RequiredChecks) -> BTreeMap<String, i64> {
    required
        .checks
        .iter()
        .filter_map(|check| {
            check
                .app_id
                .filter(|id| *id > 0)
                .map(|id| (check.context.clone(), id))
        })
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
    /// The GitHub App a required check must come from, where branch
    /// protection names one. History from another app or a commit status
    /// does not count for that check.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub required_check_apps: BTreeMap<String, i64>,
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
        if self.required_check_apps.len() > MAX_REQUIRED_CHECKS
            || self
                .required_check_apps
                .iter()
                .any(|(name, id)| !valid_name(name) || *id <= 0)
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
        let history = summarize(
            evidence.check_history.as_deref(),
            check,
            evidence.required_check_apps.get(check.as_str()).copied(),
        );
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

/// History of one check. When `app` is set, only runs that app reported
/// count; commit statuses and other apps' runs with the same name do not.
fn summarize(
    records: Option<&[CheckRunRecord]>,
    check: &str,
    app: Option<i64>,
) -> Assessed<CheckHistory> {
    let Some(records) = records else {
        return Assessed::Unknown;
    };
    let mut history = CheckHistory::default();
    let mut heads: BTreeMap<&CommitId, (bool, bool)> = BTreeMap::new();
    for record in records
        .iter()
        .filter(|record| record.check == check && app.is_none_or(|id| record.app_id == Some(id)))
    {
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

/// The pull request and exact revision a merge is judged at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSubject {
    /// Destination repository.
    pub repository: Repository,
    /// Pull request.
    pub number: IssueNumber,
    /// Exact head.
    pub head: CommitId,
    /// Exact base.
    pub base: CommitId,
}

/// What an owner is asked to accept: merging one pull request at an exact
/// head and base although a work type is below the level house policy
/// requires, for a stated reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BelowReadinessRequest {
    /// Work type below its required level.
    pub work_type: Text,
    /// Pull request and revision the decision covers.
    pub subject: MergeSubject,
    /// Why proceeding is acceptable; the owner approves this exact text.
    pub reason: Text,
}

/// An owner's approval to merge one pull request below the required
/// readiness. It exists only as the result of [`accept_below_readiness`],
/// which reads it back from a Roger approval Ask persisted in the house
/// store, so a caller cannot assert one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BelowReadinessDecision {
    house: HouseId,
    task: TaskId,
    subject: MergeSubject,
    work_type: Text,
    assessed: ReadinessLevel,
    required: ReadinessLevel,
    reason: Text,
    ask: ExternalRef,
}

impl BelowReadinessDecision {
    /// Deciding house.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }
    /// Task that persisted the Ask.
    #[must_use]
    pub const fn task(&self) -> &TaskId {
        &self.task
    }
    /// Pull request and revision the decision covers.
    #[must_use]
    pub const fn subject(&self) -> &MergeSubject {
        &self.subject
    }
    /// Work type the decision covers.
    #[must_use]
    pub const fn work_type(&self) -> &Text {
        &self.work_type
    }
    /// Level the owner saw.
    #[must_use]
    pub const fn assessed(&self) -> ReadinessLevel {
        self.assessed
    }
    /// Required level the owner accepted missing.
    #[must_use]
    pub const fn required(&self) -> ReadinessLevel {
        self.required
    }
    /// The reason the owner approved.
    #[must_use]
    pub const fn reason(&self) -> &Text {
        &self.reason
    }
    /// Roger Ask that holds the approval.
    #[must_use]
    pub const fn ask(&self) -> &ExternalRef {
        &self.ask
    }
}

/// The shortfall a request asks the owner to accept, checked against house
/// policy and the current assessment.
fn shortfall(
    house: &HouseConfig,
    readiness: &RepositoryReadiness,
    request: &BelowReadinessRequest,
) -> Result<(ReadinessLevel, ReadinessLevel), HouseError> {
    if readiness.house != house.house
        || readiness.repository != request.subject.repository
        || !house.repositories.contains(&readiness.repository)
    {
        return Err(HouseError::HouseSelection);
    }
    let assessed = readiness.level_for(&request.work_type);
    match house.merge_readiness.get(&request.work_type) {
        Some(&required) if assessed < required && !request.reason.as_str().trim().is_empty() => {
            Ok((assessed, required))
        }
        Some(_) | None => Err(HouseError::ReadinessDecision),
    }
}

/// The exact action text the owner approves. Roger compares it, so any
/// change to the work type, levels, or reason needs a new approval.
fn below_readiness_limits(
    request: &BelowReadinessRequest,
    assessed: ReadinessLevel,
    required: ReadinessLevel,
) -> Result<Text, HouseError> {
    Text::new(&format!(
        "Merge below readiness for work type {}: assessed {}, required {}. Reason: {}",
        request.work_type.as_str(),
        assessed.as_str(),
        required.as_str(),
        request.reason.as_str()
    ))
    .map_err(|_| HouseError::ReadinessDecision)
}

fn below_readiness_target(subject: &MergeSubject) -> Result<ExternalRef, HouseError> {
    ExternalRef::new(&format!(
        "pr:{}#{}",
        subject.repository,
        subject.number.get()
    ))
    .map_err(|_| HouseError::ReadinessDecision)
}

/// Build the Roger approval Ask for a below-readiness merge. The caller
/// persists it as the task's effect before it is sent; that record is what
/// [`accept_below_readiness`] later verifies. `revision` is the task's
/// evidence revision at the subject's head and base.
///
/// # Errors
/// Returns [`HouseError::HouseSelection`] for an assessment of another house
/// or repository, and [`HouseError::ReadinessDecision`] when the work type is
/// not below policy, the reason is blank, or the Ask would be invalid.
pub fn below_readiness_ask(
    house: &HouseConfig,
    readiness: &RepositoryReadiness,
    request: &BelowReadinessRequest,
    task: &TaskId,
    revision: EvidenceRevision,
) -> Result<RogerAsk, HouseError> {
    let (assessed, required) = shortfall(house, readiness, request)?;
    let subject = &request.subject;
    let ask = RogerAsk {
        binding: DecisionBinding {
            house: house.house.clone(),
            task: task.clone(),
            owner: DecisionOwner::Merge,
            repository: subject.repository.clone(),
            action: Permission::Merge,
            target: below_readiness_target(subject)?,
            revision,
            subject: Some(EvidenceSubject {
                head: subject.head.clone(),
                base: Some(subject.base.clone()),
            }),
            limits: below_readiness_limits(request, assessed, required)?,
        },
        kind: AskKind::Approval,
        risk: AskRisk::Irreversible,
        title: Text::new("Merge below the required readiness?")
            .map_err(|_| HouseError::ReadinessDecision)?,
        body: Text::new(&format!(
            "House policy requires {} readiness for {} work in {} before a merge; it is assessed {}. Approving allows merging PR #{} at head {} only.",
            required.as_str(),
            request.work_type.as_str(),
            subject.repository,
            assessed.as_str(),
            subject.number.get(),
            subject.head
        ))
        .map_err(|_| HouseError::ReadinessDecision)?,
        supersedes: None,
    };
    ask.validate().map_err(|_| HouseError::ReadinessDecision)?;
    Ok(ask)
}

/// Verify an owner's approval of a below-readiness merge from the house
/// store. The task must hold a Roger approval Ask for exactly this house,
/// pull request, head, base, work type, levels, and reason, and Roger must
/// have acknowledged it. `answer` is the Roger reply read for that Ask; it
/// counts only as an explicit, passkey-confirmed approval of the persisted
/// binding under the house's Roger scope. The decider is whoever holds that
/// house's Roger approval; nothing the caller states identifies them.
///
/// # Errors
/// Returns the [`below_readiness_ask`] refusals,
/// [`HouseError::DecisionRecord`] when the task cannot be read from this
/// house's store, and [`HouseError::ReadinessNotApproved`] when no matching
/// Ask was persisted and acknowledged or the answer does not approve it.
pub fn accept_below_readiness(
    house: &HouseConfig,
    readiness: &RepositoryReadiness,
    request: &BelowReadinessRequest,
    store: &HouseStore,
    task: &TaskId,
    scope: &HouseScope,
    answer: &[u8],
) -> Result<BelowReadinessDecision, HouseError> {
    let (assessed, required) = shortfall(house, readiness, request)?;
    if store.house() != &house.house {
        return Err(HouseError::HouseSelection);
    }
    let subject = &request.subject;
    let target = below_readiness_target(subject)?;
    let limits = below_readiness_limits(request, assessed, required)?;
    let persisted = EvidenceSubject {
        head: subject.head.clone(),
        base: Some(subject.base.clone()),
    };
    let record = store.task(task).map_err(|_| HouseError::DecisionRecord)?;
    let (binding, ask) = record
        .effects()
        .iter()
        .rev()
        .find_map(|effect| {
            let (Effect::Roger(roger), EffectState::Applied { receipt, .. }) =
                (effect.request().effect(), effect.state())
            else {
                return None;
            };
            let binding = &roger.ask.binding;
            (roger.ask.kind == AskKind::Approval
                && &roger.requester == scope.requester()
                && binding.house == house.house
                && &binding.task == task
                && binding.owner == DecisionOwner::Merge
                && binding.action == Permission::Merge
                && binding.repository == subject.repository
                && binding.target == target
                && binding.subject.as_ref() == Some(&persisted)
                && binding.limits == limits)
                .then(|| (binding, receipt.reference()))
        })
        .ok_or(HouseError::ReadinessNotApproved)?;
    match validate_answer(scope, binding, ask, answer) {
        Ok(DecisionStatus::Approved) => Ok(BelowReadinessDecision {
            house: house.house.clone(),
            task: task.clone(),
            subject: subject.clone(),
            work_type: request.work_type.clone(),
            assessed,
            required,
            reason: request.reason.clone(),
            ask: ask.clone(),
        }),
        Ok(
            DecisionStatus::Unanswered
            | DecisionStatus::Expired
            | DecisionStatus::Closed
            | DecisionStatus::Rejected
            | DecisionStatus::Instructions(_),
        )
        | Err(_) => Err(HouseError::ReadinessNotApproved),
    }
}

/// A work type whose assessed level is below the level house policy requires.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shortfall {
    work_type: Text,
    assessed: ReadinessLevel,
    required: ReadinessLevel,
}

/// House authority issued with the readiness behind its merge grants. It is
/// the only source of a gate merge grant, so every merge effect is checked
/// against readiness.
#[derive(Debug, Clone)]
#[must_use]
pub struct IssuedAuthority {
    authority: HouseGrants,
    shortfalls: BTreeMap<Repository, Vec<Shortfall>>,
    accepted_below: Vec<BelowReadinessDecision>,
}

impl IssuedAuthority {
    /// The house's standing grants, including configured merge grants.
    #[must_use]
    pub const fn grants(&self) -> &HouseGrants {
        &self.authority
    }

    /// Owner approvals supplied at issuance that match a current shortfall.
    #[must_use]
    pub fn accepted_below(&self) -> &[BelowReadinessDecision] {
        &self.accepted_below
    }

    /// Check readiness for merging `subject`. Every work type below policy
    /// in its repository needs an owner approval for exactly this pull
    /// request, head, base, and levels. Returns those approvals so the caller
    /// can record them with the merge.
    ///
    /// # Errors
    /// Returns [`HouseError::BelowReadiness`] naming the first unaccepted
    /// work type's required and assessed level.
    pub fn merge_clearance(
        &self,
        subject: &MergeSubject,
    ) -> Result<Vec<&BelowReadinessDecision>, HouseError> {
        let Some(shortfalls) = self.shortfalls.get(&subject.repository) else {
            return Ok(Vec::new());
        };
        shortfalls
            .iter()
            .map(|shortfall| {
                self.accepted_below
                    .iter()
                    .find(|decision| {
                        &decision.subject == subject
                            && decision.work_type == shortfall.work_type
                            && decision.assessed == shortfall.assessed
                            && decision.required == shortfall.required
                    })
                    .ok_or(HouseError::BelowReadiness {
                        required: shortfall.required,
                        assessed: shortfall.assessed,
                    })
            })
            .collect()
    }
}

impl HouseConfig {
    /// Issue house authority with readiness recorded for every configured
    /// [`Permission::Merge`] grant. For each repository with a merge grant,
    /// every work type in `merge_readiness` whose level is below policy is
    /// recorded as a shortfall; [`IssuedAuthority::merge_clearance`] then
    /// refuses a merge there unless `decisions` holds an owner approval for
    /// that exact pull request. A repository without an assessment counts as
    /// unready, and several assessments count at their lowest level.
    /// Approvals for another house or a stale shortfall are dropped.
    ///
    /// # Errors
    /// Returns the validation errors of [`HouseConfig::authority`].
    pub fn issue_authority(
        &self,
        readiness: &[RepositoryReadiness],
        decisions: &[BelowReadinessDecision],
    ) -> Result<IssuedAuthority, HouseError> {
        let authority = self.build_authority()?;
        let merge_repositories: BTreeSet<&Repository> = self
            .grants
            .iter()
            .filter(|grant| grant.permission == Permission::Merge)
            .filter_map(|grant| match &grant.scope {
                GrantScope::Repository(repository) => Some(repository),
                GrantScope::House => None,
            })
            .collect();
        let mut shortfalls = BTreeMap::new();
        for repository in merge_repositories {
            let below: Vec<Shortfall> = self
                .merge_readiness
                .iter()
                .filter_map(|(work_type, &required)| {
                    let assessed = readiness
                        .iter()
                        .filter(|r| r.house == self.house && &r.repository == repository)
                        .map(|r| r.level_for(work_type))
                        .min()
                        .unwrap_or(ReadinessLevel::Unready);
                    (assessed < required).then(|| Shortfall {
                        work_type: work_type.clone(),
                        assessed,
                        required,
                    })
                })
                .collect();
            if !below.is_empty() {
                shortfalls.insert(repository.clone(), below);
            }
        }
        let accepted_below = decisions
            .iter()
            .filter(|decision| {
                decision.house == self.house
                    && shortfalls
                        .get(&decision.subject.repository)
                        .is_some_and(|below| {
                            below.iter().any(|shortfall| {
                                shortfall.work_type == decision.work_type
                                    && shortfall.assessed == decision.assessed
                                    && shortfall.required == decision.required
                            })
                        })
            })
            .cloned()
            .collect();
        Ok(IssuedAuthority {
            authority,
            shortfalls,
            accepted_below,
        })
    }
}
