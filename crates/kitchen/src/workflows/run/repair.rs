//! The scheduled repair pass: assess the open pull requests of settled
//! scheduled tasks for conflict repair and restacking, and report each
//! decision. It launches no repair writer.

use std::fmt;

use super::{KitchenPullRequest, Outcome, Pass, RunError, kitchen_pull_requests};
use crate::workflows::tick::PassRun;
use crate::{
    TaskId,
    contracts::{Clock, IssueNumber, Repository, Settlement, WorkerBackend},
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport},
    selection::WorkType,
    state::{HouseStore, TaskState},
    workflows::{
        coordination::{BranchFact, current_worker},
        repair::{
            Observed, Ownership, PullRequestView, RepairCandidate, RepairDecision, RepairPolicy,
            WorktreeView, Writer, plan, repair_task_id,
        },
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Passes that may see unknown mergeability before the pull request is
/// handed over.
const MAX_UNKNOWN_RECHECKS: u8 = 3;

/// One scheduled repair pass over one repository.
pub struct RepairPass<'a, T> {
    /// The house store.
    pub store: &'a HouseStore,
    /// The house configuration.
    pub house: &'a HouseConfig,
    /// The house's worker backend, to observe branch writers.
    pub backend: &'a dyn WorkerBackend,
    /// The house's forge reads.
    pub forge: &'a GitHubClient<T>,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// The repository.
    pub repository: &'a Repository,
    /// Take over an expired pass lease instead of stopping.
    pub take_over: bool,
    /// The house tick's run this pass serves, if a tick started it. The
    /// pass only reads its tasks; it records each one it assesses.
    pub tick: Option<&'a PassRun>,
}

/// What a repair pass decided about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// The repair decision.
    Decided {
        /// The pull request.
        pull_request: IssueNumber,
        /// The task whose branch it is.
        task: TaskId,
        /// The decision.
        decision: RepairDecision,
    },
    /// A stacked layer; its stack position is not observed here, so it is
    /// not assessed.
    Stacked {
        /// The pull request.
        pull_request: IssueNumber,
        /// The task whose branch it is.
        task: TaskId,
    },
}

impl fmt::Display for RepairAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decided {
                pull_request,
                task,
                decision,
            } => write!(
                formatter,
                "pull request #{} task {task}: {decision:?}",
                pull_request.get()
            ),
            Self::Stacked { pull_request, task } => write!(
                formatter,
                "pull request #{} task {task}: stacked, not assessed",
                pull_request.get()
            ),
        }
    }
}

impl<T: GitHubReadTransport> RepairPass<'_, T> {
    /// Run one pass under the repository's repair lease.
    ///
    /// # Errors
    /// Refuses a repository outside the house before taking the lease.
    /// Returns forge read failures, which stop the pass, and store failures.
    pub fn run(&self) -> Result<Outcome<RepairAction>> {
        if !self.house.repositories.contains(self.repository) {
            return Err(RunError::RepositoryOutsideHouse.into());
        }
        let consumer = Pass::Repair.consumer(self.repository)?;
        super::under_lease(self.store, &consumer, self.take_over, self.clock, |_| {
            self.pass()
        })
    }

    fn pass(&self) -> Result<Vec<RepairAction>> {
        let found = kitchen_pull_requests(self.store, self.forge, self.repository)?;
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let policy = RepairPolicy::for_house(self.house, MAX_UNKNOWN_RECHECKS);
        let tasks = self.store.tasks()?;
        let in_flight = tasks
            .iter()
            .filter(|record| {
                record.spec().work_type == Some(WorkType::fix())
                    && record.spec().repository.as_ref() == Some(self.repository)
                    && !matches!(record.state(), TaskState::Settled { .. })
            })
            .count();
        let mut actions = Vec::new();
        let mut candidates = Vec::with_capacity(found.len());
        let mut owners = Vec::with_capacity(found.len());
        for pull_request in found {
            super::record(self.store, self.tick, &pull_request.task, self.clock)?;
            let record = self.store.task(&pull_request.task)?;
            if BranchFact::Stacked.holds(&record, &pull_request.branch) {
                actions.push(RepairAction::Stacked {
                    pull_request: pull_request.pull_request.number,
                    task: pull_request.task,
                });
                continue;
            }
            let writer = current_worker(&record).map_or(Writer::None, |view| {
                Writer::observed(
                    &pull_request.task,
                    self.backend.observe_worker(&view.worker),
                )
            });
            candidates.push(self.candidate(&pull_request, writer, &tasks, policy)?);
            owners.push((pull_request.pull_request.number, pull_request.task));
        }
        for (number, decision) in plan(&policy, &candidates, in_flight) {
            if let Some((_, task)) = owners.iter().find(|(owned, _)| *owned == number) {
                actions.push(RepairAction::Decided {
                    pull_request: number,
                    task: task.clone(),
                    decision,
                });
            }
        }
        Ok(actions)
    }

    /// The repair facts of one pull request. The writer's checkout is not
    /// observed from a scheduled pass, so its state is unknown, and a
    /// conflict is handed over rather than repaired over unseen work.
    fn candidate(
        &self,
        found: &KitchenPullRequest,
        writer: Writer,
        tasks: &[crate::state::TaskRecord],
        policy: RepairPolicy,
    ) -> Result<RepairCandidate> {
        let number = found.pull_request.number;
        let mut rounds_used: u8 = 0;
        for round in 0..=policy.budget().fix_rounds() {
            let id = repair_task_id(self.repository, number, round)?;
            if tasks.iter().any(|record| record.spec().id == id) {
                rounds_used = rounds_used.saturating_add(1);
            }
        }
        Ok(RepairCandidate {
            repository: self.repository.clone(),
            pull_request: PullRequestView::from_github(&found.pull_request),
            branch: found.branch.clone(),
            ownership: Ownership::Settled {
                task: found.task.clone(),
                settlement: Settlement::Succeeded,
            },
            writer,
            worktree: WorktreeView {
                dirty: Observed::Unknown,
                unpushed: Observed::Unknown,
            },
            stack: None,
            unknown_rechecks: 0,
            rounds_used,
        })
    }
}
