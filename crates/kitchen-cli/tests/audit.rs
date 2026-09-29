//! The `kitchn audit` process contract against a temporary registry, house
//! store, and trust ledger. Schedule evidence is a constructed fixture; no
//! runtime, forge, or model is contacted.

use std::{
    error::Error,
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use kitchen::{
    BackendId, ConsumerId, HouseId,
    adoption::HouseRegistry,
    contracts::{ExternalRef, ResourceKind, ResourceRef, Timestamp},
    house::HouseConfig,
    scheduling::{
        JudgedRun, ObservedScheduleState, RunOutcome, RunVerdict, ScheduleEvidence,
        ScheduleObservation, ScheduleRun, ScheduleUsage,
    },
    state::{HouseStore, StoreOptions},
    trust::{Ledger, Measurement},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const IDLE_KEY: &str = "schedule:gardener:reduce-idle-runs";

struct Kitchen {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Kitchen {
    /// A house with a schedule policy unless `policy` is false, an empty
    /// store and ledger, and evidence of a mostly idle schedule observed for
    /// `evidence_house`.
    fn new(policy: bool, evidence_house: &str) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let mut house: HouseConfig = serde_json::from_str(include_str!(
            "../../kitchen/tests/fixtures/house/origin89.json"
        ))?;
        if policy {
            house.schedules = Some(serde_json::from_value(serde_json::json!({
                "windowHours": 24,
                "minIntervalMinutes": 60,
                "houseBudget": {"runs": 10},
                "scheduleBudget": {"runs": 4},
            }))?);
        }
        HouseRegistry::new(root.join("registry"))?.initialize(&house)?;
        HouseStore::initialize(root.join("store"), origin89()?, StoreOptions::default())?;
        Ledger::initialize(root.join("trust"), origin89()?)?;
        let runs = (0..10)
            .map(|n| JudgedRun {
                run: ScheduleRun {
                    outcome: RunOutcome::PrecheckIdle,
                    scheduled_for: Some(Timestamp::from_unix_millis(1_000 * (n + 1))),
                    created_at: None,
                    usage: Measurement::Missing,
                    agent: None,
                },
                verdict: RunVerdict::Idle,
            })
            .collect();
        let evidence = ScheduleEvidence {
            house: HouseId::new(evidence_house)?,
            observed_at: Timestamp::from_unix_millis(60_000),
            schedules: vec![ScheduleUsage {
                consumer: ConsumerId::new("gardener")?,
                schedule: ResourceRef {
                    kind: ResourceKind::Schedule,
                    backend: BackendId::new("orca-local")?,
                    handle: ExternalRef::new("orca-automation:gardener")?,
                },
                observation: ScheduleObservation {
                    state: ObservedScheduleState::Active,
                    recent_runs: runs,
                },
            }],
        };
        fs::write(root.join("evidence.json"), serde_json::to_vec(&evidence)?)?;
        Ok(Self { _dir: dir, root })
    }

    fn audit(&self, extra: &[&str]) -> TestResult<Output> {
        let path = |name: &str| -> TestResult<String> {
            Ok(self
                .root
                .join(name)
                .to_str()
                .ok_or("non-UTF-8 path")?
                .to_owned())
        };
        let mut args = vec![
            "audit".to_owned(),
            "--registry".to_owned(),
            path("registry")?,
            "--house".to_owned(),
            "origin89".to_owned(),
            "--store".to_owned(),
            path("store")?,
            "--ledger".to_owned(),
            path("trust")?,
            "--schedule-evidence".to_owned(),
            path("evidence.json")?,
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(&args)
            .output()?)
    }
}

fn origin89() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn stdout(output: &Output) -> TestResult<String> {
    Ok(String::from_utf8(output.stdout.clone())?)
}

#[test]
fn the_preview_lists_a_draft_and_skips_an_open_one() -> TestResult {
    let kitchen = Kitchen::new(true, "origin89")?;
    let preview = kitchen.audit(&[])?;
    assert_eq!(preview.status.code(), Some(0), "{preview:?}");
    let text = stdout(&preview)?;
    assert!(text.contains("gardener: 10 of 10 recent runs idle (orca-automation:gardener)"));
    assert!(text.contains(&format!(
        "{IDLE_KEY}: schedule change, 10 samples, 1 evidence links"
    )));
    assert!(text.contains("Stations:\n  none recorded"));

    let json = kitchen.audit(&["--json"])?;
    assert_eq!(json.status.code(), Some(0), "{json:?}");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let draft = value["drafts"][0].as_str().ok_or("no draft")?;
    assert!(draft.starts_with(&format!("<!-- kitchn:brigade-audit key={IDLE_KEY} -->")));
    assert_eq!(value["report"]["proposals"][0]["key"], IDLE_KEY);

    let open = kitchen.audit(&["--open-proposal", IDLE_KEY])?;
    assert_eq!(open.status.code(), Some(0), "{open:?}");
    let text = stdout(&open)?;
    assert!(text.contains(&format!("{IDLE_KEY}: already open")));
    assert!(!text.contains("schedule change"));
    Ok(())
}

#[test]
fn invalid_input_and_another_house_are_refused() -> TestResult {
    let kitchen = Kitchen::new(true, "origin89")?;
    let bad_key = kitchen.audit(&["--open-proposal", "not a key"])?;
    assert_eq!(bad_key.status.code(), Some(2), "{bad_key:?}");
    assert!(
        String::from_utf8(bad_key.stderr)?
            .contains("invalid house configuration or installation input")
    );
    assert!(bad_key.stdout.is_empty());

    let foreign = Kitchen::new(true, "crabnebula")?;
    let refused = foreign.audit(&[])?;
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8(refused.stderr)?.contains("reads only the house it runs for"));
    assert!(refused.stdout.is_empty());

    let unbudgeted = Kitchen::new(false, "origin89")?;
    let refused = unbudgeted.audit(&[])?;
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(String::from_utf8(refused.stderr)?.contains("incomplete workflow evidence"));
    Ok(())
}
