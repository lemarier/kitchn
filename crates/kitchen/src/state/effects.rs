//! Running effects through an executor with intent persisted first.

use crate::{
    Error, TaskId,
    contracts::{
        Clock, ContractError, EffectExecutor, EffectFailure, Fence, HouseGrants, IdempotencyKey,
        Lookup, NotAppliedReason, Receipt, UncertainReason,
    },
    state::{
        EffectOutcome, EffectPlan, EffectRecord, EffectStart, HouseStore, StateError, TaskState,
    },
};

type Result<T> = std::result::Result<T, Error>;

/// Execute one logical effect: check the executor, persist intent, call the
/// executor without holding the store lock, then record the outcome. Only the
/// executor namespace recorded with an intent may resubmit it.
///
/// Delivery is at most once per idempotency key only when the backend
/// declares the effect's kind idempotent
/// ([`crate::contracts::BackendDescriptor::idempotent`]); otherwise an uncertain
/// outcome is never resubmitted and must be reconciled with [`reconcile`].
/// With idempotent requests, a repeated call first looks the key up and
/// resubmits it only when the lookup cannot establish the outcome, within
/// the task's retry policy (count and elapsed time) per logical effect.
/// If recording the outcome fails (for example, the claim was taken over),
/// the effect stays `Intended` and the next owner reconciles it.
///
/// # Errors
/// Returns any error from [`HouseStore::begin_effect`] (including
/// [`ContractError::CrossHouse`] and [`ContractError::UnsupportedCapabilities`]
/// before anything is persisted) or [`HouseStore::record_effect_outcome`].
pub fn run_effect(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    grants: &HouseGrants,
    plan: EffectPlan,
    clock: &dyn Clock,
) -> Result<EffectRecord> {
    let task = plan.task.clone();
    let fence = plan.fence;
    let record = match store.begin_effect(plan.clone(), grants, executor, clock.now())? {
        EffectStart::Resolved(record) => return Ok(record),
        EffectStart::Execute(record) => record,
        EffectStart::ReconcileFirst(pending) => {
            let outcome = look_up(executor, &pending);
            // Applied only to the submission generation the lookup observed.
            store.record_submission_outcome(
                &task,
                fence,
                pending.seq(),
                pending.submissions(),
                outcome,
                clock.now(),
            )?;
            match store.begin_effect(plan, grants, executor, clock.now())? {
                EffectStart::Execute(record) => record,
                // Another handle changed the effect meanwhile; report it as is.
                EffectStart::Resolved(record) | EffectStart::ReconcileFirst(record) => {
                    return Ok(record);
                }
            }
        }
    };
    let outcome = match executor.execute(record.request()) {
        Ok(receipt) => EffectOutcome::Applied(receipt),
        Err(EffectFailure::NotApplied(reason)) => EffectOutcome::NotApplied(reason),
        Err(EffectFailure::Uncertain(reason)) => EffectOutcome::Uncertain(reason),
    };
    store.record_submission_outcome(
        &task,
        fence,
        record.seq(),
        record.submissions(),
        outcome,
        clock.now(),
    )
}

/// What a lookup establishes about one persisted effect.
fn look_up(executor: &dyn EffectExecutor, effect: &EffectRecord) -> EffectOutcome {
    if !executor
        .descriptor()
        .supports_lookup(effect.request().effect())
    {
        return EffectOutcome::Uncertain(UncertainReason::LookupUnsupported);
    }
    match executor.lookup(effect.request()) {
        Ok(Lookup::Applied(receipt)) => EffectOutcome::Applied(receipt),
        Ok(Lookup::Absent) => EffectOutcome::NotApplied(NotAppliedReason::ConfirmedAbsent),
        Ok(Lookup::Unknown) | Err(_) => {
            EffectOutcome::Uncertain(UncertainReason::LookupInconclusive)
        }
    }
}

/// What a backend lookup proved about one write of a settled task.
///
/// Its fields are private to this module and only [`prove`] builds one, from
/// the executor's own answer for the effect's idempotency key. The store
/// therefore never records a caller-supplied outcome or receipt for a
/// settled task.
#[derive(Debug)]
pub(crate) struct SettledLookup {
    key: IdempotencyKey,
    found: Found,
}

/// A conclusive lookup answer.
#[derive(Debug)]
pub(crate) enum Found {
    /// The backend returned this receipt for the key.
    Applied(Receipt),
    /// The backend proved the key was never applied.
    Absent,
}

impl SettledLookup {
    /// The idempotency key the lookup was made for.
    pub(crate) const fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    /// The answer.
    pub(crate) fn into_found(self) -> Found {
        self.found
    }
}

/// Look `effect` up and keep only a conclusive answer. `None` when the
/// executor does not declare lookup for the effect's kind, or when the lookup
/// fails or cannot tell.
fn prove(executor: &dyn EffectExecutor, effect: &EffectRecord) -> Option<SettledLookup> {
    if !executor
        .descriptor()
        .supports_lookup(effect.request().effect())
    {
        return None;
    }
    let found = match executor.lookup(effect.request()) {
        Ok(Lookup::Applied(receipt)) => Found::Applied(receipt),
        Ok(Lookup::Absent) => Found::Absent,
        Ok(Lookup::Unknown) | Err(_) => return None,
    };
    Some(SettledLookup {
        key: effect.request().key().clone(),
        found,
    })
}

/// The result of reconciling a task's unresolved effects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Effects whose outcome is now established.
    pub resolved: Vec<EffectRecord>,
    /// Effects whose outcome is still unknown. They block new effects,
    /// attempts, and settlement until resolved. When the owner cannot
    /// establish the outcome it records [`EffectOutcome::Unresolvable`] to
    /// hand the effect over; that still blocks until a
    /// [`crate::state::RiskDecision`] or positive evidence.
    pub unresolved: Vec<EffectRecord>,
    /// Unresolved effects persisted for another backend namespace. They were
    /// not looked up; reconcile them with the backend that received them.
    pub foreign: Vec<EffectRecord>,
}

/// Ask the executor what happened to each unresolved effect of `task` and
/// record the answers under `fence`.
///
/// Lookups use the persisted idempotency key and never re-execute an effect.
/// Only effects persisted for this backend's namespace are looked up.
/// [`Lookup::Absent`] is trusted only because the backend contract reserves
/// it for proven absence. Where the executor does not declare lookup for the
/// effect's kind ([`crate::contracts::BackendDescriptor::supports_lookup`]), outcomes stay
/// unknown.
///
/// # Errors
/// Returns [`ContractError::CrossHouse`] for a foreign backend and store
/// errors, including [`crate::state::StateError::StaleFence`] when `fence`
/// does not own the task.
pub fn reconcile(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    task: &TaskId,
    fence: Fence,
    clock: &dyn Clock,
) -> Result<ReconcileReport> {
    let descriptor = executor.descriptor();
    if &descriptor.house != store.house() {
        return Err(ContractError::CrossHouse {
            expected: store.house().clone(),
            found: descriptor.house.clone(),
        }
        .into());
    }
    let record = store.task(task)?;
    match record.state() {
        TaskState::Claimed { lease } if lease.fence() == fence => {}
        TaskState::Claimed { .. } | TaskState::Open => {
            return Err(StateError::StaleFence { presented: fence }.into());
        }
        TaskState::Settled { settlement, .. } => {
            return Err(StateError::TaskSettled {
                task: task.clone(),
                settlement: *settlement,
            }
            .into());
        }
    }
    let pending: Vec<EffectRecord> = record.unresolved_effects().cloned().collect();
    let mut report = ReconcileReport::default();
    for effect in pending {
        if effect.request().backend() != &descriptor.backend {
            report.foreign.push(effect);
            continue;
        }
        let outcome = look_up(executor, &effect);
        // A negative or uncertain answer applies only if no newer submission
        // was sent since this lookup's snapshot.
        let updated = store.record_submission_outcome(
            task,
            fence,
            effect.seq(),
            effect.submissions(),
            outcome,
            clock.now(),
        )?;
        if updated.state().is_resolved() {
            report.resolved.push(updated);
        } else {
            report.unresolved.push(updated);
        }
    }
    Ok(report)
}

/// Ask the executor what happened to each unproven effect of a settled
/// `task` and record the answers.
///
/// A settled task has no lease and cannot submit anything, so unlike
/// [`reconcile`] this needs no fence and can only learn: a lookup never
/// re-executes an effect, and an inconclusive answer leaves the effect as it
/// was. Only a conclusive answer from `executor` is recorded; an effect whose
/// outcome is already established is never rewritten. It exists for the
/// person-driven review of a task that settled without success. Effects
/// persisted for another backend namespace are reported as foreign and not
/// looked up.
///
/// # Errors
/// [`ContractError::CrossHouse`] for a foreign backend,
/// [`StateError::TaskNotSettled`] while the task has not settled, and store
/// errors.
pub fn reread_settled(
    store: &HouseStore,
    executor: &dyn EffectExecutor,
    task: &TaskId,
    clock: &dyn Clock,
) -> Result<ReconcileReport> {
    let descriptor = executor.descriptor();
    if &descriptor.house != store.house() {
        return Err(ContractError::CrossHouse {
            expected: store.house().clone(),
            found: descriptor.house.clone(),
        }
        .into());
    }
    let record = store.task(task)?;
    if !matches!(record.state(), TaskState::Settled { .. }) {
        return Err(StateError::TaskNotSettled(task.clone()).into());
    }
    // Every write the forge has not proven, including one a person waived to
    // settle: a settled task has no work left for a waiver to protect.
    let pending: Vec<EffectRecord> = record
        .effects()
        .iter()
        .filter(|effect| !effect.state().is_resolved())
        .cloned()
        .collect();
    let mut report = ReconcileReport::default();
    for effect in pending {
        if effect.request().backend() != &descriptor.backend {
            report.foreign.push(effect);
            continue;
        }
        match prove(executor, &effect) {
            Some(proof) => {
                let updated =
                    store.record_settled_lookup(task, effect.seq(), proof, clock.now())?;
                report.resolved.push(updated);
            }
            None => report.unresolved.push(effect),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    //! Store transitions only this crate can reach: a settled lookup built by
    //! hand, and acknowledgements refused inside the transaction. Simulated
    //! with the fake backend.

    use std::{collections::BTreeSet, time::Duration};

    use super::{Found, SettledLookup, reread_settled, run_effect};
    use crate::{
        BackendId, CredentialId, EffectName, Error, HolderId, HouseId, TaskId,
        contracts::{
            AttemptNumber, AttemptOutcome, Capability, CapabilityRequirements, CapabilitySet,
            Claimant, Clock, CommitId, EffectSeq, EventOrigin, EvidenceRevision, ExternalRef,
            FailureClass, Fence, Grant, HouseGrants, LeaseTtl, Operation, Permission, Provenance,
            Receipt, RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Timestamp, Trigger,
            Workspace,
            fake::{ExecuteFault, FakeBackend},
        },
        state::{
            EffectPlan, EffectState, HouseStore, Limit, MAX_ACKNOWLEDGEMENT_REASON_BYTES,
            StateError, StoreOptions,
        },
    };

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    struct Setup {
        _dir: tempfile::TempDir,
        store: HouseStore,
        backend: FakeBackend,
        grants: HouseGrants,
        task: TaskId,
        fence: Fence,
    }

    const fn at(seconds: u64) -> Timestamp {
        Timestamp::from_unix_millis(seconds * 1000)
    }

    /// Time stands still inside the task's lease.
    struct Fixed;

    impl Clock for Fixed {
        fn now(&self) -> Timestamp {
            at(1)
        }
    }

    fn person() -> TestResult<Claimant> {
        Ok(Claimant::interactive(HolderId::new("owner-session")?))
    }

    fn reason() -> TestResult<Text> {
        Ok(Text::new("the writes were reviewed by hand")?)
    }

    /// A claimed task with a running attempt that may launch workers.
    fn setup() -> TestResult<Setup> {
        let dir = tempfile::tempdir()?;
        let house = HouseId::new("origin89")?;
        let store = HouseStore::initialize(
            dir.path().join("house"),
            house.clone(),
            StoreOptions::default(),
        )?;
        let backend_id = BackendId::new("fake")?;
        let grant = Grant::house(
            Permission::LaunchWorker,
            backend_id.clone(),
            CredentialId::new("origin89-orca")?,
        );
        let grants = HouseGrants::new(house.clone(), vec![grant.clone()]);
        let commit = CommitId::new(&"a".repeat(40))?;
        let task = TaskId::new("settled-writes")?;
        let spec = TaskSpec {
            id: task.clone(),
            role: Role::StationCook,
            repository: None,
            authority: TaskAuthority::delegate(&grants, [grant])?,
            retry: RetryPolicy::new(3, Duration::from_secs(3600))?,
            provenance: Provenance {
                kitchen: commit.clone(),
                house_guidance: commit,
                repository_instructions: None,
            },
            resources: BTreeSet::new(),
            requires: CapabilityRequirements::new(),
            agent: None,
            work_type: None,
        };
        let creator = Claimant::scheduled(HolderId::new("pickup")?);
        store.create_task(spec, &creator, at(0))?;
        let fence = store
            .claim(
                &task,
                &creator,
                LeaseTtl::new(Duration::from_secs(600))?,
                at(0),
            )?
            .fence();
        store.start_attempt(&task, fence, at(0))?;
        let backend = FakeBackend::new(
            backend_id,
            house,
            CapabilitySet::supporting([
                Capability::WorkerLaunchIsolated,
                Capability::EffectLookup,
                Capability::LookupLaunchWorker,
            ]),
        );
        Ok(Setup {
            _dir: dir,
            store,
            backend,
            grants,
            task,
            fence,
        })
    }

    impl Setup {
        /// Run one launch named `name`, with `fault` injected first.
        fn write(&self, name: &str, fault: Option<ExecuteFault>) -> TestResult<EffectState> {
            if let Some(fault) = fault {
                self.backend.inject(fault);
            }
            let plan = EffectPlan {
                task: self.task.clone(),
                fence: self.fence,
                name: EffectName::new(name)?,
                decided_at: EvidenceRevision::INITIAL,
                effect: Operation::LaunchWorker {
                    role: Role::StationCook,
                    workspace: Workspace::Isolated,
                    brief: Text::new("Implement the issue.")?,
                    branch: None,
                    agent: None,
                }
                .into(),
                consent: None,
                basis: None,
            };
            let record = run_effect(&self.store, &self.backend, &self.grants, plan, &Fixed)?;
            Ok(record.state().clone())
        }

        fn settle(&self, outcome: AttemptOutcome) -> TestResult {
            self.store.finish_attempt(
                &self.task,
                self.fence,
                AttemptNumber::FIRST,
                outcome,
                at(1),
            )?;
            Ok(())
        }

        fn state_of(&self, seq: u32) -> TestResult<EffectState> {
            let record = self.store.task(&self.task)?;
            let effect = record
                .effects()
                .iter()
                .find(|effect| effect.seq() == EffectSeq::new(seq))
                .ok_or("no such effect")?;
            Ok(effect.state().clone())
        }

        fn lookup(&self, seq: u32, found: Found) -> TestResult<SettledLookup> {
            let record = self.store.task(&self.task)?;
            let effect = record
                .effects()
                .iter()
                .find(|effect| effect.seq() == EffectSeq::new(seq))
                .ok_or("no such effect")?;
            Ok(SettledLookup {
                key: effect.request().key().clone(),
                found,
            })
        }
    }

    fn receipt(reference: &str) -> TestResult<Receipt> {
        Ok(Receipt::new(
            ExternalRef::new(reference)?,
            Vec::new(),
            Vec::new(),
        )?)
    }

    #[test]
    fn a_settled_lookup_never_rewrites_an_established_outcome() -> TestResult {
        let setup = setup()?;
        let EffectState::Applied { receipt: kept, .. } = setup.write("applied", None)? else {
            return Err("the launch did not apply".into());
        };
        setup.write("refused", Some(ExecuteFault::Reject))?;
        setup.settle(AttemptOutcome::Failed(FailureClass::Permanent))?;
        let applied = setup.state_of(0)?;
        let refused = setup.state_of(1)?;

        // The same receipt is a no-op.
        let same = setup.lookup(0, Found::Applied(kept))?;
        setup
            .store
            .record_settled_lookup(&setup.task, EffectSeq::new(0), same, at(2))?;
        assert_eq!(setup.state_of(0)?, applied);

        // Any other answer about an established outcome is refused.
        for (seq, found) in [
            (0, Found::Applied(receipt("invented")?)),
            (0, Found::Absent),
            (1, Found::Applied(receipt("invented")?)),
        ] {
            let lookup = setup.lookup(seq, found)?;
            let error = setup
                .store
                .record_settled_lookup(&setup.task, EffectSeq::new(seq), lookup, at(2))
                .err()
                .ok_or("an established outcome was rewritten")?;
            assert!(matches!(
                error,
                Error::State(StateError::ConflictingOutcome(refused)) if refused == EffectSeq::new(seq)
            ));
        }
        assert_eq!(setup.state_of(0)?, applied);
        assert_eq!(setup.state_of(1)?, refused);
        Ok(())
    }

    #[test]
    fn a_settled_lookup_must_name_its_effect_and_a_settled_task() -> TestResult {
        let setup = setup()?;
        setup.write("first", None)?;
        setup.write("second", None)?;

        // Unsettled: the owner reconciles under its fence instead.
        let early = setup.lookup(0, Found::Absent)?;
        let error = setup
            .store
            .record_settled_lookup(&setup.task, EffectSeq::new(0), early, at(1))
            .err()
            .ok_or("an unsettled task took a settled lookup")?;
        assert!(matches!(error, Error::State(StateError::TaskNotSettled(_))));
        setup.settle(AttemptOutcome::Failed(FailureClass::Permanent))?;
        let first = setup.state_of(0)?;

        // A lookup made for the second write cannot resolve the first.
        let other = setup.lookup(1, Found::Absent)?;
        let error = setup
            .store
            .record_settled_lookup(&setup.task, EffectSeq::new(0), other, at(1))
            .err()
            .ok_or("a lookup resolved another effect")?;
        assert!(matches!(
            error,
            Error::State(StateError::LookupScope(seq)) if seq == EffectSeq::new(0)
        ));
        assert_eq!(setup.state_of(0)?, first);
        Ok(())
    }

    #[test]
    fn only_a_person_acknowledges_a_settled_task_with_writes() -> TestResult {
        let setup = setup()?;
        setup.write("applied", None)?;
        let refused = |claimant: &Claimant| -> TestResult<Error> {
            setup
                .store
                .acknowledge_settled_writes(&setup.task, claimant, &reason()?, at(2))
                .err()
                .ok_or_else(|| "the acknowledgement was accepted".into())
        };

        // Unsettled.
        assert!(matches!(
            refused(&person()?)?,
            Error::State(StateError::TaskNotSettled(_))
        ));
        setup.settle(AttemptOutcome::Failed(FailureClass::Permanent))?;

        // No person present.
        let scheduled = Claimant::scheduled(HolderId::new("pickup")?);
        let event = Claimant {
            holder: HolderId::new("webhook")?,
            trigger: Trigger::Event(EventOrigin {
                house: HouseId::new("origin89")?,
                source: BackendId::new("github")?,
                event: ExternalRef::new("delivery-1")?,
            }),
            consumer: None,
        };
        for claimant in [&scheduled, &event] {
            assert!(matches!(
                refused(claimant)?,
                Error::State(StateError::AcknowledgementNeedsPerson)
            ));
        }

        // Reason bound: one byte over is refused, the bound itself is kept.
        let long = Text::new(&"r".repeat(MAX_ACKNOWLEDGEMENT_REASON_BYTES + 1))?;
        let error = setup
            .store
            .acknowledge_settled_writes(&setup.task, &person()?, &long, at(2))
            .err()
            .ok_or("an unbounded reason was accepted")?;
        assert!(matches!(
            error,
            Error::State(StateError::CapacityExceeded {
                limit: Limit::AcknowledgementReason
            })
        ));
        assert_eq!(setup.store.task(&setup.task)?.write_acknowledgement(), None);

        let exact = Text::new(&"r".repeat(MAX_ACKNOWLEDGEMENT_REASON_BYTES))?;
        let (first, earlier) =
            setup
                .store
                .acknowledge_settled_writes(&setup.task, &person()?, &exact, at(2))?;
        assert!(!earlier);
        assert_eq!(first.by.as_str(), "owner-session");
        assert_eq!(first.at, at(2));
        assert!(first.unresolved.is_empty());

        // Repeating keeps the first record, whoever asks.
        let again = Claimant::interactive(HolderId::new("second-session")?);
        let (second, earlier) =
            setup
                .store
                .acknowledge_settled_writes(&setup.task, &again, &reason()?, at(3))?;
        assert!(earlier);
        assert_eq!(second, first);
        Ok(())
    }

    #[test]
    fn nothing_is_acknowledged_without_a_write_or_after_success() -> TestResult {
        // Every write was refused: nothing reached the backend.
        let refused = setup()?;
        refused.write("refused", Some(ExecuteFault::Reject))?;
        refused.settle(AttemptOutcome::Failed(FailureClass::Permanent))?;
        let error = refused
            .store
            .acknowledge_settled_writes(&refused.task, &person()?, &reason()?, at(2))
            .err()
            .ok_or("a task without writes was acknowledged")?;
        assert!(matches!(
            error,
            Error::State(StateError::NothingToAcknowledge(_))
        ));

        // Settled successfully.
        let done = setup()?;
        done.write("applied", None)?;
        done.settle(AttemptOutcome::Succeeded)?;
        let error = done
            .store
            .acknowledge_settled_writes(&done.task, &person()?, &reason()?, at(2))
            .err()
            .ok_or("a successful task was acknowledged")?;
        assert!(matches!(
            error,
            Error::State(StateError::TaskSettled { .. })
        ));
        assert_eq!(done.store.task(&done.task)?.write_acknowledgement(), None);
        Ok(())
    }

    #[test]
    fn an_acknowledgement_lists_the_writes_still_unknown() -> TestResult {
        let setup = setup()?;
        setup.write("proven", None)?;
        setup.write("lost", Some(ExecuteFault::ApplyThenLoseResponse))?;
        setup.backend.fail_lookups(1);
        // Hand the lost launch over and waive it so the task can settle.
        setup.store.record_effect_outcome(
            &setup.task,
            setup.fence,
            EffectSeq::new(1),
            crate::state::EffectOutcome::Unresolvable,
            at(1),
        )?;
        let record = setup.store.task(&setup.task)?;
        let lost = record
            .effects()
            .get(1)
            .ok_or("no lost launch")?
            .request()
            .key()
            .clone();
        setup.store.accept_risk(
            &setup.task,
            setup.fence,
            EffectSeq::new(1),
            crate::state::RiskDecision {
                effect: lost,
                decided_by: HolderId::new("operator")?,
                revision: record.evidence().revision(),
                action: crate::state::RiskAction::SettleUnsuccessfully,
            },
            at(1),
        )?;
        setup.settle(AttemptOutcome::Failed(FailureClass::Permanent))?;

        // The lookup outage leaves the waived launch as it was.
        let report = reread_settled(&setup.store, &setup.backend, &setup.task, &Fixed)?;
        assert!(report.resolved.is_empty());
        assert!(matches!(setup.state_of(1)?, EffectState::Waived { .. }));

        let (recorded, _) =
            setup
                .store
                .acknowledge_settled_writes(&setup.task, &person()?, &reason()?, at(2))?;
        let names: Vec<&str> = recorded.unresolved.iter().map(EffectName::as_str).collect();
        assert_eq!(names, ["lost"]);
        Ok(())
    }
}
