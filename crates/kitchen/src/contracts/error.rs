//! Contract validation and policy failures.

use std::fmt;

use crate::{
    ErrorClass, HouseId,
    contracts::{Capability, GrantScope, Permission, ValueKind},
};

/// A rejected value or a policy refusal. Input text is never echoed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ContractError {
    /// A validated value was rejected.
    #[error("invalid {kind}")]
    InvalidValue {
        /// The kind of value.
        kind: ValueKind,
    },
    /// A backend does not fully support what a workflow requires.
    #[error(
        "backend lacks required capabilities (missing: {}; partial: {})",
        CapabilityList(missing),
        CapabilityList(partial)
    )]
    UnsupportedCapabilities {
        /// Required but undeclared.
        missing: Vec<Capability>,
        /// Required but only partially supported.
        partial: Vec<Capability>,
    },
    /// Requested or held task authority exceeds the house's grants.
    #[error("authority expansion: the house does not grant {permission} for {scope}")]
    AuthorityExpansion {
        /// The uncovered permission.
        permission: Permission,
        /// Its scope.
        scope: GrantScope,
    },
    /// The task's authority does not include a needed permission.
    #[error("task authority does not include {permission}")]
    PermissionDenied {
        /// The missing permission.
        permission: Permission,
    },
    /// A value from one house was used with another house.
    #[error("house mismatch: expected {expected}, found {found}")]
    CrossHouse {
        /// The house in scope.
        expected: HouseId,
        /// The house presented.
        found: HouseId,
    },
}

impl ContractError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidValue { .. } => ErrorClass::InvalidInput,
            Self::UnsupportedCapabilities { .. }
            | Self::AuthorityExpansion { .. }
            | Self::PermissionDenied { .. }
            | Self::CrossHouse { .. } => ErrorClass::Refused,
        }
    }
}

struct CapabilityList<'a>(&'a [Capability]);

impl fmt::Display for CapabilityList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut capabilities = self.0.iter();
        let Some(first) = capabilities.next() else {
            return formatter.write_str("none");
        };
        formatter.write_str(first.as_str())?;
        for capability in capabilities {
            write!(formatter, ", {capability}")?;
        }
        Ok(())
    }
}
