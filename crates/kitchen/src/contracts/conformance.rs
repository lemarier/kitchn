//! Shared contract checks every [`EffectExecutor`] must pass, plus the
//! worker checks every [`WorkerBackend`] must pass.
//!
//! The suite performs real effects: [`run`] applies the caller's probe effect
//! once, and [`run_worker`] launches one worker and cancels it (when
//! supported). Run it against fakes offline and against a real executor only
//! in a controlled environment where those effects are authorized. A passing
//! run is evidence about the executor it ran against, and only for the paths
//! it exercised.
//!
//! [`run_mailbox`] checks the coordinator mailbox against batches the caller
//! seeded; it acknowledges them, so it consumes the seeded messages.
//! Coordination cannot run without worker deliveries, so a backend that does
//! not declare them fails it.
//!
//! The worker launch requests a branch. [`run_worker`] uses
//! `kitchen/<run_tag>`; a backend that can only create branches under a
//! host-chosen prefix passes a branch it can obtain to
//! [`run_worker_on_branch`].

use std::{fmt, time::Duration};

use crate::{
    BackendId, ConsumerId, CredentialId, HouseId, TaskId,
    contracts::{
        AskKind, AskRisk, AttemptNumber, BackendUnavailable, BranchName, Capability,
        CoordinatorMailbox, DecisionBinding, DecisionOwner, Delivery, Effect, EffectExecutor,
        EffectFailure, EffectRequest, EvidenceRevision, ExternalRef, GitHubAction, GitHubEffect,
        GitHubMutation, IdempotencyKey, LabelDefinition, Liveness, Lookup, MAX_INVENTORY_RESOURCES,
        MailboxError, NotAppliedReason, Operation, Permission, PostingBudget, Receipt, Repository,
        ResourceKind, ResourceRef, RogerAsk, RogerEffect, Role, ScheduleEffect, Text,
        WorkerBackend, WorkerState, Workspace,
    },
    scheduling::{AgentFamily, Recurrence, ScheduleSpec, Timezone, WorkflowName},
    selection::{AgentSelection, ResolvedSelection},
};

/// One contract check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Check {
    /// The fixture's run tag leaves room for derived keys and handles.
    Fixture,
    /// The descriptor names the fixture's house.
    DescriptorHouse,
    /// A request for another house is refused without effect.
    CrossHouseRefused,
    /// A request persisted for another backend namespace is refused without effect.
    ForeignBackendRefused,
    /// Effects without full capability support are refused without effect.
    UnsupportedRefused,
    /// A never-submitted request is not reported as applied.
    UnknownKeyNotApplied,
    /// The probe effect returns a receipt.
    ProbeReceipt,
    /// Lookup of the probe key returns the probe receipt.
    LookupMatchesReceipt,
    /// Resubmitting the probe key returns the same receipt.
    IdempotentResubmission,
    /// A launch receipt names a worker on this backend and exactly the
    /// requested branch.
    LaunchReceipt,
    /// A launch naming an agent selection the descriptor does not declare
    /// support for is refused without effect, never run on a default agent.
    SelectionRefused,
    /// The launched worker is observable and not reported as settled.
    LaunchObservable,
    /// The inventory lists the launched worker as live, within its bound.
    InventoryListsLaunch,
    /// A message to the launched worker honors the executor's per-kind
    /// lookup and idempotency declarations.
    MessageRecovery,
    /// A cancelled worker is reported settled; a missing record is not evidence.
    CancelObserved,
    /// Releasing the cancelled worker names no branch in its receipt, and a
    /// branch the inventory listed before is still listed after. Without a
    /// declared inventory, only the receipt is checked.
    ReleaseKeepsBranch,
    /// The backend declares worker deliveries, which coordination requires.
    DeliveriesDeclared,
    /// An unacknowledged batch is delivered again on every read.
    DeliveryReplayed,
    /// After a restart, the adopting coordinator receives the unacknowledged
    /// batch; the previous one is fenced from reading, acknowledging, and
    /// waiting, and the batch stays with the adopter. Undeclared adoption is
    /// refused.
    AdoptionReplays,
    /// Repeating an acknowledgement succeeds and consumes no later batch.
    DuplicateAcknowledgement,
    /// Every seeded message arrives once, in the order it was sent.
    DeliveryOrder,
}

impl fmt::Display for Check {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Fixture => "fixture",
            Self::DescriptorHouse => "descriptor house",
            Self::CrossHouseRefused => "cross-house request refused",
            Self::ForeignBackendRefused => "foreign-backend request refused",
            Self::UnsupportedRefused => "unsupported effect refused",
            Self::UnknownKeyNotApplied => "unknown key not applied",
            Self::ProbeReceipt => "probe receipt",
            Self::LookupMatchesReceipt => "lookup matches receipt",
            Self::IdempotentResubmission => "idempotent resubmission",
            Self::LaunchReceipt => "launch receipt",
            Self::SelectionRefused => "undeclared agent selection refused",
            Self::LaunchObservable => "launch observable",
            Self::InventoryListsLaunch => "inventory lists launch",
            Self::MessageRecovery => "message recovery as declared",
            Self::CancelObserved => "cancel observed",
            Self::ReleaseKeepsBranch => "release keeps the branch",
            Self::DeliveriesDeclared => "worker deliveries declared",
            Self::DeliveryReplayed => "unacknowledged delivery replayed",
            Self::AdoptionReplays => "adoption after restart replays the mailbox",
            Self::DuplicateAcknowledgement => "duplicate acknowledgement",
            Self::DeliveryOrder => "delivery order",
        })
    }
}

/// How a check ended when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckResult {
    /// The executor met the contract.
    Passed,
    /// Not exercised: the executor does not declare the capability.
    NotApplicable {
        /// The undeclared capability.
        requires: Capability,
    },
    /// Not exercised: the executor declares everything the check would
    /// refuse, so there is nothing undeclared to try.
    NothingUndeclared,
}

/// A contract violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("executor contract check '{check}' failed: {problem}")]
pub struct ConformanceFailure {
    /// The failed check.
    pub check: Check,
    /// What was observed.
    pub problem: &'static str,
}

/// Results of a passing run, in execution order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConformanceReport {
    /// Each check and its result.
    pub results: Vec<(Check, CheckResult)>,
}

impl ConformanceReport {
    /// The result of `check`, if it ran.
    #[must_use]
    pub fn result(&self, check: Check) -> Option<CheckResult> {
        self.results
            .iter()
            .find(|(ran, _)| *ran == check)
            .map(|(_, result)| *result)
    }
}

/// Inputs for one conformance run.
///
/// The probe launch requests `kitchen/<run_tag>` unless the caller names the
/// branch with [`run_worker_on_branch`].
#[derive(Debug, Clone)]
pub struct ConformanceFixture {
    /// The house the executor serves.
    pub house: HouseId,
    /// Another house, used to check cross-house refusal.
    pub foreign_house: HouseId,
    /// Another backend namespace, used to check foreign-backend refusal.
    pub foreign_backend: BackendId,
    /// The credential reference the run's requests name.
    pub credential: CredentialId,
    /// A disposable task identity for the run's requests.
    pub task: TaskId,
    /// A repository for sample forge effects.
    pub repository: Repository,
    /// A tag unique to this run, so idempotency keys never collide with earlier runs.
    pub run_tag: ExternalRef,
    /// Text for briefs, questions, and label names.
    pub brief: Text,
}

struct Runner<'a> {
    executor: &'a dyn EffectExecutor,
    fixture: &'a ConformanceFixture,
    /// The branch the caller chose for the probe launch, if any.
    branch: Option<&'a BranchName>,
    report: ConformanceReport,
}

fn fail<T>(check: Check, problem: &'static str) -> Result<T, ConformanceFailure> {
    Err(ConformanceFailure { check, problem })
}

/// Run the executor checks, applying `probe` once where its capability is
/// declared.
///
/// # Errors
/// Returns the first [`ConformanceFailure`].
pub fn run(
    executor: &dyn EffectExecutor,
    fixture: &ConformanceFixture,
    probe: &Effect,
) -> Result<ConformanceReport, ConformanceFailure> {
    let mut runner = Runner::new(executor, fixture, None);
    runner.executor_checks(probe)?;
    Ok(runner.report)
}

/// Run the executor checks with a worker launch as the probe, then the
/// worker checks: the receipt names a worker, which is observable, listed
/// by the inventory, and stops when cancelled.
///
/// The launch requests the branch `kitchen/<run_tag>`. A backend that cannot
/// create that branch, such as one whose host adds its own prefix, uses
/// [`run_worker_on_branch`].
///
/// # Errors
/// Returns the first [`ConformanceFailure`].
pub fn run_worker(
    backend: &dyn WorkerBackend,
    fixture: &ConformanceFixture,
) -> Result<ConformanceReport, ConformanceFailure> {
    worker_checks(backend, fixture, None)
}

/// [`run_worker`] with the launch requesting `branch`, which the backend under
/// test supplies because only it knows which branches its host can create.
///
/// The receipt must name exactly `branch`: another branch, or a second one, is
/// a [`Check::LaunchReceipt`] failure, whatever `kitchen/<run_tag>` would have
/// been.
///
/// # Errors
/// Returns the first [`ConformanceFailure`].
pub fn run_worker_on_branch(
    backend: &dyn WorkerBackend,
    fixture: &ConformanceFixture,
    branch: &BranchName,
) -> Result<ConformanceReport, ConformanceFailure> {
    worker_checks(backend, fixture, Some(branch))
}

/// Check the coordinator mailbox of `coordinator`, whose unacknowledged
/// batches hold exactly the messages `sent`, in the order workers sent them.
/// `restarted` is a second instance for the same run, standing for the
/// coordinator after a restart: where run transfer is declared it adopts the
/// run and finishes the checks. Both instances must serve the same house and
/// backend namespace.
///
/// # Errors
/// Returns the first [`ConformanceFailure`]; a backend that does not declare
/// [`Capability::WorkerDeliveries`] fails [`Check::DeliveriesDeclared`].
pub fn run_mailbox(
    coordinator: &dyn CoordinatorMailbox,
    restarted: &dyn CoordinatorMailbox,
    sent: &[ExternalRef],
) -> Result<ConformanceReport, ConformanceFailure> {
    let mut report = ConformanceReport::default();
    let descriptor = coordinator.descriptor();
    if sent.is_empty()
        || restarted.descriptor().house != descriptor.house
        || restarted.descriptor().backend != descriptor.backend
    {
        return fail(
            Check::Fixture,
            "mailbox fixture needs seeded messages on one backend",
        );
    }
    let declared = |capability| descriptor.capabilities.support(capability).is_some();
    if !declared(Capability::WorkerDeliveries) {
        return fail(
            Check::DeliveriesDeclared,
            "coordination requires worker deliveries, which the backend does not declare",
        );
    }
    report
        .results
        .push((Check::DeliveriesDeclared, CheckResult::Passed));

    let first = match coordinator.next_delivery() {
        Ok(Some(first)) => first,
        Ok(None) => {
            return fail(
                Check::DeliveryReplayed,
                "the seeded messages were not delivered",
            );
        }
        Err(_) => return fail(Check::DeliveryReplayed, "the mailbox could not be read"),
    };
    if coordinator.next_delivery() != Ok(Some(first.clone())) {
        return fail(
            Check::DeliveryReplayed,
            "an unacknowledged batch was not replayed",
        );
    }
    report
        .results
        .push((Check::DeliveryReplayed, CheckResult::Passed));

    let consumer = if declared(Capability::RunTransfer) {
        if restarted.adopt_run().is_err() {
            return fail(
                Check::AdoptionReplays,
                "the restarted coordinator could not adopt the run",
            );
        }
        if restarted.next_delivery() != Ok(Some(first.clone())) {
            return fail(
                Check::AdoptionReplays,
                "the adopting coordinator did not receive the unacknowledged batch",
            );
        }
        if coordinator.next_delivery() != Err(MailboxError::Fenced) {
            return fail(
                Check::AdoptionReplays,
                "the previous coordinator still reads the mailbox",
            );
        }
        if coordinator.acknowledge(&first.id) != Err(MailboxError::Fenced) {
            return fail(
                Check::AdoptionReplays,
                "the previous coordinator's acknowledgement was not fenced",
            );
        }
        if restarted.next_delivery() != Ok(Some(first.clone())) {
            return fail(
                Check::AdoptionReplays,
                "the unacknowledged batch left the adopting coordinator",
            );
        }
        if coordinator.await_delivery(FENCED_WAIT) != Err(MailboxError::Fenced) {
            return fail(
                Check::AdoptionReplays,
                "the previous coordinator still waits on the mailbox",
            );
        }
        report
            .results
            .push((Check::AdoptionReplays, CheckResult::Passed));
        restarted
    } else {
        let unsupported = Err(MailboxError::Unavailable(BackendUnavailable::Unsupported(
            Capability::RunTransfer,
        )));
        if restarted.adopt_run() != unsupported {
            return fail(
                Check::AdoptionReplays,
                "undeclared adoption was not refused",
            );
        }
        report.results.push((
            Check::AdoptionReplays,
            CheckResult::NotApplicable {
                requires: Capability::RunTransfer,
            },
        ));
        coordinator
    };

    let received = drain(consumer, first, sent.len())?;
    report
        .results
        .push((Check::DuplicateAcknowledgement, CheckResult::Passed));
    if received != sent {
        return fail(
            Check::DeliveryOrder,
            "messages arrived out of order, missing, or more than once",
        );
    }
    report
        .results
        .push((Check::DeliveryOrder, CheckResult::Passed));
    Ok(report)
}

/// How long a fenced coordinator's wait may take in [`run_mailbox`]. A fenced
/// call returns at once and the adopter's batch is still waiting, so this
/// only bounds a backend that wrongly blocks.
const FENCED_WAIT: Duration = Duration::from_secs(1);

/// Acknowledge batches from `first` until none is left, repeating the first
/// acknowledgement once, and return every message id in delivery order.
fn drain(
    consumer: &dyn CoordinatorMailbox,
    first: Delivery,
    sent: usize,
) -> Result<Vec<ExternalRef>, ConformanceFailure> {
    let mut received = Vec::with_capacity(sent);
    let mut current = first;
    let mut repeated = false;
    // Each seeded batch holds at least one message, so a mailbox still
    // delivering past this bound is not draining.
    for _ in 0..=sent {
        received.extend(current.messages.iter().map(|message| message.id.clone()));
        let Ok(next) = consumer.acknowledge(&current.id) else {
            return fail(
                Check::DeliveryOrder,
                "an acknowledgement of the current batch failed",
            );
        };
        if !repeated {
            repeated = true;
            if consumer.acknowledge(&current.id) != Ok(next.clone())
                || consumer.next_delivery() != Ok(next.clone())
            {
                return fail(
                    Check::DuplicateAcknowledgement,
                    "a repeated acknowledgement failed or consumed a later batch",
                );
            }
        }
        match next {
            None => return Ok(received),
            Some(next) if next.id == current.id => {
                return fail(
                    Check::DeliveryOrder,
                    "an acknowledged batch was delivered again",
                );
            }
            Some(next) => current = next,
        }
    }
    fail(Check::DeliveryOrder, "the mailbox did not drain")
}

fn worker_checks(
    backend: &dyn WorkerBackend,
    fixture: &ConformanceFixture,
    branch: Option<&BranchName>,
) -> Result<ConformanceReport, ConformanceFailure> {
    let mut runner = Runner::new(backend, fixture, branch);
    let launch = runner.launch()?;
    let Some(receipt) = runner.executor_checks(&launch)? else {
        for check in [
            Check::LaunchReceipt,
            Check::SelectionRefused,
            Check::LaunchObservable,
            Check::InventoryListsLaunch,
            Check::MessageRecovery,
            Check::CancelObserved,
            Check::ReleaseKeepsBranch,
        ] {
            runner.record(
                check,
                CheckResult::NotApplicable {
                    requires: Capability::WorkerLaunchIsolated,
                },
            );
        }
        return Ok(runner.report);
    };
    let own = &backend.descriptor().backend;
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker && &resource.backend == own)
        .cloned();
    let Some(worker) = worker else {
        return fail(
            Check::LaunchReceipt,
            "receipt names no worker on this backend",
        );
    };
    // Exactly the requested branch: created once on this backend, and no
    // other branch created or touched.
    let requested = runner.branch()?;
    let mut branches = receipt
        .created()
        .iter()
        .chain(receipt.touched())
        .filter(|resource| resource.kind == ResourceKind::Branch);
    let exact = matches!(
        (branches.next(), branches.next()),
        (Some(branch), None) if &branch.backend == own
            && branch.handle.as_str() == requested.as_str()
            && receipt.created().contains(branch)
    );
    if !exact {
        return fail(
            Check::LaunchReceipt,
            "receipt does not name exactly the requested branch",
        );
    }
    runner.record(Check::LaunchReceipt, CheckResult::Passed);
    runner.selection_refused()?;
    runner.observable(backend, &worker)?;
    runner.inventory(backend, &worker)?;
    runner.message_recovery(backend, &worker)?;
    runner.cancel(backend, &worker)?;
    let branch = ResourceRef {
        kind: ResourceKind::Branch,
        backend: own.clone(),
        handle: ExternalRef::new(requested.as_str())
            .or_else(|_| fail(Check::Fixture, "branch is not a valid reference"))?,
    };
    runner.release_keeps_branch(backend, &worker, &branch)?;
    Ok(runner.report)
}

impl<'a> Runner<'a> {
    fn new(
        executor: &'a dyn EffectExecutor,
        fixture: &'a ConformanceFixture,
        branch: Option<&'a BranchName>,
    ) -> Self {
        Self {
            executor,
            fixture,
            branch,
            report: ConformanceReport::default(),
        }
    }

    fn supports(&self, capability: Capability) -> bool {
        self.executor.descriptor().capabilities.supports(capability)
    }

    fn record(&mut self, check: Check, result: CheckResult) {
        self.report.results.push((check, result));
    }

    fn reference(&self, suffix: &str) -> Result<ExternalRef, ConformanceFailure> {
        ExternalRef::new(&format!("{}-{suffix}", self.fixture.run_tag))
            .or_else(|_| fail(Check::Fixture, "run tag too long for a key"))
    }

    fn key(&self, suffix: &str) -> Result<IdempotencyKey, ConformanceFailure> {
        self.reference(suffix).map(IdempotencyKey::from_ref)
    }

    fn request(
        &self,
        house: &HouseId,
        backend: &BackendId,
        suffix: &str,
        effect: Effect,
    ) -> Result<EffectRequest, ConformanceFailure> {
        Ok(EffectRequest::new(
            house.clone(),
            backend.clone(),
            self.fixture.credential.clone(),
            self.fixture.task.clone(),
            AttemptNumber::FIRST,
            self.key(suffix)?,
            effect,
        ))
    }

    fn own_request(
        &self,
        suffix: &str,
        effect: Effect,
    ) -> Result<EffectRequest, ConformanceFailure> {
        let backend = self.executor.descriptor().backend.clone();
        self.request(&self.fixture.house, &backend, suffix, effect)
    }

    /// The branch the probe launch requests: the caller's, or one unique to
    /// this run.
    fn branch(&self) -> Result<BranchName, ConformanceFailure> {
        match self.branch {
            Some(branch) => Ok(branch.clone()),
            None => BranchName::new(&format!("kitchen/{}", self.fixture.run_tag))
                .or_else(|_| fail(Check::Fixture, "run tag is not a valid branch name")),
        }
    }

    fn launch(&self) -> Result<Effect, ConformanceFailure> {
        Ok(Effect::Worker(Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: self.fixture.brief.clone(),
            branch: Some(self.branch()?),
            agent: None,
        }))
    }

    /// A launch naming a selection the descriptor does not declare it can
    /// provide must be refused without effect: the executor may not start
    /// its default agent in its place.
    fn selection_refused(&mut self) -> Result<(), ConformanceFailure> {
        let descriptor = self.executor.descriptor();
        let example = match &descriptor.worker_selection {
            None => Some(AgentSelection::agent_default(AgentFamily::Claude)),
            Some(support) => support.undeclared_example(),
        };
        let Some(agent) = example else {
            // The descriptor claims every selection; nothing to refuse.
            self.record(Check::SelectionRefused, CheckResult::NothingUndeclared);
            return Ok(());
        };
        let effect = Effect::Worker(Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: self.fixture.brief.clone(),
            branch: Some(self.branch()?),
            agent: Some(agent),
        });
        let request = self.own_request("selection-refused", effect)?;
        match self.executor.execute(&request) {
            Err(EffectFailure::NotApplied(_)) => {}
            Ok(_) => return fail(Check::SelectionRefused, "an undeclared selection launched"),
            Err(_) => {
                return fail(
                    Check::SelectionRefused,
                    "an undeclared selection was not refused cleanly",
                );
            }
        }
        self.assert_not_applied(Check::SelectionRefused, &request)?;
        self.record(Check::SelectionRefused, CheckResult::Passed);
        Ok(())
    }

    /// One sample of every effect, for refusal checks.
    fn samples(&self) -> Result<Vec<(&'static str, Effect)>, ConformanceFailure> {
        let worker = ResourceRef {
            kind: ResourceKind::Worker,
            backend: self.executor.descriptor().backend.clone(),
            handle: self.reference("absent-worker")?,
        };
        let consumer = ConsumerId::new("conformance")
            .or_else(|_| fail(Check::Fixture, "invalid sample consumer"))?;
        Ok(vec![
            ("unsupported-launch", self.launch()?),
            (
                "unsupported-message",
                Effect::Worker(Operation::MessageWorker {
                    worker: worker.clone(),
                    body: self.fixture.brief.clone(),
                }),
            ),
            (
                "unsupported-reply",
                Effect::Worker(Operation::ReplyToWorker {
                    worker: worker.clone(),
                    question: self.reference("absent-question")?,
                    body: self.fixture.brief.clone(),
                }),
            ),
            (
                "unsupported-cancel",
                Effect::Worker(Operation::CancelWorker {
                    worker: worker.clone(),
                }),
            ),
            (
                "unsupported-release",
                Effect::Worker(Operation::ReleaseResource { resource: worker }),
            ),
            (
                "unsupported-label",
                Effect::GitHub(GitHubEffect {
                    requester: ExternalRef::new("fixture")
                        .or_else(|_| fail(Check::Fixture, "invalid requester"))?,
                    mutation: GitHubMutation {
                        repository: self.fixture.repository.clone(),
                        action: GitHubAction::CreateLabel {
                            label: LabelDefinition {
                                name: "conformance".into(),
                                color: "aabbcc".into(),
                                description: String::new(),
                            },
                        },
                    },
                    posting_budget: PostingBudget::new(3)
                        .or_else(|_| fail(Check::Fixture, "invalid budget"))?,
                }),
            ),
            (
                "unsupported-ask",
                Effect::Roger(RogerEffect {
                    requester: ExternalRef::new("fixture")
                        .or_else(|_| fail(Check::Fixture, "invalid requester"))?,
                    ask: RogerAsk {
                        binding: DecisionBinding {
                            house: self.fixture.house.clone(),
                            task: self.fixture.task.clone(),
                            action: Permission::Merge,
                            revision: EvidenceRevision::INITIAL,
                            subject: None,
                            owner: DecisionOwner::Merge,
                            repository: self.fixture.repository.clone(),
                            target: ExternalRef::new(&format!("pr:{}#1", self.fixture.repository))
                                .or_else(|_| fail(Check::Fixture, "invalid target"))?,
                            limits: self.fixture.brief.clone(),
                        },
                        kind: AskKind::Approval,
                        risk: AskRisk::Routine,
                        title: self.fixture.brief.clone(),
                        body: self.fixture.brief.clone(),
                        supersedes: None,
                    },
                    posting_budget: PostingBudget::new(3)
                        .or_else(|_| fail(Check::Fixture, "invalid budget"))?,
                }),
            ),
            (
                "unsupported-schedule",
                Effect::Schedule(ScheduleEffect::InstallDisabled {
                    schedule: ScheduleSpec::new(
                        WorkflowName::new("conformance")
                            .or_else(|_| fail(Check::Fixture, "invalid sample workflow"))?,
                        consumer,
                        Recurrence::Hourly,
                        Timezone::new("UTC")
                            .or_else(|_| fail(Check::Fixture, "invalid sample time zone"))?,
                        self.fixture.brief.clone(),
                        ResolvedSelection::owner(AgentSelection::agent_default(
                            AgentFamily::Claude,
                        )),
                    )
                    .into(),
                }),
            ),
        ])
    }

    /// A lookup must not report an effect that the check expected to be refused.
    fn assert_not_applied(
        &self,
        check: Check,
        request: &EffectRequest,
    ) -> Result<(), ConformanceFailure> {
        if !self.executor.descriptor().supports_lookup(request.effect()) {
            return Ok(());
        }
        match self.executor.lookup(request) {
            Ok(Lookup::Applied(_)) => fail(check, "refused request was applied"),
            Ok(Lookup::Absent | Lookup::Unknown) => Ok(()),
            Err(_) => fail(check, "lookup failed after a refused request"),
        }
    }

    /// The checks every executor passes. Returns the probe receipt when the
    /// probe's capability is declared.
    fn executor_checks(&mut self, probe: &Effect) -> Result<Option<Receipt>, ConformanceFailure> {
        if self.executor.descriptor().house != self.fixture.house {
            return fail(Check::DescriptorHouse, "descriptor serves another house");
        }
        self.record(Check::DescriptorHouse, CheckResult::Passed);
        self.cross_house(probe)?;
        self.foreign_backend(probe)?;
        self.unsupported()?;
        self.unknown_key(probe)?;
        let Some((request, receipt)) = self.probe_receipt(probe)? else {
            for check in [Check::LookupMatchesReceipt, Check::IdempotentResubmission] {
                self.record(
                    check,
                    CheckResult::NotApplicable {
                        requires: probe.required_capability(),
                    },
                );
            }
            return Ok(None);
        };
        self.lookup_matches(&request, &receipt)?;
        self.idempotent(&request, &receipt)?;
        Ok(Some(receipt))
    }

    fn cross_house(&mut self, probe: &Effect) -> Result<(), ConformanceFailure> {
        let check = Check::CrossHouseRefused;
        let backend = self.executor.descriptor().backend.clone();
        let request = self.request(
            &self.fixture.foreign_house,
            &backend,
            "foreign",
            probe.clone(),
        )?;
        match self.executor.execute(&request) {
            Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse)) => {}
            Err(_) => return fail(check, "refusal did not name the house mismatch"),
            Ok(_) => return fail(check, "request for another house was applied"),
        }
        self.assert_not_applied(check, &request)?;
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn foreign_backend(&mut self, probe: &Effect) -> Result<(), ConformanceFailure> {
        let check = Check::ForeignBackendRefused;
        if self.fixture.foreign_backend == self.executor.descriptor().backend {
            return fail(
                Check::Fixture,
                "foreign backend matches the executor under test",
            );
        }
        let request = self.request(
            &self.fixture.house,
            &self.fixture.foreign_backend,
            "foreign-backend",
            probe.clone(),
        )?;
        match self.executor.execute(&request) {
            Err(EffectFailure::NotApplied(NotAppliedReason::ForeignBackend)) => {}
            Err(_) => return fail(check, "refusal did not name the backend mismatch"),
            Ok(_) => return fail(check, "request for another backend was applied"),
        }
        self.assert_not_applied(check, &request)?;
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn unsupported(&mut self) -> Result<(), ConformanceFailure> {
        let check = Check::UnsupportedRefused;
        for (suffix, effect) in self.samples()? {
            let capability = effect.required_capability();
            if self.supports(capability) {
                continue;
            }
            let request = self.own_request(suffix, effect)?;
            match self.executor.execute(&request) {
                Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(named)))
                    if named == capability => {}
                Err(_) => return fail(check, "refusal did not name the missing capability"),
                Ok(_) => return fail(check, "effect without declared support was applied"),
            }
            self.assert_not_applied(check, &request)?;
        }
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn unknown_key(&mut self, probe: &Effect) -> Result<(), ConformanceFailure> {
        let check = Check::UnknownKeyNotApplied;
        let request = self.own_request("never-used", probe.clone())?;
        let requires = probe.kind().lookup_capability();
        if !self.executor.descriptor().supports_lookup(probe) {
            return match self.executor.lookup(&request) {
                Err(BackendUnavailable::Unsupported(named))
                    if named == requires || named == Capability::EffectLookup =>
                {
                    self.record(check, CheckResult::NotApplicable { requires });
                    Ok(())
                }
                Ok(_) | Err(_) => fail(check, "undeclared lookup did not report unsupported"),
            };
        }
        match self.executor.lookup(&request) {
            Ok(Lookup::Absent | Lookup::Unknown) => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(Lookup::Applied(_)) => fail(check, "never-used key reported as applied"),
            Err(_) => fail(check, "declared lookup was unavailable"),
        }
    }

    fn probe_receipt(
        &mut self,
        probe: &Effect,
    ) -> Result<Option<(EffectRequest, Receipt)>, ConformanceFailure> {
        let check = Check::ProbeReceipt;
        let requires = probe.required_capability();
        if !self.supports(requires) {
            self.record(check, CheckResult::NotApplicable { requires });
            return Ok(None);
        }
        let request = self.own_request("probe", probe.clone())?;
        let receipt = match self.executor.execute(&request) {
            Ok(receipt) => receipt,
            Err(EffectFailure::NotApplied(_)) => return fail(check, "probe was refused"),
            Err(EffectFailure::Uncertain(_)) => return fail(check, "probe outcome was uncertain"),
        };
        self.record(check, CheckResult::Passed);
        Ok(Some((request, receipt)))
    }

    fn lookup_matches(
        &mut self,
        request: &EffectRequest,
        receipt: &Receipt,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::LookupMatchesReceipt;
        if !self.executor.descriptor().supports_lookup(request.effect()) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: request.effect().kind().lookup_capability(),
                },
            );
            return Ok(());
        }
        match self.executor.lookup(request) {
            Ok(Lookup::Applied(found)) if &found == receipt => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(Lookup::Applied(_)) => fail(check, "lookup returned a different receipt"),
            Ok(Lookup::Absent) => fail(check, "applied probe reported as absent"),
            Ok(Lookup::Unknown) | Err(_) => fail(check, "applied probe could not be looked up"),
        }
    }

    fn idempotent(
        &mut self,
        request: &EffectRequest,
        receipt: &Receipt,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::IdempotentResubmission;
        if !self.executor.descriptor().idempotent(request.effect()) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: request.effect().kind().idempotency_capability(),
                },
            );
            return Ok(());
        }
        match self.executor.execute(request) {
            Ok(repeat) if &repeat == receipt => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(_) => fail(check, "resubmission produced a different receipt"),
            Err(_) => fail(check, "resubmission failed"),
        }
    }

    fn observable(
        &mut self,
        backend: &dyn WorkerBackend,
        worker: &ResourceRef,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::LaunchObservable;
        if !self.supports(Capability::WorkerStatusAndOutcome) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: Capability::WorkerStatusAndOutcome,
                },
            );
            return Ok(());
        }
        match backend.observe_worker(worker) {
            Ok(
                WorkerState::Starting
                | WorkerState::Ready
                | WorkerState::AwaitingReply
                | WorkerState::UserTakeover,
            ) => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(WorkerState::Settled(_)) => fail(check, "new worker reported as settled"),
            Ok(WorkerState::Missing | WorkerState::Unknown) => {
                fail(check, "launched worker is not observable")
            }
            Err(_) => fail(check, "declared status was unavailable"),
        }
    }

    fn inventory(
        &mut self,
        backend: &dyn WorkerBackend,
        worker: &ResourceRef,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::InventoryListsLaunch;
        if !self.supports(Capability::ResourceInventory) {
            return match backend.inventory() {
                Err(BackendUnavailable::Unsupported(Capability::ResourceInventory)) => {
                    self.record(
                        check,
                        CheckResult::NotApplicable {
                            requires: Capability::ResourceInventory,
                        },
                    );
                    Ok(())
                }
                Ok(_) | Err(_) => fail(check, "undeclared inventory did not report unsupported"),
            };
        }
        let observations = match backend.inventory() {
            Ok(observations) => observations,
            Err(_) => return fail(check, "declared inventory was unavailable"),
        };
        if observations.len() > MAX_INVENTORY_RESOURCES {
            return fail(check, "inventory exceeded its bound");
        }
        match observations
            .iter()
            .find(|observation| &observation.resource == worker)
            .map(|observation| observation.liveness)
        {
            Some(Liveness::Live) => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Some(Liveness::Exited) => fail(check, "new worker listed as exited"),
            Some(Liveness::Unverifiable) | None => {
                fail(check, "launched worker not listed as live")
            }
        }
    }

    fn message_recovery(
        &mut self,
        backend: &dyn WorkerBackend,
        worker: &ResourceRef,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::MessageRecovery;
        if !self.supports(Capability::WorkerMessaging) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: Capability::WorkerMessaging,
                },
            );
            return Ok(());
        }
        let request = self.own_request(
            "message",
            Effect::Worker(Operation::MessageWorker {
                worker: worker.clone(),
                body: self.fixture.brief.clone(),
            }),
        )?;
        let receipt = match backend.execute(&request) {
            Ok(receipt) => receipt,
            Err(_) => return fail(check, "message to a launched worker failed"),
        };
        let descriptor = backend.descriptor();
        if descriptor.supports_lookup(request.effect()) {
            match backend.lookup(&request) {
                Ok(Lookup::Applied(found)) if found == receipt => {}
                Ok(_) | Err(_) => return fail(check, "declared message lookup did not match"),
            }
        }
        if descriptor.idempotent(request.effect()) {
            match backend.execute(&request) {
                Ok(repeat) if repeat == receipt => {}
                Ok(_) | Err(_) => {
                    return fail(check, "declared idempotent message was not deduplicated");
                }
            }
        }
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn cancel(
        &mut self,
        backend: &dyn WorkerBackend,
        worker: &ResourceRef,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::CancelObserved;
        for requires in [Capability::WorkerCancel, Capability::WorkerStatusAndOutcome] {
            if !self.supports(requires) {
                self.record(check, CheckResult::NotApplicable { requires });
                return Ok(());
            }
        }
        let request = self.own_request(
            "cancel",
            Effect::Worker(Operation::CancelWorker {
                worker: worker.clone(),
            }),
        )?;
        match backend.execute(&request) {
            // An uncertain cancel is allowed; the observation below decides.
            Ok(_) | Err(EffectFailure::Uncertain(_)) => {}
            Err(EffectFailure::NotApplied(_)) => {
                return fail(check, "cancel of a launched worker was refused");
            }
        }
        match backend.observe_worker(worker) {
            Ok(WorkerState::Ready | WorkerState::AwaitingReply | WorkerState::Starting) => {
                fail(check, "cancelled worker still reported as running")
            }
            Ok(WorkerState::Settled(_)) => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            // No record is not proof that the worker stopped.
            Ok(WorkerState::Missing) => fail(check, "cancelled worker is missing, not settled"),
            Ok(WorkerState::UserTakeover) => {
                fail(check, "cancelled worker is held by a person, not settled")
            }
            Ok(WorkerState::Unknown) => fail(check, "cancelled worker state is unknown"),
            Err(_) => fail(check, "declared status was unavailable"),
        }
    }

    /// Release the cancelled worker. A release removes what it releases and
    /// never the branch that was checked out: the dishwasher's pushed check
    /// relies on that branch outliving a released worktree. A release the
    /// backend retains deletes nothing and passes. The release is sent only
    /// after the cancel was sent and observed, so it never reaches a worker
    /// that may still be running. The worker is the resource released: a
    /// backend such as Orca releases a worktree through its worker and
    /// refuses a release of the worktree itself. Only the receipt is checked
    /// unless the declared inventory lists the branch before the release, so
    /// a backend that deletes the branch without naming it in the receipt is
    /// caught only where its inventory lists branches.
    fn release_keeps_branch(
        &mut self,
        backend: &dyn WorkerBackend,
        worker: &ResourceRef,
        branch: &ResourceRef,
    ) -> Result<(), ConformanceFailure> {
        let check = Check::ReleaseKeepsBranch;
        for requires in [
            Capability::ResourceRelease,
            Capability::WorkerCancel,
            Capability::WorkerStatusAndOutcome,
        ] {
            if !self.supports(requires) {
                self.record(check, CheckResult::NotApplicable { requires });
                return Ok(());
            }
        }
        let listed_before = self.lists(backend, branch)?;
        let request = self.own_request(
            "release",
            Effect::Worker(Operation::ReleaseResource {
                resource: worker.clone(),
            }),
        )?;
        match backend.execute(&request) {
            Ok(receipt) => {
                if receipt
                    .created()
                    .iter()
                    .chain(receipt.touched())
                    .any(|resource| resource.kind == ResourceKind::Branch)
                {
                    return fail(check, "release receipt names a branch");
                }
            }
            Err(EffectFailure::NotApplied(_)) => {}
            Err(EffectFailure::Uncertain(_)) => {
                return fail(check, "release of a cancelled worker has no clear outcome");
            }
        }
        if listed_before && !self.lists(backend, branch)? {
            return fail(check, "branch left the inventory after the release");
        }
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    /// Whether a declared inventory lists `resource`; `false` without one.
    fn lists(
        &self,
        backend: &dyn WorkerBackend,
        resource: &ResourceRef,
    ) -> Result<bool, ConformanceFailure> {
        if !self.supports(Capability::ResourceInventory) {
            return Ok(false);
        }
        match backend.inventory() {
            Ok(observations) => Ok(observations
                .iter()
                .any(|observation| &observation.resource == resource)),
            Err(_) => fail(
                Check::ReleaseKeepsBranch,
                "declared inventory was unavailable",
            ),
        }
    }
}
