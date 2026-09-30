//! The scheduled gate pass: evaluate the open pull requests of settled
//! scheduled tasks at their exact heads and report each verdict. It records
//! no verdict and performs no merge, fix request, or handover.

use std::fmt;

use super::{Outcome, Pass, RunError, kitchen_pull_requests};
use crate::workflows::tick::PassRun;
use crate::{
    ConsumerId, TaskId,
    contracts::{Clock, CommitId, Fence, IssueNumber, Repository},
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport},
    state::HouseStore,
    workflows::gate::{
        ForgeGatePolicy, GateGrants, GateHistory, GateSupplement, SemanticReview, Verdict,
        collect_forge_evidence, evaluate,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Pull requests one gate pass evaluates, as the gate's own bound.
pub const MAX_GATE_PULL_REQUESTS: usize = 3;

/// One scheduled gate pass over one repository.
pub struct GatePass<'a, T> {
    /// The house store.
    pub store: &'a HouseStore,
    /// The house configuration.
    pub house: &'a HouseConfig,
    /// The house's forge reads.
    pub forge: &'a GitHubClient<T>,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// The repository.
    pub repository: &'a Repository,
    /// Pull request authors eligible for unattended merge, such as the
    /// house's forge login.
    pub authors: &'a [String],
    /// Take over an expired pass lease instead of stopping.
    pub take_over: bool,
    /// The house tick's run this pass serves, if a tick started it. The
    /// pass only reads its tasks; it records each one it assesses.
    pub tick: Option<&'a PassRun>,
}

/// The verdict on one pull request at one head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateAction {
    /// The pull request.
    pub pull_request: IssueNumber,
    /// The task whose branch it is.
    pub task: TaskId,
    /// The head judged.
    pub head: CommitId,
    /// The verdict.
    pub verdict: Verdict,
}

impl fmt::Display for GateAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "pull request #{} task {} at {}: {:?}",
            self.pull_request.get(),
            self.task,
            self.head,
            self.verdict
        )
    }
}

impl<T: GitHubReadTransport> GatePass<'_, T> {
    /// Run one pass under the repository's gate lease.
    ///
    /// # Errors
    /// Refuses a repository outside the house before taking the lease.
    /// Returns forge read failures, which stop the pass, and store failures.
    pub fn run(&self) -> Result<Outcome<GateAction>> {
        if !self.house.repositories.contains(self.repository) {
            return Err(RunError::RepositoryOutsideHouse.into());
        }
        let consumer = Pass::Gate.consumer(self.repository)?;
        super::under_lease(self.store, &consumer, self.take_over, self.clock, |fence| {
            self.pass(&consumer, fence)
        })
    }

    /// Evaluate the pull requests, renewing the pass lease and the tick run
    /// before each forge lookup and each evaluation.
    fn pass(&self, consumer: &ConsumerId, fence: Fence) -> Result<Vec<GateAction>> {
        let renew = || super::renew(self.store, consumer, fence, self.tick, self.clock);
        let found = kitchen_pull_requests(self.store, self.forge, self.repository, &renew)?;
        let policy = ForgeGatePolicy::for_house(
            self.house,
            self.authors.to_vec(),
            self.house.required_reviewers.iter().cloned().collect(),
        );
        let mut actions = Vec::new();
        for pull_request in found.into_iter().take(MAX_GATE_PULL_REQUESTS) {
            renew()?;
            super::record(self.store, self.tick, &pull_request.task, self.clock)?;
            let evidence = collect_forge_evidence(
                self.forge,
                self.store.house(),
                self.repository,
                pull_request.pull_request.number,
                &policy,
                unattested(),
                self.clock.now(),
            )?;
            let decision = evaluate(&evidence, GateGrants::default(), GateHistory::default());
            actions.push(GateAction {
                pull_request: pull_request.pull_request.number,
                task: pull_request.task,
                head: decision.head,
                verdict: decision.verdict,
            });
        }
        Ok(actions)
    }
}

/// No independent review, acceptance, hardware, or risk evidence: a
/// scheduled pass has no attested source for them yet, so each stays
/// unknown and can never produce a merge verdict.
fn unattested() -> GateSupplement {
    GateSupplement {
        semantic_review: SemanticReview::Unavailable,
        semantic_source: None,
        verified_findings: Vec::new(),
        disproved_findings: Vec::new(),
        semantic_head: None,
        semantic_base: None,
        semantic_read_only: false,
        semantic_independent: false,
        acceptance_met: None,
        hardware_complete: None,
        risk_classes: None,
        risk_approval: None,
        writer_working: false,
        subject: None,
    }
}
