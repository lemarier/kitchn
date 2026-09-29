//! `kitchn decompose preview` through the real binary. Reads only a
//! proposal file in a disposable directory; no forge is contacted.

use std::{fs, path::PathBuf, process::Command};

use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn preview(proposal: &Value, json_output: bool) -> TestResult<Run> {
    let dir = tempfile::tempdir()?;
    let path: PathBuf = dir.path().canonicalize()?.join("proposal.json");
    fs::write(&path, serde_json::to_vec(proposal)?)?;
    let path = path.to_str().ok_or("utf-8 path")?;
    let mut args = vec!["decompose", "preview", "--proposal", path];
    if json_output {
        args.push("--json");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .args(&args)
        .output()?;
    Ok(Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

fn issue(key: &str, path: &str, blocked_by: &[&str]) -> Value {
    json!({
        "key": key,
        "title": format!("Build {key}"),
        "outcome": format!("{key} works"),
        "ownedPaths": [path],
        "acceptance": [format!("{key} is tested")],
        "blockedBy": blocked_by.iter().map(|b| json!({"proposed": b})).collect::<Vec<_>>(),
    })
}

fn proposal(issues: Vec<Value>) -> Value {
    json!({"repository": "sample/project", "parent": 1, "issues": issues})
}

#[test]
fn a_ready_preview_prints_the_digest_to_approve() -> TestResult {
    let ready = proposal(vec![
        issue("api", "crates/api", &["core"]),
        issue("core", "crates/core", &[]),
    ]);
    let run = preview(&ready, false)?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stdout.contains("1. [core] Build core"));
    assert!(run.stdout.contains("2. [api] Build api"));
    assert!(
        run.stdout
            .contains("Writes: 2 issues, 2 sub-issue links, 1 blocked-by links")
    );

    let json_run = preview(&ready, true)?;
    assert_eq!(json_run.code, Some(0), "{}", json_run.stderr);
    let parsed: Value = serde_json::from_str(&json_run.stdout)?;
    let digest = parsed["digest"].as_str().ok_or("digest")?;
    assert!(digest.starts_with("sha256:") && digest.len() == 71);
    assert!(run.stdout.contains(digest), "text and JSON show one digest");
    Ok(())
}

#[test]
fn unordered_overlaps_exit_one() -> TestResult {
    let run = preview(
        &proposal(vec![
            issue("state", "crates/state", &[]),
            issue("store", "crates/state/store.rs", &[]),
        ]),
        false,
    )?;
    assert_eq!(run.code, Some(1));
    assert!(run.stdout.contains("state and store: UNORDERED"));
    assert!(run.stdout.contains("Not ready"));
    Ok(())
}

#[test]
fn a_cycle_or_malformed_proposal_exits_two() -> TestResult {
    let cycle = preview(
        &proposal(vec![issue("a", "a", &["b"]), issue("b", "b", &["a"])]),
        false,
    )?;
    assert_eq!(cycle.code, Some(2));
    assert!(cycle.stderr.contains("dependency cycle: a -> b -> a"));
    assert!(cycle.stdout.is_empty());

    let escaping = preview(&proposal(vec![issue("a", "../outside", &[])]), false)?;
    assert_eq!(escaping.code, Some(2));
    Ok(())
}

fn acknowledge(store: &str, task: &str, reason: &str) -> TestResult<Run> {
    acknowledge_with(store, task, reason, &[])
}

fn acknowledge_with(store: &str, task: &str, reason: &str, extra: &[&str]) -> TestResult<Run> {
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args([
            "decompose",
            "acknowledge",
            "--store",
            store,
            "--house",
            "origin89",
            "--task",
            task,
            "--holder",
            "owner-session",
            "--reason",
            reason,
        ])
        .args(extra)
        .output()?;
    Ok(Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

#[test]
fn acknowledging_a_task_that_holds_nothing_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?.join("house");
    kitchen::state::HouseStore::initialize(
        &root,
        kitchen::HouseId::new("origin89")?,
        kitchen::state::StoreOptions::default(),
    )?;
    let store = root.to_str().ok_or("utf-8 path")?;

    let run = acknowledge(
        store,
        "decompose-0123456789abcdef0123456789abcdef",
        "checked",
    )?;
    assert_ne!(run.code, Some(0), "{}", run.stdout);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
    assert!(!run.stderr.is_empty());
    Ok(())
}

#[test]
fn acknowledging_needs_a_reason() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?.join("house");
    kitchen::state::HouseStore::initialize(
        &root,
        kitchen::HouseId::new("origin89")?,
        kitchen::state::StoreOptions::default(),
    )?;
    let store = root.to_str().ok_or("utf-8 path")?;

    let run = acknowledge(store, "decompose-0123456789abcdef0123456789abcdef", "")?;
    assert_ne!(run.code, Some(0));
    assert!(!run.stderr.is_empty());
    Ok(())
}

const HELD_TASK: &str = "decompose-0123456789abcdef0123456789abcdef";

/// A store holding a decomposition task that settled after one write,
/// without success. `applied` records the write's receipt; otherwise its
/// outcome stays unknown, as after a lost response whose risk was accepted.
/// Built through the store directly: nothing reaches a forge.
fn held_store(root: &std::path::Path, applied: bool) -> TestResult<String> {
    use std::time::Duration;

    use kitchen::{
        BackendId, CredentialId, EffectName, HolderId, HouseId, TaskId,
        contracts::{
            AttemptOutcome, Capability, CapabilitySet, Claimant, CommitId, EvidenceRevision,
            ExternalRef, FailureClass, Grant, HouseGrants, LeaseTtl, Operation, Permission,
            Provenance, Receipt, RetryPolicy, Role, TaskAuthority, TaskSpec, Text, Timestamp,
            Workspace, fake::FakeBackend,
        },
        state::{
            EffectOutcome, EffectPlan, EffectStart, HouseStore, RiskAction, RiskDecision,
            StoreOptions,
        },
    };

    let house = HouseId::new("origin89")?;
    let root = root.join("house");
    let store = HouseStore::initialize(&root, house.clone(), StoreOptions::default())?;
    let grants = HouseGrants::new(
        house.clone(),
        vec![Grant::house(
            Permission::LaunchWorker,
            BackendId::new("fake")?,
            CredentialId::new("origin89-orca")?,
        )],
    );
    let commit = |fill: &str| CommitId::new(&fill.repeat(40));
    let task = TaskId::new(HELD_TASK)?;
    let spec = TaskSpec {
        id: task.clone(),
        role: Role::StationCook,
        repository: None,
        authority: TaskAuthority::delegate(
            &grants,
            vec![Grant::house(
                Permission::LaunchWorker,
                BackendId::new("fake")?,
                CredentialId::new("origin89-orca")?,
            )],
        )?,
        retry: RetryPolicy::new(1, Duration::from_secs(3600))?,
        provenance: Provenance {
            kitchen: commit("a")?,
            house_guidance: commit("b")?,
            repository_instructions: None,
        },
        resources: std::collections::BTreeSet::new(),
        requires: kitchen::contracts::CapabilityRequirements::new(),
        agent: None,
    };
    let at = |seconds: u64| Timestamp::from_unix_millis(seconds * 1000);
    let person = HolderId::new("operator")?;
    store.create_task(spec, &Claimant::scheduled(HolderId::new("pickup")?), at(0))?;
    let lease = store.claim(
        &task,
        &Claimant::scheduled(HolderId::new("coordinator")?),
        LeaseTtl::new(Duration::from_secs(600))?,
        at(1),
    )?;
    let fence = lease.fence();
    store.start_attempt(&task, fence, at(2))?;
    let backend = FakeBackend::new(
        BackendId::new("fake")?,
        house,
        CapabilitySet::supporting([Capability::WorkerLaunchIsolated, Capability::EffectLookup]),
    );
    let EffectStart::Execute(intent) = store.begin_effect(
        EffectPlan {
            task: task.clone(),
            fence,
            name: EffectName::new("issue-core")?,
            decided_at: EvidenceRevision::INITIAL,
            effect: Operation::LaunchWorker {
                role: Role::StationCook,
                workspace: Workspace::Isolated,
                brief: Text::new("Create the core issue.")?,
                branch: None,
                agent: None,
            }
            .into(),
            consent: None,
            basis: None,
        },
        &grants,
        &backend,
        at(3),
    )?
    else {
        return Err("expected a new effect".into());
    };
    if applied {
        let receipt = Receipt::new(
            ExternalRef::new("https://github.com/sample/project/issues/1")?,
            Vec::new(),
            Vec::new(),
        )?;
        store.record_effect_outcome(
            &task,
            fence,
            intent.seq(),
            EffectOutcome::Applied(receipt),
            at(4),
        )?;
        store.finish_attempt(
            &task,
            fence,
            kitchen::contracts::AttemptNumber::FIRST,
            AttemptOutcome::Failed(FailureClass::Permanent),
            at(5),
        )?;
    } else {
        store.record_effect_outcome(
            &task,
            fence,
            intent.seq(),
            EffectOutcome::Unresolvable,
            at(4),
        )?;
        let decision = RiskDecision {
            effect: intent.request().key().clone(),
            decided_by: person.clone(),
            revision: store.task(&task)?.evidence().revision(),
            action: RiskAction::SettleUnsuccessfully,
        };
        store.accept_risk(&task, fence, intent.seq(), decision, at(5))?;
        store.request_cancel(&task, &person, at(6))?;
        store.settle_cancelled(&task, fence, at(7))?;
    }
    Ok(root.to_str().ok_or("utf-8 path")?.to_owned())
}

#[test]
fn acknowledging_an_unknown_write_needs_accept_unknown() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = held_store(&dir.path().canonicalize()?, false)?;

    // No registry: nothing is re-read, so the write stays unknown.
    let refused = acknowledge(&store, HELD_TASK, "checked the forge by hand")?;
    assert_ne!(refused.code, Some(0), "{}", refused.stdout);
    assert!(refused.stdout.is_empty(), "{}", refused.stdout);
    assert!(refused.stderr.contains("issue-core"), "{}", refused.stderr);
    assert!(refused.stderr.contains("unknown"), "{}", refused.stderr);

    // Nothing was recorded: the same call with the flag is the first record.
    let accepted = acknowledge_with(
        &store,
        HELD_TASK,
        "checked the forge by hand",
        &["--accept-unknown"],
    )?;
    assert_eq!(accepted.code, Some(0), "{}", accepted.stderr);
    assert!(
        accepted
            .stdout
            .contains("unproven write accepted: issue-core"),
        "{}",
        accepted.stdout
    );
    assert!(!accepted.stdout.contains("already acknowledged"));

    let again = acknowledge(&store, HELD_TASK, "again")?;
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    assert!(
        again.stdout.contains("already acknowledged"),
        "{}",
        again.stdout
    );
    Ok(())
}

#[test]
fn acknowledging_a_proven_write_needs_no_flag() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = held_store(&dir.path().canonicalize()?, true)?;

    let run = acknowledge(&store, HELD_TASK, "the issue exists")?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(
        run.stdout.contains("Acknowledged the writes"),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("unproven write accepted"),
        "{}",
        run.stdout
    );
    Ok(())
}
