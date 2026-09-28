//! The worktree name that makes Orca create a requested branch.
//!
//! Orca names the branch of a worktree it creates from `--name`: it puts the
//! host's branch prefix setting in front and turns `/` in the name into `-`.
//! On the verified host, `--name lemarier/x` became `lemarier/lemarier-x` and
//! `--name x` became `lemarier/x`. Its CLI has no way to override this or to
//! rename the branch afterwards, so a requested branch is only obtainable as
//! the configured prefix followed by one name (or as a single name when the
//! host adds no prefix). Anything else is refused before a Task or worktree
//! exists, and what Orca actually created is still verified after the launch.

use crate::{adapters::orca::OrcaError, contracts::BranchName};

/// The `--name` that makes Orca create exactly `branch` on a host whose
/// branch prefix setting is `prefix`, or off when `None`.
///
/// The name uses only letters, digits, `.`, `_`, and `-`, and starts with a
/// letter or digit, so Orca does not rewrite it.
///
/// # Errors
/// [`OrcaError::BranchUnobtainable`] when `branch` is not `prefix/name` (or a
/// single `name` without a prefix) for a name of that form.
pub(crate) fn worktree_name(
    prefix: Option<&BranchName>,
    branch: &BranchName,
) -> Result<String, OrcaError> {
    let name = match prefix {
        Some(prefix) => branch
            .as_str()
            .strip_prefix(prefix.as_str())
            .and_then(|rest| rest.strip_prefix('/')),
        None => Some(branch.as_str()),
    };
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn without_a_prefix_only_a_single_name_is_obtainable() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(worktree_name(None, &branch("hotfix")?)?, "hotfix");
        assert_eq!(
            worktree_name(None, &branch("lemarier/x")?),
            Err(OrcaError::BranchUnobtainable {
                requested: "lemarier/x".to_owned()
            }),
            "Orca would turn the slash into a dash"
        );
        Ok(())
    }

    #[test]
    fn branches_outside_the_prefix_or_of_another_shape_are_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let prefix = branch("lemarier")?;
        for requested in [
            "kitchen/x",           // another prefix
            "lemarier",            // no name after the prefix
            "lemarier-x",          // shares the text, not the component
            "lemarier/area/topic", // more than one name
            "lemarierx/y",
            "lemarier/-x", // does not start with a letter or digit
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
