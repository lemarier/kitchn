//! The dishwasher: ownership-aware resource inspection and cleanup.
//!
//! The dishwasher owns reclamation of worker, terminal, and worktree
//! resources. It never decides from age, silence, or disk pressure: those
//! only start an inspection. A resource is eligible for release only with
//! positive evidence that its ownership ended and nothing would be lost:
//!
//! - exactly one Kitchen task created it through an applied effect, and the
//!   backend's owner record names that same effect. A backend that records no
//!   owner leaves the resource unconfirmed and it is retained; that is fixed
//!   policy, not a house option, because Kitchen's own record alone is not
//!   enough to delete on;
//! - that task is settled with every effect resolved, and no other unsettled
//!   task was given the resource;
//! - the backend reports it exited, and every worker of the owning task is
//!   settled; a person's takeover of any of them retains everything the task
//!   owns;
//! - a worktree is a linked, unlocked worktree with a branch checked out and
//!   no rebase, merge, cherry-pick, revert, `am`, or bisect unfinished (its
//!   progress lives only in the worktree), no tracked or untracked changes, no
//!   tracked file whose edits Git is told to hide (assume-unchanged,
//!   skip-worktree), and no ignored file except proven build output, because
//!   deleting a worktree deletes its local configuration, notes, and ignored
//!   nested repositories too; its `HEAD` is contained in a remote-tracking ref
//!   of a configured forge remote (a local mirror or a person's backup remote
//!   does not count) or equals the head of the pull request that merged it
//!   (for squash merges).
//!
//! The pushed check reads remote-tracking refs as the last fetch left them;
//! Kitchen never fetches, so a branch deleted on the forge since then still
//! looks pushed. It examines `HEAD` alone, which is enough only while `HEAD` is
//! on a branch and nothing is unfinished: an attached `HEAD` is its branch's
//! tip, and the branch outlives the worktree, provided the backend's release
//! of a worktree removes the checkout and never the branch. That is a
//! requirement on backends; the dishwasher itself never removes a branch. A
//! detached `HEAD` has no branch behind it, and commits made on it and then
//! left are remembered only by the worktree's own reflog, which goes with the
//! worktree, so a detached worktree is retained. During an unfinished
//! operation `HEAD` is detached or sits on a pushed base while the branch tip
//! holds unpushed commits, so the operation retains the worktree too.
//!
//! Anything else is retained with every reason that applies. Unknown and
//! legacy resources are retained. Branches and schedules are never removed.
//!
//! [`inspect`] only reads; it records nothing and approves nothing. Acting
//! needs an approval that the automation which inspected cannot give itself.
//!
//! - A scheduled run needs a person's stored approval. [`approve`] records,
//!   for one previewed step, a marker keyed by the resource and the digest of
//!   the evidence it was judged on, and refuses any claimant that is not
//!   [`Trigger::Interactive`]. [`apply`] and [`reclaim_build_output`] act only
//!   where such a marker names the unchanged digest and is not older than the
//!   allowed age, so the first run against an existing backlog is
//!   preview-only and a scheduled run never approves its own preview.
//!   Neither run writes a marker, so a full marker table cannot stop them, and
//!   recovery never depends on one.
//! - An interactive [`apply`] needs no stored approval. A person present
//!   gives one consent per release ([`ConsentSource`]), and each consent
//!   ([`ReleaseConsent`]) carries the digest of the preview they were shown.
//!   `apply` compares it with the digest of the evidence it is about to act
//!   on and refuses, before writing anything, a consent for other evidence
//!   ([`ReleaseOutcome::ConsentMismatch`]) or naming none
//!   ([`ReleaseOutcome::ConsentUnbound`]). Evidence that changed since the
//!   person looked has a different digest and needs a new consent.
//!
//! The approval marker is not a grant: no [`Permission`] names it, so nothing
//! in a house's grants says who may approve. A house `approve-cleanup` grant
//! may come later; until then the marker, recorded by an interactive claimant,
//! is the durable form. The same holds for consent. The library cannot tell a
//! person from a script: an approval is whatever an interactive claimant
//! records, and a consent is whatever a [`ConsentSource`] returns, the same
//! trust boundary as a person's consent for any other effect. Scheduled
//! workflows must never build an interactive claimant or a consent source.
//!
//! Each release runs as its own dishwasher task given exactly that resource,
//! through the durable effect path ([`crate::state::run_effect`]) and the
//! explicit release grant, and is revalidated immediately before the effect.
//! An interrupted release is reconciled by the next run before anything else.
//!
//! Build output is the one thing reclaimed without a grant: a `CACHEDIR.TAG`
//! directory that Git ignores and tracks nothing in, at the top of a
//! Kitchen-owned worktree whose workers have all settled. It is regenerable
//! and not work, so [`reclaim_build_output`] needs only the approval of the
//! same evidence; the worktree itself stays until it passes every check
//! above. Caches outside Kitchen-owned resources are never touched: under
//! disk pressure the preview lists commands a person may run instead.
//! Every step reports the space it measured before acting; that is an upper
//! bound on what it frees, not a measurement afterwards.
//!
//! # Backend requirements
//!
//! The pushed check examines `HEAD` only, so it is sound only if a backend's
//! release of a worktree removes the checkout and never deletes the branch
//! that was checked out in it: an attached `HEAD` is protected because its
//! branch survives the release. An adapter must not delete that branch, and
//! must document that it does not. The `ResourceRelease` capability calls a
//! release only "safety-retaining" and does not name the branch, so this is
//! not yet part of the contract. The dishwasher itself never removes a branch.

mod build;
mod git;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    io,
    num::NonZeroU32,
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

pub use build::{
    BuildDirectory, CACHEDIR_SIGNATURE, DiskUsage, MAX_MEASURED_ENTRIES, MAX_TOP_LEVEL_ENTRIES,
    disk_usage,
};
pub use git::{
    GitLimits, GitOperation, GitReadError, MAX_IGNORED_PATHS, RemoteName, WorktreeState,
    inspect_worktree,
};

use crate::{
    BackendId, EffectName, Error, ErrorClass, HouseId, Result, TaskId, WorkflowId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, BackendUnavailable, Capability,
        CapabilityRequirements, Claimant, Clock, CommitId, Consent, ContractError, Effect,
        EffectExecutor, EvidenceRevision, ExecutorKind, ExternalRef, FailureClass, Grant,
        HouseGrants, IdempotencyKey, LeaseTtl, Liveness, NotAppliedReason, Operation, Permission,
        Provenance, ResourceKind, ResourceObservation, ResourceRef, RetryPolicy, Role, Settlement,
        TaskAuthority, TaskSpec, Timestamp, Trigger, WorkerBackend, WorkerOutcome, WorkerState,
    },
    state::{
        EffectPlan, EffectRecord, EffectState, HouseStore, MarkerFact, MarkerKey, MarkerSchema,
        MarkerSubject, StateError, TaskRecord, TaskState, WorkItem, reconcile, run_effect,
    },
};

/// The workflow id the dishwasher records markers under.
pub const WORKFLOW: &str = "dishwasher";
/// The marker schema of a person's approval of one previewed step.
pub const APPROVAL_SCHEMA: &str = "cleanup.approval";
/// Prefix of the tasks the dishwasher creates for releases.
pub const TASK_PREFIX: &str = "dishwasher-";
/// Attempts a release task may use, including ones spent on recovery.
const RELEASE_ATTEMPTS: u32 = 4;
/// Longest a release task may keep retrying after its first attempt.
const RELEASE_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);

/// Failures specific to the dishwasher. Store and contract failures keep
/// their own types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CleanupError {
    /// A read-only backend call failed; nothing may be inferred from it.
    #[error("backend read failed: {0}")]
    Backend(#[from] BackendUnavailable),
    /// The inventory listed one resource more than once.
    #[error("backend inventory lists a resource more than once")]
    DuplicateResource,
    /// The release grant is not a release permission on this backend.
    #[error("the release grant must permit release-resource on the inspected backend")]
    GrantMismatch,
    /// Evidence could not be encoded for its digest or marker.
    #[error("cleanup evidence could not be encoded")]
    Encoding,
    /// Only a person present can approve a previewed cleanup step.
    #[error("a cleanup approval must be recorded by an interactive claimant")]
    ApprovalNeedsPerson,
    /// A remote name is empty, too long, or holds characters that are not
    /// plain letters, digits, `-`, `_`, or `.`.
    #[error("a remote name must be 1 to 64 letters, digits, '-', '_' or '.'")]
    InvalidRemote,
}

impl CleanupError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::GrantMismatch | Self::InvalidRemote => ErrorClass::InvalidInput,
            Self::ApprovalNeedsPerson => ErrorClass::Refused,
            Self::Backend(_) | Self::DuplicateResource | Self::Encoding => ErrorClass::Execution,
        }
    }
}

/// What started an inspection. Recorded with the preview; it never changes
/// eligibility, so disk pressure can prompt an inspection but not a deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InspectionTrigger {
    /// A scheduled run.
    Schedule,
    /// Low disk space on the host.
    DiskPressure,
    /// A person asked for it.
    Manual,
}

/// Maps a worktree resource to its local checkout. Adapters that know where
/// the backend placed worktrees implement it.
pub trait WorktreeLocator {
    /// The worktree's top-level directory, if known.
    fn locate(&self, worktree: &ResourceRef) -> Option<PathBuf>;
}

impl WorktreeLocator for BTreeMap<ResourceRef, PathBuf> {
    fn locate(&self, worktree: &ResourceRef) -> Option<PathBuf> {
        self.get(worktree).cloned()
    }
}

/// Everything an inspection reads. All reads are bounded and read-only.
#[derive(Clone, Copy)]
pub struct Inspector<'a> {
    /// The house's durable task store: the source of task ownership.
    pub store: &'a HouseStore,
    /// The backend whose resources are inspected. Its release of a worktree
    /// must remove the checkout and never delete the branch (see the module's
    /// backend requirements): the pushed check relies on the branch surviving.
    pub backend: &'a dyn WorkerBackend,
    /// Where worktrees are checked out.
    pub worktrees: &'a dyn WorktreeLocator,
    /// For worktrees whose pull request merged: the head commit the forge
    /// merged. Equality with the local head preserves squash-merged work.
    pub merged_heads: &'a BTreeMap<ResourceRef, CommitId>,
    /// Bounds for each Git call.
    pub git: &'a GitLimits,
    /// The forge remotes: a commit counts as pushed only when a
    /// remote-tracking ref of one of them contains it. Empty means nothing is
    /// pushed, so every unmerged commit keeps its worktree.
    pub remotes: &'a [RemoteName],
}

/// A reason a resource is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Exclusion {
    /// The resource belongs to another backend namespace.
    ForeignBackend,
    /// The dishwasher never removes this kind (branches, schedules).
    NotReclaimable,
    /// No Kitchen task created it: a legacy or foreign resource.
    UnknownOwner,
    /// More than one creating effect names it: a reused identifier.
    AmbiguousOwner,
    /// The backend's owner record names a different effect.
    BackendOwnerMismatch,
    /// The backend records no owner, so nothing but Kitchen's own record ties
    /// the resource to the task.
    BackendOwnerUnrecorded,
    /// The owning task is not settled.
    OwnerActive,
    /// The owning task has an effect whose outcome is unknown or waived.
    UnresolvedEffects,
    /// Another unsettled task was given this resource.
    SharedWithTask,
    /// The backend reports it in use.
    InUse,
    /// The backend cannot tell whether it is in use.
    LivenessUnverifiable,
    /// A person took over a worker of the owning task.
    UserTakeover,
    /// This worker has not reported a settled outcome.
    WorkerNotSettled,
    /// Another resource of the owning task is in use or unsettled.
    SiblingInUse,
    /// The worktree's checkout location is unknown.
    WorktreeUnlocated,
    /// The worktree could not be inspected.
    WorktreeUnreadable,
    /// The path is a repository's main checkout, not a linked worktree.
    MainCheckout,
    /// The worktree is locked.
    WorktreeLocked,
    /// `HEAD` is detached, so no branch vouches for its commits.
    DetachedHead,
    /// A rebase, merge, cherry-pick, revert, `am`, or bisect is unfinished.
    OperationInProgress,
    /// Tracked files have changes.
    TrackedChanges,
    /// Untracked files exist.
    UntrackedFiles,
    /// Git-ignored files exist that are not proven build output.
    IgnoredFiles,
    /// Tracked files are marked assume-unchanged or skip-worktree.
    HiddenTrackedFiles,
    /// `HEAD` has commits that are neither pushed nor the merged head.
    UnpreservedCommits,
}

impl Exclusion {
    /// The stable kebab-case name, as serialized.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForeignBackend => "foreign-backend",
            Self::NotReclaimable => "not-reclaimable",
            Self::UnknownOwner => "unknown-owner",
            Self::AmbiguousOwner => "ambiguous-owner",
            Self::BackendOwnerMismatch => "backend-owner-mismatch",
            Self::BackendOwnerUnrecorded => "backend-owner-unrecorded",
            Self::OwnerActive => "owner-active",
            Self::UnresolvedEffects => "unresolved-effects",
            Self::SharedWithTask => "shared-with-task",
            Self::InUse => "in-use",
            Self::LivenessUnverifiable => "liveness-unverifiable",
            Self::UserTakeover => "user-takeover",
            Self::WorkerNotSettled => "worker-not-settled",
            Self::SiblingInUse => "sibling-in-use",
            Self::WorktreeUnlocated => "worktree-unlocated",
            Self::WorktreeUnreadable => "worktree-unreadable",
            Self::MainCheckout => "main-checkout",
            Self::WorktreeLocked => "worktree-locked",
            Self::DetachedHead => "detached-head",
            Self::OperationInProgress => "operation-in-progress",
            Self::TrackedChanges => "tracked-changes",
            Self::UntrackedFiles => "untracked-files",
            Self::IgnoredFiles => "ignored-files",
            Self::HiddenTrackedFiles => "hidden-tracked-files",
            Self::UnpreservedCommits => "unpreserved-commits",
        }
    }
}

impl fmt::Display for Exclusion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Exclusion {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// What the dishwasher would do with a resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Decision {
    /// Release it through the backend.
    Release,
    /// Keep it, for every listed reason.
    Retain {
        /// The reasons, in a stable order.
        reasons: Vec<Exclusion>,
    },
}

/// The owning task's state as seen by the dishwasher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OwnerState {
    /// Unclaimed and unsettled.
    Open,
    /// Claimed, live or expired.
    Claimed,
    /// Settled.
    Settled {
        /// The settlement.
        settlement: Settlement,
        /// When it settled.
        at: Timestamp,
    },
}

/// The task and effect that created a resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOwner {
    /// The owning task.
    pub task: TaskId,
    /// The attempt whose effect created it.
    pub attempt: AttemptNumber,
    /// The idempotency key of the creating effect.
    pub key: IdempotencyKey,
    /// The task's state.
    pub state: OwnerState,
    /// Other unsettled tasks the resource was given to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<TaskId>,
}

/// Who owns a resource, according to the durable store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Ownership {
    /// No applied effect of any task created it.
    Unknown,
    /// Several creating effects name it.
    Ambiguous {
        /// The tasks whose effects name it.
        tasks: Vec<TaskId>,
    },
    /// Exactly one task created it.
    Task(TaskOwner),
}

/// What Git reported about a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WorktreeEvidence {
    /// No location is known.
    Unlocated,
    /// Inspection failed.
    Unreadable {
        /// Why.
        error: GitReadError,
    },
    /// Inspection succeeded.
    #[serde(rename_all = "camelCase")]
    Read {
        /// The state.
        state: WorktreeState,
        /// The merged pull-request head, when the forge reported one.
        #[serde(skip_serializing_if = "Option::is_none")]
        merged_head: Option<CommitId>,
        /// Ignored paths that are not proven build output; each keeps the
        /// worktree.
        ignored_files: Vec<String>,
    },
}

/// Build output found in a worktree and the decision about removing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildOutput {
    /// Digest of the evidence the decision rests on; sizes are excluded.
    pub observation: ExternalRef,
    /// The directories, sorted by name.
    pub directories: Vec<BuildDirectory>,
    /// [`Decision::Release`] when they may be removed.
    pub decision: Decision,
}

impl BuildOutput {
    /// Total measured size.
    #[must_use]
    pub fn usage(&self) -> DiskUsage {
        self.directories.iter().fold(
            DiskUsage {
                bytes: 0,
                complete: true,
            },
            |total, directory| DiskUsage {
                bytes: total.bytes.saturating_add(directory.usage.bytes),
                complete: total.complete && directory.usage.complete,
            },
        )
    }
}

/// A command a person may run to reclaim space Kitchen does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    /// What it reclaims.
    pub reclaims: &'static str,
    /// The command.
    pub command: &'static str,
    /// What to check first.
    pub caution: &'static str,
}

/// Suggestions listed under disk pressure. Kitchen runs none of them.
pub const EXTERNAL_CACHE_SUGGESTIONS: [Suggestion; 3] = [
    Suggestion {
        reclaims: "Cargo registry and Git dependency caches",
        command: "cargo cache --autoclean",
        caution: "needs cargo-cache; affects every project on this host",
    },
    Suggestion {
        reclaims: "nothing itself: shows how large a shared compiler cache is",
        command: "sccache --show-stats",
        caution: "sccache has no clear command; delete its cache directory yourself, and only while nothing is compiling",
    },
    Suggestion {
        reclaims: "build output in checkouts Kitchen does not own",
        command: "cargo clean",
        caution: "run it only inside a checkout you own",
    },
];

/// One inventoried resource with its evidence and decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewEntry {
    /// The resource.
    pub resource: ResourceRef,
    /// Digest of the evidence the decision rests on. A different digest
    /// means different evidence.
    pub observation: ExternalRef,
    /// The backend's liveness report.
    #[serde(serialize_with = "serialize_liveness")]
    pub liveness: Liveness,
    /// The backend's owner record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_owner: Option<ExternalRef>,
    /// The worker's observed state, for workers.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_worker"
    )]
    pub worker: Option<WorkerState>,
    /// Ownership from the durable store.
    pub ownership: Ownership,
    /// Git evidence, for worktrees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeEvidence>,
    /// Regenerable build output in this worktree, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_output: Option<BuildOutput>,
    /// For a worktree eligible for release: its measured size, which the
    /// release frees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<DiskUsage>,
    /// Time since the owning task settled: a signal for review only.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_age"
    )]
    pub settled_for: Option<Duration>,
    /// The decision.
    pub decision: Decision,
}

impl PreviewEntry {
    /// Whether the decision is to release.
    #[must_use]
    pub fn eligible(&self) -> bool {
        self.decision == Decision::Release
    }

    /// Whether this worktree's build output may be removed.
    #[must_use]
    pub fn build_output_eligible(&self) -> bool {
        self.build_output
            .as_ref()
            .is_some_and(|build| build.decision == Decision::Release)
    }

    fn owner_task(&self) -> Option<&TaskId> {
        match &self.ownership {
            Ownership::Task(owner) => Some(&owner.task),
            Ownership::Unknown | Ownership::Ambiguous { .. } => None,
        }
    }
}

/// Whether an inspection found anything to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Precheck {
    /// Nothing is eligible.
    Idle,
    /// At least one resource is eligible.
    Actionable,
}

/// A reviewable inspection of every inventoried resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// The house.
    pub house: HouseId,
    /// The backend namespace inspected.
    pub backend: BackendId,
    /// What started the inspection.
    pub trigger: InspectionTrigger,
    /// When it was observed.
    pub observed_at: Timestamp,
    /// Every inventoried resource, in inventory order.
    pub entries: Vec<PreviewEntry>,
    /// Commands for caches Kitchen does not own; listed under disk pressure.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Suggestion>,
}

impl Preview {
    /// [`Precheck::Idle`] when nothing is eligible, including build output.
    #[must_use]
    pub fn precheck(&self) -> Precheck {
        if self
            .entries
            .iter()
            .any(|entry| entry.eligible() || entry.build_output_eligible())
        {
            Precheck::Actionable
        } else {
            Precheck::Idle
        }
    }

    /// The entry for `resource`.
    #[must_use]
    pub fn entry(&self, resource: &ResourceRef) -> Option<&PreviewEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.resource == resource)
    }
}

/// Which cleanup step a preview covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Release the resource through the backend.
    Release,
    /// Remove the worktree's build output.
    BuildOutput,
}

/// The payload of an approval marker. Who approved is the marker's own
/// recorder; a marker not recorded by an interactive claimant approves nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApprovalFact {
    step: Step,
    owner: TaskId,
    approved_at: Timestamp,
}

/// Inspect every inventoried resource without recording anything.
///
/// # Errors
/// Refuses a backend for another house or without inventory and worker
/// status; returns [`CleanupError::Backend`] when a backend read fails and
/// store errors. A failed read is never reported as an idle inspection.
pub fn inspect(
    inspector: &Inspector<'_>,
    trigger: InspectionTrigger,
    now: Timestamp,
) -> Result<Preview> {
    let entries = evaluate(inspector, now, |_| true)?;
    let descriptor = inspector.backend.descriptor();
    Ok(Preview {
        house: descriptor.house.clone(),
        backend: descriptor.backend.clone(),
        trigger,
        observed_at: now,
        entries,
        suggestions: match trigger {
            InspectionTrigger::DiskPressure => EXTERNAL_CACHE_SUGGESTIONS.to_vec(),
            InspectionTrigger::Schedule | InspectionTrigger::Manual => Vec::new(),
        },
    })
}

/// The eligible steps of `entry` and the evidence digest of each.
fn previewed_steps(entry: &PreviewEntry) -> impl Iterator<Item = (Step, &ExternalRef)> {
    let release = entry
        .eligible()
        .then_some((Step::Release, &entry.observation));
    let build = entry
        .build_output
        .as_ref()
        .filter(|build| build.decision == Decision::Release)
        .map(|build| (Step::BuildOutput, &build.observation));
    release.into_iter().chain(build)
}

/// Bounds and authority for [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOptions {
    /// The explicit release grant. Scheduled runs delegate it to each release
    /// task, so the house must hold it as a standing grant; interactive runs
    /// need a person's consent per release within house policy instead.
    pub release: Grant,
    /// Instruction revisions pinned on each release task.
    pub provenance: Provenance,
    /// Lease on each release task.
    pub lease: LeaseTtl,
    /// Oldest stored approval that still authorizes a scheduled release. An
    /// interactive run uses no stored approval.
    pub max_approval_age: Duration,
    /// Most releases attempted in one call; the rest are deferred.
    pub max_releases: usize,
}

/// A person's consent to one release, with the digest of the preview they gave
/// it for.
///
/// [`apply`] compares that digest with the digest of the evidence it is about
/// to act on and refuses the release, before writing anything, unless the two
/// are equal. A consent that names no digest is refused too: it does not say
/// what the person saw. The digest is the `observation` of the
/// [`PreviewEntry`] they were shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseConsent {
    consent: Consent,
    digest: Option<ExternalRef>,
}

impl ReleaseConsent {
    /// A consent that names no digest yet; bind it with [`Self::for_digest`].
    #[must_use]
    pub const fn new(consent: Consent) -> Self {
        Self {
            consent,
            digest: None,
        }
    }

    /// The consent, given for the preview whose evidence digest is `digest`.
    #[must_use]
    pub fn for_digest(mut self, digest: ExternalRef) -> Self {
        self.digest = Some(digest);
        self
    }
}

/// Supplies a person's consent for one release under an interactive claim.
///
/// An interactive [`apply`] needs nothing else: no stored approval. It asks
/// for a consent before it writes anything, and it is given no digest to echo
/// back: the [`ReleaseConsent`] must carry the digest of a preview the person
/// was actually shown, taken from an earlier [`inspect`]. `apply` inspects
/// again and compares that digest with the evidence it executes, so evidence
/// that changed since the person looked is refused instead of released. The
/// release task is derived from the same digest, so a [`Consent`] minted for
/// other evidence names another task and is refused as well, and the evidence
/// is checked once more immediately before the effect.
///
/// The library cannot tell a person from a script: an implementation must
/// return a consent only for something a person present agreed to, the same
/// trust boundary as consent for any other effect. Scheduled workflows never
/// build one.
pub trait ConsentSource {
    /// The consent for exactly `effect` on `task` at `revision`, if a person
    /// gave it, together with the digest of the preview they gave it for.
    fn consent(
        &self,
        task: &TaskId,
        effect: &Effect,
        revision: EvidenceRevision,
    ) -> Option<ReleaseConsent>;
}

/// No consent: the source for scheduled runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoConsent;

impl ConsentSource for NoConsent {
    fn consent(&self, _: &TaskId, _: &Effect, _: EvidenceRevision) -> Option<ReleaseConsent> {
        None
    }
}

/// What happened to one resource in [`apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
#[serde(tag = "type", content = "detail", rename_all = "kebab-case")]
pub enum ReleaseOutcome {
    /// The backend released it in this call.
    Released,
    /// Eligible, but no person has approved this exact evidence. Only a
    /// scheduled run reports it; an interactive run reports
    /// [`Self::ConsentMissing`].
    NotApproved,
    /// The approval of this evidence is older than allowed; a person must
    /// approve it again.
    ApprovalExpired,
    /// The evidence changed between the approval and the effect.
    Changed,
    /// Another run holds the release task, or an earlier release of this
    /// resource that another run holds.
    HeldElsewhere,
    /// An interactive run had no consent for this release. Nothing was
    /// written: no task exists and the release bound is untouched.
    ConsentMissing,
    /// The consent names no digest, so it does not say what the person saw.
    /// Nothing was written.
    ConsentUnbound,
    /// The consent was given for a preview whose digest differs from the
    /// evidence now: it changed since the person looked. Nothing was written.
    ConsentMismatch,
    /// The backend did not apply the release.
    NotApplied(NotAppliedReason),
    /// The outcome is unknown, here or in an earlier release of this
    /// resource; the next run reconciles it before any new release.
    Uncertain,
    /// The release task had already settled; nothing was repeated.
    AlreadySettled(Settlement),
    /// Over this call's release bound.
    Deferred,
}

impl ReleaseOutcome {
    /// Whether an interactive run refused the release for want of a usable
    /// consent, before it wrote anything.
    const fn consent_refused(self) -> bool {
        matches!(
            self,
            Self::ConsentMissing | Self::ConsentUnbound | Self::ConsentMismatch
        )
    }
}

/// One resource's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseResult {
    /// The resource.
    pub resource: ResourceRef,
    /// The release task, once one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// The outcome.
    pub outcome: ReleaseOutcome,
    /// For a worktree released in this call: its size measured before the
    /// release. It is what the release should free, not proof of it: the
    /// backend's response is the only evidence the release happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<DiskUsage>,
}

/// The result of [`apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    /// The inspection this call acted on.
    pub preview: Preview,
    /// Interrupted releases from earlier runs, reconciled first.
    pub recovered: Vec<ReleaseResult>,
    /// Results for the eligible resources.
    pub results: Vec<ReleaseResult>,
}

/// Reconcile interrupted releases, then release each eligible resource whose
/// exact evidence a person approved with [`approve`] (a scheduled `claimant`)
/// or consented to through `consents` (an interactive `claimant`, which needs
/// no stored approval). A consent counts only if the digest it carries equals
/// the digest of the evidence this call inspected; otherwise the release is
/// refused before anything is written. Writes no marker.
///
/// # Errors
/// As [`inspect`]; [`CleanupError::GrantMismatch`] for a grant that is not
/// release on this backend; [`ContractError::AuthorityExpansion`] when a
/// scheduled claimant's house does not hold the release grant as a standing
/// grant; and store errors, which leave interrupted work for the next run.
pub fn apply(
    inspector: &Inspector<'_>,
    grants: &HouseGrants,
    claimant: &Claimant,
    consents: &dyn ConsentSource,
    options: &ApplyOptions,
    clock: &dyn Clock,
) -> Result<ApplyReport> {
    let descriptor = inspector.backend.descriptor();
    if options.release.permission != Permission::ReleaseResource
        || options.release.destination != descriptor.backend
    {
        return Err(CleanupError::GrantMismatch.into());
    }
    let authority = match claimant.trigger {
        Trigger::Scheduled | Trigger::Event(_) => {
            TaskAuthority::delegate(grants, [options.release.clone()])?
        }
        Trigger::Interactive => TaskAuthority::delegate(grants, [])?,
    };
    let run = Run {
        inspector,
        grants,
        claimant,
        consents,
        options,
        clock,
        authority,
    };
    let trigger = match claimant.trigger {
        Trigger::Scheduled | Trigger::Event(_) => InspectionTrigger::Schedule,
        Trigger::Interactive => InspectionTrigger::Manual,
    };
    let preview = inspect(inspector, trigger, clock.now())?;
    let eligible: Vec<&PreviewEntry> = preview
        .entries
        .iter()
        .filter(|entry| entry.eligible())
        .collect();

    // Decide what each eligible resource needs before acting on anything.
    let mut planned = Vec::with_capacity(eligible.len());
    for entry in eligible {
        let plan = match &claimant.trigger {
            // A person present gives one consent per release, for exactly the
            // evidence in this preview; that consent is the whole gate.
            Trigger::Interactive => Plan::Drive(release_task_id(
                &entry.observation,
                preview.observed_at,
                &claimant.trigger,
            )?),
            // Nobody is present, so a person's earlier approval of this
            // evidence must stand in for them.
            Trigger::Scheduled | Trigger::Event(_) => match approval(
                inspector.store,
                &entry.resource,
                Step::Release,
                &entry.observation,
                options.max_approval_age,
                clock.now(),
            )? {
                Approval::Missing => Plan::Report(ReleaseOutcome::NotApproved),
                Approval::Expired => Plan::Report(ReleaseOutcome::ApprovalExpired),
                Approval::Approved(approved_at) => Plan::Drive(release_task_id(
                    &entry.observation,
                    approved_at,
                    &claimant.trigger,
                )?),
            },
        };
        planned.push((entry, plan));
    }

    // Reconcile every other unfinished release first. A resource whose
    // earlier release is still unresolved gets no second release.
    let mut recovered = Vec::new();
    let mut blocked: BTreeMap<ResourceRef, ReleaseOutcome> = BTreeMap::new();
    for task in inspector.store.tasks()? {
        if !is_release_task(&task) || matches!(task.state(), TaskState::Settled { .. }) {
            continue;
        }
        let Some(resource) = task.spec().resources.iter().next() else {
            continue;
        };
        let id = &task.spec().id;
        if planned
            .iter()
            .any(|(_, plan)| matches!(plan, Plan::Drive(planned) if planned == id))
        {
            continue;
        }
        let outcome = run.drive(id.clone(), resource, None)?;
        if matches!(
            outcome,
            ReleaseOutcome::Uncertain | ReleaseOutcome::HeldElsewhere
        ) {
            blocked.insert(resource.clone(), outcome);
        }
        recovered.push(ReleaseResult {
            resource: resource.clone(),
            task: Some(id.clone()),
            outcome,
            measured: None,
        });
    }

    let mut results = Vec::with_capacity(planned.len());
    let mut attempted = 0_usize;
    for (entry, plan) in planned {
        let (task, outcome) = match plan {
            Plan::Report(outcome) => (None, outcome),
            Plan::Drive(task) => {
                // Blocked and deferred resources get no release task.
                if let Some(outcome) = blocked.get(&entry.resource) {
                    (None, *outcome)
                } else if attempted >= options.max_releases {
                    (None, ReleaseOutcome::Deferred)
                } else {
                    let outcome = run.drive(task.clone(), &entry.resource, Some(entry))?;
                    if outcome.consent_refused() {
                        // Refused before anything was written: no task, and
                        // no share of the bound, so a person can refuse some
                        // releases and still consent to others in one run.
                        (None, outcome)
                    } else {
                        attempted = attempted.saturating_add(1);
                        (Some(task), outcome)
                    }
                }
            }
        };
        let measured = entry.usage.filter(|_| outcome == ReleaseOutcome::Released);
        results.push(ReleaseResult {
            resource: entry.resource.clone(),
            task,
            outcome,
            measured,
        });
    }
    Ok(ApplyReport {
        preview,
        recovered,
        results,
    })
}

/// What happened to one build output directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum BuildOutcome {
    /// Removed in this call.
    Removed,
    /// No person has approved this evidence.
    NotApproved,
    /// The approval is older than allowed; a person must approve it again.
    ApprovalExpired,
    /// The evidence changed since the approval; nothing was removed.
    Changed,
    /// The final checks refused the directory; nothing was removed.
    Refused,
    /// Removal failed part way; a later run finishes it.
    Failed,
}

/// One build output directory's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildResult {
    /// The worktree.
    pub resource: ResourceRef,
    /// The directory's name at the top of the worktree.
    pub directory: String,
    /// The outcome.
    pub outcome: BuildOutcome,
    /// Space measured just before removal, for a removed directory. Removal
    /// that succeeded should free it; hard links to files outside the
    /// directory would not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<DiskUsage>,
    /// For a failed removal: the I/O error kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The result of [`reclaim_build_output`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildReport {
    /// The inspection this call acted on.
    pub preview: Preview,
    /// One result per eligible directory.
    pub results: Vec<BuildResult>,
}

impl BuildReport {
    /// Total space measured before the removals that succeeded.
    #[must_use]
    pub fn measured(&self) -> DiskUsage {
        self.results
            .iter()
            .filter_map(|result| result.measured)
            .fold(
                DiskUsage {
                    bytes: 0,
                    complete: true,
                },
                |total, measured| DiskUsage {
                    bytes: total.bytes.saturating_add(measured.bytes),
                    complete: total.complete && measured.complete,
                },
            )
    }
}

/// Remove regenerable build output from Kitchen-owned worktrees whose
/// workers have all settled, where a person approved the same evidence with
/// [`approve`]. Needs no grant: nothing leaves the worktree's own ignored
/// build directories, and nothing goes through the backend. Each worktree is
/// revalidated immediately before its directories are removed. Writes no
/// marker.
///
/// # Errors
/// As [`inspect`].
pub fn reclaim_build_output(
    inspector: &Inspector<'_>,
    trigger: InspectionTrigger,
    max_approval_age: Duration,
    clock: &dyn Clock,
) -> Result<BuildReport> {
    let preview = inspect(inspector, trigger, clock.now())?;
    let mut results = Vec::new();
    for entry in preview
        .entries
        .iter()
        .filter(|entry| entry.build_output_eligible())
    {
        let Some(build) = &entry.build_output else {
            continue;
        };
        let report = |outcome: BuildOutcome| {
            build.directories.iter().map(move |directory| BuildResult {
                resource: entry.resource.clone(),
                directory: directory.name.clone(),
                outcome,
                measured: None,
                error: None,
            })
        };
        match approval(
            inspector.store,
            &entry.resource,
            Step::BuildOutput,
            &build.observation,
            max_approval_age,
            clock.now(),
        )? {
            Approval::Missing => {
                results.extend(report(BuildOutcome::NotApproved));
                continue;
            }
            Approval::Expired => {
                results.extend(report(BuildOutcome::ApprovalExpired));
                continue;
            }
            Approval::Approved(_) => {}
        }
        // Revalidate immediately before removing anything.
        let owner = entry.owner_task();
        let fresh = evaluate(inspector, clock.now(), |observation| {
            observation.resource == &entry.resource
                || owner.is_some_and(|task| observation.owner == Some(task))
        })?;
        let unchanged = fresh
            .iter()
            .find(|candidate| candidate.resource == entry.resource)
            .and_then(|candidate| candidate.build_output.as_ref())
            .is_some_and(|current| {
                current.decision == Decision::Release && current.observation == build.observation
            });
        let path = inspector.worktrees.locate(&entry.resource);
        let Some(path) = path.filter(|_| unchanged) else {
            results.extend(report(BuildOutcome::Changed));
            continue;
        };
        for directory in &build.directories {
            let usage = disk_usage(&path.join(&directory.name));
            let (outcome, measured, error) = match build::remove(&path, &directory.name) {
                Ok(()) => (BuildOutcome::Removed, Some(usage), None),
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    (BuildOutcome::Refused, None, None)
                }
                Err(error) => (BuildOutcome::Failed, None, Some(error.kind().to_string())),
            };
            results.push(BuildResult {
                resource: entry.resource.clone(),
                directory: directory.name.clone(),
                outcome,
                measured,
                error,
            });
        }
    }
    Ok(BuildReport { preview, results })
}

/// What [`apply`] does with one eligible resource.
enum Plan {
    /// Report without acting.
    Report(ReleaseOutcome),
    /// Drive this release task.
    Drive(TaskId),
}

/// One [`apply`] call's fixed inputs.
struct Run<'a> {
    inspector: &'a Inspector<'a>,
    grants: &'a HouseGrants,
    claimant: &'a Claimant,
    consents: &'a dyn ConsentSource,
    options: &'a ApplyOptions,
    clock: &'a dyn Clock,
    authority: TaskAuthority,
}

impl Run<'_> {
    /// Move one release task forward. With `entry`, a missing task is
    /// created and a release not yet attempted runs after revalidation;
    /// without it (recovery), only reconciliation and settlement happen.
    fn drive(
        &self,
        id: TaskId,
        resource: &ResourceRef,
        entry: Option<&PreviewEntry>,
    ) -> Result<ReleaseOutcome> {
        let store = self.inspector.store;
        let effect = Effect::Worker(Operation::ReleaseResource {
            resource: resource.clone(),
        });
        let existing = match store.task(&id) {
            Ok(record) => Some(record),
            Err(Error::State(StateError::TaskNotFound(_))) if entry.is_some() => None,
            Err(error) => return Err(error),
        };
        // A settled task needs nothing more, least of all a consent.
        if let Some(record) = &existing
            && let TaskState::Settled { settlement, .. } = record.state()
        {
            return Ok(ReleaseOutcome::AlreadySettled(*settlement));
        }
        // Consent is needed only where a release may run; recovery never
        // starts one. It is asked for before anything is written, so a declined
        // run leaves no task behind in the house's bounded store. A task that
        // does not exist yet starts at the initial evidence revision.
        let consent = match (&self.claimant.trigger, entry) {
            (Trigger::Scheduled | Trigger::Event(_), _) | (Trigger::Interactive, None) => None,
            (Trigger::Interactive, Some(entry)) => {
                let revision = existing
                    .as_ref()
                    .map_or(EvidenceRevision::INITIAL, |record| {
                        record.evidence().revision()
                    });
                let Some(given) = self.consents.consent(&id, &effect, revision) else {
                    return Ok(ReleaseOutcome::ConsentMissing);
                };
                // The person's consent must name the evidence this run is
                // about to act on. Compared before anything is written, so a
                // consent for other or unstated evidence leaves no task.
                match given.digest {
                    Some(digest) if digest == entry.observation => Some(given.consent),
                    Some(_) => return Ok(ReleaseOutcome::ConsentMismatch),
                    None => return Ok(ReleaseOutcome::ConsentUnbound),
                }
            }
        };
        // Read again after the person answered: the record may have moved.
        let record = match existing {
            Some(_) => store.task(&id)?,
            None => {
                store.create_task(self.spec(&id, resource)?, self.claimant, self.clock.now())?;
                store.task(&id)?
            }
        };
        let now = self.clock.now();
        let lease = match record.state() {
            TaskState::Settled { settlement, .. } => {
                return Ok(ReleaseOutcome::AlreadySettled(*settlement));
            }
            TaskState::Open => store.claim(&id, self.claimant, self.options.lease, now),
            TaskState::Claimed { lease } if lease.is_live(now) => {
                return Ok(ReleaseOutcome::HeldElsewhere);
            }
            TaskState::Claimed { .. } => {
                store.take_over(&id, self.claimant, self.options.lease, now)
            }
        };
        let fence = match lease {
            Ok(lease) => lease.fence(),
            Err(Error::State(
                StateError::ClaimHeld { .. }
                | StateError::LeaseExpired { .. }
                | StateError::LeaseLive { .. },
            )) => return Ok(ReleaseOutcome::HeldElsewhere),
            Err(error) => return Err(error),
        };
        let report = reconcile(store, self.executor(), &id, fence, self.clock)?;
        if !report.unresolved.is_empty() || !report.foreign.is_empty() {
            store.relinquish(&id, fence, self.clock.now())?;
            return Ok(ReleaseOutcome::Uncertain);
        }
        let attempt = match store.start_attempt(&id, fence, self.clock.now())? {
            AttemptStart::Started(attempt) | AttemptStart::AlreadyRunning(attempt) => attempt,
            AttemptStart::Exhausted => {
                return Ok(ReleaseOutcome::AlreadySettled(Settlement::Exhausted));
            }
        };
        let finish = |outcome: AttemptOutcome| {
            store.finish_attempt(&id, fence, attempt, outcome, self.clock.now())
        };
        let failed = AttemptOutcome::Failed(FailureClass::Permanent);
        match release_state(&store.task(&id)?) {
            Some(EffectState::Applied { .. }) => {
                finish(AttemptOutcome::Succeeded)?;
                return Ok(ReleaseOutcome::Released);
            }
            // Proven absent: the release may run again after revalidation.
            Some(EffectState::NotApplied {
                reason: NotAppliedReason::ConfirmedAbsent,
                ..
            }) if entry.is_some() => {}
            Some(EffectState::NotApplied { reason, .. }) => {
                let reason = *reason;
                finish(failed)?;
                return Ok(ReleaseOutcome::NotApplied(reason));
            }
            Some(_) => {
                // Unreachable after a clean reconcile; never act on it.
                store.relinquish(&id, fence, self.clock.now())?;
                return Ok(ReleaseOutcome::Uncertain);
            }
            None => {}
        }
        let Some(entry) = entry else {
            // No longer eligible and never attempted: give it up.
            finish(failed)?;
            return Ok(ReleaseOutcome::Changed);
        };
        // Revalidate immediately before the effect.
        let owner = entry.owner_task();
        let fresh = evaluate(self.inspector, self.clock.now(), |observation| {
            observation.resource == resource
                || owner.is_some_and(|task| observation.owner == Some(task))
        })?;
        let unchanged = fresh
            .iter()
            .find(|candidate| &candidate.resource == resource)
            .is_some_and(|candidate| {
                candidate.eligible() && candidate.observation == entry.observation
            });
        if !unchanged {
            finish(failed)?;
            return Ok(ReleaseOutcome::Changed);
        }
        let plan = EffectPlan {
            task: id.clone(),
            fence,
            name: EffectName::new("release")?,
            decided_at: store.task(&id)?.evidence().revision(),
            effect,
            consent,
        };
        let record = run_effect(store, self.executor(), self.grants, plan, self.clock)?;
        match record.state() {
            EffectState::Applied { .. } => {
                finish(AttemptOutcome::Succeeded)?;
                Ok(ReleaseOutcome::Released)
            }
            EffectState::NotApplied { reason, .. } => {
                let reason = *reason;
                finish(failed)?;
                Ok(ReleaseOutcome::NotApplied(reason))
            }
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => {
                store.relinquish(&id, fence, self.clock.now())?;
                Ok(ReleaseOutcome::Uncertain)
            }
        }
    }

    fn executor(&self) -> &dyn EffectExecutor {
        self.inspector.backend
    }

    fn spec(&self, id: &TaskId, resource: &ResourceRef) -> Result<TaskSpec> {
        Ok(TaskSpec {
            id: id.clone(),
            role: Role::Dishwasher,
            repository: None,
            authority: self.authority.clone(),
            retry: RetryPolicy::new(RELEASE_ATTEMPTS, RELEASE_BUDGET)?,
            provenance: self.options.provenance.clone(),
            resources: BTreeSet::from([resource.clone()]),
            requires: CapabilityRequirements::new().with(
                ExecutorKind::Worker,
                [
                    Capability::ResourceInventory,
                    Capability::WorkerStatusAndOutcome,
                    Capability::ResourceRelease,
                ],
            ),
            agent: None,
        })
    }
}

/// The latest release effect of a release task.
fn release_state(task: &TaskRecord) -> Option<&EffectState> {
    task.effects()
        .iter()
        .rev()
        .find(|effect| {
            matches!(
                effect.request().effect(),
                Effect::Worker(Operation::ReleaseResource { .. })
            )
        })
        .map(EffectRecord::state)
}

fn is_release_task(task: &TaskRecord) -> bool {
    task.spec().role == Role::Dishwasher && task.spec().id.as_str().starts_with(TASK_PREFIX)
}

/// The release task for one observation. `at` is the approval's time for a
/// scheduled run and the run's own time for an interactive one, whose consent
/// is given per release: either way a new approval or a new run starts a new
/// task, while an interrupted run resumes the same one (or, once its time has
/// passed, is reconciled as an unfinished task first). The digest is part of
/// the identity, so a consent minted for one release task names one piece of
/// evidence. So is the trigger: a scheduled and an interactive run hold
/// different authority, and neither may inherit a task the other created.
fn release_task_id(observation: &ExternalRef, at: Timestamp, trigger: &Trigger) -> Result<TaskId> {
    let mut digest = Sha256::new();
    digest.update(b"kitchen-dishwasher-release-v2\0");
    digest.update(observation.as_str().as_bytes());
    digest.update(at.as_unix_millis().to_be_bytes());
    digest.update(trigger.to_string().as_bytes());
    let hex = hex(digest.finalize().as_slice());
    let short = hex.get(..48).ok_or(CleanupError::Encoding)?;
    Ok(TaskId::new(&format!("{TASK_PREFIX}{short}"))?)
}

/// What the store holds for one step's evidence.
enum Approval {
    /// No approval by a person names this evidence.
    Missing,
    /// A person approved it, but longer ago than allowed.
    Expired,
    /// A person approved it at this time.
    Approved(Timestamp),
}

/// Whether a person approved `observation` for `step` of `resource`. Reads
/// only. A marker under the key counts only when an interactive claimant
/// recorded a `cleanup.approval` fact for this step: anything else, however it
/// got there, approves nothing.
fn approval(
    store: &HouseStore,
    resource: &ResourceRef,
    step: Step,
    observation: &ExternalRef,
    max_age: Duration,
    now: Timestamp,
) -> Result<Approval> {
    let key = marker_key(resource, observation)?;
    let Some(marker) = store.marker(&key)? else {
        return Ok(Approval::Missing);
    };
    if marker.recorded_by().trigger != Trigger::Interactive {
        return Ok(Approval::Missing);
    }
    let approves = marker
        .fact()
        .decode::<ApprovalFact>(&schema()?)
        .is_ok_and(|fact| fact.step == step);
    if !approves {
        return Ok(Approval::Missing);
    }
    if now.saturating_since(marker.recorded_at()) > max_age {
        return Ok(Approval::Expired);
    }
    Ok(Approval::Approved(marker.recorded_at()))
}

/// What [`approve`] did with one digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    /// Recorded: this step of this resource may now run until the approval
    /// expires, while its evidence stays the same.
    Approved {
        /// The resource.
        resource: ResourceRef,
        /// The step.
        step: Step,
    },
    /// No current eligible step has this digest: the evidence changed, the
    /// step is retained, or the digest is not from this house's inspection.
    NotCurrent,
}

/// One digest's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalResult {
    /// The evidence digest the person named.
    pub observation: ExternalRef,
    /// What happened.
    pub outcome: ApprovalOutcome,
}

/// Record a person's approval of the previewed steps named by `digests`, the
/// `observation` values of a [`Preview`]. Inspects again first, so only a
/// step that is eligible now, on this exact evidence, is approved; nothing is
/// recorded for a digest that no longer matches. Approving again renews the
/// approval's time.
///
/// # Errors
/// [`CleanupError::ApprovalNeedsPerson`] unless `approver` is
/// [`Trigger::Interactive`], because an approval by the automation that
/// inspected is no approval; as [`inspect`]; and marker store errors, such as
/// the shared marker table being full. Nothing is recorded for the digests
/// after a failure.
pub fn approve(
    inspector: &Inspector<'_>,
    approver: &Claimant,
    digests: &[ExternalRef],
    clock: &dyn Clock,
) -> Result<Vec<ApprovalResult>> {
    if approver.trigger != Trigger::Interactive {
        return Err(CleanupError::ApprovalNeedsPerson.into());
    }
    let now = clock.now();
    let preview = inspect(inspector, InspectionTrigger::Manual, now)?;
    let mut results = Vec::with_capacity(digests.len());
    for digest in digests {
        // An eligible step always has a settled owning task; without one
        // there is nothing to approve.
        let target = preview.entries.iter().find_map(|entry| {
            let owner = entry.owner_task()?;
            previewed_steps(entry)
                .find(|(_, observation)| *observation == digest)
                .map(|(step, _)| (entry, owner, step))
        });
        let outcome = match target {
            Some((entry, owner, step)) => {
                record_approval(inspector.store, entry, owner, step, digest, approver, now)?;
                ApprovalOutcome::Approved {
                    resource: entry.resource.clone(),
                    step,
                }
            }
            None => ApprovalOutcome::NotCurrent,
        };
        results.push(ApprovalResult {
            observation: digest.clone(),
            outcome,
        });
    }
    Ok(results)
}

fn schema() -> Result<MarkerSchema> {
    Ok(MarkerSchema::new(APPROVAL_SCHEMA, NonZeroU32::MIN)?)
}

fn marker_key(resource: &ResourceRef, observation: &ExternalRef) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(WORKFLOW)?,
        item: WorkItem::Resource {
            resource: resource.clone(),
        },
        subject: MarkerSubject::Observation(observation.clone()),
    })
}

/// Record or renew the approval marker for one step of `entry`, whose
/// owning task is `owner`.
fn record_approval(
    store: &HouseStore,
    entry: &PreviewEntry,
    owner: &TaskId,
    step: Step,
    observation: &ExternalRef,
    approver: &Claimant,
    now: Timestamp,
) -> Result<()> {
    let fact = MarkerFact::workflow(
        schema()?,
        &ApprovalFact {
            step,
            owner: owner.clone(),
            approved_at: now,
        },
    )?;
    let key = marker_key(&entry.resource, observation)?;
    match store.marker(&key)? {
        None => store.record_marker(key, fact, approver, now).map(drop),
        Some(current) => store
            .supersede_marker(&key, current.fact(), fact, approver, now)
            .map(drop),
    }
}

/// Evaluate the inventoried resources that `include` selects. Sibling
/// checks still see every resource of the same owning task.
fn evaluate(
    inspector: &Inspector<'_>,
    now: Timestamp,
    include: impl Fn(&Observed<'_>) -> bool,
) -> Result<Vec<PreviewEntry>> {
    let descriptor = inspector.backend.descriptor();
    if &descriptor.house != inspector.store.house() {
        return Err(ContractError::CrossHouse {
            expected: inspector.store.house().clone(),
            found: descriptor.house.clone(),
        }
        .into());
    }
    descriptor.capabilities.require([
        Capability::ResourceInventory,
        Capability::WorkerStatusAndOutcome,
    ])?;
    let inventory = inspector
        .backend
        .inventory()
        .map_err(CleanupError::Backend)?;
    let mut seen = BTreeSet::new();
    if !inventory
        .iter()
        .all(|observation| seen.insert(&observation.resource))
    {
        return Err(CleanupError::DuplicateResource.into());
    }
    let tasks = inspector.store.tasks()?;
    let mut observed = Vec::with_capacity(inventory.len());
    for observation in &inventory {
        let ownership = ownership(&tasks, &observation.resource);
        let owner = match &ownership {
            Ownership::Task(owner) => Some(owner.task.clone()),
            Ownership::Unknown | Ownership::Ambiguous { .. } => None,
        };
        observed.push((observation, ownership, owner));
    }
    // Workers are observed only when they are selected or share a selected
    // resource's owner, so a narrow revalidation stays narrow.
    let selected: Vec<bool> = observed
        .iter()
        .map(|(observation, _, owner)| {
            include(&Observed {
                resource: &observation.resource,
                owner: owner.as_ref(),
            })
        })
        .collect();
    let relevant_owners: BTreeSet<&TaskId> = observed
        .iter()
        .zip(&selected)
        .filter_map(|((_, _, owner), chosen)| owner.as_ref().filter(|_| *chosen))
        .collect();
    let mut workers = Vec::with_capacity(observed.len());
    for ((observation, _, owner), chosen) in observed.iter().zip(&selected) {
        let relevant = *chosen
            || owner
                .as_ref()
                .is_some_and(|task| relevant_owners.contains(task));
        let state = if relevant && observation.resource.kind == ResourceKind::Worker {
            Some(
                inspector
                    .backend
                    .observe_worker(&observation.resource)
                    .map_err(CleanupError::Backend)?,
            )
        } else {
            None
        };
        workers.push(state);
    }
    // Per owning task: is anything it owns in use, or taken over?
    let mut busy: BTreeMap<&TaskId, (bool, bool)> = BTreeMap::new();
    for ((observation, _, owner), worker) in observed.iter().zip(&workers) {
        let Some(task) = owner else { continue };
        // A branch or schedule is never reclaimed, so it being in use says
        // nothing about whether the task's other resources are.
        if !reclaimable(observation.resource.kind) {
            continue;
        }
        let flags = busy.entry(task).or_default();
        flags.0 |= observation.liveness != Liveness::Exited
            || worker.is_some_and(|state| !matches!(state, WorkerState::Settled(_)));
        flags.1 |= *worker == Some(WorkerState::UserTakeover);
    }
    // A worker the owning task created is observed even when the inventory
    // omits it: a person's takeover or a live agent must still keep the task.
    for task_id in &relevant_owners {
        let Some(record) = tasks.iter().find(|task| &task.spec().id == *task_id) else {
            continue;
        };
        for worker in created_workers(record).filter(|worker| !seen.contains(worker)) {
            let state = inspector
                .backend
                .observe_worker(worker)
                .map_err(CleanupError::Backend)?;
            let flags = busy.entry(task_id).or_default();
            flags.0 |= !matches!(state, WorkerState::Settled(_) | WorkerState::Missing);
            flags.1 |= state == WorkerState::UserTakeover;
        }
    }
    let mut entries = Vec::new();
    for (((observation, ownership, owner), worker), chosen) in
        observed.iter().zip(&workers).zip(&selected)
    {
        if !*chosen {
            continue;
        }
        let inspected = (observation.resource.kind == ResourceKind::Worktree)
            .then(|| worktree_evidence(inspector, &observation.resource));
        let (worktree, build_dirs) = match inspected {
            Some((evidence, build_dirs)) => (Some(evidence), build_dirs),
            None => (None, Vec::new()),
        };
        let mut reasons = BTreeSet::new();
        own_reasons(
            &mut reasons,
            observation,
            descriptor.backend == observation.resource.backend,
            ownership,
            &tasks,
            *worker,
            worktree.as_ref(),
        );
        if let Some(task) = owner
            && let Some((in_use, takeover)) = busy.get(task)
        {
            let own_busy = observation.liveness != Liveness::Exited
                || worker.is_some_and(|state| !matches!(state, WorkerState::Settled(_)));
            if *in_use && !own_busy {
                reasons.insert(Exclusion::SiblingInUse);
            }
            if *takeover {
                reasons.insert(Exclusion::UserTakeover);
            }
        }
        let decision = if reasons.is_empty() {
            Decision::Release
        } else {
            Decision::Retain {
                reasons: reasons.into_iter().collect(),
            }
        };
        let settled_for = match ownership {
            Ownership::Task(TaskOwner {
                state: OwnerState::Settled { at, .. },
                ..
            }) => Some(now.saturating_since(*at)),
            Ownership::Task(_) | Ownership::Unknown | Ownership::Ambiguous { .. } => None,
        };
        let build_output = match &worktree {
            Some(WorktreeEvidence::Read { .. }) => {
                build_output(inspector, observation, ownership, &decision, build_dirs)?
            }
            Some(WorktreeEvidence::Unlocated | WorktreeEvidence::Unreadable { .. }) | None => None,
        };
        let usage = (decision == Decision::Release && worktree.is_some())
            .then(|| inspector.worktrees.locate(&observation.resource))
            .flatten()
            .map(|path| disk_usage(&path));
        let digest = digest(&DigestInput {
            domain: Step::Release,
            resource: &observation.resource,
            liveness: liveness_name(observation.liveness),
            backend_owner: observation.owner.as_ref(),
            worker: worker.map(worker_name),
            ownership,
            worktree: worktree.as_ref(),
            decision: &decision,
        })?;
        entries.push(PreviewEntry {
            resource: observation.resource.clone(),
            observation: digest,
            liveness: observation.liveness,
            backend_owner: observation.owner.clone(),
            worker: *worker,
            ownership: ownership.clone(),
            worktree,
            build_output,
            usage,
            settled_for,
            decision,
        });
    }
    Ok(entries)
}

/// A resource and its owner, for selecting what [`evaluate`] inspects.
struct Observed<'a> {
    resource: &'a ResourceRef,
    owner: Option<&'a TaskId>,
}

fn own_reasons(
    reasons: &mut BTreeSet<Exclusion>,
    observation: &ResourceObservation,
    same_backend: bool,
    ownership: &Ownership,
    tasks: &[TaskRecord],
    worker: Option<WorkerState>,
    worktree: Option<&WorktreeEvidence>,
) {
    if !same_backend {
        reasons.insert(Exclusion::ForeignBackend);
    }
    if !reclaimable(observation.resource.kind) {
        reasons.insert(Exclusion::NotReclaimable);
    }
    match ownership {
        Ownership::Unknown => {
            reasons.insert(Exclusion::UnknownOwner);
        }
        Ownership::Ambiguous { .. } => {
            reasons.insert(Exclusion::AmbiguousOwner);
        }
        Ownership::Task(owner) => {
            match &observation.owner {
                Some(recorded) if recorded.as_str() != owner.key.as_str() => {
                    reasons.insert(Exclusion::BackendOwnerMismatch);
                }
                Some(_) => {}
                // Kitchen's own record is not enough to delete on.
                None => {
                    reasons.insert(Exclusion::BackendOwnerUnrecorded);
                }
            }
            if !matches!(owner.state, OwnerState::Settled { .. }) {
                reasons.insert(Exclusion::OwnerActive);
            }
            let unresolved = tasks
                .iter()
                .find(|task| task.spec().id == owner.task)
                .is_some_and(|task| {
                    task.unresolved_effects().next().is_some()
                        || task
                            .effects()
                            .iter()
                            .any(|effect| matches!(effect.state(), EffectState::Waived { .. }))
                });
            if unresolved {
                reasons.insert(Exclusion::UnresolvedEffects);
            }
            if !owner.shared_with.is_empty() {
                reasons.insert(Exclusion::SharedWithTask);
            }
        }
    }
    match observation.liveness {
        Liveness::Exited => {}
        Liveness::Live => {
            reasons.insert(Exclusion::InUse);
        }
        Liveness::Unverifiable => {
            reasons.insert(Exclusion::LivenessUnverifiable);
        }
    }
    match worker {
        None | Some(WorkerState::Settled(_)) => {}
        Some(WorkerState::UserTakeover) => {
            reasons.insert(Exclusion::UserTakeover);
        }
        Some(
            WorkerState::Starting
            | WorkerState::Ready
            | WorkerState::AwaitingReply
            | WorkerState::Missing
            | WorkerState::Unknown,
        ) => {
            reasons.insert(Exclusion::WorkerNotSettled);
        }
    }
    match worktree {
        None => {}
        Some(WorktreeEvidence::Unlocated) => {
            reasons.insert(Exclusion::WorktreeUnlocated);
        }
        Some(WorktreeEvidence::Unreadable { .. }) => {
            reasons.insert(Exclusion::WorktreeUnreadable);
        }
        Some(WorktreeEvidence::Read {
            state,
            merged_head,
            ignored_files,
        }) => {
            let checks = [
                (!state.linked, Exclusion::MainCheckout),
                (state.locked, Exclusion::WorktreeLocked),
                (state.detached_head, Exclusion::DetachedHead),
                (state.operation.is_some(), Exclusion::OperationInProgress),
                (state.tracked_changes > 0, Exclusion::TrackedChanges),
                (state.untracked_files > 0, Exclusion::UntrackedFiles),
                (!ignored_files.is_empty(), Exclusion::IgnoredFiles),
                (state.hidden_tracked > 0, Exclusion::HiddenTrackedFiles),
                (
                    state.unpushed_commits && merged_head.as_ref() != Some(&state.head),
                    Exclusion::UnpreservedCommits,
                ),
            ];
            reasons.extend(
                checks
                    .into_iter()
                    .filter_map(|(applies, reason)| applies.then_some(reason)),
            );
        }
    }
}

/// Whether the dishwasher may ever release a resource of `kind`. Branches and
/// schedules are never removed.
const fn reclaimable(kind: ResourceKind) -> bool {
    match kind {
        ResourceKind::Worker | ResourceKind::Terminal | ResourceKind::Worktree => true,
        ResourceKind::Branch | ResourceKind::Schedule => false,
    }
}

/// The workers that applied effects of `task` created.
fn created_workers(task: &TaskRecord) -> impl Iterator<Item = &ResourceRef> {
    task.effects()
        .iter()
        .filter_map(|effect| match effect.state() {
            EffectState::Applied { receipt, .. } => Some(receipt.created()),
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::NotApplied { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => None,
        })
        .flatten()
        .filter(|resource| resource.kind == ResourceKind::Worker)
}

/// Ownership of `resource` according to the store's applied effects.
fn ownership(tasks: &[TaskRecord], resource: &ResourceRef) -> Ownership {
    let creators: Vec<(&TaskRecord, &EffectRecord)> = tasks
        .iter()
        .flat_map(|task| task.effects().iter().map(move |effect| (task, effect)))
        .filter(|(_, effect)| {
            matches!(effect.state(), EffectState::Applied { receipt, .. }
                if receipt.created().contains(resource))
        })
        .collect();
    let [(task, effect)] = creators.as_slice() else {
        if creators.is_empty() {
            return Ownership::Unknown;
        }
        let tasks: BTreeSet<TaskId> = creators
            .iter()
            .map(|(task, _)| task.spec().id.clone())
            .collect();
        return Ownership::Ambiguous {
            tasks: tasks.into_iter().collect(),
        };
    };
    let state = match task.state() {
        TaskState::Open => OwnerState::Open,
        TaskState::Claimed { .. } => OwnerState::Claimed,
        TaskState::Settled { settlement, at } => OwnerState::Settled {
            settlement: *settlement,
            at: *at,
        },
    };
    let shared_with = tasks
        .iter()
        .filter(|other| {
            other.spec().id != task.spec().id
                && !is_release_task(other)
                && !matches!(other.state(), TaskState::Settled { .. })
                && other.spec().resources.contains(resource)
        })
        .map(|other| other.spec().id.clone())
        .collect();
    Ownership::Task(TaskOwner {
        task: task.spec().id.clone(),
        attempt: effect.request().attempt(),
        key: effect.request().key().clone(),
        state,
        shared_with,
    })
}

/// What Git reports about a worktree, and the top-level directories of it that
/// are proven build output.
fn worktree_evidence(
    inspector: &Inspector<'_>,
    resource: &ResourceRef,
) -> (WorktreeEvidence, Vec<String>) {
    let Some(path) = inspector.worktrees.locate(resource) else {
        return (WorktreeEvidence::Unlocated, Vec::new());
    };
    match inspect_worktree(&path, inspector.git, inspector.remotes) {
        Ok(state) => {
            // A checkout Git cannot list precisely proves no build output, so
            // every ignored path then counts as work.
            let build_dirs = build::find(&path, inspector.git).unwrap_or_default();
            let ignored_files = state
                .ignored
                .iter()
                .filter(|ignored| !inside_any(ignored, &build_dirs))
                .cloned()
                .collect();
            let evidence = WorktreeEvidence::Read {
                state,
                merged_head: inspector.merged_heads.get(resource).cloned(),
                ignored_files,
            };
            (evidence, build_dirs)
        }
        Err(error) => (WorktreeEvidence::Unreadable { error }, Vec::new()),
    }
}

/// Whether the Git-listed `path` is a build directory or lies inside one.
fn inside_any(path: &str, build_dirs: &[String]) -> bool {
    let top = path.split('/').next().unwrap_or(path);
    build_dirs.iter().any(|dir| dir == top)
}

/// The evidence a decision rests on. Time-dependent values are excluded so
/// unchanged evidence keeps its digest.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DigestInput<'a> {
    domain: Step,
    resource: &'a ResourceRef,
    liveness: &'static str,
    backend_owner: Option<&'a ExternalRef>,
    worker: Option<&'static str>,
    ownership: &'a Ownership,
    worktree: Option<&'a WorktreeEvidence>,
    decision: &'a Decision,
}

/// The evidence a build output decision rests on. Sizes are excluded so a
/// measurement difference is not a change of evidence.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BuildDigestInput<'a> {
    domain: Step,
    resource: &'a ResourceRef,
    backend_owner: Option<&'a ExternalRef>,
    ownership: &'a Ownership,
    directories: Vec<&'a str>,
    decision: &'a Decision,
}

/// Exclusions that also keep build output. Uncommitted or unpushed work,
/// and a task that is still open, do not: build output is not work.
const fn blocks_build_output(reason: Exclusion) -> bool {
    match reason {
        Exclusion::ForeignBackend
        | Exclusion::NotReclaimable
        | Exclusion::UnknownOwner
        | Exclusion::AmbiguousOwner
        | Exclusion::BackendOwnerMismatch
        | Exclusion::BackendOwnerUnrecorded
        | Exclusion::UnresolvedEffects
        | Exclusion::SharedWithTask
        | Exclusion::InUse
        | Exclusion::LivenessUnverifiable
        | Exclusion::UserTakeover
        | Exclusion::WorkerNotSettled
        | Exclusion::SiblingInUse
        | Exclusion::WorktreeUnlocated
        | Exclusion::WorktreeUnreadable
        | Exclusion::MainCheckout
        | Exclusion::WorktreeLocked => true,
        Exclusion::OwnerActive
        | Exclusion::DetachedHead
        | Exclusion::OperationInProgress
        | Exclusion::TrackedChanges
        | Exclusion::UntrackedFiles
        | Exclusion::IgnoredFiles
        | Exclusion::HiddenTrackedFiles
        | Exclusion::UnpreservedCommits => false,
    }
}

/// Build output in a readable worktree, with its own decision.
fn build_output(
    inspector: &Inspector<'_>,
    observation: &ResourceObservation,
    ownership: &Ownership,
    worktree_decision: &Decision,
    names: Vec<String>,
) -> Result<Option<BuildOutput>> {
    if names.is_empty() {
        return Ok(None);
    }
    let Some(path) = inspector.worktrees.locate(&observation.resource) else {
        return Ok(None);
    };
    let reasons: Vec<Exclusion> = match worktree_decision {
        Decision::Release => Vec::new(),
        Decision::Retain { reasons } => reasons
            .iter()
            .copied()
            .filter(|reason| blocks_build_output(*reason))
            .collect(),
    };
    let decision = if reasons.is_empty() {
        Decision::Release
    } else {
        Decision::Retain { reasons }
    };
    let digest_input = BuildDigestInput {
        domain: Step::BuildOutput,
        resource: &observation.resource,
        backend_owner: observation.owner.as_ref(),
        ownership,
        directories: names.iter().map(String::as_str).collect(),
        decision: &decision,
    };
    let observation = digest(&digest_input)?;
    let directories = names
        .into_iter()
        .map(|name| {
            let usage = disk_usage(&path.join(&name));
            BuildDirectory { name, usage }
        })
        .collect();
    Ok(Some(BuildOutput {
        observation,
        directories,
        decision,
    }))
}

fn digest(input: &impl Serialize) -> Result<ExternalRef> {
    let bytes = serde_json::to_vec(input).map_err(|_| CleanupError::Encoding)?;
    let mut digest = Sha256::new();
    digest.update(b"kitchen-dishwasher-observation-v1\0");
    digest.update(&bytes);
    Ok(ExternalRef::new(&format!(
        "sha256:{}",
        hex(digest.finalize().as_slice())
    ))?)
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

const fn liveness_name(liveness: Liveness) -> &'static str {
    match liveness {
        Liveness::Live => "live",
        Liveness::Exited => "exited",
        Liveness::Unverifiable => "unverifiable",
    }
}

const fn worker_name(state: WorkerState) -> &'static str {
    match state {
        WorkerState::Starting => "starting",
        WorkerState::Ready => "ready",
        WorkerState::AwaitingReply => "awaiting-reply",
        WorkerState::UserTakeover => "user-takeover",
        WorkerState::Settled(WorkerOutcome::Succeeded) => "settled-succeeded",
        WorkerState::Settled(WorkerOutcome::Failed) => "settled-failed",
        WorkerState::Settled(WorkerOutcome::Cancelled) => "settled-cancelled",
        WorkerState::Missing => "missing",
        WorkerState::Unknown => "unknown",
    }
}

fn serialize_liveness<S: Serializer>(
    liveness: &Liveness,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(liveness_name(*liveness))
}

#[expect(
    clippy::ref_option,
    reason = "serde's serialize_with passes a reference to the field"
)]
fn serialize_worker<S: Serializer>(
    state: &Option<WorkerState>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match state {
        Some(state) => serializer.serialize_str(worker_name(*state)),
        None => serializer.serialize_none(),
    }
}

#[expect(
    clippy::ref_option,
    reason = "serde's serialize_with passes a reference to the field"
)]
fn serialize_age<S: Serializer>(
    age: &Option<Duration>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match age {
        Some(age) => serializer.serialize_u64(u64::try_from(age.as_millis()).unwrap_or(u64::MAX)),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(text: &str) -> Result<ExternalRef> {
        Ok(ExternalRef::new(text)?)
    }

    #[test]
    fn a_release_task_names_its_digest_time_and_trigger() -> Result<()> {
        let first = digest_of("sha256:aaaa")?;
        let at = Timestamp::from_unix_millis(1_000);
        let id = release_task_id(&first, at, &Trigger::Interactive)?;
        assert!(id.as_str().starts_with(TASK_PREFIX));
        // The same evidence at the same time by the same kind of run resumes
        // the same task.
        assert_eq!(release_task_id(&first, at, &Trigger::Interactive)?, id);
        // A consent minted for one release task cannot serve another piece of
        // evidence, a later run or approval, or the other kind of run.
        for other in [
            release_task_id(&digest_of("sha256:bbbb")?, at, &Trigger::Interactive)?,
            release_task_id(
                &first,
                Timestamp::from_unix_millis(1_001),
                &Trigger::Interactive,
            )?,
            release_task_id(&first, at, &Trigger::Scheduled)?,
        ] {
            assert_ne!(other, id);
        }
        Ok(())
    }
}
