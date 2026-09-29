use super::{
    Assessed, HouseError, LabelPreview, LabelStatus, ReadinessEvidence, RepositoryConfig,
    RepositoryLabel, RepositoryReadiness, StackTool, Workflow, assess, missing_capabilities,
    preview_labels, workflow_requirements,
};
use crate::{
    HouseId,
    adoption::{HouseRegistry, ResolvedInstructions, resolve_instructions},
    contracts::{Capability, CapabilitySet, Repository},
    scheduling::{BudgetError, ScheduleEvidence, SchedulePolicy, TokenUsage, UndeliveredReport},
    selection::OfferedModels,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Read-only access evidence. This reports access, never authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessStatus {
    /// No scoped access probe was supplied.
    Unobserved,
    /// The integration observed access to this house/repository.
    Available,
    /// The integration could not obtain required access.
    Missing,
}
/// Integration-supplied observations must identify the exact scope. They are
/// diagnostic evidence, not reusable credential or action authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DoctorEvidence {
    /// House whose access was checked.
    pub house: HouseId,
    /// Repository whose labels/access were checked.
    pub repository: Repository,
    /// Fully/partially supported scheduled execution capabilities.
    pub capabilities: CapabilitySet,
    /// `None` means labels were not observed; empty means observed and absent.
    pub labels: Option<Vec<RepositoryLabel>>,
    /// Access result, independent of granted authority.
    pub access: AccessStatus,
    /// Models each installed agent reports offering; `None` means not observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_models: Option<Vec<OfferedModels>>,
    /// The configured stack tool as detected on this host; `None` when it
    /// was not probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_tool: Option<StackToolStatus>,
    /// The house's schedules and their recent runs; `None` means not observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<ScheduleEvidence>,
    /// Repository readiness observations; `None` reports every fact as unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ReadinessEvidence>,
    /// Budget exhaustions whose owner report had no destination, as
    /// recorded by the budget tick; empty when there are none or they were
    /// not read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undelivered_budget_reports: Vec<UndeliveredReport>,
}

/// Whether the house's stack tool is usable on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum StackToolStatus {
    /// The tool answered with its version.
    Installed {
        /// The version it reported.
        version: String,
    },
    /// The tool or the program that hosts it is not installed.
    Missing,
}
/// A precise remaining setup action. No command here is executed automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorFinding {
    /// Stable diagnostic category.
    pub code: DoctorCode,
    /// Human-readable diagnosis without credentials.
    pub message: String,
    /// Exact next operation or decision to resolve it.
    pub next_step: String,
}
/// Stable categories for structured diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorCode {
    /// Pinned files absent or changed.
    Instructions,
    /// Forge access was not positively observed.
    Access,
    /// Label observation/creation/manual conflict resolution needed.
    Labels,
    /// Scheduler/backend capability is absent or partial.
    Capability,
    /// A configured model is not offered by the installed agent, or was not checked.
    AgentModel,
    /// A scheduled workflow's resolved selection names a model or effort the
    /// schedule backend cannot enforce.
    ScheduleAgent,
    /// A legacy `.kitchen.json` remains in the working tree.
    LegacyBinding,
    /// The configured stack tool is missing or was not probed.
    StackTool,
    /// Schedule limits cannot be checked or enforced as configured.
    ScheduleBudget,
    /// A schedule's precheck mostly reports idle; a recommendation only.
    IdleSchedule,
    /// A work type is below the readiness house policy requires for a merge grant.
    Readiness,
    /// A budget exhaustion could not be reported to its owner because the
    /// budget schedule has no report destination.
    BudgetReport,
}
impl DoctorFinding {
    /// Report a leftover legacy binding file. Kitchen never deletes it.
    #[must_use]
    pub fn legacy_binding(path: &std::path::Path) -> Self {
        Self {
            code: DoctorCode::LegacyBinding,
            message: format!(
                "Legacy repository binding {} is still in the working tree; Kitchen no longer reads it.",
                path.display()
            ),
            next_step: "Import it with kitchen house import if the registry lacks this binding, then delete the file yourself; Kitchen does not delete repository files.".into(),
        }
    }
}
/// Read-only setup report. Healthy means configuration evidence is complete,
/// never that an external effect is authorized or a workflow was activated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    /// Selected house.
    pub house: HouseId,
    /// Selected repository.
    pub repository: Repository,
    /// Verified task-loadable instructions, or a named finding.
    pub instructions: Option<ResolvedInstructions>,
    /// Per-workflow, deduplicated label preview.
    pub labels: Vec<LabelPreview>,
    /// Missing/partial scheduled capabilities by workflow.
    pub missing_capabilities: BTreeMap<Workflow, BTreeSet<Capability>>,
    /// Scoped access observation.
    pub access: AccessStatus,
    /// Diagnostic merge readiness; it never grants merge authority.
    pub readiness: RepositoryReadiness,
    /// Every known incomplete setup item and its next step.
    pub findings: Vec<DoctorFinding>,
    /// Suggestions that do not make setup incomplete, such as mostly idle
    /// schedules. Kitchen acts on none of them.
    #[serde(default)]
    pub recommendations: Vec<DoctorFinding>,
}
impl DoctorReport {
    /// Whether all supplied diagnostic requirements were satisfied.
    pub fn healthy(&self) -> bool {
        self.findings.is_empty()
    }
    /// Render the same structured findings for a person.
    pub fn human_readable(&self) -> String {
        let mut text = format!(
            "House: {}\nRepository: {}\nDoctor: {}\n",
            self.house,
            self.repository,
            if self.healthy() {
                "ready (no authority granted)"
            } else {
                "setup incomplete"
            }
        );
        for label in self
            .labels
            .iter()
            .filter(|label| label.status == LabelStatus::Drift)
        {
            text.push_str(&format!(
                "\nLabel {}: metadata drift; existing color and description retained.\n",
                label.requirement.name
            ));
        }
        for finding in &self.findings {
            text.push_str(&format!(
                "\n{}\nNext: {}\n",
                finding.message, finding.next_step
            ));
        }
        for recommendation in &self.recommendations {
            text.push_str(&format!(
                "\nRecommendation: {}\nConsider: {}\n",
                recommendation.message, recommendation.next_step
            ));
        }
        text.push_str(&format!(
            "\nReadiness: {} (diagnostic; grants no merge authority)\n",
            self.readiness.level.as_str()
        ));
        text.push_str(&format!(
            "Required checks: {}\n",
            match &self.readiness.required_checks {
                Assessed::Known(checks) if checks.is_empty() => "none".to_owned(),
                Assessed::Known(checks) => checks.iter().cloned().collect::<Vec<_>>().join(", "),
                Assessed::Unknown => "unknown".to_owned(),
            }
        ));
        for (check, history) in &self.readiness.check_history {
            match history {
                Assessed::Known(history) => text.push_str(&format!(
                    "Check {check}: {} passed, {} failed, {} inconclusive, {} flaky head(s)\n",
                    history.passed, history.failed, history.inconclusive, history.flaky_heads
                )),
                Assessed::Unknown => text.push_str(&format!("Check {check}: history unknown\n")),
            }
        }
        for work_type in self.readiness.acceptance_checks.keys() {
            text.push_str(&format!(
                "Work type {}: {}\n",
                work_type.as_str(),
                self.readiness.level_for(work_type).as_str()
            ));
        }
        for gap in &self.readiness.gaps {
            text.push_str(&format!("{}\nNext: {}\n", gap.message(), gap.next_step()));
        }
        if let Some(instructions) = &self.instructions {
            text.push_str(&format!(
                "\nPinned instructions: {}\n",
                instructions.entrypoint.display()
            ));
        }
        text.push_str("\nNo workers, schedules, labels, or external actions were activated.\n");
        text
    }
}

/// Diagnose an adopted repository with optional scoped read-only observations.
/// Unknown backend/access/labels are reported as gaps, not successful probes.
pub fn doctor(
    registry: &HouseRegistry,
    repository: &RepositoryConfig,
    evidence: Option<&DoctorEvidence>,
) -> Result<DoctorReport, HouseError> {
    let house = registry.load(&repository.house)?;
    repository.validate(&house)?;
    if let Some(evidence) = evidence
        && (evidence.house != house.house || evidence.repository != repository.repository)
    {
        return Err(HouseError::HouseSelection);
    }
    let mut findings = Vec::new();
    let instructions = match resolve_instructions(registry.root(), &house, None) {
        Ok(instructions) => Some(instructions),
        Err(error) => {
            findings.push(DoctorFinding { code: DoctorCode::Instructions, message: error.to_string(), next_step: format!("Obtain the verified bundle for Kitchen {} and house guidance {}, then run kitchen house sync --registry '{}' --house {} --bundle <verified-bundle.json>.", house.kitchen, house.guidance, registry.root().display(), house.house) });
            None
        }
    };
    let declarations: Vec<_> = repository
        .workflows
        .iter()
        .copied()
        .map(workflow_requirements)
        .collect();
    let labels = preview_labels(
        &declarations,
        evidence.and_then(|evidence| evidence.labels.as_deref()),
    )?;
    for label in &labels {
        let action = match label.status {
            LabelStatus::Present | LabelStatus::Drift => continue,
            LabelStatus::Unobserved => {
                "Use the house-scoped GitHub integration to read this repository's labels, then rerun doctor with that observation."
            }
            LabelStatus::Missing => {
                "Preview and approve creation of this missing label through the house-scoped GitHub setup integration; recheck before creating it."
            }
            LabelStatus::Conflict => {
                "Ask the repository owner to resolve the conflicting declaration or existing label manually; Kitchen will not rename, recolor, or delete it."
            }
        };
        findings.push(DoctorFinding {
            code: DoctorCode::Labels,
            message: format!(
                "Label {}: {:?} (workflows: {}).",
                label.requirement.name,
                label.status,
                label
                    .workflows
                    .iter()
                    .map(|workflow| workflow.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            next_step: action.into(),
        });
    }
    let capabilities =
        evidence.map_or_else(CapabilitySet::new, |evidence| evidence.capabilities.clone());
    let missing_capabilities = missing_capabilities(&repository.workflows, &capabilities);
    for (workflow, missing) in &missing_capabilities {
        findings.push(DoctorFinding { code: DoctorCode::Capability, message: format!("Scheduled {} requires: {}.", workflow.as_str(), missing.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")), next_step: "Configure a backend that positively supports each named capability and rerun doctor with its scoped observation; keep scheduling disabled until then. Interactive single-agent work remains separate.".into() });
    }
    if let Some(agents) = &house.agents {
        let offered = evidence.and_then(|evidence| evidence.agent_models.as_deref());
        let (message, missing) = match offered {
            None => (
                "Configured agent models were not checked against the installed agents",
                agents.configured_models(),
            ),
            Some(offered) => (
                "Configured agent models the installed agents do not offer",
                agents.unoffered_models(offered),
            ),
        };
        if !missing.is_empty() {
            let names = missing
                .iter()
                .map(|(agent, model)| format!("{}:{model}", agent.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(DoctorFinding { code: DoctorCode::AgentModel, message: format!("{message}: {names}."), next_step: "List the models each installed agent offers and rerun doctor with that observation. Correct the house agents policy for any model that is not offered; Kitchen refuses launches it cannot provide and never substitutes another model.".into() });
        }
    }
    if let Some(agents) = &house.agents
        && !capabilities.supports(Capability::AgentSelectModel)
    {
        for workflow in &repository.workflows {
            let request = workflow
                .schedule_request()
                .map_err(|_| HouseError::InvalidInput)?;
            let selection = agents.resolve(&request).selection;
            let named: Vec<String> = [
                selection
                    .model
                    .as_ref()
                    .map(|model| format!("model {model}")),
                selection
                    .effort
                    .as_ref()
                    .map(|effort| format!("effort {effort}")),
            ]
            .into_iter()
            .flatten()
            .collect();
            if named.is_empty() {
                continue;
            }
            findings.push(DoctorFinding { code: DoctorCode::ScheduleAgent, message: format!("Scheduled {workflow} resolves to {} {}, which the schedule backend cannot enforce; Kitchen refuses to install it rather than run the agent's default.", selection.agent.as_str(), named.join(" and ")), next_step: format!("Add a family-only house agents rule for role {} and work type {workflow}, so the schedule stays a trigger and the workers it hands work to carry the model; or configure a schedule backend that fully supports {}.", workflow.role(), Capability::AgentSelectModel) });
        }
    }
    if let Some(tool) = house.stack_tool
        && let Some(finding) = stack_tool_finding(
            tool,
            &repository.workflows,
            evidence.and_then(|evidence| evidence.stack_tool.as_ref()),
        )
    {
        findings.push(finding);
    }
    let recommendations = diagnose_schedules(
        house.schedules.as_ref(),
        &house.house,
        evidence.and_then(|evidence| evidence.schedules.as_ref()),
        &mut findings,
    )?;
    let readiness = assess(
        &house,
        repository,
        evidence.and_then(|evidence| evidence.readiness.as_ref()),
    )?;
    for (work_type, required) in &house.merge_readiness {
        let assessed = readiness.level_for(work_type);
        if assessed < *required {
            findings.push(DoctorFinding { code: DoctorCode::Readiness, message: format!("Work type {} requires {} readiness before a merge grant; assessed {}.", work_type.as_str(), required.as_str(), assessed.as_str()), next_step: "Close the readiness gaps below and rerun doctor, or keep merges for this work type manual. Below the level, a merge needs an owner's Roger approval with a reason for that exact pull request; readiness never grants merge authority.".into() });
        }
    }
    for report in evidence.map_or(&[][..], |evidence| &evidence.undelivered_budget_reports) {
        findings.push(DoctorFinding { code: DoctorCode::BudgetReport, message: format!("Schedule {} was paused for exhausting its {} ({} of {}) in the window ending at {} (Unix ms), but its owner was not told: the budget schedule has no report destination.", report.consumer, report.exhausted.limit, report.exhausted.used, report.exhausted.allowed, report.window.end.as_unix_millis()), next_step: "Reinstall the budget schedule with kitchen budget install --report-issue owner/repo#N naming a house posting destination, or tell the schedule's owner yourself; it stays paused until the owner activates it.".into() });
    }
    let access = evidence.map_or(AccessStatus::Unobserved, |evidence| evidence.access);
    if access != AccessStatus::Available {
        findings.push(DoctorFinding { code: DoctorCode::Access, message: format!("House-scoped repository access: {access:?}."), next_step: format!("Configure {} access in the external credential provider, then probe {} through the house-scoped integration and rerun doctor; never put credential values in repository files.", house.house, repository.repository) });
    }
    Ok(DoctorReport {
        house: house.house,
        repository: repository.repository.clone(),
        instructions,
        labels,
        missing_capabilities,
        access,
        readiness,
        findings,
        recommendations,
    })
}

/// Workflows that create dependent branches and stacked pull requests.
const STACKING_WORKFLOWS: [Workflow; 1] = [Workflow::Pickup];

/// The blocking finding for a configured stack tool that a stacking
/// workflow needs and that is missing or was not probed.
#[must_use]
pub fn stack_tool_finding(
    tool: StackTool,
    workflows: &BTreeSet<Workflow>,
    status: Option<&StackToolStatus>,
) -> Option<DoctorFinding> {
    let needed: Vec<&str> = STACKING_WORKFLOWS
        .iter()
        .filter(|workflow| workflows.contains(workflow))
        .map(|workflow| workflow.as_str())
        .collect();
    if needed.is_empty() {
        return None;
    }
    let (state, next_step) = match status {
        Some(StackToolStatus::Installed { .. }) => return None,
        Some(StackToolStatus::Missing) => (
            "is not installed",
            format!(
                "Install {} on this host, then rerun doctor with its probe; dependent pull requests stay blocked until then.",
                tool.command()
            ),
        ),
        None => (
            "was not probed",
            format!(
                "Probe {} on this host and rerun doctor with the result; dependent pull requests stay blocked until then.",
                tool.command()
            ),
        ),
    };
    Some(DoctorFinding {
        code: DoctorCode::StackTool,
        message: format!(
            "Stack tool {} {state} (needed by: {}).",
            tool.command(),
            needed.join(", ")
        ),
        next_step,
    })
}

/// Schedule limit findings, and idle-schedule recommendations.
fn diagnose_schedules(
    policy: Option<&SchedulePolicy>,
    house: &HouseId,
    evidence: Option<&ScheduleEvidence>,
    findings: &mut Vec<DoctorFinding>,
) -> Result<Vec<DoctorFinding>, HouseError> {
    let Some(policy) = policy else {
        if evidence.is_some_and(|evidence| !evidence.schedules.is_empty()) {
            findings.push(DoctorFinding { code: DoctorCode::ScheduleBudget, message: format!("House {house} has schedules but no schedule policy; their intervals and usage are not limited."), next_step: "Add a schedules policy with a minimum interval, a usage window, and house and per-schedule budgets to the house configuration.".into() });
        }
        return Ok(Vec::new());
    };
    let Some(evidence) = evidence else {
        findings.push(DoctorFinding { code: DoctorCode::ScheduleBudget, message: "Schedule usage was not observed; budgets and idle schedules were not checked.".into(), next_step: "Observe this house's schedules and their recent runs through the backend adapter, then rerun doctor with that evidence.".into() });
        return Ok(Vec::new());
    };
    let unenforceable = policy
        .unenforceable_token_budgets(house, evidence)
        .map_err(house_error)?;
    for schedule in policy
        .unverifiable_run_budgets(house, evidence)
        .map_err(house_error)?
    {
        findings.push(DoctorFinding { code: DoctorCode::ScheduleBudget, message: format!("Schedule {}: its {} observed agent runs this window may not be all of them, because the run history does not reach the window's start, so its run budget cannot be verified.", schedule.consumer, schedule.usage.runs), next_step: "Use a shorter usage window or a run budget below the retained run history; activation is refused until the window is fully observed.".into() });
    }
    for schedule in unenforceable {
        findings.push(DoctorFinding { code: DoctorCode::ScheduleBudget, message: format!("Schedule {}: usage was unknown for most of its {} agent runs this window ({}), so its token budget cannot be enforced; its run budget is the effective limit.", schedule.consumer, schedule.usage.runs, describe_tokens(schedule.usage.tokens)), next_step: "Use a backend that reports run usage, or set the run budget to the spend you accept; unknown usage is never counted as zero.".into() });
    }
    Ok(policy
        .idle_schedules(evidence)
        .into_iter()
        .map(|idle| DoctorFinding { code: DoctorCode::IdleSchedule, message: format!("Schedule {}: the precheck reported idle on {} of its {} recent runs, which used {}.", idle.consumer, idle.idle_runs, idle.runs, describe_tokens(idle.tokens)), next_step: "Lengthen its interval or make its precheck cheaper; Kitchen changes nothing.".into() })
        .collect())
}

/// How a refused schedule assessment surfaces in doctor: evidence for
/// another house and policy relaxation keep their own refusals.
fn house_error(error: BudgetError) -> HouseError {
    match error {
        BudgetError::HouseMismatch => HouseError::HouseSelection,
        BudgetError::Relaxation { .. } => HouseError::PolicyRelaxation,
        BudgetError::InvalidPolicy
        | BudgetError::UnreadableRecurrence
        | BudgetError::IntervalTooShort { .. }
        | BudgetError::Overcommitted { .. }
        | BudgetError::Exhausted { .. }
        | BudgetError::IncompleteEvidence { .. }
        | BudgetError::UnobservedSchedule { .. }
        | BudgetError::InvalidEvidence => HouseError::InvalidInput,
    }
}

fn describe_tokens(tokens: TokenUsage) -> String {
    match tokens {
        TokenUsage::Known { tokens } => format!("{tokens} tokens"),
        TokenUsage::Unknown {
            known_tokens,
            unknown_runs,
        } => format!(
            "at least {known_tokens} tokens, with usage unknown for {unknown_runs} {}",
            if unknown_runs == 1 { "run" } else { "runs" }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::ScheduleLimit;

    #[test]
    fn budget_refusals_keep_their_house_error() {
        assert!(matches!(
            house_error(BudgetError::HouseMismatch),
            HouseError::HouseSelection
        ));
        assert!(matches!(
            house_error(BudgetError::Relaxation {
                limit: ScheduleLimit::HouseRuns
            }),
            HouseError::PolicyRelaxation
        ));
    }

    #[test]
    fn other_budget_refusals_are_invalid_input() {
        assert!(matches!(
            house_error(BudgetError::InvalidEvidence),
            HouseError::InvalidInput
        ));
        assert!(matches!(
            house_error(BudgetError::InvalidPolicy),
            HouseError::InvalidInput
        ));
    }
}
