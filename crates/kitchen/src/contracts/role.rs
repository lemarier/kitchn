//! Kitchen station roles. A role names responsibilities; it never grants authority.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::contracts::{ContractError, ValueKind};

/// A Kitchen role. Authority comes only from explicit grants, never from the role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Role {
    /// Owns house policy and final decisions.
    ChefOwner,
    /// Coordinates pickup, supervision, and repair.
    SousChef,
    /// Implements one task in an isolated workspace.
    StationCook,
    /// Performs bounded preparation work for a station.
    Commis,
    /// Evaluates exact-head evidence at the pass (merge gate).
    Expediter,
    /// Maintains issue hygiene and specifications.
    Gardener,
    /// Inspects and reclaims resources with positive ownership evidence.
    Dishwasher,
    /// Inspects delivered work against concrete questions.
    Inspector,
}

impl Role {
    /// Every role, in declaration order.
    pub const ALL: [Self; 8] = [
        Self::ChefOwner,
        Self::SousChef,
        Self::StationCook,
        Self::Commis,
        Self::Expediter,
        Self::Gardener,
        Self::Dishwasher,
        Self::Inspector,
    ];

    /// The stable kebab-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChefOwner => "chef-owner",
            Self::SousChef => "sous-chef",
            Self::StationCook => "station-cook",
            Self::Commis => "commis",
            Self::Expediter => "expediter",
            Self::Gardener => "gardener",
            Self::Dishwasher => "dishwasher",
            Self::Inspector => "inspector",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|role| role.as_str() == value)
            .ok_or(ContractError::InvalidValue {
                kind: ValueKind::Role,
            })
    }
}
