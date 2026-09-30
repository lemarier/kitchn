//! The house tick against a temporary house store and a fake pass runner:
//! due and not-due passes, duplicate ticks, a crash mid-run that blocks its
//! pass until a person settles it, lease renewal within the run deadline,
//! superseded and late runs, ledger bounds and retention, cross-house
//! refusal, and the printed triggers. Effects go to the fake backend. No
//! workflow pass, real backend, or live trigger runs, so none of this is live
//! evidence.

mod common;

use std::{
    cell::Cell,
    error::Error,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use kitchen::{
    ConsumerId, HolderId, HouseId, TaskId,
    contracts::{
        Clock, ContractError, ExternalRef, Fence, LeaseTtl, Text, Timestamp, fake::ExecuteFault,
    },
    house::{HouseConfig, HouseError},
    scheduling::IntervalMinutes,
    state::{
        ConsumerState, EffectState, HouseStore, Limit, MAX_ACKNOWLEDGEMENT_REASON_BYTES,
        MAX_RUNS_PER_PASS, RunId, RunSettle, RunStart, RunState, StateError, StoreOptions,
        TokenCounts, reconcile, run_effect,
    },
    workflows::tick::{
        self, MAX_PASS_RUNTIME, MAX_RUN_EVIDENCE, MAX_RUN_TASKS, PASS_LEASE, Pass, PassFailure,
        PassOutcome, PassReport, PassRun, PassRunner, RunUsage, TickDecision, TickError,
        TriggerMinutes, TriggerTarget, trigger_cron, trigger_plist,
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const MINUTE: u64 = 60_000;
const T0: u64 = 1_800_000_000_000;

struct Fixed(Cell<u64>);

impl Fixed {
    const fn at(millis: u64) -> Self {
        Self(Cell::new(millis))
    }

    fn set(&self, millis: u64) {
        self.0.set(millis);
    }
}

impl Clock for Fixed {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.get())
    }
}

/// Records each pass it runs and reports `outcome`.
struct Recorder {
    ran: Vec<Pass>,
    outcome: PassOutcome,
}

impl Recorder {
    const fn new(outcome: PassOutcome) -> Self {
        Self {
            ran: Vec::new(),
            outcome,
        }
    }
}

impl PassRunner for Recorder {
    fn run(&mut self, pass: Pass, _run: &PassRun) -> PassReport {
        self.ran.push(pass);
        PassReport {
            outcome: self.outcome,
            usage: RunUsage::Reported {
                tokens: TokenCounts {
                    input: Some(100),
                    output: Some(20),
                    cache_read: None,
                    cache_write: None,
                },
            },
            backend_runs: ExternalRef::new("orca-run-1").into_iter().collect(),
        }
    }
}

struct House {
    _dir: tempfile::TempDir,
    store: PathBuf,
}

impl House {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let store = dir.path().canonicalize()?.join("store");
        HouseStore::initialize(&store, origin89()?, StoreOptions::default())?;
        Ok(Self { _dir: dir, store })
    }

    fn open(&self) -> TestResult<HouseStore> {
        open(&self.store)
    }
}

fn open(path: &Path) -> TestResult<HouseStore> {
    Ok(HouseStore::open(
        path,
        origin89()?,
        StoreOptions::default(),
    )?)
}

fn origin89() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn holder(name: &str) -> TestResult<HolderId> {
    Ok(HolderId::new(name)?)
}

fn every(minutes: u32) -> TestResult<IntervalMinutes> {
    Ok(IntervalMinutes::new(minutes)?)
}

fn at(millis: u64) -> Timestamp {
    Timestamp::from_unix_millis(millis)
}

/// The origin89 fixture with a tick of `passes` (pass, minutes).
fn config(passes: &[(&str, u32)]) -> TestResult<HouseConfig> {
    let mut house: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    let passes: serde_json::Map<String, serde_json::Value> = passes
        .iter()
        .map(|(pass, minutes)| {
            (
                (*pass).to_owned(),
                serde_json::json!({ "everyMinutes": minutes }),
            )
        })
        .collect();
    house["tick"] = serde_json::json!({ "passes": passes });
    let config: HouseConfig = serde_json::from_value(house)?;
    config.validate()?;
    Ok(config)
}

fn tick_error(error: &kitchen::Error) -> Option<TickError> {
    match error {
        kitchen::Error::Tick(error) => Some(*error),
        _ => None,
    }
}

fn refused<T>(result: kitchen::Result<T>) -> Option<TickError> {
    result.err().as_ref().and_then(tick_error)
}

fn tick_consumer(pass: &str) -> TestResult<ConsumerId> {
    Ok(ConsumerId::new(&format!("tick-{pass}"))?)
}

fn lease_idle(store: &HouseStore, pass: &str) -> TestResult<bool> {
    Ok(matches!(
        store
            .consumer(&tick_consumer(pass)?)?
            .map(|c| c.state().clone()),
        Some(ConsumerState::Idle)
    ))
}

fn only_pass(report: &tick::TickReport) -> TestResult<&tick::PassTick> {
    match report.passes.as_slice() {
        [pass] => Ok(pass),
        other => Err(format!("expected one pass, got {}", other.len()).into()),
    }
}

#[test]
fn a_due_pass_runs_and_a_pass_not_due_waits() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 15), ("gate", 60)])?;
    let clock = Fixed::at(T0);
    let mut runner = Recorder::new(PassOutcome::Done);

    let first = tick::tick(&store, &config, &holder("tick-a")?, &mut runner, &clock)?;
    assert!(first.healthy());
    assert_eq!(runner.ran, [Pass::Pickup, Pass::Gate]);

    clock.set(T0 + 20 * MINUTE);
    let second = tick::tick(&store, &config, &holder("tick-b")?, &mut runner, &clock)?;
    assert_eq!(runner.ran, [Pass::Pickup, Pass::Gate, Pass::Pickup]);
    let [pickup, gate] = second.passes.as_slice() else {
        return Err("two passes".into());
    };
    assert!(matches!(
        pickup.decision,
        TickDecision::Ran {
            outcome: PassOutcome::Done,
            ..
        }
    ));
    assert_eq!(
        gate.decision,
        TickDecision::NotDue {
            next_due: at(T0 + 60 * MINUTE)
        }
    );

    let runs = store.runs()?;
    assert_eq!(runs.len(), 3);
    let Some(RunState::Ended {
        ended_at,
        outcome,
        usage,
        backend_runs,
    }) = runs.last().map(|run| &run.state)
    else {
        return Err("the last run ended".into());
    };
    assert_eq!(*ended_at, at(T0 + 20 * MINUTE));
    assert_eq!(*outcome, PassOutcome::Done);
    assert!(matches!(usage, RunUsage::Reported { tokens } if tokens.input == Some(100)));
    assert_eq!(backend_runs.len(), 1);
    // Each run released its lease.
    assert!(lease_idle(&store, "pickup")?);
    Ok(())
}

#[test]
fn a_failed_pass_is_recorded_and_makes_the_tick_unhealthy() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let mut runner = Recorder::new(PassOutcome::Failed {
        reason: PassFailure::NotAvailable,
    });
    let report = tick::tick(
        &store,
        &config(&[("repair", 30)])?,
        &holder("tick-a")?,
        &mut runner,
        &Fixed::at(T0),
    )?;
    assert!(!report.healthy());
    assert!(matches!(
        store.runs()?.first().map(|run| &run.state),
        Some(RunState::Ended {
            outcome: PassOutcome::Failed {
                reason: PassFailure::NotAvailable
            },
            ..
        })
    ));
    Ok(())
}

/// Starts a second tick on its own store handle while the first tick's
/// pickup pass is running, and waits for it before returning.
struct Overlapping<'a> {
    store: &'a Path,
    config: &'a HouseConfig,
    second: Option<tick::TickReport>,
}

impl PassRunner for Overlapping<'_> {
    fn run(&mut self, _pass: Pass, _run: &PassRun) -> PassReport {
        let (sender, receiver) = mpsc::channel();
        let (store, config) = (self.store.to_path_buf(), self.config.clone());
        thread::scope(|scope| {
            scope.spawn(move || {
                let result = open(&store).and_then(|store| {
                    let mut idle = Recorder::new(PassOutcome::Idle);
                    let report = tick::tick(
                        &store,
                        &config,
                        &holder("tick-b")?,
                        &mut idle,
                        &Fixed::at(T0 + MINUTE),
                    )?;
                    if !idle.ran.is_empty() {
                        return Err("the second tick ran a pass".into());
                    }
                    Ok(report)
                });
                let _ = sender.send(result.map_err(|error| error.to_string()));
            });
        });
        self.second = receiver
            .recv_timeout(Duration::from_secs(10))
            .ok()
            .and_then(Result::ok);
        PassReport::new(PassOutcome::Done)
    }
}

#[test]
fn a_duplicate_tick_while_a_pass_runs_leaves_it_alone() -> TestResult {
    let house = House::new()?;
    let config = config(&[("pickup", 15)])?;
    let mut runner = Overlapping {
        store: &house.store,
        config: &config,
        second: None,
    };
    let first = tick::tick(
        &house.open()?,
        &config,
        &holder("tick-a")?,
        &mut runner,
        &Fixed::at(T0),
    )?;
    assert!(matches!(
        first.passes.first().map(|p| &p.decision),
        Some(TickDecision::Ran { .. })
    ));
    let second = runner.second.ok_or("the second tick finished")?;
    assert_eq!(
        second.passes.first().map(|pass| &pass.decision),
        Some(&TickDecision::Busy {
            holder: holder("tick-a")?,
            expires_at: at(T0).saturating_add(PASS_LEASE),
        })
    );
    assert_eq!(house.open()?.runs()?.len(), 1);
    Ok(())
}

/// A tick that crashed after starting `pickup`: its run stays open.
fn crashed_run(store: &HouseStore) -> TestResult<(RunId, Fence)> {
    match store.start_run(
        Pass::Pickup,
        every(15)?,
        &holder("tick-crashed")?,
        LeaseTtl::new(PASS_LEASE)?,
        at(T0),
    )? {
        RunStart::Started { run, fence } => Ok((run, fence)),
        other => Err(format!("expected a start, got {other:?}").into()),
    }
}

fn reason(text: &str) -> TestResult<Text> {
    Ok(Text::new(text)?)
}

#[test]
fn a_crash_mid_run_blocks_the_pass_across_ticks_until_a_person_settles_it() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 15)])?;
    let (crashed, fence) = crashed_run(&store)?;
    let mut runner = Recorder::new(PassOutcome::Done);

    // The lease is still live: the run may still be going.
    let clock = Fixed::at(T0 + 30 * MINUTE);
    let busy = tick::tick(&store, &config, &holder("tick-b")?, &mut runner, &clock)?;
    assert!(matches!(
        only_pass(&busy)?.decision,
        TickDecision::Busy { .. }
    ));
    assert_eq!(
        store.runs()?.first().map(|run| &run.state),
        Some(&RunState::Running)
    );

    // Past the lease: the next tick records it as uncertain and stops.
    // Every later tick, however late, stays blocked and holds no lease.
    let blocked = TickDecision::Blocked {
        run: crashed,
        unresolved_effects: 0,
    };
    for (index, minutes) in [61, 90, 24 * 60].into_iter().enumerate() {
        clock.set(T0 + minutes * MINUTE);
        let report = tick::tick(&store, &config, &holder("tick-c")?, &mut runner, &clock)?;
        assert!(!report.healthy());
        let pass = only_pass(&report)?;
        assert_eq!(pass.decision, blocked);
        assert_eq!(pass.newly_uncertain, index == 0);
        assert!(runner.ran.is_empty());
        assert!(lease_idle(&store, "pickup")?);
    }
    let uncertain = RunState::Uncertain {
        recorded_at: at(T0 + 61 * MINUTE),
    };
    assert_eq!(store.runs()?.len(), 1);
    assert_eq!(
        store.runs()?.first().map(|run| &run.state),
        Some(&uncertain)
    );

    // The crashed run was superseded: its late end is refused.
    assert_eq!(
        refused(store.finish_run(
            crashed,
            fence,
            PassReport::new(PassOutcome::Done),
            at(T0 + 62 * MINUTE),
        )),
        Some(TickError::Superseded)
    );
    assert_eq!(
        store.runs()?.first().map(|run| &run.state),
        Some(&uncertain)
    );

    // A person settles it; the record says who, when, and why.
    let settled_at = at(T0 + 25 * 60 * MINUTE);
    let settled = store.settle_run(
        Pass::Pickup,
        crashed,
        &common::interactive("david")?,
        &reason("checked the forge: nothing was opened")?,
        settled_at,
    )?;
    let expected = RunState::Settled {
        uncertain_at: at(T0 + 61 * MINUTE),
        by: holder("david")?,
        settled_at,
        reason: reason("checked the forge: nothing was opened")?,
        unresolved_effects: 0,
    };
    assert!(matches!(&settled, RunSettle::Settled(record) if record.state == expected));
    assert_eq!(store.runs()?.first().map(|run| &run.state), Some(&expected));

    // Settling again keeps the first record.
    let again = store.settle_run(
        Pass::Pickup,
        crashed,
        &common::interactive("someone-else")?,
        &reason("another reason")?,
        at(T0 + 26 * 60 * MINUTE),
    )?;
    assert!(matches!(&again, RunSettle::AlreadySettled(record) if record.state == expected));

    // The pass runs again.
    clock.set(T0 + 26 * 60 * MINUTE);
    let report = tick::tick(&store, &config, &holder("tick-d")?, &mut runner, &clock)?;
    assert!(matches!(
        only_pass(&report)?.decision,
        TickDecision::Ran { .. }
    ));
    assert_eq!(runner.ran, [Pass::Pickup]);
    Ok(())
}

#[test]
fn only_a_person_settles_an_uncertain_run_of_the_named_pass() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 15)])?;
    let (crashed, _) = crashed_run(&store)?;
    let person = common::interactive("david")?;
    let why = reason("checked")?;
    let now = at(T0 + 30 * MINUTE);

    // A running run cannot be settled.
    assert_eq!(
        refused(store.settle_run(Pass::Pickup, crashed, &person, &why, now)),
        Some(TickError::NotUncertain)
    );

    let clock = Fixed::at(T0 + 61 * MINUTE);
    tick::tick(
        &store,
        &config,
        &holder("tick-b")?,
        &mut Recorder::new(PassOutcome::Done),
        &clock,
    )?;
    let later = at(T0 + 62 * MINUTE);

    // A scheduled trigger never settles a run.
    assert_eq!(
        refused(store.settle_run(
            Pass::Pickup,
            crashed,
            &common::scheduled("tick-c")?,
            &why,
            later
        )),
        Some(TickError::SettleNeedsPerson)
    );
    // Another pass's name, or an unknown run, names nothing to settle.
    assert_eq!(
        refused(store.settle_run(Pass::Gate, crashed, &person, &why, later)),
        Some(TickError::UnknownRun)
    );
    let unknown: RunId = "999".parse()?;
    assert_eq!(
        refused(store.settle_run(Pass::Pickup, unknown, &person, &why, later)),
        Some(TickError::UnknownRun)
    );
    // The reason is bounded.
    let long = "x".repeat(MAX_ACKNOWLEDGEMENT_REASON_BYTES + 1);
    assert!(matches!(
        store.settle_run(Pass::Pickup, crashed, &person, &reason(&long)?, later),
        Err(kitchen::Error::State(StateError::CapacityExceeded {
            limit: Limit::AcknowledgementReason
        }))
    ));
    // Nothing was recorded, and the pass is still blocked.
    assert!(matches!(
        store.runs()?.first().map(|run| &run.state),
        Some(RunState::Uncertain { .. })
    ));
    let report = tick::tick(
        &store,
        &config,
        &holder("tick-d")?,
        &mut Recorder::new(PassOutcome::Done),
        &Fixed::at(T0 + 63 * MINUTE),
    )?;
    assert!(matches!(
        only_pass(&report)?.decision,
        TickDecision::Blocked { .. }
    ));
    Ok(())
}

#[test]
fn a_run_id_parses_only_from_its_number() -> TestResult {
    assert_eq!("7".parse::<RunId>()?.get(), 7);
    assert_eq!("run 7".parse::<RunId>().err(), Some(TickError::UnknownRun));
    assert_eq!("".parse::<RunId>().err(), Some(TickError::UnknownRun));
    assert_eq!("gate".parse::<Pass>()?, Pass::Gate);
    assert_eq!("Gate".parse::<Pass>().err(), Some(TickError::UnknownPass));
    Ok(())
}

#[test]
fn a_resolved_effect_still_leaves_the_run_for_a_person_to_settle() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 15)])?;
    let start = at(T0);

    // The crashed run recorded its task, then launched a worker and lost
    // the response: the launch may have happened.
    let (crashed, run_fence) = crashed_run(&store)?;
    let task: TaskId = common::task_id("pickup-task")?;
    store.record_run_task(crashed, run_fence, &task, start)?;
    store.create_task(common::spec("pickup-task")?, &common::creator()?, start)?;
    let fence = store
        .claim(
            &task,
            &common::scheduled("pickup-a")?,
            common::ttl(600)?,
            start,
        )?
        .fence();
    store.start_attempt(&task, fence, start)?;
    let backend = common::refusing()?;
    backend.inject(ExecuteFault::ApplyThenLoseResponse);
    let lost = run_effect(
        &store,
        &backend,
        &common::grants()?,
        common::plan(&task, fence, "launch", common::launch()?)?,
        &Fixed::at(T0),
    )?;
    assert!(matches!(lost.state(), EffectState::Uncertain { .. }));

    // The tick reports the unresolved effect and does not run the pass.
    let clock = Fixed::at(T0 + 61 * MINUTE);
    let mut runner = Recorder::new(PassOutcome::Done);
    let report = tick::tick(&store, &config, &holder("tick-b")?, &mut runner, &clock)?;
    assert_eq!(
        only_pass(&report)?.decision,
        TickDecision::Blocked {
            run: crashed,
            unresolved_effects: 1
        }
    );

    // Resolving the effect proves the launch applied, but the run may have
    // acted beyond its recorded task, so the pass stays blocked.
    let lease = store.take_over(
        &task,
        &common::scheduled("pickup-review")?,
        common::ttl(600)?,
        clock.now(),
    )?;
    let resolved = reconcile(&store, &backend, &task, lease.fence(), &clock)?;
    assert!(resolved.unresolved.is_empty());
    clock.set(T0 + 75 * MINUTE);
    let report = tick::tick(&store, &config, &holder("tick-c")?, &mut runner, &clock)?;
    assert_eq!(
        only_pass(&report)?.decision,
        TickDecision::Blocked {
            run: crashed,
            unresolved_effects: 0
        }
    );
    assert!(runner.ran.is_empty());

    // A person settles it; the pass runs once and never relaunches.
    let settled = store.settle_run(
        Pass::Pickup,
        crashed,
        &common::interactive("david")?,
        &reason("the worker launch applied; nothing else ran")?,
        clock.now(),
    )?;
    assert!(matches!(settled, RunSettle::Settled(_)));
    clock.set(T0 + 76 * MINUTE);
    tick::tick(&store, &config, &holder("tick-d")?, &mut runner, &clock)?;
    assert_eq!(runner.ran, [Pass::Pickup]);
    assert_eq!(backend.execute_calls(), 1);
    Ok(())
}

/// A pass that runs past [`PASS_LEASE`]. With `renew`, it renews its run
/// before the lease lapses; either way, a second tick on its own store
/// handle fires once the original lease would have expired.
struct Slow<'a> {
    store: &'a Path,
    config: &'a HouseConfig,
    renew: bool,
    renewed: Option<Timestamp>,
    after: Vec<Result<(), Option<TickError>>>,
    second: Option<tick::TickReport>,
}

impl PassRunner for Slow<'_> {
    fn run(&mut self, _pass: Pass, run: &PassRun) -> PassReport {
        let own = open(self.store);
        if self.renew
            && let Ok(store) = &own
        {
            self.renewed = store
                .renew_run(
                    run.run,
                    run.fence,
                    LeaseTtl::new(PASS_LEASE).unwrap_or_else(|_| unreachable!()),
                    at(T0 + 50 * MINUTE),
                )
                .ok();
        }
        let (sender, receiver) = mpsc::channel();
        let (path, config) = (self.store.to_path_buf(), self.config.clone());
        thread::scope(|scope| {
            scope.spawn(move || {
                let result = open(&path).and_then(|store| {
                    let mut idle = Recorder::new(PassOutcome::Idle);
                    let report = tick::tick(
                        &store,
                        &config,
                        &holder("tick-b")?,
                        &mut idle,
                        &Fixed::at(T0 + 70 * MINUTE),
                    )?;
                    if !idle.ran.is_empty() {
                        return Err("the second tick ran a pass".into());
                    }
                    Ok(report)
                });
                let _ = sender.send(result.map_err(|error| error.to_string()));
            });
        });
        match receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(report)) => self.second = Some(report),
            Ok(Err(error)) => eprintln!("second tick: {error}"),
            Err(error) => eprintln!("second tick: {error}"),
        }
        // What the first run may still do after the second tick.
        if let Ok(store) = &own {
            let now = at(T0 + 71 * MINUTE);
            let error = |result: kitchen::Result<()>| result.map_err(|e| tick_error(&e));
            self.after = vec![
                error(
                    store
                        .renew_run(
                            run.run,
                            run.fence,
                            LeaseTtl::new(PASS_LEASE).unwrap_or_else(|_| unreachable!()),
                            now,
                        )
                        .map(|_| ()),
                ),
                error(store.record_run_task(
                    run.run,
                    run.fence,
                    &TaskId::new("pickup-late").unwrap_or_else(|_| unreachable!()),
                    now,
                )),
            ];
        }
        PassReport::new(PassOutcome::Done)
    }
}

#[test]
fn a_slow_run_that_renews_is_not_overtaken() -> TestResult {
    let house = House::new()?;
    let config = config(&[("pickup", 15)])?;
    let mut runner = Slow {
        store: &house.store,
        config: &config,
        renew: true,
        renewed: None,
        after: Vec::new(),
        second: None,
    };
    let clock = Fixed::at(T0);
    let first = tick::tick(
        &house.open()?,
        &config,
        &holder("tick-a")?,
        &mut runner,
        &clock,
    )?;
    let renewed = at(T0 + 50 * MINUTE).saturating_add(PASS_LEASE);
    assert_eq!(runner.renewed, Some(renewed));
    let second = runner.second.ok_or("the second tick finished")?;
    assert_eq!(
        second.passes.first().map(|pass| &pass.decision),
        Some(&TickDecision::Busy {
            holder: holder("tick-a")?,
            expires_at: renewed,
        })
    );
    // The live run could still act, and its end was recorded.
    assert_eq!(runner.after, [Ok(()), Ok(())]);
    assert!(matches!(
        first.passes.first().map(|p| &p.decision),
        Some(TickDecision::Ran {
            outcome: PassOutcome::Done,
            ..
        })
    ));
    let runs = house.open()?.runs()?;
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs.first().map(|run| run.tasks.clone()),
        Some(vec![TaskId::new("pickup-late")?])
    );
    Ok(())
}

#[test]
fn a_slow_run_that_does_not_renew_is_superseded_and_fenced() -> TestResult {
    let house = House::new()?;
    let config = config(&[("pickup", 15)])?;
    let mut runner = Slow {
        store: &house.store,
        config: &config,
        renew: false,
        renewed: None,
        after: Vec::new(),
        second: None,
    };
    let first = tick::tick(
        &house.open()?,
        &config,
        &holder("tick-a")?,
        &mut runner,
        &Fixed::at(T0),
    )?;
    let second = runner.second.ok_or("the second tick finished")?;
    let run = match first.passes.first().map(|p| &p.decision) {
        Some(TickDecision::Superseded { run }) => *run,
        other => return Err(format!("expected a superseded run, got {other:?}").into()),
    };
    // The second tick took the lease over, recorded the run as uncertain,
    // and blocked the pass.
    let pass = only_pass(&second)?;
    assert!(pass.newly_uncertain);
    assert_eq!(
        pass.decision,
        TickDecision::Blocked {
            run,
            unresolved_effects: 0
        }
    );
    // The overtaken run can neither renew, record a task, nor end.
    assert_eq!(
        runner.after,
        [
            Err(Some(TickError::Superseded)),
            Err(Some(TickError::Superseded))
        ]
    );
    assert!(!first.healthy());
    let runs = house.open()?.runs()?;
    assert_eq!(runs.len(), 1);
    assert!(matches!(
        runs.first().map(|run| &run.state),
        Some(RunState::Uncertain { .. })
    ));
    assert!(runs.first().is_some_and(|run| run.tasks.is_empty()));
    Ok(())
}

#[test]
fn a_late_end_after_the_lease_lapsed_is_refused_and_changes_nothing() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 15)])?;
    let (run, fence) = crashed_run(&store)?;
    // The lease lapsed, but no tick has taken it over yet.
    let late = at(T0).saturating_add(PASS_LEASE);
    assert_eq!(
        refused(store.finish_run(run, fence, PassReport::new(PassOutcome::Done), late)),
        Some(TickError::Superseded)
    );
    assert_eq!(
        refused(store.record_run_task(run, fence, &TaskId::new("late")?, late)),
        Some(TickError::Superseded)
    );
    assert_eq!(
        store.runs()?.first().map(|record| &record.state),
        Some(&RunState::Running)
    );
    // The next tick records it as uncertain, not as ended.
    let report = tick::tick(
        &store,
        &config,
        &holder("tick-b")?,
        &mut Recorder::new(PassOutcome::Done),
        &Fixed::at(T0 + 61 * MINUTE),
    )?;
    assert_eq!(
        only_pass(&report)?.decision,
        TickDecision::Blocked {
            run,
            unresolved_effects: 0
        }
    );
    Ok(())
}

#[test]
fn a_blocked_pass_waits_for_its_interval_after_it_is_settled() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 120)])?;
    let (crashed, _) = crashed_run(&store)?;
    let mut runner = Recorder::new(PassOutcome::Done);
    let clock = Fixed::at(T0 + 61 * MINUTE);

    tick::tick(&store, &config, &holder("tick-b")?, &mut runner, &clock)?;
    let _ = store.settle_run(
        Pass::Pickup,
        crashed,
        &common::interactive("david")?,
        &reason("nothing ran")?,
        clock.now(),
    )?;
    clock.set(T0 + 62 * MINUTE);
    let report = tick::tick(&store, &config, &holder("tick-c")?, &mut runner, &clock)?;
    assert_eq!(
        only_pass(&report)?.decision,
        TickDecision::NotDue {
            next_due: at(T0 + 120 * MINUTE)
        }
    );
    assert!(runner.ran.is_empty());
    assert!(lease_idle(&store, "pickup")?);

    clock.set(T0 + 121 * MINUTE);
    tick::tick(&store, &config, &holder("tick-d")?, &mut runner, &clock)?;
    assert_eq!(runner.ran, [Pass::Pickup]);
    Ok(())
}

#[test]
fn the_ledger_keeps_the_newest_runs_per_pass_and_drops_expired_ones() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let config = config(&[("pickup", 15), ("gate", 15)])?;
    let mut runner = Recorder::new(PassOutcome::Idle);
    let clock = Fixed::at(T0);
    let ticks = u64::try_from(MAX_RUNS_PER_PASS)? + 6;
    for index in 0..ticks {
        clock.set(T0 + index * 15 * MINUTE);
        tick::tick(&store, &config, &holder("tick")?, &mut runner, &clock)?;
    }
    let runs = store.runs()?;
    for pass in [Pass::Pickup, Pass::Gate] {
        let kept: Vec<_> = runs.iter().filter(|run| run.pass == pass).collect();
        assert_eq!(kept.len(), MAX_RUNS_PER_PASS);
        // The oldest six runs of the pass went; the newest stayed.
        assert_eq!(
            kept.first().map(|run| run.started_at),
            Some(at(T0 + 6 * 15 * MINUTE))
        );
    }
    let last_id = runs.last().map(|run| run.id.get()).ok_or("runs")?;

    // Thirty days after the last run, every ended run expires; ids go on.
    clock.set(T0 + ticks * 15 * MINUTE + 31 * 24 * 60 * MINUTE);
    tick::tick(&store, &config, &holder("tick")?, &mut runner, &clock)?;
    let runs = store.runs()?;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.id.get() > last_id));
    Ok(())
}

#[test]
fn recording_an_end_refuses_other_owners_unknown_runs_and_conflicts() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let (run, fence) = crashed_run(&store)?;
    let other_fence = match store.start_run(
        Pass::Gate,
        every(15)?,
        &holder("tick-other")?,
        LeaseTtl::new(PASS_LEASE)?,
        at(T0),
    )? {
        RunStart::Started { fence, .. } => fence,
        other => return Err(format!("expected a start, got {other:?}").into()),
    };
    let now = at(T0 + MINUTE);
    let done = || PassReport::new(PassOutcome::Done);

    assert_eq!(
        refused(store.finish_run(run, other_fence, done(), now)),
        Some(TickError::NotRunOwner)
    );
    let mut flooded = done();
    flooded.backend_runs = (0..=MAX_RUN_EVIDENCE)
        .map(|index| ExternalRef::new(&format!("run-{index}")))
        .collect::<Result<_, _>>()?;
    assert_eq!(
        refused(store.finish_run(run, fence, flooded, now)),
        Some(TickError::TooMuchEvidence {
            max: MAX_RUN_EVIDENCE
        })
    );
    assert_eq!(
        store.runs()?.first().map(|r| &r.state),
        Some(&RunState::Running)
    );

    store.finish_run(run, fence, done(), now)?;
    // Repeating the same end is a no-op; a different one conflicts.
    store.finish_run(run, fence, done(), now)?;
    assert_eq!(
        refused(store.finish_run(run, fence, PassReport::new(PassOutcome::Idle), now)),
        Some(TickError::AlreadyFinished)
    );
    // An ended run is not uncertain, so there is nothing to settle.
    assert_eq!(
        refused(store.settle_run(
            Pass::Pickup,
            run,
            &common::interactive("david")?,
            &reason("done")?,
            now
        )),
        Some(TickError::NotUncertain)
    );
    let unknown: RunId = serde_json::from_value(serde_json::json!(999))?;
    assert_eq!(
        refused(store.finish_run(unknown, fence, done(), now)),
        Some(TickError::UnknownRun)
    );
    Ok(())
}

#[test]
fn a_live_run_records_bounded_tasks_and_renews_up_to_its_deadline() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let (run, fence) = crashed_run(&store)?;
    let now = at(T0 + MINUTE);

    for index in 0..MAX_RUN_TASKS {
        store.record_run_task(run, fence, &TaskId::new(&format!("task-{index}"))?, now)?;
    }
    // Recording a task twice is a no-op; one more is refused.
    store.record_run_task(run, fence, &TaskId::new("task-0")?, now)?;
    assert_eq!(
        refused(store.record_run_task(run, fence, &TaskId::new("task-extra")?, now)),
        Some(TickError::TooManyTasks { max: MAX_RUN_TASKS })
    );
    assert_eq!(
        store.runs()?.first().map(|record| record.tasks.len()),
        Some(MAX_RUN_TASKS)
    );

    // Each renewal extends the lease by its ttl, but never past the run's
    // deadline.
    let ttl = LeaseTtl::new(PASS_LEASE)?;
    let deadline = at(T0).saturating_add(MAX_PASS_RUNTIME);
    let mut renewed_at = at(T0);
    while renewed_at.saturating_add(PASS_LEASE / 2) < deadline {
        renewed_at = renewed_at.saturating_add(PASS_LEASE / 2);
        assert_eq!(
            store.renew_run(run, fence, ttl, renewed_at)?,
            renewed_at.saturating_add(PASS_LEASE).min(deadline)
        );
    }
    // The renewal at five and a half hours is clamped to six.
    assert_eq!(
        store.renew_run(run, fence, ttl, at(T0 + 330 * MINUTE))?,
        deadline
    );
    // The last millisecond before the deadline still renews, to the deadline.
    let last = at(T0 + 360 * MINUTE - 1);
    assert_eq!(store.renew_run(run, fence, ttl, last)?, deadline);
    // At the deadline the lease has lapsed: nothing more is accepted, and
    // the run stays running for the next tick to record as uncertain.
    assert_eq!(
        refused(store.renew_run(run, fence, ttl, deadline)),
        Some(TickError::Superseded)
    );
    assert_eq!(
        refused(store.record_run_task(run, fence, &TaskId::new("task-late")?, deadline)),
        Some(TickError::Superseded)
    );
    assert_eq!(
        refused(store.finish_run(run, fence, PassReport::new(PassOutcome::Done), deadline)),
        Some(TickError::Superseded)
    );
    assert_eq!(
        store.runs()?.first().map(|record| &record.state),
        Some(&RunState::Running)
    );
    Ok(())
}

#[test]
fn a_run_may_finish_just_before_its_deadline() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let (run, fence) = crashed_run(&store)?;
    let ttl = LeaseTtl::new(PASS_LEASE)?;
    let deadline = at(T0).saturating_add(MAX_PASS_RUNTIME);
    for step in 1..=6 {
        store.renew_run(run, fence, ttl, at(T0 + step * 50 * MINUTE))?;
    }
    assert_eq!(
        store.renew_run(run, fence, ttl, at(T0 + 330 * MINUTE))?,
        deadline
    );
    let last = at(T0 + 360 * MINUTE - 1);
    store.finish_run(run, fence, PassReport::new(PassOutcome::Done), last)?;
    assert!(matches!(
        store.runs()?.first().map(|record| &record.state),
        Some(RunState::Ended { ended_at, .. }) if *ended_at == last
    ));
    assert!(lease_idle(&store, "pickup")?);
    assert_eq!(
        refused(store.renew_run(run, fence, ttl, deadline)),
        Some(TickError::AlreadyFinished)
    );
    Ok(())
}

#[test]
fn a_tick_refuses_another_houses_store_and_a_house_without_passes() -> TestResult {
    let house = House::new()?;
    let store = house.open()?;
    let mut other = config(&[("pickup", 15)])?;
    other.house = HouseId::new("crabnebula")?;
    let mut runner = Recorder::new(PassOutcome::Done);
    let clock = Fixed::at(T0);

    let error = tick::tick(&store, &other, &holder("tick")?, &mut runner, &clock)
        .err()
        .ok_or("refused")?;
    assert_eq!(tick_error(&error), Some(TickError::CrossHouse));
    assert!(runner.ran.is_empty());
    assert!(store.runs()?.is_empty());

    // The store itself refuses to open for another house.
    let opened = HouseStore::open(
        &house.store,
        HouseId::new("crabnebula")?,
        StoreOptions::default(),
    );
    assert!(matches!(
        opened.err(),
        Some(kitchen::Error::Contract(ContractError::CrossHouse { .. }))
    ));

    let error = tick::tick(&store, &config(&[])?, &holder("tick")?, &mut runner, &clock)
        .err()
        .ok_or("refused")?;
    assert_eq!(tick_error(&error), Some(TickError::NoPasses));
    Ok(())
}

#[test]
fn house_config_validates_tick_passes() -> TestResult {
    let tick = config(&[("pickup", 15), ("coordinate", 5)])?;
    let policy = tick.tick.as_ref().ok_or("tick")?;
    assert_eq!(
        policy.passes.keys().copied().collect::<Vec<_>>(),
        [Pass::Pickup, Pass::Coordinate]
    );

    // An interval below the house schedule minimum relaxes house policy.
    let mut relaxed = tick.clone();
    relaxed.schedules = Some(serde_json::from_value(serde_json::json!({
        "windowHours": 24,
        "minIntervalMinutes": 10,
        "houseBudget": {"runs": 10},
        "scheduleBudget": {"runs": 4},
    }))?);
    assert!(matches!(
        relaxed.validate(),
        Err(HouseError::PolicyRelaxation)
    ));

    assert!(config(&[("deploy", 15)]).is_err());
    assert!(config(&[("pickup", 0)]).is_err());
    Ok(())
}

#[test]
fn triggers_print_the_tick_command_and_refuse_unsafe_paths() -> TestResult {
    let target = TriggerTarget::new(
        Path::new("/usr/local/bin/kitchn"),
        Path::new("/Users/me/it's 100% <mine>"),
        origin89()?,
        TriggerMinutes::new(5)?,
    )?;
    assert_eq!(
        trigger_cron(&target),
        r"*/5 * * * * '/usr/local/bin/kitchn' 'tick' '--registry' '/Users/me/it'\''s 100\% <mine>' '--house' 'origin89'"
    );
    let plist = trigger_plist(&target);
    assert!(plist.contains("<string>com.getkitchn.tick.origin89</string>"));
    assert!(plist.contains("    <string>/Users/me/it&apos;s 100% &lt;mine&gt;</string>\n"));
    assert!(plist.contains("<integer>300</integer>"));

    let relative = TriggerTarget::new(
        Path::new("kitchn"),
        Path::new("/registry"),
        origin89()?,
        TriggerMinutes::new(5)?,
    );
    assert_eq!(relative.err(), Some(TickError::TriggerPath));
    let control = TriggerTarget::new(
        Path::new("/bin/kitchn"),
        Path::new("/registry\n* * * * * rm"),
        origin89()?,
        TriggerMinutes::new(5)?,
    );
    assert_eq!(control.err(), Some(TickError::TriggerPath));
    assert_eq!(
        TriggerMinutes::new(0).err(),
        Some(TickError::TriggerInterval)
    );
    assert_eq!(
        TriggerMinutes::new(60).err(),
        Some(TickError::TriggerInterval)
    );
    assert!(TriggerMinutes::new(59).is_ok());
    Ok(())
}
