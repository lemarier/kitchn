//! The `kitchn audit` process contract against a temporary registry, house
//! store, and trust ledger. Orca is a fake `orca` script listing one
//! schedule, and schedule evidence files are constructed fixtures; no live
//! runtime, forge, or model is contacted.
#![cfg(unix)]

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use kitchen::{
    BackendId, ConsumerId, CredentialId, HouseId,
    adoption::HouseRegistry,
    contracts::{ExternalRef, Grant, Permission, ResourceKind, ResourceRef, Timestamp},
    house::{BackendBinding, BackendKind, HouseConfig},
    scheduling::{
        JudgedRun, ObservedScheduleState, RunOutcome, RunVerdict, ScheduleEvidence,
        ScheduleObservation, ScheduleRun, ScheduleUsage,
    },
    state::{HouseStore, StoreOptions},
    trust::{Ledger, Measurement},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const BUSY_KEY: &str = "schedule:pickup:revisit-budget";
const IDLE_KEY: &str = "schedule:gardener:reduce-idle-runs";

/// Answers the status check and lists one active schedule, `pickup`, with
/// four runs due just now.
const FAKE_ORCA: &str = r#"#!/bin/sh
dir=$(dirname "$0")
ok() { printf '{"id":"fake","ok":true,"result":%s}' "$1"; }
case "$1 $2" in
  "status --json")
    ok '{"runtime":{"state":"ready","reachable":true,"appVersion":"1.4.212","capabilities":["orchestration.contract.v1","orchestration.worker-stop-verdict.v1"]}}' ;;
  "automations list")
    ok '{"automations":[{"id":"auto-1","name":"kitchen:origin89:pickup","enabled":true}]}' ;;
  "automations runs")
    ok "{\"runs\":$(cat "$dir/runs")}" ;;
  *) exit 1 ;;
esac
"#;

struct Kitchen {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Kitchen {
    /// A house bound to the fake Orca, with a schedule policy unless
    /// `policy` is false, an empty store and ledger, and an evidence file of
    /// a mostly idle schedule observed for `evidence_house`.
    fn new(policy: bool, evidence_house: &str) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let mut house: HouseConfig = serde_json::from_str(include_str!(
            "../../kitchen/tests/fixtures/house/origin89.json"
        ))?;
        let (orca, credential) = (
            BackendId::new("orca-local")?,
            CredentialId::new("orca-host-session")?,
        );
        let grant = Grant::house(Permission::ManageSchedule, orca.clone(), credential.clone());
        house.policy_limits.insert(grant.clone());
        house.grants.insert(grant);
        house.backend = Some(BackendBinding {
            kind: BackendKind::Orca.into(),
            backend: orca,
            credential,
        });
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

        let bin = root.join("orca-bin");
        fs::create_dir(&bin)?;
        fs::create_dir(root.join("runtime"))?;
        let script = bin.join("orca");
        fs::write(&script, FAKE_ORCA)?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let runs: Vec<_> = (0..4_u64)
            .map(|index| {
                serde_json::json!({"id": format!("run-{index}"), "status": "completed",
                    "scheduledFor": now.saturating_sub(index * 1000)})
            })
            .collect();
        fs::write(bin.join("runs"), serde_json::to_string(&runs)?)?;

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

    fn path(&self, name: &str) -> TestResult<String> {
        Ok(self
            .root
            .join(name)
            .to_str()
            .ok_or("non-UTF-8 path")?
            .to_owned())
    }

    fn audit(&self, extra: &[&str]) -> TestResult<Output> {
        let mut args = vec![
            "audit".to_owned(),
            "--registry".to_owned(),
            self.path("registry")?,
            "--house".to_owned(),
            "origin89".to_owned(),
            "--store".to_owned(),
            self.path("store")?,
            "--ledger".to_owned(),
            self.path("trust")?,
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(&args)
            .output()?)
    }

    /// Audit with the schedules listed by the fake Orca.
    fn listed(&self, extra: &[&str]) -> TestResult<Output> {
        let (orca, runtime) = (self.path("orca-bin/orca")?, self.path("runtime")?);
        let mut args = vec!["--orca", &orca, "--runtime-dir", &runtime];
        args.extend(extra);
        self.audit(&args)
    }

    /// Audit with the schedule evidence file.
    fn unproven(&self, extra: &[&str]) -> TestResult<Output> {
        let evidence = self.path("evidence.json")?;
        let mut args = vec!["--schedule-evidence", &evidence];
        args.extend(extra);
        self.audit(&args)
    }
}

fn origin89() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn stdout(output: &Output) -> TestResult<String> {
    Ok(String::from_utf8(output.stdout.clone())?)
}

#[test]
fn the_listed_preview_proposes_a_draft_with_its_evidence_and_skips_an_open_one() -> TestResult {
    let kitchen = Kitchen::new(true, "origin89")?;
    let preview = kitchen.listed(&["--no-open-proposals"])?;
    assert_eq!(preview.status.code(), Some(0), "{preview:?}");
    let text = stdout(&preview)?;
    assert!(!text.contains("No drafts proposed"), "{text}");
    assert!(
        text.contains("  pickup: 4 of 4 runs this window\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "  {BUSY_KEY}: schedule change, 4 samples\n    evidence: 1\n      - schedule consumer `pickup`\n"
        )),
        "{text}"
    );
    assert!(text.contains("Stations:\n  none recorded"));
    // The backend's automation handle stays private.
    assert!(!text.contains("auto-1"), "{text}");

    let json = kitchen.listed(&["--no-open-proposals", "--json"])?;
    assert_eq!(json.status.code(), Some(0), "{json:?}");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let draft = value["drafts"][0].as_str().ok_or("no draft")?;
    assert!(draft.starts_with(&format!("<!-- kitchn:brigade-audit key={BUSY_KEY} -->")));
    assert!(draft.contains("- schedule consumer `pickup`\n"), "{draft}");
    assert_eq!(value["report"]["proposals"][0]["key"], BUSY_KEY);
    assert_eq!(value["report"]["withheld"], serde_json::json!([]));
    assert!(!String::from_utf8(json.stdout)?.contains("auto-1"));

    let open = kitchen.listed(&["--open-proposal", BUSY_KEY])?;
    assert_eq!(open.status.code(), Some(0), "{open:?}");
    let text = stdout(&open)?;
    assert!(
        text.contains(&format!("{BUSY_KEY}: already open")),
        "{text}"
    );
    assert!(!text.contains("schedule change"));
    Ok(())
}

#[test]
fn without_a_proven_budget_or_the_open_proposals_nothing_is_proposed() -> TestResult {
    let kitchen = Kitchen::new(true, "origin89")?;
    // The open set is never taken as empty.
    let unknown = kitchen.listed(&[])?;
    assert_eq!(unknown.status.code(), Some(0), "{unknown:?}");
    let text = stdout(&unknown)?;
    assert!(
        text.contains("No drafts proposed: open proposals unknown"),
        "{text}"
    );
    assert!(
        text.contains(&format!("  {BUSY_KEY}: withheld\n")),
        "{text}"
    );
    assert!(text.contains("Draft proposals:\n  none\n"), "{text}");

    // A file may omit schedules: the budget is unknown even with the open set.
    let unproven = kitchen.unproven(&["--no-open-proposals"])?;
    assert_eq!(unproven.status.code(), Some(0), "{unproven:?}");
    let text = stdout(&unproven)?;
    assert!(
        text.contains(
            "No drafts proposed: budget unknown: the schedule evidence is not the complete listing"
        ),
        "{text}"
    );
    assert!(
        text.contains("gardener: 10 of 10 recent runs idle\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!("  {IDLE_KEY}: withheld\n")),
        "{text}"
    );
    let json = kitchen.unproven(&["--no-open-proposals", "--json"])?;
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(value["drafts"], serde_json::json!([]));
    assert_eq!(
        value["report"]["withheld"],
        serde_json::json!(["unproven-inventory"])
    );
    Ok(())
}

#[test]
fn invalid_input_and_another_house_are_refused() -> TestResult {
    let kitchen = Kitchen::new(true, "origin89")?;
    let bad_key = kitchen.listed(&["--open-proposal", "not a key"])?;
    assert_eq!(bad_key.status.code(), Some(2), "{bad_key:?}");
    assert!(
        String::from_utf8(bad_key.stderr)?
            .contains("invalid house configuration or installation input")
    );
    assert!(bad_key.stdout.is_empty());

    // One schedule source is required, and an open set is either listed or
    // stated empty.
    for args in [
        &[][..],
        &["--no-open-proposals", "--open-proposal", BUSY_KEY][..],
    ] {
        let refused = kitchen.audit(args)?;
        assert_eq!(refused.status.code(), Some(2), "{refused:?}");
        assert!(refused.stdout.is_empty());
    }

    let foreign = Kitchen::new(true, "crabnebula")?;
    let refused = foreign.unproven(&[])?;
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8(refused.stderr)?.contains("reads only the house it runs for"));
    assert!(refused.stdout.is_empty());

    let unbudgeted = Kitchen::new(false, "origin89")?;
    let refused = unbudgeted.listed(&["--no-open-proposals"])?;
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(String::from_utf8(refused.stderr)?.contains("incomplete workflow evidence"));
    Ok(())
}
