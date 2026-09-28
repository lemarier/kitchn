//! Contract validation and policy failures.

use std::fmt;

use crate::{
    ErrorClass, HouseId,
    contracts::{Capability, ExecutorKind, GrantScope, Permission, ValueKind},
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
    /// Equally specific grants name different credentials for one action.
    #[error("grants name more than one credential for {permission}")]
    AmbiguousCredential {
        /// The permission.
        permission: Permission,
    },
    /// Work claimed by a scheduled trigger presented a person's consent.
    #[error("scheduled work acts only on standing grants, not consent")]
    ConsentNotAccepted,
    /// Work claimed by an interactive trigger needs the person's consent.
    #[error("interactive work needs consent for {permission}")]
    ConsentRequired {
        /// The permission the effect needs.
        permission: Permission,
    },
    /// The consent is for a different house, task, operation, or revision.
    #[error("consent does not cover this effect")]
    ConsentMismatch,
    /// Current authority selects a different credential than the one an
    /// existing intent was persisted with; the intent is not resubmitted.
    #[error("the authorized credential changed since the intent was persisted")]
    CredentialChanged,
    /// The effect acts outside the task's scope, such as another repository.
    #[error("effect scope {effect} is outside the task scope {task}")]
    OutOfTaskScope {
        /// The effect's scope.
        effect: GrantScope,
        /// The task's scope.
        task: GrantScope,
    },
    /// A decision request names another house, task, or evidence revision.
    #[error("decision binding does not match this task")]
    DecisionBindingMismatch,
    /// The task used its budget of effects for one executor family.
    #[error("the task used its budget of {limit} {executor:?} effects")]
    EffectBudgetExhausted {
        /// The executor family.
        executor: ExecutorKind,
        /// The budget.
        limit: u32,
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
            | Self::AmbiguousCredential { .. }
            | Self::ConsentNotAccepted
            | Self::ConsentRequired { .. }
            | Self::ConsentMismatch
            | Self::OutOfTaskScope { .. }
            | Self::CredentialChanged
            | Self::DecisionBindingMismatch
            | Self::EffectBudgetExhausted { .. }
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
