//! The scheduled coordination pass: continue every scheduled task, read the
//! worker deliveries on the backend's mailbox route, and supervise each task
//! once.

use std::{
    cell::Cell, collections::BTreeMap, fmt, fmt::Write as _, num::NonZeroU32, time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    Outcome, RunError, TASK_LEASE,
    follow_up::{
        FollowUpVerdict, ThreadTarget, changed, record_changed, record_report, snapshot, target,
    },
    held_by_run, pass_current, run_claimant, scheduled_writer, transfer,
};
use crate::{
    ConsumerId, EffectName, TaskId, WorkflowId,
    contracts::{
        AttemptNumber, AttemptOutcome, Capability, CheckoutReport, Clock, CoordinatorMailbox,
        Delivery, Effect, Evidence, EvidenceKind, EvidenceSubject, EvidenceVerdict, ExternalRef,
        Fence, GitHubAction, GitHubMutation, IssueNumber, LeaseTtl, MailMessage, MessageKind,
        Operation, Repository, ResourceRef, Text, Timestamp, WorkerOutcome, WorkerState,
    },
    house::HouseConfig,
    integrations::github::{
        GitHubClient, GitHubExecutor, GitHubMutationTransport, HeadLocation, IntegrationError,
        IssueState, Observation,
    },
    state::{
        EffectPlan, EffectState, HouseMailbox, HouseStore, MailSender, MarkerFact, MarkerKey,
        MarkerSchema, MarkerSubject, OwnershipEvent, PostKind, StateError, TaskRecord, TaskState,
        WorkItem, WorkerPost, reconcile, run_effect,
    },
    workflows::{
        coordination::{
            Completion, Context, CoordinatorStart, MailboxRoute, Standing, Supervision,
            SupervisionInput, SupervisionPolicy, current_worker, launched_workers,
            start_coordinator_recording, supervise, task_branch,
        },
        known,
        push::last_pushed_head,
        tick::PassRun,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeldDelivery {
    message: ExternalRef,
    attempt: AttemptNumber,
    checkout: CheckoutReport,
    escalated: bool,
}

fn hold_workflow() -> Result<WorkflowId> {
    Ok(WorkflowId::new("delivery-hold")?)
}

fn hold_schema() -> Result<MarkerSchema> {
    Ok(MarkerSchema::new("delivery-hold", NonZeroU32::MIN)?)
}

fn hold_key(task: &TaskId, message: &ExternalRef) -> Result<MarkerKey> {
    Ok(MarkerKey {
        workflow: hold_workflow()?,
        item: WorkItem::Task { task: task.clone() },
        subject: MarkerSubject::Observation(message.clone()),
    })
}

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
    /// The house-scoped forge effect executor, required to complete a
    /// review-thread follow-up report.
    pub forge_executor: Option<&'a GitHubExecutor<T>>,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// Take over an expired pass lease, and expired scheduled task claims,
    /// instead of stopping.
    pub take_over: bool,
    /// The house tick's run this pass serves, if a tick started it: each
    /// task is recorded on it before the pass moves, claims, or supervises
    /// it, and it is renewed with the pass lease.
    pub tick: Option<&'a PassRun>,
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
    /// Optional merged-delivery recovery failed for this task. Mailbox
    /// processing and other tasks continue; the next pass may retry it.
    RecoveryFailed {
        /// The task.
        task: TaskId,
        /// The structured error rendered for the operator.
        reason: String,
    },
    /// The worker finished its work but has not delivered a pull request.
    AwaitingDelivery {
        /// The task awaiting a pull request.
        task: TaskId,
        /// The worker completion held for later processing.
        message: ExternalRef,
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
            Self::RecoveryFailed { task, reason } => {
                write!(
                    formatter,
                    "merged delivery recovery failed for task {task}: {reason}"
                )
            }
            Self::AwaitingDelivery { task, message } => {
                write!(
                    formatter,
                    "task {task} awaits pull request delivery after {message}"
                )
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

impl<T: GitHubMutationTransport> CoordinatePass<'_, T> {
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
        // A relinquished task is recorded on the tick run before this pass
        // adopts it, so a pass that stops right after the claim still names
        // it.
        let (lease, took_over) = match start_coordinator_recording(
            self.store,
            self.backend.descriptor(),
            &consumer,
            &claimant,
            ttl,
            now,
            |task| super::record(self.store, self.tick, task, self.clock),
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
        // Kitchen recorded ownership before this binding. Orca declares
        // RunTransfer partial because run-use records no relinquish itself,
        // but it must still bind this terminal before any mailbox read.
        if route == MailboxRoute::House
            || descriptor
                .capabilities
                .support(Capability::RunTransfer)
                .is_some()
        {
            mailbox.adopt_run().map_err(RunError::Mailbox)?;
        }
        let grants = super::standing_grants(self.house)?;
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
        for task in &owned {
            match still_owned(self.settle_merged(task)) {
                Ok(Some(Some(outcome))) => {
                    supervised.insert(task.task.clone(), outcome);
                }
                Ok(Some(None)) => {}
                Ok(None) => actions.push(CoordinateAction::Lost {
                    task: task.task.clone(),
                }),
                Err(error) => actions.push(CoordinateAction::RecoveryFailed {
                    task: task.task.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        let mut delivery = mailbox.next_delivery().map_err(RunError::Mailbox)?;
        for _ in 0..MAX_BATCHES {
            let Some(batch) = delivery else { break };
            super::renew(self.store, consumer, fence, self.tick, self.clock)?;
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
                super::renew(self.store, consumer, fence, self.tick, self.clock)?;
                if let Some(outcome) = self.resume_delivery(&ctx, &policy, owned)? {
                    supervised.insert(owned.task.clone(), outcome);
                    continue;
                }
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

    /// A merged linked PR can prove delivery even when the worker's mailbox
    /// report was lost. Require the last checked push of this task, the exact
    /// PR head, a merge commit, and the worker's successful settlement.
    fn settle_merged(&self, owned: &Owned) -> Result<Option<Supervision>> {
        let record = self.store.task(&owned.task)?;
        let (Some(number), Some(branch), Some(repository), Some(worker)) = (
            record.pull_request(),
            task_branch(&record),
            record.spec().repository.as_ref(),
            current_worker(&record),
        ) else {
            return Ok(None);
        };
        let Some(pushed) = last_pushed_head(self.store, &owned.task, &branch)? else {
            return Ok(None);
        };
        let Observation::Known(pr) =
            self.forge
                .pull_request(self.store.house(), repository, number)
        else {
            return Ok(None);
        };
        if pr.state != IssueState::Closed
            || !pr.merged
            || pr.head_location(repository) != HeadLocation::SameRepository
            || pr.head.name != branch.as_str()
            || pr.head.sha != pushed
        {
            return Ok(None);
        }
        let Some(merge_commit) = pr.merge_commit_sha else {
            return Ok(None);
        };
        if self.backend.observe_worker(&worker.worker)
            != Ok(WorkerState::Settled(WorkerOutcome::Succeeded))
        {
            return Ok(None);
        }
        let Some(attempt) =
            self.store
                .continue_attempt(&owned.task, owned.fence, self.clock.now())?
        else {
            return Ok(None);
        };
        let evidence = Evidence {
            kind: EvidenceKind::ForgeMerge(merge_commit),
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: pushed,
                base: None,
            },
            source: ExternalRef::new(&format!(
                "https://github.com/{repository}/pull/{}",
                number.get()
            ))?,
            observed_at: self.clock.now(),
        };
        if !record.evidence().items().iter().any(|prior| {
            prior.kind == evidence.kind
                && prior.subject == evidence.subject
                && prior.source == evidence.source
                && prior.verdict == evidence.verdict
        }) {
            self.store
                .record_evidence(&owned.task, owned.fence, evidence, self.clock.now())?;
        }
        let outcome = self.store.finish_attempt(
            &owned.task,
            owned.fence,
            attempt,
            AttemptOutcome::Succeeded,
            self.clock.now(),
        )?;
        Ok(Some(match outcome {
            crate::contracts::Disposition::Settled(settlement) => Supervision::Settled(settlement),
            crate::contracts::Disposition::RetryAvailable { remaining } => {
                Supervision::Retry { remaining }
            }
        }))
    }

    /// Continue every scheduled pickup task: the runner's live claims,
    /// relinquished tasks (adopted), and with `take_over`, its expired ones.
    /// A claim bound to a pickup pass that ended is moved to this pass; one
    /// whose pickup pass is still current is left to it. After this pass
    /// `took_over` an expired lease, every live claim is moved, so the
    /// replaced process holds only stale fences. A claim bound to this pass,
    /// at `consumer` and `pass_fence`, was adopted by
    /// [`start_coordinator_recording`], which recorded it on the tick run
    /// before the claim, and is this pass's own. Each other task this pass
    /// continues is recorded on its tick run before it is moved or claimed.
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
                super::record(self.store, self.tick, &task, self.clock)?;
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
                        super::record(self.store, self.tick, &task, self.clock)?;
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
                    super::record(self.store, self.tick, &task, self.clock)?;
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
                    if message.outcome == Some(WorkerOutcome::Succeeded)
                        && task_branch(&record).is_some()
                        && record.spec().repository.as_ref().is_some_and(|repository| {
                            super::issue_of(&record, repository).is_some()
                        })
                        && record.pull_request().is_none()
                        && self.branch_has_commits(&record)?
                    {
                        let key = hold_key(&owned.task, &message.id)?;
                        let fact = MarkerFact::workflow(
                            hold_schema()?,
                            &HeldDelivery {
                                message: message.id.clone(),
                                attempt: current_worker(&record)
                                    .ok_or(IntegrationError::Unknown)?
                                    .attempt,
                                checkout: message.checkout,
                                escalated: false,
                            },
                        )?;
                        self.store.record_task_marker_unless(
                            key,
                            fact,
                            &owned.task,
                            owned.fence,
                            self.clock.now(),
                            |_| Ok(None::<()>),
                        )?;
                        actions.push(CoordinateAction::AwaitingDelivery {
                            task: owned.task.clone(),
                            message: message.id.clone(),
                        });
                        supervised.insert(owned.task.clone(), Supervision::AwaitingLaunch);
                        continue;
                    }
                    let completion = match message.outcome {
                        Some(WorkerOutcome::Succeeded) => self.completion(&record, message)?,
                        Some(WorkerOutcome::Failed | WorkerOutcome::Cancelled) | None => None,
                    };
                    if message.outcome == Some(WorkerOutcome::Succeeded)
                        && snapshot(self.store, &owned.task)?.is_some()
                    {
                        self.complete_follow_up(owned, &record, message, completion.as_ref())?;
                    }
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

    /// Resume a report saved before acknowledging its mailbox batch.
    fn resume_delivery(
        &self,
        ctx: &Context<'_>,
        policy: &SupervisionPolicy,
        owned: &Owned,
    ) -> Result<Option<Supervision>> {
        let record = self.store.task(&owned.task)?;
        let marker = self
            .store
            .markers(&hold_workflow()?)?
            .into_iter()
            .rev()
            .find(|marker| {
                marker.key().item
                    == WorkItem::Task {
                        task: owned.task.clone(),
                    }
            });
        let Some(marker) = marker else {
            return Ok(None);
        };
        let mut held: HeldDelivery = marker.fact().decode(&hold_schema()?)?;
        if current_worker(&record).is_none_or(|worker| worker.attempt != held.attempt) {
            return Ok(None);
        }
        if record.pull_request().is_some() {
            let message = MailMessage {
                id: held.message,
                kind: MessageKind::WorkerDone,
                worker: None,
                outcome: Some(WorkerOutcome::Succeeded),
                subject: None,
                body: None,
                checkout: held.checkout,
            };
            let completion = self.completion(&record, &message)?;
            return still_owned(supervise(
                ctx,
                &owned.task,
                owned.fence,
                policy,
                &SupervisionInput {
                    completion: completion.as_ref(),
                    ..SupervisionInput::default()
                },
            ));
        }
        if !held.escalated
            && self.clock.now().saturating_since(marker.recorded_at()) >= IDLE_DEADLINE
        {
            let digest = Sha256::digest(held.message.as_str().as_bytes());
            let mut subject_text = String::from("Delivery overdue: ");
            for byte in digest.iter().take(16) {
                let _ = write!(subject_text, "{byte:02x}");
            }
            let subject = Text::new(&subject_text)?;
            let exists = self.store.open_questions(512)?.iter().any(|question| {
                question.task == owned.task && question.subject.as_ref() == Some(&subject)
            });
            if !exists {
                self.store
                    .continue_attempt(&owned.task, owned.fence, self.clock.now())?;
                self.store.post_mail(
                    &MailSender { task: owned.task.clone(), fence: owned.fence },
                    WorkerPost {
                        kind: PostKind::Question,
                        subject: Some(subject),
                        body: Text::new("Worker report is held because the pushed branch has commits but no pull request is linked. Check delivery or decide how to finish the task.")?,
                    },
                    self.clock.now(),
                )?;
            }
            held.escalated = true;
            self.store.supersede_task_marker(
                marker.key(),
                marker.fact(),
                MarkerFact::workflow(hold_schema()?, &held)?,
                &owned.task,
                owned.fence,
                self.clock.now(),
            )?;
        }
        Ok(Some(Supervision::AwaitingLaunch))
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
                return Ok(Some(
                    if matches!(record.state(), TaskState::Settled { .. }) {
                        Route::Stale(owned.task.clone())
                    } else {
                        Route::Owned(owned, Box::new(record))
                    },
                ));
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
        let head = match self
            .forge
            .branch_tip(self.store.house(), repository, &branch)
        {
            Observation::Known(head) => head,
            Observation::Unavailable(IntegrationError::NotFound) => return Ok(None),
            other => known(other)?,
        };
        Ok(Some(Completion {
            observed_branch: branch.as_str().to_owned(),
            requested: branch,
            report: Evidence {
                kind: EvidenceKind::WorkerReport(message.checkout),
                verdict: EvidenceVerdict::Pass,
                subject: EvidenceSubject { head, base: None },
                source: message.id.clone(),
                observed_at: self.clock.now(),
            },
            addressed: Vec::new(),
        }))
    }

    /// Record every disposition before posting. Each forge write has its own
    /// stable effect name and persisted intent, so a lost reply or resolution
    /// response is reconciled on redelivery before any later write.
    fn complete_follow_up(
        &self,
        owned: &Owned,
        record: &TaskRecord,
        message: &MailMessage,
        completion: Option<&Completion>,
    ) -> Result<()> {
        if !message.checkout.clean_and_pushed()
            || current_worker(record).is_none_or(|worker| {
                self.backend.observe_worker(&worker.worker)
                    != Ok(WorkerState::Settled(WorkerOutcome::Succeeded))
            })
        {
            return Err(RunError::FollowUpPushMissing.into());
        }
        self.store
            .continue_attempt(&owned.task, owned.fence, self.clock.now())?
            .ok_or(RunError::DispositionInvalid)?;
        let saved = snapshot(self.store, &owned.task)?.ok_or(RunError::DispositionInvalid)?;
        let report = record_report(
            self.store,
            &owned.task,
            owned.fence,
            message.body.as_ref(),
            self.clock.now(),
        )?;
        let head = &completion
            .ok_or(RunError::DispositionInvalid)?
            .report
            .subject
            .head;
        let repository = record
            .spec()
            .repository
            .as_ref()
            .ok_or(RunError::DispositionInvalid)?;
        let branch = task_branch(record).ok_or(RunError::DispositionInvalid)?;
        if last_pushed_head(self.store, &owned.task, &branch)?.as_ref() != Some(head) {
            return Err(RunError::FollowUpPushMissing.into());
        }
        let executor = self.forge_executor.ok_or(RunError::NoBackend)?;
        let pending = reconcile(self.store, executor, &owned.task, owned.fence, self.clock)?;
        if !pending.unresolved.is_empty() || !pending.foreign.is_empty() {
            return Err(RunError::FollowUpEffectUncertain.into());
        }
        let current_record = self.store.task(&owned.task)?;
        let pr = known(self.forge.pull_request(
            self.store.house(),
            repository,
            saved.pull_request,
        ))?;
        if pr.state != IssueState::Open
            || pr.merged
            || &pr.head.sha != head
            || pr.head.name != branch.as_str()
            || pr.head_location(repository) != HeadLocation::SameRepository
        {
            return Err(RunError::AttestationStaleHead.into());
        }
        let reviews = known(self.forge.reviews(
            self.store.house(),
            repository,
            saved.pull_request,
        ))?;
        if saved.reviews.iter().any(|expected| {
            !reviews.iter().any(|review| {
                review.id == expected.id
                    && review.state == crate::integrations::github::ReviewState::ChangesRequested
                    && review.commit_id == saved.source_head
                    && super::super::pickup::stable_hash(
                        review.body.as_deref().unwrap_or_default().as_bytes(),
                    ) == expected.body_digest
            })
        }) {
            return Err(RunError::DispositionInvalid.into());
        }
        let grants = super::standing_grants(self.house)?;
        for item in report.dispositions {
            let expected = saved
                .threads
                .iter()
                .find(|thread| thread.id == item.thread)
                .ok_or(RunError::DispositionInvalid)?;
            let explanation = item.reply.clone();
            let reply = GitHubMutation {
                repository: repository.clone(),
                action: GitHubAction::ReplyToReviewThread {
                    number: saved.pull_request,
                    expected_head: head.clone(),
                    thread: item.thread.clone(),
                    body: item.reply,
                },
            };
            let digest = super::super::pickup::stable_hash(item.thread.as_str().as_bytes());
            let resolved_here = current_record.effects().iter().any(|effect| {
                effect.name().as_str() == format!("thread-{digest:016x}-resolve")
                    && matches!(effect.state(), EffectState::Applied { .. })
            });
            if !self.thread_current(
                repository,
                saved.pull_request,
                expected,
                resolved_here,
                owned,
            )? {
                continue;
            }
            let mut changed = false;
            for (suffix, mutation) in std::iter::once(("reply", reply)).chain(
                (item.verdict == FollowUpVerdict::Fixed).then(|| {
                    (
                        "resolve",
                        GitHubMutation {
                            repository: repository.clone(),
                            action: GitHubAction::ResolveReviewThread {
                                number: saved.pull_request,
                                expected_head: head.clone(),
                                thread: item.thread.clone(),
                            },
                        },
                    )
                }),
            ) {
                if !self.thread_current(
                    repository,
                    saved.pull_request,
                    expected,
                    resolved_here,
                    owned,
                )? {
                    changed = true;
                    break;
                }
                let effect = executor.effect(mutation)?.into();
                let result = run_effect(
                    self.store,
                    executor,
                    &grants,
                    EffectPlan {
                        task: owned.task.clone(),
                        fence: owned.fence,
                        name: EffectName::new(&format!("thread-{digest:016x}-{suffix}"))?,
                        decided_at: self.store.task(&owned.task)?.evidence().revision(),
                        effect,
                        consent: None,
                        basis: None,
                    },
                    self.clock,
                )?;
                if !matches!(result.state(), EffectState::Applied { .. }) {
                    return Err(RunError::FollowUpEffectUncertain.into());
                }
            }
            if changed {
                continue;
            }
            if item.verdict == FollowUpVerdict::Declined {
                self.store.post_mail_unique(
                    &MailSender {
                        task: owned.task.clone(),
                        fence: owned.fence,
                    },
                    WorkerPost {
                        kind: PostKind::Question,
                        subject: Some(Text::new(&format!(
                            "Follow-up thread {digest:016x} declined"
                        ))?),
                        body: explanation,
                    },
                    self.clock.now(),
                )?;
            }
        }
        Ok(())
    }

    fn thread_current(
        &self,
        repository: &Repository,
        number: IssueNumber,
        expected: &ThreadTarget,
        resolved_here: bool,
        owned: &Owned,
    ) -> Result<bool> {
        if changed(self.store, &owned.task, &expected.id)? {
            return Ok(false);
        }
        let live = known(
            self.forge
                .follow_up_threads(self.store.house(), repository, number),
        )?;
        let current = live.iter().any(|thread| {
            thread.id == expected.id.as_str()
                && target(thread, self.forge.scope().requester().as_str())
                    .is_ok_and(|now| now == *expected)
                && (!thread.is_resolved || resolved_here)
        });
        if !current {
            record_changed(
                self.store,
                &owned.task,
                owned.fence,
                &expected.id,
                self.clock.now(),
            )?;
        }
        Ok(current)
    }

    /// A branch needs delivery only when the forge proves it has commits
    /// ahead of its launch base branch. A later ordinary advance of that
    /// base cannot make an unchanged worker branch appear ahead.
    fn branch_has_commits(&self, record: &TaskRecord) -> Result<bool> {
        let (Some(branch), Some(repository)) = (task_branch(record), &record.spec().repository)
        else {
            return Ok(false);
        };
        let house = self.store.house();
        let base = if super::super::coordination::BranchFact::Stacked.holds(record, &branch) {
            let attempt = current_worker(record)
                .ok_or(crate::integrations::github::IntegrationError::Unknown)?
                .attempt;
            let branch = record.effects().iter().rev().find_map(|effect| {
                if effect.request().attempt() != attempt {
                    return None;
                }
                let Effect::Worker(Operation::LaunchWorker { brief, .. }) =
                    effect.request().effect()
                else {
                    return None;
                };
                brief.as_str().lines().find_map(|line| {
                    line.strip_prefix("Base: stack layer ")
                        .and_then(|line| line.split_once(" on `"))
                        .and_then(|(_, rest)| rest.split_once('`'))
                        .and_then(|(branch, _)| crate::contracts::BranchName::new(branch).ok())
                })
            });
            branch.ok_or(crate::integrations::github::IntegrationError::Unknown)?
        } else {
            let info = known(self.forge.repository(house, repository))?;
            crate::contracts::BranchName::new(&info.default_branch)?
        };
        let base_head = known(self.forge.branch_tip(house, repository, &base))?;
        let head = match self.forge.branch_tip(house, repository, &branch) {
            Observation::Known(head) => head,
            Observation::Unavailable(IntegrationError::NotFound) => return Ok(false),
            other => known(other)?,
        };
        if head == base_head {
            return Ok(false);
        }
        Ok(known(self.forge.compare(house, repository, &base_head, &head))?.ahead_by > 0)
    }
}

/// Whether `record` is a scheduled writer task: one made from an issue of
/// its repository, or a repair round a scheduled repair pass created.
fn scheduled(record: &TaskRecord) -> bool {
    record
        .spec()
        .repository
        .as_ref()
        .is_some_and(|repository| scheduled_writer(record, repository))
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
