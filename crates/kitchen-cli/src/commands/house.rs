use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    adapters::orca::{DEFAULT_CALL_TIMEOUT, OrcaError, SystemRunner, probe_coordinator},
    adoption::{
        BindingDigest, HouseRegistry, InstructionBundle, LegacyImportStatus, RepositoryMatch,
        StoreOutcome, StoreSetup, decode, encode, legacy_binding,
    },
    contracts::Repository,
    house::{
        DoctorCode, DoctorEvidence, DoctorFinding, DoctorReport, HouseConfig, HouseError,
        REPOSITORY_BINDING_SCHEMA, RepositoryConfig, Workflow, doctor, runtime_config,
    },
    state::{HouseStore, StoreOptions},
};
use std::{
    collections::BTreeSet,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

#[derive(Args)]
pub struct HouseArgs {
    #[command(subcommand)]
    command: HouseCommand,
}
#[derive(Subcommand)]
enum HouseCommand {
    /// Preview and explicitly grant standing house authority.
    Grant(super::house_grant::GrantArgs),
    /// Preview and revoke standing house authority.
    Revoke(super::house_grant::GrantArgs),
    /// Register a house without granting new authority. Without --config,
    /// asks only for what cannot be inferred and pins the default guidance.
    Init {
        /// External registry directory (default: ~/.kitchn).
        #[arg(long)]
        registry: Option<PathBuf>,
        /// A reviewed house policy file; registers it without prompts.
        #[arg(long, requires = "registry", conflicts_with_all = super::house_init::GUIDED)]
        config: Option<PathBuf>,
        #[command(flatten)]
        guided: Box<super::house_init::InitArgs>,
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
    /// Copy a legacy .kitchen.json binding into the registry. Previews by
    /// default; --yes --digest stores the previewed binding and refuses if the
    /// file changed since. The file is never modified or deleted.
    Import {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long, default_value = ".")]
        repository_path: PathBuf,
        /// Store the previewed binding; requires its --digest.
        #[arg(long, requires = "digest")]
        yes: bool,
        /// The digest the preview printed; approves exactly that content.
        #[arg(long, requires = "yes")]
        digest: Option<BindingDigest>,
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
        /// The house's initialized state store, read to report how full its
        /// shared tables are. Replaces any store capacity in --evidence.
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: HouseArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        HouseCommand::Grant(args) => super::house_grant::run(args, false),
        HouseCommand::Revoke(args) => super::house_grant::run(args, true),
        HouseCommand::Init {
            registry,
            config: None,
            guided,
        } => super::house_init::run(registry, guided),
        HouseCommand::Init {
            registry,
            config: Some(config),
            guided: _,
        } => {
            // Clap requires --registry with --config.
            let registry = registry.ok_or(HouseError::InvalidInput)?;
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let config: HouseConfig = decode(&config)?;
            registry.initialize(&config)?;
            // Registered already: say so, as the guided path does, so a rerun
            // is known to resume rather than start over.
            let store = registry.initialize_store(&config.house).map_err(|source| {
                kitchen::house::HouseInitError::StoreNotInitialized {
                    source: Box::new(source),
                }
            })?;
            Ok((
                format!(
                    "Registered house {}. No authority or workflows activated.\n{}\nNext: from a checkout of an allowed repository, run kitchn house setup --registry '{}'",
                    config.house,
                    store_text(&store),
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
            store,
            json,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let start = canonical_root(repository_path)?;
            // Only a hint: when the checkout cannot be read, resolution below
            // fails with that error instead.
            let legacy = legacy_binding(&start).ok().flatten();
            match registry.resolve_repository(&start) {
                Ok(RepositoryMatch::Bound(config)) => diagnose(
                    &registry,
                    &config,
                    legacy.as_deref(),
                    Observed { evidence, store },
                    json,
                ),
                Ok(RepositoryMatch::Unbound { repository, house }) => {
                    let root = registry.root().display();
                    let next = match &legacy {
                        Some(legacy) => format!(
                            "kitchn house import --registry '{root}' to keep the settings in {}",
                            legacy.display()
                        ),
                        None => format!(
                            "kitchn house setup --registry '{root}' --repository {repository} --house {house}"
                        ),
                    };
                    Ok((
                        format!(
                            "Repository {repository} is claimed by house {house} but not set up.\nNext: {next}"
                        ),
                        false,
                    ))
                }
                Err(error) => {
                    if let Some(legacy) = legacy {
                        writeln!(
                            io::stderr().lock(),
                            "Found legacy binding {}; import it with kitchn house import --registry '{}'.",
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
            yes: _,
            digest,
            json,
        } => {
            let registry = HouseRegistry::new(canonical_root(registry)?)?;
            let start = canonical_root(repository_path)?;
            let import = registry.import_legacy(&start, digest.as_ref())?;
            let accepted = import.status != LegacyImportStatus::Conflict;
            if json {
                return Ok((json_text(&import)?, accepted));
            }
            let source = import.source.display();
            // Everything that would be stored, so approval covers all of it.
            let binding = format!(
                "{} -> house {}\n  workflows: {}\n  additional reviewers: {}\n  additional checks: {}",
                import.binding.repository,
                import.binding.house,
                joined(&import.binding.workflows),
                joined(&import.binding.additional_reviewers),
                joined(&import.binding.additional_checks),
            );
            let text = match import.status {
                LegacyImportStatus::WouldCreate => format!(
                    "Would import {source} into the registry as {binding}\nNext: rerun with --yes --digest {} to store exactly this binding.",
                    import.digest
                ),
                LegacyImportStatus::Created => format!(
                    "Imported {source} into the registry as {binding}\nNext: delete {source} yourself when no older Kitchen needs it; Kitchen does not delete repository files."
                ),
                LegacyImportStatus::Unchanged => format!(
                    "The registry already holds {binding}\nNext: delete {source} yourself; Kitchen does not delete repository files."
                ),
                LegacyImportStatus::Conflict => format!(
                    "The registry holds a different binding for {}; nothing imported.\nNext: compare it with {source} and change the registry binding with kitchn house setup.",
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
/// What doctor reads besides the registry.
struct Observed {
    evidence: Option<PathBuf>,
    store: Option<PathBuf>,
}

fn diagnose(
    registry: &HouseRegistry,
    config: &RepositoryConfig,
    legacy: Option<&std::path::Path>,
    observed: Observed,
    json: bool,
) -> Result<(String, bool), kitchen::Error> {
    let mut evidence: Option<DoctorEvidence> =
        observed.evidence.as_deref().map(decode).transpose()?;
    if let Some(store) = observed.store {
        let capacity =
            HouseStore::open(store, config.house.clone(), StoreOptions::default())?.capacity()?;
        evidence
            .get_or_insert_with(|| {
                DoctorEvidence::unobserved(config.house.clone(), config.repository.clone())
            })
            .store_capacity = Some(capacity);
    }
    let mut report = doctor(registry, config, evidence.as_ref())?;
    match runtime_config(registry, &config.house) {
        Ok(Some(runtime)) => {
            if let Some(orca) = runtime.orca {
                let status = probe_coordinator(
                    &SystemRunner::new(orca.executable),
                    &orca.run,
                    &orca.coordinator,
                    DEFAULT_CALL_TIMEOUT,
                );
                if let Err(error) = status {
                    let stale = matches!(&error, OrcaError::Refused { code, .. } if code == "terminal_handle_stale");
                    report.findings.push(DoctorFinding {
                        code: DoctorCode::Coordinator,
                        message: if stale { "The stored Orca coordinator terminal handle is stale.".into() } else { format!("The stored Orca coordinator terminal could not be checked: {error}") },
                        next_step: if stale { "Create a live terminal with `orca terminal create --focus`, bind it with `orca orchestration run-use --id <run> --from <new-terminal>`, then run `kitchn tick configure --registry <registry> --house <house> --orca-coordinator <new-terminal>` and rerun doctor.".into() } else { "Check the Orca runtime and stored Run, then rerun doctor. Do not infer that the mailbox is empty.".into() },
                    });
                }
            }
        }
        Ok(None) => {}
        Err(error) => report.findings.push(DoctorFinding {
            code: DoctorCode::Coordinator,
            message: format!("The house's stored runtime configuration could not be read: {error}"),
            next_step: "Inspect and remove the invalid runtime configuration, then store it again with `kitchn tick configure` and rerun doctor. Do not infer that the mailbox is empty.".into(),
        }),
    }
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
/// Comma-separated values in order, or `none`.
fn joined(values: &BTreeSet<impl std::fmt::Display>) -> String {
    if values.is_empty() {
        return "none".to_owned();
    }
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
fn parse_workflows(value: &str) -> Result<BTreeSet<Workflow>, HouseError> {
    if value == "none" {
        return Ok(BTreeSet::new());
    }
    value.split(',').map(|name| name.trim().parse()).collect()
}
pub(super) fn prompt(message: &str) -> Result<String, HouseError> {
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
/// One line saying where the house store is and whether this run created it.
pub(super) fn store_text(store: &StoreSetup) -> String {
    match store.outcome {
        StoreOutcome::Created => format!("Created the house store at {}.", store.path.display()),
        StoreOutcome::Existing => format!("Kept the house store at {}.", store.path.display()),
    }
}
/// `--store` when given, else the house's store in `--registry`. Clap
/// requires one of them; neither is invalid input.
pub(super) fn store_or_default(
    store: Option<PathBuf>,
    registry: Option<&Path>,
    house: &HouseId,
) -> Result<PathBuf, HouseError> {
    match (store, registry) {
        (Some(store), _) => Ok(store),
        (None, Some(registry)) => {
            HouseRegistry::new(canonical_root(registry.to_path_buf())?)?.store_path(house)
        }
        (None, None) => Err(HouseError::InvalidInput),
    }
}
pub(super) fn canonical_root(path: PathBuf) -> Result<PathBuf, HouseError> {
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
