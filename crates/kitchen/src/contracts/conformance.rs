//! Shared contract checks every [`ExecutionBackend`] must pass.
//!
//! The suite performs real effects: it launches one worker (when supported)
//! and cancels it (when supported). Run it against the fake backend offline
//! and against a real backend only in a controlled environment where that
//! launch is authorized. A passing run is evidence about the backend it ran
//! against, and only for the paths it exercised.

use std::fmt;

use crate::{
    BackendId, CredentialId, HouseId, TaskId,
    contracts::{
        AttemptNumber, BackendUnavailable, Capability, EffectFailure, EffectRequest,
        ExecutionBackend, ExternalRef, IdempotencyKey, Lookup, NotAppliedReason, Operation,
        Receipt, ResourceKind, ResourceRef, Role, Text, WorkerState, Workspace,
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
    /// Operations without full capability support are refused without effect.
    UnsupportedRefused,
    /// A never-used key is not reported as applied.
    UnknownKeyNotApplied,
    /// A launch returns a receipt naming a worker on this backend.
    LaunchReceipt,
    /// Lookup of the launch key returns the launch receipt.
    LookupMatchesReceipt,
    /// Resubmitting the launch key returns the same receipt.
    IdempotentResubmission,
    /// The launched worker is observable and not reported as settled.
    LaunchObservable,
    /// A cancelled worker is no longer reported as running.
    CancelObserved,
}

impl fmt::Display for Check {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Fixture => "fixture",
            Self::DescriptorHouse => "descriptor house",
            Self::CrossHouseRefused => "cross-house request refused",
            Self::ForeignBackendRefused => "foreign-backend request refused",
            Self::UnsupportedRefused => "unsupported operation refused",
            Self::UnknownKeyNotApplied => "unknown key not applied",
            Self::LaunchReceipt => "launch receipt",
            Self::LookupMatchesReceipt => "lookup matches receipt",
            Self::IdempotentResubmission => "idempotent resubmission",
            Self::LaunchObservable => "launch observable",
            Self::CancelObserved => "cancel observed",
        })
    }
}

/// How a check ended when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckResult {
    /// The backend met the contract.
    Passed,
    /// Not exercised: the backend does not declare the capability.
    NotApplicable {
        /// The undeclared capability.
        requires: Capability,
    },
}

/// A contract violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("backend contract check '{check}' failed: {problem}")]
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
    /// The house the backend serves.
    pub house: HouseId,
    /// Another house, used to check cross-house refusal.
    pub foreign_house: HouseId,
    /// Another backend namespace, used to check foreign-backend refusal.
    pub foreign_backend: BackendId,
    /// The credential reference the run's requests name.
    pub credential: CredentialId,
    /// A disposable task identity for the run's requests.
    pub task: TaskId,
    /// A tag unique to this run, so idempotency keys never collide with earlier runs.
    pub run_tag: ExternalRef,
    /// The brief for the launched worker.
    pub brief: Text,
}

struct Runner<'a> {
    backend: &'a dyn ExecutionBackend,
    fixture: &'a ConformanceFixture,
    report: ConformanceReport,
}

fn fail<T>(check: Check, problem: &'static str) -> Result<T, ConformanceFailure> {
    Err(ConformanceFailure { check, problem })
}

/// Run every check against `backend`.
///
/// # Errors
/// Returns the first [`ConformanceFailure`].
pub fn run(
    backend: &dyn ExecutionBackend,
    fixture: &ConformanceFixture,
) -> Result<ConformanceReport, ConformanceFailure> {
    let mut runner = Runner {
        backend,
        fixture,
        report: ConformanceReport::default(),
    };
    runner.run()?;
    Ok(runner.report)
}

impl Runner<'_> {
    fn supports(&self, capability: Capability) -> bool {
        self.backend.descriptor().capabilities.supports(capability)
    }

    fn record(&mut self, check: Check, result: CheckResult) {
        self.report.results.push((check, result));
    }

    fn key(&self, suffix: &str) -> Result<IdempotencyKey, ConformanceFailure> {
        ExternalRef::new(&format!("{}-{suffix}", self.fixture.run_tag))
            .map(IdempotencyKey::from_ref)
            .or_else(|_| fail(Check::Fixture, "run tag too long for a key"))
    }

    fn request(
        &self,
        house: &HouseId,
        suffix: &str,
        operation: Operation,
    ) -> Result<EffectRequest, ConformanceFailure> {
        Ok(EffectRequest::new(
            house.clone(),
            self.backend.descriptor().backend.clone(),
            self.fixture.credential.clone(),
            self.fixture.task.clone(),
            AttemptNumber::FIRST,
            self.key(suffix)?,
            operation,
        ))
    }

    fn launch(&self) -> Operation {
        Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: self.fixture.brief.clone(),
        }
    }

    fn absent_worker(&self) -> Result<ResourceRef, ConformanceFailure> {
        let handle = ExternalRef::new(&format!("{}-absent-worker", self.fixture.run_tag))
            .or_else(|_| fail(Check::Fixture, "run tag too long for a handle"))?;
        Ok(ResourceRef {
            kind: ResourceKind::Worker,
            backend: self.backend.descriptor().backend.clone(),
            handle,
        })
    }

    /// A lookup must not report an effect that the check expected to be refused.
    fn assert_not_applied(
        &self,
        check: Check,
        key: &IdempotencyKey,
    ) -> Result<(), ConformanceFailure> {
        if !self.supports(Capability::EffectLookup) {
            return Ok(());
        }
        match self.backend.lookup(key) {
            Ok(Lookup::Applied(_)) => fail(check, "refused request was applied"),
            Ok(Lookup::Absent | Lookup::Unknown) => Ok(()),
            Err(_) => fail(check, "lookup failed after a refused request"),
        }
    }

    fn run(&mut self) -> Result<(), ConformanceFailure> {
        if self.backend.descriptor().house != self.fixture.house {
            return fail(Check::DescriptorHouse, "descriptor serves another house");
        }
        self.record(Check::DescriptorHouse, CheckResult::Passed);
        self.cross_house()?;
        self.foreign_backend()?;
        self.unsupported()?;
        self.unknown_key()?;
        let Some((request, receipt)) = self.launch_receipt()? else {
            for check in [
                Check::LookupMatchesReceipt,
                Check::IdempotentResubmission,
                Check::LaunchObservable,
                Check::CancelObserved,
            ] {
                self.record(
                    check,
                    CheckResult::NotApplicable {
                        requires: Capability::WorkerLaunchIsolated,
                    },
                );
            }
            return Ok(());
        };
        self.lookup_matches(&request, &receipt)?;
        self.idempotent(&request, &receipt)?;
        let worker = receipt
            .resources()
            .iter()
            .find(|resource| resource.kind == ResourceKind::Worker)
            .cloned();
        let Some(worker) = worker else {
            return fail(Check::LaunchReceipt, "receipt names no worker");
        };
        self.observable(&worker)?;
        self.cancel(&worker)
    }

    fn cross_house(&mut self) -> Result<(), ConformanceFailure> {
        let check = Check::CrossHouseRefused;
        let request = self.request(&self.fixture.foreign_house, "foreign", self.launch())?;
        match self.backend.execute(&request) {
            Err(EffectFailure::NotApplied(NotAppliedReason::CrossHouse)) => {}
            Err(_) => return fail(check, "refusal did not name the house mismatch"),
            Ok(_) => return fail(check, "request for another house was applied"),
        }
        self.assert_not_applied(check, request.key())?;
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn foreign_backend(&mut self) -> Result<(), ConformanceFailure> {
        let check = Check::ForeignBackendRefused;
        let own = self.request(&self.fixture.house, "foreign-backend", self.launch())?;
        let request = EffectRequest::new(
            own.house().clone(),
            self.fixture.foreign_backend.clone(),
            own.credential().clone(),
            own.task().clone(),
            own.attempt(),
            own.key().clone(),
            own.operation().clone(),
        );
        if request.backend() == &self.backend.descriptor().backend {
            return fail(
                Check::Fixture,
                "foreign backend matches the backend under test",
            );
        }
        match self.backend.execute(&request) {
            Err(EffectFailure::NotApplied(NotAppliedReason::ForeignBackend)) => {}
            Err(_) => return fail(check, "refusal did not name the backend mismatch"),
            Ok(_) => return fail(check, "request for another backend was applied"),
        }
        self.assert_not_applied(check, request.key())?;
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn unsupported(&mut self) -> Result<(), ConformanceFailure> {
        let check = Check::UnsupportedRefused;
        let worker = self.absent_worker()?;
        let operations = [
            ("unsupported-launch", self.launch()),
            (
                "unsupported-message",
                Operation::MessageWorker {
                    worker: worker.clone(),
                    body: self.fixture.brief.clone(),
                },
            ),
            (
                "unsupported-cancel",
                Operation::CancelWorker {
                    worker: worker.clone(),
                },
            ),
            (
                "unsupported-release",
                Operation::ReleaseResource { resource: worker },
            ),
        ];
        for (suffix, operation) in operations {
            let capability = operation.required_capability();
            if self.supports(capability) {
                continue;
            }
            let request = self.request(&self.fixture.house, suffix, operation)?;
            match self.backend.execute(&request) {
                Err(EffectFailure::NotApplied(NotAppliedReason::Unsupported(named)))
                    if named == capability => {}
                Err(_) => return fail(check, "refusal did not name the missing capability"),
                Ok(_) => return fail(check, "operation without declared support was applied"),
            }
            self.assert_not_applied(check, request.key())?;
        }
        self.record(check, CheckResult::Passed);
        Ok(())
    }

    fn unknown_key(&mut self) -> Result<(), ConformanceFailure> {
        let check = Check::UnknownKeyNotApplied;
        let key = self.key("never-used")?;
        if !self.supports(Capability::EffectLookup) {
            return match self.backend.lookup(&key) {
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
        match self.backend.lookup(&key) {
            Ok(Lookup::Absent | Lookup::Unknown) => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(Lookup::Applied(_)) => fail(check, "never-used key reported as applied"),
            Err(_) => fail(check, "declared lookup was unavailable"),
        }
    }

    fn launch_receipt(&mut self) -> Result<Option<(EffectRequest, Receipt)>, ConformanceFailure> {
        let check = Check::LaunchReceipt;
        if !self.supports(Capability::WorkerLaunchIsolated) {
            self.record(
                check,
                CheckResult::NotApplicable {
                    requires: Capability::WorkerLaunchIsolated,
                },
            );
            return Ok(None);
        }
        let request = self.request(&self.fixture.house, "launch", self.launch())?;
        let receipt = match self.backend.execute(&request) {
            Ok(receipt) => receipt,
            Err(EffectFailure::NotApplied(_)) => return fail(check, "launch was refused"),
            Err(EffectFailure::Uncertain(_)) => return fail(check, "launch outcome was uncertain"),
        };
        let backend = &self.backend.descriptor().backend;
        let names_worker = receipt
            .resources()
            .iter()
            .any(|resource| resource.kind == ResourceKind::Worker && &resource.backend == backend);
        if !names_worker {
            return fail(check, "receipt names no worker on this backend");
        }
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
        match self.backend.lookup(request.key()) {
            Ok(Lookup::Applied(found)) if &found == receipt => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(Lookup::Applied(_)) => fail(check, "lookup returned a different receipt"),
            Ok(Lookup::Absent) => fail(check, "applied launch reported as absent"),
            Ok(Lookup::Unknown) | Err(_) => fail(check, "applied launch could not be looked up"),
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
        match self.backend.execute(request) {
            Ok(repeat) if &repeat == receipt => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(_) => fail(check, "resubmission produced a different receipt"),
            Err(_) => fail(check, "resubmission failed"),
        }
    }

    fn observable(&mut self, worker: &ResourceRef) -> Result<(), ConformanceFailure> {
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
        match self.backend.observe_worker(worker) {
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

    fn cancel(&mut self, worker: &ResourceRef) -> Result<(), ConformanceFailure> {
        let check = Check::CancelObserved;
        for requires in [Capability::WorkerCancel, Capability::WorkerStatusAndOutcome] {
            if !self.supports(requires) {
                self.record(check, CheckResult::NotApplicable { requires });
                return Ok(());
            }
        }
        let request = self.request(
            &self.fixture.house,
            "cancel",
            Operation::CancelWorker {
                worker: worker.clone(),
            },
        )?;
        if self.backend.execute(&request).is_err() {
            return fail(check, "cancel of a launched worker failed");
        }
        match self.backend.observe_worker(worker) {
            Ok(WorkerState::Ready | WorkerState::AwaitingReply | WorkerState::Starting) => {
                fail(check, "cancelled worker still reported as running")
            }
            Ok(WorkerState::Settled(_) | WorkerState::Missing) => {
                self.record(check, CheckResult::Passed);
                Ok(())
            }
            Ok(WorkerState::Unknown) => fail(check, "cancelled worker state is unknown"),
            Err(_) => fail(check, "declared status was unavailable"),
        }
    }
}
