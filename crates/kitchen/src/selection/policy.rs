//! House agent policy and its resolution for one task.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::contracts::{Repository, Role};

use super::{
    AgentModel, AgentSelection, ResolvedSelection, SelectionError, SelectionSource, TaskGroup,
    WorkType,
};
use crate::scheduling::AgentFamily;

/// Most rules one [`AgentPolicy`] may hold.
pub const MAX_SELECTION_RULES: usize = 256;

/// Where and to what a [`SelectionRule`] applies. Every field that is set must
/// match the task. At most one of `task_group` and `repository` may be set;
/// with neither, the rule is house-wide and must name a role or work type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleMatch {
    /// Tasks in this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_group: Option<TaskGroup>,
    /// Tasks targeting this repository; it must be one the house serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    /// Tasks for this role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// Tasks of this work type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_type: Option<WorkType>,
}

impl RuleMatch {
    /// Role and work type together outrank role alone, which outranks work
    /// type alone, which outranks neither.
    const fn specificity(&self) -> u8 {
        match (self.role.is_some(), self.work_type.is_some()) {
            (true, true) => 3,
            (true, false) => 2,
            (false, true) => 1,
            (false, false) => 0,
        }
    }

    fn fits(&self, request: &SelectionRequest) -> bool {
        self.role.is_none_or(|role| role == request.role)
            && self
                .work_type
                .as_ref()
                .is_none_or(|work_type| request.work_type.as_ref() == Some(work_type))
    }
}

/// One override: when the task matches `when`, use `selection` whole. Fields
/// are never merged across rules, so a model always goes with its family.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectionRule {
    /// Which tasks the rule applies to.
    pub when: RuleMatch,
    /// The agent, model, and effort those tasks use.
    #[serde(rename = "use")]
    pub selection: AgentSelection,
}

/// House agent policy, stored in the house configuration outside every
/// repository. Repository bindings cannot override it; repository and task
/// group overrides are rules here.
///
/// Resolution for a task checks the task's group, then its repository, then
/// house-wide rules, and takes the most specific matching rule at the first
/// level that has one; with none, it uses `default`. A repository or group
/// rule that does not name a role yields to a house rule that does, so the
/// house's per-role choice, such as a lighter reviewer, is overridden only by
/// a repository or group rule that names that role. A model or effort left
/// unset is the agent's own default. Selection is configuration: it grants no
/// authority, and model diversity alone is not review evidence.
///
/// Roles map to the usual duties: `station-cook` implements, `inspector`
/// performs second validation, and `expediter` judges at the gate. Small fix
/// rounds are a house work type such as `fix`, so reviewers and fix rounds can
/// default to lighter models than implementation.
///
/// ```
/// use kitchen::{contracts::Role, selection::{AgentPolicy, SelectionRequest, SelectionSource}};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let policy: AgentPolicy = serde_json::from_str(r#"{
///     "default": {"agent": "codex", "model": "gpt-6-sol", "effort": "high"},
///     "rules": [{"when": {"role": "inspector"}, "use": {"agent": "claude", "model": "sonnet"}}]
/// }"#)?;
/// let review = policy.resolve(&SelectionRequest::new(Role::Inspector));
/// assert_eq!(review.source, SelectionSource::HouseRule);
/// assert_eq!(review.selection.attribution_model()?.as_str(), "claude:sonnet");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPolicy {
    /// The house default for tasks no rule matches.
    pub default: AgentSelection,
    /// Overrides by task group, repository, role, and work type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<SelectionRule>,
}

/// What a task is, for resolving its selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRequest {
    /// The task's role.
    pub role: Role,
    /// The task's work type, when the workflow assigns one.
    pub work_type: Option<WorkType>,
    /// The target repository; `None` for house-level work.
    pub repository: Option<Repository>,
    /// The task's group, when it has one.
    pub task_group: Option<TaskGroup>,
}

impl SelectionRequest {
    /// A house-level request for `role` with no work type or group.
    #[must_use]
    pub const fn new(role: Role) -> Self {
        Self {
            role,
            work_type: None,
            repository: None,
            task_group: None,
        }
    }
}

impl AgentPolicy {
    /// Check bounds and scopes against the house repository allowlist.
    ///
    /// # Errors
    /// Rejects too many rules, a rule scoped to both a group and a
    /// repository, a house-wide catch-all rule, a repository the house does
    /// not serve, and two rules with the same match.
    pub fn validate(&self, repositories: &BTreeSet<Repository>) -> Result<(), SelectionError> {
        if self.rules.len() > MAX_SELECTION_RULES {
            return Err(SelectionError::TooManyRules);
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let when = &rule.when;
            if when.task_group.is_some() && when.repository.is_some() {
                return Err(SelectionError::AmbiguousScope);
            }
            if when.task_group.is_none() && when.repository.is_none() && when.specificity() == 0 {
                return Err(SelectionError::UnscopedCatchAll);
            }
            if when
                .repository
                .as_ref()
                .is_some_and(|repository| !repositories.contains(repository))
            {
                return Err(SelectionError::RepositoryNotServed);
            }
            if self
                .rules
                .iter()
                .skip(index.saturating_add(1))
                .any(|other| &other.when == when)
            {
                return Err(SelectionError::DuplicateRule);
            }
        }
        Ok(())
    }

    /// Resolve the selection for one task. Call this once, when the task is
    /// created, and store the result in its
    /// [`TaskSpec::agent`](crate::contracts::TaskSpec::agent).
    #[must_use]
    pub fn resolve(&self, request: &SelectionRequest) -> ResolvedSelection {
        let levels = [
            request.task_group.as_ref().map(|group| {
                (
                    SelectionSource::TaskGroup {
                        group: group.clone(),
                    },
                    Scope::TaskGroup(group),
                )
            }),
            request.repository.as_ref().map(|repository| {
                (
                    SelectionSource::Repository {
                        repository: repository.clone(),
                    },
                    Scope::Repository(repository),
                )
            }),
            Some((SelectionSource::HouseRule, Scope::House)),
        ];
        // A house rule naming this role beats a repository or group rule that
        // does not, so a repository-wide model cannot silently replace the
        // lighter model the house chose for reviewers.
        let house_names_role = self.rules.iter().any(|rule| {
            Scope::House.contains(&rule.when) && rule.when.role.is_some() && rule.when.fits(request)
        });
        for (source, scope) in levels.into_iter().flatten() {
            let best = self
                .rules
                .iter()
                .filter(|rule| {
                    scope.contains(&rule.when)
                        && rule.when.fits(request)
                        && !(house_names_role
                            && !matches!(scope, Scope::House)
                            && rule.when.role.is_none())
                })
                .max_by_key(|rule| rule.when.specificity());
            if let Some(rule) = best {
                return ResolvedSelection {
                    selection: rule.selection.clone(),
                    source,
                };
            }
        }
        ResolvedSelection {
            selection: self.default.clone(),
            source: SelectionSource::HouseDefault,
        }
    }

    /// Every distinct agent family and model the policy names, for doctor to
    /// compare with what the installed agents offer.
    #[must_use]
    pub fn configured_models(&self) -> Vec<(AgentFamily, AgentModel)> {
        let mut models: Vec<(AgentFamily, AgentModel)> = Vec::new();
        for selection in
            std::iter::once(&self.default).chain(self.rules.iter().map(|r| &r.selection))
        {
            if let Some(model) = &selection.model
                && !models
                    .iter()
                    .any(|(agent, known)| *agent == selection.agent && known == model)
            {
                models.push((selection.agent, model.clone()));
            }
        }
        models
    }

    /// Configured family and model pairs that `offered` does not list. A
    /// family missing from `offered` offers nothing.
    #[must_use]
    pub fn unoffered_models(&self, offered: &[OfferedModels]) -> Vec<(AgentFamily, AgentModel)> {
        self.configured_models()
            .into_iter()
            .filter(|(agent, model)| {
                !offered
                    .iter()
                    .any(|entry| entry.agent == *agent && entry.models.contains(model))
            })
            .collect()
    }
}

/// The models one installed agent reports offering, observed read-only by an
/// integration for doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfferedModels {
    /// The installed agent family.
    pub agent: AgentFamily,
    /// Model ids it accepts.
    pub models: BTreeSet<AgentModel>,
}

enum Scope<'a> {
    TaskGroup(&'a TaskGroup),
    Repository(&'a Repository),
    House,
}

impl Scope<'_> {
    fn contains(&self, when: &RuleMatch) -> bool {
        match self {
            Self::TaskGroup(group) => when.task_group.as_ref() == Some(*group),
            Self::Repository(repository) => when.repository.as_ref() == Some(*repository),
            Self::House => when.task_group.is_none() && when.repository.is_none(),
        }
    }
}
