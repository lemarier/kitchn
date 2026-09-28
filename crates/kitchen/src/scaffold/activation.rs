//! Path classes whose presence lets other tools act on a repository.
//!
//! Scaffolding only writes files. These classes name what a written file can
//! start later, and when, so the preview shows authority-bearing additions
//! before consent. Classification is best-effort: it matches known paths, plus
//! the `on` triggers of Actions-style workflows read as YAML. A file that
//! matches nothing can still be run by some tool.

use std::{collections::BTreeMap, fmt};

use saphyr_parser::{Event, Parser};

use super::MAX_RENDERED_BYTES;
use crate::adoption::RelativePath;

/// What a file can activate once present, and at which boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Activation {
    /// A CI configuration, such as `.github/workflows/*`, `.gitlab-ci.yml`,
    /// `.circleci/`, or a `Jenkinsfile`.
    CiWorkflow,
    /// A workflow with a `schedule` trigger.
    ScheduledWorkflow,
    /// Dependency update configuration, such as Dependabot or Renovate.
    DependencyUpdates,
    /// Agent or editor settings, such as `.claude/`, `.codex/`, or `.vscode/`.
    AgentSettings,
    /// MCP server configuration, such as `.mcp.json`.
    McpServers,
    /// An environment file, such as `.env` or `.envrc`.
    Environment,
    /// A Git hook directory, such as `.githooks/` or `.husky/`.
    GitHooks,
    /// A dev container definition, such as `.devcontainer/devcontainer.json`.
    DevContainer,
    /// Cargo configuration, such as `.cargo/config.toml`.
    CargoConfig,
}

/// Deepest YAML nesting [`Activation::of`] reads in a workflow. A deeper
/// document is treated as one that may be scheduled.
pub const MAX_WORKFLOW_YAML_DEPTH: usize = 32;

/// Workflow directories that use GitHub Actions syntax, where `on` lists the
/// triggers. Forges read them only at the repository root.
const ACTIONS_DIRS: [&str; 3] = [
    ".github/workflows/",
    ".forgejo/workflows/",
    ".gitea/workflows/",
];

/// Other CI configuration read only at the repository root.
const CI_ROOT_DIRS: [&str; 3] = [".gitlab/ci/", ".circleci/", ".buildkite/"];
const CI_ROOT_FILES: [&str; 3] = [".gitlab-ci.yml", ".travis.yml", "bitbucket-pipelines.yml"];

/// CI configuration whose location is set in the service, so monorepos keep it
/// in subdirectories.
const CI_FILE_NAMES: [&str; 3] = ["azure-pipelines.yml", "azure-pipelines.yaml", "jenkinsfile"];

/// Directories that editors and agent clients read from whichever folder they
/// open, so they count at any depth.
const AGENT_DIRS: [&str; 9] = [
    ".claude",
    ".codex",
    ".gemini",
    ".agents",
    ".opencode",
    ".cursor",
    ".vscode",
    ".idea",
    ".zed",
];

impl Activation {
    /// Classify a planned file by its path and, for Actions-style workflows,
    /// its contents. Paths compare case-insensitively, as on common forge
    /// checkouts.
    ///
    /// Paths that a forge or CI service reads only from the repository root
    /// (`.github/workflows/`, `.github/dependabot.yml`, `.circleci/`,
    /// `.travis.yml`) and `.githooks/` match only there. Directories that
    /// editors, agent clients, Husky, dev containers, and Cargo read from the
    /// folder they run in match at any depth, as do file names such as
    /// `Jenkinsfile`, `renovate.json`, `.mcp.json`, and `.env`.
    ///
    /// A workflow is scheduled when its top-level `on` names `schedule` as an
    /// event, in any YAML spelling. A workflow that is larger than
    /// [`MAX_RENDERED_BYTES`], nested deeper than [`MAX_WORKFLOW_YAML_DEPTH`],
    /// contains a NUL, or is not valid YAML is assumed scheduled: a wrong
    /// scheduled label over-warns, and a missing one hides unattended runs.
    #[must_use]
    pub fn of(path: &RelativePath, contents: &str) -> Option<Self> {
        let path = path.as_str().to_ascii_lowercase();
        let (dirs, name) = path.rsplit_once('/').unwrap_or(("", &path));
        let under = |dir: &str| dirs.split('/').any(|part| part == dir);

        if ACTIONS_DIRS.iter().any(|dir| path.starts_with(dir)) {
            return Some(if runs_on_schedule(contents) {
                Self::ScheduledWorkflow
            } else {
                Self::CiWorkflow
            });
        }
        if CI_ROOT_DIRS.iter().any(|dir| path.starts_with(dir))
            || CI_ROOT_FILES.contains(&path.as_str())
            || CI_FILE_NAMES.contains(&name)
        {
            return Some(Self::CiWorkflow);
        }
        if matches!(
            path.as_str(),
            ".github/dependabot.yml" | ".github/dependabot.yaml"
        ) || matches!(
            name,
            "renovate.json"
                | "renovate.json5"
                | ".renovaterc"
                | ".renovaterc.json"
                | ".renovaterc.json5"
        ) {
            return Some(Self::DependencyUpdates);
        }
        if matches!(name, ".mcp.json" | "mcp.json") {
            return Some(Self::McpServers);
        }
        if AGENT_DIRS.iter().any(|dir| under(dir))
            || matches!(name, "opencode.json" | "opencode.jsonc")
        {
            return Some(Self::AgentSettings);
        }
        if under(".devcontainer") || name == ".devcontainer.json" {
            return Some(Self::DevContainer);
        }
        if under(".cargo") && matches!(name, "config" | "config.toml") {
            return Some(Self::CargoConfig);
        }
        if matches!(name, ".env" | ".envrc")
            || (name.starts_with(".env.") && name != ".env.example")
        {
            return Some(Self::Environment);
        }
        if under(".husky") || path.starts_with(".githooks/") {
            return Some(Self::GitHooks);
        }
        None
    }

    /// When the activation happens. Kitchen itself never crosses it.
    #[must_use]
    pub const fn boundary(self) -> &'static str {
        match self {
            Self::CiWorkflow => "runs in CI once pushed",
            Self::ScheduledWorkflow => {
                "runs in CI once pushed, then on its schedule from the default branch"
            }
            Self::DependencyUpdates => "opens forge pull requests once pushed",
            Self::AgentSettings => "applies when an agent or editor opens the repository",
            Self::McpServers => "can start servers when an agent client opens the repository",
            Self::Environment => "loaded by tools that read it, such as direnv",
            Self::GitHooks => "runs on Git operations once hooks are configured",
            Self::DevContainer => "runs its lifecycle commands when opened in a dev container",
            Self::CargoConfig => {
                "applies to Cargo commands run in or below its directory, and can set runners and wrappers"
            }
        }
    }
}

impl fmt::Display for Activation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CiWorkflow => "CI workflow",
            Self::ScheduledWorkflow => "scheduled workflow",
            Self::DependencyUpdates => "dependency updates",
            Self::AgentSettings => "agent or editor settings",
            Self::McpServers => "MCP servers",
            Self::Environment => "environment file",
            Self::GitHooks => "Git hooks",
            Self::DevContainer => "dev container",
            Self::CargoConfig => "Cargo configuration",
        })
    }
}

/// Whether a workflow's triggers can include a schedule; `true` when they
/// cannot be read. The scanner treats NUL as the end of the stream and would
/// skip whatever follows it, so a NUL also counts as unreadable.
fn runs_on_schedule(contents: &str) -> bool {
    contents.len() > MAX_RENDERED_BYTES
        || contents.contains('\0')
        || triggers_name_schedule(contents).unwrap_or(true)
}

/// What a finished YAML node tells its parent.
#[derive(Debug, Clone, Copy, Default)]
struct Node {
    /// The scalar `schedule`, or a collection with it as a key or an item.
    names_schedule: bool,
    /// The scalar `on`.
    is_on: bool,
    /// The scalar `<<`, YAML's merge key.
    is_merge: bool,
}

/// A collection whose end event has not arrived.
struct Open {
    anchor: usize,
    mapping: bool,
    /// For a mapping, the finished key waiting for its value.
    key: Option<Node>,
    names_schedule: bool,
}

impl Open {
    const fn new(anchor: usize, mapping: bool) -> Self {
        Self {
            anchor,
            mapping,
            key: None,
            names_schedule: false,
        }
    }

    /// Fold in a finished child. Returns the key when the child was the value
    /// that completes a mapping entry.
    fn absorb(&mut self, child: Node) -> Option<Node> {
        if !self.mapping {
            self.names_schedule |= child.names_schedule;
            return None;
        }
        let Some(key) = self.key.take() else {
            self.key = Some(child);
            return None;
        };
        // A merge key contributes the keys of the mapping it merges in.
        self.names_schedule |= key.names_schedule || (key.is_merge && child.names_schedule);
        Some(key)
    }
}

/// Whether the top-level `on` of any document in `contents` names `schedule`,
/// as a mapping key (`on: {schedule: …}`), a list item, or the whole value.
/// `None` when the text is not valid YAML or nests deeper than
/// [`MAX_WORKFLOW_YAML_DEPTH`].
///
/// Walks parser events without building a document. An alias is one event
/// that reuses what its anchor summarised, so nothing is expanded and the work
/// stays linear in the input.
fn triggers_name_schedule(contents: &str) -> Option<bool> {
    let mut open: Vec<Open> = Vec::new();
    let mut anchors: BTreeMap<usize, bool> = BTreeMap::new();
    for event in Parser::new_from_str(contents) {
        let (event, _span) = event.ok()?;
        let (anchor, node) = match event {
            Event::Scalar(text, _style, anchor, _tag) => (
                anchor,
                Node {
                    names_schedule: text == "schedule",
                    is_on: text == "on",
                    is_merge: text == "<<",
                },
            ),
            Event::Alias(id) => (
                0,
                Node {
                    // An anchor still open is an alias cycle: assume it names one.
                    names_schedule: anchors.get(&id).copied().unwrap_or(true),
                    ..Node::default()
                },
            ),
            Event::SequenceStart(anchor, _tag) => {
                open.push(Open::new(anchor, false));
                if open.len() > MAX_WORKFLOW_YAML_DEPTH {
                    return None;
                }
                continue;
            }
            Event::MappingStart(anchor, _tag) => {
                open.push(Open::new(anchor, true));
                if open.len() > MAX_WORKFLOW_YAML_DEPTH {
                    return None;
                }
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let closed = open.pop()?;
                (
                    closed.anchor,
                    Node {
                        names_schedule: closed.names_schedule,
                        ..Node::default()
                    },
                )
            }
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart(_)
            | Event::DocumentEnd => continue,
        };
        if anchor != 0 {
            anchors.insert(anchor, node.names_schedule);
        }
        let in_root = open.len() == 1;
        if let Some(parent) = open.last_mut()
            && let Some(key) = parent.absorb(node)
            && in_root
            && key.is_on
            && node.names_schedule
        {
            return Some(true);
        }
    }
    Some(false)
}
