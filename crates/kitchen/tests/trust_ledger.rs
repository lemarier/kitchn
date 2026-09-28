//! Offline fixtures: no live usage, GitHub, model, or equipment is exercised.
mod common;
use common::{
    Fixture, TestResult, at, commit, creator, grants, holder, house, other_house, scheduled, spec,
    task_id, ttl,
};
use kitchen::{
    contracts::{
        AttemptNumber, AttemptOutcome, EvidenceSubject, ExternalRef, Grant, Permission, Repository,
        Text,
    },
    state::{Corruption, StateError},
    trust::{
        Attribution, AutonomyGrant, AutonomyProposal, EvidenceMode, Ledger, Measurement,
        Observation, PullRequestEvidence, StationScope, TrustError,
    },
    workflows::inspector::{FollowUpRoute, InspectionPlan, SampleReservation, SampleResult},
};
use std::{fs, num::NonZeroU32};

fn source(value: &str) -> TestResult<ExternalRef> {
    Ok(ExternalRef::new(value)?)
}
fn measured<T>(value: T) -> TestResult<Measurement<T>> {
    Ok(Measurement::Observed {
        value,
        samples: NonZeroU32::MIN,
        source: source("fixture:source")?,
    })
}
fn scope() -> TestResult<StationScope> {
    Ok(StationScope {
        station: Text::new("rust")?,
        project: Repository::new("example/project")?,
        work_type: Text::new("implementation")?,
    })
}
fn attribution() -> TestResult<Attribution> {
    Ok(Attribution {
        scope: scope()?,
        agent: measured(holder("worker")?)?,
        model: measured(Text::new("fixture-model-v1")?)?,
        tokens: measured(120)?,
    })
}
fn observation(f: &Fixture) -> TestResult<Observation> {
    let mut task = spec("task")?;
    task.repository = Some(scope()?.project);
    f.store.create_task(task, &creator()?, at(0))?;
    let lease = f
        .store
        .claim(&task_id("task")?, &scheduled("owner")?, ttl(60)?, at(1))?;
    f.store
        .start_attempt(&task_id("task")?, lease.fence(), at(2))?;
    f.store.finish_attempt(
        &task_id("task")?,
        lease.fence(),
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(3),
    )?;
    Ok(Observation::collect(
        &f.store,
        &task_id("task")?,
        source("fixture:task")?,
        attribution()?,
        EvidenceMode::Simulated,
        at(4),
    )?)
}
fn ledger(f: &Fixture) -> TestResult<Ledger> {
    Ok(Ledger::initialize(f.dir.path().join("trust"), house()?)?)
}
fn bind_evidence(l: &Ledger, f: &Fixture) -> TestResult {
    let task = f.store.task(&task_id("task")?)?;
    l.bind_task(
        task.spec(),
        scope()?,
        Text::new("fixture-model-v1")?,
        source("fixture:task-binding")?,
    )?;
    Ok(())
}
fn reopen(f: &Fixture) -> TestResult<Ledger> {
    Ok(Ledger::open(f.dir.path().join("trust"), house()?)?)
}
fn subject() -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit('a')?,
        base: Some(commit('b')?),
    })
}
fn with_pr(mut o: Observation) -> TestResult<Observation> {
    o.pull_request = measured(PullRequestEvidence {
        house: house()?,
        task: o.task.clone(),
        repository: scope()?.project,
        source: source("https://example.invalid/pr/1")?,
        subject: subject()?,
        first_pass: Measurement::Missing,
        findings: Measurement::Missing,
        reverts: Measurement::Missing,
        regressions: Measurement::Missing,
        checks: Measurement::Unavailable,
    })?;
    Ok(o)
}
fn eligible(mut o: Observation) -> TestResult<Observation> {
    o = with_pr(o)?;
    o.mode = EvidenceMode::Live;
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.first_pass = measured(true)?;
        value.findings = measured(Vec::new())?;
        value.reverts = measured(Vec::new())?;
        value.regressions = measured(Vec::new())?;
        value.checks = measured(vec![kitchen::contracts::Evidence {
            kind: kitchen::contracts::EvidenceKind::Check,
            verdict: kitchen::contracts::EvidenceVerdict::Pass,
            subject: value.subject.clone(),
            source: source("fixture:passing-check")?,
            observed_at: at(4),
        }])?;
    }
    Ok(o)
}
fn grant() -> TestResult<AutonomyGrant> {
    Ok(AutonomyGrant {
        id: source("fixture:grant")?,
        house: house()?,
        scope: scope()?,
        claim: Grant::repository(
            Permission::LaunchWorker,
            scope()?.project,
            common::backend_id()?,
            common::credential()?,
        ),
        approved_by: holder("owner")?,
        decision: source("fixture:decision")?,
        evidence: vec![(source("fixture:task")?, NonZeroU32::MIN)],
        proposal: None,
        at: at(5),
    })
}
fn revoke(l: &Ledger) -> TestResult<bool> {
    Ok(l.revoke(
        &grant()?.id,
        holder("owner")?,
        source("fixture:revoke")?,
        at(6),
    )?)
}
fn plan() -> TestResult<InspectionPlan> {
    Ok(InspectionPlan {
        id: source("fixture:inspection")?,
        house: house()?,
        observation: source("fixture:task")?,
        question: Text::new("Does the changed parser reject duplicate keys?")?,
        inspector: holder("independent-reviewer")?,
        independent: true,
        max_samples: 2,
        max_tokens: 100,
        deadline: at(60),
    })
}

#[test]
fn derives_task_attempt_effect_evidence_without_inventing_acceptance() -> TestResult {
    let f = Fixture::new()?;
    let o = observation(&f)?;
    assert_eq!(o.attempts.len(), 1);
    assert_eq!(o.attempts[0].0, AttemptNumber::FIRST);
    assert!(matches!(
        o.state,
        kitchen::state::TaskState::Settled {
            settlement: kitchen::contracts::Settlement::Succeeded,
            ..
        }
    ));
    assert_eq!(
        o.instructions,
        f.store.task(&task_id("task")?)?.spec().provenance
    );
    assert_eq!(o.pull_request, Measurement::Missing);
    assert_eq!(o.bench, Measurement::Missing);
    assert!(!o.trust_eligible());
    let mut wrong = attribution()?;
    wrong.scope.project = Repository::new("other/project")?;
    assert!(
        Observation::collect(
            &f.store,
            &task_id("task")?,
            source("fixture:wrong")?,
            wrong,
            EvidenceMode::Live,
            at(5)
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn duplicate_reordered_corrections_preserve_history_and_sample_sizes() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let first = observation(&f)?;
    let mut corrected = first.clone();
    corrected.revision = NonZeroU32::new(2).ok_or("revision")?;
    corrected.correction = Some(source("fixture:attribution-investigation")?);
    corrected.attribution.agent = measured(holder("actual-worker")?)?;
    assert!(l.record(&f.store, corrected.clone())?);
    assert!(matches!(l.latest(&first.id), Err(TrustError::Incomplete)));
    assert!(l.record(&f.store, first.clone())?);
    assert!(!l.record(&f.store, first.clone())?);
    assert_eq!(reopen(&f)?.latest(&first.id)?, corrected);
    assert_eq!(l.history()?.len(), 2);
    let mut conflict = first.clone();
    conflict.attribution.tokens = measured(0)?;
    assert!(matches!(
        l.record(&f.store, conflict),
        Err(TrustError::Conflict)
    ));
    assert_eq!(l.history()?.len(), 2);
    assert_eq!(l.latest(&first.id)?.attribution.tokens, measured(120)?);
    Ok(())
}

#[test]
fn incomplete_bench_and_cross_house_inputs_never_pass() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = with_pr(observation(&f)?)?;
    o.bench = Measurement::Untested;
    l.record(&f.store, o.clone())?;
    assert_eq!(l.latest(&o.id)?.bench, Measurement::Untested);
    let mut other = o.clone();
    other.house = other_house()?;
    other.id = source("fixture:other")?;
    assert!(l.record(&f.store, other).is_err());
    assert!(Ledger::open(f.dir.path().join("trust"), other_house()?).is_err());
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.house = other_house()?;
    }
    assert!(l.record(&f.store, o).is_err());
    Ok(())
}

#[test]
fn unknown_attribution_and_usage_do_not_derive_trust() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = observation(&f)?;
    // This flag models the live adapter contract using fixtures; it is not live evidence.
    o.mode = EvidenceMode::Live;
    o.attribution.tokens = Measurement::Unavailable;
    assert!(!o.trust_eligible());
    l.record(&f.store, o.clone())?;
    assert!(matches!(
        l.grant(grant()?, &grants()?),
        Err(TrustError::Refused)
    ));
    assert_eq!(
        l.latest(&o.id)?.attribution.tokens,
        Measurement::Unavailable
    );
    o.attribution.tokens = measured(120)?;
    o.attribution.agent = Measurement::Missing;
    assert!(!o.trust_eligible());
    o.attribution.agent = measured(holder("worker")?)?;
    o.attribution.model = Measurement::Missing;
    assert!(!o.trust_eligible());
    assert!(l.grant_history()?.is_empty());
    Ok(())
}

#[test]
fn trust_requires_success_positive_usage_and_positive_pr_evidence() -> TestResult {
    use kitchen::{contracts::Settlement, state::TaskState};
    let f = Fixture::new()?;
    let mut o = eligible(observation(&f)?)?;
    assert!(o.trust_eligible());
    o.attribution.tokens = measured(0)?;
    assert!(!o.trust_eligible());
    o.attribution.tokens = measured(120)?;
    for settlement in [
        Settlement::Failed,
        Settlement::Cancelled,
        Settlement::Exhausted,
    ] {
        let mut bad = o.clone();
        if let TaskState::Settled {
            settlement: value, ..
        } = &mut bad.state
        {
            *value = settlement;
        }
        assert!(!bad.trust_eligible());
    }
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.first_pass = measured(false)?;
    }
    assert!(!o.trust_eligible());
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.first_pass = measured(true)?;
        if let Measurement::Observed { value: checks, .. } = &mut value.checks {
            checks[0].verdict = kitchen::contracts::EvidenceVerdict::Fail;
        }
    }
    assert!(!o.trust_eligible());
    if let Measurement::Observed { value, .. } = &mut o.pull_request
        && let Measurement::Observed { value: checks, .. } = &mut value.checks
    {
        checks[0].verdict = kitchen::contracts::EvidenceVerdict::Pass;
    }
    o.bench = measured(vec![kitchen::trust::BenchResult {
        subject: subject()?,
        passed: false,
        procedure: Text::new("bench fixture")?,
    }])?;
    assert!(!o.trust_eligible());
    o.bench = Measurement::Missing;
    o.pull_request = Measurement::Missing;
    assert!(!o.trust_eligible());
    Ok(())
}

#[test]
fn fabricated_core_outcome_and_scope_correction_are_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = eligible(observation(&f)?)?;
    let original = o.clone();
    o.task = task_id("absent")?;
    assert!(l.record(&f.store, o).is_err());
    let mut o = original.clone();
    o.effects.clear();
    o.state = kitchen::state::TaskState::Settled {
        settlement: kitchen::contracts::Settlement::Failed,
        at: at(10),
    };
    assert!(l.record(&f.store, o).is_err());
    l.record(&f.store, original.clone())?;
    let mut corrected = original;
    corrected.revision = NonZeroU32::new(2).ok_or("revision")?;
    corrected.correction = Some(source("fixture:scope-correction")?);
    corrected.attribution.scope.station = Text::new("other-station")?;
    assert!(l.record(&f.store, corrected).is_err());
    Ok(())
}

#[test]
fn nonterminal_task_cannot_be_recorded_as_completed_evidence() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut task = spec("running")?;
    task.repository = Some(scope()?.project);
    f.store.create_task(task, &creator()?, at(0))?;
    let mut o = Observation::collect(
        &f.store,
        &task_id("running")?,
        source("fixture:running")?,
        attribution()?,
        EvidenceMode::Live,
        at(1),
    )?;
    assert!(matches!(
        l.record(&f.store, o.clone()),
        Err(TrustError::Refused)
    ));
    o.state = kitchen::state::TaskState::Settled {
        settlement: kitchen::contracts::Settlement::Succeeded,
        at: at(2),
    };
    assert!(matches!(l.record(&f.store, o), Err(TrustError::Refused)));
    Ok(())
}

#[test]
fn task_binding_is_write_once_and_rejects_role_confusion() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut task = spec("bound-task")?;
    task.repository = Some(scope()?.project);
    let model = Text::new("fixture-model-v1")?;
    let origin = source("fixture:binding")?;
    assert!(l.bind_task(&task, scope()?, model.clone(), origin.clone())?);
    assert!(!l.bind_task(&task, scope()?, model.clone(), origin)?);
    let mut changed = task.clone();
    changed.provenance.house_guidance = commit('c')?;
    assert!(matches!(
        l.bind_task(
            &changed,
            scope()?,
            model.clone(),
            source("fixture:binding")?
        ),
        Err(TrustError::Conflict)
    ));
    let mut wrong_role = scope()?;
    wrong_role.station = Text::new("inspector")?;
    assert!(matches!(
        l.bind_task(&task, wrong_role, model, source("fixture:other")?),
        Err(TrustError::Refused)
    ));
    Ok(())
}

#[test]
fn revocation_succeeds_at_history_capacity_and_stays_revoked() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    l.grant(grant()?, &grants()?)?;
    let path = f.dir.path().join("trust/ledger.json");
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let audits = document["grants"].as_array_mut().ok_or("grants")?;
    let original = audits.first().cloned().ok_or("grant")?;
    for index in 1..4094 {
        let mut additional = original.clone();
        additional["Issued"]["id"] = serde_json::json!(format!("fixture:grant-{index}"));
        audits.push(additional);
    }
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert!(revoke(&l)?);
    assert!(!revoke(&reopen(&f)?)?);
    assert!(matches!(
        l.grant(grant()?, &grants()?),
        Err(TrustError::Conflict)
    ));
    assert_eq!(l.grant_history()?.len(), 4094);
    Ok(())
}

#[test]
fn waiting_writer_yields_to_pending_revocation() -> TestResult {
    use std::{fs::OpenOptions, thread, time::Duration};
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    l.grant(grant()?, &grants()?)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.dir.path().join("trust/ledger.lock"))?;
    lock.lock()?;
    let revoker = l.clone();
    let grant_id = grant()?.id;
    let actor = holder("owner")?;
    let decision = source("fixture:revoke")?;
    let revocation = thread::spawn(move || revoker.revoke(&grant_id, actor, decision, at(6)));
    let pending = f.dir.path().join("trust/revoke.pending");
    for _ in 0..100 {
        if pending.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(pending.exists());
    let reader = l.clone();
    let read = thread::spawn(move || reader.grant_history());
    drop(lock);
    assert!(revocation.join().map_err(|_| "revoker panicked")??);
    let history = read.join().map_err(|_| "reader panicked")??;
    assert!(matches!(
        history.as_slice(),
        [kitchen::trust::GrantAudit::Revoked { .. }]
    ));
    Ok(())
}

#[test]
fn proposal_within_limits_needs_explicit_approval_and_can_be_revoked() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let g = grant()?;
    let policy = kitchen::contracts::HouseGrants::with_limits(house()?, [g.claim.clone()], [])?;
    assert!(matches!(
        l.grant(g.clone(), &policy),
        Err(TrustError::Refused)
    ));
    let proposal = AutonomyProposal {
        id: g.id.clone(),
        house: g.house.clone(),
        scope: g.scope.clone(),
        claim: g.claim.clone(),
        evidence: g.evidence.clone(),
        source: source("fixture:proposal")?,
        at: at(5),
    };
    assert!(l.propose(proposal.clone(), &policy)?);
    assert!(!l.propose(proposal, &policy)?);
    assert!(matches!(
        l.grant_history()?.as_slice(),
        [kitchen::trust::GrantAudit::Proposed(_)]
    ));
    assert!(l.approve(
        &g.id,
        holder("owner")?,
        source("fixture:approval")?,
        at(6),
        &policy
    )?);
    assert!(matches!(
        l.grant_history()?.as_slice(),
        [kitchen::trust::GrantAudit::Issued(AutonomyGrant {
            proposal: Some(_),
            ..
        })]
    ));
    assert!(revoke(&l)?);
    assert!(matches!(
        reopen(&f)?.grant_history()?.as_slice(),
        [kitchen::trust::GrantAudit::Revoked { .. }]
    ));
    Ok(())
}

#[test]
fn explicit_grant_uses_core_authority_and_revocation_survives_restart() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let g = grant()?;
    let policy = kitchen::contracts::HouseGrants::with_limits(house()?, [g.claim.clone()], [])?;
    let proposal = AutonomyProposal {
        id: g.id.clone(),
        house: g.house.clone(),
        scope: g.scope.clone(),
        claim: g.claim.clone(),
        evidence: g.evidence.clone(),
        source: source("fixture:proposal")?,
        at: at(5),
    };
    l.propose(proposal, &policy)?;
    let mut acting = spec("acting")?;
    acting.repository = Some(scope()?.project);
    acting.authority = kitchen::contracts::TaskAuthority::delegate(&policy, [])?;
    l.bind_task(
        &acting,
        scope()?,
        Text::new("fixture-model-v1")?,
        source("fixture:acting-binding")?,
    )?;
    assert!(
        !l.standing_for_task(&f.store, &acting, &policy)?
            .covers(&g.claim)
    );
    l.approve(
        &g.id,
        holder("owner")?,
        source("fixture:approval")?,
        at(6),
        &policy,
    )?;
    let projected = l.standing_for_task(&f.store, &acting, &policy)?;
    assert!(projected.covers(&g.claim));
    let foreign_policy = kitchen::contracts::HouseGrants::new(other_house()?, []);
    assert!(matches!(
        l.standing_for_task(&f.store, &acting, &foreign_policy),
        Err(TrustError::Refused)
    ));
    acting.authority = kitchen::contracts::TaskAuthority::delegate(&projected, [g.claim.clone()])?;
    f.store.create_task(acting.clone(), &creator()?, at(7))?;
    assert_eq!(
        acting.authority.authorize(
            &l.standing_for_task(&f.store, &acting, &policy)?,
            g.claim.permission,
            &g.claim.scope,
            &g.claim.destination
        )?,
        g.claim.credential
    );
    let mut other_scope = scope()?;
    other_scope.work_type = Text::new("release")?;
    let mut other = spec("other-acting")?;
    other.repository = Some(scope()?.project);
    other.authority = kitchen::contracts::TaskAuthority::delegate(&policy, [])?;
    l.bind_task(
        &other,
        other_scope,
        Text::new("fixture-model-v1")?,
        source("fixture:other-binding")?,
    )?;
    assert!(
        !l.standing_for_task(&f.store, &other, &policy)?
            .covers(&g.claim)
    );
    let mut altered = acting.clone();
    altered.provenance.house_guidance = commit('c')?;
    assert!(matches!(
        l.standing_for_task(&f.store, &altered, &policy),
        Err(TrustError::Refused)
    ));
    assert!(revoke(&l)?);
    assert!(!revoke(&l)?);
    let after = reopen(&f)?.standing_for_task(&f.store, &acting, &policy)?;
    assert!(!after.covers(&g.claim));
    assert!(
        acting
            .authority
            .authorize(
                &after,
                g.claim.permission,
                &g.claim.scope,
                &g.claim.destination
            )
            .is_err()
    );
    assert_eq!(l.grant_history()?.len(), 1);
    Ok(())
}

#[test]
fn unknown_revocation_and_privileged_grants_are_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = eligible(observation(&f)?)?;
    l.record(&f.store, o)?;
    bind_evidence(&l, &f)?;
    assert!(matches!(
        l.revoke(
            &grant()?.id,
            holder("owner")?,
            source("fixture:revoke")?,
            at(6)
        ),
        Err(TrustError::NotFound)
    ));
    for permission in [
        Permission::ReleaseResource,
        Permission::PushBranch,
        Permission::OpenPullRequest,
        Permission::Merge,
        Permission::ManageSchedule,
        Permission::Publish,
        Permission::OperateEquipment,
        Permission::ActivateSchedule,
        Permission::TrialSchedule,
    ] {
        let mut g = grant()?;
        g.claim.permission = permission;
        let policy = kitchen::contracts::HouseGrants::new(house()?, [g.claim.clone()]);
        assert!(matches!(l.grant(g, &policy), Err(TrustError::Refused)));
    }
    let mut g = grant()?;
    g.house = other_house()?;
    assert!(l.grant(g, &grants()?).is_err());
    Ok(())
}

#[test]
fn corrected_grant_evidence_requires_new_explicit_approval() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = eligible(observation(&f)?)?;
    l.record(&f.store, o.clone())?;
    bind_evidence(&l, &f)?;
    l.grant(grant()?, &grants()?)?;
    o.revision = NonZeroU32::new(2).ok_or("revision")?;
    o.correction = Some(source("fixture:correction")?);
    l.record(&f.store, o)?;
    let g = grant()?;
    let policy = kitchen::contracts::HouseGrants::with_limits(house()?, [g.claim.clone()], [])?;
    let task = f.store.task(&task_id("task")?)?;
    assert!(
        !l.standing_for_task(&f.store, task.spec(), &policy)?
            .covers(&g.claim)
    );
    assert_eq!(l.grant_history()?.len(), 1);
    Ok(())
}

#[test]
fn inspection_reserves_before_execution_and_restarts_without_budget_reset() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, with_pr(observation(&f)?)?)?;
    l.start_inspection(plan()?, at(5))?;
    let sample = l.reserve_sample(&plan()?.id, 1, 60, at(6))?;
    let SampleReservation::Reserved(sample) = sample else {
        return Err("new reservation expected".into());
    };
    assert_eq!(
        SampleReservation::Existing(sample),
        reopen(&f)?.reserve_sample(&plan()?.id, 1, 60, at(7))?
    );
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 2, 40, at(7)),
        Err(TrustError::Incomplete)
    ));
    l.finish_sample(&plan()?.id, 1, SampleResult::Unavailable)?;
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 2, 41, at(8)),
        Err(TrustError::Exhausted)
    ));
    l.reserve_sample(&plan()?.id, 2, 40, at(8))?;
    l.finish_sample(
        &plan()?.id,
        2,
        SampleResult::NoFinding {
            source: source("fixture:check")?,
        },
    )?;
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 3, 1, at(9)),
        Err(TrustError::Exhausted)
    ));
    assert_eq!(l.start_inspection(plan()?, at(10))?.samples().len(), 2);
    Ok(())
}

#[test]
fn inspection_independence_missing_evidence_and_deadline_fail_closed() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = with_pr(observation(&f)?)?;
    o.attribution.agent = Measurement::Unavailable;
    l.record(&f.store, o.clone())?;
    assert!(matches!(
        l.start_inspection(plan()?, at(5)),
        Err(TrustError::Refused)
    ));
    o.revision = NonZeroU32::new(2).ok_or("revision")?;
    o.correction = Some(source("fixture:known-agent")?);
    o.attribution.agent = measured(holder("independent-reviewer")?)?;
    l.record(&f.store, o)?;
    assert!(matches!(
        l.start_inspection(plan()?, at(5)),
        Err(TrustError::Refused)
    ));
    let mut p = plan()?;
    p.independent = false;
    l.start_inspection(p.clone(), at(5))?;
    assert!(matches!(
        l.reserve_sample(&p.id, 1, 1, at(60)),
        Err(TrustError::Exhausted)
    ));
    assert!(l.reserve_sample(&p.id, 0, 1, at(6)).is_err());
    assert!(matches!(
        l.reserve_sample(&p.id, 1, 1, at(4)),
        Err(TrustError::Invalid)
    ));
    l.cancel_inspection(&p.id)?;
    assert!(matches!(
        l.reserve_sample(&p.id, 1, 1, at(6)),
        Err(TrustError::Refused)
    ));
    Ok(())
}

#[test]
fn confirmed_inspector_findings_route_once_with_exact_revision() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, with_pr(observation(&f)?)?)?;
    l.start_inspection(plan()?, at(5))?;
    l.reserve_sample(&plan()?.id, 1, 20, at(6))?;
    let result = SampleResult::Confirmed {
        finding: kitchen::trust::Finding {
            source: source("fixture:finding")?,
            subject: subject()?,
            consequence: Text::new("Duplicate keys overwrite the original value.")?,
        },
        route: FollowUpRoute::Test,
    };
    let mut stale = result.clone();
    if let SampleResult::Confirmed { finding, .. } = &mut stale {
        finding.subject.head = commit('c')?;
    }
    assert!(l.finish_sample(&plan()?.id, 1, stale).is_err());
    assert!(l.finish_sample(&plan()?.id, 1, result.clone())?);
    assert!(!l.finish_sample(&plan()?.id, 1, result)?);
    assert!(
        l.finish_sample(&plan()?.id, 1, SampleResult::Unavailable)
            .is_err()
    );
    let inspection = reopen(&f)?.inspection(&plan()?.id)?;
    let routes: Vec<_> = inspection.follow_ups().collect();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].0, 1);
    assert_eq!(routes[0].2, FollowUpRoute::Test);
    Ok(())
}

#[test]
fn storage_corruption_and_partial_initialization_never_reset_history() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, observation(&f)?)?;
    let path = f.dir.path().join("trust/ledger.json");
    fs::write(&path, b"{bad")?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Storage(StateError::CorruptState(
            Corruption::Syntax { .. }
        )))
    ));
    assert_eq!(fs::read(&path)?, b"{bad");
    assert!(matches!(
        Ledger::initialize(f.dir.path().join("trust"), house()?),
        Err(TrustError::Storage(StateError::AlreadyInitialized))
    ));
    assert_eq!(fs::read(&path)?, b"{bad");
    // A crash after the marker leaves a store without a snapshot: it is
    // neither reinitialized nor opened as empty history.
    let partial = f.dir.path().join("partial");
    Ledger::initialize(&partial, house()?)?;
    fs::remove_file(partial.join("ledger.json"))?;
    assert!(matches!(
        Ledger::initialize(&partial, house()?),
        Err(TrustError::Storage(StateError::AlreadyInitialized))
    ));
    assert!(matches!(
        Ledger::open(&partial, house()?),
        Err(TrustError::Storage(StateError::StateMissing))
    ));
    assert!(!partial.join("ledger.json").exists());
    // An existing empty directory holds no store and may be initialized.
    let empty = f.dir.path().join("empty");
    fs::create_dir(&empty)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&empty, fs::Permissions::from_mode(0o755))?;
        assert!(matches!(
            Ledger::initialize(&empty, house()?),
            Err(TrustError::Storage(StateError::PublicPath))
        ));
        fs::set_permissions(&empty, fs::Permissions::from_mode(0o700))?;
    }
    assert!(matches!(
        Ledger::open(&empty, house()?),
        Err(TrustError::Storage(StateError::NotInitialized))
    ));
    Ledger::initialize(&empty, house()?)?;
    assert!(Ledger::open(&empty, house()?)?.history()?.is_empty());
    Ok(())
}

#[test]
fn ledger_marker_binds_snapshot_to_one_store_identity() -> TestResult {
    let f = Fixture::new()?;
    let first = ledger(&f)?;
    let second = Ledger::initialize(f.dir.path().join("second-trust"), house()?)?;
    first.record(&f.store, observation(&f)?)?;
    fs::copy(
        f.dir.path().join("trust/ledger.json"),
        f.dir.path().join("second-trust/ledger.json"),
    )?;
    let identity = |result: Result<_, TrustError>| {
        matches!(
            result,
            Err(TrustError::Storage(StateError::CorruptState(
                Corruption::StoreIdentity
            )))
        )
    };
    assert!(identity(
        Ledger::open(f.dir.path().join("second-trust"), house()?).map(|_| ())
    ));
    assert!(identity(second.history().map(|_| ())));
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), other_house()?),
        Err(TrustError::Authority(_))
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn storage_rejects_symlinks_public_permissions_and_repository_paths() -> TestResult {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, observation(&f)?)?;
    let link = f.dir.path().join("link");
    symlink(f.dir.path().join("trust"), &link)?;
    assert!(matches!(
        Ledger::open(link, house()?),
        Err(TrustError::Storage(StateError::RedirectedPath))
    ));
    let snapshot = f.dir.path().join("trust/ledger.json");
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o644))?;
    assert!(matches!(
        l.history(),
        Err(TrustError::Storage(StateError::PublicPath))
    ));
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600))?;
    let dir = f.dir.path().join("trust");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Storage(StateError::PublicPath))
    ));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let repo = f.dir.path().join("repo");
    fs::create_dir(&repo)?;
    fs::write(repo.join(".git"), b"gitdir: elsewhere")?;
    assert!(matches!(
        Ledger::initialize(repo.join("private"), house()?),
        Err(TrustError::Storage(StateError::StorageInsideRepository))
    ));
    let original = fs::read(&snapshot)?;
    fs::remove_file(&snapshot)?;
    let elsewhere = f.dir.path().join("outside");
    fs::write(&elsewhere, &original)?;
    symlink(&elsewhere, &snapshot)?;
    assert!(matches!(
        l.history(),
        Err(TrustError::Storage(StateError::RedirectedPath))
    ));
    assert_eq!(fs::read(elsewhere)?, original);
    Ok(())
}

#[test]
fn sourced_pr_and_bench_fixtures_preserve_measurements_and_reject_stale_findings() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = with_pr(observation(&f)?)?;
    let finding = kitchen::trust::Finding {
        source: source("fixture:review/1")?,
        subject: subject()?,
        consequence: Text::new("Regression reproduced by the test.")?,
    };
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.first_pass = measured(false)?;
        value.findings = measured(vec![finding.clone()])?;
        value.reverts = measured(Vec::new())?;
        value.regressions = measured(vec![finding.clone()])?;
    }
    o.bench = measured(vec![kitchen::trust::BenchResult {
        subject: subject()?,
        passed: false,
        procedure: Text::new("power interruption test")?,
    }])?;
    // Exercise the adapter serialization boundary with a sanitized fixture.
    let bytes = serde_json::to_vec(&o)?;
    let decoded: Observation = serde_json::from_slice(&bytes)?;
    l.record(&f.store, decoded.clone())?;
    assert_eq!(l.latest(&o.id)?, decoded);
    let mut correction = decoded;
    correction.revision = NonZeroU32::new(2).ok_or("revision")?;
    correction.correction = Some(source("fixture:investigation")?);
    if let Measurement::Observed { value, .. } = &mut correction.pull_request {
        value.subject.head = commit('d')?;
    }
    assert!(matches!(
        l.record(&f.store, correction),
        Err(TrustError::Invalid)
    ));
    let invalid = String::from_utf8(bytes)?.replace("\"samples\":1", "\"samples\":0");
    assert!(serde_json::from_str::<Observation>(&invalid).is_err());
    Ok(())
}

#[test]
fn concurrent_duplicate_delivery_has_one_writer_and_no_lost_history() -> TestResult {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = observation(&f)?;
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let l = l.clone();
        let store = f.store.clone();
        let o = o.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            l.record(&store, o)
        }));
    }
    let mut inserted = 0;
    for handle in handles {
        if handle.join().map_err(|_| "thread failed")?? {
            inserted += 1;
        }
    }
    assert_eq!(inserted, 1);
    assert_eq!(l.history()?.len(), 1);
    Ok(())
}

#[test]
fn lock_contention_is_bounded_and_interrupted_temp_write_preserves_snapshot() -> TestResult {
    use std::fs::OpenOptions;
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = observation(&f)?;
    l.record(&f.store, o.clone())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.dir.path().join("trust/ledger.lock"))?;
    lock.lock()?;
    assert!(matches!(
        l.history(),
        Err(TrustError::Storage(StateError::LockTimeout { .. }))
    ));
    drop(lock);
    let temp = f.dir.path().join("trust/ledger.tmp");
    fs::write(&temp, b"partial write")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    assert_eq!(reopen(&f)?.latest(&o.id)?, o);
    let mut next = o.clone();
    next.revision = NonZeroU32::new(2).ok_or("revision")?;
    next.correction = Some(source("fixture:refresh")?);
    l.record(&f.store, next.clone())?;
    assert_eq!(reopen(&f)?.latest(&o.id)?, next);
    assert!(!temp.exists());
    Ok(())
}

#[test]
fn independent_ledger_readers_share_the_snapshot_lock() -> TestResult {
    use std::fs::OpenOptions;
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, observation(&f)?)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.dir.path().join("trust/ledger.lock"))?;
    lock.lock_shared()?;
    assert_eq!(l.history()?.len(), 1);
    Ok(())
}

#[test]
fn invalid_history_budget_and_inspector_inputs_do_not_commit() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = observation(&f)?;
    let mut bad = o.clone();
    bad.revision = NonZeroU32::new(2).ok_or("revision")?;
    assert!(matches!(l.record(&f.store, bad), Err(TrustError::Invalid)));
    assert!(l.history()?.is_empty());
    l.record(&f.store, o.clone())?;
    assert!(matches!(
        l.start_inspection(plan()?, at(5)),
        Err(TrustError::Incomplete)
    ));
    let mut o = with_pr(o)?;
    o.revision = NonZeroU32::new(2).ok_or("revision")?;
    o.correction = Some(source("fixture:pr-attached")?);
    l.record(&f.store, o)?;
    for (samples, tokens, deadline) in [
        (0, 10, 60),
        (33, 10, 60),
        (1, 0, 60),
        (1, 1_000_001, 60),
        (1, 1, 5),
        (1, 1, 3606),
    ] {
        let mut p = plan()?;
        p.max_samples = samples;
        p.max_tokens = tokens;
        p.deadline = at(deadline);
        assert!(l.start_inspection(p, at(5)).is_err());
    }
    assert!(l.inspection(&plan()?.id).is_err());
    let mut fractional = plan()?;
    fractional.deadline = kitchen::contracts::Timestamp::from_unix_millis(3_605_001);
    assert!(l.start_inspection(fractional, at(5)).is_err());
    let mut p = plan()?;
    p.house = other_house()?;
    assert!(l.start_inspection(p, at(5)).is_err());
    l.start_inspection(plan()?, at(5))?;
    l.reserve_sample(&plan()?.id, 1, 1, at(6))?;
    l.cancel_inspection(&plan()?.id)?;
    l.finish_sample(&plan()?.id, 1, SampleResult::Unavailable)?;
    assert!(l.reserve_sample(&plan()?.id, 2, 1, at(7)).is_err());
    Ok(())
}

#[test]
fn collection_preserves_uncertain_effect_and_exact_core_evidence() -> TestResult {
    use kitchen::{
        contracts::{Evidence, EvidenceKind, EvidenceVerdict},
        state::{EffectOutcome, EffectState},
    };
    let f = Fixture::new()?;
    let mut task = spec("task")?;
    task.repository = Some(scope()?.project);
    f.store.create_task(task, &creator()?, at(0))?;
    let lease = f
        .store
        .claim(&task_id("task")?, &scheduled("owner")?, ttl(60)?, at(1))?;
    f.store
        .start_attempt(&task_id("task")?, lease.fence(), at(2))?;
    let backend = common::refusing()?;
    let started = f.store.begin_effect(
        common::plan(
            &task_id("task")?,
            lease.fence(),
            "launch",
            common::launch()?,
        )?,
        &grants()?,
        &backend,
        at(3),
    )?;
    let record = match started {
        kitchen::state::EffectStart::Execute(record) => record,
        _ => return Err("new effect expected".into()),
    };
    f.store.record_effect_outcome(
        &task_id("task")?,
        lease.fence(),
        record.seq(),
        EffectOutcome::Uncertain(kitchen::contracts::UncertainReason::Transport),
        at(4),
    )?;
    let evidence = Evidence {
        kind: EvidenceKind::Check,
        verdict: EvidenceVerdict::Unavailable,
        subject: subject()?,
        source: source("fixture:check-unavailable")?,
        observed_at: at(5),
    };
    f.store
        .record_evidence(&task_id("task")?, lease.fence(), evidence.clone(), at(5))?;
    let o = Observation::collect(
        &f.store,
        &task_id("task")?,
        source("fixture:task")?,
        attribution()?,
        EvidenceMode::Live,
        at(6),
    )?;
    assert!(matches!(
        o.effects.first().map(|effect| effect.state()),
        Some(EffectState::Uncertain { .. })
    ));
    assert_eq!(o.evidence, vec![evidence]);
    assert!(!o.trust_eligible());
    Ok(())
}

#[test]
fn unknown_authority_fields_and_dangling_persisted_evidence_are_rejected() -> TestResult {
    let mut encoded = serde_json::to_value(grant()?)?;
    encoded
        .as_object_mut()
        .ok_or("grant object")?
        .insert("expired".into(), serde_json::json!(true));
    assert!(serde_json::from_value::<AutonomyGrant>(encoded).is_err());
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = eligible(observation(&f)?)?;
    l.record(&f.store, o)?;
    bind_evidence(&l, &f)?;
    l.grant(grant()?, &grants()?)?;
    let path = f.dir.path().join("trust/ledger.json");
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    document["observations"] = serde_json::json!([]);
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert!(reopen(&f).is_err());
    Ok(())
}

#[test]
fn moved_observation_stops_inspection_and_duplicate_findings_do_not_route_twice() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = with_pr(observation(&f)?)?;
    l.record(&f.store, o.clone())?;
    let mut inspection_plan = plan()?;
    inspection_plan.max_samples = 4;
    l.start_inspection(inspection_plan, at(5))?;
    l.reserve_sample(&plan()?.id, 1, 10, at(6))?;
    let result = SampleResult::Confirmed {
        finding: kitchen::trust::Finding {
            source: source("fixture:single-finding")?,
            subject: subject()?,
            consequence: Text::new("Reproduced regression")?,
        },
        route: FollowUpRoute::Issue,
    };
    l.finish_sample(&plan()?.id, 1, result.clone())?;
    l.reserve_sample(&plan()?.id, 2, 10, at(7))?;
    assert!(l.finish_sample(&plan()?.id, 2, result).is_err());
    assert_eq!(l.inspection(&plan()?.id)?.follow_ups().count(), 1);
    o.revision = NonZeroU32::new(2).ok_or("revision")?;
    o.correction = Some(source("fixture:attribution-correction")?);
    l.record(&f.store, o.clone())?;
    assert!(l.finish_sample(&plan()?.id, 2, SampleResult::Unavailable)?);
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 2, 10, at(8))?,
        SampleReservation::Existing(_)
    ));
    assert!(l.reserve_sample(&plan()?.id, 3, 10, at(8)).is_err());
    let mut next_inspection = plan()?;
    next_inspection.id = source("fixture:inspection-after-correction")?;
    l.start_inspection(next_inspection.clone(), at(8))?;
    l.reserve_sample(&next_inspection.id, 1, 10, at(9))?;
    o.revision = NonZeroU32::new(3).ok_or("revision")?;
    o.correction = Some(source("fixture:new-head")?);
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.subject.head = commit('c')?;
    }
    l.record(&f.store, o)?;
    assert!(l.finish_sample(&next_inspection.id, 1, SampleResult::Unavailable)?);
    assert!(
        l.reserve_sample(&next_inspection.id, 2, 10, at(10))
            .is_err()
    );
    assert!(l.reserve_sample(&plan()?.id, 4, 10, at(9)).is_err());
    Ok(())
}
