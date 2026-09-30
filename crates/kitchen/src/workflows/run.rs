//! Scheduled runner passes: one bounded pass of pickup, coordination,
//! repair, or the merge gate, started by any trigger with the house identity.
//!
//! Every pass takes its workflow's single-consumer lease first
//! ([`Pass::consumer`]), so a duplicate trigger is refused by the store
//! rather than run twice ([`Outcome::Busy`]). A pass that ends without error
//! releases the lease; a pass that fails relinquishes it, and the next start
//! adopts it through the recorded relinquish. A pass that died without either
//! leaves an expired lease: ownership is then uncertain
//! ([`Outcome::OwnerUncertain`]) and only a start that asks for a takeover
//! proceeds, recording it.
//!
//! A task has one acting pass at a time, and that pass's fence is the only
//! one the store accepts for it. Pickup claims and launches under its own
//! consumer lease ([`Claimant::under`]): once that lease is released or
//! superseded, the store refuses the claim's effects and renewals. The
//! coordination pass then moves the task to its own claim, under the
//! scheduled runner holder ([`RUN_HOLDER`]) for [`TASK_LEASE`], and renews
//! it while it supervises across passes. A coordination claim is not bound
//! to one pass's lease, because binding it would move every task on every
//! pass and fill its ownership history. Instead, a coordination pass that
//! took over an expired lease moves every task it continues to a new fence
//! before acting, so the process it replaced holds only stale fences. Each
//! move is a recorded relinquish and adoption; unresolved effects stay with
//! the task and supervision reconciles them before anything else. A task
//! relinquished by a failed pass is adopted by the next coordination pass
//! under that pass's lease; that pass acts on it, and the pass after moves
//! it to an unbound claim. A task whose claim expired because no pass ran
//! is uncertain, and only a takeover continues it.
//!
//! A pass reads before it spends: when nothing is actionable it returns
//! [`Outcome::Idle`] without launching or messaging any worker.

use std::{fmt, str::FromStr, time::Duration};

use crate::{
    ConsumerId, ErrorClass, HolderId, TaskId,
    contracts::{
        Claimant, Clock, ConsumerFence, ExecutorKind, Fence, IssueNumber, LeaseTtl, MailboxError,
        Provenance, Repository, RetryPolicy, Role, TaskAuthority, Timestamp,
    },
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport, HeadLocation, IssueState},
    state::{ConsumerState, HouseStore, Lease, StateError, TaskRecord, TaskState},
    workflows::{
        coordination::{MailboxRoute, task_branch},
        pickup::{IssueRef, TaskTemplate, issue_task_id, stable_hash},
    },
};

mod coordinate;
mod gate;
mod pickup;
mod repair;

pub use coordinate::{CoordinateAction, CoordinatePass, Unroutable};
pub use gate::{GateAction, GatePass, MAX_GATE_PULL_REQUESTS};
pub use pickup::{MAX_READY_INSPECTED, PickupAction, PickupLabels, PickupPass, PickupSettings};
pub use repair::{RepairAction, RepairPass};

type Result<T> = std::result::Result<T, crate::Error>;

/// The holder every scheduled pass acts as.
pub const RUN_HOLDER: &str = "kitchen-run";

/// How long one pass holds its consumer lease. Every pass is bounded by a
/// few forge reads and backend calls per item, each under its own deadline.
pub const PASS_LEASE: Duration = Duration::from_secs(15 * 60);

/// How long a scheduled task claim lasts without a coordination pass
/// renewing it. Past it, the task is uncertain until a takeover.
pub const TASK_LEASE: Duration = Duration::from_secs(2 * 60 * 60);

/// Attempts a scheduled task may make.
const ATTEMPTS: u32 = 3;

/// Time budget of a scheduled task's attempts.
const RETRY_BUDGET: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A scheduled runner pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pass {
    /// Claim ready issues and launch their workers ([`PickupPass`]).
    Pickup,
    /// Read worker deliveries and supervise running tasks ([`CoordinatePass`]).
    Coordinate,
    /// Assess Kitchen pull requests for conflict repair ([`RepairPass`]).
    Repair,
    /// Evaluate Kitchen pull requests at their exact heads ([`GatePass`]).
    Gate,
}

impl Pass {
    /// Every pass.
    pub const ALL: [Self; 4] = [Self::Pickup, Self::Coordinate, Self::Repair, Self::Gate];

    /// The command and consumer name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pickup => "pickup",
            Self::Coordinate => "coordinate",
            Self::Repair => "repair",
            Self::Gate => "gate",
        }
    }

    /// The single-consumer scope of this pass. Coordination reads the whole
    /// house mailbox, so it has one scope per house; the others have one per
    /// repository.
    ///
    /// # Errors
    /// Never fails for valid inputs; the id syntax error is propagated.
    pub fn consumer(self, repository: &Repository) -> Result<ConsumerId> {
        Ok(match self {
            Self::Coordinate => coordinate::consumer()?,
            Self::Pickup | Self::Repair | Self::Gate => ConsumerId::new(&format!(
                "run-{}-{:016x}",
                self.as_str(),
                stable_hash(repository.as_str().as_bytes())
            ))?,
        })
    }
}

impl fmt::Display for Pass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Pass {
    type Err = RunError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|pass| pass.as_str() == value)
            .ok_or(RunError::UnknownPass)
    }
}

/// A pass refused its input. Input text is never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// No pass has this name.
    #[error("unknown pass; expected pickup, coordinate, repair, or gate")]
    UnknownPass,
    /// The repository is not one of the house's repositories.
    #[error("the repository is not one of the house's repositories")]
    RepositoryOutsideHouse,
    /// The house has several repositories and none was named.
    #[error("the house has several repositories; name one")]
    RepositoryAmbiguous,
    /// The house's worker backend needs host arguments the caller did not
    /// give; the text names them.
    #[error("this house's worker backend needs {0}")]
    BackendArguments(&'static str),
    /// The worker mailbox refused or could not be read.
    #[error("worker mailbox: {0}")]
    Mailbox(#[source] MailboxError),
}

impl RunError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::UnknownPass
            | Self::RepositoryOutsideHouse
            | Self::RepositoryAmbiguous
            | Self::BackendArguments(_) => ErrorClass::InvalidInput,
            Self::Mailbox(MailboxError::Fenced) => ErrorClass::Conflict,
            Self::Mailbox(MailboxError::Unavailable(_)) => ErrorClass::Execution,
        }
    }
}

/// The repository a pass serves: `named` when it is one of the house's
/// repositories, or the house's only repository when none is named.
///
/// # Errors
/// [`RunError::RepositoryOutsideHouse`] and [`RunError::RepositoryAmbiguous`].
pub fn pass_repository(
    house: &HouseConfig,
    named: Option<Repository>,
) -> std::result::Result<Repository, RunError> {
    match named {
        Some(repository) if house.repositories.contains(&repository) => Ok(repository),
        Some(_) => Err(RunError::RepositoryOutsideHouse),
        None => {
            let mut repositories = house.repositories.iter();
            match (repositories.next(), repositories.next()) {
                (Some(only), None) => Ok(only.clone()),
                (None, _) => Err(RunError::RepositoryOutsideHouse),
                (Some(_), Some(_)) => Err(RunError::RepositoryAmbiguous),
            }
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<A> {
    /// Nothing was actionable; no worker was launched or messaged.
    Idle,
    /// Another pass of this kind holds a live lease; this start did nothing.
    Busy,
    /// A previous pass's lease expired without a release or relinquish.
    /// Nothing ran; start again with a takeover to continue.
    OwnerUncertain {
        /// When the previous lease expired.
        expired_at: Timestamp,
    },
    /// The pass acted; one entry per item it decided on.
    Acted(Vec<A>),
}

/// The claimant every pass and scheduled task claim uses.
///
/// # Errors
/// Never fails; the holder syntax error is propagated.
pub fn run_claimant() -> Result<Claimant> {
    Ok(Claimant::scheduled(HolderId::new(RUN_HOLDER)?))
}

/// The result of taking a pass's consumer lease.
enum Start {
    Held(Lease),
    Busy,
    Uncertain(Timestamp),
}

/// Take `consumer`'s lease for one pass. An expired lease is taken over only
/// when `take_over` is set; the takeover is recorded.
fn start(
    store: &HouseStore,
    consumer: &ConsumerId,
    take_over: bool,
    now: Timestamp,
) -> Result<Start> {
    let claimant = run_claimant()?;
    let ttl = LeaseTtl::new(PASS_LEASE)?;
    match store.acquire_consumer(consumer, &claimant, ttl, now) {
        Ok(lease) => Ok(Start::Held(lease)),
        Err(crate::Error::State(StateError::ClaimHeld { .. })) => Ok(Start::Busy),
        Err(crate::Error::State(StateError::LeaseExpired { .. })) if take_over => Ok(Start::Held(
            store.take_over_consumer(consumer, &claimant, ttl, now)?,
        )),
        Err(crate::Error::State(StateError::LeaseExpired { expired_at })) => {
            Ok(Start::Uncertain(expired_at))
        }
        Err(error) => Err(error),
    }
}

/// Run `body` under `consumer`'s lease: release it after a pass that ended
/// without error, relinquish it after one that failed. When the relinquish
/// itself fails, the pass's own error is returned and the lease expires.
fn under_lease<A>(
    store: &HouseStore,
    consumer: &ConsumerId,
    take_over: bool,
    clock: &dyn Clock,
    body: impl FnOnce(Fence) -> Result<Vec<A>>,
) -> Result<Outcome<A>> {
    let lease = match start(store, consumer, take_over, clock.now())? {
        Start::Held(lease) => lease,
        Start::Busy => return Ok(Outcome::Busy),
        Start::Uncertain(expired_at) => return Ok(Outcome::OwnerUncertain { expired_at }),
    };
    finish(store, consumer, lease.fence(), clock, body(lease.fence()))
}

/// Extend the pass lease before the pass's next effect. A pass whose lease
/// was taken over, or ran out, stops here with the store's refusal.
fn renew(store: &HouseStore, consumer: &ConsumerId, fence: Fence, clock: &dyn Clock) -> Result<()> {
    store.renew_consumer(consumer, fence, LeaseTtl::new(PASS_LEASE)?, clock.now())?;
    Ok(())
}

/// End a pass holding `consumer` at `fence` with `result`.
fn finish<A>(
    store: &HouseStore,
    consumer: &ConsumerId,
    fence: Fence,
    clock: &dyn Clock,
    result: Result<Vec<A>>,
) -> Result<Outcome<A>> {
    match result {
        Ok(actions) => {
            store.release_consumer(consumer, fence, clock.now())?;
            Ok(if actions.is_empty() {
                Outcome::Idle
            } else {
                Outcome::Acted(actions)
            })
        }
        Err(error) => {
            // The pass's error is what the trigger must see; an unrecorded
            // relinquish leaves the lease to expire into an uncertain owner.
            let _ = store.relinquish_consumer(consumer, fence, clock.now());
            Err(error)
        }
    }
}

/// The task template scheduled pickup creates tasks from: the house's
/// standing grants delegated whole, its agent policy, the pinned
/// `provenance`, and the worker capabilities `route` needs.
///
/// # Errors
/// Rejects a house whose configuration or grants are invalid.
pub fn task_template(
    house: &HouseConfig,
    route: MailboxRoute,
    provenance: Provenance,
) -> Result<TaskTemplate> {
    let grants = house.authority()?;
    Ok(TaskTemplate {
        authority: TaskAuthority::delegate(&grants, house.grants.iter().cloned())?,
        retry: RetryPolicy::new(ATTEMPTS, RETRY_BUDGET)?,
        provenance,
        requires: crate::contracts::CapabilityRequirements::new().with(
            ExecutorKind::Worker,
            route.worker_requirements().iter().copied(),
        ),
        agents: house.agents.clone(),
    })
}

/// The issue a pickup task of `repository` was created for, confirmed by
/// deriving the task id again, or `None` for another kind of task.
fn issue_of(record: &TaskRecord, repository: &Repository) -> Option<IssueRef> {
    let id = &record.spec().id;
    if record.spec().role != Role::StationCook
        || record.spec().repository.as_ref() != Some(repository)
    {
        return None;
    }
    let (_, number) = id.as_str().rsplit_once('-')?;
    let issue = IssueRef {
        repository: repository.clone(),
        number: IssueNumber::new(number.parse().ok()?).ok()?,
    };
    issue_task_id(&issue)
        .ok()
        .filter(|derived| derived == id)
        .map(|_| issue)
}

/// The scheduled runner's live claim on `record`, if it holds one.
fn held_by_run(record: &TaskRecord, now: Timestamp) -> Option<&Lease> {
    match record.state() {
        TaskState::Claimed { lease }
            if lease.holder().as_str() == RUN_HOLDER && lease.is_live(now) =>
        {
            Some(lease)
        }
        TaskState::Claimed { .. } | TaskState::Open | TaskState::Settled { .. } => None,
    }
}

/// Whether the pass lease `bound` names is still held, current, and live:
/// that pass may still act on the tasks it claimed.
fn pass_current(store: &HouseStore, bound: &ConsumerFence, now: Timestamp) -> Result<bool> {
    Ok(store
        .consumer(&bound.consumer)?
        .is_some_and(|record| match record.state() {
            ConsumerState::Held { lease } => lease.fence() == bound.fence && lease.is_live(now),
            ConsumerState::Idle | ConsumerState::Relinquished { .. } => false,
        }))
}

/// Move `task`, claimed at `fence`, to a new claim for `claimant`: the
/// relinquish makes `fence` stale for every later change and effect, and the
/// claim records an adoption. Unresolved effects stay for the new owner to
/// reconcile. `None` when the task changed hands first; nothing was moved.
fn transfer(
    store: &HouseStore,
    task: &TaskId,
    fence: Fence,
    claimant: &Claimant,
    now: Timestamp,
) -> Result<Option<Fence>> {
    match store.relinquish(task, fence, now) {
        Ok(()) => {}
        Err(crate::Error::State(
            StateError::StaleFence { .. } | StateError::TaskSettled { .. },
        )) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    match store.claim(task, claimant, LeaseTtl::new(TASK_LEASE)?, now) {
        Ok(lease) => Ok(Some(lease.fence())),
        Err(crate::Error::State(
            StateError::ClaimHeld { .. }
            | StateError::LeaseExpired { .. }
            | StateError::TaskSettled { .. },
        )) => Ok(None),
        Err(error) => Err(error),
    }
}

/// A Kitchen pull request: an open pull request on the branch of a settled,
/// successful pickup task.
#[derive(Debug, Clone)]
struct KitchenPullRequest {
    task: TaskId,
    branch: crate::contracts::BranchName,
    pull_request: crate::integrations::github::PullRequest,
}

/// Largest number of settled tasks one repair or gate pass looks up.
const MAX_SETTLED_LOOKUPS: usize = 16;

/// The open pull requests of settled, successful pickup tasks in
/// `repository`, found through each issue's linked pull requests by the
/// task's branch. At most [`MAX_SETTLED_LOOKUPS`] tasks are looked up, newest
/// first; a lookup that fails stops the pass rather than reading as none.
fn kitchen_pull_requests<T: GitHubReadTransport>(
    store: &HouseStore,
    forge: &GitHubClient<T>,
    repository: &Repository,
) -> Result<Vec<KitchenPullRequest>> {
    let mut settled: Vec<(Timestamp, TaskRecord, IssueRef)> = store
        .tasks()?
        .into_iter()
        .filter_map(|record| match record.state() {
            TaskState::Settled {
                settlement: crate::contracts::Settlement::Succeeded,
                at,
            } => {
                let at = *at;
                issue_of(&record, repository).map(|issue| (at, record, issue))
            }
            TaskState::Settled { .. } | TaskState::Claimed { .. } | TaskState::Open => None,
        })
        .collect();
    settled.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut found = Vec::new();
    for (_, record, issue) in settled.into_iter().take(MAX_SETTLED_LOOKUPS) {
        let Some(branch) = task_branch(&record) else {
            continue;
        };
        let linked =
            super::known(forge.linked_pull_requests(store.house(), repository, issue.number))?;
        if let Some(linked) = linked.into_iter().find(|linked| {
            &linked.repository == repository
                && linked.pull_request.state == IssueState::Open
                && linked.pull_request.head.name == branch.as_str()
                && linked.pull_request.head_location(repository) == HeadLocation::SameRepository
        }) {
            found.push(KitchenPullRequest {
                task: record.spec().id.clone(),
                branch,
                pull_request: linked.pull_request,
            });
        }
    }
    Ok(found)
}
