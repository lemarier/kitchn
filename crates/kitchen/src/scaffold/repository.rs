//! Compose house selection, repository binding and template files in one plan.
use super::{FilePlan, RenderedFile, Template, VariableName};
use crate::{
    HouseId,
    adoption::{FileMode, HouseRegistry, REPOSITORY_CONFIG, RelativePath, encode, read_repository},
    contracts::Repository,
    house::{HouseError, RepositoryConfig, resolve_house},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Preview repository adoption without writing or selecting a house implicitly.
/// Existing bindings retain their workflows and stricter local requirements.
/// Templates cannot supply or override Kitchen's repository binding.
///
/// # Errors
/// Refuses missing/mismatched house or repository selection, invalid templates,
/// and unsafe destinations through the house installer.
pub fn plan_repository(
    registry: &HouseRegistry,
    target: &Path,
    house: Option<HouseId>,
    repository: Option<Repository>,
    template: &Template,
    variables: &BTreeMap<VariableName, String>,
) -> crate::Result<FilePlan> {
    let config = match read_repository(target) {
        Ok(existing) => {
            if house.as_ref().is_some_and(|house| *house != existing.house)
                || repository
                    .as_ref()
                    .is_some_and(|repo| *repo != existing.repository)
            {
                return Err(HouseError::HouseSelection.into());
            }
            existing
        }
        Err(HouseError::HouseSelection) => RepositoryConfig {
            schema: 1,
            house: house.ok_or(HouseError::HouseSelection)?,
            repository: repository.ok_or(HouseError::HouseSelection)?,
            workflows: BTreeSet::new(),
            additional_reviewers: BTreeSet::new(),
            additional_checks: BTreeSet::new(),
        },
        Err(error) => return Err(error.into()),
    };
    let houses = registry.houses()?;
    let house = resolve_house(&config, &houses)?;
    let mut rendered = template.render(&house.house, &house.guidance, variables)?;
    if rendered.files.iter().any(|file| {
        file.path.as_str().eq_ignore_ascii_case(REPOSITORY_CONFIG)
            || file
                .path
                .as_str()
                .to_ascii_lowercase()
                .starts_with(".kitchen.json/")
    }) {
        return Err(HouseError::InvalidInput.into());
    }
    rendered.files.push(RenderedFile {
        path: RelativePath::new(REPOSITORY_CONFIG)?,
        contents: String::from_utf8(encode(&config)?).map_err(|_| HouseError::InvalidInput)?,
        mode: FileMode::Regular,
        managed: false,
    });
    FilePlan::new(rendered, target)
}
