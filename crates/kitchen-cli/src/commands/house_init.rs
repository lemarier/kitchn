//! Guided `house init`: flags, the terminal prompt, and the environment
//! observations the library's wizard takes as input.
use clap::Args;
use kitchen::{
    CredentialId, HouseId,
    adapters::orca::{ManagedAccounts, SystemRunner, managed_accounts},
    adoption::{checkout_remotes, decode},
    contracts::{BranchName, CommitId, ExternalRef, Permission, PostingBudget, Repository},
    house::{
        AgentEvidence, AgentInventory, HouseError, HouseInitError, InitAnswers, InitDecision,
        InitFacts, NoGitHubAccess, ObservedChecks, Probe, Prompter, RequiredCheckSource,
        plan_house_init, register_house,
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
pub const GUIDED: [&str; 19] = [
    "house",
    "repositories",
    "posting_destinations",
    "sous_chef",
    "station_cook",
    "expediter",
    "required_checks",
    "required_reviewers",
    "fix_rounds",
    "review_requests",
    "forge_requester",
    "forge_credential",
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
    /// Review-fix rounds allowed per pull request, 0 to 10 (default: 2).
    #[arg(long)]
    fix_rounds: Option<String>,
    /// Review requests allowed per pull request head, 0 to 10 (default: 1).
    #[arg(long)]
    review_requests: Option<String>,
    /// GitHub login kitchn writes as, or none (default: the logged-in gh
    /// account, else none). Stores a forge binding, never a credential.
    #[arg(long)]
    forge_requester: Option<String>,
    /// Credential name of the forge binding (default: github).
    #[arg(long)]
    forge_credential: Option<String>,
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
        fix_rounds: args.fix_rounds,
        review_requests: args.review_requests,
        forge_requester: args.forge_requester,
        forge_credential: args.forge_credential,
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
        agents: {
            let path = std::env::var_os("PATH");
            agent_inventory(path.as_deref(), orca_accounts(path.as_deref()))
        },
        forge_login: super::forge::gh_login(std::env::var_os("PATH")),
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
    if answers.yes {
        // The interactive path already showed the config when asking.
        announce(
            &format!("{}\n{}", plan.config_text()?, plan.forge_text()),
            &mut io::stderr().lock(),
        )?;
    }
    let report = register_house(&plan)?;
    let guidance = plan.config.guidance.as_str();
    let forge = match &report.forge {
        Some(bound) => format!(
            "\nBound the house to {} as {}.\n{}",
            bound.binding.forge,
            bound.binding.requester.as_str(),
            super::forge::token_text(&bound.binding, &bound.credential_path, bound.credential),
        ),
        None => String::new(),
    };
    Ok((
        format!(
            "Registered house {} in {} and pinned {} guidance at {}.\nNo authority or workflows activated.{forge}\n{}\nSaved your answers as {}. Review it any time.\nNext: from a checkout of an allowed repository, run kitchn house setup --registry '{}'",
            plan.config.house,
            plan.registry.display(),
            if answers.bundle.is_some() {
                "the bundle's"
            } else {
                "the default"
            },
            guidance.get(..7).unwrap_or(guidance),
            super::house::store_text(&report.store),
            report.config_path.display(),
            plan.registry.display(),
        ),
        true,
    ))
}

/// Writes the config about to be registered. A failure aborts before any write.
fn announce(text: &str, out: &mut impl Write) -> Result<(), HouseError> {
    writeln!(out, "{text}")?;
    out.flush()?;
    Ok(())
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

/// The executable `name` in the absolute `PATH` entries, if any. Found means
/// an executable file exists; it is never run by this lookup.
fn on_path(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .take(MAX_PATH_ENTRIES)
        .map(|directory| directory.join(name))
        .find(|candidate| executable(candidate))
}

/// The managed accounts `orca account list --json` reports, or `None` when
/// Orca is not on `PATH` or does not answer. Only the account list is read;
/// no agent is started.
fn orca_accounts(path: Option<&std::ffi::OsStr>) -> Option<ManagedAccounts> {
    let orca = on_path(path?, "orca")?;
    managed_accounts(&SystemRunner::new(orca)).ok()
}

/// What `PATH` and Orca's account list show for each agent. A missing `PATH`
/// or an unreadable account list is unknown, never absent.
fn agent_inventory(
    path: Option<&std::ffi::OsStr>,
    accounts: Option<ManagedAccounts>,
) -> AgentInventory {
    let evidence = |agent: AgentFamily| AgentEvidence {
        path: match path {
            None => Probe::Unknown,
            Some(path) if on_path(path, agent.as_str()).is_some() => Probe::Found,
            Some(_) => Probe::NotFound,
        },
        orca_account: match accounts.and_then(|accounts| accounts.count(agent)) {
            None => Probe::Unknown,
            Some(0) => Probe::NotFound,
            Some(_) => Probe::Found,
        },
    };
    AgentInventory {
        claude: evidence(AgentFamily::Claude),
        codex: evidence(AgentFamily::Codex),
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
        let Ok(default_branch) = BranchName::new(&info.default_branch) else {
            return ObservedChecks::Unavailable;
        };
        let Observation::Known(required) =
            client.required_checks(house, repository, &default_branch)
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

    fn accounts(claude: Option<usize>, codex: Option<usize>) -> Option<ManagedAccounts> {
        Some(ManagedAccounts { claude, codex })
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
        let found = agent_inventory(Some(&path), None);
        assert_eq!(found.codex.path, Probe::Found);
        assert_eq!(found.claude.path, Probe::NotFound);
        // Not executable, a directory, and a relative entry are not agents.
        #[cfg(unix)]
        tool(&first, "claude", 0o644)?;
        std::fs::create_dir_all(second.join("claude"))?;
        let relative = std::env::join_paths([std::path::Path::new("first"), &first, &second])?;
        assert_eq!(agent_inventory(Some(&relative), None), found);
        let empty = agent_inventory(Some(std::ffi::OsStr::new("")), None);
        assert_eq!(empty.claude.path, Probe::NotFound);
        assert_eq!(empty.codex.path, Probe::NotFound);
        // Without a PATH nothing is claimed either way.
        assert_eq!(agent_inventory(None, None), AgentInventory::UNKNOWN);
        Ok(())
    }

    #[test]
    fn orca_accounts_are_kept_apart_from_path_evidence() {
        let none = std::ffi::OsStr::new("");
        let inventory = agent_inventory(Some(none), accounts(Some(2), Some(0)));
        assert_eq!(inventory.claude.orca_account, Probe::Found);
        assert_eq!(inventory.codex.orca_account, Probe::NotFound);
        assert_eq!(inventory.claude.path, Probe::NotFound);
        // A family the account list did not report, and an unreadable list,
        // are unknown rather than absent.
        let partial = agent_inventory(Some(none), accounts(None, Some(1)));
        assert_eq!(partial.claude.orca_account, Probe::Unknown);
        assert_eq!(partial.codex.orca_account, Probe::Found);
        assert_eq!(
            agent_inventory(Some(none), None).claude.orca_account,
            Probe::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_orca_on_path_is_asked_only_for_its_account_list()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let directory = temp.path().canonicalize()?;
        let path = std::env::join_paths([&directory])?;
        assert_eq!(orca_accounts(Some(&path)), None, "no orca on PATH");
        assert_eq!(orca_accounts(None), None);
        let write = |script: &str| -> std::io::Result<()> {
            let file = directory.join("orca");
            std::fs::write(&file, script)?;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
        };
        write(
            "#!/bin/sh\n[ \"$*\" = 'account list --json' ] || exit 3\necho '{\"ok\":true,\"result\":{\"claude\":{\"accounts\":[{}]},\"codex\":{\"accounts\":[]}}}'\n",
        )?;
        assert_eq!(orca_accounts(Some(&path)), accounts(Some(1), Some(0)));
        // A failing or garbled Orca leaves the accounts unknown.
        write("#!/bin/sh\nexit 1\n")?;
        assert_eq!(orca_accounts(Some(&path)), None);
        write("#!/bin/sh\necho garbage\n")?;
        assert_eq!(orca_accounts(Some(&path)), None);
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

    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn announce_writes_the_config_with_a_newline() {
        let mut out = Vec::new();
        assert!(announce("{}", &mut out).is_ok());
        assert_eq!(out, b"{}\n");
    }

    #[test]
    fn announce_reports_an_output_failure() {
        assert!(announce("{}", &mut Broken).is_err());
    }
}
