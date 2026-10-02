//! The `kitchn tick` process contract against a temporary registry and house
//! store, including `kitchn tick settle` for a run a crash left uncertain.
//! The house has no installed instructions, so a due pickup pass runs and
//! fails before any forge read or backend call; no backend or live trigger
//! runs, so none of this is live evidence. A pass that acts is exercised in
//! `crates/kitchen/tests/run_passes.rs` against fakes.

use std::{
    error::Error,
    path::PathBuf,
    process::{Command, Output},
};

use kitchen::{
    HolderId, HouseId, TaskId,
    adoption::HouseRegistry,
    contracts::{Clock, LeaseTtl, SystemClock, Timestamp},
    house::HouseConfig,
    scheduling::IntervalMinutes,
    state::{HouseStore, RunStart, RunState, StoreOptions},
    workflows::tick::{PASS_LEASE, Pass, PassFailure, PassOutcome},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Why a pickup pass stops in a house without installed instructions.
const MISSING_INSTRUCTIONS: &str =
    "pinned instructions are missing or modified; restore the verified bundle and sync";

struct Kitchen {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Kitchen {
    /// Registers `house` with `tick` as its tick policy and creates its store.
    fn new(houses: &[(&str, Option<serde_json::Value>)]) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let registry = HouseRegistry::new(root.join("registry"))?;
        for (house, tick) in houses {
            let mut config: serde_json::Value = serde_json::from_str(include_str!(
                "../../../kitchen/tests/fixtures/house/origin89.json"
            ))?;
            config["house"] = serde_json::json!(house);
            if let Some(tick) = tick {
                config["tick"] = tick.clone();
            }
            let config: HouseConfig = serde_json::from_value(config)?;
            registry.initialize(&config)?;
            registry.initialize_store(&config.house)?;
        }
        Ok(Self { _dir: dir, root })
    }

    fn registry(&self) -> String {
        self.root.join("registry").display().to_string()
    }

    fn store(&self, house: &str) -> TestResult<HouseStore> {
        let house = HouseId::new(house)?;
        let path = HouseRegistry::new(self.root.join("registry"))?.store_path(&house)?;
        Ok(HouseStore::open(path, house, StoreOptions::default())?)
    }

    fn kitchn(&self, args: &[&str]) -> TestResult<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .env_remove("KITCHN_HOME")
            .env("HOME", &self.root)
            .args(args)
            .output()?)
    }

    fn tick(&self, house: &str, extra: &[&str]) -> TestResult<Output> {
        let registry = self.registry();
        let mut args = vec!["tick", "--registry", &registry, "--house", house];
        args.extend_from_slice(extra);
        self.kitchn(&args)
    }
}

fn pickup_every(minutes: u32) -> Option<serde_json::Value> {
    Some(serde_json::json!({"passes": {"pickup": {"everyMinutes": minutes}}}))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A pickup run a crashed tick started two hours ago, optionally after
/// recording a task.
fn crashed_pickup(kitchen: &Kitchen, task: Option<&str>) -> TestResult {
    let store = kitchen.store("origin89")?;
    let started = Timestamp::from_unix_millis(
        SystemClock
            .now()
            .as_unix_millis()
            .saturating_sub(2 * 60 * 60 * 1000),
    );
    let RunStart::Started { run, fence } = store.start_run(
        Pass::Pickup,
        IntervalMinutes::new(15)?,
        &HolderId::new("tick-crashed")?,
        LeaseTtl::new(PASS_LEASE)?,
        started,
    )?
    else {
        return Err("the crashed tick started pickup".into());
    };
    if let Some(task) = task {
        store.record_run_task(run, fence, &TaskId::new(task)?, started)?;
    }
    Ok(())
}

impl Kitchen {
    fn settle(&self, extra: &[&str]) -> TestResult<Output> {
        let registry = self.registry();
        let mut args = vec![
            "tick",
            "settle",
            "--registry",
            &registry,
            "--house",
            "origin89",
        ];
        args.extend_from_slice(extra);
        self.kitchn(&args)
    }
}

const SETTLE: [&str; 8] = [
    "--pass",
    "pickup",
    "--run",
    "0",
    "--reason",
    "checked the forge: nothing was opened",
    "--holder",
    "david",
];

#[test]
fn a_crashed_run_blocks_the_pass_until_a_person_settles_it() -> TestResult {
    let kitchen = Kitchen::new(&[("origin89", pickup_every(15))])?;
    crashed_pickup(&kitchen, Some("pickup-task"))?;

    for recorded in [", recorded now", ""] {
        let output = kitchen.tick("origin89", &[])?;
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert_eq!(
            stdout(&output).trim(),
            format!(
                "pickup: blocked: run 0 is uncertain{recorded} (0 unresolved effects on the tasks it recorded; it may have done more). Check what it did, then run `kitchn tick settle --pass pickup --run 0`."
            )
        );
    }
    let runs = kitchen.store("origin89")?.runs()?;
    assert_eq!(runs.len(), 1);
    assert!(matches!(
        runs.first().map(|run| &run.state),
        Some(RunState::Uncertain { .. })
    ));

    let settled = kitchen.settle(&SETTLE)?;
    assert_eq!(settled.status.code(), Some(0), "{}", stderr(&settled));
    assert_eq!(
        stdout(&settled).trim(),
        "pickup: settled run 0 for david: checked the forge: nothing was opened (0 unresolved effects recorded)\nThe pass runs again when due."
    );
    let registry = kitchen.registry();
    let listed = kitchen.kitchn(&[
        "tick",
        "runs",
        "--registry",
        &registry,
        "--house",
        "origin89",
    ])?;
    assert!(
        stdout(&listed).contains("uncertain, settled by david at ",)
            && stdout(&listed)
                .contains("(0 unresolved effects): checked the forge: nothing was opened"),
        "{}",
        stdout(&listed)
    );

    // Settling again changes nothing.
    let again = kitchen.settle(&SETTLE)?;
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(stdout(&again).starts_with("pickup: run 0 was already settled by david at "));

    let output = kitchen.tick("origin89", &[])?;
    assert_eq!(
        stdout(&output).trim(),
        format!("pickup: run 1: failed: {MISSING_INSTRUCTIONS}")
    );
    Ok(())
}

#[test]
fn settle_refuses_a_run_that_is_not_uncertain_and_invalid_input() -> TestResult {
    let kitchen = Kitchen::new(&[("origin89", pickup_every(15))])?;
    crashed_pickup(&kitchen, None)?;

    // Still running as far as the ledger knows: nothing to settle.
    let running = kitchen.settle(&SETTLE)?;
    assert_eq!(running.status.code(), Some(1));
    assert!(stderr(&running).contains("the run is not uncertain"));

    let mut unknown = SETTLE;
    unknown[3] = "7";
    let missing = kitchen.settle(&unknown)?;
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("no such run"));

    for (index, value) in [(1, "nightly"), (3, "run-0")] {
        let mut invalid = SETTLE;
        invalid[index] = value;
        assert_eq!(kitchen.settle(&invalid)?.status.code(), Some(2));
    }
    let without_reason = kitchen.settle(&SETTLE[..4])?;
    assert_eq!(without_reason.status.code(), Some(2));

    let runs = kitchen.store("origin89")?.runs()?;
    assert_eq!(runs.first().map(|run| &run.state), Some(&RunState::Running));
    Ok(())
}

#[test]
fn a_tick_records_each_due_pass_and_skips_it_until_due_again() -> TestResult {
    let kitchen = Kitchen::new(&[("origin89", pickup_every(15))])?;

    let first = kitchen.tick("origin89", &[])?;
    assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
    assert_eq!(
        stdout(&first).trim(),
        format!("pickup: run 0: failed: {MISSING_INSTRUCTIONS}")
    );
    let runs = kitchen.store("origin89")?.runs()?;
    assert!(matches!(
        runs.as_slice(),
        [run] if matches!(run.state, RunState::Ended {
            outcome: PassOutcome::Failed { reason: PassFailure::Execution },
            ..
        })
    ));

    let second = kitchen.tick("origin89", &[])?;
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert!(stdout(&second).starts_with("pickup: not due until "));
    assert_eq!(kitchen.store("origin89")?.runs()?.len(), 1);

    let registry = kitchen.registry();
    let listed = kitchen.kitchn(&[
        "tick",
        "runs",
        "--registry",
        &registry,
        "--house",
        "origin89",
    ])?;
    assert_eq!(listed.status.code(), Some(0));
    assert!(stdout(&listed).starts_with("run 0 pickup started "));
    Ok(())
}

#[test]
fn a_tick_refuses_a_house_without_passes_and_invalid_input() -> TestResult {
    let kitchen = Kitchen::new(&[("origin89", None)])?;
    let none = kitchen.tick("origin89", &[])?;
    assert_eq!(none.status.code(), Some(1));
    assert!(stderr(&none).contains("schedules no tick passes"));

    let invalid = kitchen.tick("not a house", &[])?;
    assert_eq!(invalid.status.code(), Some(2));
    let missing = kitchen.kitchn(&["tick"])?;
    assert_eq!(missing.status.code(), Some(2));
    assert!(kitchen.store("origin89")?.runs()?.is_empty());
    Ok(())
}

#[test]
fn a_tick_refuses_another_houses_store() -> TestResult {
    let kitchen = Kitchen::new(&[
        ("origin89", pickup_every(15)),
        ("crabnebula", pickup_every(15)),
    ])?;
    let registry = HouseRegistry::new(kitchen.root.join("registry"))?;
    let origin89_store = registry.store_path(&HouseId::new("origin89")?)?;
    let crossed = kitchen.tick(
        "crabnebula",
        &["--store", &origin89_store.display().to_string()],
    )?;
    assert_eq!(crossed.status.code(), Some(1), "{}", stderr(&crossed));
    assert!(kitchen.store("origin89")?.runs()?.is_empty());
    assert!(kitchen.store("crabnebula")?.runs()?.is_empty());
    Ok(())
}

#[test]
fn trigger_prints_a_cron_line_or_plist_and_refuses_relative_paths() -> TestResult {
    let kitchen = Kitchen::new(&[])?;
    let registry = kitchen.registry();
    let cron = kitchen.kitchn(&[
        "tick",
        "trigger",
        "cron",
        "--kitchn",
        "/usr/local/bin/kitchn",
        "--registry",
        &registry,
        "--house",
        "origin89",
        "--every-minutes",
        "10",
    ])?;
    assert_eq!(cron.status.code(), Some(0), "{}", stderr(&cron));
    assert_eq!(
        stdout(&cron).trim(),
        format!(
            "*/10 * * * * '/usr/local/bin/kitchn' 'tick' '--registry' '{registry}' '--house' 'origin89'"
        )
    );
    let plist = kitchen.kitchn(&[
        "tick",
        "trigger",
        "launchd",
        "--kitchn",
        "/usr/local/bin/kitchn",
        "--registry",
        &registry,
        "--house",
        "origin89",
    ])?;
    assert_eq!(plist.status.code(), Some(0));
    assert!(stdout(&plist).contains("<integer>300</integer>"));
    // Nothing was written: the registry has no house and no store.
    assert!(!kitchen.root.join("registry").join("origin89").exists());

    let relative = kitchen.kitchn(&[
        "tick",
        "trigger",
        "cron",
        "--kitchn",
        "kitchn",
        "--registry",
        &registry,
        "--house",
        "origin89",
    ])?;
    assert_eq!(relative.status.code(), Some(2));
    let hourly = kitchen.kitchn(&[
        "tick",
        "trigger",
        "cron",
        "--kitchn",
        "/bin/kitchn",
        "--registry",
        &registry,
        "--house",
        "origin89",
        "--every-minutes",
        "60",
    ])?;
    assert_eq!(hourly.status.code(), Some(2));
    Ok(())
}
