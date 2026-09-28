//! `kitchen cleanup`: preview dishwasher decisions from a captured inventory.
//!
//! The preview reads the house store, an inventory snapshot exported from the
//! backend, and the listed worktrees through bounded read-only Git calls. It
//! records one preview marker per eligible resource and releases nothing:
//! this command has no path to a backend effect.

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf, time::Duration};

use clap::{Args, Subcommand, ValueEnum};
use kitchen::{
    BackendId, HolderId, HouseId,
    adoption::{decode, encode},
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Claimant, Clock,
        CommitId, EffectExecutor, EffectFailure, EffectRequest, ExternalRef, Liveness, Lookup,
        MAX_INVENTORY_RESOURCES, NotAppliedReason, Receipt, ResourceKind, ResourceObservation,
        ResourceRef, SystemClock, WorkerBackend, WorkerOutcome, WorkerState,
    },
    house::HouseError,
    state::{HouseStore, StoreOptions},
    workflows::cleanup::{
        Decision, DiskUsage, GitLimits, InspectionTrigger, Inspector, OwnerState, Ownership,
        Preview, preview,
    },
};
use serde::Deserialize;

#[derive(Args)]
pub struct CleanupArgs {
    #[command(subcommand)]
    command: CleanupCommand,
}

#[derive(Subcommand)]
enum CleanupCommand {
    /// Explain what the dishwasher would release and why everything else is kept.
    /// Records the preview in the house store; releases nothing.
    Preview {
        /// The house's initialized state store.
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        house: HouseId,
        /// Inventory snapshot exported from the backend (JSON). Trusted input:
        /// Git reads each listed worktree path.
        #[arg(long)]
        inventory: PathBuf,
        /// Who is recording the preview.
        #[arg(long)]
        holder: HolderId,
        #[arg(long, value_enum, default_value_t = TriggerArg::Manual)]
        trigger: TriggerArg,
        /// Seconds after which an unchanged preview is recorded again.
        #[arg(long, default_value_t = 86_400)]
        max_age_secs: u64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum TriggerArg {
    Schedule,
    DiskPressure,
    Manual,
}

impl From<TriggerArg> for InspectionTrigger {
    fn from(trigger: TriggerArg) -> Self {
        match trigger {
            TriggerArg::Schedule => Self::Schedule,
            TriggerArg::DiskPressure => Self::DiskPressure,
            TriggerArg::Manual => Self::Manual,
        }
    }
}

pub fn run(args: CleanupArgs) -> Result<(String, bool), kitchen::Error> {
    match args.command {
        CleanupCommand::Preview {
            store,
            house,
            inventory,
            holder,
            trigger,
            max_age_secs,
            json,
        } => {
            let snapshot: Snapshot = decode(&inventory)?;
            let backend = SnapshotBackend::new(house.clone(), snapshot)?;
            let store = HouseStore::open(store, house, StoreOptions::default())?;
            let git = GitLimits::default();
            let inspector = Inspector {
                store: &store,
                backend: &backend,
                worktrees: &backend.paths,
                merged_heads: &backend.merged,
                git: &git,
            };
            let preview = preview(
                &inspector,
                trigger.into(),
                &Claimant::interactive(holder),
                Duration::from_secs(max_age_secs),
                SystemClock.now(),
            )?;
            let output = if json {
                String::from_utf8(encode(&preview)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                render(&preview)
            };
            Ok((output, true))
        }
    }
}

fn render(preview: &Preview) -> String {
    let eligible = preview
        .entries
        .iter()
        .filter(|entry| entry.eligible())
        .count();
    let mut text = format!(
        "Dishwasher preview for house {} on backend {} ({}): {eligible} to release, {} retained. Nothing was released.",
        preview.house,
        preview.backend,
        trigger_name(preview.trigger),
        preview.entries.len().saturating_sub(eligible),
    );
    for entry in &preview.entries {
        let owner = match &entry.ownership {
            Ownership::Unknown => "owner unknown".to_owned(),
            Ownership::Ambiguous { tasks } => format!("owner ambiguous ({} tasks)", tasks.len()),
            Ownership::Task(owner) => {
                let state = match owner.state {
                    OwnerState::Open => "open".to_owned(),
                    OwnerState::Claimed => "claimed".to_owned(),
                    OwnerState::Settled { settlement, .. } => format!("settled {settlement}"),
                };
                format!("task {} {} {state}", owner.task, owner.attempt)
            }
        };
        let decision = match &entry.decision {
            Decision::Release => "release".to_owned(),
            Decision::Retain { reasons } => {
                let names: Vec<&str> = reasons.iter().map(|reason| reason.as_str()).collect();
                format!("retain: {}", names.join(", "))
            }
        };
        let _ = write!(
            text,
            "\n{} {} [{owner}] {decision}",
            kind_name(entry.resource.kind),
            entry.resource.handle,
        );
        if let Some(usage) = entry.usage {
            let _ = write!(text, " ({})", size(usage));
        }
        if let Some(build) = &entry.build_output {
            let names: Vec<&str> = build
                .directories
                .iter()
                .map(|directory| directory.name.as_str())
                .collect();
            let verdict = match &build.decision {
                Decision::Release => "remove".to_owned(),
                Decision::Retain { reasons } => format!(
                    "keep: {}",
                    reasons
                        .iter()
                        .map(|reason| reason.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let _ = write!(
                text,
                "\n  build output {} ({}) {verdict}",
                names.join(", "),
                size(build.usage()),
            );
        }
    }
    for suggestion in &preview.suggestions {
        let _ = write!(
            text,
            "\nNot run (outside Kitchen): {} reclaims {}; {}.",
            suggestion.command, suggestion.reclaims, suggestion.caution
        );
    }
    text
}

fn size(usage: DiskUsage) -> String {
    let bound = if usage.complete { "" } else { "at least " };
    format!("{bound}{} bytes", usage.bytes)
}

const fn trigger_name(trigger: InspectionTrigger) -> &'static str {
    match trigger {
        InspectionTrigger::Schedule => "schedule",
        InspectionTrigger::DiskPressure => "disk pressure",
        InspectionTrigger::Manual => "manual",
    }
}

const fn kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Worker => "worker",
        ResourceKind::Worktree => "worktree",
        ResourceKind::Terminal => "terminal",
        ResourceKind::Branch => "branch",
        ResourceKind::Schedule => "schedule",
        _ => "resource",
    }
}

/// An inventory exported from a backend, with local worktree paths and
/// forge facts attached.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    backend: BackendId,
    resources: Vec<SnapshotResource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotResource {
    kind: ResourceKind,
    handle: ExternalRef,
    #[serde(default)]
    owner: Option<ExternalRef>,
    liveness: LivenessArg,
    #[serde(default)]
    worker: Option<WorkerArg>,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    merged_head: Option<CommitId>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LivenessArg {
    Live,
    Exited,
    Unverifiable,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WorkerArg {
    Starting,
    Ready,
    AwaitingReply,
    UserTakeover,
    SettledSucceeded,
    SettledFailed,
    SettledCancelled,
    Missing,
    Unknown,
}

impl From<WorkerArg> for WorkerState {
    fn from(state: WorkerArg) -> Self {
        match state {
            WorkerArg::Starting => Self::Starting,
            WorkerArg::Ready => Self::Ready,
            WorkerArg::AwaitingReply => Self::AwaitingReply,
            WorkerArg::UserTakeover => Self::UserTakeover,
            WorkerArg::SettledSucceeded => Self::Settled(WorkerOutcome::Succeeded),
            WorkerArg::SettledFailed => Self::Settled(WorkerOutcome::Failed),
            WorkerArg::SettledCancelled => Self::Settled(WorkerOutcome::Cancelled),
            WorkerArg::Missing => Self::Missing,
            WorkerArg::Unknown => Self::Unknown,
        }
    }
}

/// A read-only backend over a snapshot. It declares only inventory and
/// worker status, so it refuses every effect.
struct SnapshotBackend {
    descriptor: BackendDescriptor,
    observations: Vec<ResourceObservation>,
    workers: BTreeMap<ResourceRef, WorkerState>,
    paths: BTreeMap<ResourceRef, PathBuf>,
    merged: BTreeMap<ResourceRef, CommitId>,
}

impl SnapshotBackend {
    fn new(house: HouseId, snapshot: Snapshot) -> Result<Self, HouseError> {
        if snapshot.resources.len() > MAX_INVENTORY_RESOURCES {
            return Err(HouseError::InvalidInput);
        }
        let mut backend = Self {
            descriptor: BackendDescriptor {
                backend: snapshot.backend.clone(),
                house,
                capabilities: CapabilitySet::supporting([
                    Capability::ResourceInventory,
                    Capability::WorkerStatusAndOutcome,
                ]),
            },
            observations: Vec::with_capacity(snapshot.resources.len()),
            workers: BTreeMap::new(),
            paths: BTreeMap::new(),
            merged: BTreeMap::new(),
        };
        for item in snapshot.resources {
            let resource = ResourceRef {
                kind: item.kind,
                backend: snapshot.backend.clone(),
                handle: item.handle,
            };
            // The worker's own state is required to judge it; absent means unknown.
            if item.kind == ResourceKind::Worker {
                backend.workers.insert(
                    resource.clone(),
                    item.worker.map_or(WorkerState::Unknown, WorkerState::from),
                );
            }
            if let Some(path) = item.path {
                if !path.is_absolute() {
                    return Err(HouseError::InvalidInput);
                }
                backend.paths.insert(resource.clone(), path);
            }
            if let Some(head) = item.merged_head {
                backend.merged.insert(resource.clone(), head);
            }
            backend.observations.push(ResourceObservation {
                resource,
                owner: item.owner,
                liveness: match item.liveness {
                    LivenessArg::Live => Liveness::Live,
                    LivenessArg::Exited => Liveness::Exited,
                    LivenessArg::Unverifiable => Liveness::Unverifiable,
                },
            });
        }
        Ok(backend)
    }
}

impl EffectExecutor for SnapshotBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(
            request.effect().required_capability(),
        )))
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        Err(BackendUnavailable::Unsupported(
            request.effect().kind().lookup_capability(),
        ))
    }
}

impl WorkerBackend for SnapshotBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        Ok(self
            .workers
            .get(worker)
            .copied()
            .unwrap_or(WorkerState::Missing))
    }

    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        Ok(self.observations.clone())
    }
}
