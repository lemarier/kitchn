//! Scheduled and interactive triggers: shared claims, standing grants, and
//! per-effect consent. Simulated with temporary stores and the fake backend.

mod common;

use common::{
    Fixture, ManualClock, TestResult, at, backend_id, commit, creator, credential, grant, holder,
    house, interactive, launch, other_house, plan, scheduled, spec, task_id, ttl,
};
use kitchen::{
    Error, TaskId,
    contracts::{
        Authorization, Consent, ContractError, Evidence, EvidenceKind, EvidenceRevision,
        EvidenceSubject, EvidenceVerdict, ExternalRef, Fence, HouseGrants, Operation, Permission,
        Role, TaskAuthority, Text, Trigger, Workspace, fake::FakeBackend,
    },
    state::{EffectPlan, EffectState, OwnershipEvent, StateError, TaskState, run_effect},
};

/// A house whose policy permits launching and messaging workers but whose
/// standing grants only allow cancelling them.
fn house_policy() -> TestResult<HouseGrants> {
    Ok(HouseGrants::with_limits(
        house()?,
        [
            grant(Permission::LaunchWorker)?,
            grant(Permission::MessageWorker)?,
            grant(Permission::CancelWorker)?,
        ],
        [grant(Permission::CancelWorker)?],
    )?)
}

/// A task with no delegated standing authority, claimed interactively.
fn interactive_task(fixture: &Fixture, id: &str) -> TestResult<(TaskId, Fence)> {
    let task = task_id(id)?;
    let mut work = spec(id)?;
    work.authority = TaskAuthority::delegate(&house_policy()?, [])?;
    fixture
        .store
        .create_task(work, &interactive("session-1")?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &interactive("session-1")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    Ok((task, fence))
}

fn consent(id: &str, task: &TaskId, operation: Operation) -> TestResult<Consent> {
    let effect = operation.into();
    Ok(Consent {
        id: ExternalRef::new(id)?,
        given_by: holder("person")?,
        house: house()?,
        task: task.clone(),
        effect,
        revision: EvidenceRevision::INITIAL,
    })
}

fn with_consent(mut plan: EffectPlan, consent: Consent) -> EffectPlan {
    plan.consent = Some(consent);
    plan
}

#[test]
fn consent_authorizes_exactly_one_effect_and_is_recorded() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = interactive_task(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let approval = consent("approval-1", &task, launch()?)?;

    let launched = run_effect(
        &fixture.store,
        &backend,
        &house_policy()?,
        with_consent(plan(&task, fence, "launch", launch()?)?, approval.clone()),
        &clock,
    )?;
    assert!(matches!(launched.state(), EffectState::Applied { .. }));
    assert_eq!(
        launched.authorization(),
        &Authorization::Consent {
            id: approval.id.clone(),
            given_by: holder("person")?,
        }
    );
    assert_eq!(launched.request().credential(), &credential()?);

    // Repeating the same effect with its consent returns the recorded result.
    let repeated = run_effect(
        &fixture.store,
        &backend,
        &house_policy()?,
        with_consent(plan(&task, fence, "launch", launch()?)?, approval.clone()),
        &clock,
    )?;
    assert_eq!(repeated, launched);

    // The same approval cannot authorize a second launch.
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &house_policy()?,
            with_consent(plan(&task, fence, "relaunch", launch()?)?, approval),
            &clock,
        ),
        Err(Error::State(StateError::ConsentReused))
    ));
    assert_eq!(backend.effects_performed(), 1);
    Ok(())
}

#[test]
fn consent_for_a_different_effect_is_rejected() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = interactive_task(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let other_brief = Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new("A different brief.")?,
    };
    let other_task = consent("c-task", &task_id("task-2")?, launch()?)?;
    let mut other_house_consent = consent("c-house", &task, launch()?)?;
    other_house_consent.house = other_house()?;
    let mut stale = consent("c-stale", &task, launch()?)?;
    stale.revision = fixture.store.record_evidence(
        &task,
        fence,
        Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: commit('c')?,
                base: None,
            },
            source: ExternalRef::new("ci-1")?,
            observed_at: at(1),
        },
        at(1),
    )?;
    // The consent saw revision 1; move the evidence again before using it.
    let current = fixture.store.record_evidence(
        &task,
        fence,
        Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: EvidenceSubject {
                head: commit('d')?,
                base: None,
            },
            source: ExternalRef::new("ci-2")?,
            observed_at: at(2),
        },
        at(2),
    )?;

    let at_current = |mut consent: Consent| {
        consent.revision = current;
        consent
    };
    let matching = at_current(consent("c-ok", &task, launch()?)?);
    for mismatched in [
        at_current(consent("c-op", &task, other_brief)?),
        at_current(other_task),
        at_current(other_house_consent),
        stale,
    ] {
        let mut attempt = with_consent(plan(&task, fence, "launch", launch()?)?, mismatched);
        attempt.decided_at = current;
        assert!(matches!(
            run_effect(&fixture.store, &backend, &house_policy()?, attempt, &clock),
            Err(Error::Contract(ContractError::ConsentMismatch))
        ));
    }
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(backend.execute_calls(), 0);

    // The matching consent differs from each case in exactly one part.
    let mut accepted = with_consent(plan(&task, fence, "launch", launch()?)?, matching);
    accepted.decided_at = current;
    let launched = run_effect(&fixture.store, &backend, &house_policy()?, accepted, &clock)?;
    assert!(matches!(launched.state(), EffectState::Applied { .. }));
    Ok(())
}

#[test]
fn scheduled_run_acts_only_on_standing_grants() -> TestResult {
    let fixture = Fixture::new()?;
    let task = task_id("task-1")?;
    let mut work = spec("task-1")?;
    work.authority = TaskAuthority::delegate(&house_policy()?, [grant(Permission::CancelWorker)?])?;
    fixture.store.create_task(work, &creator()?, at(0))?;
    let fence = fixture
        .store
        .claim(&task, &scheduled("tick-1")?, ttl(600)?, at(0))?
        .fence();
    fixture.store.start_attempt(&task, fence, at(0))?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);

    // Policy permits launching, but no standing grant does.
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &house_policy()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock
        ),
        Err(Error::Contract(ContractError::PermissionDenied {
            permission: Permission::LaunchWorker
        }))
    ));
    // A scheduled run never uses a person's consent.
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &house_policy()?,
            with_consent(
                plan(&task, fence, "launch", launch()?)?,
                consent("c-1", &task, launch()?)?
            ),
            &clock,
        ),
        Err(Error::Contract(ContractError::ConsentNotAccepted))
    ));
    assert!(fixture.store.task(&task)?.effects().is_empty());
    assert_eq!(backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn interactive_consent_cannot_exceed_house_policy() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = interactive_task(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let narrow = HouseGrants::with_limits(house()?, [grant(Permission::MessageWorker)?], [])?;
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &narrow,
            with_consent(
                plan(&task, fence, "launch", launch()?)?,
                consent("c-1", &task, launch()?)?
            ),
            &clock,
        ),
        Err(Error::Contract(ContractError::AuthorityExpansion {
            permission: Permission::LaunchWorker,
            ..
        }))
    ));
    // Without consent, interactive work has no authority at all.
    assert!(matches!(
        run_effect(
            &fixture.store,
            &backend,
            &house_policy()?,
            plan(&task, fence, "launch", launch()?)?,
            &clock
        ),
        Err(Error::Contract(ContractError::ConsentRequired {
            permission: Permission::LaunchWorker
        }))
    ));
    assert_eq!(backend.execute_calls(), 0);
    Ok(())
}

#[test]
fn triggers_share_claims_and_hand_over_through_relinquish_and_adopt() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let issue = task_id("km43-issue-12")?;
    store.create_task(spec("km43-issue-12")?, &interactive("session-1")?, at(0))?;
    let person = store.claim(&issue, &interactive("session-1")?, ttl(600)?, at(1))?;
    assert!(matches!(
        store.claim(&issue, &scheduled("pickup-tick")?, ttl(60)?, at(2)),
        Err(Error::State(StateError::ClaimHeld { holder: current, .. })) if current == holder("session-1")?
    ));

    // A scheduled claim blocks an interactive one the same way.
    let pickup = task_id("firmware-issue-7")?;
    store.create_task(spec("firmware-issue-7")?, &creator()?, at(0))?;
    store.claim(&pickup, &scheduled("pickup-tick")?, ttl(60)?, at(1))?;
    assert!(matches!(
        store.claim(&pickup, &interactive("session-1")?, ttl(600)?, at(2)),
        Err(Error::State(StateError::ClaimHeld { .. }))
    ));

    // The person hands the issue to scheduled work.
    store.relinquish(&issue, person.fence(), at(3))?;
    let adopted = store.claim(&issue, &scheduled("pickup-tick")?, ttl(60)?, at(4))?;
    assert_eq!(adopted.trigger(), Trigger::Scheduled);
    let record = store.task(&issue)?;
    assert_eq!(record.created_by().trigger, Trigger::Interactive);
    assert!(
        matches!(record.state(), TaskState::Claimed { lease } if lease.trigger() == Trigger::Scheduled)
    );
    assert!(matches!(
        record.ownership(),
        [
            OwnershipEvent::Claimed { trigger: Trigger::Interactive, .. },
            OwnershipEvent::Relinquished { fence, .. },
            OwnershipEvent::Adopted { previous, trigger: Trigger::Scheduled, .. },
        ] if *fence == person.fence() && *previous == person.fence()
    ));
    Ok(())
}

#[test]
fn a_resubmission_fails_closed_when_the_permitted_credential_changed() -> TestResult {
    let fixture = Fixture::new()?;
    let (task, fence) = interactive_task(&fixture, "task-1")?;
    let backend = FakeBackend::fully_capable(backend_id()?, house()?);
    let clock = ManualClock::starting_at(1);
    let approval = consent("approval-1", &task, launch()?)?;
    backend.fail_lookups(100);
    backend.inject(kitchen::contracts::fake::ExecuteFault::TimeoutWithoutApplying);
    let lost = run_effect(
        &fixture.store,
        &backend,
        &house_policy()?,
        with_consent(plan(&task, fence, "launch", launch()?)?, approval.clone()),
        &clock,
    )?;
    assert_eq!(lost.request().credential(), &credential()?);

    // House policy now permits the launch only with another credential.
    let rotated = HouseGrants::with_limits(
        house()?,
        [kitchen::contracts::Grant::house(
            Permission::LaunchWorker,
            backend_id()?,
            kitchen::CredentialId::new("rotated-token")?,
        )],
        [],
    )?;
    let retried = run_effect(
        &fixture.store,
        &backend,
        &rotated,
        with_consent(plan(&task, fence, "launch", launch()?)?, approval),
        &clock,
    );
    assert!(
        matches!(
            retried,
            Err(Error::Contract(ContractError::CredentialChanged))
        ),
        "{retried:?}"
    );
    assert_eq!(
        backend.execute_calls(),
        1,
        "nothing ran with the old credential"
    );
    Ok(())
}
