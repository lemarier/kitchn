//! Guided `house init`: flags, the terminal prompt, and the environment
//! observations the library's wizard takes as input.
use clap::Args;
use kitchen::{
    CredentialId, HouseId,
    adoption::{checkout_remotes, decode},
    contracts::{CommitId, ExternalRef, Permission, PostingBudget, Repository},
    house::{
        HouseError, HouseInitError, InitAnswers, InitDecision, InitFacts, InstalledAgents,
        NoGitHubAccess, ObservedChecks, Prompter, RequiredCheckSource, plan_house_init,
        register_house,
    },
    integrations::github::{
        CredentialFile, CredentialRef, GhCli, GitHubClient, HouseScope, Observation, ReadLimits,
    },
    scheduling::AgentFamily,
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

/// Most `PATH` entries searched for agent executables.
const MAX_PATH_ENTRIES: usize = 256;

/// Argument ids that answer guided questions; none may accompany `--config`.
pub const GUIDED: [&str; 15] = [
    "house",
    "repositories",
    "posting_destinations",
    "sous_chef",
    "station_cook",
    "expediter",
    "required_checks",
    "required_reviewers",
    "kitchen",
    "bundle",
    "yes",
    "github_requester",
    "github_credential",
    "github_credential_file",
    "gh",
];

/// Answers to the guided questions. Each skips its prompt.
#[derive(Args)]
pub struct InitArgs {
    /// House name.
    #[arg(long)]
    house: Option<String>,
    /// Comma-separated owner/name repositories (default: this checkout's).
    #[arg(long)]
    repositories: Option<String>,
    /// Comma-separated repositories tasks may post to (default: the repositories).
    #[arg(long)]
    posting_destinations: Option<String>,
    /// Agent at the sous-chef station: claude or codex (default: claude).
    #[arg(long)]
    sous_chef: Option<String>,
    /// Agent at the station cook station: claude or codex (default: codex).
    #[arg(long)]
    station_cook: Option<String>,
    /// Agent at the expediter station: claude or codex (default: claude).
    #[arg(long)]
    expediter: Option<String>,
    /// Comma-separated required checks, or none.
    #[arg(long)]
    required_checks: Option<String>,
    /// Comma-separated required reviewers, or none (default: expediter).
    #[arg(long)]
    required_reviewers: Option<String>,
    /// Kitchen commit to pin (default: the commit this binary records).
    #[arg(long)]
    kitchen: Option<String>,
    /// Verified guidance bundle to pin instead of the default guidance.
    #[arg(long)]
    bundle: Option<PathBuf>,
    /// Register without asking for confirmation.
    #[arg(long)]
    yes: bool,
    #[command(flatten)]
    github: GitHubArgs,
}

/// House-scoped GitHub read access, used only to offer required checks.
#[derive(Args)]
struct GitHubArgs {
    /// The GitHub login the credential must authenticate as.
    #[arg(long, requires_all = ["github_credential", "github_credential_file", "gh"])]
    github_requester: Option<ExternalRef>,
    /// Credential name for the house.
    #[arg(long, requires = "github_requester")]
    github_credential: Option<CredentialId>,
    /// Absolute path of the private file holding the read token.
    #[arg(long, requires = "github_requester")]
    github_credential_file: Option<PathBuf>,
    /// Absolute path of the GitHub CLI.
    #[arg(long, requires = "github_requester")]
    gh: Option<PathBuf>,
}

pub fn run(
    registry: Option<PathBuf>,
    args: Box<InitArgs>,
) -> Result<(String, bool), kitchen::Error> {
    let args = *args;
    let answers = InitAnswers {
        registry: registry.map(super::house::canonical_root).transpose()?,
        house: args.house,
        repositories: args.repositories,
        posting_destinations: args.posting_destinations,
        sous_chef: args.sous_chef,
        station_cook: args.station_cook,
        expediter: args.expediter,
        required_checks: args.required_checks,
        required_reviewers: args.required_reviewers,
        kitchen: args.kitchen,
        bundle: args.bundle.as_deref().map(decode).transpose()?,
        yes: args.yes,
    };
    let facts = InitFacts {
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute()),
        checkout: std::env::current_dir()
            .map_err(HouseError::from)
            .and_then(|start| checkout_remotes(&start))
            .map(|remotes| remotes.selected.repository),
        kitchen: option_env!("KITCHEN_COMMIT").and_then(|commit| CommitId::new(commit).ok()),
        agents: installed_agents(std::env::var_os("PATH")),
    };
    let github = match args.github {
        GitHubArgs {
            github_requester: Some(requester),
            github_credential: Some(credential),
            github_credential_file: Some(credential_file),
            gh: Some(gh),
        } => Some(HouseGitHub {
            requester,
            credential,
            credential_file,
            gh,
        }),
        _ => None,
    };
    let checks: &dyn RequiredCheckSource = match &github {
        Some(github) => github,
        None => &NoGitHubAccess,
    };
    let mut terminal = Terminal;
    let prompter: Option<&mut dyn Prompter> = if io::stdin().is_terminal() {
        Some(&mut terminal)
    } else {
        None
    };
    let plan = match plan_house_init(&answers, &facts, checks, prompter)? {
        InitDecision::Confirmed(plan) => plan,
        InitDecision::Declined(_) => {
            return Ok(("Declined; nothing was registered.".to_owned(), false));
        }
    };
    let report = register_house(&plan)?;
    let guidance = plan.config.guidance.as_str();
    Ok((
        format!(
            "Registered house {} in {} and pinned {} guidance at {}.\nNo authority or workflows activated.\nSaved your answers as {}. Review it any time.\nNext: from a checkout of an allowed repository, run kitchen house setup --registry '{}'",
            plan.config.house,
            plan.registry.display(),
            if answers.bundle.is_some() {
                "the bundle's"
            } else {
                "the default"
            },
            guidance.get(..7).unwrap_or(guidance),
            report.config_path.display(),
            plan.registry.display(),
        ),
        true,
    ))
}

/// Prompts on standard error; answers from standard input.
struct Terminal;

impl Prompter for Terminal {
    fn show(&mut self, text: &str) -> Result<(), HouseInitError> {
        Ok(writeln!(io::stderr().lock(), "{text}")?)
    }
    fn ask(&mut self, prompt: &str) -> Result<String, HouseInitError> {
        Ok(super::house::prompt(prompt)?)
    }
}

/// Agent executables on `PATH`. Found means an executable file exists; it
/// is never run.
fn installed_agents(path: Option<std::ffi::OsString>) -> InstalledAgents {
    let Some(path) = path else {
        return InstalledAgents::Unknown;
    };
    let directories: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .take(MAX_PATH_ENTRIES)
        .collect();
    let found = [AgentFamily::Claude, AgentFamily::Codex]
        .into_iter()
        .filter(|agent| {
            directories
                .iter()
                .any(|directory| executable(&directory.join(agent.as_str())))
        })
        .collect();
    InstalledAgents::Observed {
        source: "PATH",
        found,
    }
}

fn executable(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Read-only house-scoped GitHub access for the required-checks offer.
struct HouseGitHub {
    requester: ExternalRef,
    credential: CredentialId,
    credential_file: PathBuf,
    gh: PathBuf,
}

impl RequiredCheckSource for HouseGitHub {
    fn required_checks(&self, house: &HouseId, repository: &Repository) -> ObservedChecks {
        let reference = CredentialRef::new(
            house.clone(),
            self.credential.clone(),
            self.requester.clone(),
        );
        let Ok(budget) = PostingBudget::new(0) else {
            return ObservedChecks::Unavailable;
        };
        let (Ok(scope), Ok(credential)) = (
            HouseScope::new(
                house.clone(),
                [repository.clone()],
                self.requester.clone(),
                reference.clone(),
                budget,
                std::iter::empty::<Permission>(),
            ),
            CredentialFile::new(reference, self.credential_file.clone()),
        ) else {
            return ObservedChecks::Unavailable;
        };
        let Ok(gh) = GhCli::new(self.gh.clone(), credential) else {
            return ObservedChecks::Unavailable;
        };
        let client = GitHubClient::new(scope, gh, ReadLimits::default());
        let Observation::Known(info) = client.repository(house, repository) else {
            return ObservedChecks::Unavailable;
        };
        let Observation::Known(required) =
            client.required_checks(house, repository, &info.default_branch)
        else {
            return ObservedChecks::Unavailable;
        };
        ObservedChecks::Observed {
            branch: info.default_branch,
            checks: required
                .contexts
                .into_iter()
                .chain(required.checks.into_iter().map(|check| check.context))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(directory: &std::path::Path, name: &str, mode: u32) -> std::io::Result<()> {
        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }

    #[test]
    fn agents_on_path_are_observed_without_running_them() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first)?;
        std::fs::create_dir_all(&second)?;
        // The fake exits 1 if run; only its presence counts.
        tool(&second, "codex", 0o755)?;
        let path = std::env::join_paths([&first, &second])?;
        assert_eq!(
            installed_agents(Some(path.clone())),
            InstalledAgents::Observed {
                source: "PATH",
                found: vec![AgentFamily::Codex],
            }
        );
        // Not executable, a directory, and a relative entry are not agents.
        #[cfg(unix)]
        tool(&first, "claude", 0o644)?;
        std::fs::create_dir_all(second.join("claude"))?;
        let relative = std::env::join_paths([std::path::Path::new("first"), &first, &second])?;
        assert_eq!(
            installed_agents(Some(relative)),
            InstalledAgents::Observed {
                source: "PATH",
                found: vec![AgentFamily::Codex],
            }
        );
        assert_eq!(
            installed_agents(Some(std::ffi::OsString::new())),
            InstalledAgents::Observed {
                source: "PATH",
                found: Vec::new(),
            }
        );
        assert_eq!(installed_agents(None), InstalledAgents::Unknown);
        Ok(())
    }
}

#[cfg(test)]
mod guided_ids {
    use super::*;

    #[test]
    fn every_guided_argument_conflicts_with_config() {
        let command = <InitArgs as clap::Args>::augment_args(clap::Command::new("init"));
        let mut ids: Vec<&str> = command
            .get_arguments()
            .map(|argument| argument.get_id().as_str())
            .collect();
        ids.sort_unstable();
        let mut guided = GUIDED.to_vec();
        guided.sort_unstable();
        assert_eq!(ids, guided);
    }
}
