use std::collections::BTreeSet;

use crate::{
    HouseId,
    adoption::{InstructionAsset, InstructionBundle, RelativePath, role_cards_digest},
    contracts::CommitId,
    house::HouseError,
};

/// Entry point of the default guidance.
const ENTRYPOINT: &str = "README.md";
/// The default house guidance, embedded in this Kitchen revision.
const DEFAULT_GUIDANCE: &str = include_str!("default-guidance.md");

/// The guidance a house gets when `house init` is given no bundle: this
/// Kitchen revision's role cards and a short generic entry point, pinned at
/// the same revision as Kitchen itself (`guidance == kitchen`).
///
/// # Errors
/// Never fails for the embedded asset; path validation is fallible only in
/// type.
pub fn default_guidance(
    house: &HouseId,
    kitchen: &CommitId,
) -> Result<InstructionBundle, HouseError> {
    let entrypoint = RelativePath::new(ENTRYPOINT)?;
    Ok(InstructionBundle {
        schema: 1,
        house: house.clone(),
        kitchen: kitchen.clone(),
        role_cards_digest: role_cards_digest(),
        guidance: kitchen.clone(),
        entrypoint: entrypoint.clone(),
        notices: BTreeSet::new(),
        assets: vec![InstructionAsset {
            path: entrypoint,
            contents: DEFAULT_GUIDANCE.to_owned(),
        }],
    })
}
