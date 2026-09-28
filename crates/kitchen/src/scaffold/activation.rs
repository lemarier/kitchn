//! Path classes whose presence lets other tools act on a repository.
//!
//! Scaffolding only writes files. These classes name what a written file can
//! start later, and when, so the preview shows authority-bearing additions
//! before consent. Classification is by path, plus a `schedule` trigger check
//! for workflows; it cannot prove a file inert.

use std::fmt;

use crate::adoption::RelativePath;

/// What a file can activate once present, and at which boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Activation {
    /// A forge CI workflow, such as `.github/workflows/*` or `.gitlab-ci.yml`.
    CiWorkflow,
    /// A forge workflow with a `schedule` trigger.
    ScheduledWorkflow,
    /// Dependency update configuration, such as Dependabot or Renovate.
    DependencyUpdates,
    /// Agent or editor settings, such as `.claude/`, `.vscode/`, or `.idea/`.
    AgentSettings,
    /// MCP server configuration, such as `.mcp.json`.
    McpServers,
    /// An environment file, such as `.env` or `.envrc`.
    Environment,
    /// A Git hook directory, such as `.githooks/` or `.husky/`.
    GitHooks,
}

impl Activation {
    /// Classify a planned file by its path and, for workflows, its contents.
    /// Paths compare case-insensitively, as on common forge checkouts.
    #[must_use]
    pub fn of(path: &RelativePath, contents: &str) -> Option<Self> {
        let path = path.as_str().to_ascii_lowercase();
        let name = path.rsplit('/').next().unwrap_or(&path);
        let workflow = path.starts_with(".github/workflows/")
            || path == ".gitlab-ci.yml"
            || path.starts_with(".gitlab/ci/")
            || path.starts_with(".forgejo/workflows/")
            || path.starts_with(".gitea/workflows/");
        if workflow {
            let scheduled = contents
                .lines()
                .any(|line| line.trim_start().starts_with("schedule:"));
            return Some(if scheduled {
                Self::ScheduledWorkflow
            } else {
                Self::CiWorkflow
            });
        }
        if matches!(
            path.as_str(),
            ".github/dependabot.yml" | ".github/dependabot.yaml"
        ) || matches!(name, "renovate.json" | "renovate.json5" | ".renovaterc")
        {
            return Some(Self::DependencyUpdates);
        }
        if name == ".mcp.json" || name == "mcp.json" {
            return Some(Self::McpServers);
        }
        if [".claude/", ".vscode/", ".cursor/", ".idea/", ".zed/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            return Some(Self::AgentSettings);
        }
        if matches!(name, ".env" | ".envrc")
            || (name.starts_with(".env.") && name != ".env.example")
        {
            return Some(Self::Environment);
        }
        if path.starts_with(".githooks/") || path.starts_with(".husky/") {
            return Some(Self::GitHooks);
        }
        None
    }

    /// When the activation happens. Kitchen itself never crosses it.
    #[must_use]
    pub const fn boundary(self) -> &'static str {
        match self {
            Self::CiWorkflow => "runs on the forge once pushed",
            Self::ScheduledWorkflow => {
                "runs on the forge once pushed, then on its schedule from the default branch"
            }
            Self::DependencyUpdates => "opens forge pull requests once pushed",
            Self::AgentSettings => "applies when an agent or editor opens the repository",
            Self::McpServers => "can start servers when an agent client opens the repository",
            Self::Environment => "loaded by tools that read it, such as direnv",
            Self::GitHooks => "runs on Git operations once hooks are configured",
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
        })
    }
}
