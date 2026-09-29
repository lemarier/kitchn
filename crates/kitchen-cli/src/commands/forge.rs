//! `kitchn forge`: the private binding a house writes to its forge with.
use clap::{Args, Subcommand};
use kitchen::{
    BackendId, CredentialId, HouseId,
    adoption::HouseRegistry,
    contracts::{EffectExecutor, ExternalRef, NotAppliedReason, PostingBudget, SystemClock},
    house::{
        BindOutcome, CredentialKind, CredentialStatus, FORGE_BINDING_SCHEMA, ForgeBinding,
        ForgeCredential, ForgeError, ForgeKind, ForgeReader, bind_forge, credential_path,
        credential_status, forge_binding, forge_reader,
    },
    integrations::github::{AppId, AppTokens, CurlApi, GhCli, GitHubApp, InstallationId},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

/// Longest wait for `gh` to report its logged-in account.
const GH_DEADLINE: Duration = Duration::from_secs(3);
/// Most bytes read from `gh` for one login.
const GH_OUTPUT_BYTES: u64 = 1024;
/// Most `PATH` entries searched for `gh`.
const MAX_PATH_ENTRIES: usize = 256;

#[derive(Args)]
pub struct ForgeArgs {
    #[command(subcommand)]
    command: ForgeCommand,
}

#[derive(Subcommand)]
enum ForgeCommand {
    /// Bind a house to the GitHub account or app it writes as. Stores no
    /// credential: the token or app private key stays in a file only you
    /// place, in the house's private registry directory.
    Bind {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        house: HouseId,
        /// The GitHub login the token must authenticate as; for an app, its
        /// `<app-slug>[bot]` login.
        #[arg(long)]
        requester: ExternalRef,
        /// Write as this GitHub App; the credential file then holds its PEM
        /// private key. Needs --installation.
        #[arg(long, requires = "installation")]
        app_id: Option<u64>,
        /// The app's installation on the account owning the repositories.
        #[arg(long, requires = "app_id")]
        installation: Option<u64>,
        /// Credential name; house policy limits for forge writes must name it.
        #[arg(long, default_value = "github")]
        credential: CredentialId,
        /// Most writes one task may make (0 to 100).
        #[arg(long, default_value_t = 20)]
        posting_budget: u32,
    },
    /// Show a house's forge binding and whether its token file is in place.
    /// Exits 1 when the token file is not ready.
    Show {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        house: HouseId,
    },
}

pub fn run(args: ForgeArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        ForgeCommand::Bind {
            registry,
            house,
            requester,
            app_id,
            installation,
            credential,
            posting_budget,
        } => {
            let registry = HouseRegistry::new(super::house::canonical_root(registry)?)?;
            let credential_kind = match (app_id, installation) {
                (Some(app_id), Some(installation)) => CredentialKind::GitHubApp(GitHubApp {
                    app_id: AppId::new(app_id)?,
                    installation: InstallationId::new(installation)?,
                }),
                // Clap requires both or neither.
                _ => CredentialKind::Token,
            };
            let binding = ForgeBinding {
                schema: FORGE_BINDING_SCHEMA,
                house,
                forge: ForgeKind::GitHub,
                backend: BackendId::new("github")?,
                requester,
                credential,
                credential_kind,
                posting_budget: PostingBudget::new(posting_budget)?,
            };
            let outcome = bind_forge(&registry, &binding)?;
            let (path, status) = token(&registry, &binding)?;
            Ok((
                format!(
                    "{} house {} to {} as {}{}.\n{}",
                    match outcome {
                        BindOutcome::Created => "Bound",
                        BindOutcome::Unchanged => "Already bound",
                    },
                    binding.house,
                    binding.forge,
                    binding.requester.as_str(),
                    app_text(&binding),
                    token_text(&binding, &path, status),
                ),
                true,
            ))
        }
        ForgeCommand::Show { registry, house } => {
            let registry = HouseRegistry::new(super::house::canonical_root(registry)?)?;
            let binding = forge_binding(&registry, &house)?;
            let (path, status) = token(&registry, &binding)?;
            Ok((
                format!(
                    "House {} writes to {} as {}{} with credential {}, at most {} writes per task.\n{}",
                    binding.house,
                    binding.forge,
                    binding.requester.as_str(),
                    app_text(&binding),
                    binding.credential,
                    binding.posting_budget.limit(),
                    token_text(&binding, &path, status),
                ),
                status == CredentialStatus::Ready,
            ))
        }
    }
}

fn token(
    registry: &HouseRegistry,
    binding: &ForgeBinding,
) -> Result<(PathBuf, CredentialStatus), kitchen::Error> {
    let path = credential_path(registry, binding)?;
    let status = credential_status(registry, binding)?;
    Ok((path, status))
}

/// The app and installation an app binding writes through, or nothing.
fn app_text(binding: &ForgeBinding) -> String {
    match binding.credential_kind {
        CredentialKind::Token => String::new(),
        CredentialKind::GitHubApp(app) => {
            format!(
                " (GitHub App {}, installation {})",
                app.app_id, app.installation
            )
        }
    }
}

/// Where the token file belongs, its state, and how to place it. Never
/// prints or reads the token.
pub fn token_text(binding: &ForgeBinding, path: &Path, status: CredentialStatus) -> String {
    let file = path.display();
    if let CredentialKind::GitHubApp(app) = binding.credential_kind {
        return key_text(app, path, status);
    }
    match status {
        CredentialStatus::Ready => format!(
            "Token file {file} is ready. Kitchen reads it only when it writes and never copies it."
        ),
        // Writing through the path would follow what is there, so only
        // removal is suggested.
        CredentialStatus::NotRegularFile
        | CredentialStatus::Redirected
        | CredentialStatus::NotOwned => format!(
            "Token file {file} is {status}, so kitchn will not use it. Remove what is there, keep every directory on that path a real directory you own, then place a token for {login} readable only by you.",
            login = binding.requester.as_str(),
        ),
        CredentialStatus::Missing | CredentialStatus::Exposed => {
            let directory = quote(&path.parent().unwrap_or(path).display().to_string());
            format!(
                "Token file {file} is {status}. Place a token for {login} there, readable only by you, for example:\n  mkdir -p {directory} && (umask 077; gh auth token --user {user} > {target})\nKitchen reads it only when it writes and never copies it.",
                login = binding.requester.as_str(),
                user = quote(binding.requester.as_str()),
                target = quote(&file.to_string()),
            )
        }
    }
}

/// Where an app's private key file belongs, its state, and how to place it.
/// Never prints or reads the key.
fn key_text(app: GitHubApp, path: &Path, status: CredentialStatus) -> String {
    let file = path.display();
    match status {
        CredentialStatus::Ready => format!(
            "Private key file {file} of GitHub App {} is ready. Kitchen reads it only to mint installation tokens when it writes and never copies it.",
            app.app_id
        ),
        CredentialStatus::NotRegularFile
        | CredentialStatus::Redirected
        | CredentialStatus::NotOwned => format!(
            "Private key file {file} is {status}, so kitchn will not use it. Remove what is there, keep every directory on that path a real directory you own, then place the private key of GitHub App {} readable only by you.",
            app.app_id
        ),
        CredentialStatus::Missing | CredentialStatus::Exposed => {
            let directory = quote(&path.parent().unwrap_or(path).display().to_string());
            format!(
                "Private key file {file} is {status}. Place the .pem private key of GitHub App {} there, readable only by you, for example:\n  mkdir -p {directory} && (umask 077; cp <downloaded-key.pem> {target})\nKitchen reads it only to mint installation tokens when it writes and never copies it.",
                app.app_id,
                target = quote(&file.to_string()),
            )
        }
    }
}

/// Single-quote `text` for a POSIX shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The first `gh` in the absolute entries of `path`.
fn gh_executable(path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    executable(path, "gh")
}

/// The first `name` in the absolute entries of `path`.
fn executable(path: Option<std::ffi::OsString>, name: &str) -> Option<PathBuf> {
    std::env::split_paths(&path?)
        .filter(|directory| directory.is_absolute())
        .take(MAX_PATH_ENTRIES)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// The GitHub CLI on `PATH`, over the house's checked credential file. `gh`
/// gets only that token, or for an app the installation tokens `curl` on
/// `PATH` mints with its key, never the host's own login.
pub fn connect_gh(credential: ForgeCredential) -> Result<GhCli, ForgeError> {
    let path = std::env::var_os("PATH");
    let gh = gh_executable(path.clone()).ok_or(ForgeError::GhNotFound)?;
    match credential {
        ForgeCredential::Token(file) => GhCli::new(gh, file),
        ForgeCredential::App { app, key } => {
            let curl = executable(path, "curl").ok_or(ForgeError::CurlNotFound)?;
            let api = CurlApi::new(curl).map_err(ForgeError::Integration)?;
            GhCli::app(gh, AppTokens::new(app, key, api, Arc::new(SystemClock)))
        }
    }
    .map_err(ForgeError::Integration)
}

/// Whether a command re-reads the forge, and why not when it does not.
pub enum Reread {
    /// Through the house's forge binding.
    Forge(Box<ForgeReader<GhCli>>),
    /// The person chose not to.
    Declined,
    /// The house has no forge binding.
    Unbound,
    /// No registry was given to read the binding from.
    NoRegistry,
}

impl Reread {
    /// Build the house's forge reader, unless `declined`. A house without a
    /// forge binding is not re-read; any other refusal is an error, since
    /// the person expects the forge to be checked.
    pub fn open(
        registry: &HouseRegistry,
        house: &HouseId,
        declined: bool,
    ) -> Result<Self, kitchen::Error> {
        if declined {
            return Ok(Self::Declined);
        }
        match forge_reader(registry, house, connect_gh) {
            Ok(reader) => Ok(Self::Forge(Box::new(reader))),
            Err(ForgeError::MissingBinding { .. }) => Ok(Self::Unbound),
            Err(error) => Err(error.into()),
        }
    }

    /// The executor to re-read with, if any.
    pub fn executor(&self) -> Option<&dyn EffectExecutor> {
        match self {
            Self::Forge(reader) => Some(reader.as_ref()),
            Self::Declined | Self::Unbound | Self::NoRegistry => None,
        }
    }

    /// A sentence for a report, when the forge was not re-read.
    pub fn skipped(&self, house: &HouseId) -> Option<String> {
        match self {
            Self::Forge(_) => None,
            Self::Declined => Some("The forge was not re-read (--without-forge).".to_owned()),
            Self::Unbound => Some(format!(
                "The forge was not re-read: house {house} has no forge binding (`kitchn forge bind`)."
            )),
            Self::NoRegistry => Some(
                "The forge was not re-read: pass --registry to use the house's forge binding."
                    .to_owned(),
            ),
        }
    }
}

/// Why the forge refused a write, for a report.
pub fn not_applied(reason: NotAppliedReason) -> String {
    match reason {
        NotAppliedReason::Rejected => "rejected; check the token's account and access".to_owned(),
        NotAppliedReason::RateLimited {
            retry_after: Some(delay),
        } => format!("rate limited; retry after {}s", delay.as_secs()),
        NotAppliedReason::RateLimited { retry_after: None } => "rate limited".to_owned(),
        NotAppliedReason::Unsupported(capability) => format!("unsupported: {capability}"),
        NotAppliedReason::CrossHouse => "another house's request".to_owned(),
        NotAppliedReason::ForeignBackend => "another backend's request".to_owned(),
        NotAppliedReason::ConfirmedAbsent => "confirmed absent".to_owned(),
    }
}

/// The GitHub login `gh` is logged in as, from its configuration. Runs
/// `gh config get user`, which reads no token; any failure means unknown.
pub fn gh_login(path: Option<std::ffi::OsString>) -> Option<ExternalRef> {
    let gh = gh_executable(path)?;
    let mut command = Command::new(gh);
    command
        .args(["config", "get", "user", "--host", "github.com"])
        .env_clear()
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in ["HOME", "XDG_CONFIG_HOME", "GH_CONFIG_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GH_DEADLINE => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                // Best effort: the login stays unknown either way.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut output = String::new();
    child
        .stdout
        .take()?
        .take(GH_OUTPUT_BYTES)
        .read_to_string(&mut output)
        .ok()?;
    ExternalRef::new(output.trim())
        .ok()
        .filter(|login| ForgeKind::GitHub.accepts_requester(login))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_gh(directory: &Path, script: &str) -> std::io::Result<()> {
        let path = directory.join("gh");
        std::fs::write(&path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    #[test]
    fn hints_quote_paths_and_logins_for_the_shell() {
        assert_eq!(quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(quote("octo-cat[bot]"), "'octo-cat[bot]'");
    }

    #[cfg(unix)]
    #[test]
    fn the_gh_login_is_read_from_its_config_command() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().canonicalize()?;
        let path = std::env::join_paths([&directory])?;
        assert_eq!(gh_login(Some(path.clone())), None, "no gh on PATH");

        fake_gh(
            &directory,
            "#!/bin/sh\n[ \"$*\" = 'config get user --host github.com' ] || exit 3\n[ -z \"$GH_TOKEN\" ] || exit 4\necho octo-cat\n",
        )?;
        assert_eq!(
            gh_login(Some(path.clone())),
            Some(ExternalRef::new("octo-cat")?)
        );

        // Logged out, unparseable, and hung all leave the login unknown.
        fake_gh(&directory, "#!/bin/sh\nexit 1\n")?;
        assert_eq!(gh_login(Some(path.clone())), None);
        // A login the forge could not issue is not offered as a default.
        fake_gh(&directory, "#!/bin/sh\necho two--hyphens\n")?;
        assert_eq!(gh_login(Some(path.clone())), None);
        // Enterprise Managed User logins carry an underscore.
        fake_gh(&directory, "#!/bin/sh\necho user_acme\n")?;
        assert_eq!(
            gh_login(Some(path.clone())),
            Some(ExternalRef::new("user_acme")?)
        );
        fake_gh(&directory, "#!/bin/sh\necho 'not a login'\n")?;
        assert_eq!(gh_login(Some(path.clone())), None);
        fake_gh(&directory, "#!/bin/sh\nexec /bin/sleep 30\n")?;
        let started = Instant::now();
        assert_eq!(gh_login(Some(path)), None);
        assert!(started.elapsed() < GH_DEADLINE + Duration::from_secs(2));
        assert_eq!(gh_login(None), None);
        Ok(())
    }
}
