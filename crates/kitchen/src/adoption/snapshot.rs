use super::{FileMode, NewFile, RelativePath, read_bounded};
use crate::{
    HouseId,
    contracts::{CommitId, Provenance, Role},
    house::{HouseConfig, HouseError, role_card},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// One instruction or notice from a caller-verified export of pinned guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionAsset {
    /// Destination below the immutable snapshot's `house/` directory.
    pub path: RelativePath,
    /// Exact UTF-8 content, including required notices.
    pub contents: String,
}
/// Portable guidance bundle. The caller authenticates its source before import;
/// pins and exact bytes are checked here, not a remote Git signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionBundle {
    /// Schema version; currently one.
    pub schema: u32,
    /// House that owns these instructions.
    pub house: HouseId,
    /// Kitchen revision supplying the role cards.
    pub kitchen: CommitId,
    /// Digest of the role cards exported from the caller-authenticated Kitchen pin.
    pub role_cards_digest: RoleCardsDigest,
    /// Guidance revision supplying the house assets.
    pub guidance: CommitId,
    /// Entry point within `assets`.
    pub entrypoint: RelativePath,
    /// Required notices; all must exist in `assets` and are retained verbatim.
    pub notices: BTreeSet<RelativePath>,
    /// Complete guidance and specialization assets.
    pub assets: Vec<InstructionAsset>,
}
impl InstructionBundle {
    /// Validate pins, bounds, entry point, notices and path uniqueness.
    pub fn validate(&self, house: &HouseConfig) -> Result<(), HouseError> {
        house.validate()?;
        if self.house != house.house
            || self.kitchen != house.kitchen
            || self.guidance != house.guidance
        {
            return Err(HouseError::PinMismatch);
        }
        if self.schema != 1
            || self.assets.is_empty()
            || self.assets.len() > 200
            || self.notices.len() > 64
        {
            return Err(HouseError::InvalidInput);
        }
        let mut paths = BTreeSet::new();
        for asset in &self.assets {
            if asset.contents.is_empty()
                || asset.contents.contains('\0')
                || !paths.insert(asset.path.clone())
            {
                return Err(HouseError::InvalidInput);
            }
        }
        if !paths.contains(&self.entrypoint) || !self.notices.is_subset(&paths) {
            return Err(HouseError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotManifest {
    bundle: InstructionBundle,
    roles: Vec<InstructionAsset>,
}

/// A task's immutable instruction reference. Store the provenance with the core
/// task and retain this path for new agents working on that same task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedInstructions {
    /// Owning house, preventing cross-house reuse.
    pub house: HouseId,
    /// Core task provenance contract.
    pub provenance: Provenance,
    /// Content identity retained independently of later binaries and manifests.
    pub role_cards_digest: RoleCardsDigest,
    /// Immutable snapshot directory; updates never delete old snapshots.
    pub snapshot: PathBuf,
    /// Exact entry point inside the snapshot.
    pub entrypoint: PathBuf,
}

impl ResolvedInstructions {
    /// Reverify the retained snapshot before a fresh agent joins an active task.
    /// Current policy is loaded only to confirm the house still exists; the
    /// task's original instruction pins remain unchanged by later updates.
    pub fn verify(&self, registry: &super::HouseRegistry) -> Result<(), HouseError> {
        let mut house = registry.load(&self.house)?;
        house.kitchen = self.provenance.kitchen.clone();
        house.guidance = self.provenance.house_guidance.clone();
        let verified = resolve_instructions(
            registry.root(),
            &house,
            self.provenance.repository_instructions.clone(),
        )?;
        if verified != *self {
            return Err(HouseError::UnverifiedSnapshot);
        }
        Ok(())
    }
}

pub(crate) fn snapshot_path(root: &Path, house: &HouseConfig) -> PathBuf {
    root.join("snapshots")
        .join(house.house.as_str())
        .join(format!("{}-{}", house.kitchen, house.guidance))
}

/// Install the selected pins. Existing snapshots must be byte-identical; no
/// overwrite, refresh, role activation, or authority grant is performed.
pub(crate) fn install_snapshot(
    root: &Path,
    house: &HouseConfig,
    bundle: &InstructionBundle,
) -> Result<ResolvedInstructions, HouseError> {
    super::registry::ensure_external(root)?;
    bundle.validate(house)?;
    if bundle.role_cards_digest != role_cards_digest() {
        return Err(HouseError::PinMismatch);
    }
    let manifest = SnapshotManifest {
        bundle: bundle.clone(),
        roles: Role::ALL
            .into_iter()
            .map(|role| {
                Ok(InstructionAsset {
                    path: RelativePath::new(&format!("roles/{}.md", role.as_str()))?,
                    contents: role_card(role).to_owned(),
                })
            })
            .collect::<Result<_, HouseError>>()?,
    };
    let mut assets = manifest.roles.clone();
    for asset in &bundle.assets {
        assets.push(InstructionAsset {
            path: RelativePath::new(&format!("house/{}", asset.path.as_str()))?,
            contents: asset.contents.clone(),
        });
    }
    // The manifest is last; a partial directory is never a verified installation.
    assets.push(InstructionAsset {
        path: RelativePath::new("manifest.json")?,
        contents: serde_json::to_string_pretty(&manifest).map_err(|_| HouseError::InvalidInput)?,
    });
    let files: Vec<_> = assets
        .iter()
        .map(|asset| NewFile {
            path: &asset.path,
            contents: asset.contents.as_bytes(),
            mode: FileMode::Regular,
        })
        .collect();
    let snapshot = snapshot_path(root, house);
    super::installer::install_private_files(&snapshot, &files)?;
    resolve_instructions(root, house, None)
}

/// Verify files against the locally trusted manifest before handing instructions to a
/// fresh agent. Repository instructions are pinned by the task's Git revision;
/// they remain in the repository and are not silently copied from another house.
pub fn resolve_instructions(
    root: &Path,
    house: &HouseConfig,
    repository_revision: Option<CommitId>,
) -> Result<ResolvedInstructions, HouseError> {
    verified_snapshot(root, house, repository_revision).map(|(resolved, _)| resolved)
}

/// Verify the house's configured snapshot and return its bundle as verified in
/// the same pass, so callers never reread snapshot files after verification.
pub(crate) fn verified_snapshot(
    root: &Path,
    house: &HouseConfig,
    repository_revision: Option<CommitId>,
) -> Result<(ResolvedInstructions, InstructionBundle), HouseError> {
    super::registry::ensure_external(root)?;
    let snapshot = snapshot_path(root, house);
    let manifest: SnapshotManifest = serde_json::from_slice(
        &read_bounded(&snapshot.join("manifest.json"))
            .map_err(|_| HouseError::UnverifiedSnapshot)?,
    )
    .map_err(|_| HouseError::UnverifiedSnapshot)?;
    manifest.bundle.validate(house)?;
    if manifest.roles.len() != Role::ALL.len() {
        return Err(HouseError::UnverifiedSnapshot);
    }
    let role_paths: BTreeSet<_> = manifest
        .roles
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    for role in Role::ALL {
        if !role_paths.contains(format!("roles/{}.md", role.as_str()).as_str()) {
            return Err(HouseError::UnverifiedSnapshot);
        }
    }
    if digest_cards(
        manifest
            .roles
            .iter()
            .map(|asset| (asset.path.as_str().to_owned(), asset.contents.as_str())),
    ) != manifest.bundle.role_cards_digest
    {
        return Err(HouseError::UnverifiedSnapshot);
    }
    for asset in &manifest.roles {
        if read_bounded(&snapshot.join(asset.path.as_path()))
            .map_err(|_| HouseError::UnverifiedSnapshot)?
            != asset.contents.as_bytes()
        {
            return Err(HouseError::UnverifiedSnapshot);
        }
    }
    for asset in &manifest.bundle.assets {
        if read_bounded(&snapshot.join("house").join(asset.path.as_path()))
            .map_err(|_| HouseError::UnverifiedSnapshot)?
            != asset.contents.as_bytes()
        {
            return Err(HouseError::UnverifiedSnapshot);
        }
    }
    let resolved = ResolvedInstructions {
        house: house.house.clone(),
        provenance: Provenance {
            kitchen: house.kitchen.clone(),
            house_guidance: house.guidance.clone(),
            repository_instructions: repository_revision,
        },
        entrypoint: snapshot
            .join("house")
            .join(manifest.bundle.entrypoint.as_path()),
        role_cards_digest: manifest.bundle.role_cards_digest.clone(),
        snapshot,
    };
    Ok((resolved, manifest.bundle))
}

/// SHA-256 of the ordered role paths and UTF-8 contents, encoded as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoleCardsDigest(String);
impl RoleCardsDigest {
    /// Parse an exact SHA-256 hex digest without accepting arbitrary labels.
    pub fn new(value: &str) -> Result<Self, HouseError> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(HouseError::InvalidInput);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
    /// Canonical lowercase hexadecimal encoding.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for RoleCardsDigest {
    type Error = HouseError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}
impl From<RoleCardsDigest> for String {
    fn from(value: RoleCardsDigest) -> Self {
        value.0
    }
}

/// Digest an export in lexicographic role-path order: domain `kitchen-role-cards-v1\0`, then
/// each `roles/<name>.md` path and content, each prefixed with its u64 big-endian
/// byte length. Exporters must compute this from the claimed source revision.
pub fn role_cards_digest() -> RoleCardsDigest {
    digest_cards(
        Role::ALL
            .into_iter()
            .map(|role| (format!("roles/{}.md", role.as_str()), role_card(role))),
    )
}
fn digest_cards<'a>(cards: impl IntoIterator<Item = (String, &'a str)>) -> RoleCardsDigest {
    let mut cards: Vec<_> = cards.into_iter().collect();
    cards.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    digest.update(b"kitchen-role-cards-v1\0");
    for (path, contents) in cards {
        for value in [path.as_bytes(), contents.as_bytes()] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
    }
    RoleCardsDigest(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}
