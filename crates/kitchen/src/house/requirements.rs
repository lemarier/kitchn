use super::HouseError;
use crate::contracts::{Capability, CapabilitySet};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

/// Configurable workflows; enabling selection alone grants no execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Workflow {
    /// Select eligible issues.
    Pickup,
    /// Resolve specification questions.
    Triage,
    /// Evaluate exact-head merge evidence.
    Gate,
    /// Maintain issue hygiene.
    Gardener,
    /// Inspect resource cleanup.
    Dishwasher,
    /// Inspect delivered work.
    Inspector,
}
impl Workflow {
    /// Supported configuration names.
    pub const ALL: [Self; 6] = [
        Self::Pickup,
        Self::Triage,
        Self::Gate,
        Self::Gardener,
        Self::Dishwasher,
        Self::Inspector,
    ];
    /// Stable CLI name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pickup => "pickup",
            Self::Triage => "triage",
            Self::Gate => "gate",
            Self::Gardener => "gardener",
            Self::Dishwasher => "dishwasher",
            Self::Inspector => "inspector",
        }
    }
}
impl FromStr for Workflow {
    type Err = HouseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|workflow| workflow.as_str() == value)
            .ok_or(HouseError::InvalidInput)
    }
}

/// A label's closed workflow meaning, independent of its display name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelPurpose {
    /// Eligible pickup input.
    Ready,
    /// Mirrors a durable claim; never establishes ownership itself.
    Working,
    /// Requires a specification decision.
    NeedsSpec,
    /// Excludes automatic pickup or gate effects.
    HumanOnly,
    /// Requires human review of a gate/repair handoff.
    NeedsHumanReview,
}
/// A creation declaration consumed by the forge integration (#7).
/// Existing labels must never be renamed, recolored or deleted to satisfy it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelRequirement {
    /// Semantic use.
    pub purpose: LabelPurpose,
    /// Exact desired label name.
    pub name: String,
    /// Six hexadecimal RGB digits, without a leading hash.
    pub color: String,
    /// Description used only when creating a missing label.
    pub description: String,
}
/// Repository and backend requirements for one selected workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowRequirements {
    /// Declaring workflow.
    pub workflow: Workflow,
    /// Labels to inspect and create only if missing.
    pub labels: Vec<LabelRequirement>,
    /// Required backend capabilities for scheduled execution.
    pub capabilities: BTreeSet<Capability>,
}

/// Generic declarations; house/domain guidance remains separate.
pub fn workflow_requirements(workflow: Workflow) -> WorkflowRequirements {
    use LabelPurpose::{HumanOnly, NeedsHumanReview, NeedsSpec, Ready, Working};
    let purposes: &[LabelPurpose] = match workflow {
        Workflow::Pickup => &[Ready, Working, NeedsSpec, HumanOnly],
        Workflow::Triage => &[NeedsSpec, Ready, HumanOnly],
        Workflow::Gate => &[NeedsHumanReview, HumanOnly],
        Workflow::Gardener => &[Ready, Working, NeedsSpec, HumanOnly],
        Workflow::Dishwasher | Workflow::Inspector => &[],
    };
    let mut capabilities = BTreeSet::from([
        Capability::SchedulePrecheck,
        Capability::ScheduleSingleConsumer,
        Capability::ScheduleRunTimeout,
        Capability::WorkerLaunchReadiness,
    ]);
    match workflow {
        Workflow::Pickup => {
            capabilities.extend([
                Capability::WorkerLaunchIsolated,
                Capability::WorkerMessaging,
                Capability::WorkerStatusAndOutcome,
                Capability::HouseCredentials,
            ]);
        }
        Workflow::Triage | Workflow::Gate | Workflow::Gardener => {
            capabilities.insert(Capability::HouseCredentials);
        }
        Workflow::Dishwasher => {
            capabilities.extend([Capability::ResourceInventory, Capability::ResourceRelease]);
        }
        Workflow::Inspector => {}
    }
    let labels = purposes
        .iter()
        .map(|purpose| {
            let (name, color, description) = match purpose {
                Ready => (
                    "agent-ready",
                    "0e8a16",
                    "Ready for an explicitly authorized worker",
                ),
                Working => ("agent-working", "fbca04", "Mirrors a durable Kitchen claim"),
                NeedsSpec => ("needs-spec", "d876e3", "Specification decision required"),
                HumanOnly => ("human-only", "b60205", "Excluded from automatic work"),
                NeedsHumanReview => (
                    "needs-human-review",
                    "d93f0b",
                    "Human review required before proceeding",
                ),
            };
            LabelRequirement {
                purpose: *purpose,
                name: name.into(),
                color: color.into(),
                description: description.into(),
            }
        })
        .collect();
    WorkflowRequirements {
        workflow,
        labels,
        capabilities,
    }
}

/// Label inventory obtained by an authenticated, house-scoped forge reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryLabel {
    /// Existing name.
    pub name: String,
    /// Existing RGB color.
    pub color: String,
    /// Existing description.
    pub description: String,
}
/// Preview verdict for one required label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelStatus {
    /// Inventory was not supplied; absence is not proof a label is missing.
    Unobserved,
    /// Safe to propose creation; the integration must recheck before applying.
    Missing,
    /// Exact name, color and description already match.
    Present,
    /// Exact name exists with different color or description; informational only.
    Drift,
    /// Existing case differs, duplicate names exist, or declarations disagree.
    Conflict,
}
/// A deduplicated label requirement with all consumers and its preview verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelPreview {
    /// Desired label declaration.
    pub requirement: LabelRequirement,
    /// Enabled workflows consuming it.
    pub workflows: BTreeSet<Workflow>,
    /// No existing label is ever mutated by this verdict.
    pub status: LabelStatus,
}

/// Preview only enabled workflows. `None` inventory produces unknowns, never
/// creation requests. Disabling a workflow produces no removal operation.
pub fn preview_labels(
    requirements: &[WorkflowRequirements],
    observed: Option<&[RepositoryLabel]>,
) -> Result<Vec<LabelPreview>, HouseError> {
    if requirements.len() > Workflow::ALL.len()
        || observed.is_some_and(|labels| labels.len() > 4096)
    {
        return Err(HouseError::InvalidInput);
    }
    let mut previews: BTreeMap<String, LabelPreview> = BTreeMap::new();
    for declaration in requirements {
        if declaration.labels.len() > 64 {
            return Err(HouseError::InvalidInput);
        }
        for label in &declaration.labels {
            if label.name.is_empty()
                || label.name.len() > 50
                || label.name.chars().any(char::is_control)
                || label.color.len() != 6
                || !label.color.bytes().all(|byte| byte.is_ascii_hexdigit())
                || label.description.len() > 100
                || label.description.chars().any(char::is_control)
            {
                return Err(HouseError::InvalidInput);
            }
            let key = label.name.to_lowercase();
            if let Some(previous) = previews.get_mut(&key) {
                previous.workflows.insert(declaration.workflow);
                if previous.requirement != *label {
                    previous.status = LabelStatus::Conflict;
                }
                continue;
            }
            let status = observed.map_or(LabelStatus::Unobserved, |labels| {
                let matches: Vec<_> = labels
                    .iter()
                    .filter(|existing| existing.name.eq_ignore_ascii_case(&label.name))
                    .collect();
                match matches.as_slice() {
                    [] => LabelStatus::Missing,
                    [existing]
                        if existing.name == label.name
                            && existing.color.eq_ignore_ascii_case(&label.color)
                            && existing.description == label.description =>
                    {
                        LabelStatus::Present
                    }
                    [existing] if existing.name == label.name => LabelStatus::Drift,
                    _ => LabelStatus::Conflict,
                }
            });
            previews.insert(
                key,
                LabelPreview {
                    requirement: label.clone(),
                    workflows: BTreeSet::from([declaration.workflow]),
                    status,
                },
            );
        }
    }
    Ok(previews.into_values().collect())
}

/// Missing or partial scheduled capabilities, grouped by workflow. Interactive
/// single-agent work need not satisfy these scheduler requirements.
pub fn missing_capabilities(
    workflows: &BTreeSet<Workflow>,
    capabilities: &CapabilitySet,
) -> BTreeMap<Workflow, BTreeSet<Capability>> {
    workflows
        .iter()
        .filter_map(|workflow| {
            let missing: BTreeSet<_> = workflow_requirements(*workflow)
                .capabilities
                .into_iter()
                .filter(|capability| !capabilities.supports(*capability))
                .collect();
            (!missing.is_empty()).then_some((*workflow, missing))
        })
        .collect()
}
