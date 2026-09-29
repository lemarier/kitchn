//! Brigade audit (#45): report and draft proposals from the trust ledger,
//! attempt usage, and schedule runs of one house.
//!
//! Offline fixtures: no live ledger, backend, schedule, or forge is read.
//! "Live" below is the evidence mode a fixture declares, not a live run.
mod common;
use common::{
    Fixture, ManualClock, TestResult, at, commit, creator, holder, house, other_house, scheduled,
    task_id, ttl,
};
use kitchen::{
    BackendId, ConsumerId, ErrorClass, HouseId,
    contracts::{
        AttemptNumber, AttemptOutcome, Capability, CapabilitySet, Evidence, EvidenceKind,
        EvidenceSubject, EvidenceVerdict, ExternalRef, Fence, Repository, ResourceKind,
        ResourceRef, Role, Support, TaskSpec, Text,
    },
    scheduling::{
        AgentFamily, Budget, IdlePolicy, IntervalMinutes, JudgedRun, ObservedScheduleState,
        RunOutcome, RunVerdict, ScheduleEvidence, ScheduleObservation, SchedulePolicy, ScheduleRun,
        ScheduleUsage, WindowHours,
    },
    selection::{AgentModel, AgentSelection, ResolvedSelection, WorkType},
    state::{Cost, CostBasis, HouseStore, StoreOptions, TokenCounts, UsageReport, UsdMicros},
    trust::{
        Attribution, EvidenceMode, Finding, Ledger, Measurement, Observation, PullRequestEvidence,
        StationScope,
    },
    workflows::{
        audit::{
            Acceptance, AuditError, AuditInputs, AuditPolicy, AuditReport, MAX_PROPOSALS, Proposal,
            ProposalKey, ProposalKind, ScheduleChange, ScheduleSignal, audit, marker, proposal_key,
        },
        inspector::{FollowUpRoute, InspectionPlan, SampleResult},
    },
};
use std::{collections::BTreeSet, num::NonZeroU32};

const MODEL: &str = "fixture-model-v1";
const ATTRIBUTED_MODEL: &str = "claude:fixture-model-v1";
/// Private detail a confirmed finding carries; it must never reach a report.
const CONSEQUENCE: &str = "Private: leaked the staging token in worker transcript line 42.";

fn project() -> TestResult<Repository> {
    Ok(Repository::new("example/project")?)
}

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

fn subject() -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit('a')?,
        base: Some(commit('b')?),
    })
}

fn finding(name: &str) -> TestResult<Finding> {
    Ok(Finding {
        source: source(name)?,
        subject: subject()?,
        consequence: Text::new(CONSEQUENCE)?,
    })
}

fn pr(name: &str) -> TestResult<ExternalRef> {
    source(&format!("https://example.invalid/pr/{name}"))
}

/// One delivery: settle task `name` under `work_type`, bind it, and record
/// its observation with a first-pass verdict and confirmed review findings.
/// Returns the settled attempt's fence.
struct Delivery<'a> {
    name: &'a str,
    work_type: &'a str,
    mode: EvidenceMode,
    first_pass: bool,
    findings: Vec<Finding>,
}

impl<'a> Delivery<'a> {
    fn live(name: &'a str, work_type: &'a str) -> Self {
        Self {
            name,
            work_type,
            mode: EvidenceMode::Live,
            first_pass: true,
            findings: Vec::new(),
        }
    }

    fn record(self, f: &Fixture, ledger: &Ledger) -> TestResult<Fence> {
        let mut task: TaskSpec = common::spec(self.name)?;
        task.agent = Some(ResolvedSelection::owner(AgentSelection {
            agent: AgentFamily::Claude,
            model: Some(AgentModel::new(MODEL)?),
            effort: None,
        }));
        task.work_type = Some(WorkType::new(self.work_type)?);
        task.repository = Some(project()?);
        let id = task_id(self.name)?;
        f.store.create_task(task, &creator()?, at(0))?;
        let lease = f.store.claim(&id, &scheduled("owner")?, ttl(60)?, at(1))?;
        f.store.start_attempt(&id, lease.fence(), at(2))?;
        f.store.finish_attempt(
            &id,
            lease.fence(),
            AttemptNumber::FIRST,
            AttemptOutcome::Succeeded,
            at(3),
        )?;
        ledger.bind_task(
            f.store.task(&id)?.spec(),
            source(&format!("fixture:bind:{}", self.name))?,
        )?;
        let attribution = Attribution {
            scope: StationScope {
                station: Role::StationCook,
                project: project()?,
                work_type: WorkType::new(self.work_type)?,
            },
            agent: measured(holder("worker")?)?,
            model: measured(Text::new(ATTRIBUTED_MODEL)?)?,
            tokens: measured(120)?,
        };
        let mut observation = Observation::collect(
            &f.store,
            &id,
            source(&format!("fixture:stream:{}", self.name))?,
            attribution,
            self.mode,
            at(4),
        )?;
        observation.pull_request = measured(PullRequestEvidence {
            house: house()?,
            task: id,
            repository: project()?,
            source: pr(self.name)?,
            subject: subject()?,
            first_pass: measured(self.first_pass)?,
            findings: measured(self.findings)?,
            reverts: measured(Vec::new())?,
            regressions: measured(Vec::new())?,
            checks: measured(vec![Evidence {
                kind: EvidenceKind::Check,
                verdict: EvidenceVerdict::Pass,
                subject: subject()?,
                source: source("fixture:passing-check")?,
                observed_at: at(4),
            }])?,
        })?;
        ledger.record(&f.store, observation)?;
        Ok(lease.fence())
    }
}

fn ledger(f: &Fixture) -> TestResult<Ledger> {
    Ok(Ledger::initialize(f.dir.path().join("trust"), house()?)?)
}

fn budget(runs: u32) -> TestResult<Budget> {
    Ok(Budget {
        runs: NonZeroU32::new(runs).ok_or("zero runs")?,
        tokens: None,
    })
}

/// A daily window; 10 house runs and 4 per schedule. Mostly idle means 80%
/// of at least 10 recent runs.
fn schedules() -> TestResult<SchedulePolicy> {
    Ok(SchedulePolicy {
        window_hours: WindowHours::new(24)?,
        min_interval_minutes: IntervalMinutes::new(60)?,
        house_budget: budget(10)?,
        schedule_budget: budget(4)?,
        schedules: std::collections::BTreeMap::new(),
        idle: IdlePolicy::default(),
    })
}

fn run(due_seconds: u64, verdict: RunVerdict) -> JudgedRun {
    let outcome = match verdict {
        RunVerdict::Idle => RunOutcome::PrecheckIdle,
        RunVerdict::PrecheckFailed => RunOutcome::PrecheckFailed,
        RunVerdict::Skipped => RunOutcome::Skipped,
        RunVerdict::LaunchFailed => RunOutcome::LaunchFailed,
        RunVerdict::Pending | RunVerdict::Started => RunOutcome::LaunchReported,
        RunVerdict::Unknown => RunOutcome::Unknown,
    };
    JudgedRun {
        run: ScheduleRun {
            outcome,
            scheduled_for: Some(at(due_seconds)),
            created_at: None,
            usage: Measurement::Missing,
            agent: None,
        },
        verdict,
    }
}

fn schedule_ref(name: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Schedule,
        backend: BackendId::new("orca-local")?,
        handle: source(&format!("orca-automation:{name}"))?,
    })
}

/// `name` with `started` agent runs and `idle` idle prechecks in the window.
fn schedule(name: &str, started: u64, idle: u64) -> TestResult<ScheduleUsage> {
    let runs = (0..started)
        .map(|n| run(10 + n, RunVerdict::Started))
        .chain((0..idle).map(|n| run(1000 + n, RunVerdict::Idle)))
        .collect();
    Ok(ScheduleUsage {
        consumer: ConsumerId::new(name)?,
        schedule: schedule_ref(name)?,
        observation: ScheduleObservation {
            state: ObservedScheduleState::Active,
            recent_runs: runs,
        },
    })
}

fn evidence(house: HouseId, schedules: Vec<ScheduleUsage>) -> ScheduleEvidence {
    ScheduleEvidence {
        house,
        observed_at: at(5000),
        schedules,
    }
}

fn run_audit(
    f: &Fixture,
    ledger: &Ledger,
    evidence: &ScheduleEvidence,
    open: &BTreeSet<ProposalKey>,
    policy: &AuditPolicy,
) -> TestResult<kitchen::Result<AuditReport>> {
    let (house, schedules) = (house()?, schedules()?);
    Ok(audit(
        policy,
        &AuditInputs {
            house: &house,
            ledger,
            store: &f.store,
            schedules: &schedules,
            evidence,
            open,
        },
    ))
}

fn only(report: &AuditReport) -> TestResult<&Proposal> {
    match report.proposals.as_slice() {
        [proposal] => Ok(proposal),
        other => Err(format!("expected one proposal, got {other:?}").into()),
    }
}

fn key(value: &str) -> TestResult<ProposalKey> {
    ProposalKey::parse(value).ok_or_else(|| format!("invalid key {value}").into())
}

#[test]
fn an_empty_ledger_reports_nothing_and_proposes_nothing() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    let quiet = evidence(house()?, vec![schedule("pickup", 1, 2)?]);
    let report = run_audit(
        &f,
        &ledger,
        &quiet,
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    assert_eq!(report.house, house()?);
    assert_eq!(report.observed_at, at(5000));
    assert!(report.stations.is_empty());
    assert!(report.schedules.is_empty());
    assert!(report.divergences.is_empty());
    assert!(report.proposals.is_empty());
    assert!(report.deduplicated.is_empty() && report.deferred.is_empty());
    assert_eq!(
        (report.incomplete_streams, report.unattributed_attempts),
        (0, 0)
    );
    Ok(())
}

#[test]
fn repeated_confirmed_findings_propose_a_guidance_draft_with_evidence() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    Delivery::live("clean", "implementation").record(&f, &ledger)?;
    Delivery {
        findings: vec![finding("https://example.invalid/review/1")?],
        first_pass: false,
        ..Delivery::live("first", "implementation")
    }
    .record(&f, &ledger)?;
    Delivery {
        findings: vec![finding("https://example.invalid/review/2")?],
        ..Delivery::live("second", "implementation")
    }
    .record(&f, &ledger)?;
    // A simulated delivery is counted apart and never supports a proposal.
    Delivery {
        mode: EvidenceMode::Simulated,
        findings: vec![finding("fixture:simulated-finding")?],
        ..Delivery::live("simulated", "implementation")
    }
    .record(&f, &ledger)?;
    // One finding on another work type is not repeated.
    Delivery {
        findings: vec![finding("https://example.invalid/review/fw")?],
        ..Delivery::live("firmware", "firmware")
    }
    .record(&f, &ledger)?;

    let report = run_audit(
        &f,
        &ledger,
        &evidence(house()?, Vec::new()),
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let [firmware, implementation] = report.stations.as_slice() else {
        return Err(format!("expected two station records, got {:?}", report.stations).into());
    };
    assert_eq!(firmware.work_type, WorkType::new("firmware")?);
    assert_eq!(firmware.findings.len(), 1);
    assert_eq!(implementation.station, Role::StationCook);
    assert_eq!(
        (implementation.deliveries, implementation.simulated),
        (3, 1)
    );
    assert_eq!(
        implementation.first_pass,
        Acceptance {
            accepted: 2,
            judged: 3
        }
    );
    assert_eq!(
        implementation.findings,
        BTreeSet::from([
            source("https://example.invalid/review/1")?,
            source("https://example.invalid/review/2")?,
        ])
    );
    assert_eq!(
        implementation.deliveries_with_findings,
        BTreeSet::from([pr("first")?, pr("second")?])
    );

    let proposal = only(&report)?;
    assert_eq!(proposal.key, key("guidance:station-cook:implementation")?);
    assert_eq!(
        proposal.kind,
        ProposalKind::Guidance {
            station: Role::StationCook,
            work_type: WorkType::new("implementation")?,
            findings: 2,
        }
    );
    assert_eq!(proposal.samples, 3);
    assert_eq!(
        proposal.evidence,
        vec![
            source("https://example.invalid/review/1")?,
            source("https://example.invalid/review/2")?,
            pr("first")?,
            pr("second")?,
        ]
    );

    let draft = proposal.draft()?;
    assert!(draft.as_str().starts_with(&marker(&proposal.key)));
    assert_eq!(proposal_key(draft.as_str()), Some(proposal.key.clone()));
    assert!(draft.as_str().contains("Sample size: 3."));
    assert!(draft.as_str().contains("needs the owner's decision"));
    assert!(
        draft
            .as_str()
            .contains("- https://example.invalid/review/2")
    );
    // Nothing private reaches the draft or the serialized report.
    assert!(!draft.as_str().contains(CONSEQUENCE));
    assert!(!serde_json::to_string(&report)?.contains(CONSEQUENCE));
    Ok(())
}

#[test]
fn a_confirmed_inspection_sample_counts_as_a_finding_of_the_delivery() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    Delivery {
        findings: vec![finding("https://example.invalid/review/1")?],
        ..Delivery::live("reviewed", "implementation")
    }
    .record(&f, &ledger)?;
    Delivery::live("inspected", "implementation").record(&f, &ledger)?;
    let below = run_audit(
        &f,
        &ledger,
        &evidence(house()?, Vec::new()),
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    // One finding is below the repeated threshold of two.
    assert!(below.proposals.is_empty());

    let mut inspector = common::spec("inspector")?;
    inspector.role = Role::Inspector;
    inspector.repository = Some(project()?);
    f.store.create_task(inspector, &creator()?, at(0))?;
    let fence = f
        .store
        .claim(
            &task_id("inspector")?,
            &scheduled("reviewer")?,
            ttl(600)?,
            at(0),
        )?
        .fence();
    let plan = InspectionPlan {
        id: source("fixture:inspection")?,
        house: house()?,
        observation: source("fixture:stream:inspected")?,
        question: Text::new("Does the parser reject duplicate keys?")?,
        inspector: holder("reviewer")?,
        task: task_id("inspector")?,
        independent: true,
        max_samples: 1,
        max_tokens: 100,
        deadline: at(60),
    };
    ledger.start_inspection(&f.store, plan.clone(), fence, &ManualClock::starting_at(5))?;
    ledger.reserve_sample(
        &f.store,
        &plan.id,
        fence,
        1,
        50,
        &ManualClock::starting_at(6),
    )?;
    ledger.finish_sample(
        &f.store,
        &plan.id,
        fence,
        1,
        SampleResult::Confirmed {
            finding: finding("fixture:inspection-finding")?,
            route: FollowUpRoute::Guidance,
        },
        &ManualClock::starting_at(7),
    )?;

    let report = run_audit(
        &f,
        &ledger,
        &evidence(house()?, Vec::new()),
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let proposal = only(&report)?;
    assert_eq!(proposal.key, key("guidance:station-cook:implementation")?);
    assert!(
        proposal
            .evidence
            .contains(&source("fixture:inspection-finding")?)
    );
    Ok(())
}

#[test]
fn a_mostly_idle_schedule_proposes_fewer_idle_runs() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    let observed = evidence(
        house()?,
        vec![
            // 9 of 10 idle: at the 80% share over at least 10 runs.
            schedule("gardener", 1, 9)?,
            // 7 of 9 idle: too few runs to judge.
            schedule("triage", 2, 7)?,
        ],
    );
    let report = run_audit(
        &f,
        &ledger,
        &observed,
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let [idle] = report.schedules.as_slice() else {
        return Err(format!("expected one schedule, got {:?}", report.schedules).into());
    };
    assert_eq!(idle.consumer, ConsumerId::new("gardener")?);
    assert_eq!(
        idle.signal,
        ScheduleSignal::MostlyIdle {
            runs: 10,
            idle_runs: 9
        }
    );
    let proposal = only(&report)?;
    assert_eq!(proposal.key, key("schedule:gardener:reduce-idle-runs")?);
    assert_eq!(
        proposal.kind,
        ProposalKind::Schedule {
            consumer: ConsumerId::new("gardener")?,
            change: ScheduleChange::ReduceIdleRuns,
        }
    );
    assert_eq!(proposal.samples, 10);
    assert_eq!(proposal.evidence, vec![source("orca-automation:gardener")?]);
    Ok(())
}

#[test]
fn a_schedule_near_its_run_budget_proposes_revisiting_it() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    // 4 of 4 runs is at least 80%; 3 of 4 is 75%, below it.
    let observed = evidence(
        house()?,
        vec![schedule("pickup", 4, 0)?, schedule("repair", 3, 0)?],
    );
    let report = run_audit(
        &f,
        &ledger,
        &observed,
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let [busy] = report.schedules.as_slice() else {
        return Err(format!("expected one schedule, got {:?}", report.schedules).into());
    };
    assert_eq!(
        busy.signal,
        ScheduleSignal::HighUsage {
            runs: 4,
            allowed: 4,
            complete: true
        }
    );
    assert_eq!(only(&report)?.key, key("schedule:pickup:revisit-budget")?);
    Ok(())
}

#[test]
fn diverged_work_types_of_one_station_propose_a_split() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    for n in 0..5 {
        Delivery::live(&format!("impl-{n}"), "implementation").record(&f, &ledger)?;
        // Docs: 2 of 5 accepted on the first pass, 60 points behind.
        Delivery {
            first_pass: n < 2,
            ..Delivery::live(&format!("docs-{n}"), "docs")
        }
        .record(&f, &ledger)?;
    }
    // Four verdicts are below the five-sample minimum and are not compared.
    for n in 0..4 {
        Delivery {
            first_pass: false,
            ..Delivery::live(&format!("fw-{n}"), "firmware")
        }
        .record(&f, &ledger)?;
    }
    let report = run_audit(
        &f,
        &ledger,
        &evidence(house()?, Vec::new()),
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let [divergence] = report.divergences.as_slice() else {
        return Err(format!("expected one divergence, got {:?}", report.divergences).into());
    };
    assert_eq!(divergence.station, Role::StationCook);
    assert_eq!(
        divergence.leading,
        (
            WorkType::new("implementation")?,
            Acceptance {
                accepted: 5,
                judged: 5
            }
        )
    );
    assert_eq!(
        divergence.trailing,
        (
            WorkType::new("docs")?,
            Acceptance {
                accepted: 2,
                judged: 5
            }
        )
    );
    let proposal = only(&report)?;
    assert_eq!(proposal.key, key("split:station-cook:docs")?);
    assert_eq!(proposal.samples, 10);
    assert_eq!(
        proposal.evidence,
        vec![pr("docs-2")?, pr("docs-3")?, pr("docs-4")?]
    );

    // A wider threshold than the 60-point gap reports no divergence.
    let strict = AuditPolicy {
        divergence_points: 61,
        ..AuditPolicy::default()
    };
    let report = run_audit(
        &f,
        &ledger,
        &evidence(house()?, Vec::new()),
        &BTreeSet::new(),
        &strict,
    )??;
    assert!(report.divergences.is_empty() && report.proposals.is_empty());
    Ok(())
}

#[test]
fn an_open_proposal_is_not_proposed_again_and_the_limit_defers_the_rest() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    for name in ["first", "second"] {
        Delivery {
            findings: vec![finding(&format!("https://example.invalid/review/{name}"))?],
            ..Delivery::live(name, "implementation")
        }
        .record(&f, &ledger)?;
    }
    let observed = evidence(
        house()?,
        vec![schedule("gardener", 0, 10)?, schedule("pickup", 4, 0)?],
    );
    let first = run_audit(
        &f,
        &ledger,
        &observed,
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let keys: Vec<&str> = first.proposals.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "guidance:station-cook:implementation",
            "schedule:gardener:reduce-idle-runs",
            "schedule:pickup:revisit-budget",
        ]
    );

    // The guidance draft was filed and is still open: its marker is read back.
    let filed = first.proposals.first().ok_or("no proposal")?.draft()?;
    let open: BTreeSet<ProposalKey> = proposal_key(filed.as_str()).into_iter().collect();
    let limited = AuditPolicy {
        max_proposals: NonZeroU32::MIN,
        ..AuditPolicy::default()
    };
    let second = run_audit(&f, &ledger, &observed, &open, &limited)??;
    assert_eq!(
        second.deduplicated,
        vec![key("guidance:station-cook:implementation")?]
    );
    assert_eq!(
        only(&second)?.key,
        key("schedule:gardener:reduce-idle-runs")?
    );
    assert_eq!(
        second.deferred,
        vec![key("schedule:pickup:revisit-budget")?]
    );
    Ok(())
}

#[test]
fn the_audit_refuses_to_read_another_house() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    let foreign_ledger = Ledger::initialize(f.dir.path().join("other-trust"), other_house()?)?;
    let foreign_store = HouseStore::initialize(
        f.dir.path().join("other-store"),
        other_house()?,
        StoreOptions::default(),
    )?;
    let ours = evidence(house()?, Vec::new());
    let theirs = evidence(other_house()?, Vec::new());
    let policy = AuditPolicy::default();
    let open = BTreeSet::new();
    let schedules = schedules()?;
    let cases = [
        (&foreign_ledger, &f.store, &ours),
        (&ledger, &foreign_store, &ours),
        (&ledger, &f.store, &theirs),
    ];
    for (ledger, store, evidence) in cases {
        let refused = audit(
            &policy,
            &AuditInputs {
                house: &house()?,
                ledger,
                store,
                schedules: &schedules,
                evidence,
                open: &open,
            },
        );
        let Err(kitchen::Error::Audit(error)) = refused else {
            return Err(format!("cross-house read was not refused: {refused:?}").into());
        };
        assert_eq!(error, AuditError::CrossHouse);
        assert_eq!(error.class(), ErrorClass::Refused);
    }
    // The foreign house can audit its own sources.
    let own = audit(
        &policy,
        &AuditInputs {
            house: &other_house()?,
            ledger: &foreign_ledger,
            store: &foreign_store,
            schedules: &schedules,
            evidence: &theirs,
            open: &open,
        },
    )?;
    assert!(own.stations.is_empty());
    Ok(())
}

#[test]
fn an_exhausted_or_unproven_house_budget_refuses_the_run() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    // Three schedules with 4 runs each exhaust the 10-run house budget.
    let spent = evidence(
        house()?,
        vec![
            schedule("pickup", 4, 0)?,
            schedule("repair", 4, 0)?,
            schedule("gate", 4, 0)?,
        ],
    );
    let refused = run_audit(
        &f,
        &ledger,
        &spent,
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )?;
    let Err(kitchen::Error::Audit(AuditError::BudgetExhausted(exhausted))) = refused else {
        return Err(format!("an exhausted budget was not refused: {refused:?}").into());
    };
    assert_eq!((exhausted.used, exhausted.allowed), (12, 10));

    // A full run history that starts inside the window holds lower bounds
    // only, so it cannot show that budget remains.
    let truncated = (0..u64::try_from(kitchen::scheduling::MAX_SCHEDULE_RUNS)?)
        .map(|n| run(4000 + n % 900, RunVerdict::Idle))
        .collect();
    let unproven = evidence(
        house()?,
        vec![ScheduleUsage {
            consumer: ConsumerId::new("pickup")?,
            schedule: schedule_ref("pickup")?,
            observation: ScheduleObservation {
                state: ObservedScheduleState::Active,
                recent_runs: truncated,
            },
        }],
    );
    let refused = run_audit(
        &f,
        &ledger,
        &unproven,
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )?;
    assert!(matches!(
        refused,
        Err(kitchen::Error::Audit(AuditError::IncompleteBudget))
    ));
    Ok(())
}

#[test]
fn attempt_usage_is_summarized_per_station_and_work_type() -> TestResult {
    let f = Fixture::new()?;
    let ledger = ledger(&f)?;
    let fence = Delivery::live("reported", "implementation").record(&f, &ledger)?;
    let mut backend = common::descriptor_with([])?;
    backend.capabilities =
        CapabilitySet::default().with(Capability::UsageAttribution, Support::Supported);
    f.store.record_attempt_usage(
        &task_id("reported")?,
        fence,
        AttemptNumber::FIRST,
        &backend,
        UsageReport {
            source: source("orca-run:1")?,
            agent: Some(AgentFamily::Claude),
            model: None,
            tokens: TokenCounts {
                input: Some(100),
                output: Some(50),
                cache_read: Some(0),
                cache_write: Some(0),
            },
            cost: Some(Cost {
                amount: UsdMicros(2_500),
                basis: CostBasis::Reported,
            }),
        },
        at(10),
    )?;
    Delivery::live("unreported", "implementation").record(&f, &ledger)?;
    // A task without a work type is counted apart.
    f.store
        .create_task(common::spec("untyped")?, &creator()?, at(0))?;
    let lease = f
        .store
        .claim(&task_id("untyped")?, &scheduled("owner")?, ttl(60)?, at(1))?;
    f.store
        .start_attempt(&task_id("untyped")?, lease.fence(), at(2))?;

    let report = run_audit(
        &f,
        &ledger,
        &evidence(house()?, Vec::new()),
        &BTreeSet::new(),
        &AuditPolicy::default(),
    )??;
    let [record] = report.stations.as_slice() else {
        return Err(format!("expected one station record, got {:?}", report.stations).into());
    };
    let usage = record.usage;
    assert_eq!((usage.attempts, usage.reported), (2, 1));
    assert_eq!((usage.tokens, usage.token_samples), (150, 1));
    assert_eq!((usage.cost_micros, usage.cost_samples), (2_500, 1));
    assert_eq!(report.unattributed_attempts, 1);
    Ok(())
}

#[test]
fn policy_bounds_and_malformed_markers_are_refused() -> TestResult {
    for policy in [
        AuditPolicy {
            max_proposals: NonZeroU32::new(u32::try_from(MAX_PROPOSALS)? + 1).ok_or("zero")?,
            ..AuditPolicy::default()
        },
        AuditPolicy {
            high_usage_percent: 0,
            ..AuditPolicy::default()
        },
        AuditPolicy {
            divergence_points: 101,
            ..AuditPolicy::default()
        },
    ] {
        assert_eq!(policy.validate(), Err(AuditError::InvalidPolicy));
        assert_eq!(AuditError::InvalidPolicy.class(), ErrorClass::InvalidInput);
    }
    assert!(AuditPolicy::default().validate().is_ok());

    assert_eq!(proposal_key("no marker here"), None);
    assert_eq!(
        proposal_key("<!-- kitchn:brigade-audit key=bad key --> text"),
        None
    );
    assert_eq!(proposal_key("<!-- kitchn:brigade-audit key= -->"), None);
    assert_eq!(
        proposal_key("intro\n<!-- kitchn:brigade-audit key=schedule:pickup:revisit-budget -->"),
        Some(key("schedule:pickup:revisit-budget")?)
    );
    assert_eq!(ProposalKey::parse(&"a".repeat(201)), None);
    Ok(())
}
