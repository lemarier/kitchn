//! The branch a launched worker must work on.
//!
//! Orca names the branch of a worktree it creates from `--name`: it puts the
//! host's branch prefix in front and turns `/` in the name into `-`. On the
//! verified host, `--name lemarier/x` became `lemarier/lemarier-x` and
//! `--name x` became `lemarier/x`. A request for the exact branch
//! `lemarier/x` therefore passes the name `x`, and the branch Orca reports is
//! checked afterwards: the adapter cannot read the host's prefix setting, so
//! the first segment of a requested branch is only an assumption about it.
//!
//! This module is a shim. [`crate::contracts::Operation::LaunchWorker`] has no
//! branch field yet, so [`crate::adapters::orca::OrcaBackend::with_branch_source`]
//! lets the caller say which branch a launch needs. When the field lands,
//! the backend reads it from the operation, [`BranchSource`] goes away, and
//! [`RequestedBranch`] is replaced by the contract's type.

use std::fmt;

use crate::{adapters::orca::OrcaError, contracts::EffectRequest};

/// Longest branch name accepted.
const MAX_BRANCH_BYTES: usize = 200;

/// A validated branch name a worker must work on.
///
/// Segments separated by `/` start with a letter or digit and use only
/// letters, digits, `.`, `_`, and `-`, without `..` and without ending in
/// `.` or `.lock`. That is a subset of Git's rules, chosen so the name means
/// the same thing to Git, Orca, and a shell.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestedBranch(String);

impl RequestedBranch {
    /// Validate a branch name.
    ///
    /// # Errors
    /// [`OrcaError::InvalidBranch`] for an empty, oversized, or malformed name.
    pub fn new(value: &str) -> Result<Self, OrcaError> {
        let segment_ok = |segment: &str| {
            segment
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                && !segment.contains("..")
                && !segment.ends_with('.')
                && !segment.ends_with(".lock")
        };
        if value.len() <= MAX_BRANCH_BYTES && value.split('/').all(segment_ok) {
            Ok(Self(value.to_owned()))
        } else {
            Err(OrcaError::InvalidBranch)
        }
    }

    /// Borrow the branch name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `--name` that makes Orca create this branch: the last segment of
    /// a branch under one prefix, or the whole name of one without a prefix.
    ///
    /// # Errors
    /// [`OrcaError::BranchUnobtainable`] for a branch with more than one
    /// `/`, which Orca would turn into `-`: no name yields it.
    pub(crate) fn worktree_name(&self) -> Result<String, OrcaError> {
        let mut segments = self.0.split('/');
        match (segments.next(), segments.next(), segments.next()) {
            (Some(name), None, _) | (Some(_), Some(name), None) => Ok(name.to_owned()),
            _ => Err(OrcaError::BranchUnobtainable {
                requested: self.0.clone(),
            }),
        }
    }
}

impl fmt::Display for RequestedBranch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

type Source = dyn Fn(&EffectRequest) -> Option<RequestedBranch> + Send + Sync;

/// Where a launch's requested branch comes from; none by default.
#[derive(Default)]
pub(crate) struct BranchSource(Option<Box<Source>>);

impl BranchSource {
    pub(crate) fn new(
        source: impl Fn(&EffectRequest) -> Option<RequestedBranch> + Send + Sync + 'static,
    ) -> Self {
        Self(Some(Box::new(source)))
    }

    pub(crate) fn requested(&self, request: &EffectRequest) -> Option<RequestedBranch> {
        self.0.as_ref().and_then(|source| source(request))
    }
}

impl fmt::Debug for BranchSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("BranchSource")
            .field(&self.0.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_are_accepted_and_map_to_worktree_names() -> Result<(), OrcaError> {
        assert_eq!(
            RequestedBranch::new("lemarier/orca-adapter")?.worktree_name()?,
            "orca-adapter"
        );
        assert_eq!(RequestedBranch::new("main")?.worktree_name()?, "main");
        assert_eq!(
            RequestedBranch::new("lemarier/fix_1.2")?.as_str(),
            "lemarier/fix_1.2"
        );
        Ok(())
    }

    #[test]
    fn malformed_names_are_rejected() {
        for bad in [
            "", "/x", "x/", "a//b", ".hidden", "-flag", "a b", "a..b", "x.", "x.lock", "a/x.lock",
            "x@{1}", "é", "a\0b",
        ] {
            assert_eq!(
                RequestedBranch::new(bad),
                Err(OrcaError::InvalidBranch),
                "{bad:?}"
            );
        }
        assert!(RequestedBranch::new(&"a".repeat(MAX_BRANCH_BYTES)).is_ok());
        assert_eq!(
            RequestedBranch::new(&"a".repeat(MAX_BRANCH_BYTES + 1)),
            Err(OrcaError::InvalidBranch)
        );
    }

    #[test]
    fn deeper_branches_have_no_worktree_name() -> Result<(), OrcaError> {
        let branch = RequestedBranch::new("lemarier/area/topic")?;
        assert_eq!(
            branch.worktree_name(),
            Err(OrcaError::BranchUnobtainable {
                requested: "lemarier/area/topic".to_owned()
            })
        );
        Ok(())
    }
}
