//! Selection values, the selection itself, and the record a task keeps.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    contracts::{Repository, Text},
    scheduling::AgentFamily,
};

use super::{SelectionError, SelectionValue};

/// Longest accepted [`AgentModel`] in bytes.
pub const MAX_MODEL_BYTES: usize = 128;
/// Longest accepted [`EffortLevel`], [`WorkType`], or [`TaskGroup`] in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// Provider model ids are opaque: printable ASCII without spaces. A leading
/// `-` is refused so the id can never be read as a command-line flag.
fn validate_model(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MODEL_BYTES
        && !value.starts_with('-')
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Lowercase names: ASCII letters, digits, `-`, `_`, and `.`, starting with a
/// letter or digit.
fn validate_name(value: &str) -> bool {
    value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

macro_rules! selection_value {
    ($name:ident, $kind:expr, $validate:path, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate and own the value without normalization.
            ///
            /// # Errors
            /// Returns [`SelectionError::InvalidValue`] without echoing the input.
            pub fn new(value: &str) -> Result<Self, SelectionError> {
                if $validate(value) {
                    Ok(Self(value.to_owned()))
                } else {
                    Err(SelectionError::InvalidValue { kind: $kind })
                }
            }

            /// Borrow the validated value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = SelectionError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::new(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

selection_value!(
    AgentModel,
    SelectionValue::Model,
    validate_model,
    "An opaque provider model id, such as `gpt-6-sol` or `sonnet`.\n\nKitchen keeps no model catalog: the id is passed to the agent unchanged, and doctor compares it with the models the installed agent reports."
);
selection_value!(
    EffortLevel,
    SelectionValue::Effort,
    validate_name,
    "An opaque reasoning effort level, such as `high`, passed to the agent unchanged."
);
selection_value!(
    WorkType,
    SelectionValue::WorkType,
    validate_name,
    "A house-defined work category, such as `fix` or `firmware`.\n\nThe same name is the trust ledger's station work type."
);
selection_value!(
    TaskGroup,
    SelectionValue::TaskGroup,
    validate_name,
    "A house-defined group of related tasks, such as one parent issue's sub-issues."
);

/// An agent family with an optional model and effort. A missing model or
/// effort means the agent's own default, and is recorded as that default.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSelection {
    /// Agent family to launch.
    pub agent: AgentFamily,
    /// Provider model id; `None` leaves the agent's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AgentModel>,
    /// Reasoning effort; `None` leaves the default for the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
}

impl AgentSelection {
    /// The agent family with its default model and effort.
    #[must_use]
    pub const fn agent_default(agent: AgentFamily) -> Self {
        Self {
            agent,
            model: None,
            effort: None,
        }
    }

    /// The model identity for trust attribution: `family:model`, or only
    /// `family` when the agent's default model was left in place. Effort is
    /// not part of the identity because backends do not report it.
    ///
    /// The trust ledger compares this text verbatim with the model the
    /// backend reports, so a task that ran on the agent's default never
    /// matches an observation and earns no trust from it.
    ///
    /// # Errors
    /// Never fails for a validated selection; the conversion to [`Text`] is
    /// fallible only in type.
    pub fn attribution_model(&self) -> Result<Text, SelectionError> {
        let identity = match &self.model {
            Some(model) => format!("{}:{model}", self.agent.as_str()),
            None => self.agent.as_str().to_owned(),
        };
        Text::new(&identity).map_err(|_| SelectionError::InvalidValue {
            kind: SelectionValue::Model,
        })
    }
}

/// Which configuration produced a [`ResolvedSelection`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum SelectionSource {
    /// A house rule scoped to the task's group.
    TaskGroup {
        /// The matched group.
        group: TaskGroup,
    },
    /// A house rule scoped to the task's repository.
    Repository {
        /// The matched repository.
        repository: Repository,
    },
    /// A house-wide rule for the task's role or work type.
    HouseRule,
    /// The house default.
    HouseDefault,
    /// An explicit owner choice for this task, outside the policy.
    Owner,
}

/// The selection a task resolved when it was created, and where it came
/// from. A task keeps it for every attempt; a later policy change never
/// alters it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedSelection {
    /// The selected agent, model, and effort.
    pub selection: AgentSelection,
    /// The configuration that produced it.
    pub source: SelectionSource,
}

impl ResolvedSelection {
    /// An explicit owner choice. Changing an active task's selection means
    /// creating a replacement task with this, since a task's selection and its
    /// trust binding are fixed once written.
    #[must_use]
    pub const fn owner(selection: AgentSelection) -> Self {
        Self {
            selection,
            source: SelectionSource::Owner,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_opaque_but_bounded() {
        for valid in ["gpt-6-sol", "sonnet", "sonnet[1m]", "us.anthropic.x:1@v2"] {
            assert!(AgentModel::new(valid).is_ok(), "{valid}");
        }
        let long = "m".repeat(MAX_MODEL_BYTES + 1);
        for invalid in ["", "-flag", "two words", "tab\t", long.as_str()] {
            assert_eq!(
                AgentModel::new(invalid),
                Err(SelectionError::InvalidValue {
                    kind: SelectionValue::Model
                }),
                "{invalid:?}"
            );
        }
        assert!(AgentModel::new(&"m".repeat(MAX_MODEL_BYTES)).is_ok());
    }

    #[test]
    fn names_are_lowercase_tokens() {
        assert!(WorkType::new("fix").is_ok());
        assert!(EffortLevel::new("xhigh").is_ok());
        assert!(TaskGroup::new("issue-1.v2_a").is_ok());
        for invalid in ["", "Fix", "-x", "_x", "a b", "a/b"] {
            assert!(WorkType::new(invalid).is_err(), "{invalid:?}");
        }
        assert!(EffortLevel::new(&"e".repeat(MAX_NAME_BYTES + 1)).is_err());
    }

    #[test]
    fn attribution_names_the_default_honestly() -> Result<(), SelectionError> {
        let default = AgentSelection::agent_default(AgentFamily::Claude);
        assert_eq!(default.attribution_model()?.as_str(), "claude");
        let pinned = AgentSelection {
            agent: AgentFamily::Codex,
            model: Some(AgentModel::new("gpt-6-sol")?),
            effort: Some(EffortLevel::new("high")?),
        };
        assert_eq!(pinned.attribution_model()?.as_str(), "codex:gpt-6-sol");
        Ok(())
    }
}
