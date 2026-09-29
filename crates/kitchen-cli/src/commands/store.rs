//! `kitchen store`: how full the house store is, and its retention pass.
//!
//! Retention previews by default. Issue and pull-request state comes from
//! the forge through the house's forge binding, never from a file; items the
//! forge did not answer completely keep everything that depends on them.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, Subcommand};
use kitchen::{
    HouseId,
    adoption::{HouseRegistry, encode},
    contracts::{Clock, SystemClock},
    house::{HouseError, credential_path, forge_binding},
    integrations::github::{CredentialFile, GhCli, GitHubClient, ReadLimits},
    state::{
        HouseStore, Inventory, RetentionPolicy, RetentionReport, StoreCapacity, StoreOptions,
        TableUsage,
    },
};
use serde::Serialize;

/// Forge lookups one retention pass makes at most.
const MAX_LOOKUPS: usize = 1000;

#[derive(Args)]
pub struct StoreArgs {
    #[command(subcommand)]
    command: StoreCommand,
}

#[derive(Subcommand)]
enum StoreCommand {
    /// Report how full the house store's shared tables are. Reads only.
    Capacity {
        #[arg(long)]
        house: HouseId,
        /// Absolute path of the house's initialized state store.
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Preview the markers and settled tasks no workflow still needs; with
    /// --apply, remove them.
    Retain(RetainArgs),
}

#[derive(Args)]
struct RetainArgs {
    /// The house registry holding the forge binding --gh reads through.
    #[arg(long)]
    registry: Option<PathBuf>,
    #[arg(long)]
    house: HouseId,
    /// Absolute path of the house's initialized state store.
    #[arg(long)]
    store: PathBuf,
    /// Absolute path of the GitHub CLI, used with the house's forge binding
    /// to read which issues and pull requests are closed. Without it nothing
    /// that depends on an issue or pull request is removed.
    #[arg(long, requires = "registry")]
    gh: Option<PathBuf>,
    /// Days a settled task stays, at least 31.
    #[arg(long, default_value_t = 31)]
    window_days: u16,
    /// Issues and pull requests to look up, at most 1000; the rest keep
    /// their records until a later pass.
    #[arg(long, default_value_t = 200)]
    max_lookups: usize,
    /// Remove what the preview lists. Without it nothing is written.
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    json: bool,
}

pub fn run(args: StoreArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        StoreCommand::Capacity { house, store, json } => {
            let capacity = open(&store, house)?.capacity()?;
            let healthy = !capacity.near_limit();
            let output = if json {
                json_text(&capacity)?
            } else {
                capacity_text(&capacity)
            };
            Ok((output, healthy))
        }
        StoreCommand::Retain(args) => retain(args),
    }
}

/// What a retention pass reports.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RetainOutput {
    /// Issues and pull requests the pass depends on.
    subjects: usize,
    /// How many of them the forge answered completely.
    observed: usize,
    retention: RetentionReport,
    capacity: StoreCapacity,
}

fn retain(args: RetainArgs) -> Result<(String, bool), kitchen::Error> {
    if args.max_lookups > MAX_LOOKUPS {
        return Err(HouseError::InvalidInput.into());
    }
    let policy = RetentionPolicy::new(Duration::from_secs(
        u64::from(args.window_days) * 24 * 60 * 60,
    ))?;
    if args.gh.as_ref().is_some_and(|gh| !gh.is_absolute()) {
        return Err(HouseError::InvalidInput.into());
    }
    let store = open(&args.store, args.house.clone())?;
    let subjects = store.retention_subjects()?;
    let mut inventory = Inventory::new();
    let observed = match &args.gh {
        Some(gh) => {
            let registry = args.registry.ok_or(HouseError::InvalidInput)?;
            let registry = HouseRegistry::new(super::house::canonical_root(registry)?)?;
            let config = registry.load(&args.house)?;
            let binding = forge_binding(&registry, &args.house)?;
            let scope = binding.scope(&config)?;
            let credential = CredentialFile::new(
                binding.credential_ref(),
                credential_path(&registry, &binding)?,
            )?;
            let client = GitHubClient::new(
                scope,
                GhCli::new(gh.clone(), credential)?,
                ReadLimits::default(),
            );
            inventory.observe_forge(&client, &args.house, &subjects.items, args.max_lookups)
        }
        None => 0,
    };
    let now = SystemClock.now();
    let retention = if args.apply {
        store.retain(&policy, &inventory, now)?
    } else {
        store.preview_retention(&policy, &inventory, now)?
    };
    let output = RetainOutput {
        subjects: subjects.items.len(),
        observed,
        retention,
        capacity: store.capacity()?,
    };
    let text = if args.json {
        json_text(&output)?
    } else {
        retain_text(&output, args.apply)
    };
    Ok((text, true))
}

fn open(store: &Path, house: HouseId) -> Result<HouseStore, kitchen::Error> {
    if !store.is_absolute() {
        return Err(HouseError::InvalidInput.into());
    }
    HouseStore::open(store, house, StoreOptions::default())
}

fn usage_line(text: &mut String, table: &str, usage: TableUsage) {
    let _ = writeln!(
        text,
        "{table}: {} of {}{}",
        usage.used,
        usage.limit,
        if usage.near_limit() {
            " (near the limit)"
        } else {
            ""
        }
    );
}

fn capacity_text(capacity: &StoreCapacity) -> String {
    let mut text = String::new();
    usage_line(&mut text, "Tasks", capacity.tasks);
    let _ = writeln!(text, "  settled: {}", capacity.settled_tasks);
    usage_line(&mut text, "Workflow markers", capacity.markers);
    for (workflow, count) in &capacity.markers_by_workflow {
        let _ = writeln!(text, "  {workflow}: {count}");
    }
    usage_line(&mut text, "Consumer leases", capacity.consumers);
    text
}

fn retain_text(output: &RetainOutput, apply: bool) -> String {
    let retention = &output.retention;
    let mut text = format!(
        "{} {} marker(s) and {} settled task(s). The forge answered for {} of {} issue(s) and pull request(s); the rest keep their records.\n",
        if apply { "Removed" } else { "Would remove" },
        retention.markers.len(),
        retention.tasks.len(),
        output.observed,
        output.subjects,
    );
    for marker in &retention.markers {
        let _ = writeln!(
            text,
            "  marker {} {:?}: {:?}",
            marker.key.workflow, marker.key.item, marker.reason
        );
    }
    for task in &retention.tasks {
        let _ = writeln!(text, "  task {}: {:?}", task.task, task.reason);
    }
    if !apply {
        text.push_str("Preview only; rerun with --apply to remove them.\n");
    }
    text.push('\n');
    text.push_str(&capacity_text(&output.capacity));
    text
}

fn json_text(value: &impl Serialize) -> Result<String, kitchen::Error> {
    Ok(String::from_utf8(encode(value)?).map_err(|_| HouseError::InvalidInput)?)
}
