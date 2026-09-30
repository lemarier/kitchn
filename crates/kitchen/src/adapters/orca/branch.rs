//! The worktree name that makes Orca create a requested branch.
//!
//! Orca names the branch of a new worktree from `--name`: it puts the host's
//! branch prefix in front and rewrites `/` in the name. Kitchen passes only a
//! plain final component, then verifies the full branch Orca reports.

use crate::{adapters::orca::OrcaError, contracts::BranchName};

/// The `--name` for a requested branch. The caller's prefix is a logical
/// label; Orca replaces it with the configured host prefix.
///
/// The name uses only letters, digits, `.`, `_`, and `-`, and starts with a
/// letter or digit, so Orca does not rewrite it.
///
/// # Errors
/// [`OrcaError::BranchUnobtainable`] when the request cannot be represented by
/// one plain worktree name.
pub(crate) fn worktree_name(
    prefix: Option<&BranchName>,
    branch: &BranchName,
) -> Result<String, OrcaError> {
    let name = prefix
        .and_then(|prefix| {
            branch
                .as_str()
                .strip_prefix(prefix.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
        })
        .or_else(|| match branch.as_str().split_once('/') {
            Some((_, name)) if !name.contains('/') => Some(name),
            None => Some(branch.as_str()),
            Some(_) => None,
        });
    match name {
        Some(name)
            if name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                }) =>
        {
            Ok(name.to_owned())
        }
        Some(_) | None => Err(OrcaError::BranchUnobtainable {
            requested: branch.as_str().to_owned(),
        }),
    }
}

/// The full branch a new Orca worktree should create for `requested`.
pub(crate) fn created_branch(
    prefix: Option<&BranchName>,
    requested: &BranchName,
) -> Result<BranchName, OrcaError> {
    let name = worktree_name(prefix, requested)?;
    let full = prefix.map_or(name.clone(), |prefix| format!("{prefix}/{name}"));
    BranchName::new(&full).map_err(OrcaError::from)
}

/// Whether `actual` is the branch Orca creates in place of `requested` when
/// `requested` already exists: `requested`, a `-`, and a number from 2.
pub(crate) fn is_collision(requested: &str, actual: &str) -> bool {
    actual
        .strip_prefix(requested)
        .and_then(|rest| rest.strip_prefix('-'))
        .filter(|suffix| !suffix.starts_with('0') && suffix.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|suffix| suffix.parse::<u32>().ok())
        .is_some_and(|n| n >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collision_is_the_requested_branch_with_a_numeric_suffix() {
        assert!(is_collision("lemarier/x", "lemarier/x-2"));
        assert!(is_collision("lemarier/x", "lemarier/x-17"));
        for actual in [
            "lemarier/x",    // the requested branch itself
            "lemarier/x-1",  // Orca starts at 2
            "lemarier/x-02", // not a number Orca writes
            "lemarier/x-",
            "lemarier/x-2a",
            "lemarier/x2",
            "other/x-2", // another prefix: a plain mismatch
            "lemarier/y-2",
        ] {
            assert!(!is_collision("lemarier/x", actual), "{actual}");
        }
    }

    fn branch(value: &str) -> Result<BranchName, Box<dyn std::error::Error>> {
        Ok(BranchName::new(value)?)
    }

    #[test]
    fn a_branch_under_the_prefix_is_its_last_component() -> Result<(), Box<dyn std::error::Error>> {
        let prefix = branch("lemarier")?;
        assert_eq!(
            worktree_name(Some(&prefix), &branch("lemarier/orca-adapter")?)?,
            "orca-adapter"
        );
        assert_eq!(
            worktree_name(Some(&prefix), &branch("lemarier/fix_1.2")?)?,
            "fix_1.2"
        );
        // A prefix may itself have several components.
        assert_eq!(
            worktree_name(Some(&branch("team/lemarier")?), &branch("team/lemarier/x")?)?,
            "x"
        );
        Ok(())
    }

    #[test]
    fn a_requested_label_is_replaced_by_the_host_prefix() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(worktree_name(None, &branch("hotfix")?)?, "hotfix");
        assert_eq!(worktree_name(None, &branch("kitchen/x")?)?, "x");
        assert_eq!(created_branch(None, &branch("kitchen/x")?)?, branch("x")?);
        assert_eq!(
            created_branch(Some(&branch("lemarier")?), &branch("kitchen/x")?)?,
            branch("lemarier/x")?
        );
        Ok(())
    }

    #[test]
    fn branches_with_unrepresentable_names_are_refused() -> Result<(), Box<dyn std::error::Error>> {
        let prefix = branch("lemarier")?;
        for requested in [
            "lemarier/area/topic", // more than one name
            "lemarier/-x",         // does not start with a letter or digit
        ] {
            let requested = branch(requested)?;
            assert_eq!(
                worktree_name(Some(&prefix), &requested),
                Err(OrcaError::BranchUnobtainable {
                    requested: requested.as_str().to_owned()
                }),
                "{requested}"
            );
        }
        // Characters Orca might rewrite are never passed.
        assert!(worktree_name(Some(&prefix), &branch("lemarier/a@b")?).is_err());
        assert!(worktree_name(Some(&prefix), &branch("lemarier/a+b")?).is_err());
        Ok(())
    }
}
