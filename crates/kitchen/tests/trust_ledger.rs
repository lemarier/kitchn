//! Offline fixtures: no live usage, GitHub, model, or equipment is exercised.
mod common;
use common::{
    Fixture, TestResult, at, commit, creator, grants, holder, house, other_house, scheduled, spec,
    task_id, ttl,
};
use kitchen::{
    contracts::{
        AttemptNumber, AttemptOutcome, ContractError, Evidence, EvidenceKind, EvidenceSubject,
        EvidenceVerdict, ExternalRef, Grant, HouseGrants, Permission, Repository, Role,
        TaskAuthority, TaskSpec, Text,
    },
    state::{Corruption, HouseStore, StateError, StoreOptions},
    trust::{
        Attribution, AutonomyGrant, AutonomyProposal, BenchResult, EvidenceMode, Finding,
        GrantAudit, Ledger, Measurement, Observation, PullRequestEvidence, StationScope,
        TrustError,
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
/// Settle `name` as a succeeded worker task in the example project and collect
/// its observation. `edit` adjusts the spec before creation; `core` is recorded
/// as the task's core evidence before it settles.
fn settled(
    f: &Fixture,
    name: &str,
    stream: &str,
    edit: impl FnOnce(&mut TaskSpec),
    core: Option<Evidence>,
) -> TestResult<Observation> {
    let mut task = spec(name)?;
    task.repository = Some(scope()?.project);
    edit(&mut task);
    let id = task_id(name)?;
    f.store.create_task(task, &creator()?, at(0))?;
    let lease = f.store.claim(&id, &scheduled("owner")?, ttl(60)?, at(1))?;
    f.store.start_attempt(&id, lease.fence(), at(2))?;
    if let Some(evidence) = core {
        f.store
            .record_evidence(&id, lease.fence(), evidence, at(2))?;
    }
    f.store.finish_attempt(
        &id,
        lease.fence(),
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(3),
    )?;
    Ok(Observation::collect(
        &f.store,
        &id,
        source(stream)?,
        attribution()?,
        EvidenceMode::Simulated,
        at(4),
    )?)
}
fn observation(f: &Fixture) -> TestResult<Observation> {
    settled(f, "task", "fixture:task", |_| {}, None)
}
fn core_check(verdict: EvidenceVerdict, subject: EvidenceSubject) -> TestResult<Evidence> {
    Ok(Evidence {
        kind: EvidenceKind::Check,
        verdict,
        subject,
        source: source("fixture:core-check")?,
        observed_at: at(2),
    })
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
fn with_pr_at(mut o: Observation, subject: EvidenceSubject) -> TestResult<Observation> {
    o.pull_request = measured(PullRequestEvidence {
        house: house()?,
        task: o.task.clone(),
        repository: scope()?.project,
        source: source("https://example.invalid/pr/1")?,
        subject,
        first_pass: Measurement::Missing,
        findings: Measurement::Missing,
        reverts: Measurement::Missing,
        regressions: Measurement::Missing,
        checks: Measurement::Unavailable,
    })?;
    Ok(o)
}
fn with_pr(o: Observation) -> TestResult<Observation> {
    with_pr_at(o, subject()?)
}
/// Live evidence with every positive measurement for the PR head `at`.
fn eligible_at(o: Observation, at_head: EvidenceSubject) -> TestResult<Observation> {
    let mut o = with_pr_at(o, at_head)?;
    o.mode = EvidenceMode::Live;
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.first_pass = measured(true)?;
        value.findings = measured(Vec::new())?;
        value.reverts = measured(Vec::new())?;
        value.regressions = measured(Vec::new())?;
        value.checks = measured(vec![Evidence {
            kind: EvidenceKind::Check,
            verdict: EvidenceVerdict::Pass,
            subject: value.subject.clone(),
            source: source("fixture:passing-check")?,
            observed_at: at(4),
        }])?;
    }
    Ok(o)
}
fn eligible(o: Observation) -> TestResult<Observation> {
    eligible_at(o, subject()?)
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
fn proposal() -> TestResult<AutonomyProposal> {
    let g = grant()?;
    Ok(AutonomyProposal {
        id: g.id,
        house: g.house,
        scope: g.scope,
        claim: g.claim,
        evidence: g.evidence,
        source: source("fixture:proposal")?,
        at: at(5),
    })
}
/// Propose and approve the fixture grant under `policy`.
fn issue(l: &Ledger, policy: &kitchen::contracts::HouseGrants) -> TestResult {
    l.propose(proposal()?, policy)?;
    l.approve(
        &grant()?.id,
        holder("owner")?,
        source("fixture:decision")?,
        at(5),
        policy,
    )?;
    Ok(())
}
fn revoke(l: &Ledger) -> TestResult<bool> {
    Ok(l.revoke(
        &grant()?.id,
        holder("owner")?,
        source("fixture:revoke")?,
        at(6),
    )?)
}
fn ledger_path(f: &Fixture) -> std::path::PathBuf {
    f.dir.path().join("trust/ledger.json")
}
fn pr_mut(o: &mut Observation) -> TestResult<&mut PullRequestEvidence> {
    match &mut o.pull_request {
        Measurement::Observed { value, .. } => Ok(value),
        _ => Err("observed pull request expected".into()),
    }
}
/// A prospective task bound to the fixture station with `model`, for
/// `standing_for_task` before the task exists. `edit` changes its spec.
fn bound_acting(
    l: &Ledger,
    policy: &HouseGrants,
    name: &str,
    model: &str,
    edit: impl FnOnce(&mut TaskSpec),
) -> TestResult<TaskSpec> {
    let mut acting = spec(name)?;
    acting.repository = Some(scope()?.project);
    acting.authority = TaskAuthority::delegate(policy, [])?;
    edit(&mut acting);
    l.bind_task(
        &acting,
        scope()?,
        Text::new(model)?,
        source(&format!("fixture:{name}-binding"))?,
    )?;
    Ok(acting)
}
/// An ordinary (non-priority) write: bind one more prospective task.
fn try_bind_extra(l: &Ledger) -> TestResult<Result<bool, TrustError>> {
    let mut extra = spec("extra")?;
    extra.repository = Some(scope()?.project);
    Ok(l.bind_task(
        &extra,
        scope()?,
        Text::new("fixture-model-v1")?,
        source("fixture:extra-binding")?,
    ))
}
fn try_approve(l: &Ledger, current: &HouseGrants) -> TestResult<Result<bool, TrustError>> {
    Ok(l.approve(
        &grant()?.id,
        holder("owner")?,
        source("fixture:decision")?,
        at(6),
        current,
    ))
}
/// Ways a measurement can fail to be a positive result without being a
/// negative one.
fn absent<T>() -> [Measurement<T>; 3] {
    [
        Measurement::Missing,
        Measurement::Untested,
        Measurement::Unavailable,
    ]
}
/// [`absent`] plus a measurement that observed `negative`.
fn unproven<T>(negative: T) -> TestResult<[Measurement<T>; 4]> {
    let [missing, untested, unavailable] = absent();
    Ok([missing, untested, unavailable, measured(negative)?])
}
/// `edit` must make a copy of `base` ineligible for trust.
fn assert_blocks(
    base: &Observation,
    label: &str,
    edit: impl FnOnce(&mut Observation) -> TestResult,
) -> TestResult {
    let mut o = base.clone();
    edit(&mut o)?;
    assert!(!o.trust_eligible(), "{label} must block trust");
    Ok(())
}
fn confirmed(name: &str) -> TestResult<Finding> {
    Ok(Finding {
        source: source(name)?,
        subject: subject()?,
        consequence: Text::new("Confirmed consequence.")?,
    })
}
fn findings_of(pr: &mut PullRequestEvidence) -> &mut Measurement<Vec<Finding>> {
    &mut pr.findings
}
fn reverts_of(pr: &mut PullRequestEvidence) -> &mut Measurement<Vec<Finding>> {
    &mut pr.reverts
}
fn regressions_of(pr: &mut PullRequestEvidence) -> &mut Measurement<Vec<Finding>> {
    &mut pr.regressions
}
/// The largest snapshot an ordinary write may produce, and the extra bytes only
/// revocations may use.
const ORDINARY_LIMIT: u64 = 8 * 1024 * 1024;
const REVOCATION_RESERVE: u64 = 4096 * 1024;
const MAX_TEXT: usize = 64 * 1024;

/// A valid observation of another task that carries `bytes` of bench text.
fn bulky(template: &Observation, index: usize, bytes: usize) -> TestResult<serde_json::Value> {
    let mut o = template.clone();
    o.id = source(&format!("fixture:bulk-{index}"))?;
    o.task = task_id(&format!("bulk-{index}"))?;
    pr_mut(&mut o)?.task = o.task.clone();
    o.bench = measured(vec![BenchResult {
        subject: subject()?,
        passed: true,
        procedure: Text::new(&"b".repeat(bytes))?,
    }])?;
    Ok(serde_json::to_value(o)?)
}
/// Grow the persisted ledger to exactly `target` bytes by appending valid
/// observations with bulk bench text. Every entry passes the ledger's own
/// validation when it next loads the file.
fn fill_ledger(l: &Ledger, f: &Fixture, target: u64) -> TestResult {
    let template = l.history()?.into_iter().next().ok_or("an observation")?;
    let path = ledger_path(f);
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let mut size = u64::try_from(serde_json::to_vec(&document)?.len())?;
    let entries = document["observations"]
        .as_array_mut()
        .ok_or("observations")?;
    for index in 0.. {
        let missing = target
            .checked_sub(size)
            .ok_or("ledger already over target")?;
        // Bytes an entry adds with one byte of text, plus its separator.
        let fixed = u64::try_from(serde_json::to_vec(&bulky(&template, index, 1)?)?.len())? + 1;
        let room = (missing + 1)
            .checked_sub(fixed)
            .ok_or("target too close to pad")?;
        let last = room <= u64::try_from(MAX_TEXT)?;
        let text = if last { usize::try_from(room)? } else { 60_000 };
        entries.push(bulky(&template, index, text)?);
        size += fixed - 1 + u64::try_from(text)?;
        if last {
            break;
        }
    }
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert_eq!(fs::metadata(&path)?.len(), target);
    Ok(())
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
    assert!(matches!(
        Observation::collect(
            &f.store,
            &task_id("task")?,
            source("fixture:wrong")?,
            wrong,
            EvidenceMode::Live,
            at(5)
        ),
        Err(kitchen::Error::Trust(TrustError::Refused))
    ));
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
fn untested_bench_is_preserved_and_cross_house_inputs_are_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = with_pr(observation(&f)?)?;
    o.bench = Measurement::Untested;
    l.record(&f.store, o.clone())?;
    assert_eq!(l.latest(&o.id)?.bench, Measurement::Untested);
    let mut other = o.clone();
    other.house = other_house()?;
    other.id = source("fixture:other")?;
    assert!(matches!(
        l.record(&f.store, other),
        Err(TrustError::Refused)
    ));
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), other_house()?),
        Err(TrustError::Authority(ContractError::CrossHouse { .. }))
    ));
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.house = other_house()?;
    }
    assert!(matches!(l.record(&f.store, o), Err(TrustError::Refused)));
    assert_eq!(l.history()?.len(), 1);
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
    bind_evidence(&l, &f)?;
    l.record(&f.store, o.clone())?;
    assert!(matches!(
        l.propose(proposal()?, &grants()?),
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
fn trust_requires_every_positive_measurement() -> TestResult {
    use kitchen::{contracts::Settlement, state::TaskState};
    type FindingsField = fn(&mut PullRequestEvidence) -> &mut Measurement<Vec<Finding>>;
    let f = Fixture::new()?;
    let ok = eligible(observation(&f)?)?;
    assert!(ok.trust_eligible());

    assert_blocks(&ok, "simulated evidence", |o| {
        o.mode = EvidenceMode::Simulated;
        Ok(())
    })?;
    for agent in absent() {
        assert_blocks(&ok, "unknown agent", |o| {
            o.attribution.agent = agent;
            Ok(())
        })?;
    }
    for model in absent() {
        assert_blocks(&ok, "unknown model", |o| {
            o.attribution.model = model;
            Ok(())
        })?;
    }
    for tokens in unproven(0)? {
        assert_blocks(&ok, "unknown or zero token use", |o| {
            o.attribution.tokens = tokens;
            Ok(())
        })?;
    }
    for settlement in [
        Settlement::Failed,
        Settlement::Cancelled,
        Settlement::Exhausted,
    ] {
        assert_blocks(&ok, "settlement other than success", |o| {
            if let TaskState::Settled {
                settlement: value, ..
            } = &mut o.state
            {
                *value = settlement;
            }
            Ok(())
        })?;
    }
    for pull_request in absent() {
        assert_blocks(&ok, "no pull request evidence", |o| {
            o.pull_request = pull_request;
            Ok(())
        })?;
    }
    for first_pass in unproven(false)? {
        assert_blocks(&ok, "first-pass acceptance", |o| {
            pr_mut(o)?.first_pass = first_pass;
            Ok(())
        })?;
    }
    let lists: [(&str, FindingsField); 3] = [
        ("findings", findings_of),
        ("reverts", reverts_of),
        ("regressions", regressions_of),
    ];
    for (label, field) in lists {
        for measurement in unproven(vec![confirmed("fixture:finding")?])? {
            assert_blocks(&ok, label, |o| {
                *field(pr_mut(o)?) = measurement;
                Ok(())
            })?;
        }
    }
    for checks in unproven(Vec::<Evidence>::new())? {
        assert_blocks(&ok, "required checks", |o| {
            pr_mut(o)?.checks = checks;
            Ok(())
        })?;
    }
    for verdict in [EvidenceVerdict::Fail, EvidenceVerdict::Unavailable] {
        assert_blocks(&ok, "a check that is not a pass", |o| {
            if let Measurement::Observed { value: checks, .. } = &mut pr_mut(o)?.checks {
                let mut extra = checks[0].clone();
                extra.verdict = verdict;
                extra.source = source("fixture:second-check")?;
                checks.push(extra);
            }
            Ok(())
        })?;
    }
    assert_blocks(&ok, "a failing bench result", |o| {
        o.bench = measured(vec![
            BenchResult {
                subject: subject()?,
                passed: true,
                procedure: Text::new("bench fixture")?,
            },
            BenchResult {
                subject: subject()?,
                passed: false,
                procedure: Text::new("power interruption test")?,
            },
        ])?;
        Ok(())
    })?;
    assert_blocks(&ok, "an inappropriate escalation", |o| {
        o.appropriate_escalation = measured(false)?;
        Ok(())
    })?;
    let other_head = EvidenceSubject {
        head: commit('c')?,
        base: subject()?.base,
    };
    for (label, item) in [
        (
            "a failed core check",
            core_check(EvidenceVerdict::Fail, subject()?)?,
        ),
        (
            "an unavailable core check",
            core_check(EvidenceVerdict::Unavailable, subject()?)?,
        ),
        (
            "core evidence for another head",
            core_check(EvidenceVerdict::Pass, other_head)?,
        ),
    ] {
        assert_blocks(&ok, label, |o| {
            o.evidence.push(item);
            Ok(())
        })?;
    }

    // Recorded positive results and an unrecorded bench keep the record eligible.
    let mut positive = ok.clone();
    positive.bench = measured(vec![BenchResult {
        subject: subject()?,
        passed: true,
        procedure: Text::new("bench fixture")?,
    }])?;
    positive.appropriate_escalation = measured(true)?;
    positive
        .evidence
        .push(core_check(EvidenceVerdict::Pass, subject()?)?);
    assert!(positive.trust_eligible());
    assert_eq!(ok.bench, Measurement::Missing);
    Ok(())
}

#[test]
fn fabricated_core_outcome_and_scope_correction_are_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = eligible(observation(&f)?)?;
    let original = o.clone();
    o.task = task_id("absent")?;
    assert!(matches!(l.record(&f.store, o), Err(TrustError::Refused)));
    let mut o = original.clone();
    o.effects.clear();
    o.state = kitchen::state::TaskState::Settled {
        settlement: kitchen::contracts::Settlement::Failed,
        at: at(10),
    };
    assert!(matches!(l.record(&f.store, o), Err(TrustError::Refused)));
    l.record(&f.store, original.clone())?;
    let mut corrected = original;
    corrected.revision = NonZeroU32::new(2).ok_or("revision")?;
    corrected.correction = Some(source("fixture:scope-correction")?);
    corrected.attribution.scope.station = Text::new("other-station")?;
    assert!(matches!(
        l.record(&f.store, corrected),
        Err(TrustError::Refused)
    ));
    assert_eq!(l.history()?.len(), 1);
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
fn record_refuses_a_task_binding_that_differs_from_the_stored_task() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = observation(&f)?;
    // The adapter bound a prospective spec whose role is not the stored task's.
    let mut prospective = f.store.task(&task_id("task")?)?.spec().clone();
    prospective.role = Role::Commis;
    l.bind_task(
        &prospective,
        scope()?,
        Text::new("fixture-model-v1")?,
        source("fixture:binding")?,
    )?;
    assert!(matches!(l.record(&f.store, o), Err(TrustError::Refused)));
    assert!(l.history()?.is_empty());
    let second = settled(&f, "second", "fixture:second", |_| {}, None)?;
    let exact = f.store.task(&task_id("second")?)?.spec().clone();
    l.bind_task(
        &exact,
        scope()?,
        Text::new("fixture-model-v1")?,
        source("fixture:second-binding")?,
    )?;
    assert!(l.record(&f.store, second)?);
    Ok(())
}

#[test]
fn pr_evidence_must_describe_the_head_core_recorded_and_agree_with_core_checks() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let head = subject()?;
    let core = core_check(EvidenceVerdict::Pass, head.clone())?;
    let recorded = settled(&f, "task", "fixture:task", |_| {}, Some(core))?;
    for moved in [
        EvidenceSubject {
            head: commit('c')?,
            base: head.base.clone(),
        },
        EvidenceSubject {
            head: head.head.clone(),
            base: Some(commit('c')?),
        },
        EvidenceSubject {
            head: head.head.clone(),
            base: None,
        },
    ] {
        assert!(matches!(
            l.record(&f.store, eligible_at(recorded.clone(), moved)?),
            Err(TrustError::Refused)
        ));
    }
    assert!(l.history()?.is_empty());
    assert!(l.record(&f.store, eligible_at(recorded.clone(), head.clone())?)?);
    assert!(l.latest(&recorded.id)?.trust_eligible());
    // Where core holds no evidence, the adapter's PR evidence is the only source.
    let bare = settled(&f, "bare", "fixture:bare", |_| {}, None)?;
    let elsewhere = EvidenceSubject {
        head: commit('c')?,
        base: None,
    };
    assert!(l.record(&f.store, eligible_at(bare, elsewhere)?)?);
    // A core check that is not a pass keeps the record out of trust even when
    // the adapter reports passing checks for the same head.
    for (name, verdict) in [
        ("failed-check", EvidenceVerdict::Fail),
        ("unavailable-check", EvidenceVerdict::Unavailable),
    ] {
        let core = core_check(verdict, head.clone())?;
        let stream = format!("fixture:{name}");
        let o = eligible_at(
            settled(&f, name, &stream, |_| {}, Some(core))?,
            head.clone(),
        )?;
        assert!(!o.trust_eligible());
        assert!(l.record(&f.store, o.clone())?);
        assert!(!l.latest(&o.id)?.trust_eligible());
    }
    Ok(())
}

#[test]
fn core_store_faults_are_storage_errors_and_an_absent_task_is_a_refusal() -> TestResult {
    use std::{fs::OpenOptions, time::Duration};
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = observation(&f)?;
    let task_spec = f.store.task(&task_id("task")?)?.spec().clone();
    l.bind_task(
        &task_spec,
        scope()?,
        Text::new("fixture-model-v1")?,
        source("fixture:binding")?,
    )?;
    let quick = HouseStore::open(
        f.dir.path().join("house"),
        house()?,
        StoreOptions {
            lock_timeout: Duration::from_millis(30),
            ..StoreOptions::default()
        },
    )?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.dir.path().join("house/state.lock"))?;
    lock.lock()?;
    assert!(matches!(
        l.record(&quick, o.clone()),
        Err(TrustError::Storage(StateError::LockTimeout { .. }))
    ));
    assert!(matches!(
        l.standing_for_task(&quick, &task_spec, &grants()?),
        Err(TrustError::Storage(StateError::LockTimeout { .. }))
    ));
    drop(lock);
    let mut absent = o.clone();
    absent.task = task_id("absent")?;
    assert!(matches!(l.record(&quick, absent), Err(TrustError::Refused)));
    assert!(l.history()?.is_empty());
    assert!(l.record(&quick, o)?);
    Ok(())
}

#[test]
fn revocation_succeeds_at_history_capacity_and_stays_revoked() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
    let path = f.dir.path().join("trust/ledger.json");
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let audits = document["grants"].as_array_mut().ok_or("grants")?;
    let original = audits.first().cloned().ok_or("grant")?;
    for index in 1..4094 {
        let mut additional = original.clone();
        let id = serde_json::json!(format!("fixture:grant-{index}"));
        additional["proposal"]["id"] = id.clone();
        additional["id"] = id;
        audits.push(additional);
    }
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert!(revoke(&l)?);
    assert!(!revoke(&reopen(&f)?)?);
    assert!(matches!(
        l.propose(proposal()?, &grants()?),
        Err(TrustError::Conflict)
    ));
    assert_eq!(l.grant_history()?.len(), 4094);
    Ok(())
}

#[test]
fn readers_yield_to_a_pending_revocation() -> TestResult {
    use std::{fs::OpenOptions, sync::mpsc, thread, time::Duration};
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.dir.path().join("trust/ledger.lock"))?;
    // An in-flight reader holds the shared lock, so the revocation cannot start.
    // A new reader would normally join it and finish; the pending revocation
    // must hold that reader back.
    lock.lock_shared()?;
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
    let (ready, started) = mpsc::channel();
    let reader = l.clone();
    let read = thread::spawn(move || {
        // A send fails only if the test already ended.
        let _ = ready.send(());
        reader.grant_history()
    });
    started.recv_timeout(Duration::from_secs(1))?;
    thread::sleep(Duration::from_millis(50));
    assert!(
        !read.is_finished(),
        "a reader must not slip past a pending revocation"
    );
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
    assert!(l.propose(proposal()?, &policy)?);
    assert!(!l.propose(proposal()?, &policy)?);
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
    l.propose(proposal()?, &policy)?;
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
    assert!(matches!(
        acting.authority.authorize(
            &after,
            g.claim.permission,
            &g.claim.scope,
            &g.claim.destination
        ),
        Err(ContractError::AuthorityExpansion { .. })
    ));
    assert_eq!(l.grant_history()?.len(), 1);
    Ok(())
}

#[test]
fn unknown_revocation_and_privileged_proposals_are_refused() -> TestResult {
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
    // Only the earned-autonomy allowlist is proposable, whatever the policy says.
    let excluded = [
        Permission::ReleaseResource,
        Permission::CloseIssue,
        Permission::PushBranch,
        Permission::OpenPullRequest,
        Permission::Merge,
        Permission::ManageSchedule,
        Permission::ActivateSchedule,
        Permission::TrialSchedule,
        Permission::Publish,
        Permission::OperateEquipment,
    ];
    for permission in Permission::ALL {
        let mut p = proposal()?;
        p.id = source(&format!("fixture:proposal-{}", permission.as_str()))?;
        p.claim.permission = permission;
        let policy = kitchen::contracts::HouseGrants::new(house()?, [p.claim.clone()]);
        if excluded.contains(&permission) {
            assert!(
                matches!(l.propose(p, &policy), Err(TrustError::Refused)),
                "{permission:?} must not be proposable"
            );
        } else {
            assert!(l.propose(p, &policy)?, "{permission:?} is on the allowlist");
        }
    }
    assert_eq!(
        l.grant_history()?.len(),
        Permission::ALL.len() - excluded.len()
    );
    let mut foreign = proposal()?;
    foreign.house = other_house()?;
    assert!(matches!(
        l.propose(foreign, &grants()?),
        Err(TrustError::Refused)
    ));
    Ok(())
}

#[test]
fn replayed_proposal_and_approval_are_no_ops_and_conflicts_are_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let policy = grants()?;
    let p = proposal()?;
    let owner = holder("owner")?;
    let decision = source("fixture:decision")?;
    assert!(l.propose(p.clone(), &policy)?);
    assert!(l.approve(&p.id, owner.clone(), decision.clone(), at(5), &policy)?);
    let approved = l.grant_history()?;
    // A retry after an uncertain outcome: same approver and decision, later clock.
    assert!(!l.approve(&p.id, owner.clone(), decision.clone(), at(9), &policy)?);
    assert!(!l.propose(p.clone(), &policy)?);
    assert_eq!(l.grant_history()?, approved);
    // A different decision, approver, or proposal body is a conflict, not a replay.
    assert!(matches!(
        l.approve(
            &p.id,
            owner.clone(),
            source("fixture:other-decision")?,
            at(9),
            &policy
        ),
        Err(TrustError::Conflict)
    ));
    assert!(matches!(
        l.approve(
            &p.id,
            holder("other-owner")?,
            decision.clone(),
            at(9),
            &policy
        ),
        Err(TrustError::Conflict)
    ));
    let mut changed = p.clone();
    changed.source = source("fixture:other-proposal")?;
    assert!(matches!(
        l.propose(changed, &policy),
        Err(TrustError::Conflict)
    ));
    assert_eq!(l.grant_history()?, approved);
    // Once revoked, the approval is no longer the current state.
    assert!(revoke(&l)?);
    assert!(matches!(
        l.approve(&p.id, owner.clone(), decision.clone(), at(9), &policy),
        Err(TrustError::Conflict)
    ));
    assert!(matches!(l.propose(p, &policy), Err(TrustError::Conflict)));
    assert!(matches!(
        l.approve(&source("fixture:unknown")?, owner, decision, at(9), &policy),
        Err(TrustError::NotFound)
    ));
    Ok(())
}

#[test]
fn corrected_grant_evidence_requires_new_explicit_approval() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut o = eligible(observation(&f)?)?;
    l.record(&f.store, o.clone())?;
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
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
    assert!(matches!(
        l.reserve_sample(&p.id, 0, 1, at(6)),
        Err(TrustError::Invalid)
    ));
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
    assert!(matches!(
        l.finish_sample(&plan()?.id, 1, stale),
        Err(TrustError::Refused)
    ));
    assert!(l.finish_sample(&plan()?.id, 1, result.clone())?);
    assert!(!l.finish_sample(&plan()?.id, 1, result)?);
    assert!(matches!(
        l.finish_sample(&plan()?.id, 1, SampleResult::Unavailable),
        Err(TrustError::Conflict)
    ));
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
    assert!(
        serde_json::from_str::<Observation>(&invalid).is_err_and(|error| error.is_data()),
        "a zero sample count is a data error"
    );
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
        assert!(
            matches!(l.start_inspection(p, at(5)), Err(TrustError::Invalid)),
            "bounds {samples}/{tokens}/{deadline} must be invalid"
        );
    }
    assert!(matches!(
        l.inspection(&plan()?.id),
        Err(TrustError::Incomplete)
    ));
    let mut fractional = plan()?;
    fractional.deadline = kitchen::contracts::Timestamp::from_unix_millis(3_605_001);
    assert!(matches!(
        l.start_inspection(fractional, at(5)),
        Err(TrustError::Invalid)
    ));
    let mut p = plan()?;
    p.house = other_house()?;
    assert!(matches!(
        l.start_inspection(p, at(5)),
        Err(TrustError::Refused)
    ));
    l.start_inspection(plan()?, at(5))?;
    l.reserve_sample(&plan()?.id, 1, 1, at(6))?;
    l.cancel_inspection(&plan()?.id)?;
    l.finish_sample(&plan()?.id, 1, SampleResult::Unavailable)?;
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 2, 1, at(7)),
        Err(TrustError::Refused)
    ));
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
    let unknown = serde_json::from_value::<AutonomyGrant>(encoded)
        .err()
        .ok_or("an unknown field must be rejected")?;
    assert!(unknown.to_string().contains("expired"));
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = eligible(observation(&f)?)?;
    l.record(&f.store, o)?;
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
    let path = f.dir.path().join("trust/ledger.json");
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    document["observations"] = serde_json::json!([]);
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Corrupt)
    ));
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
    assert!(matches!(
        l.finish_sample(&plan()?.id, 2, result),
        Err(TrustError::Refused)
    ));
    assert_eq!(l.inspection(&plan()?.id)?.follow_ups().count(), 1);
    o.revision = NonZeroU32::new(2).ok_or("revision")?;
    o.correction = Some(source("fixture:attribution-correction")?);
    l.record(&f.store, o.clone())?;
    assert!(l.finish_sample(&plan()?.id, 2, SampleResult::Unavailable)?);
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 2, 10, at(8))?,
        SampleReservation::Existing(_)
    ));
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 3, 10, at(8)),
        Err(TrustError::Refused)
    ));
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
    assert!(matches!(
        l.reserve_sample(&next_inspection.id, 2, 10, at(10)),
        Err(TrustError::Refused)
    ));
    assert!(matches!(
        l.reserve_sample(&plan()?.id, 4, 10, at(9)),
        Err(TrustError::Refused)
    ));
    Ok(())
}

#[test]
fn record_refuses_a_model_or_scope_that_differs_from_the_task_binding() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = eligible(observation(&f)?)?;
    bind_evidence(&l, &f)?;
    let mut wrong_model = o.clone();
    wrong_model.attribution.model = measured(Text::new("other-model-v2")?)?;
    let mut wrong_station = o.clone();
    wrong_station.attribution.scope.station = Text::new("python")?;
    let mut wrong_work_type = o.clone();
    wrong_work_type.attribution.scope.work_type = Text::new("review")?;
    for (label, bad) in [
        ("model", wrong_model),
        ("station", wrong_station),
        ("work type", wrong_work_type),
    ] {
        assert!(
            matches!(l.record(&f.store, bad), Err(TrustError::Refused)),
            "a different {label} must be refused"
        );
    }
    assert!(l.history()?.is_empty());
    assert!(l.record(&f.store, o)?);
    Ok(())
}

#[test]
fn earned_standing_requires_the_evidence_tasks_role_pins_and_bound_model() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let claim = grant()?.claim;
    let policy = HouseGrants::with_limits(house()?, [claim.clone()], [])?;
    issue(&l, &policy)?;
    let earned = |acting: &TaskSpec| -> TestResult<bool> {
        Ok(l.standing_for_task(&f.store, acting, &policy)?
            .covers(&claim))
    };

    let same = bound_acting(&l, &policy, "same", "fixture-model-v1", |_| {})?;
    assert!(
        earned(&same)?,
        "the evidence task's own pins earn the grant"
    );

    let pin = commit('c')?;
    let repinned = [
        bound_acting(&l, &policy, "kitchen-pin", "fixture-model-v1", |s| {
            s.provenance.kitchen = pin.clone();
        })?,
        bound_acting(&l, &policy, "guidance-pin", "fixture-model-v1", |s| {
            s.provenance.house_guidance = pin.clone();
        })?,
        bound_acting(&l, &policy, "repository-pin", "fixture-model-v1", |s| {
            s.provenance.repository_instructions = Some(pin.clone());
        })?,
    ];
    for acting in &repinned {
        assert!(!earned(acting)?, "{} must not inherit standing", acting.id);
    }
    let remodelled = bound_acting(&l, &policy, "remodelled", "fixture-model-v2", |_| {})?;
    assert!(!earned(&remodelled)?);
    let other_role = bound_acting(&l, &policy, "other-role", "fixture-model-v1", |s| {
        s.role = Role::Commis;
    })?;
    assert!(!earned(&other_role)?);
    Ok(())
}

#[test]
fn proposals_refuse_evidence_from_a_task_without_a_binding() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    assert!(matches!(
        l.propose(proposal()?, &grants()?),
        Err(TrustError::Refused)
    ));
    assert!(l.grant_history()?.is_empty());
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
    // A stored grant whose evidence task has lost its binding is corrupt.
    let path = ledger_path(&f);
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    document["bindings"] = serde_json::json!([]);
    fs::write(&path, serde_json::to_vec(&document)?)?;
    assert!(matches!(
        Ledger::open(f.dir.path().join("trust"), house()?),
        Err(TrustError::Corrupt)
    ));
    Ok(())
}

#[test]
fn approval_is_refused_for_withdrawn_limits_changed_credentials_and_stale_evidence() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let o = eligible(observation(&f)?)?;
    l.record(&f.store, o.clone())?;
    bind_evidence(&l, &f)?;
    let claim = grant()?.claim;
    let policy = HouseGrants::with_limits(house()?, [claim.clone()], [])?;
    l.propose(proposal()?, &policy)?;

    let withdrawn = HouseGrants::new(house()?, []);
    assert!(matches!(
        try_approve(&l, &withdrawn)?,
        Err(TrustError::Authority(
            ContractError::AuthorityExpansion { .. }
        ))
    ));
    let other_credential = Grant::repository(
        claim.permission,
        scope()?.project,
        common::backend_id()?,
        kitchen::CredentialId::new("other-credential")?,
    );
    let rekeyed = HouseGrants::with_limits(house()?, [other_credential], [])?;
    assert!(matches!(
        try_approve(&l, &rekeyed)?,
        Err(TrustError::Refused)
    ));
    let mut corrected = o;
    corrected.revision = NonZeroU32::new(2).ok_or("revision")?;
    corrected.correction = Some(source("fixture:correction")?);
    l.record(&f.store, corrected)?;
    assert!(matches!(
        try_approve(&l, &policy)?,
        Err(TrustError::Refused)
    ));
    assert!(matches!(
        l.grant_history()?.as_slice(),
        [GrantAudit::Proposed(_)]
    ));
    Ok(())
}

#[test]
fn a_revoked_proposal_never_becomes_authority() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let claim = grant()?.claim;
    let policy = HouseGrants::with_limits(house()?, [claim.clone()], [])?;
    l.propose(proposal()?, &policy)?;
    assert!(revoke(&l)?);
    assert!(!revoke(&reopen(&f)?)?);
    assert!(matches!(
        reopen(&f)?.grant_history()?.as_slice(),
        [GrantAudit::RevokedProposal { .. }]
    ));
    assert!(matches!(
        try_approve(&l, &policy)?,
        Err(TrustError::Conflict)
    ));
    let acting = bound_acting(&l, &policy, "acting", "fixture-model-v1", |_| {})?;
    assert!(
        !l.standing_for_task(&f.store, &acting, &policy)?
            .covers(&claim)
    );
    Ok(())
}

#[test]
fn revocation_uses_the_byte_reserve_when_ordinary_writes_are_full() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
    // Eight bytes under the ordinary bound: less room than any revocation needs.
    fill_ledger(&l, &f, ORDINARY_LIMIT - 8)?;
    let path = ledger_path(&f);
    let full = fs::read(&path)?;
    assert!(matches!(
        try_bind_extra(&l)?,
        Err(TrustError::Storage(StateError::StateTooLarge {
            limit_bytes: ORDINARY_LIMIT
        }))
    ));
    assert_eq!(
        fs::read(&path)?,
        full,
        "a refused write leaves the file alone"
    );

    assert!(revoke(&l)?);
    let revoked = fs::metadata(&path)?.len();
    assert!(
        revoked > ORDINARY_LIMIT && revoked <= ORDINARY_LIMIT + REVOCATION_RESERVE,
        "revoked ledger is {revoked} bytes"
    );
    assert!(matches!(
        reopen(&f)?.grant_history()?.as_slice(),
        [GrantAudit::Revoked { .. }]
    ));
    // The reserve stays closed to ordinary writers.
    assert!(matches!(
        try_bind_extra(&l)?,
        Err(TrustError::Storage(StateError::StateTooLarge {
            limit_bytes: ORDINARY_LIMIT
        }))
    ));
    Ok(())
}

#[test]
fn revocation_growth_stays_inside_the_per_grant_reserve() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let policy = grants()?;
    issue(&l, &policy)?;
    let mut pending = proposal()?;
    pending.id = source("fixture:second-proposal")?;
    l.propose(pending.clone(), &policy)?;
    // The most a revocation can add: a 256-byte decision whose every byte the
    // JSON encoding escapes, a 64-byte holder, and the widest timestamp.
    let decision = source(&"\"".repeat(256))?;
    let holder_max = holder(&"h".repeat(64))?;
    let path = ledger_path(&f);
    for id in [grant()?.id, pending.id] {
        let before = fs::metadata(&path)?.len();
        assert!(l.revoke(
            &id,
            holder_max.clone(),
            decision.clone(),
            kitchen::contracts::Timestamp::from_unix_millis(u64::MAX),
        )?);
        let growth = fs::metadata(&path)?.len() - before;
        // 1 KiB is each entry's share of the ledger's revocation reserve
        // (`MAX_HISTORY` entries share `REVOCATION_RESERVE`).
        assert!(growth <= 1024, "a revocation added {growth} bytes");
        assert!(growth > 512 + 64, "the worst-case fields were not written");
    }
    Ok(())
}

#[test]
fn persisted_records_follow_core_serialization_conventions() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    issue(&l, &grants()?)?;
    let document: serde_json::Value = serde_json::from_slice(&fs::read(ledger_path(&f))?)?;
    let observation = &document["observations"][0];
    assert_eq!(observation["mode"], "live");
    assert_eq!(observation["pullRequest"]["type"], "observed");
    assert_eq!(
        observation["pullRequest"]["value"]["firstPass"]["type"],
        "observed"
    );
    assert_eq!(observation["bench"]["type"], "missing");
    assert_eq!(
        observation["attribution"]["scope"]["workType"],
        "implementation"
    );
    assert!(observation.get("pull_request").is_none());
    let grant = &document["grants"][0];
    assert_eq!(grant["type"], "issued");
    assert_eq!(grant["approvedBy"], "owner");

    // Tagged records reject unknown fields and unknown kinds.
    let mut extra = serde_json::to_value(measured(true)?)?;
    extra["extra"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<Measurement<bool>>(extra).is_err_and(|error| error.is_data()),
        "an unknown field is a data error"
    );
    let unknown = serde_json::json!({ "type": "estimated" });
    assert!(
        serde_json::from_value::<Measurement<bool>>(unknown).is_err_and(|error| error.is_data()),
        "an unknown kind is a data error"
    );
    let round_trip: Measurement<bool> =
        serde_json::from_value(serde_json::to_value(Measurement::<bool>::Untested)?)?;
    assert_eq!(round_trip, Measurement::Untested);
    Ok(())
}

#[test]
fn proposal_claims_must_be_repository_scoped_with_distinct_evidence() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, eligible(observation(&f)?)?)?;
    bind_evidence(&l, &f)?;
    let policy = grants()?;
    let evidence = proposal()?.evidence;
    let entry = evidence.first().cloned().ok_or("evidence entry")?;

    let mut none = proposal()?;
    none.evidence = Vec::new();
    assert!(matches!(l.propose(none, &policy), Err(TrustError::Refused)));
    let mut repeated = proposal()?;
    repeated.evidence = vec![entry.clone(), entry.clone()];
    assert!(matches!(
        l.propose(repeated, &policy),
        Err(TrustError::Invalid)
    ));
    let mut house_wide = proposal()?;
    house_wide.claim = Grant::house(
        Permission::LaunchWorker,
        common::backend_id()?,
        common::credential()?,
    );
    assert!(matches!(
        l.propose(house_wide, &policy),
        Err(TrustError::Refused)
    ));
    let mut elsewhere = proposal()?;
    elsewhere.claim = Grant::repository(
        Permission::LaunchWorker,
        Repository::new("other/project")?,
        common::backend_id()?,
        common::credential()?,
    );
    assert!(matches!(
        l.propose(elsewhere, &policy),
        Err(TrustError::Refused)
    ));
    let mut oversized = proposal()?;
    oversized.evidence = (1..=129)
        .map(|index| Ok((source(&format!("fixture:stream-{index}"))?, entry.1)))
        .collect::<TestResult<Vec<_>>>()?;
    assert!(matches!(
        l.propose(oversized, &policy),
        Err(TrustError::Refused)
    ));
    assert!(l.grant_history()?.is_empty());
    assert!(l.propose(proposal()?, &policy)?);
    Ok(())
}

#[test]
fn sample_count_and_token_budgets_bound_independently() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, with_pr(observation(&f)?)?)?;
    let mut by_count = plan()?;
    by_count.max_samples = 1;
    by_count.max_tokens = 100;
    l.start_inspection(by_count.clone(), at(5))?;
    l.reserve_sample(&by_count.id, 1, 10, at(6))?;
    l.finish_sample(&by_count.id, 1, SampleResult::Unavailable)?;
    assert!(matches!(
        l.reserve_sample(&by_count.id, 2, 10, at(7)),
        Err(TrustError::Exhausted)
    ));
    let mut by_tokens = plan()?;
    by_tokens.id = source("fixture:token-bound")?;
    by_tokens.max_samples = 4;
    by_tokens.max_tokens = 25;
    l.start_inspection(by_tokens.clone(), at(5))?;
    for number in 1..=2 {
        l.reserve_sample(&by_tokens.id, number, 10, at(6))?;
        l.finish_sample(&by_tokens.id, number, SampleResult::Unavailable)?;
    }
    assert!(matches!(
        l.reserve_sample(&by_tokens.id, 3, 10, at(7)),
        Err(TrustError::Exhausted)
    ));
    assert!(matches!(
        l.reserve_sample(&by_tokens.id, 4, 5, at(7)),
        Err(TrustError::Invalid)
    ));
    assert!(matches!(
        l.reserve_sample(&by_tokens.id, 3, 5, at(7))?,
        SampleReservation::Reserved(_)
    ));
    Ok(())
}

#[test]
fn persisted_inspections_with_broken_sample_sequences_are_invalid() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    l.record(&f.store, with_pr(observation(&f)?)?)?;
    l.start_inspection(plan()?, at(5))?;
    l.reserve_sample(&plan()?.id, 1, 1, at(6))?;
    let path = ledger_path(&f);
    let original = fs::read(&path)?;
    let owner = house()?;
    let open = || Ledger::open(f.dir.path().join("trust"), owner.clone());

    let mut out_of_sequence: serde_json::Value = serde_json::from_slice(&original)?;
    out_of_sequence["inspections"][0]["samples"][0]["number"] = serde_json::json!(2);
    fs::write(&path, serde_json::to_vec(&out_of_sequence)?)?;
    assert!(matches!(open(), Err(TrustError::Invalid)));

    // Three samples exceed a plan that allows two.
    let mut too_many: serde_json::Value = serde_json::from_slice(&original)?;
    let first = too_many["inspections"][0]["samples"][0].clone();
    for number in [2, 3] {
        let mut sample = first.clone();
        sample["number"] = serde_json::json!(number);
        too_many["inspections"][0]["samples"]
            .as_array_mut()
            .ok_or("samples")?
            .push(sample);
    }
    fs::write(&path, serde_json::to_vec(&too_many)?)?;
    assert!(matches!(open(), Err(TrustError::Invalid)));

    fs::write(&path, &original)?;
    assert_eq!(open()?.inspection(&plan()?.id)?.samples().len(), 1);
    Ok(())
}
