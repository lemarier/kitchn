//! The scheduled coordination pass: continue every scheduled task, read the
//! worker deliveries on the backend's mailbox route, and supervise each task
//! once.

use std::{collections::BTreeMap, fmt, time::Duration};

use super::{
    Outcome, RunError, TASK_LEASE, held_by_run, issue_of, pass_current, run_claimant, transfer,
};
use crate::{
    ConsumerId, TaskId,
    contracts::{
        Capability, Clock, CoordinatorMailbox, Delivery, Evidence, EvidenceKind, EvidenceSubject,
        EvidenceVerdict, ExternalRef, Fence, LeaseTtl, MailMessage, MessageKind, Timestamp,
        WorkerOutcome,
    },
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport},
    state::{HouseMailbox, HouseStore, OwnershipEvent, StateError, TaskRecord, TaskState},
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
    /// A delivery could not be handled completely and stays unacknowledged:
    /// it holds a message from a worker of no task this pass owns, an
    /// unreadable message, or a report whose task has not settled yet.
    Unacknowledged {
        /// The delivery.
        delivery: ExternalRef,
    },
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
    /// No task this pass may act for.
    Unknown,
}

/// A scheduled task this pass owns.
struct Owned {
    task: TaskId,
    fence: Fence,
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
        let (mut actions, owned) = self.own(took_over)?;
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
            let handled =
                self.handle(&ctx, &policy, &owned, &batch, &mut supervised, &mut actions)?;
            if !handled {
                actions.push(CoordinateAction::Unacknowledged { delivery: batch.id });
                break;
            }
            delivery = mailbox.acknowledge(&batch.id).map_err(RunError::Mailbox)?;
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
    /// replaced process holds only stale fences.
    fn own(&self, took_over: bool) -> Result<(Vec<CoordinateAction>, Vec<Owned>)> {
        let claimant = run_claimant()?;
        let ttl = LeaseTtl::new(TASK_LEASE)?;
        let now = self.clock.now();
        let mut actions = Vec::new();
        let mut owned = Vec::new();
        for record in self.store.tasks()? {
            let task = record.spec().id.clone();
            let scheduled = record
                .spec()
                .repository
                .as_ref()
                .is_some_and(|repository| issue_of(&record, repository).is_some());
            if !scheduled {
                continue;
            }
            if let Some(lease) = held_by_run(&record, now) {
                let bound = lease.consumer();
                if let Some(bound) = bound
                    && pass_current(self.store, bound, now)?
                {
                    continue;
                }
                if bound.is_none() && !took_over {
                    owned.push(Owned {
                        task,
                        fence: lease.fence(),
                    });
                } else if let Some(fence) =
                    transfer(self.store, &task, lease.fence(), &claimant, now)?
                {
                    actions.push(CoordinateAction::Moved { task: task.clone() });
                    owned.push(Owned { task, fence });
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
                        owned.push(Owned {
                            task,
                            fence: lease.fence(),
                        });
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
                            owned.push(Owned {
                                task,
                                fence: lease.fence(),
                            });
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

    /// Handle every actionable message of `batch`; true when the batch may
    /// be acknowledged. A report is handled once supervision settled or
    /// ended its attempt; until then the batch stays for the next pass.
    fn handle(
        &self,
        ctx: &Context<'_>,
        policy: &SupervisionPolicy,
        owned: &[Owned],
        batch: &Delivery,
        supervised: &mut BTreeMap<TaskId, Supervision>,
        actions: &mut Vec<CoordinateAction>,
    ) -> Result<bool> {
        let mut handled = batch.unreadable == 0;
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
                Route::Unknown => {
                    handled = false;
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
        Ok(handled)
    }

    /// Where `message` belongs: the owned task whose current worker sent
    /// it, or a worker whose task settled or whose attempt was replaced.
    fn route<'o>(&self, owned: &'o [Owned], message: &MailMessage) -> Result<Route<'o>> {
        let Some(worker) = &message.worker else {
            return Ok(Route::Unknown);
        };
        for owned in owned {
            let record = self.store.task(&owned.task)?;
            if current_worker(&record).is_some_and(|view| &view.worker == worker) {
                return Ok(Route::Owned(owned, Box::new(record)));
            }
        }
        for record in self.store.tasks()? {
            let settled = matches!(record.state(), TaskState::Settled { .. });
            let replaced = owned.iter().any(|owned| owned.task == record.spec().id);
            if (settled || replaced) && launched_workers(&record).any(|view| &view.worker == worker)
            {
                return Ok(Route::Stale(record.spec().id.clone()));
            }
        }
        Ok(Route::Unknown)
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

/// A supervision step's outcome, or `None` when the task's fence went stale
/// because another pass moved the task during this one.
fn still_owned(step: Result<Supervision>) -> Result<Option<Supervision>> {
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
