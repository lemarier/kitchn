//! The schedule budget pass: apply a house's [`SchedulePolicy`] to its live
//! schedules, pause each exhausted one, and hand its owner report back once
//! per window.
//!
//! [`tick`] is the caller: the `kitchn budget run` command, started by the
//! house's budget schedule ([`install`], always installed paused) after
//! `kitchn budget precheck` found work. It claims one task per budget
//! window, reconciles that task's earlier effects, runs the pass, posts each
//! due report through the report channel, and records a report only after
//! its post applied. [`run`] can also run as a step of another workflow that
//! holds a claimed task with
//! [`Permission::ManageSchedule`](crate::contracts::Permission::ManageSchedule).
//! The caller observes the house's schedules through its adapter, such as
//! [`OrcaBackend::schedule_evidence`](crate::adapters::orca::OrcaBackend::schedule_evidence),
//! and passes that one observation to every step.
//!
//! Each pause is a [`ScheduleEffect::SetState`]
//! run through the state store, so its intent is persisted, its outcome is
//! recorded on the task, and an uncertain outcome is reconciled before the
//! task does anything else. The pass never activates a schedule. It is
//! bounded by the evidence, which holds at most
//! [`MAX_EVIDENCE_SCHEDULES`](crate::scheduling::MAX_EVIDENCE_SCHEDULES)
//! schedules, and by the executor's own call deadlines.
//!
//! The owner report is recorded only after delivery, with
//! [`confirm_reported`]. A pass interrupted after a pause and before that
//! finds the schedule paused and unreported next time, and returns the
//! report again without pausing twice.
//!
//! A report with no destination is recorded once per window with
//! [`confirm_undeliverable`], not as reported: the schedule stays paused,
//! later ticks in the window stay idle, and [`undelivered_reports`] gives
//! doctor the exhaustions whose owner was never told.
//!
//! The budget schedule is an ordinary schedule. Its runs count toward its own
//! budget and the house budget like any other's, and it holds an allocation of
//! the house budget. When the house budget is exhausted, [`tick`] reports
//! first, once per schedule and window, and then pauses every exhausted
//! schedule, its own tick last: a pass posts the tick's own report before it
//! pauses the tick, and pauses it even when that post did not apply. A paused
//! tick stays paused until the owner reactivates it after the window resets;
//! the pass never unpauses anything.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};

use crate::{
    BackendId, ConsumerId, CredentialId, Error, HouseId, TaskId, WorkflowId,
    contracts::{
        AttemptOutcome, Capability, CapabilityRequirements, Claimant, Clock, Effect,
        EffectExecutor, Fence, Grant, HouseGrants, LeaseTtl, Provenance, Repository, RetryPolicy,
        Role, ScheduleEffect, TaskAuthority, TaskSpec, Text, Timestamp,
    },
    id::EffectName,
    scheduling::{
        self, BudgetExhaustion, PrecheckTimeout, Recurrence, ScheduleEvidence, SchedulePolicy,
        ScheduleSpec, ScheduleState, Timezone, UndeliveredReport, UsageWindow, WorkflowName,
    },
    selection::ResolvedSelection,
    state::{EffectPlan, EffectRecord, EffectState, HouseStore, MarkerFact, StateError, TaskState},
};

use super::{Precheck, WorkflowError};

type Result<T> = std::result::Result<T, Error>;

/// What the pass did for one exhausted schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassAction {
    /// The schedule is paused, by this pass or earlier, and its owner has
    /// not been told this window. Deliver [`BudgetExhaustion::report`], then
    /// call [`confirm_reported`].
    Report(BudgetExhaustion),
    /// This pass paused a schedule the owner re-activated after this
    /// window's report; the owner is not told twice.
    Repaused(BudgetExhaustion),
    /// The pause did not apply, so nothing is reported. The record says
    /// whether it was refused or is uncertain; an uncertain pause must be
    /// reconciled before the task's next effect.
    PauseNotApplied {
        /// The exhausted schedule.
        exhaustion: BudgetExhaustion,
        /// The pause effect as recorded.
        record: Box<EffectRecord>,
    },
}

/// The claimed task a pass acts under, and the house grants its pauses draw
/// on. The grants also name the house whose schedules the pass judges.
#[derive(Debug, Clone, Copy)]
pub struct PassClaim<'a> {
    /// A task claimed by the scheduled tick, holding
    /// [`Permission::ManageSchedule`](crate::contracts::Permission::ManageSchedule).
    pub task: &'a TaskId,
    /// The claim's fence.
    pub fence: Fence,
    /// The house's grants.
    pub grants: &'a HouseGrants,
}

/// The result of one budget pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetPass {
    /// No schedule needed a pause or a report.
    Idle,
    /// One action per exhausted schedule, in evidence order. A pause that
    /// did not apply stops the pass; later schedules wait for the next one.
    Acted(Vec<PassAction>),
}

/// Whether a pass over `evidence` has anything to do: an exhausted schedule
/// to pause or an exhaustion not yet reported. Reads only.
///
/// # Errors
/// Budget refusals, such as evidence for another house, and store failures
/// reading report markers. A failure is never idle.
pub fn precheck(
    store: &HouseStore,
    house: &HouseId,
    policy: &SchedulePolicy,
    evidence: &ScheduleEvidence,
) -> Result<Precheck> {
    let plan = plan(store, house, policy, evidence)?;
    Ok(if plan.is_empty() {
        Precheck::Idle
    } else {
        Precheck::Actionable
    })
}

/// Apply `policy` to `evidence` under the claimed `task`: pause every
/// exhausted schedule that is still active, whose state is unknown, or whose
/// report is due, and return the reports its owner has not received this
/// window.
///
/// A schedule observed paused is paused again before it is reported, because
/// its owner may have re-activated it since the observation; the report then
/// holds when it is delivered. Pausing a paused schedule changes nothing.
///
/// Each pause is named by its schedule, budget window, and observation time.
/// A pass repeated on the same observation reuses its recorded pauses,
/// whatever order the evidence lists schedules in, and a later observation
/// pauses a schedule the owner re-activated.
///
/// # Errors
/// Budget refusals, and store refusals such as a lost claim, a missing
/// [`Permission::ManageSchedule`](crate::contracts::Permission::ManageSchedule)
/// grant, or an unresolved earlier effect. Nothing is paused after an error.
pub fn run(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    claim: PassClaim<'_>,
    policy: &SchedulePolicy,
    evidence: &ScheduleEvidence,
    clock: &dyn Clock,
) -> Result<BudgetPass> {
    let mut plan = plan(store, claim.grants.house(), policy, evidence)?;
    if plan.is_empty() {
        return Ok(BudgetPass::Idle);
    }
    // The budget schedule's own pause goes last: it is an ordinary schedule,
    // but pausing it ends the passes that would pause the rest.
    plan.sort_by_key(is_own_tick);
    Ok(BudgetPass::Acted(pause_all(
        store, executor, claim, plan, evidence, clock,
    )?))
}

/// Whether `exhaustion` is the budget schedule's own.
fn is_own_tick(exhaustion: &BudgetExhaustion) -> bool {
    exhaustion.consumer.as_str() == WORKFLOW
}

/// Pause `plan` in order, stopping at the first pause that did not apply.
fn pause_all(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    claim: PassClaim<'_>,
    plan: Vec<BudgetExhaustion>,
    evidence: &ScheduleEvidence,
    clock: &dyn Clock,
) -> Result<Vec<PassAction>> {
    let decided_at = store.task(claim.task)?.evidence().revision();
    let mut actions = Vec::with_capacity(plan.len());
    for exhaustion in plan {
        let name = pause_name(&exhaustion, evidence.observed_at)?;
        let effect = Effect::Schedule(ScheduleEffect::SetState {
            schedule: exhaustion.schedule.clone(),
            state: ScheduleState::Paused,
        });
        let record = crate::state::run_effect(
            store,
            executor,
            claim.grants,
            EffectPlan {
                task: claim.task.clone(),
                fence: claim.fence,
                name,
                decided_at,
                effect,
                consent: None,
                basis: None,
            },
            clock,
        )?;
        match record.state() {
            EffectState::Applied { .. } if exhaustion.report_due => {
                actions.push(PassAction::Report(exhaustion));
            }
            EffectState::Applied { .. } => actions.push(PassAction::Repaused(exhaustion)),
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::NotApplied { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => {
                actions.push(PassAction::PauseNotApplied {
                    exhaustion,
                    record: Box::new(record),
                });
                break;
            }
        }
    }
    Ok(actions)
}

/// Record that the owner received `exhaustion`'s report, so later passes in
/// the same window do not report it again. Recording it twice is harmless.
///
/// # Errors
/// Budget errors building the marker and store failures.
pub fn confirm_reported(
    store: &HouseStore,
    claimant: &Claimant,
    exhaustion: &BudgetExhaustion,
    now: Timestamp,
) -> Result<()> {
    store.record_marker(
        exhaustion.marker_key()?,
        exhaustion.marker_fact()?,
        claimant,
        now,
    )?;
    Ok(())
}

/// Record that `exhaustion`'s report had no destination, so later passes in
/// the same window neither report it nor keep the precheck actionable for
/// it. The owner was not told: [`BudgetExhaustion::marker_key`] stays absent,
/// and [`undelivered_reports`] lists it for doctor. Recording it twice is
/// harmless.
///
/// # Errors
/// Budget errors building the marker and store failures.
pub fn confirm_undeliverable(
    store: &HouseStore,
    claimant: &Claimant,
    exhaustion: &BudgetExhaustion,
    now: Timestamp,
) -> Result<()> {
    store.record_marker(
        exhaustion.undeliverable_key()?,
        exhaustion.undeliverable_fact()?,
        claimant,
        now,
    )?;
    Ok(())
}

/// Every budget exhaustion whose report had no destination, oldest first,
/// as evidence for doctor.
///
/// # Errors
/// Store failures, and a marker whose payload does not decode.
pub fn undelivered_reports(store: &HouseStore) -> Result<Vec<UndeliveredReport>> {
    let schema = scheduling::undeliverable_schema()?;
    let workflow = WorkflowId::new(scheduling::BUDGET_WORKFLOW)?;
    store
        .markers(&workflow)?
        .iter()
        .filter(|marker| {
            matches!(marker.fact(), MarkerFact::Workflow { schema: found, .. } if *found == schema)
        })
        .map(|marker| Ok(marker.fact().decode(&schema)?))
        .collect()
}

/// The workflow name and consumer the budget schedule runs under, and the
/// prefix of its task ids.
pub const WORKFLOW: &str = "budget";

/// Required backend support before the budget schedule is installed.
pub const REQUIRED_CAPABILITIES: [Capability; 4] = [
    Capability::ScheduleManage,
    Capability::SchedulePrecheck,
    Capability::ScheduleSingleConsumer,
    Capability::ScheduleRunTimeout,
];

/// Bound on one precheck run: one schedule listing and one run listing per
/// schedule, each under the adapter's call deadline.
const PRECHECK_TIMEOUT: Duration = PrecheckTimeout::MAX;

/// Attempts a window's task may start. A tick continues the running attempt,
/// so only a lost or interrupted attempt spends one.
const TICK_ATTEMPTS: u32 = 16;

/// The retry policy of a window's task. The task is created inside the
/// window, so an elapsed budget of the window's length keeps it retryable
/// until the window ends, whatever the policy's window length.
fn tick_retry(window: UsageWindow) -> Result<RetryPolicy> {
    let length = window
        .end
        .as_unix_millis()
        .saturating_sub(window.start.as_unix_millis());
    Ok(RetryPolicy::new(
        TICK_ATTEMPTS,
        Duration::from_millis(length),
    )?)
}

/// Which budget command a schedule step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickCommand {
    /// `kitchn budget precheck`: reads only.
    Precheck,
    /// `kitchn budget run`: pauses and reports.
    Run,
}

impl TickCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Precheck => "precheck",
            Self::Run => "run",
        }
    }
}

/// Where the tick posts owner reports: an issue in one of the house's
/// posting destinations, under the GitHub CLI and a house read/write
/// credential file.
#[derive(Debug, Clone)]
pub struct ReportArgs {
    /// The issue's repository; must be a house posting destination.
    pub repository: Repository,
    /// The issue number.
    pub issue: crate::contracts::IssueNumber,
    /// The GitHub backend namespace the house's comment grant names.
    pub backend: BackendId,
    /// The authenticated GitHub login the credential must belong to.
    pub requester: crate::contracts::ExternalRef,
    /// The house credential the comment grant names.
    pub credential: CredentialId,
    /// The private file holding that credential.
    pub credential_file: PathBuf,
    /// The GitHub CLI executable.
    pub gh: PathBuf,
}

/// Everything the budget schedule's commands need, rendered as argument
/// vectors for `kitchn budget precheck` and `kitchn budget run`. Paths are
/// absolute because the backend runs them outside any checkout. Credential
/// file paths are recorded in the schedule; tokens never are.
#[derive(Debug, Clone)]
pub struct TickArgs {
    /// The installed `kitchn` executable.
    pub kitchen: PathBuf,
    /// The house registry holding the house configuration.
    pub registry: PathBuf,
    /// The house.
    pub house: HouseId,
    /// The house's initialized state store.
    pub store: PathBuf,
    /// The Orca executable.
    pub orca: PathBuf,
    /// The Orca backend namespace the house's schedule grant names.
    pub backend: BackendId,
    /// The Orca host session credential that grant names.
    pub credential: CredentialId,
    /// House-scoped Orca runtime storage shared by every caller.
    pub runtime_dir: PathBuf,
    /// The report channel; without one, each report is printed as
    /// undeliverable and recorded once per window.
    pub report: Option<ReportArgs>,
}

impl TickArgs {
    /// The argument vector of `command`.
    ///
    /// # Errors
    /// Refuses a relative or non-UTF-8 path.
    pub fn argv(&self, command: TickCommand) -> Result<Vec<Text>> {
        let mut args = vec![
            absolute(&self.kitchen)?,
            WORKFLOW,
            command.as_str(),
            "--registry",
            absolute(&self.registry)?,
            "--house",
            self.house.as_str(),
            "--store",
            absolute(&self.store)?,
            "--orca",
            absolute(&self.orca)?,
            "--backend",
            self.backend.as_str(),
            "--credential",
            self.credential.as_str(),
            "--runtime-dir",
            absolute(&self.runtime_dir)?,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        if let (TickCommand::Run, Some(report)) = (command, &self.report) {
            args.extend([
                "--report-issue".to_owned(),
                format!("{}#{}", report.repository, report.issue.get()),
                "--github-backend".to_owned(),
                report.backend.to_string(),
                "--requester".to_owned(),
                report.requester.to_string(),
                "--github-credential".to_owned(),
                report.credential.to_string(),
                "--credential-file".to_owned(),
                absolute(&report.credential_file)?.to_owned(),
                "--gh".to_owned(),
                absolute(&report.gh)?.to_owned(),
            ]);
        }
        args.iter()
            .map(|arg| Text::new(arg).map_err(|_| WorkflowError::IncompleteEvidence.into()))
            .collect()
    }
}

fn absolute(path: &Path) -> Result<&str> {
    path.is_absolute()
        .then(|| path.to_str())
        .flatten()
        .ok_or_else(|| WorkflowError::IncompleteEvidence.into())
}

/// The house's budget schedule under the consumer [`WORKFLOW`], for the
/// backend's schedule installer, such as
/// [`OrcaBackend::install_schedule`](crate::adapters::orca::OrcaBackend::install_schedule),
/// which installs it paused and counts its allocation against the house
/// budget. It is budgeted like any other schedule, so an exhausted house
/// budget pauses it too. Nothing in Kitchen activates it: turning it on, and
/// again after a pause, is the owner's separate schedule effect under its own
/// permission. Its precheck is
/// `kitchn budget precheck`, so a tick with no exhausted schedule starts no
/// agent; the agent it starts runs `kitchn budget run` and relays that
/// output.
///
/// # Errors
/// Refuses invalid arguments.
pub fn install(
    recurrence: Recurrence,
    timezone: Timezone,
    agent: ResolvedSelection,
    args: &TickArgs,
) -> Result<ScheduleSpec> {
    fn invalid<E>(_: E) -> Error {
        WorkflowError::IncompleteEvidence.into()
    }
    let check = scheduling::Precheck::new(
        args.argv(TickCommand::Precheck)?,
        PrecheckTimeout::new(PRECHECK_TIMEOUT).map_err(invalid)?,
    )
    .map_err(invalid)?;
    let run = args
        .argv(TickCommand::Run)?
        .iter()
        .map(Text::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let prompt = Text::new(&format!(
        "Run the Kitchen schedule budget pass for house {} with exactly this command, and report its output: {run}. Do not change schedules yourself; the command pauses exhausted schedules and never activates one.",
        args.house
    ))
    .map_err(invalid)?;
    Ok(ScheduleSpec::new(
        WorkflowName::new(WORKFLOW).map_err(invalid)?,
        ConsumerId::new(WORKFLOW).map_err(invalid)?,
        recurrence,
        timezone,
        prompt,
        agent,
    )
    .with_precheck(check))
}

/// How a tick posts one owner report.
pub struct ReportChannel<'a> {
    /// Executes the post, such as a
    /// [`GitHubExecutor`](crate::integrations::github::GitHubExecutor).
    pub executor: &'a dyn EffectExecutor,
    /// Builds the effect that posts `exhaustion`'s report.
    pub effect: &'a dyn Fn(&BudgetExhaustion) -> Result<Effect>,
}

/// What a tick acts under.
pub struct Tick<'a> {
    /// The house's state store.
    pub store: &'a HouseStore,
    /// The schedule backend the pauses go to.
    pub schedules: &'a dyn EffectExecutor,
    /// The report channel; `None` leaves every report undeliverable.
    pub reports: Option<ReportChannel<'a>>,
    /// The house's grants.
    pub grants: &'a HouseGrants,
    /// The grants each window's task is delegated: a schedule grant, and a
    /// comment grant for the report issue. Keep them the same for a window;
    /// a changed set is refused as a changed task.
    pub authority: Vec<Grant>,
    /// Kitchen and house guidance revisions recorded on each task.
    pub provenance: Provenance,
    /// The scheduled claimant.
    pub claimant: &'a Claimant,
    /// The claim's lease.
    pub ttl: LeaseTtl,
    /// Time source.
    pub clock: &'a dyn Clock,
}

/// What happened to one due report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Posted and recorded; later passes this window do not report it.
    Delivered(BudgetExhaustion),
    /// No report channel is configured. This is recorded once per window
    /// ([`confirm_undeliverable`]), so later ticks in it stay idle; the
    /// owner was not told.
    Undeliverable(BudgetExhaustion),
    /// The post did not apply. The report stays due; an uncertain post is
    /// looked up by the window's next tick, never posted twice.
    NotDelivered {
        /// The exhaustion.
        exhaustion: BudgetExhaustion,
        /// The post as recorded.
        record: Box<EffectRecord>,
    },
}

/// The result of one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// The pass.
    pub pass: BudgetPass,
    /// One entry per report the pass returned, in order. A post that did
    /// not apply stops delivery; the rest stay due.
    pub deliveries: Vec<Delivery>,
    /// Tasks of earlier windows this tick settled.
    pub settled: Vec<TaskId>,
}

/// Run one budget tick over `evidence`: claim this window's task
/// (`budget-<window start>`), reconcile its unresolved effects with both
/// backends, run the pass, post each due report, and record it only once its
/// post applied. The claim is given back at the end, even after an error, so
/// the window's next tick continues the same task. Then settle earlier
/// windows' tasks. An exhaustion-free observation claims nothing.
///
/// # Errors
/// Budget refusals; task creation, claim, and reconcile refusals, such as a
/// claim another tick holds or an effect still unresolved after lookup;
/// and [`run`] and posting refusals.
pub fn tick(
    tick: &Tick<'_>,
    policy: &SchedulePolicy,
    evidence: &ScheduleEvidence,
) -> Result<TickReport> {
    let house = tick.grants.house();
    let plan = plan(tick.store, house, policy, evidence)?;
    let Some(window) = plan.first().map(|exhaustion| exhaustion.window) else {
        return Ok(TickReport {
            pass: BudgetPass::Idle,
            deliveries: Vec::new(),
            settled: Vec::new(),
        });
    };
    let task = TaskId::new(&format!("{WORKFLOW}-{}", window.start.as_unix_millis()))?;
    let spec = TaskSpec {
        id: task.clone(),
        role: Role::Expediter,
        repository: None,
        authority: TaskAuthority::delegate(tick.grants, tick.authority.iter().cloned())?,
        retry: tick_retry(window)?,
        provenance: tick.provenance.clone(),
        requires: CapabilityRequirements::new(),
        resources: BTreeSet::new(),
        agent: None,
    };
    tick.store
        .create_task(spec, tick.claimant, tick.clock.now())?;
    let fence = claim(tick, &task)?;
    let result = act(tick, &task, fence, policy, evidence);
    let released = tick.store.relinquish(&task, fence, tick.clock.now());
    let (pass, deliveries) = result?;
    released?;
    // Cleanup runs after the pass, so it can never hold back a pause.
    let settled = settle_earlier(tick, &task)?;
    Ok(TickReport {
        pass,
        deliveries,
        settled,
    })
}

/// Claim `task`, taking over a claim whose lease expired, and continue its
/// attempt or start the first.
fn claim(tick: &Tick<'_>, task: &TaskId) -> Result<Fence> {
    let now = tick.clock.now();
    let lease = match tick.store.claim(task, tick.claimant, tick.ttl, now) {
        Err(Error::State(StateError::LeaseExpired { .. })) => {
            tick.store.take_over(task, tick.claimant, tick.ttl, now)?
        }
        other => other?,
    };
    let fence = lease.fence();
    let started = tick
        .store
        .continue_attempt(task, fence, now)
        .and_then(|running| match running {
            Some(_) => Ok(()),
            None => tick.store.start_attempt(task, fence, now).map(|_| ()),
        });
    if let Err(error) = started {
        tick.store.relinquish(task, fence, now)?;
        return Err(error);
    }
    Ok(fence)
}

fn act(
    tick: &Tick<'_>,
    task: &TaskId,
    fence: Fence,
    policy: &SchedulePolicy,
    evidence: &ScheduleEvidence,
) -> Result<(BudgetPass, Vec<Delivery>)> {
    reconcile(tick, task, fence)?;
    let claim = PassClaim {
        task,
        fence,
        grants: tick.grants,
    };
    let (own, others): (Vec<_>, Vec<_>) = plan(tick.store, tick.grants.house(), policy, evidence)?
        .into_iter()
        .partition(is_own_tick);
    let mut actions = pause_all(
        tick.store,
        tick.schedules,
        claim,
        others,
        evidence,
        tick.clock,
    )?;
    // A pause that did not apply stops the pass, the tick's own included.
    let stopped = actions
        .iter()
        .any(|action| matches!(action, PassAction::PauseNotApplied { .. }));
    let mut deliveries = Vec::new();
    for action in &actions {
        let PassAction::Report(exhaustion) = action else {
            continue;
        };
        let delivery = deliver(tick, task, fence, exhaustion.clone())?;
        let stop = matches!(delivery, Delivery::NotDelivered { .. });
        deliveries.push(delivery);
        if stop {
            break;
        }
    }
    if !stopped && !own.is_empty() {
        // The report comes first: pausing the tick ends the passes that
        // would retry it. A post that did not apply does not hold back the
        // pause, so the tick cannot keep spending.
        for exhaustion in &own {
            if exhaustion.report_due {
                deliveries.push(deliver(tick, task, fence, exhaustion.clone())?);
            }
        }
        actions.extend(pause_all(
            tick.store,
            tick.schedules,
            claim,
            own,
            evidence,
            tick.clock,
        )?);
    }
    if actions.is_empty() {
        return Ok((BudgetPass::Idle, deliveries));
    }
    Ok((BudgetPass::Acted(actions), deliveries))
}

/// Look up the task's unresolved effects with the backend each went to.
fn reconcile(tick: &Tick<'_>, task: &TaskId, fence: Fence) -> Result<()> {
    crate::state::reconcile(tick.store, tick.schedules, task, fence, tick.clock)?;
    if let Some(reports) = &tick.reports {
        crate::state::reconcile(tick.store, reports.executor, task, fence, tick.clock)?;
    }
    Ok(())
}

fn deliver(
    tick: &Tick<'_>,
    task: &TaskId,
    fence: Fence,
    exhaustion: BudgetExhaustion,
) -> Result<Delivery> {
    let Some(reports) = &tick.reports else {
        confirm_undeliverable(tick.store, tick.claimant, &exhaustion, tick.clock.now())?;
        return Ok(Delivery::Undeliverable(exhaustion));
    };
    let name = report_name(&exhaustion)?;
    let task_record = tick.store.task(task)?;
    // An earlier post of this window's report is resubmitted as recorded, so
    // a changed limit or allowance cannot post a second, different report.
    let effect = match task_record
        .effects()
        .iter()
        .find(|record| record.name() == &name)
    {
        Some(earlier) => earlier.request().effect().clone(),
        None => (reports.effect)(&exhaustion)?,
    };
    let record = crate::state::run_effect(
        tick.store,
        reports.executor,
        tick.grants,
        EffectPlan {
            task: task.clone(),
            fence,
            name,
            decided_at: task_record.evidence().revision(),
            effect,
            consent: None,
            basis: None,
        },
        tick.clock,
    )?;
    match record.state() {
        EffectState::Applied { .. } => {
            confirm_reported(tick.store, tick.claimant, &exhaustion, tick.clock.now())?;
            Ok(Delivery::Delivered(exhaustion))
        }
        EffectState::Intended
        | EffectState::Uncertain { .. }
        | EffectState::NotApplied { .. }
        | EffectState::Unresolvable { .. }
        | EffectState::Waived { .. } => Ok(Delivery::NotDelivered {
            exhaustion,
            record: Box::new(record),
        }),
    }
}

/// Settle the budget tasks of earlier windows that no live tick holds,
/// including one a crashed tick left claimed: reconcile their unresolved
/// effects and finish them once nothing is unresolved. A task that is still
/// unresolved, or whose claim or lookup fails now, is left for a later tick.
///
/// # Errors
/// Only a store read of the task list, and a claim that cannot be given back.
fn settle_earlier(tick: &Tick<'_>, current: &TaskId) -> Result<Vec<TaskId>> {
    let earlier: Vec<TaskId> = tick
        .store
        .tasks()?
        .into_iter()
        .filter(|record| {
            let id = record.spec().id.as_str();
            let window = id
                .strip_prefix(WORKFLOW)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|start| {
                    !start.is_empty() && start.bytes().all(|b| b.is_ascii_digit())
                });
            record.spec().id != *current
                && window
                && !matches!(record.state(), TaskState::Settled { .. })
        })
        .map(|record| record.spec().id.clone())
        .collect();
    let mut settled = Vec::with_capacity(earlier.len());
    for task in earlier {
        let Ok(fence) = claim(tick, &task) else {
            continue;
        };
        if let Ok(true) = finish(tick, &task, fence) {
            settled.push(task);
        } else {
            tick.store.relinquish(&task, fence, tick.clock.now())?;
        }
    }
    Ok(settled)
}

/// Finish an earlier window's task when nothing on it is unresolved.
fn finish(tick: &Tick<'_>, task: &TaskId, fence: Fence) -> Result<bool> {
    reconcile(tick, task, fence)?;
    let record = tick.store.task(task)?;
    if record.unresolved_effects().next().is_some() {
        return Ok(false);
    }
    let Some(attempt) = tick.store.continue_attempt(task, fence, tick.clock.now())? else {
        return Ok(false);
    };
    tick.store.finish_attempt(
        task,
        fence,
        attempt,
        AttemptOutcome::Succeeded,
        tick.clock.now(),
    )?;
    Ok(true)
}

/// A report post's name: one per schedule and window, like its marker, so a
/// tick that finds an earlier post of it uncertain looks it up instead of
/// posting again, even after the exhausted limit or its allowance changed.
fn report_name(exhaustion: &BudgetExhaustion) -> Result<EffectName> {
    let schedule = &exhaustion.schedule;
    digest_name(
        "budget-report-",
        &[
            schedule.backend.as_str(),
            schedule.handle.as_str(),
            &exhaustion.window.start.as_unix_millis().to_string(),
        ],
    )
}

/// The pause effect's name: one per schedule, window, and observation.
fn pause_name(exhaustion: &BudgetExhaustion, observed_at: Timestamp) -> Result<EffectName> {
    let schedule = &exhaustion.schedule;
    digest_name(
        "budget-pause-",
        &[
            schedule.backend.as_str(),
            schedule.handle.as_str(),
            &exhaustion.window.start.as_unix_millis().to_string(),
            &observed_at.as_unix_millis().to_string(),
        ],
    )
}

/// `prefix` and a digest of `parts`, because a backend handle can exceed an
/// effect name's length and alphabet.
fn digest_name(prefix: &str, parts: &[&str]) -> Result<EffectName> {
    let mut digest = Sha256::new();
    for part in parts {
        // Length-prefixed, so no two part lists share a digest input.
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    let mut name = String::from(prefix);
    for byte in digest.finalize().iter().take(16) {
        let _ = write!(name, "{byte:02x}");
    }
    Ok(EffectName::new(&name)?)
}

/// The exhausted schedules to act on. A marker that cannot be read fails
/// the pass rather than counting as reported or unreported.
fn plan(
    store: &HouseStore,
    house: &HouseId,
    policy: &SchedulePolicy,
    evidence: &ScheduleEvidence,
) -> Result<Vec<BudgetExhaustion>> {
    let failure = RefCell::new(None);
    let plan = policy.plan_exhaustion(house, evidence, |key| match store.marker(key) {
        Ok(marker) => marker.is_some(),
        Err(error) => {
            failure.borrow_mut().get_or_insert(error);
            true
        }
    })?;
    match failure.into_inner() {
        Some(error) => Err(error),
        None => Ok(plan),
    }
}
