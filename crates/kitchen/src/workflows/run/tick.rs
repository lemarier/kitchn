//! The house tick's pass runner: each due pass of [`crate::workflows::tick`]
//! runs the scheduled pass of this module in process, under the tick's run.
//!
//! The scheduled pass takes its own workflow lease as it does for
//! `kitchn run`, so a tick and a `kitchn run` of the same pass never act at
//! once: the second finds the lease held and reports
//! [`PassFailure::Busy`]. The pass records every task on the tick run before
//! it touches it and renews the run with its own lease. The tick never takes
//! over an expired lease or task claim; that stays a person's decision
//! ([`PassFailure::OwnerUncertain`]).

use super::{
    CoordinatePass, GatePass, Outcome, PickupAction, PickupPass, PickupSettings, RepairPass,
    RunError,
};
use crate::{
    ErrorClass,
    contracts::{Clock, CoordinatorMailbox, ExternalRef, Repository},
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport},
    state::HouseStore,
    workflows::tick::{
        MAX_RUN_EVIDENCE, Pass, PassFailure, PassOutcome, PassReport, PassRun, PassRunner,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Runs the house tick's passes through [`PickupPass`], [`CoordinatePass`],
/// [`RepairPass`], and [`GatePass`].
pub struct TickPasses<'a, T> {
    /// The house store.
    pub store: &'a HouseStore,
    /// The house configuration.
    pub house: &'a HouseConfig,
    /// The house's worker backend. Pickup, coordination, and repair need
    /// it; without one they fail with [`RunError::NoBackend`].
    pub backend: Option<&'a dyn CoordinatorMailbox>,
    /// The house's forge reads.
    pub forge: &'a GitHubClient<T>,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// The repository pickup, repair, and the gate serve.
    pub repository: &'a Repository,
    /// What pickup picks up, for `repository`. Without it pickup fails with
    /// [`RunError::NoPickupSettings`].
    pub pickup: Option<&'a PickupSettings>,
    /// Pull request authors eligible for unattended merge, for the gate.
    pub authors: &'a [String],
}

impl<T: GitHubReadTransport> TickPasses<'_, T> {
    /// Run `pass` once under the tick's `run` and report how it ended.
    ///
    /// # Errors
    /// [`RunError::NoBackend`] for a pass that needs the worker backend when
    /// none was given, [`RunError::NoPickupSettings`] for pickup without its
    /// settings, and the pass's own errors.
    pub fn run_pass(&self, pass: Pass, run: &PassRun) -> Result<PassReport> {
        let backend = || self.backend.ok_or(RunError::NoBackend);
        let tick = Some(run);
        let repository = self.repository;
        Ok(match pass {
            Pass::Pickup => report(
                PickupPass {
                    store: self.store,
                    house: self.house,
                    backend: backend()?,
                    forge: self.forge,
                    clock: self.clock,
                    settings: self.pickup.ok_or(RunError::NoPickupSettings)?,
                    take_over: false,
                    tick,
                }
                .run()?,
                launched_worker,
            ),
            Pass::Coordinate => report(
                CoordinatePass {
                    store: self.store,
                    house: self.house,
                    backend: backend()?,
                    forge: self.forge,
                    clock: self.clock,
                    take_over: false,
                    tick,
                }
                .run()?,
                |_| None,
            ),
            Pass::Repair => report(
                RepairPass {
                    store: self.store,
                    house: self.house,
                    backend: backend()?,
                    forge: self.forge,
                    clock: self.clock,
                    repository,
                    take_over: false,
                    tick,
                }
                .run()?,
                |_| None,
            ),
            Pass::Gate => report(
                GatePass {
                    store: self.store,
                    house: self.house,
                    forge: self.forge,
                    clock: self.clock,
                    repository,
                    authors: self.authors,
                    take_over: false,
                    tick,
                }
                .run()?,
                |_| None,
            ),
        })
    }
}

impl<T: GitHubReadTransport> PassRunner for TickPasses<'_, T> {
    fn run(&mut self, pass: Pass, run: &PassRun) -> PassReport {
        self.run_pass(pass, run)
            .unwrap_or_else(|error| failed_report(&error))
    }
}

/// The report of a pass that stopped with `error`: refused for invalid
/// input or a refusal, an execution failure otherwise.
#[must_use]
pub fn failed_report(error: &crate::Error) -> PassReport {
    PassReport::new(PassOutcome::Failed {
        reason: match error.class() {
            ErrorClass::InvalidInput | ErrorClass::Refused => PassFailure::Refused,
            ErrorClass::Conflict | ErrorClass::Execution => PassFailure::Execution,
        },
    })
}

/// The tick's report of a scheduled pass's outcome, linking the backend
/// references `evidence` names.
fn report<A>(outcome: Outcome<A>, evidence: impl Fn(&A) -> Option<ExternalRef>) -> PassReport {
    let failed = |reason| PassReport::new(PassOutcome::Failed { reason });
    match outcome {
        Outcome::Idle => PassReport::new(PassOutcome::Idle),
        Outcome::Busy => failed(PassFailure::Busy),
        Outcome::OwnerUncertain { .. } => failed(PassFailure::OwnerUncertain),
        Outcome::Acted(actions) => PassReport {
            backend_runs: actions
                .iter()
                .filter_map(evidence)
                .take(MAX_RUN_EVIDENCE)
                .collect(),
            ..PassReport::new(PassOutcome::Done)
        },
    }
}

/// The backend's reference to the worker a pickup launch created.
fn launched_worker(action: &PickupAction) -> Option<ExternalRef> {
    match action {
        PickupAction::Launched { worker, .. } => Some(worker.handle.clone()),
        PickupAction::NotLaunched { .. }
        | PickupAction::NotClaimed { .. }
        | PickupAction::IssueClosed { .. }
        | PickupAction::StackedRetry { .. }
        | PickupAction::Moved { .. } => None,
    }
}
