//! Supervised coordination: worker launch, supervision until settlement,
//! worker questions, and coordinator relinquish and adoption.
//!
//! Lifecycle and authority are enforced here; semantic judgment (answering
//! a question, deciding whether a person must decide) comes from scoped
//! agents as typed input. Every external effect goes through
//! [`crate::state::run_effect`], so intent is persisted first, an uncertain
//! launch is never repeated, and a superseded coordinator cannot act.
//!
//! Two inputs are interim seams until #4's follow-up contract lands:
//! [`TerminalControl`] (user takeover) is supplied by the caller rather than
//! observed through [`WorkerBackend`], and the exact branch travels in the
//! brief and is verified after the fact with
//! [`crate::workflows::pickup::BranchName::verify_observed`].

use std::time::Duration;

use crate::{
    ConsumerId, EffectName, ErrorClass, TaskId,
    contracts::{
        AskKind, AskRisk, AttemptNumber, AttemptOutcome, AttemptStart, BackendDescriptor,
        Capability, Claimant, Clock, CommitId, Consent, ContractError, DecisionBinding,
        DecisionOwner, Disposition, Effect, EffectExecutor, Evidence, EvidenceKind,
        EvidenceRevision, EvidenceVerdict, ExternalRef, FailureClass, Fence, HouseGrants, LeaseTtl,
        NotAppliedReason, Operation, Permission, PostingBudget, ResourceKind, ResourceRef,
        RogerAsk, RogerEffect, Settlement, Text, Timestamp, WorkerBackend, WorkerOutcome,
        WorkerState, Workspace,
    },
    state::{
        AttemptState, ConsumerState, EffectPlan, EffectRecord, EffectState, HouseStore, Lease,
        OwnershipEvent, StateError, TaskRecord, TaskState, reconcile, run_effect,
    },
    workflows::pickup::{BranchName, stable_hash},
};

type Result<T> = std::result::Result<T, crate::Error>;

/// A pickup, coordination, or repair failure. Input text is never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CoordinationError {
    /// A branch name failed Git's reference rules.
    #[error("invalid branch name")]
    InvalidBranchName,
    /// The backend created a branch other than the one requested.
    #[error("observed branch differs from the requested branch")]
    BranchMismatch,
    /// A brief disagrees with its task's house, pins, or repository, or has
    /// no acceptance criteria.
    #[error("worker brief does not match its task")]
    BriefMismatch,
    /// A human decision needs a repository-scoped task.
    #[error("task has no repository for a decision binding")]
    MissingRepository,
}

impl CoordinationError {
    /// Broad handling class.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::InvalidBranchName | Self::BriefMismatch | Self::MissingRepository => {
                ErrorClass::InvalidInput
            }
            Self::BranchMismatch => ErrorClass::Conflict,
        }
    }
}

/// Supplies a person's consent for one exact effect under an interactive
/// claim. Scheduled work uses [`Standing`], which never consents: its effects
/// act on the task's standing authority.
pub trait ConsentSource {
    /// The person's consent for exactly `effect` of `task` at `revision`, or
    /// `None` when there is no person or they declined.
    fn consent(
        &self,
        task: &TaskId,
        effect: &Effect,
        revision: EvidenceRevision,
    ) -> Option<Consent>;
}

/// No person present: effects use standing authority only.
#[derive(Debug, Clone, Copy, Default)]
pub struct Standing;

impl ConsentSource for Standing {
    fn consent(&self, _: &TaskId, _: &Effect, _: EvidenceRevision) -> Option<Consent> {
        None
    }
}

/// Worker backend capabilities supervision needs: isolated launch, positive
/// readiness, messaging, status, and cancellation of a stalled launch. They
/// are checked when a coordinator starts rather than recorded as task
/// requirements, because the store applies task requirements to every
/// executor, including forge and Roger executors.
pub const REQUIRED_WORKER_CAPABILITIES: [Capability; 5] = [
    Capability::WorkerLaunchIsolated,
    Capability::WorkerLaunchReadiness,
    Capability::WorkerMessaging,
    Capability::WorkerStatusAndOutcome,
    Capability::WorkerCancel,
];

/// What coordination acts through.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The worker backend.
    pub backend: &'a dyn WorkerBackend,
    /// The house's current grants.
    pub grants: &'a HouseGrants,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// Consent for interactive claims.
    pub consent: &'a dyn ConsentSource,
}

impl Context<'_> {
    fn run(
        &self,
        executor: &dyn EffectExecutor,
        task: &TaskId,
        fence: Fence,
        name: &str,
        effect: Effect,
        revision: EvidenceRevision,
    ) -> Result<EffectRecord> {
        let consent = self.consent.consent(task, &effect, revision);
        let plan = EffectPlan {
            task: task.clone(),
            fence,
            name: EffectName::new(name)?,
            decided_at: revision,
            effect,
            consent,
        };
        run_effect(self.store, executor, self.grants, plan, self.clock)
    }
}

/// Who controls a worker's terminal. Interim seam: until the backend
/// contract reports a takeover, the caller supplies it from the backend's
/// own record. A terminal a person took over belongs to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalControl {
    /// The agent runs the terminal.
    Agent,
    /// A person took the terminal over.
    UserTakeover,
}

/// The result of a launch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// The backend accepted the launch. This is not readiness.
    Accepted {
        /// The attempt it belongs to.
        attempt: AttemptNumber,
        /// The worker the backend created.
        worker: ResourceRef,
    },
    /// The launch definitely did not happen; the attempt was finished.
    NotApplied {
        /// Why.
        reason: NotAppliedReason,
        /// What happens next.
        disposition: Disposition,
    },
    /// The outcome is unknown. Never launch again; reconcile first.
    Uncertain,
    /// Unresolved effects must be reconciled before a new attempt.
    ReconcileFirst,
    /// The retry budget is spent; the task settled as exhausted.
    Exhausted,
}

/// Start (or continue) an attempt and launch its worker. A repeated call for
/// the same attempt never launches a second worker.
///
/// # Errors
/// Returns store and authority failures, such as a superseded consumer.
pub fn launch_worker(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
    workspace: Workspace,
    brief: Text,
) -> Result<LaunchOutcome> {
    let attempt = match ctx.store.start_attempt(task, fence, ctx.clock.now()) {
        Ok(AttemptStart::Started(attempt) | AttemptStart::AlreadyRunning(attempt)) => attempt,
        Ok(AttemptStart::Exhausted) => return Ok(LaunchOutcome::Exhausted),
        Err(crate::Error::State(StateError::UnresolvedEffects { .. })) => {
            return Ok(LaunchOutcome::ReconcileFirst);
        }
        Err(error) => return Err(error),
    };
    let record = ctx.store.task(task)?;
    let role = record.spec().role;
    let revision = record.evidence().revision();
    let effect = Effect::Worker(Operation::LaunchWorker {
        role,
        workspace,
        brief,
        branch: None,
    });
    let record = match ctx.run(
        ctx.backend,
        task,
        fence,
        &format!("launch-{}", attempt.get()),
        effect,
        revision,
    ) {
        Ok(record) => record,
        // An earlier submission's outcome is unknown and the backend cannot
        // deduplicate: never submit the launch again.
        Err(crate::Error::State(StateError::UnsafeRetry(_))) => {
            return Ok(LaunchOutcome::Uncertain);
        }
        Err(error) => return Err(error),
    };
    match record.state() {
        EffectState::Applied { receipt, .. } => {
            match receipt
                .created()
                .iter()
                .find(|resource| resource.kind == ResourceKind::Worker)
            {
                Some(worker) => Ok(LaunchOutcome::Accepted {
                    attempt,
                    worker: worker.clone(),
                }),
                // Accepted without a worker handle: nothing to supervise.
                None => Ok(LaunchOutcome::Uncertain),
            }
        }
        EffectState::NotApplied { reason, .. } => {
            let class = match reason {
                NotAppliedReason::Rejected
                | NotAppliedReason::ConfirmedAbsent
                | NotAppliedReason::RateLimited { .. } => FailureClass::Retryable,
                NotAppliedReason::Unsupported(_)
                | NotAppliedReason::CrossHouse
                | NotAppliedReason::ForeignBackend => FailureClass::Permanent,
            };
            let disposition = ctx.store.finish_attempt(
                task,
                fence,
                attempt,
                AttemptOutcome::Failed(class),
                ctx.clock.now(),
            )?;
            Ok(LaunchOutcome::NotApplied {
                reason: *reason,
                disposition,
            })
        }
        EffectState::Intended
        | EffectState::Uncertain { .. }
        | EffectState::Unresolvable { .. }
        | EffectState::Waived { .. } => Ok(LaunchOutcome::Uncertain),
    }
}

/// The worker a task's latest applied launch created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerView {
    /// The worker.
    pub worker: ResourceRef,
    /// When the launch was confirmed.
    pub launched_at: Timestamp,
}

/// The worker of the task's latest applied launch, from any attempt. After
/// an adoption, this is the previous owner's worker, which keeps running.
#[must_use]
pub fn current_worker(record: &TaskRecord) -> Option<WorkerView> {
    record.effects().iter().rev().find_map(|effect| {
        match (effect.request().effect(), effect.state()) {
            (
                Effect::Worker(Operation::LaunchWorker { .. }),
                EffectState::Applied { receipt, at },
            ) => receipt
                .created()
                .iter()
                .find(|resource| resource.kind == ResourceKind::Worker)
                .map(|worker| WorkerView {
                    worker: worker.clone(),
                    launched_at: *at,
                }),
            _ => None,
        }
    })
}

/// Supervision bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionPolicy {
    /// How long a launched worker may stay unready before the launch counts
    /// as failed.
    pub readiness_deadline: Duration,
    /// How long a question may wait for an answer before escalation.
    pub question_deadline: Duration,
    /// Claim renewal period.
    pub claim_ttl: LeaseTtl,
}

/// What a worker reported when it finished successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The branch the brief requested.
    pub requested: BranchName,
    /// The branch the backend actually holds.
    pub observed_branch: String,
    /// The worker's readable report, bound to the exact head it describes.
    pub report: Evidence,
}

/// Why supervision needs a person or the owning coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// The worker reported success without readable passing evidence.
    MissingEvidence,
    /// The worker's branch is not the requested one.
    BranchMismatch,
}

/// The result of one supervision step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervision {
    /// Effects have unknown outcomes; nothing new may start.
    Reconciling {
        /// Effects still unresolved, including those for another backend.
        unresolved: usize,
    },
    /// No worker has been launched for this task yet.
    AwaitingLaunch,
    /// The worker is running.
    Running(WorkerState),
    /// A person took the terminal over. Nothing is dispatched into it, and
    /// another writer on its branch needs the person's approval.
    PersonOwnsTerminal,
    /// The backend could not be queried; nothing is inferred.
    Unobservable,
    /// The backend has no record of the worker: uncertain, never settled.
    WorkerMissing,
    /// No readiness evidence arrived in time; the worker was stopped and the
    /// attempt failed.
    LaunchStalled {
        /// What happens next.
        disposition: Disposition,
    },
    /// The attempt failed and another attempt may start.
    Retry {
        /// Attempts left.
        remaining: u32,
    },
    /// The worker needs a decision before settlement.
    Escalate(Escalation),
    /// The task settled.
    Settled(Settlement),
}

fn running_attempt(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
) -> Result<Option<AttemptNumber>> {
    match ctx.store.start_attempt(task, fence, ctx.clock.now())? {
        AttemptStart::Started(attempt) | AttemptStart::AlreadyRunning(attempt) => Ok(Some(attempt)),
        AttemptStart::Exhausted => Ok(None),
    }
}

fn finish(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
    outcome: AttemptOutcome,
) -> Result<Supervision> {
    let Some(attempt) = running_attempt(ctx, task, fence)? else {
        return Ok(Supervision::Settled(Settlement::Exhausted));
    };
    Ok(
        match ctx
            .store
            .finish_attempt(task, fence, attempt, outcome, ctx.clock.now())?
        {
            Disposition::Settled(settlement) => Supervision::Settled(settlement),
            Disposition::RetryAvailable { remaining } => Supervision::Retry { remaining },
        },
    )
}

/// Run one supervision step for an owned task: renew the claim, reconcile
/// unresolved effects, observe the worker, and settle only on positive
/// evidence. Silence, a missing worker, or an unreachable backend never
/// settles a task.
///
/// # Errors
/// Returns store failures, including a stale fence or a superseded
/// consumer lease: the caller no longer owns the task and must stop.
pub fn supervise(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
    policy: &SupervisionPolicy,
    control: TerminalControl,
    completion: Option<&Completion>,
) -> Result<Supervision> {
    let now = ctx.clock.now();
    ctx.store.renew(task, fence, policy.claim_ttl, now)?;
    let report = reconcile(ctx.store, ctx.backend, task, fence, ctx.clock)?;
    let unresolved = report.unresolved.len().saturating_add(report.foreign.len());
    if unresolved > 0 {
        return Ok(Supervision::Reconciling { unresolved });
    }
    let record = ctx.store.task(task)?;
    if let TaskState::Settled { settlement, .. } = record.state() {
        return Ok(Supervision::Settled(*settlement));
    }
    // A finished or cancelled latest attempt already accounted for its
    // worker; only a new launch has something to supervise. An interrupted
    // attempt (relinquish, takeover) may still have a live worker.
    let attempt_done = record.attempts().last().is_some_and(|attempt| {
        matches!(
            attempt.state(),
            AttemptState::Finished { .. } | AttemptState::Cancelled { .. }
        )
    });
    let Some(view) = current_worker(&record).filter(|_| !attempt_done) else {
        return Ok(Supervision::AwaitingLaunch);
    };
    match control {
        TerminalControl::Agent => {}
        TerminalControl::UserTakeover => return Ok(Supervision::PersonOwnsTerminal),
    }
    let Ok(state) = ctx.backend.observe_worker(&view.worker) else {
        return Ok(Supervision::Unobservable);
    };
    match state {
        WorkerState::Starting
            if now.saturating_since(view.launched_at) > policy.readiness_deadline =>
        {
            stop_stalled(ctx, task, fence, &view.worker)
        }
        WorkerState::Starting | WorkerState::Ready | WorkerState::AwaitingReply => {
            Ok(Supervision::Running(state))
        }
        WorkerState::UserTakeover => Ok(Supervision::PersonOwnsTerminal),
        WorkerState::Missing => Ok(Supervision::WorkerMissing),
        WorkerState::Unknown => Ok(Supervision::Unobservable),
        WorkerState::Settled(WorkerOutcome::Succeeded) => {
            let Some(completion) = completion else {
                return Ok(Supervision::Escalate(Escalation::MissingEvidence));
            };
            if completion
                .requested
                .verify_observed(&completion.observed_branch)
                .is_err()
            {
                return Ok(Supervision::Escalate(Escalation::BranchMismatch));
            }
            if completion.report.kind != EvidenceKind::WorkerReport
                || completion.report.verdict != EvidenceVerdict::Pass
            {
                return Ok(Supervision::Escalate(Escalation::MissingEvidence));
            }
            ctx.store
                .record_evidence(task, fence, completion.report.clone(), now)?;
            finish(ctx, task, fence, AttemptOutcome::Succeeded)
        }
        WorkerState::Settled(WorkerOutcome::Cancelled) if record.cancel_request().is_some() => {
            ctx.store.settle_cancelled(task, fence, now)?;
            Ok(Supervision::Settled(Settlement::Cancelled))
        }
        WorkerState::Settled(WorkerOutcome::Failed | WorkerOutcome::Cancelled) => finish(
            ctx,
            task,
            fence,
            AttemptOutcome::Failed(FailureClass::Retryable),
        ),
    }
}

fn stop_stalled(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
    worker: &ResourceRef,
) -> Result<Supervision> {
    let Some(attempt) = running_attempt(ctx, task, fence)? else {
        return Ok(Supervision::Settled(Settlement::Exhausted));
    };
    let revision = ctx.store.task(task)?.evidence().revision();
    let record = ctx.run(
        ctx.backend,
        task,
        fence,
        &format!("stop-stalled-{}", attempt.get()),
        Effect::Worker(Operation::CancelWorker {
            worker: worker.clone(),
        }),
        revision,
    )?;
    if !record.state().is_resolved() {
        return Ok(Supervision::Reconciling { unresolved: 1 });
    }
    let disposition = ctx.store.finish_attempt(
        task,
        fence,
        attempt,
        AttemptOutcome::Failed(FailureClass::Retryable),
        ctx.clock.now(),
    )?;
    Ok(Supervision::LaunchStalled { disposition })
}

/// A question a worker asked, as read by the backend adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerQuestion {
    /// The backend's message reference.
    pub id: ExternalRef,
    /// When it was asked.
    pub asked_at: Timestamp,
}

/// A genuine human decision, as classified by a scoped agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanDecision {
    /// The action the person decides on.
    pub action: Permission,
    /// The exact target, such as `pr:owner/name#12` or `task:<id>`.
    pub target: ExternalRef,
    /// The exact commit the person sees.
    pub subject: CommitId,
    /// Human-visible constraints.
    pub limits: Text,
    /// Approval or question.
    pub kind: AskKind,
    /// Consequence level chosen by house policy.
    pub risk: AskRisk,
    /// One-line title.
    pub title: Text,
    /// Sanitized context.
    pub body: Text,
}

/// How a question should be handled, from the coordinator or a scoped agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Answer the worker with this text.
    Answer(Text),
    /// Only a person can decide.
    Human(HumanDecision),
    /// No answer yet.
    Pending,
}

/// The Roger decision service, when the house installed it.
#[derive(Clone, Copy)]
pub struct RogerChannel<'a> {
    /// The Roger executor for this house.
    pub executor: &'a dyn EffectExecutor,
    /// The house's requester identity for this workflow.
    pub requester: &'a ExternalRef,
    /// The house posting budget for asks.
    pub budget: PostingBudget,
}

/// Why a question needs the coordinator's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionEscalation {
    /// A human decision is needed and Roger is not installed; nothing is faked.
    NoHumanChannel,
    /// The question stayed unanswered past its deadline.
    Unanswered,
    /// The worker is gone or was never launched.
    NoWorker,
    /// The reply or ask definitely failed.
    NotApplied,
}

/// What happened to a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionRoute {
    /// The worker received the answer.
    Replied,
    /// The question was already answered; nothing was sent again.
    Duplicate,
    /// A person was asked through Roger.
    AskedHuman,
    /// Waiting for an answer within the deadline, or for a person.
    Waiting,
    /// The effect's outcome is unknown; reconcile before anything else.
    Uncertain,
    /// Escalate to the owner.
    Escalate(QuestionEscalation),
}

fn named_effect<'r>(record: &'r TaskRecord, name: &str) -> Option<&'r EffectRecord> {
    record
        .effects()
        .iter()
        .rev()
        .find(|effect| effect.name().as_str() == name)
}

/// Handle one worker question. Answers are delivered once per question
/// across retries, restarts, and adoption; a human decision goes through
/// Roger only when installed, at most once per question.
///
/// # Errors
/// Returns store and authority failures.
pub fn handle_question(
    ctx: &Context<'_>,
    task: &TaskId,
    fence: Fence,
    policy: &SupervisionPolicy,
    question: &WorkerQuestion,
    response: &Response,
    roger: Option<&RogerChannel<'_>>,
) -> Result<QuestionRoute> {
    let now = ctx.clock.now();
    let key = stable_hash(question.id.as_str().as_bytes());
    let reply_name = format!("reply-{key:016x}");
    let ask_name = format!("ask-{key:016x}");
    let record = ctx.store.task(task)?;
    if let Some(existing) = named_effect(&record, &reply_name) {
        return Ok(match existing.state() {
            EffectState::Applied { .. } => {
                ctx.store.consume_message(task, fence, &question.id, now)?;
                QuestionRoute::Duplicate
            }
            EffectState::NotApplied { .. } => {
                QuestionRoute::Escalate(QuestionEscalation::NotApplied)
            }
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => QuestionRoute::Uncertain,
        });
    }
    let Some(view) = current_worker(&record) else {
        return Ok(QuestionRoute::Escalate(QuestionEscalation::NoWorker));
    };
    let revision = record.evidence().revision();
    let asked = named_effect(&record, &ask_name).map(EffectRecord::state);
    let overdue = now.saturating_since(question.asked_at) > policy.question_deadline;
    match response {
        Response::Answer(body) => {
            let effect = Effect::Worker(Operation::ReplyToWorker {
                worker: view.worker,
                question: question.id.clone(),
                body: body.clone(),
            });
            let reply = ctx.run(ctx.backend, task, fence, &reply_name, effect, revision)?;
            Ok(match reply.state() {
                EffectState::Applied { .. } => {
                    ctx.store.consume_message(task, fence, &question.id, now)?;
                    QuestionRoute::Replied
                }
                EffectState::NotApplied { .. } => {
                    QuestionRoute::Escalate(QuestionEscalation::NotApplied)
                }
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. } => QuestionRoute::Uncertain,
            })
        }
        Response::Pending | Response::Human(_) if overdue => {
            Ok(QuestionRoute::Escalate(QuestionEscalation::Unanswered))
        }
        Response::Pending => Ok(QuestionRoute::Waiting),
        Response::Human(_) if asked.is_some() => Ok(match asked {
            Some(EffectState::NotApplied { .. }) => {
                QuestionRoute::Escalate(QuestionEscalation::NotApplied)
            }
            Some(EffectState::Applied { .. }) | None => QuestionRoute::Waiting,
            Some(
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. },
            ) => QuestionRoute::Uncertain,
        }),
        Response::Human(decision) => {
            let Some(roger) = roger else {
                return Ok(QuestionRoute::Escalate(QuestionEscalation::NoHumanChannel));
            };
            let repository = record
                .spec()
                .repository
                .clone()
                .ok_or(CoordinationError::MissingRepository)?;
            let effect = Effect::Roger(RogerEffect {
                requester: roger.requester.clone(),
                ask: RogerAsk {
                    binding: DecisionBinding {
                        house: ctx.store.house().clone(),
                        task: task.clone(),
                        owner: DecisionOwner::Task,
                        repository,
                        action: decision.action,
                        target: decision.target.clone(),
                        revision,
                        subject: Some(crate::contracts::EvidenceSubject {
                            head: decision.subject.clone(),
                            base: None,
                        }),
                        limits: decision.limits.clone(),
                    },
                    kind: decision.kind,
                    risk: decision.risk,
                    title: decision.title.clone(),
                    body: decision.body.clone(),
                    supersedes: None,
                },
                posting_budget: roger.budget,
            });
            let ask = ctx.run(roger.executor, task, fence, &ask_name, effect, revision)?;
            Ok(match ask.state() {
                EffectState::Applied { .. } => QuestionRoute::AskedHuman,
                EffectState::NotApplied { .. } => {
                    QuestionRoute::Escalate(QuestionEscalation::NotApplied)
                }
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. } => QuestionRoute::Uncertain,
            })
        }
    }
}

/// The result of starting a coordinator for a consumer scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorStart {
    /// The scope was idle or new.
    Fresh(Lease),
    /// The previous coordinator recorded a relinquish; this one adopted the
    /// scope and the tasks it had relinquished. Workers were not touched.
    Adopted {
        /// The new consumer lease.
        lease: Lease,
        /// Adopted task claims.
        tasks: Vec<(TaskId, Lease)>,
        /// Relinquished tasks someone else claimed first.
        skipped: Vec<TaskId>,
    },
    /// Another coordinator holds a live lease: this tick does nothing.
    Busy,
    /// The previous coordinator's lease expired without a relinquish.
    /// Ownership is uncertain; nothing is adopted implicitly.
    Uncertain {
        /// When it expired.
        expired_at: Timestamp,
    },
}

/// Start a coordinator: check that the worker backend supports what
/// supervision needs, then take the durable single-consumer lease for
/// `consumer`, adopting relinquished work through recorded adoptions.
/// A duplicate tick is refused by the lease, not by prompt text.
///
/// # Errors
/// Returns [`ContractError::UnsupportedCapabilities`] naming every missing
/// capability before any lease is taken, [`ContractError::CrossHouse`] for
/// another house's backend, and store failures.
pub fn start_coordinator(
    store: &HouseStore,
    backend: &BackendDescriptor,
    consumer: &ConsumerId,
    claimant: &Claimant,
    ttl: LeaseTtl,
    now: Timestamp,
) -> Result<CoordinatorStart> {
    if &backend.house != store.house() {
        return Err(ContractError::CrossHouse {
            expected: store.house().clone(),
            found: backend.house.clone(),
        }
        .into());
    }
    backend.capabilities.require(REQUIRED_WORKER_CAPABILITIES)?;
    let previous = store
        .consumer(consumer)?
        .and_then(|record| match record.state() {
            ConsumerState::Relinquished { lease, .. } => Some(lease.holder().clone()),
            ConsumerState::Idle | ConsumerState::Held { .. } => None,
        });
    let lease = match store.acquire_consumer(consumer, claimant, ttl, now) {
        Ok(lease) => lease,
        Err(crate::Error::State(StateError::ClaimHeld { .. })) => {
            return Ok(CoordinatorStart::Busy);
        }
        Err(crate::Error::State(StateError::LeaseExpired { expired_at })) => {
            return Ok(CoordinatorStart::Uncertain { expired_at });
        }
        Err(error) => return Err(error),
    };
    let Some(previous) = previous else {
        return Ok(CoordinatorStart::Fresh(lease));
    };
    let under = claimant.clone().under(consumer.clone(), lease.fence());
    let mut tasks = Vec::new();
    let mut skipped = Vec::new();
    for record in store.tasks()? {
        let events = record.ownership();
        let relinquished_by_previous = match events {
            [
                ..,
                OwnershipEvent::Claimed { holder, fence, .. }
                | OwnershipEvent::Adopted { holder, fence, .. }
                | OwnershipEvent::TakenOver { holder, fence, .. },
                OwnershipEvent::Relinquished { fence: gave_up, .. },
            ] => holder == &previous && fence == gave_up,
            _ => false,
        };
        if !relinquished_by_previous || !matches!(record.state(), TaskState::Open) {
            continue;
        }
        let id = record.spec().id.clone();
        match store.claim(&id, &under, ttl, now) {
            Ok(task_lease) => tasks.push((id, task_lease)),
            Err(crate::Error::State(
                StateError::ClaimHeld { .. } | StateError::LeaseExpired { .. },
            )) => skipped.push(id),
            Err(error) => return Err(error),
        }
    }
    Ok(CoordinatorStart::Adopted {
        lease,
        tasks,
        skipped,
    })
}

/// Hand a coordinator's scope over with work in flight: relinquish every
/// task claimed under this consumer lease, then the lease itself. The next
/// coordinator adopts both through [`start_coordinator`]. Workers keep
/// running.
///
/// # Errors
/// Returns [`StateError::StaleFence`] when `fence` no longer holds the scope.
pub fn relinquish_coordinator(
    store: &HouseStore,
    consumer: &ConsumerId,
    fence: Fence,
    now: Timestamp,
) -> Result<Vec<TaskId>> {
    let mut relinquished = Vec::new();
    for record in store.tasks()? {
        if let TaskState::Claimed { lease } = record.state()
            && lease
                .consumer()
                .is_some_and(|held| &held.consumer == consumer && held.fence == fence)
        {
            store.relinquish(&record.spec().id, lease.fence(), now)?;
            relinquished.push(record.spec().id.clone());
        }
    }
    store.relinquish_consumer(consumer, fence, now)?;
    Ok(relinquished)
}
