//! `kitchen cleanup`: preview dishwasher decisions from a captured inventory
//! and record a person's approval of what the preview showed.
//!
//! Both subcommands read the house store, an inventory snapshot exported from
//! the backend, and the listed worktrees through bounded read-only Git calls.
//! `preview` writes nothing. `approve` records one approval marker per named
//! digest, as an interactive claimant, so a scheduled run cannot approve for
//! itself; an interactive session that releases through the library consents
//! per release instead and needs no marker. Neither subcommand releases
//! anything: this command has no path to a backend effect. The command cannot
//! tell a person from a script, so scheduled jobs must not run `approve`.

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf};

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
        ApprovalOutcome, ApprovalResult, Decision, DiskUsage, GitLimits, InspectionTrigger,
        Inspector, OwnerState, Ownership, Preview, RemoteName, Step, WorktreeEvidence, approve,
        inspect,
    },
};
use serde::Deserialize;

#[derive(Args)]
pub struct CleanupArgs {
    #[command(subcommand)]
    command: CleanupCommand,
}

/// Where the inventory comes from and which house it belongs to.
#[derive(Args)]
struct Source {
    /// The house's initialized state store.
    #[arg(long)]
    store: PathBuf,
    #[arg(long)]
    house: HouseId,
    /// Inventory snapshot exported from the backend (JSON). Trusted input:
    /// Git reads each listed worktree path.
    #[arg(long)]
    inventory: PathBuf,
    /// A forge remote whose remote-tracking refs prove a commit is pushed.
    /// Repeat for several. A commit no such ref contains is unpushed, so a
    /// worktree holding it is kept.
    #[arg(long = "remote", default_value = "origin")]
    remotes: Vec<RemoteName>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum CleanupCommand {
    /// Explain what the dishwasher would release and why everything else is kept.
    /// Writes nothing; each step it would take shows the digest to approve.
    Preview {
        #[command(flatten)]
        source: Source,
        #[arg(long, value_enum, default_value_t = TriggerArg::Manual)]
        trigger: TriggerArg,
    },
    /// Record your approval of previewed steps, by the digests the preview
    /// showed. A scheduled run acts only on steps a person approved this way,
    /// and only while the evidence still matches. Releases nothing. Run it
    /// yourself: the command cannot tell a person from a script.
    Approve {
        #[command(flatten)]
        source: Source,
        /// The person approving.
        #[arg(long)]
        holder: HolderId,
        /// An evidence digest from the preview, such as `sha256:…`. Repeat for
        /// each step.
        #[arg(long = "digest", required = true)]
        digests: Vec<ExternalRef>,
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
        CleanupCommand::Preview { source, trigger } => {
            let json = source.json;
            let opened = open(source)?;
            let git = GitLimits::default();
            let preview = inspect(&inspector(&opened, &git), trigger.into(), SystemClock.now())?;
            let output = if json {
                String::from_utf8(encode(&preview)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                render(&preview)
            };
            Ok((output, true))
        }
        CleanupCommand::Approve {
            source,
            holder,
            digests,
        } => {
            let json = source.json;
            let opened = open(source)?;
            let git = GitLimits::default();
            let results = approve(
                &inspector(&opened, &git),
                &Claimant::interactive(holder),
                &digests,
                &SystemClock,
            )?;
            let all_current = results
                .iter()
                .all(|result| matches!(result.outcome, ApprovalOutcome::Approved { .. }));
            let output = if json {
                String::from_utf8(encode(&results)?).map_err(|_| HouseError::InvalidInput)?
            } else {
                render_approvals(&results)
            };
            Ok((output, all_current))
        }
    }
}

/// What a subcommand reads: the snapshot backend, the house store, and the
/// forge remotes that prove a commit is pushed.
struct Opened {
    backend: SnapshotBackend,
    store: HouseStore,
    remotes: Vec<RemoteName>,
}

fn open(source: Source) -> Result<Opened, kitchen::Error> {
    let snapshot: Snapshot = decode(&source.inventory)?;
    let backend = SnapshotBackend::new(source.house.clone(), snapshot)?;
    let store = HouseStore::open(source.store, source.house, StoreOptions::default())?;
    Ok(Opened {
        backend,
        store,
        remotes: source.remotes,
    })
}

fn inspector<'a>(opened: &'a Opened, git: &'a GitLimits) -> Inspector<'a> {
    Inspector {
        store: &opened.store,
        backend: &opened.backend,
        worktrees: &opened.backend.paths,
        merged_heads: &opened.backend.merged,
        git,
        remotes: &opened.remotes,
    }
}

fn render_approvals(results: &[ApprovalResult]) -> String {
    let approved = results
        .iter()
        .filter(|result| matches!(result.outcome, ApprovalOutcome::Approved { .. }))
        .count();
    let mut text = format!(
        "Recorded {approved} of {} approvals. Nothing was released.",
        results.len()
    );
    for result in results {
        match &result.outcome {
            ApprovalOutcome::Approved { resource, step } => {
                let _ = write!(
                    text,
                    "\napproved {} {} {}",
                    step_name(*step),
                    kind_name(resource.kind),
                    resource.handle,
                );
            }
            ApprovalOutcome::NotCurrent => {
                let _ = write!(
                    text,
                    "\nnot approved: {} matches no eligible step now; preview again",
                    result.observation
                );
            }
        }
    }
    text
}

const fn step_name(step: Step) -> &'static str {
    match step {
        Step::Release => "release",
        Step::BuildOutput => "build output of",
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
        if entry.eligible() {
            let _ = write!(text, "\n  approve with --digest {}", entry.observation);
        }
        if let Some(WorktreeEvidence::Read {
            state,
            ignored_files,
            ..
        }) = &entry.worktree
        {
            if let Some(operation) = state.operation {
                let _ = write!(text, "\n  {operation} in progress, kept");
            }
            if !ignored_files.is_empty() {
                let _ = write!(text, "\n  ignored, kept: {}", ignored_files.join(", "));
            }
            if state.hidden_tracked > 0 {
                let _ = write!(
                    text,
                    "\n  {} tracked files hide edits (assume-unchanged or skip-worktree)",
                    state.hidden_tracked
                );
            }
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
            if build.decision == Decision::Release {
                let _ = write!(text, "\n  approve with --digest {}", build.observation);
            }
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
