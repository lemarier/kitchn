//! Graduation policy, eligibility, and owner decisions over the trust ledger.
//! Offline fixtures only: no live runtime, forge, model, or schedule is
//! exercised. "Live" below is the evidence mode the fixture declares.
use crate::common;
use common::{
    Fixture, TestResult, at, backend_id, commit, creator, credential, holder, house, interactive,
    other_house, scheduled, task_id, ttl,
};
use kitchen::{
    TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, Claimant, EvidenceSubject, ExternalRef, Grant, HouseGrants,
        Permission, Repository, ResourceKind, ResourceRef, Role, ScheduleEffect, TaskAuthority,
        TaskSpec, Text, Timestamp,
    },
    house::{HouseConfig, HouseError},
    scheduling::{AgentFamily, ScheduleState},
    selection::{AgentModel, AgentSelection, ResolvedSelection, WorkType},
    state::{HouseStore, StoreOptions},
    trust::{
        Attribution, Eligibility, EvidenceMode, ExclusionReason, Finding, GraduationAudit,
        GraduationDecision, GraduationPolicy, GuidanceChange, Ledger, Measurement, Observation,
        PullRequestEvidence, RegressionResponse, StationScope, TrustError, test_hooks,
    },
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU16, NonZeroU32},
    rc::Rc,
};

const DAY: u64 = 86_400;
const NOW: u64 = 20 * DAY;
const MODEL: &str = "claude:fixture-model-v1";

fn source(value: &str) -> TestResult<ExternalRef> {
    Ok(ExternalRef::new(value)?)
}
fn project() -> TestResult<Repository> {
    Ok(Repository::new("example/project")?)
}
fn scope() -> TestResult<StationScope> {
    Ok(StationScope {
        station: Role::StationCook,
        project: project()?,
        work_type: WorkType::new("implementation")?,
    })
}
fn measured<T>(value: T) -> TestResult<Measurement<T>> {
    Ok(Measurement::Observed {
        value,
        samples: NonZeroU32::MIN,
        source: source("fixture:measurement")?,
    })
}
fn policy(runs: u32, percent: u8) -> TestResult<GraduationPolicy> {
    Ok(GraduationPolicy {
        min_supervised_runs: NonZeroU32::new(runs).ok_or("zero runs")?,
        min_first_pass_percent: percent,
        window_days: NonZeroU16::new(7).ok_or("zero window")?,
        on_guidance_change: GuidanceChange::Reset,
        on_regression: RegressionResponse::PauseSchedule,
    })
}
fn repo_grant(permission: Permission) -> TestResult<Grant> {
    Ok(Grant::repository(
        permission,
        project()?,
        backend_id()?,
        credential()?,
    ))
}
/// Interactive limits allow launching workers, requesting review, and merging
/// in the project; nothing is a standing grant.
fn config(policy: GraduationPolicy) -> TestResult<HouseConfig> {
    Ok(HouseConfig {
        schema: 1,
        house: house()?,
        kitchen: commit('a')?,
        guidance: commit('b')?,
        repositories: BTreeSet::from([project()?]),
        posting_destinations: BTreeSet::from([project()?]),
        required_reviewers: BTreeSet::new(),
        required_checks: BTreeSet::new(),
        policy_limits: BTreeSet::from([
            repo_grant(Permission::LaunchWorker)?,
            repo_grant(Permission::RequestReview)?,
            repo_grant(Permission::Merge)?,
        ]),
        grants: BTreeSet::new(),
        agents: None,
        stack_tool: None,
        schedules: None,
        merge_readiness: BTreeMap::new(),
        disk_pressure: None,
        follow_up: None,
        backend: None,
        graduation: BTreeMap::from([(scope()?.work_type, policy)]),
        tick: None,
    })
}
fn spec(name: &str, guidance: char) -> TestResult<TaskSpec> {
    let mut task = common::spec(name)?;
    task.repository = Some(project()?);
    task.provenance.house_guidance = commit(guidance)?;
    task.work_type = Some(scope()?.work_type);
    task.agent = Some(ResolvedSelection::owner(AgentSelection {
        agent: AgentFamily::Claude,
        model: Some(AgentModel::new("fixture-model-v1")?),
        effort: None,
    }));
    Ok(task)
}

/// One run: settle a task claimed by `claimant` and collect its observation.
fn settle(f: &Fixture, name: &str, guidance: char, claimant: &Claimant) -> TestResult<TaskId> {
    let id = task_id(name)?;
    f.store
        .create_task(spec(name, guidance)?, &creator()?, at(0))?;
    let lease = f.store.claim(&id, claimant, ttl(60)?, at(1))?;
    f.store.start_attempt(&id, lease.fence(), at(2))?;
    f.store.finish_attempt(
        &id,
        lease.fence(),
        AttemptNumber::FIRST,
        AttemptOutcome::Succeeded,
        at(3),
    )?;
    Ok(id)
}
fn subject() -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit('d')?,
        base: Some(commit('e')?),
    })
}
fn observe(
    f: &Fixture,
    id: &TaskId,
    observed: u64,
    first_pass: Measurement<bool>,
) -> TestResult<Observation> {
    let mut o = Observation::collect(
        &f.store,
        id,
        source(&format!("fixture:{id}"))?,
        Attribution {
            scope: scope()?,
            agent: measured(holder("worker")?)?,
            model: measured(Text::new(MODEL)?)?,
            tokens: measured(100)?,
        },
        EvidenceMode::Live,
        at(observed),
    )?;
    o.pull_request = measured(PullRequestEvidence {
        house: house()?,
        task: id.clone(),
        repository: project()?,
        source: source(&format!("https://example.invalid/pr/{id}"))?,
        subject: subject()?,
        first_pass,
        findings: measured(Vec::new())?,
        reverts: measured(Vec::new())?,
        regressions: measured(Vec::new())?,
        checks: Measurement::Unavailable,
    })?;
    Ok(o)
}
/// A live, supervised run on `guidance`, observed `observed` seconds in.
fn run(
    f: &Fixture,
    l: &Ledger,
    name: &str,
    guidance: char,
    accepted: bool,
    observed: u64,
) -> TestResult<(ExternalRef, NonZeroU32)> {
    let id = settle(f, name, guidance, &interactive("owner")?)?;
    let o = observe(f, &id, observed, measured(accepted)?)?;
    let cited = (o.id.clone(), o.revision);
    l.record(&f.store, o)?;
    Ok(cited)
}
fn ledger(f: &Fixture) -> TestResult<Ledger> {
    Ok(Ledger::initialize(f.dir.path().join("trust"), house()?)?)
}
fn decision(
    id: &str,
    guidance: char,
    claims: &[Permission],
    evidence: Vec<(ExternalRef, NonZeroU32)>,
) -> TestResult<GraduationDecision> {
    Ok(GraduationDecision {
        id: source(id)?,
        house: house()?,
        scope: scope()?,
        guidance: commit(guidance)?,
        claims: claims
            .iter()
            .map(|p| repo_grant(*p))
            .collect::<TestResult<_>>()?,
        evidence,
        schedule: Some(ResourceRef {
            kind: ResourceKind::Schedule,
            backend: backend_id()?,
            handle: source("schedule:implementation")?,
        }),
        approved_by: holder("owner")?,
        source: source("fixture:owner-decision")?,
        at: at(NOW),
        expires_at: at(NOW + 30 * DAY),
    })
}
/// Three accepted supervised runs on `guidance`, inside the window.
fn three_runs(
    f: &Fixture,
    l: &Ledger,
    prefix: &str,
    guidance: char,
) -> TestResult<Vec<(ExternalRef, NonZeroU32)>> {
    (0..3)
        .map(|i| {
            run(
                f,
                l,
                &format!("{prefix}-{i}"),
                guidance,
                true,
                NOW - DAY + i,
            )
        })
        .collect()
}
/// Interactive limits as the base authority, and a bound task pinned to `guidance`.
fn acting(l: &Ledger, name: &str, guidance: char) -> TestResult<TaskSpec> {
    let mut task = spec(name, guidance)?;
    task.authority = TaskAuthority::delegate(&common::grants()?, [])?;
    l.bind_task(&task, source("fixture:binding")?)?;
    Ok(task)
}
fn base(c: &HouseConfig) -> TestResult<HouseGrants> {
    Ok(HouseGrants::with_limits(
        house()?,
        c.policy_limits.iter().cloned(),
        [],
    )?)
}
/// Whether `grants` lets a scheduled task hold `permission` in the project.
fn holds(grants: &HouseGrants, permission: Permission) -> TestResult<bool> {
    Ok(TaskAuthority::delegate(grants, [repo_grant(permission)?]).is_ok())
}
fn standing(
    f: &Fixture,
    l: &Ledger,
    c: &HouseConfig,
    task: &TaskSpec,
    now: u64,
) -> TestResult<HouseGrants> {
    Ok(l.graduated_standing(&f.store, c, task, &base(c)?, at(now))?)
}

#[test]
fn house_config_bounds_graduation_thresholds() -> TestResult {
    config(policy(3, 80)?)?.validate()?;
    for bad in [
        GraduationPolicy {
            min_first_pass_percent: 101,
            ..policy(3, 80)?
        },
        GraduationPolicy {
            window_days: NonZeroU16::new(366).ok_or("zero")?,
            ..policy(3, 80)?
        },
        policy(129, 80)?,
    ] {
        assert!(matches!(bad.validate(), Err(TrustError::Invalid)));
        assert!(matches!(
            config(bad)?.validate(),
            Err(HouseError::InvalidInput)
        ));
    }
    // Work types are validated names in the house file too.
    let json = serde_json::to_string(&config(policy(3, 80)?)?)?;
    let renamed = json.replace("\"implementation\":", "\"Not A Work Type\":");
    assert_ne!(renamed, json);
    assert!(serde_json::from_str::<HouseConfig>(&renamed).is_err());
    // The policy round-trips through the strict house file format.
    let json = serde_json::to_string(&config(policy(3, 80)?)?)?;
    assert!(json.contains("\"minSupervisedRuns\":3"));
    assert_eq!(
        serde_json::from_str::<HouseConfig>(&json)?,
        config(policy(3, 80)?)?
    );
    Ok(())
}

#[test]
fn insufficient_samples_are_reported_and_cannot_graduate() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = vec![
        run(&f, &l, "one", 'b', true, NOW - DAY)?,
        run(&f, &l, "two", 'b', true, NOW - DAY)?,
    ];
    let report = l.eligibility(&f.store, &c, &scope()?, at(NOW))?;
    assert_eq!(
        report.verdict,
        Eligibility::InsufficientSamples {
            runs: 2,
            required: NonZeroU32::new(3).ok_or("zero")?
        }
    );
    assert_eq!(report.guidance, commit('b')?);
    assert_eq!(report.runs.len(), 2);
    assert!(report.runs.iter().all(|r| r.accepted));
    assert_eq!(
        report.runs[0].source,
        source("fixture:measurement")?,
        "each run names its measurement source"
    );
    let refused = l.graduate(
        &f.store,
        &c,
        decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?,
        at(NOW),
    );
    assert!(matches!(refused, Err(TrustError::Refused)));
    assert!(l.graduation_history()?.is_empty());

    // A work type without thresholds is reported, never eligible.
    let mut none = c.clone();
    none.graduation.clear();
    let report = l.eligibility(&f.store, &none, &scope()?, at(NOW))?;
    assert_eq!(report.verdict, Eligibility::NoPolicy);
    assert_eq!(report.runs.len(), 2);
    Ok(())
}

#[test]
fn first_pass_threshold_is_inclusive() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    for (i, accepted) in [true, true, true, false].into_iter().enumerate() {
        run(&f, &l, &format!("run-{i}"), 'b', accepted, NOW - DAY)?;
    }
    let report = l.eligibility(&f.store, &config(policy(4, 75)?)?, &scope()?, at(NOW))?;
    assert_eq!(report.verdict, Eligibility::Eligible);
    assert_eq!(report.accepted(), 3);
    let report = l.eligibility(&f.store, &config(policy(4, 76)?)?, &scope()?, at(NOW))?;
    assert_eq!(
        report.verdict,
        Eligibility::BelowAcceptance {
            accepted: 3,
            runs: 4,
            required_percent: 76
        }
    );
    Ok(())
}

#[test]
fn mixed_guidance_revisions_count_only_the_current_one() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    three_runs(&f, &l, "old", 'c')?;
    run(&f, &l, "new-0", 'b', true, NOW - DAY)?;
    run(&f, &l, "new-1", 'b', true, NOW - DAY)?;
    let mut c = config(policy(3, 80)?)?;
    let report = l.eligibility(&f.store, &c, &scope()?, at(NOW))?;
    assert_eq!(report.runs.len(), 2);
    assert!(matches!(
        report.verdict,
        Eligibility::InsufficientSamples { runs: 2, .. }
    ));
    assert_eq!(report.other_revisions.len(), 1);
    assert_eq!(report.other_revisions[0].guidance, commit('c')?);
    assert_eq!(report.other_revisions[0].runs.len(), 3);

    // The same evidence evaluated on revision c.
    c.guidance = commit('c')?;
    let report = l.eligibility(&f.store, &c, &scope()?, at(NOW))?;
    assert_eq!(report.verdict, Eligibility::Eligible);
    assert_eq!(report.other_revisions[0].guidance, commit('b')?);
    assert_eq!(report.other_revisions[0].runs.len(), 2);
    Ok(())
}

#[test]
fn missing_simulated_and_unsupervised_evidence_is_excluded_with_reasons() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(1, 0)?)?;
    let mut expected = BTreeMap::new();

    let id = settle(&f, "no-first-pass", 'b', &interactive("owner")?)?;
    let o = observe(&f, &id, NOW - DAY, Measurement::Missing)?;
    expected.insert(o.id.clone(), ExclusionReason::FirstPassMissing);
    l.record(&f.store, o)?;

    let id = settle(&f, "simulated", 'b', &interactive("owner")?)?;
    let mut o = observe(&f, &id, NOW - DAY, measured(true)?)?;
    o.mode = EvidenceMode::Simulated;
    expected.insert(o.id.clone(), ExclusionReason::Simulated);
    l.record(&f.store, o)?;

    let id = settle(&f, "scheduled", 'b', &scheduled("tick")?)?;
    let o = observe(&f, &id, NOW - DAY, measured(true)?)?;
    expected.insert(o.id.clone(), ExclusionReason::Unsupervised);
    l.record(&f.store, o)?;

    let id = settle(&f, "stale", 'b', &interactive("owner")?)?;
    let o = observe(&f, &id, NOW - 8 * DAY, measured(true)?)?;
    expected.insert(o.id.clone(), ExclusionReason::OutsideWindow);
    l.record(&f.store, o)?;

    let id = settle(&f, "future", 'b', &interactive("owner")?)?;
    let o = observe(&f, &id, NOW + 1, measured(true)?)?;
    expected.insert(o.id.clone(), ExclusionReason::OutsideWindow);
    l.record(&f.store, o)?;

    // Only a correction was delivered: revision 1 is still missing.
    let id = settle(&f, "gap", 'b', &interactive("owner")?)?;
    let mut o = observe(&f, &id, NOW - DAY, measured(true)?)?;
    o.revision = NonZeroU32::new(2).ok_or("zero")?;
    o.correction = Some(source("fixture:correction")?);
    expected.insert(o.id.clone(), ExclusionReason::IncompleteStream);
    l.record(&f.store, o)?;

    let report = l.eligibility(&f.store, &c, &scope()?, at(NOW))?;
    assert!(report.runs.is_empty());
    assert!(report.other_revisions.is_empty());
    let excluded: BTreeMap<_, _> = report
        .excluded
        .into_iter()
        .map(|e| (e.stream, e.reason))
        .collect();
    assert_eq!(excluded, expected);
    assert!(matches!(
        report.verdict,
        Eligibility::InsufficientSamples { runs: 0, .. }
    ));
    Ok(())
}

#[test]
fn cross_house_evidence_and_decisions_are_refused() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    let c = config(policy(3, 80)?)?;

    let mut foreign = c.clone();
    foreign.house = other_house()?;
    assert!(matches!(
        l.eligibility(&f.store, &foreign, &scope()?, at(NOW)),
        Err(TrustError::Refused)
    ));
    let other_store = HouseStore::initialize(
        f.dir.path().join("other"),
        other_house()?,
        StoreOptions::default(),
    )?;
    assert!(matches!(
        l.eligibility(&other_store, &c, &scope()?, at(NOW)),
        Err(TrustError::Refused)
    ));
    // The other house's ledger has no view of this house's evidence.
    let theirs = Ledger::initialize(f.dir.path().join("other-trust"), other_house()?)?;
    let id = task_id("run-0")?;
    assert!(matches!(
        theirs.record(&f.store, observe(&f, &id, NOW - DAY, measured(true)?)?),
        Err(TrustError::Refused)
    ));
    let mut stolen = decision("grad:x", 'b', &[Permission::LaunchWorker], evidence.clone())?;
    stolen.house = other_house()?;
    assert!(matches!(
        theirs.graduate(&other_store, &foreign, stolen.clone(), at(NOW)),
        Err(TrustError::Refused)
    ));
    assert!(matches!(
        l.graduate(&f.store, &c, stolen, at(NOW)),
        Err(TrustError::Refused)
    ));
    // A project outside the house is refused rather than reported empty.
    let mut elsewhere = scope()?;
    elsewhere.project = Repository::new("example/other")?;
    assert!(matches!(
        l.eligibility(&f.store, &c, &elsewhere, at(NOW)),
        Err(TrustError::Refused)
    ));
    assert!(l.graduation_history()?.is_empty());
    Ok(())
}

#[test]
fn eligibility_never_grants_until_the_owner_decides() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    let task = acting(&l, "next", 'b')?;
    assert_eq!(
        l.eligibility(&f.store, &c, &scope()?, at(NOW))?.verdict,
        Eligibility::Eligible
    );
    assert!(!holds(
        &standing(&f, &l, &c, &task, NOW)?,
        Permission::LaunchWorker
    )?);

    // The decision must cite exactly the counted runs.
    let partial = decision(
        "grad:1",
        'b',
        &[Permission::LaunchWorker],
        evidence[..2].to_vec(),
    )?;
    assert!(matches!(
        l.graduate(&f.store, &c, partial, at(NOW)),
        Err(TrustError::Refused)
    ));
    let mut doubled = evidence.clone();
    doubled.push(evidence[0].clone());
    assert!(matches!(
        l.graduate(
            &f.store,
            &c,
            decision("grad:1", 'b', &[Permission::LaunchWorker], doubled)?,
            at(NOW)
        ),
        Err(TrustError::Refused)
    ));
    let stale = decision("grad:1", 'c', &[Permission::LaunchWorker], evidence.clone())?;
    assert!(matches!(
        l.graduate(&f.store, &c, stale, at(NOW)),
        Err(TrustError::Refused)
    ));

    let owner = decision("grad:1", 'b', &[Permission::LaunchWorker], evidence.clone())?;
    assert!(l.graduate(&f.store, &c, owner.clone(), at(NOW))?);
    assert!(
        !l.graduate(&f.store, &c, owner.clone(), at(NOW))?,
        "idempotent"
    );
    let mut edited = owner.clone();
    edited.expires_at = at(NOW + DAY);
    assert!(matches!(
        l.graduate(&f.store, &c, edited, at(NOW)),
        Err(TrustError::Conflict)
    ));
    let granted = standing(&f, &l, &c, &task, NOW)?;
    assert!(holds(&granted, Permission::LaunchWorker)?);
    assert!(!holds(&granted, Permission::RequestReview)?);

    // Durable across a reopen, with the original decision intact.
    let reopened = Ledger::open(f.dir.path().join("trust"), house()?)?;
    assert_eq!(
        reopened.graduation_history()?,
        vec![GraduationAudit::Decided(owner)]
    );
    Ok(())
}

#[test]
fn unattended_claims_stay_within_interactive_scope() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    // PostComment is outside the interactive limits; Merge is inside them but
    // never earned; the others are separate authority entirely.
    for permission in [
        Permission::PostComment,
        Permission::Merge,
        Permission::Publish,
        Permission::OperateEquipment,
        Permission::ActivateSchedule,
        Permission::PushBranch,
    ] {
        let d = decision(
            "grad:1",
            'b',
            &[Permission::LaunchWorker, permission],
            evidence.clone(),
        )?;
        assert!(
            matches!(
                l.graduate(&f.store, &c, d, at(NOW)),
                Err(TrustError::Refused)
            ),
            "{permission} must not graduate"
        );
    }
    let mut wide = decision("grad:1", 'b', &[], evidence.clone())?;
    wide.claims = BTreeSet::from([Grant::house(
        Permission::LaunchWorker,
        backend_id()?,
        credential()?,
    )]);
    assert!(matches!(
        l.graduate(&f.store, &c, wide, at(NOW)),
        Err(TrustError::Refused)
    ));
    let mut not_a_schedule =
        decision("grad:1", 'b', &[Permission::LaunchWorker], evidence.clone())?;
    if let Some(schedule) = &mut not_a_schedule.schedule {
        schedule.kind = ResourceKind::Worktree;
    }
    assert!(matches!(
        l.graduate(&f.store, &c, not_a_schedule, at(NOW)),
        Err(TrustError::Invalid)
    ));
    let empty = decision("grad:1", 'b', &[], evidence.clone())?;
    assert!(matches!(
        l.graduate(&f.store, &c, empty, at(NOW)),
        Err(TrustError::Invalid)
    ));
    assert!(l.graduation_history()?.is_empty());

    // Narrowing the interactive limits after the decision withdraws the claim.
    let d = decision(
        "grad:2",
        'b',
        &[Permission::LaunchWorker, Permission::RequestReview],
        evidence,
    )?;
    l.graduate(&f.store, &c, d, at(NOW))?;
    let task = acting(&l, "next", 'b')?;
    let mut narrowed = c.clone();
    narrowed
        .policy_limits
        .remove(&repo_grant(Permission::RequestReview)?);
    let granted = standing(&f, &l, &narrowed, &task, NOW)?;
    assert!(holds(&granted, Permission::LaunchWorker)?);
    assert!(!holds(&granted, Permission::RequestReview)?);
    Ok(())
}

#[test]
fn decisions_expire_and_terms_are_bounded() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    for expires in [at(NOW), at(NOW + 366 * DAY)] {
        let mut d = decision("grad:1", 'b', &[Permission::LaunchWorker], evidence.clone())?;
        d.expires_at = expires;
        assert!(matches!(
            l.graduate(&f.store, &c, d, at(NOW)),
            Err(TrustError::Invalid)
        ));
    }
    let d = decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?;
    let expiry = d.expires_at.as_unix_millis() / 1000;
    l.graduate(&f.store, &c, d, at(NOW))?;
    let task = acting(&l, "next", 'b')?;
    // Within the term; outside the eligibility window no longer matters.
    assert!(holds(
        &standing(&f, &l, &c, &task, expiry - 1)?,
        Permission::LaunchWorker
    )?);
    assert!(!holds(
        &standing(&f, &l, &c, &task, expiry)?,
        Permission::LaunchWorker
    )?);
    assert!(l.graduation_reviews(&c, at(expiry))?.is_empty());
    Ok(())
}

/// One millisecond before `t`.
fn instant_before(t: Timestamp) -> TestResult<Timestamp> {
    Ok(Timestamp::from_unix_millis(
        t.as_unix_millis().checked_sub(1).ok_or("underflow")?,
    ))
}

#[test]
fn decisions_apply_only_within_their_term_and_are_never_future_dated() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    let d = decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?;
    let before = instant_before(d.at)?;
    // Recorded an instant before its own time, the decision is refused.
    assert!(matches!(
        l.graduate(&f.store, &c, d.clone(), before),
        Err(TrustError::Invalid)
    ));
    assert!(l.graduation_history()?.is_empty());
    assert!(l.graduate(&f.store, &c, d.clone(), d.at)?);

    let task = acting(&l, "next", 'b')?;
    let held = |now| -> TestResult<bool> {
        holds(
            &l.graduated_standing(&f.store, &c, &task, &base(&c)?, now)?,
            Permission::LaunchWorker,
        )
    };
    assert!(!held(before)?, "not before the decision's time");
    assert!(held(d.at)?, "from the decision's time");
    assert!(held(instant_before(d.expires_at)?)?);
    assert!(!held(d.expires_at)?, "not at expiry");
    Ok(())
}

#[test]
fn evidence_recorded_during_a_decision_refuses_it() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    // A stream in scope that is already missing revision 1.
    let id = settle(&f, "gap", 'b', &interactive("owner")?)?;
    let mut gapped = observe(&f, &id, NOW - DAY, measured(true)?)?;
    gapped.revision = NonZeroU32::new(2).ok_or("zero")?;
    gapped.correction = Some(source("fixture:correction")?);
    l.record(&f.store, gapped.clone())?;
    // Its revision 3 reports a revert observed before the decision, which
    // demotion does not look at, and leaves the stream incomplete.
    let mut corrected = gapped;
    corrected.revision = NonZeroU32::new(3).ok_or("zero")?;
    corrected.correction = Some(source("fixture:second-correction")?);
    if let Measurement::Observed { value, .. } = &mut corrected.pull_request {
        value.reverts = measured(vec![Finding {
            source: source("https://example.invalid/pull/11")?,
            subject: subject()?,
            consequence: Text::new("Reverted after merge.")?,
        }])?;
    }
    let d = decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?;

    // Revision 3 lands after the report is read and before the write.
    let recorded = Rc::new(RefCell::new(None));
    test_hooks::on_next_graduation_write({
        let (path, house, store) = (f.dir.path().join("trust"), house()?, f.reopen()?);
        let recorded = Rc::clone(&recorded);
        move || {
            recorded.replace(Some(
                Ledger::open(path, house).and_then(|other| other.record(&store, corrected)),
            ));
        }
    });
    assert!(matches!(
        l.graduate(&f.store, &c, d.clone(), at(NOW)),
        Err(TrustError::Refused)
    ));
    assert!(recorded.take().ok_or("hook did not run")??);
    assert!(l.graduation_history()?.is_empty());
    // The report itself is unchanged, so an owner who has seen the new
    // revision can record the same decision.
    let report = l.eligibility(&f.store, &c, &scope()?, at(NOW))?;
    assert_eq!(report.verdict, Eligibility::Eligible);
    assert!(
        report
            .excluded
            .iter()
            .any(|e| e.reason == ExclusionReason::IncompleteStream)
    );
    assert!(l.graduate(&f.store, &c, d.clone(), at(NOW))?);

    // A new complete run in scope refuses a decision the same way.
    let id = settle(&f, "late", 'b', &interactive("owner")?)?;
    let late = observe(&f, &id, NOW - DAY, measured(false)?)?;
    let recorded = Rc::new(RefCell::new(None));
    test_hooks::on_next_graduation_write({
        let (path, house, store) = (f.dir.path().join("trust"), house()?, f.reopen()?);
        let recorded = Rc::clone(&recorded);
        move || {
            recorded.replace(Some(
                Ledger::open(path, house).and_then(|other| other.record(&store, late)),
            ));
        }
    });
    let second = GraduationDecision {
        id: source("grad:2")?,
        ..d
    };
    assert!(matches!(
        l.graduate(&f.store, &c, second, at(NOW)),
        Err(TrustError::Refused)
    ));
    assert!(recorded.take().ok_or("hook did not run")??);
    assert_eq!(l.graduation_history()?.len(), 1);
    Ok(())
}

#[test]
fn revocation_is_immediate_and_audited() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    let d = decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?;
    l.graduate(&f.store, &c, d.clone(), at(NOW))?;
    let task = acting(&l, "next", 'b')?;
    let id = source("grad:1")?;
    assert!(l.revoke_graduation(
        &id,
        holder("owner")?,
        source("fixture:revoke")?,
        at(NOW + 1)
    )?);
    assert!(!l.revoke_graduation(
        &id,
        holder("owner")?,
        source("fixture:revoke")?,
        at(NOW + 2)
    )?);
    assert!(matches!(
        l.revoke_graduation(
            &source("grad:unknown")?,
            holder("owner")?,
            source("fixture:revoke")?,
            at(NOW)
        ),
        Err(TrustError::NotFound)
    ));
    assert!(!holds(
        &standing(&f, &l, &c, &task, NOW + 3)?,
        Permission::LaunchWorker
    )?);
    assert!(matches!(
        l.graduate(&f.store, &c, d.clone(), at(NOW)),
        Err(TrustError::Conflict)
    ));
    assert!(matches!(
        l.graduation_history()?.as_slice(),
        [GraduationAudit::Revoked { decision, .. }] if decision == &d
    ));
    Ok(())
}

#[test]
fn guidance_change_resets_or_re_evaluates_by_policy() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "old", 'b')?;
    l.graduate(
        &f.store,
        &c,
        decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?,
        at(NOW),
    )?;
    c.guidance = commit('c')?;
    let old_pin = acting(&l, "old-pin", 'b')?;
    let new_pin = acting(&l, "new-pin", 'c')?;

    // Reset: tasks still pinned to b keep the decision; c starts over.
    assert!(holds(
        &standing(&f, &l, &c, &old_pin, NOW)?,
        Permission::LaunchWorker
    )?);
    assert!(!holds(
        &standing(&f, &l, &c, &new_pin, NOW)?,
        Permission::LaunchWorker
    )?);

    // Re-evaluate: suspended on c until c's own evidence is eligible.
    let mut reeval = policy(3, 80)?;
    reeval.on_guidance_change = GuidanceChange::ReEvaluate;
    c.graduation.insert(scope()?.work_type, reeval);
    assert!(!holds(
        &standing(&f, &l, &c, &new_pin, NOW)?,
        Permission::LaunchWorker
    )?);
    three_runs(&f, &l, "new", 'c')?;
    assert!(holds(
        &standing(&f, &l, &c, &new_pin, NOW)?,
        Permission::LaunchWorker
    )?);
    // The older evidence stays visible, separately.
    let report = l.eligibility(&f.store, &c, &scope()?, at(NOW))?;
    assert_eq!(report.other_revisions[0].guidance, commit('b')?);

    // Withdrawing the work type's policy stops every decision for it.
    c.graduation.clear();
    assert!(!holds(
        &standing(&f, &l, &c, &old_pin, NOW)?,
        Permission::LaunchWorker
    )?);
    Ok(())
}

#[test]
fn a_regression_after_graduation_demotes_and_plans_a_pause() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let mut c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    l.graduate(
        &f.store,
        &c,
        decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?,
        at(NOW),
    )?;
    let task = acting(&l, "next", 'b')?;
    assert!(l.graduation_reviews(&c, at(NOW + 1))?.is_empty());

    // An unattended run after the decision is later found to regress.
    let id = settle(&f, "unattended", 'b', &scheduled("tick")?)?;
    let mut o = observe(&f, &id, NOW + DAY, measured(true)?)?;
    let finding = Finding {
        source: source("https://example.invalid/issues/9")?,
        subject: subject()?,
        consequence: Text::new("Broke the release build.")?,
    };
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.regressions = measured(vec![finding.clone()])?;
    }
    l.record(&f.store, o)?;

    assert!(!holds(
        &standing(&f, &l, &c, &task, NOW + DAY)?,
        Permission::LaunchWorker
    )?);
    let reviews = l.graduation_reviews(&c, at(NOW + DAY))?;
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].decision, source("grad:1")?);
    assert_eq!(reviews[0].findings, vec![finding.source.clone()]);
    assert_eq!(
        reviews[0].pause,
        Some(ScheduleEffect::SetState {
            schedule: ResourceRef {
                kind: ResourceKind::Schedule,
                backend: backend_id()?,
                handle: source("schedule:implementation")?,
            },
            state: ScheduleState::Paused,
            requires: None,
        })
    );
    // A revert delivered behind a revision gap still counts.
    let id = settle(&f, "reverted", 'b', &scheduled("tick")?)?;
    let mut o = observe(&f, &id, NOW + DAY, measured(true)?)?;
    o.revision = NonZeroU32::new(2).ok_or("zero")?;
    o.correction = Some(source("fixture:correction")?);
    let revert = Finding {
        source: source("https://example.invalid/pull/10")?,
        ..finding.clone()
    };
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.reverts = measured(vec![revert.clone()])?;
    }
    l.record(&f.store, o)?;
    let reviews = l.graduation_reviews(&c, at(NOW + DAY))?;
    assert_eq!(
        reviews[0].findings,
        vec![finding.source.clone(), revert.source]
    );
    // A report-only house reviews without planning a pause.
    let mut report_only = policy(3, 80)?;
    report_only.on_regression = RegressionResponse::Report;
    c.graduation.insert(scope()?.work_type, report_only);
    let reviews = l.graduation_reviews(&c, at(NOW + DAY))?;
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].pause, None);
    Ok(())
}

#[test]
fn archiving_keeps_graduation_evidence_and_later_regressions() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    // Uncited: an older run before the window, on another revision.
    let (unrelated, _) = run(&f, &l, "unrelated", 'c', true, DAY)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    l.graduate(
        &f.store,
        &c,
        decision("grad:1", 'b', &[Permission::LaunchWorker], evidence.clone())?,
        at(NOW),
    )?;
    let id = settle(&f, "regressed", 'b', &scheduled("tick")?)?;
    let mut o = observe(&f, &id, NOW + DAY, measured(true)?)?;
    let regressed = o.id.clone();
    if let Measurement::Observed { value, .. } = &mut o.pull_request {
        value.regressions = measured(vec![Finding {
            source: source("https://example.invalid/issues/9")?,
            subject: subject()?,
            consequence: Text::new("Broke the release build.")?,
        }])?;
    }
    l.record(&f.store, o)?;

    let report = l.archive(at(NOW + 2 * DAY))?;
    let moved: Vec<_> = report.streams.iter().map(|s| s.id.clone()).collect();
    assert_eq!(moved, vec![unrelated]);
    assert_eq!(report.kept.streams_cited_by_grants, 3);
    assert_eq!(report.kept.streams_after_graduation, 1);
    // The demotion survives the archival.
    let reviews = l.graduation_reviews(&c, at(NOW + 2 * DAY))?;
    assert_eq!(reviews.len(), 1);
    assert!(l.history()?.iter().any(|o| o.id == regressed));
    // Revoked decisions stop holding later streams but keep their evidence.
    l.revoke_graduation(
        &source("grad:1")?,
        holder("owner")?,
        source("fixture:revoke")?,
        at(NOW + 2 * DAY),
    )?;
    let report = l.preview_archive(at(NOW + 3 * DAY))?;
    let moved: Vec<_> = report.streams.iter().map(|s| s.id.clone()).collect();
    assert_eq!(moved, vec![regressed]);
    assert_eq!(report.kept.streams_cited_by_grants, evidence.len());
    Ok(())
}

#[test]
fn an_expired_decision_stops_holding_later_streams() -> TestResult {
    let f = Fixture::new()?;
    let l = ledger(&f)?;
    let c = config(policy(3, 80)?)?;
    let evidence = three_runs(&f, &l, "run", 'b')?;
    l.graduate(
        &f.store,
        &c,
        decision("grad:1", 'b', &[Permission::LaunchWorker], evidence)?,
        at(NOW),
    )?;
    let (later, _) = run(&f, &l, "later", 'b', true, NOW + DAY)?;
    // While the decision is in force, the later stream is kept for it.
    let kept = l.preview_archive(at(NOW + 2 * DAY))?;
    assert!(kept.streams.iter().all(|s| s.id != later));
    assert_eq!(kept.kept.streams_after_graduation, 1);
    // Once it expires unrevoked, nothing reads that stream for it any more.
    let expired = l.preview_archive(at(NOW + 31 * DAY))?;
    assert_eq!(expired.kept.streams_after_graduation, 0);
    Ok(())
}
