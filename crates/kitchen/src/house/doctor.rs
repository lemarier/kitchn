use super::{
    HouseError, LabelPreview, LabelStatus, RepositoryConfig, RepositoryLabel, Workflow,
    missing_capabilities, preview_labels, workflow_requirements,
};
use crate::{
    HouseId,
    adoption::{HouseRegistry, ResolvedInstructions, resolve_instructions},
    contracts::{Capability, CapabilitySet, Repository},
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
    /// Every known incomplete setup item and its next step.
    pub findings: Vec<DoctorFinding>,
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
    let access = evidence.map_or(AccessStatus::Unobserved, |evidence| evidence.access);
    if access != AccessStatus::Available {
        findings.push(DoctorFinding { code: DoctorCode::Access, message: format!("House-scoped repository access: {access:?}."), next_step: format!("Configure {} access in the external credential provider, then probe {} through the house-scoped integration and rerun doctor; never put credential values in .kitchen.json.", house.house, repository.repository) });
    }
    Ok(DoctorReport {
        house: house.house,
        repository: repository.repository.clone(),
        instructions,
        labels,
        missing_capabilities,
        access,
        findings,
    })
}
