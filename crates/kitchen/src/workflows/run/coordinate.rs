//! The scheduled coordination pass: continue every scheduled task, read the
//! worker deliveries on the backend's mailbox route, and supervise each task
//! once.

use std::{cell::Cell, collections::BTreeMap, fmt, time::Duration};

use super::{
    Outcome, RunError, TASK_LEASE, held_by_run, issue_of, pass_current, run_claimant, transfer,
};
use crate::{
    ConsumerId, TaskId,
    contracts::{
        Capability, Clock, CoordinatorMailbox, Delivery, Effect, Evidence, EvidenceKind,
        EvidenceSubject, EvidenceVerdict, ExternalRef, Fence, LeaseTtl, MailMessage, MessageKind,
        Operation, ResourceRef, Timestamp, WorkerOutcome,
    },
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport},
    state::{
        HouseMailbox, HouseStore, OwnershipEvent, StateError, TaskRecord, TaskState, reconcile,
    },
    workflows::{
        coordination::{
            Completion, Context, CoordinatorStart, MailboxRoute, Standing, Supervision,
            SupervisionInput, SupervisionPolicy, current_worker, launched_workers,
            start_coordinator, supervise, task_branch,
        },
        known,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Mailbox batches one pass reads.
const MAX_BATCHES: usize = 4;

/// How long a launched worker may stay unready.
const READINESS_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// How long a question may wait before escalation.
const QUESTION_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// How long a worker may sit idle at its prompt before it counts as stalled.
const IDLE_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// The coordination consumer: one per house.
pub(super) fn consumer() -> Result<ConsumerId> {
    Ok(ConsumerId::new("run-coordinate")?)
}

/// One scheduled coordination pass over every scheduled task of the house.
pub struct CoordinatePass<'a, T> {
    /// The house store.
    pub store: &'a HouseStore,
    /// The house configuration.
    pub house: &'a HouseConfig,
    /// The house's worker backend, which also carries deliveries when it
    /// declares them.
    pub backend: &'a dyn CoordinatorMailbox,
    /// The house's forge reads, for the head a completed worker pushed.
    pub forge: &'a GitHubClient<T>,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// Take over an expired pass lease, and expired scheduled task claims,
    /// instead of stopping.
    pub take_over: bool,
}

/// What a coordination pass did about one task or message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinateAction {
    /// A relinquished scheduled task was adopted.
    Adopted {
        /// The task.
        task: TaskId,
    },
    /// A scheduled task was moved to this pass's claim: from a pickup pass
    /// that ended, or, after this pass took over an expired lease, from the
    /// pass it replaced. The earlier claim's fence is stale.
    Moved {
        /// The task.
        task: TaskId,
    },
    /// The task changed hands before this pass could move it, or during the
    /// pass; this pass does not act on it.
    Lost {
        /// The task.
        task: TaskId,
    },
    /// An expired scheduled task claim was taken over.
    TakenOver {
        /// The task.
        task: TaskId,
    },
    /// A scheduled task claim expired; nothing was done with the task.
    Uncertain {
        /// The task.
        task: TaskId,
        /// When the claim expired.
        expired_at: Timestamp,
    },
    /// One supervision step.
    Supervised {
        /// The task.
        task: TaskId,
        /// Its result.
        outcome: Supervision,
    },
    /// A worker asked a question, which waits for a person: on the house
    /// route through `kitchn mailbox reply`, otherwise through the backend.
    Question {
        /// The task.
        task: TaskId,
        /// The message.
        message: ExternalRef,
    },
    /// A worker escalated.
    Escalation {
        /// The task.
        task: TaskId,
        /// The message.
        message: ExternalRef,
    },
    /// A message from a worker whose task already settled, or whose attempt
    /// was replaced; it is acknowledged without acting.
    Stale {
        /// The task.
        task: TaskId,
        /// The message.
        message: ExternalRef,
    },
    /// A delivery held rows Kitchen could not read. Waiting cannot make
    /// them readable, so the delivery was acknowledged without acting on
    /// them.
    Unreadable {
        /// The delivery.
        delivery: ExternalRef,
        /// How many rows could not be read.
        rows: usize,
    },
    /// A message no task can take, acknowledged without acting.
    Unroutable {
        /// The message.
        message: ExternalRef,
        /// Why no task can take it.
        reason: Unroutable,
    },
    /// A delivery could not be handled yet and stays unacknowledged: it
    /// holds a message from a worker of a scheduled task this pass does not
    /// own, from a worker a scheduled task's unresolved launch may have
    /// started, or a report whose attempt has not ended.
    Unacknowledged {
        /// The delivery.
        delivery: ExternalRef,
    },
}

/// Why a delivered message reaches no task. Waiting cannot change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unroutable {
    /// The backend named no sending worker.
    NoWorker,
    /// No stored task launched the sending worker, and no scheduled task
    /// has a launch whose outcome is unresolved.
    UnknownWorker {
        /// The worker.
        worker: ResourceRef,
    },
    /// The sending worker belongs to a task scheduled passes do not run.
    Unscheduled {
        /// The task.
        task: TaskId,
    },
}

impl fmt::Display for Unroutable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWorker => formatter.write_str("no sending worker"),
            Self::UnknownWorker { worker } => {
                write!(formatter, "no task launched worker {}", worker.handle)
            }
            Self::Unscheduled { task } => write!(formatter, "task {task} is not scheduled"),
        }
    }
}

impl fmt::Display for CoordinateAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adopted { task } => write!(formatter, "adopted task {task}"),
            Self::Moved { task } => write!(formatter, "moved task {task} to this pass"),
            Self::Lost { task } => write!(formatter, "task {task} changed hands during the pass"),
            Self::TakenOver { task } => write!(formatter, "took over task {task}"),
            Self::Uncertain { task, expired_at } => write!(
                formatter,
                "uncertain task {task}: claim expired at {}",
                expired_at.as_unix_millis()
            ),
            Self::Supervised { task, outcome } => {
                write!(formatter, "supervised task {task}: {outcome:?}")
            }
            Self::Question { task, message } => {
                write!(
                    formatter,
                    "question {message} from task {task} waits for a person"
                )
            }
            Self::Escalation { task, message } => {
                write!(formatter, "escalation {message} from task {task}")
            }
            Self::Stale { task, message } => {
                write!(formatter, "stale message {message} from task {task}")
            }
            Self::Unreadable { delivery, rows } => {
                write!(formatter, "delivery {delivery} had {rows} unreadable rows")
            }
            Self::Unroutable { message, reason } => {
                write!(formatter, "unroutable message {message}: {reason}")
            }
            Self::Unacknowledged { delivery } => {
                write!(formatter, "delivery {delivery} kept unacknowledged")
            }
        }
    }
}

/// Where a message belongs.
enum Route<'o> {
    /// The current worker of a task this pass owns.
    Owned(&'o Owned, Box<TaskRecord>),
    /// A worker of a settled task, or an earlier worker of an owned task:
    /// nothing is left to act on.
    Stale(TaskId),
    /// A worker of a scheduled task this pass does not own: another pass
    /// holds it, or its claim expired. Or a worker no stored launch created
    /// while a scheduled task's launch is unresolved. A later pass may own
    /// it.
    Pending,
    /// No task can take the message.
    Unroutable(Unroutable),
}

/// A scheduled task this pass owns.
struct Owned {
    task: TaskId,
    fence: Fence,
    /// Whether routing already reconciled the task's effects this pass.
    reconciled: Cell<bool>,
}

impl Owned {
    const fn new(task: TaskId, fence: Fence) -> Self {
        Self {
            task,
            fence,
            reconciled: Cell::new(false),
        }
    }
}

impl<T: GitHubReadTransport> CoordinatePass<'_, T> {
    /// Run one pass under the house's coordination lease.
    ///
    /// # Errors
    /// Refuses, before taking the lease, a backend of another house or one
    /// that does not support supervision on its mailbox route. Returns
    /// mailbox, forge, store, and authority failures.
    pub fn run(&self) -> Result<Outcome<CoordinateAction>> {
        let consumer = consumer()?;
        let claimant = run_claimant()?;
        let now = self.clock.now();
        let ttl = LeaseTtl::new(super::PASS_LEASE)?;
        let (lease, took_over) = match start_coordinator(
            self.store,
            self.backend.descriptor(),
            &consumer,
            &claimant,
            ttl,
            now,
        )? {
            CoordinatorStart::Fresh(lease) | CoordinatorStart::Adopted { lease, .. } => {
                (lease, false)
            }
            CoordinatorStart::Busy => return Ok(Outcome::Busy),
            CoordinatorStart::Uncertain { .. } if self.take_over => (
                self.store
                    .take_over_consumer(&consumer, &claimant, ttl, now)?,
                true,
            ),
            CoordinatorStart::Uncertain { expired_at } => {
                return Ok(Outcome::OwnerUncertain { expired_at });
            }
        };
        let fence = lease.fence();
        super::finish(
            self.store,
            &consumer,
            fence,
            self.clock,
            self.pass(&consumer, fence, took_over),
        )
    }

    fn pass(
        &self,
        consumer: &ConsumerId,
        fence: Fence,
        took_over: bool,
    ) -> Result<Vec<CoordinateAction>> {
        // The mailbox is read even without owned tasks, so a late message
        // from a settled task's worker cannot hold it up.
        let (mut actions, owned) = self.own(consumer, fence, took_over)?;
        let descriptor = self.backend.descriptor();
        let route = MailboxRoute::select(descriptor);
        let house_mailbox;
        let mailbox: &dyn CoordinatorMailbox = match route {
            MailboxRoute::Backend => self.backend,
            MailboxRoute::House => {
                house_mailbox = HouseMailbox::new(
                    self.store,
                    self.backend,
                    self.clock,
                    consumer.clone(),
                    fence,
                )?;
                &house_mailbox
            }
        };
        // This process is a new coordinator instance: it fences the previous
        // reader before reading. A backend without run transfer has no other
        // reader to fence.
        if route == MailboxRoute::House || descriptor.capabilities.supports(Capability::RunTransfer)
        {
            mailbox.adopt_run().map_err(RunError::Mailbox)?;
        }
        let grants = self.house.authority()?;
        let ctx = Context {
            store: self.store,
            backend: self.backend,
            grants: &grants,
            clock: self.clock,
            consent: &Standing,
        };
        let policy = SupervisionPolicy {
            readiness_deadline: READINESS_DEADLINE,
            question_deadline: QUESTION_DEADLINE,
            idle_deadline: IDLE_DEADLINE,
            claim_ttl: LeaseTtl::new(TASK_LEASE)?,
        };
        let mut supervised: BTreeMap<TaskId, Supervision> = BTreeMap::new();
        let mut delivery = mailbox.next_delivery().map_err(RunError::Mailbox)?;
        for _ in 0..MAX_BATCHES {
            let Some(batch) = delivery else { break };
            super::renew(self.store, consumer, fence, self.clock)?;
            let Some(dropped) =
                self.handle(&ctx, &policy, &owned, &batch, &mut supervised, &mut actions)?
            else {
                actions.push(CoordinateAction::Unacknowledged { delivery: batch.id });
                break;
            };
            delivery = mailbox.acknowledge(&batch.id).map_err(RunError::Mailbox)?;
            actions.extend(dropped);
        }
        for owned in &owned {
            if !supervised.contains_key(&owned.task) {
                super::renew(self.store, consumer, fence, self.clock)?;
                match still_owned(supervise(
                    &ctx,
                    &owned.task,
                    owned.fence,
                    &policy,
                    &SupervisionInput::default(),
                ))? {
                    Some(outcome) => {
                        supervised.insert(owned.task.clone(), outcome);
                    }
                    None => actions.push(CoordinateAction::Lost {
                        task: owned.task.clone(),
                    }),
                }
            }
        }
        actions.extend(
            supervised
                .into_iter()
                .map(|(task, outcome)| CoordinateAction::Supervised { task, outcome }),
        );
        Ok(actions)
    }

    /// Continue every scheduled pickup task: the runner's live claims,
    /// relinquished tasks (adopted), and with `take_over`, its expired ones.
    /// A claim bound to a pickup pass that ended is moved to this pass; one
    /// whose pickup pass is still current is left to it. After this pass
    /// `took_over` an expired lease, every live claim is moved, so the
    /// replaced process holds only stale fences. A claim bound to this pass,
    /// at `consumer` and `pass_fence`, was adopted by [`start_coordinator`]
    /// and is this pass's own.
    fn own(
        &self,
        consumer: &ConsumerId,
        pass_fence: Fence,
        took_over: bool,
    ) -> Result<(Vec<CoordinateAction>, Vec<Owned>)> {
        let claimant = run_claimant()?;
        let ttl = LeaseTtl::new(TASK_LEASE)?;
        let now = self.clock.now();
        let mut actions = Vec::new();
        let mut owned = Vec::new();
        for record in self.store.tasks()? {
            let task = record.spec().id.clone();
            if !scheduled(&record) {
                continue;
            }
            if let Some(lease) = held_by_run(&record, now) {
                let bound = lease.consumer();
                if bound
                    .is_some_and(|bound| &bound.consumer == consumer && bound.fence == pass_fence)
                {
                    actions.push(CoordinateAction::Adopted { task: task.clone() });
                    owned.push(Owned::new(task, lease.fence()));
                    continue;
                }
                if let Some(bound) = bound
                    && pass_current(self.store, bound, now)?
                {
                    continue;
                }
                if bound.is_none() && !took_over {
                    owned.push(Owned::new(task, lease.fence()));
                } else if let Some(fence) =
                    transfer(self.store, &task, lease.fence(), &claimant, now)?
                {
                    actions.push(CoordinateAction::Moved { task: task.clone() });
                    owned.push(Owned::new(task, fence));
                } else {
                    actions.push(CoordinateAction::Lost { task });
                }
                continue;
            }
            match record.state() {
                TaskState::Claimed { lease } if lease.holder().as_str() == super::RUN_HOLDER => {
                    if self.take_over {
                        let lease = self.store.take_over(&task, &claimant, ttl, now)?;
                        actions.push(CoordinateAction::TakenOver { task: task.clone() });
                        owned.push(Owned::new(task, lease.fence()));
                    } else {
                        actions.push(CoordinateAction::Uncertain {
                            task,
                            expired_at: lease.expires_at(),
                        });
                    }
                }
                TaskState::Open
                    if matches!(
                        record.ownership().last(),
                        Some(OwnershipEvent::Relinquished { .. })
                    ) =>
                {
                    match self.store.claim(&task, &claimant, ttl, now) {
                        Ok(lease) => {
                            actions.push(CoordinateAction::Adopted { task: task.clone() });
                            owned.push(Owned::new(task, lease.fence()));
                        }
                        Err(crate::Error::State(
                            StateError::ClaimHeld { .. } | StateError::LeaseExpired { .. },
                        )) => {}
                        Err(error) => return Err(error),
                    }
                }
                TaskState::Claimed { .. } | TaskState::Open | TaskState::Settled { .. } => {}
            }
        }
        Ok((actions, owned))
    }

    /// Handle every actionable message of `batch`. Returns the actions to
    /// record once the batch is acknowledged, for content no task can take,
    /// or `None` while something in it is pending: a message for a
    /// scheduled task this pass does not own or from a worker an unresolved
    /// launch may have started, or a report whose attempt supervision has
    /// not ended. A pending batch stays for the next pass.
    fn handle(
        &self,
        ctx: &Context<'_>,
        policy: &SupervisionPolicy,
        owned: &[Owned],
        batch: &Delivery,
        supervised: &mut BTreeMap<TaskId, Supervision>,
        actions: &mut Vec<CoordinateAction>,
    ) -> Result<Option<Vec<CoordinateAction>>> {
        let mut handled = true;
        let mut dropped = Vec::new();
        if batch.unreadable > 0 {
            dropped.push(CoordinateAction::Unreadable {
                delivery: batch.id.clone(),
                rows: batch.unreadable,
            });
        }
        for message in batch.actionable() {
            let (owned, record) = match self.route(owned, message)? {
                Route::Owned(owned, record) => (owned, *record),
                Route::Stale(task) => {
                    actions.push(CoordinateAction::Stale {
                        task,
                        message: message.id.clone(),
                    });
                    continue;
                }
                Route::Pending => {
                    handled = false;
                    continue;
                }
                Route::Unroutable(reason) => {
                    dropped.push(CoordinateAction::Unroutable {
                        message: message.id.clone(),
                        reason,
                    });
                    continue;
                }
            };
            match message.kind {
                MessageKind::Question => actions.push(CoordinateAction::Question {
                    task: owned.task.clone(),
                    message: message.id.clone(),
                }),
                MessageKind::Escalation => actions.push(CoordinateAction::Escalation {
                    task: owned.task.clone(),
                    message: message.id.clone(),
                }),
                MessageKind::WorkerDone => {
                    let completion = match message.outcome {
                        Some(WorkerOutcome::Succeeded) => self.completion(&record, message)?,
                        Some(WorkerOutcome::Failed | WorkerOutcome::Cancelled) | None => None,
                    };
                    let Some(outcome) = still_owned(supervise(
                        ctx,
                        &owned.task,
                        owned.fence,
                        policy,
                        &SupervisionInput {
                            completion: completion.as_ref(),
                            ..SupervisionInput::default()
                        },
                    ))?
                    else {
                        handled = false;
                        actions.push(CoordinateAction::Lost {
                            task: owned.task.clone(),
                        });
                        continue;
                    };
                    handled &= ends_attempt(&outcome);
                    supervised.insert(owned.task.clone(), outcome);
                }
                MessageKind::Heartbeat | MessageKind::Status | MessageKind::Other => {}
            }
        }
        Ok(handled.then_some(dropped))
    }

    /// Where `message` belongs: the owned task whose current worker sent
    /// it, a worker whose task settled or whose attempt was replaced, or a
    /// worker of a task this pass does not own.
    ///
    /// A worker no applied launch created may come from a launch whose
    /// outcome is not recorded yet: a pickup pass can start the worker
    /// before it stores the receipt. This pass reconciles those launches on
    /// the tasks it owns, once per pass, and routes again. While a scheduled task still has
    /// one unresolved, the message is pending; only when none has is the
    /// worker unknown.
    fn route<'o>(&self, owned: &'o [Owned], message: &MailMessage) -> Result<Route<'o>> {
        let Some(worker) = &message.worker else {
            return Ok(Route::Unroutable(Unroutable::NoWorker));
        };
        if let Some(route) = self.launched_by(owned, worker)? {
            return Ok(route);
        }
        let mut reconciled = false;
        for owned in owned {
            if owned.reconciled.get() {
                continue;
            }
            let record = self.store.task(&owned.task)?;
            if !matches!(record.state(), TaskState::Settled { .. }) && unresolved_launch(&record) {
                // Once per pass: a lookup that cannot tell now will not tell
                // for the next message either. A stale fence means another
                // pass took the task; the scan below still sees its launch.
                owned.reconciled.set(true);
                still_owned(reconcile(
                    self.store,
                    self.backend,
                    &owned.task,
                    owned.fence,
                    self.clock,
                ))?;
                reconciled = true;
            }
        }
        if reconciled && let Some(route) = self.launched_by(owned, worker)? {
            return Ok(route);
        }
        let unresolved = self.store.tasks()?.iter().any(|record| {
            scheduled(record)
                && !matches!(record.state(), TaskState::Settled { .. })
                && unresolved_launch(record)
        });
        Ok(if unresolved {
            Route::Pending
        } else {
            Route::Unroutable(Unroutable::UnknownWorker {
                worker: worker.clone(),
            })
        })
    }

    /// The route of `worker` when an applied launch created it.
    fn launched_by<'o>(
        &self,
        owned: &'o [Owned],
        worker: &ResourceRef,
    ) -> Result<Option<Route<'o>>> {
        for owned in owned {
            let record = self.store.task(&owned.task)?;
            if current_worker(&record).is_some_and(|view| &view.worker == worker) {
                return Ok(Some(Route::Owned(owned, Box::new(record))));
            }
        }
        for record in self.store.tasks()? {
            if !launched_workers(&record).any(|view| &view.worker == worker) {
                continue;
            }
            let task = record.spec().id.clone();
            let settled = matches!(record.state(), TaskState::Settled { .. });
            let replaced = owned.iter().any(|owned| owned.task == task);
            return Ok(Some(if settled || replaced {
                Route::Stale(task)
            } else if scheduled(&record) {
                Route::Pending
            } else {
                Route::Unroutable(Unroutable::Unscheduled { task })
            }));
        }
        Ok(None)
    }

    /// The completion a successful report stands for: the head the forge
    /// shows on the task's branch now, attested by the report message.
    /// `None` when the task has no launched branch.
    fn completion(&self, record: &TaskRecord, message: &MailMessage) -> Result<Option<Completion>> {
        let (Some(branch), Some(repository)) = (task_branch(record), &record.spec().repository)
        else {
            return Ok(None);
        };
        let head = known(
            self.forge
                .branch_tip(self.store.house(), repository, &branch),
        )?;
        Ok(Some(Completion {
            observed_branch: branch.as_str().to_owned(),
            requested: branch,
            report: Evidence {
                kind: EvidenceKind::WorkerReport,
                verdict: EvidenceVerdict::Pass,
                subject: EvidenceSubject { head, base: None },
                source: message.id.clone(),
                observed_at: self.clock.now(),
            },
            addressed: Vec::new(),
        }))
    }
}

/// Whether `record` is a scheduled pickup task: one made from an issue of
/// its repository.
fn scheduled(record: &TaskRecord) -> bool {
    record
        .spec()
        .repository
        .as_ref()
        .is_some_and(|repository| issue_of(record, repository).is_some())
}

/// Whether a launch of `record` has no recorded outcome yet: it may have
/// started a worker whose receipt is not stored.
fn unresolved_launch(record: &TaskRecord) -> bool {
    record.unresolved_effects().any(|effect| {
        matches!(
            effect.request().effect(),
            Effect::Worker(Operation::LaunchWorker { .. })
        )
    })
}

/// A step's result, or `None` when the task's fence went stale because
/// another pass moved the task during this one.
fn still_owned<S>(step: Result<S>) -> Result<Option<S>> {
    match step {
        Ok(outcome) => Ok(Some(outcome)),
        Err(crate::Error::State(StateError::StaleFence { .. })) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether supervision acted on a report: it settled the task or ended the
/// worker's attempt, or needs a decision.
const fn ends_attempt(outcome: &Supervision) -> bool {
    match outcome {
        Supervision::Settled(_)
        | Supervision::Retry { .. }
        | Supervision::FollowUpRound { .. }
        | Supervision::Escalate(_)
        | Supervision::LaunchStalled { .. }
        | Supervision::IdleStopped { .. }
        | Supervision::Replace { .. }
        | Supervision::EnvironmentFailure { .. } => true,
        Supervision::Reconciling { .. }
        | Supervision::AwaitingLaunch
        | Supervision::Running(_)
        | Supervision::PersonOwnsTerminal
        | Supervision::Unobservable
        | Supervision::WorkerMissing
        | Supervision::StartUnconfirmed
        | Supervision::Parked { .. }
        | Supervision::Resumed => false,
    }
}
