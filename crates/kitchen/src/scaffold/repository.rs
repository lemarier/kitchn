//! Compose house selection, the registry binding and template files in one plan.
use super::{FilePlan, Template, TemplateName, VariableName};
use crate::{
    HouseId,
    adoption::{HouseRegistry, InstallReport, LEGACY_REPOSITORY_CONFIG, verified_snapshot},
    contracts::Repository,
    house::{HouseError, REPOSITORY_BINDING_SCHEMA, RepositoryConfig, resolve_house},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

/// Preview repository adoption without writing or selecting a house implicitly.
/// Existing bindings retain their workflows and stricter local requirements.
///
/// The repository binding lives in the house registry, never in `target`:
/// the plan's files are only the template's. Without `repository`, the
/// repository is read from the remotes of the checkout at `target`. Templates
/// cannot render the legacy `.kitchen.json` binding file.
///
/// The template is resolved by name from the selected house's verified
/// instruction snapshot for its configured guidance revision, so the revision
/// recorded in provenance markers is the revision that supplied the content.
///
/// # Errors
/// Refuses missing/mismatched house or repository selection, a missing or
/// modified snapshot, a template absent from the pinned guidance, invalid
/// templates, and unsafe destinations through the house installer.
pub fn plan_repository(
    registry: &HouseRegistry,
    target: &Path,
    house: Option<HouseId>,
    repository: Option<Repository>,
    template: &TemplateName,
    variables: &BTreeMap<VariableName, String>,
) -> crate::Result<RepositoryPlan> {
    let (repository, existing) = match repository {
        Some(repository) => {
            let existing = registry.binding(&repository)?;
            (repository, existing)
        }
        None if target.is_dir() => registry.claims(target)?.setup_target()?,
        None => return Err(HouseError::HouseSelection.into()),
    };
    let binding = match &existing {
        Some(existing) => {
            if house.as_ref().is_some_and(|house| *house != existing.house) {
                return Err(HouseError::HouseSelection.into());
            }
            existing.clone()
        }
        None => RepositoryConfig {
            schema: REPOSITORY_BINDING_SCHEMA,
            house: house.ok_or(HouseError::HouseSelection)?,
            repository,
            workflows: BTreeSet::new(),
            additional_reviewers: BTreeSet::new(),
            additional_checks: BTreeSet::new(),
        },
    };
    let houses = registry.houses()?;
    let house = resolve_house(&binding, &houses.available)?;
    let (_, guidance) = verified_snapshot(registry.root(), house, None)?;
    let template = Template::from_guidance(&guidance.assets, template)?;
    let rendered = template.render(&house.house, &house.guidance, variables)?;
    if rendered.files.iter().any(|file| {
        let path = file.path.as_str().to_ascii_lowercase();
        path == LEGACY_REPOSITORY_CONFIG || path.starts_with(".kitchen.json/")
    }) {
        return Err(HouseError::InvalidInput.into());
    }
    Ok(RepositoryPlan {
        files: FilePlan::new(rendered, target)?,
        registry: registry.clone(),
        binding,
        existing,
    })
}

/// A previewed adoption: template files for the working tree and the
/// repository binding for the registry. Building it writes nothing.
#[derive(Debug, Clone)]
pub struct RepositoryPlan {
    files: FilePlan,
    registry: HouseRegistry,
    binding: RepositoryConfig,
    existing: Option<RepositoryConfig>,
}

impl RepositoryPlan {
    /// The template files and what applying does with each.
    #[must_use]
    pub const fn files(&self) -> &FilePlan {
        &self.files
    }

    /// The binding the registry holds after applying.
    #[must_use]
    pub const fn binding(&self) -> &RepositoryConfig {
        &self.binding
    }

    /// Whether applying stores a new binding in the registry.
    #[must_use]
    pub const fn adds_binding(&self) -> bool {
        self.existing.is_none()
    }

    /// Recheck the registry binding, create the planned file additions, then
    /// store a new binding. A binding that changed since planning refuses the
    /// apply before any file is written; a rerun of the same plan is safe.
    ///
    /// # Errors
    /// [`HouseError::Conflict`] for a changed binding, and the errors of
    /// [`FilePlan::apply`] and [`HouseRegistry::bind_repository`].
    pub fn apply(&self) -> crate::Result<InstallReport> {
        if self.registry.binding(&self.binding.repository)? != self.existing {
            return Err(HouseError::Conflict.into());
        }
        let report = self.files.apply()?;
        if self.existing.is_none() {
            self.registry.bind_repository(&self.binding)?;
        }
        Ok(report)
    }
}

/// The file preview followed by the registry binding line.
impl fmt::Display for RepositoryPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.files)?;
        writeln!(
            formatter,
            "Registry binding {} -> house {}: {} (stored outside the working tree)",
            self.binding.repository,
            self.binding.house,
            if self.adds_binding() {
                "add"
            } else {
                "unchanged"
            }
        )
    }
}
