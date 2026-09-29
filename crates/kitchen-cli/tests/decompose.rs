//! `kitchen decompose preview` through the real binary. Reads only a
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
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
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
