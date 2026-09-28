use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    adoption::{
        HouseRegistry, InstructionBundle, LegacyImportStatus, RepositoryMatch, decode, encode,
        legacy_binding,
    },
    contracts::Repository,
    house::{
        DoctorEvidence, DoctorFinding, DoctorReport, HouseConfig, HouseError,
        REPOSITORY_BINDING_SCHEMA, RepositoryConfig, Workflow, doctor,
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
    /// Bind a repository in the registry; prompt only for house and workflows.
    /// Nothing is written to the repository's working tree.
    Setup {
        #[arg(long)]
        registry: PathBuf,
        /// Checkout whose Git remotes identify the repository (default: the current directory).
        #[arg(long, conflicts_with = "repository")]
        repository_path: Option<PathBuf>,
        /// GitHub owner/name, instead of reading a checkout's remotes.
        #[arg(long)]
        repository: Option<Repository>,
        /// The house to bind; stored in the registry as the one-time choice.
        #[arg(long)]
        house: Option<HouseId>,
        /// Comma-separated workflows, or 'none' for interactive-only use.
        #[arg(long)]
        workflows: Option<String>,
        /// Preview without storing the registry binding.
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
    /// Copy a legacy .kitchen.json binding into the registry; previews unless --yes.
    /// The file is never modified or deleted.
    Import {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long, default_value = ".")]
        repository_path: PathBuf,
        /// Store the previewed binding.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
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
                    "Registered house {}. No authority or workflows activated.\nNext: from a checkout of an allowed repository, run kitchen house setup --registry '{}'",
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
            let start = canonical_root(repository_path)?;
            // Only a hint: when the checkout cannot be read, resolution below
            // fails with that error instead.
            let legacy = legacy_binding(&start).ok().flatten();
            match registry.resolve_repository(&start) {
                Ok(RepositoryMatch::Bound(config)) => {
                    diagnose(&registry, &config, legacy.as_deref(), evidence, json)
                }
                Ok(RepositoryMatch::Unbound { repository, house }) => Ok((
                    format!(
                        "Repository {repository} is claimed by house {house} but not set up.\nNext: kitchen house setup --registry '{}' --repository {repository} --house {house}",
                        registry.root().display()
                    ),
                    false,
                )),
                Err(error) => {
                    if let Some(legacy) = legacy {
                        writeln!(
                            io::stderr().lock(),
                            "Found legacy binding {}; import it with kitchen house import --registry '{}'.",
                            legacy.display(),
                            registry.root().display()
                        )
                        .map_err(HouseError::from)?;
                    }
                    Err(error.into())
                }
            }
        }
        HouseCommand::Import {
            registry,
            repository_path,
            yes,
            json,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let start = canonical_root(repository_path)?;
            let import = registry.import_legacy(&start, yes)?;
            let accepted = import.status != LegacyImportStatus::Conflict;
            if json {
                return Ok((json_text(&import)?, accepted));
            }
            let source = import.source.display();
            let binding = format!(
                "{} -> house {}",
                import.binding.repository, import.binding.house
            );
            let text = match import.status {
                LegacyImportStatus::WouldCreate => format!(
                    "Would import {source} into the registry as {binding}.\nNext: rerun with --yes to store it."
                ),
                LegacyImportStatus::Created => format!(
                    "Imported {source} into the registry as {binding}.\nNext: delete {source} yourself when no older Kitchen needs it; Kitchen does not delete repository files."
                ),
                LegacyImportStatus::Unchanged => format!(
                    "The registry already holds {binding}.\nNext: delete {source} yourself; Kitchen does not delete repository files."
                ),
                LegacyImportStatus::Conflict => format!(
                    "The registry holds a different binding for {}; nothing imported.\nNext: compare it with {source} and change the registry binding with kitchen house setup.",
                    import.binding.repository
                ),
            };
            Ok((text, accepted))
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
            let (repository, existing) = match repository {
                Some(repository) => {
                    let existing = registry.binding(&repository)?;
                    // Keys ignore case; keep the stored spelling of a bound repository.
                    let repository = existing
                        .as_ref()
                        .map_or(repository, |existing| existing.repository.clone());
                    (repository, existing)
                }
                None => {
                    let start = canonical_root(repository_path.unwrap_or_else(|| ".".into()))?;
                    registry.claims(&start)?.setup_target()?
                }
            };
            let house = match (house, &existing) {
                (Some(house), _) => house,
                (None, Some(existing)) => existing.house.clone(),
                (None, None) => {
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
            let config = RepositoryConfig {
                schema: REPOSITORY_BINDING_SCHEMA,
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
            let binding = match &existing {
                Some(existing)
                    if existing.house != config.house
                        || existing.repository != config.repository =>
                {
                    BindingStatus::Refused
                }
                Some(existing) if existing == &config => BindingStatus::Unchanged,
                Some(_) => BindingStatus::Updated,
                None => BindingStatus::Created,
            };
            let accepted = matches!(
                binding,
                BindingStatus::Created | BindingStatus::Unchanged | BindingStatus::Updated
            );
            if !preview {
                match (&existing, binding) {
                    (Some(existing), BindingStatus::Updated) => {
                        registry.configure_repository(existing, &config)?;
                    }
                    (None, BindingStatus::Created) => {
                        registry.bind_repository(&config)?;
                    }
                    _ => {}
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
                    (BindingStatus::Unchanged, _) => "Unchanged binding",
                    (BindingStatus::Created, true) => "Would bind",
                    (BindingStatus::Updated, true) => "Would update",
                    (BindingStatus::Created, false) => "Bound",
                    (BindingStatus::Updated, false) => "Updated",
                };
                format!(
                    "{action} {} to house {} in the registry; the working tree is unchanged.\n{}",
                    config.repository,
                    config.house,
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
    legacy: Option<&std::path::Path>,
    evidence: Option<PathBuf>,
    json: bool,
) -> Result<(String, bool), kitchen::Error> {
    let evidence: Option<DoctorEvidence> = evidence.as_deref().map(decode).transpose()?;
    let mut report = doctor(registry, config, evidence.as_ref())?;
    report
        .findings
        .extend(legacy.map(DoctorFinding::legacy_binding));
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
