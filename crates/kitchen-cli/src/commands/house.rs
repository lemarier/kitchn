use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    adoption::{
        FileMode, HouseRegistry, InstructionBundle, NewFile, RelativePath, SafeInstaller,
        adopt_repository, decode, encode, git_repository_root, read_repository,
        repository_from_path,
    },
    contracts::Repository,
    house::{
        DoctorEvidence, DoctorReport, HouseConfig, HouseError, RepositoryConfig, Workflow, doctor,
    },
};
use std::{
    collections::BTreeSet,
    io::{self, BufRead, Write},
    path::PathBuf,
};

#[derive(Args)]
pub struct HouseArgs {
    #[command(subcommand)]
    command: HouseCommand,
}
#[derive(Subcommand)]
enum HouseCommand {
    /// Register a reviewed external house policy without granting new authority.
    Init {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    /// Adopt a repository; prompt only for house and selected workflows.
    Setup {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        repository_path: Option<PathBuf>,
        #[arg(long)]
        repository: Repository,
        #[arg(long)]
        house: Option<HouseId>,
        /// Comma-separated workflows, or 'none' for interactive-only use.
        #[arg(long)]
        workflows: Option<String>,
        /// Preview without writing the repository binding.
        #[arg(long)]
        preview: bool,
        /// Optional scoped read-only integration observations.
        #[arg(long)]
        evidence: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Install the exact configured pins from a verified local export.
    Sync {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        house: HouseId,
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Explicitly update pins after the complete new bundle verifies.
    Update {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        house: HouseId,
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Diagnose pins, scoped access, labels, and scheduled capabilities.
    Doctor {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long, default_value = ".")]
        repository_path: PathBuf,
        #[arg(long)]
        evidence: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: HouseArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        HouseCommand::Init { registry, config } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let config: HouseConfig = decode(&config)?;
            registry.initialize(&config)?;
            Ok((
                format!(
                    "Registered house {}. No authority or workflows activated.\nNext: kitchen house setup --registry '{}' --repository <owner/name>",
                    config.house,
                    registry.root().display()
                ),
                true,
            ))
        }
        HouseCommand::Sync {
            registry,
            house,
            bundle,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let bundle: InstructionBundle = decode(&bundle)?;
            let resolved = registry.sync(&house, &bundle)?;
            Ok((json_text(&resolved)?, true))
        }
        HouseCommand::Update {
            registry,
            house,
            bundle,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let bundle: InstructionBundle = decode(&bundle)?;
            let current = registry.load(&house)?;
            let resolved = registry.update(&current, &bundle)?;
            Ok((json_text(&resolved)?, true))
        }
        HouseCommand::Doctor {
            registry,
            repository_path,
            evidence,
            json,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let root = canonical_root(repository_path)?;
            let (_, config) = repository_from_path(&root)?;
            diagnose(&registry, &config, evidence, json)
        }
        HouseCommand::Setup {
            registry,
            repository_path,
            repository,
            house,
            workflows,
            preview,
            evidence,
            json,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let root = match repository_path {
                Some(path) => canonical_root(path)?,
                None => git_repository_root(&std::env::current_dir().map_err(HouseError::from)?)?,
            };
            let house = match house {
                Some(house) => house,
                None => {
                    let listing = registry.houses()?;
                    for (house, error) in &listing.unavailable {
                        writeln!(io::stderr().lock(), "House {house} unavailable: {error}")
                            .map_err(HouseError::from)?;
                    }
                    let eligible: Vec<_> = listing
                        .available
                        .iter()
                        .filter(|house| house.repositories.contains(&repository))
                        .map(|house| house.house.as_str())
                        .collect();
                    if eligible.is_empty() {
                        return Err(HouseError::HouseSelection.into());
                    }
                    let answer = prompt(&format!("House ({}): ", eligible.join(", ")))?;
                    HouseId::new(&answer)?
                }
            };
            let workflows = match workflows {
                Some(value) => parse_workflows(&value)?,
                None => parse_workflows(&prompt(
                    "Workflows (pickup,triage,gate,gardener,dishwasher,inspector; or none): ",
                )?)?,
            };
            let existing = match read_repository(&root) {
                Ok(config) => Some(config),
                Err(HouseError::HouseSelection) => None,
                Err(error) => return Err(error.into()),
            };
            let config = RepositoryConfig {
                schema: 1,
                house,
                repository,
                workflows,
                additional_reviewers: existing
                    .as_ref()
                    .map_or_else(BTreeSet::new, |config| config.additional_reviewers.clone()),
                additional_checks: existing
                    .as_ref()
                    .map_or_else(BTreeSet::new, |config| config.additional_checks.clone()),
            };
            let house = registry.load(&config.house)?;
            config.validate(&house)?;
            let evidence: Option<DoctorEvidence> = evidence.as_deref().map(decode).transpose()?;
            // Scope-check observations and preview label changes before any write.
            let report = doctor(&registry, &config, evidence.as_ref())?;
            let binding = if let Some(existing) = &existing {
                if existing.house != config.house || existing.repository != config.repository {
                    BindingStatus::Refused
                } else if existing == &config {
                    BindingStatus::Unchanged
                } else {
                    BindingStatus::Updated
                }
            } else {
                let path = RelativePath::new(".kitchen.json")?;
                let contents = encode(&config)?;
                let plan = SafeInstaller::preview(
                    &root,
                    &[NewFile {
                        path: &path,
                        contents: &contents,
                        mode: FileMode::Regular,
                    }],
                )?;
                if plan.has_conflicts() {
                    BindingStatus::Conflict
                } else {
                    BindingStatus::Created
                }
            };
            let accepted = matches!(
                binding,
                BindingStatus::Created | BindingStatus::Unchanged | BindingStatus::Updated
            );
            if !preview && accepted {
                if let Some(existing) = &existing {
                    registry.configure_repository(&root, existing, &config)?;
                } else {
                    adopt_repository(&root, &config, &house)?;
                }
            }
            let result = if json {
                json_text(&SetupReport {
                    preview,
                    binding,
                    written: !preview
                        && matches!(binding, BindingStatus::Created | BindingStatus::Updated),
                    doctor: &report,
                })?
            } else {
                let action = match (binding, preview) {
                    (BindingStatus::Refused, _) => "Refused rebinding",
                    (BindingStatus::Conflict, _) => "Conflicting binding",
                    (BindingStatus::Unchanged, _) => "Unchanged binding",
                    (BindingStatus::Created, true) => "Would adopt",
                    (BindingStatus::Updated, true) => "Would update",
                    (BindingStatus::Created, false) => "Adopted",
                    (BindingStatus::Updated, false) => "Updated",
                };
                format!(
                    "{action} .kitchen.json for {}.\n{}",
                    config.repository,
                    report.human_readable()
                )
            };
            Ok((result, accepted))
        }
    }
}
fn diagnose(
    registry: &HouseRegistry,
    config: &RepositoryConfig,
    evidence: Option<PathBuf>,
    json: bool,
) -> Result<(String, bool), kitchen::Error> {
    let evidence: Option<DoctorEvidence> = evidence.as_deref().map(decode).transpose()?;
    let report = doctor(registry, config, evidence.as_ref())?;
    Ok((
        if json {
            json_text(&report)?
        } else {
            report.human_readable()
        },
        report.healthy(),
    ))
}
fn parse_workflows(value: &str) -> Result<BTreeSet<Workflow>, HouseError> {
    if value == "none" {
        return Ok(BTreeSet::new());
    }
    value.split(',').map(|name| name.trim().parse()).collect()
}
fn prompt(message: &str) -> Result<String, HouseError> {
    let mut output = io::stderr().lock();
    write!(output, "{message}")?;
    output.flush()?;
    let mut line = Vec::new();
    // Bounded input even on piped input; EOF is not consent or a default house.
    let mut input = io::stdin().lock();
    for _ in 0..1024 {
        let available = input.fill_buf()?;
        let Some(byte) = available.first().copied() else {
            return Err(HouseError::InvalidInput);
        };
        input.consume(1);
        if byte == b'\n' {
            return String::from_utf8(line)
                .map(|value| value.trim().to_owned())
                .map_err(|_| HouseError::InvalidInput);
        }
        line.push(byte);
    }
    Err(HouseError::InvalidInput)
}
fn canonical_root(path: PathBuf) -> Result<PathBuf, HouseError> {
    // Preserve path redirects for the library to reject; canonicalization would
    // erase the evidence. Make only the normal current-directory prefix absolute.
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
fn json_text(value: &impl serde::Serialize) -> Result<String, HouseError> {
    String::from_utf8(encode(value)?).map_err(|_| HouseError::InvalidInput)
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum BindingStatus {
    Created,
    Unchanged,
    Updated,
    Conflict,
    Refused,
}
#[derive(serde::Serialize)]
struct SetupReport<'a> {
    preview: bool,
    binding: BindingStatus,
    written: bool,
    #[serde(flatten)]
    doctor: &'a DoctorReport,
}
