//! Shared contract checks every [`EffectExecutor`] must pass, plus the
//! worker checks every [`WorkerBackend`] must pass.
//!
//! The suite performs real effects: [`run`] applies the caller's probe effect
//! once, and [`run_worker`] launches one worker and cancels it (when
//! supported). Run it against fakes offline and against a real executor only
//! in a controlled environment where those effects are authorized. A passing
//! run is evidence about the executor it ran against, and only for the paths
//! it exercised.

use std::fmt;

use crate::{
    BackendId, ConsumerId, CredentialId, HouseId, TaskId,
    contracts::{
        AttemptNumber, BackendUnavailable, Capability, DecisionBinding, Effect, EffectExecutor,
        EffectFailure, EffectRequest, EvidenceRevision, ExternalRef, GitHubEffect, IdempotencyKey,
        Liveness, Lookup, MAX_INVENTORY_RESOURCES, NotAppliedReason, Operation, Permission,
        Receipt, Repository, ResourceKind, ResourceRef, RogerEffect, Role, ScheduleEffect, Text,
        WorkerBackend, WorkerState, Workspace,
    },
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
    /// A launch receipt names a worker on this backend.
    LaunchReceipt,
    /// The launched worker is observable and not reported as settled.
    LaunchObservable,
    /// The inventory lists the launched worker as live, within its bound.
    InventoryListsLaunch,
    /// A cancelled worker is reported settled; a missing record is not evidence.
    CancelObserved,
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
            Self::LaunchObservable => "launch observable",
            Self::InventoryListsLaunch => "inventory lists launch",
            Self::CancelObserved => "cancel observed",
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
    let mut runner = Runner::new(executor, fixture);
    runner.executor_checks(probe)?;
    Ok(runner.report)
}

/// Run the executor checks with a worker launch as the probe, then the
/// worker checks: the receipt names a worker, which is observable, listed
/// by the inventory, and stops when cancelled.
///
/// # Errors
/// Returns the first [`ConformanceFailure`].
pub fn run_worker(
    backend: &dyn WorkerBackend,
    fixture: &ConformanceFixture,
) -> Result<ConformanceReport, ConformanceFailure> {
    let mut runner = Runner::new(backend, fixture);
    let launch = runner.launch();
    let Some(receipt) = runner.executor_checks(&launch)? else {
        for check in [
            Check::LaunchReceipt,
            Check::LaunchObservable,
            Check::InventoryListsLaunch,
            Check::CancelObserved,
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
    runner.record(Check::LaunchReceipt, CheckResult::Passed);
    runner.observable(backend, &worker)?;
    runner.inventory(backend, &worker)?;
    runner.cancel(backend, &worker)?;
    Ok(runner.report)
}

impl<'a> Runner<'a> {
    fn new(executor: &'a dyn EffectExecutor, fixture: &'a ConformanceFixture) -> Self {
        Self {
            executor,
            fixture,
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

    fn launch(&self) -> Effect {
        Effect::Worker(Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: self.fixture.brief.clone(),
        })
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
            ("unsupported-launch", self.launch()),
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
                Effect::GitHub(GitHubEffect::CreateLabel {
                    repository: self.fixture.repository.clone(),
                    name: self.fixture.brief.clone(),
                }),
            ),
            (
                "unsupported-ask",
                Effect::Roger(RogerEffect::Ask {
                    binding: DecisionBinding {
                        house: self.fixture.house.clone(),
                        task: self.fixture.task.clone(),
                        action: Permission::Merge,
                        revision: EvidenceRevision::INITIAL,
                    },
                    question: self.fixture.brief.clone(),
                }),
            ),
            (
                "unsupported-schedule",
                Effect::Schedule(ScheduleEffect::InstallDisabled { consumer }),
            ),
        ])
    }

    /// A lookup must not report an effect that the check expected to be refused.
    fn assert_not_applied(
        &self,
        check: Check,
        request: &EffectRequest,
    ) -> Result<(), ConformanceFailure> {
        if !self.supports(Capability::EffectLookup) {
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
        if !self.supports(Capability::EffectLookup) {
            return match self.executor.lookup(&request) {
                Err(BackendUnavailable::Unsupported(Capability::EffectLookup)) => {
                    self.record(
                        check,
                        CheckResult::NotApplicable {
                            requires: Capability::EffectLookup,
                        },
                    );
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
        if !self.supports(Capability::EffectLookup) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: Capability::EffectLookup,
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
        if !self.supports(Capability::EffectIdempotentRequests) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: Capability::EffectIdempotentRequests,
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
            Ok(WorkerState::Starting | WorkerState::Ready | WorkerState::AwaitingReply) => {
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
            Ok(WorkerState::Unknown) => fail(check, "cancelled worker state is unknown"),
            Err(_) => fail(check, "declared status was unavailable"),
        }
    }
}
