//! Fixtures shared by the pickup, coordination, and repair workflow tests.
//! Everything runs against temporary stores and the in-memory fake backend:
//! simulated evidence, not live runtime evidence.

#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::time::Duration;

use kitchen::{
    BackendId, ConsumerId, TaskId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Claimant, Consent,
        Effect, EffectExecutor, EffectFailure, EffectRequest, EvidenceRevision, ExternalRef,
        HouseGrants, IssueNumber, Lookup, Operation, Permission, Provenance, Receipt, Repository,
        ResourceKind, ResourceRef, RetryPolicy, TaskAuthority, Text, Timestamp, WorkerBackend,
        WorkerState, fake::FakeBackend,
    },
    workflows::{
        coordination::{ConsentSource, Context, Standing, SupervisionPolicy},
        pickup::{
            Base, Blockers, Candidate, FollowUpBudget, IssueRef, LinkedWork, Overlap, PickupPolicy,
            PinnedInstructions, Readiness, TaskTemplate, WorkerBrief,
        },
    },
};

use crate::common::{
    Fixture, ManualClock, TestResult, WORKER_PERMISSIONS, backend_id, commit, credential, grant,
    house, ttl,
};

pub fn repo() -> TestResult<Repository> {
    Ok(Repository::new("origin89hq/firmware")?)
}

pub fn issue(number: u64) -> TestResult<IssueRef> {
    Ok(IssueRef {
        repository: repo()?,
        number: IssueNumber::new(number)?,
    })
}

/// A ready, unblocked, unlinked candidate.
pub fn ready(number: u64) -> TestResult<Candidate> {
    Ok(Candidate {
        issue: issue(number)?,
        readiness: Readiness::Ready,
        human_only: false,
        assigned: false,
        blockers: Blockers::Known(Vec::new()),
        prose_dependencies: Vec::new(),
        linked: LinkedWork::None,
        overlap: Overlap::None,
        blocks_open: 0,
        milestone_due: None,
        created_at: Timestamp::from_unix_millis(number),
    })
}

pub fn policy(capacity: u32) -> TestResult<PickupPolicy> {
    Ok(PickupPolicy {
        repositories: vec![repo()?],
        capacity,
    })
}

pub fn roger_backend_id() -> TestResult<BackendId> {
    Ok(BackendId::new("roger")?)
}

/// Standing grants: worker lifecycle on the fake orchestrator and asks on
/// the fake Roger service.
pub fn house_grants() -> TestResult<HouseGrants> {
    let mut grants: Vec<_> = WORKER_PERMISSIONS
        .iter()
        .map(|permission| grant(*permission))
        .collect::<TestResult<_>>()?;
    grants.push(kitchen::contracts::Grant::house(
        Permission::AskHuman,
        roger_backend_id()?,
        credential()?,
    ));
    Ok(HouseGrants::new(house()?, grants))
}

pub fn provenance(fill: char) -> TestResult<Provenance> {
    Ok(Provenance {
        kitchen: commit(fill)?,
        house_guidance: commit('b')?,
        repository_instructions: Some(commit('c')?),
    })
}

pub fn template_with(attempts: u32, pins: Provenance) -> TestResult<TaskTemplate> {
    let grants = house_grants()?;
    let requested: Vec<_> = WORKER_PERMISSIONS
        .iter()
        .map(|permission| grant(*permission))
        .chain([Ok(kitchen::contracts::Grant::house(
            Permission::AskHuman,
            roger_backend_id()?,
            credential()?,
        ))])
        .collect::<TestResult<_>>()?;
    Ok(TaskTemplate {
        authority: TaskAuthority::delegate(&grants, requested)?,
        retry: RetryPolicy::new(attempts, Duration::from_secs(24 * 3600))?,
        provenance: pins,
        // Worker needs apply to the worker backend only; Roger asks in the
        // same task are not refused for lacking them.
        requires: kitchen::contracts::CapabilityRequirements::new().with(
            kitchen::contracts::ExecutorKind::Worker,
            kitchen::workflows::coordination::REQUIRED_WORKER_CAPABILITIES,
        ),
    })
}

pub fn template() -> TestResult<TaskTemplate> {
    template_with(3, provenance('a')?)
}

pub fn consumer() -> TestResult<ConsumerId> {
    Ok(ConsumerId::new("pickup-origin89")?)
}

pub fn supervision() -> TestResult<SupervisionPolicy> {
    Ok(SupervisionPolicy {
        readiness_deadline: Duration::from_secs(120),
        question_deadline: Duration::from_secs(600),
        idle_deadline: Duration::from_secs(240),
        claim_ttl: ttl(300)?,
    })
}

pub fn branch(value: &str) -> TestResult<kitchen::contracts::BranchName> {
    Ok(kitchen::workflows::pickup::work_branch(value)?)
}

pub fn brief(number: u64) -> TestResult<WorkerBrief> {
    Ok(WorkerBrief {
        issue: issue(number)?,
        branch: branch(&format!("lemarier/issue-{number}"))?,
        base: Base::DefaultBranch,
        instructions: PinnedInstructions {
            house: house()?,
            provenance: provenance('a')?,
            entrypoint: Text::new("snapshots/origin89/AGENTS.md")?,
        },
        acceptance: vec![Text::new("The firmware builds with the new driver.")?],
        budget: FollowUpBudget {
            fix_rounds: 2,
            review_requests: 1,
        },
        report_path: Text::new("reports/issue.md")?,
    })
}

/// A person who approves exactly the effect shown, at the shown revision.
pub struct Approves {
    pub person: kitchen::HolderId,
    pub given: std::cell::Cell<u32>,
}

impl Approves {
    pub fn new(person: &str) -> TestResult<Self> {
        Ok(Self {
            person: kitchen::HolderId::new(person)?,
            given: std::cell::Cell::new(0),
        })
    }
}

impl ConsentSource for Approves {
    fn consent(
        &self,
        task: &TaskId,
        effect: &Effect,
        revision: EvidenceRevision,
    ) -> Option<Consent> {
        self.given.set(self.given.get() + 1);
        Some(Consent {
            id: ExternalRef::new(&format!("consent-{task}-{}", self.given.get())).ok()?,
            given_by: self.person.clone(),
            house: house().ok()?,
            task: task.clone(),
            effect: effect.clone(),
            revision,
        })
    }
}

/// A store, a fake orchestrator, a fake Roger, grants, and a manual clock.
pub struct World {
    pub fixture: Fixture,
    pub backend: FakeBackend,
    pub roger: FakeBackend,
    pub grants: HouseGrants,
    pub clock: ManualClock,
}

impl World {
    pub fn new() -> TestResult<Self> {
        Self::with_capabilities(CapabilitySet::supporting(Capability::ALL))
    }

    pub fn with_capabilities(capabilities: CapabilitySet) -> TestResult<Self> {
        Ok(Self {
            fixture: Fixture::new()?,
            backend: FakeBackend::new(backend_id()?, house()?, capabilities),
            roger: FakeBackend::new(
                roger_backend_id()?,
                house()?,
                CapabilitySet::supporting([Capability::AskHuman, Capability::EffectLookup]),
            ),
            grants: house_grants()?,
            clock: ManualClock::starting_at(1_000),
        })
    }

    pub fn ctx(&self) -> Context<'_> {
        self.ctx_with(&Standing)
    }

    pub fn ctx_with<'a>(&'a self, consent: &'a dyn ConsentSource) -> Context<'a> {
        Context {
            store: &self.fixture.store,
            backend: &self.backend,
            grants: &self.grants,
            clock: &self.clock,
            consent,
        }
    }

    pub fn now(&self) -> Timestamp {
        use kitchen::contracts::Clock;
        self.clock.now()
    }
}

/// A scheduled claimant acting under a fresh consumer lease.
pub fn under_consumer(
    world: &World,
    holder: &str,
) -> TestResult<(Claimant, kitchen::state::Lease)> {
    let claimant = crate::common::scheduled(holder)?;
    let lease =
        world
            .fixture
            .store
            .acquire_consumer(&consumer()?, &claimant, ttl(600)?, world.now())?;
    Ok((claimant.under(consumer()?, lease.fence()), lease))
}

/// The fake orchestrator behind an adapter that reports the branch its
/// worktree really got in the launch receipt, as the Orca adapter does. Orca
/// prefixes requested names, so an adapter may report a branch other than the
/// one asked for.
pub struct ReportsBranch<'a> {
    pub inner: &'a FakeBackend,
    pub branch: &'a str,
    /// Refuse every stop request, as a backend that cannot reach the worker.
    pub refuse_stop: bool,
}

impl EffectExecutor for ReportsBranch<'_> {
    fn descriptor(&self) -> &BackendDescriptor {
        self.inner.descriptor()
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        if self.refuse_stop
            && matches!(
                request.effect(),
                Effect::Worker(Operation::CancelWorker { .. })
            )
        {
            return Err(EffectFailure::NotApplied(
                kitchen::contracts::NotAppliedReason::Rejected,
            ));
        }
        let receipt = self.inner.execute(request)?;
        if !matches!(
            request.effect(),
            Effect::Worker(Operation::LaunchWorker { .. })
        ) {
            return Ok(receipt);
        }
        let branch = ExternalRef::new(self.branch).map_err(|_| {
            EffectFailure::NotApplied(kitchen::contracts::NotAppliedReason::Rejected)
        })?;
        let mut created = receipt.created().to_vec();
        created.push(ResourceRef {
            kind: ResourceKind::Branch,
            backend: self.inner.descriptor().backend.clone(),
            handle: branch,
        });
        Receipt::new(
            receipt.reference().clone(),
            created,
            receipt.touched().to_vec(),
        )
        .map_err(|_| EffectFailure::NotApplied(kitchen::contracts::NotAppliedReason::Rejected))
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(request)
    }
}

impl WorkerBackend for ReportsBranch<'_> {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.inner.observe_worker(worker)
    }
}

/// Recovery signals for `worker`: working, agent's terminal, no provider
/// error, and a transcript whose last activity is `last_activity`.
pub fn signals(
    worker: &ResourceRef,
    last_activity: Option<Timestamp>,
) -> kitchen::workflows::recovery::RecoverySignals {
    use kitchen::workflows::recovery::{
        PromptState, RecoverySignals, StartEvidence, TerminalHolder, TranscriptProgress,
    };
    RecoverySignals {
        worker: worker.clone(),
        start: StartEvidence::TurnObserved,
        prompt: PromptState::Working,
        transcript: Some(TranscriptProgress {
            agent_spoke: true,
            last_activity,
        }),
        terminal: TerminalHolder::Agent,
        provider: None,
    }
}

/// Positive proof that `worker`'s first turn never started: an empty,
/// readable transcript and an idle prompt.
pub fn never_started(worker: &ResourceRef) -> kitchen::workflows::recovery::RecoverySignals {
    use kitchen::workflows::recovery::{PromptState, StartEvidence, TranscriptProgress};
    kitchen::workflows::recovery::RecoverySignals {
        start: StartEvidence::NoTurn,
        prompt: PromptState::Idle,
        transcript: Some(TranscriptProgress {
            agent_spoke: false,
            last_activity: None,
        }),
        ..signals(worker, None)
    }
}

/// Kitchen's own Git configuration for a test push, written to `dir` (a
/// directory outside every checkout) with `settings`.
pub fn isolated_config(
    dir: &std::path::Path,
    settings: &[kitchen::workflows::push::PushSetting],
) -> TestResult<kitchen::workflows::push::IsolatedGitConfig> {
    Ok(kitchen::workflows::push::IsolatedGitConfig::create(
        std::path::Path::new("/usr/bin/git"),
        dir.join("kitchen-gitconfig"),
        settings,
        Duration::from_secs(10),
    )?)
}
