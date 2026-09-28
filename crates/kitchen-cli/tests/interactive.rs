//! The interactive entrypoints through the real CLI, in disposable registries,
//! checkouts, and stores. Orca evidence is a sanitized capture; nothing
//! reaches a live forge or orchestrator.
use kitchen::{
    adoption::{HouseRegistry, InstructionBundle},
    house::HouseConfig,
    state::{HouseStore, StoreOptions},
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SKILL: &str = include_str!("../../../skills/kitchn/SKILL.md");
const REVISION: &str = "dddddddddddddddddddddddddddddddddddddddd";

struct Setup {
    _temp: tempfile::TempDir,
    root: PathBuf,
    registry: HouseRegistry,
    consumer: PathBuf,
    store: PathBuf,
}

fn git(path: &Path, args: &[&str]) -> TestResult {
    let status = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()?;
    if !status.success() {
        return Err(format!("git {args:?} failed").into());
    }
    Ok(())
}

/// A crabnebula registry, a checkout of its fixture repository, and an
/// initialized house store; bound unless `bind` is false.
fn setup(bind: bool) -> TestResult<Setup> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let house: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    let bundle: InstructionBundle = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula-bundle.json"
    ))?;
    registry.initialize(&house)?;
    registry.sync(&house.house, &bundle)?;
    let consumer = root.join("consumer");
    fs::create_dir_all(&consumer)?;
    git(&consumer, &["init", "--quiet"])?;
    git(
        &consumer,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/crabnebula/tauri-fixture.git",
        ],
    )?;
    if bind {
        registry.bind_repository(&kitchen::house::RepositoryConfig {
            schema: 2,
            house: house.house.clone(),
            repository: "crabnebula/tauri-fixture".parse()?,
            workflows: BTreeSet::new(),
            additional_reviewers: BTreeSet::new(),
            additional_checks: BTreeSet::new(),
        })?;
    }
    let store = root.join("store");
    HouseStore::initialize(&store, house.house, StoreOptions::default())?;
    Ok(Setup {
        _temp: temp,
        root,
        registry,
        consumer,
        store,
    })
}

impl Setup {
    fn file(&self, name: &str, contents: &str) -> TestResult<PathBuf> {
        let path = self.root.join(name);
        fs::write(&path, contents)?;
        Ok(path)
    }

    /// Run `kitchen <args>` in the checkout with the session flags.
    fn run(&self, args: &[&str], claim: Option<&str>) -> TestResult<Output> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kitchen"));
        command
            .current_dir(&self.consumer)
            .args(args)
            .arg("--registry")
            .arg(self.registry.root())
            .args(["--revision", REVISION]);
        if let Some(holder) = claim {
            command
                .arg("--store")
                .arg(&self.store)
                .args(["--holder", holder]);
        }
        Ok(command.output()?)
    }
}

fn json(output: &Output) -> TestResult<serde_json::Value> {
    Ok(serde_json::from_slice(&output.stdout)?)
}

const ISSUE: &str = r#"{"status":"open","subIssues":[],"independentParts":false}"#;

#[test]
fn work_claims_the_issue_and_a_second_session_is_skipped() -> TestResult {
    let setup = setup(true)?;
    let facts = setup.file("facts.json", ISSUE)?;
    let facts = facts.to_str().ok_or("path")?;
    let first = setup.run(&["work", "17", "--facts", facts, "--json"], Some("person"))?;
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let report = json(&first)?;
    assert_eq!(report["house"], "crabnebula");
    assert_eq!(report["plan"]["type"], "implement");
    assert_eq!(report["mode"]["type"], "solo");
    assert_eq!(report["mode"]["reason"]["reason"], "not-observed");
    assert_eq!(report["instructions"]["repositoryInstructions"], REVISION);
    assert_eq!(report["lease"]["trigger"], "interactive");
    let task = report["plan"]["task"].as_str().ok_or("task")?.to_owned();

    // Human output names the pinned rules and the single-agent limit.
    let text = setup.run(&["work", "17", "--facts", facts], Some("person"))?;
    let text = String::from_utf8(text.stdout)?;
    assert!(text.contains("House rules: read "));
    assert!(text.contains("fan-out unavailable (no orchestrator evidence)"));

    let second = setup.run(&["work", "17", "--facts", facts, "--json"], Some("other"))?;
    assert_eq!(second.status.code(), Some(1));
    assert_eq!(json(&second)?["plan"]["refusal"]["trigger"], "interactive");

    // Only the holder hands back; the other session then adopts it.
    let mut hand_back = Command::new(env!("CARGO_BIN_EXE_kitchen"));
    hand_back
        .current_dir(&setup.consumer)
        .args(["hand-back", &task, "--registry"])
        .arg(setup.registry.root())
        .arg("--store")
        .arg(&setup.store);
    let refused = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .current_dir(&setup.consumer)
        .args(["hand-back", &task, "--holder", "other", "--registry"])
        .arg(setup.registry.root())
        .arg("--store")
        .arg(&setup.store)
        .output()?;
    assert_eq!(refused.status.code(), Some(1));
    let released = hand_back.args(["--holder", "person"]).output()?;
    assert_eq!(released.status.code(), Some(0), "{released:?}");
    assert!(String::from_utf8(released.stdout)?.contains("Handed back task"));
    let adopted = setup.run(&["work", "17", "--facts", facts, "--json"], Some("other"))?;
    assert_eq!(json(&adopted)?["plan"]["adopted"], true);
    Ok(())
}

#[test]
fn orca_captures_enable_fan_out_only_for_this_repository() -> TestResult {
    let setup = setup(true)?;
    let facts = setup.file(
        "facts.json",
        r#"{"status":"open","subIssues":[{"number":18,"status":"open"},{"number":19,"status":"open","blocked":true}]}"#,
    )?;
    let status = setup.file(
        "status.json",
        r#"{"ok":true,"result":{"runtime":{"state":"ready","reachable":true,"appVersion":"1.4.216","capabilities":["orchestration.contract.v1","orchestration.worker-stop-verdict.v1"]}}}"#,
    )?;
    let here = setup.file(
        "here.json",
        r#"{"ok":true,"result":{"worktree":{"projectId":"github:crabnebula/tauri-fixture"}}}"#,
    )?;
    let elsewhere = setup.file(
        "elsewhere.json",
        r#"{"ok":true,"result":{"worktree":{"projectId":"github:crabnebula/other"}}}"#,
    )?;
    let run = |worktree: &Path, holder: &str| {
        setup.run(
            &[
                "work",
                "17",
                "--facts",
                facts.to_str().unwrap_or_default(),
                "--orca-status",
                status.to_str().unwrap_or_default(),
                "--orca-worktree",
                worktree.to_str().unwrap_or_default(),
                "--json",
            ],
            Some(holder),
        )
    };
    let fan_out = json(&run(&here, "person")?)?;
    assert_eq!(fan_out["mode"]["type"], "fan-out");
    assert_eq!(fan_out["plan"]["type"], "coordinate");
    assert_eq!(fan_out["plan"]["ready"], serde_json::json!([18]));
    assert_eq!(fan_out["plan"]["waiting"], serde_json::json!([19]));
    assert_eq!(fan_out["plan"]["fanOut"], true);

    let mismatch = setup.run(
        &[
            "work",
            "20",
            "--facts",
            facts.to_str().ok_or("path")?,
            "--orca-status",
            status.to_str().ok_or("path")?,
            "--orca-worktree",
            elsewhere.to_str().ok_or("path")?,
            "--json",
        ],
        Some("person"),
    )?;
    let mismatch = json(&mismatch)?;
    assert_eq!(mismatch["mode"]["reason"]["type"], "project-mismatch");
    assert_eq!(mismatch["plan"]["fanOut"], false);
    // One capture without the other is an input error.
    let half = setup.run(
        &[
            "work",
            "17",
            "--facts",
            facts.to_str().ok_or("path")?,
            "--orca-status",
            status.to_str().ok_or("path")?,
        ],
        Some("person"),
    )?;
    assert_eq!(half.status.code(), Some(2));
    Ok(())
}

#[test]
fn an_unbound_repository_asks_for_setup_and_claims_nothing() -> TestResult {
    let setup = setup(false)?;
    let facts = setup.file("facts.json", ISSUE)?;
    let output = setup.run(
        &["work", "17", "--facts", facts.to_str().ok_or("path")?],
        Some("person"),
    )?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("claimed by house crabnebula but not set up"));
    assert!(stdout.contains("kitchen house setup --registry"));
    let store = HouseStore::open(&setup.store, "crabnebula".parse()?, StoreOptions::default())?;
    assert!(store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn pr_routes_a_conflict_to_repair_at_the_exact_head() -> TestResult {
    let setup = setup(true)?;
    let facts = setup.file(
        "pr.json",
        r#"{"state":"open","head":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","headBranch":"feature","baseBranch":"main","mergeability":"conflicting","review":"reviewed"}"#,
    )?;
    let facts = facts.to_str().ok_or("path")?;
    let repair = setup.run(&["pr", "61", "--facts", facts, "--json"], Some("person"))?;
    assert_eq!(repair.status.code(), Some(0), "{repair:?}");
    let report = json(&repair)?;
    assert_eq!(report["plan"]["type"], "repair");
    assert_eq!(report["plan"]["round"], 1);
    assert_eq!(
        report["plan"]["head"],
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    );
    let gate = setup.run(
        &["pr", "62", "--facts", facts, "--as", "gate", "--json"],
        Some("person"),
    )?;
    assert_eq!(json(&gate)?["plan"]["type"], "gate");
    let exhausted = setup.run(
        &["pr", "63", "--facts", facts, "--fix-rounds", "0", "--json"],
        Some("person"),
    )?;
    assert_eq!(exhausted.status.code(), Some(1));
    assert_eq!(json(&exhausted)?["plan"]["type"], "budget-exhausted");
    let bad = setup.file("bad.json", r#"{"state":"open"}"#)?;
    let invalid = setup.run(
        &["pr", "61", "--facts", bad.to_str().ok_or("path")?],
        Some("person"),
    )?;
    assert_eq!(invalid.status.code(), Some(2));
    Ok(())
}

#[test]
fn pr_refuses_session_round_counts_and_raised_budgets() -> TestResult {
    let setup = setup(true)?;
    // A facts file that still carries a round count is refused: rounds come
    // from the house store, never from what the session read.
    let stale = setup.file(
        "stale.json",
        r#"{"state":"open","head":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","headBranch":"feature","baseBranch":"main","mergeability":"conflicting","review":"reviewed","roundsUsed":0}"#,
    )?;
    let refused = setup.run(
        &["pr", "61", "--facts", stale.to_str().ok_or("path")?],
        Some("person"),
    )?;
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");

    let facts = setup.file(
        "pr.json",
        r#"{"state":"open","head":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","headBranch":"feature","baseBranch":"main","mergeability":"conflicting","review":"reviewed"}"#,
    )?;
    let facts = facts.to_str().ok_or("path")?;
    let raised = setup.run(
        &["pr", "61", "--facts", facts, "--fix-rounds", "3", "--json"],
        Some("person"),
    )?;
    assert_eq!(raised.status.code(), Some(1), "{raised:?}");
    assert!(String::from_utf8(raised.stderr)?.contains("exceed the house budget"));
    let store = HouseStore::open(&setup.store, "crabnebula".parse()?, StoreOptions::default())?;
    assert!(store.tasks()?.is_empty(), "a refused budget claims nothing");

    // Within the house budget the round is claimed, and the next session
    // for the same pull request is skipped rather than opening round 2.
    let first = setup.run(
        &["pr", "61", "--facts", facts, "--fix-rounds", "2", "--json"],
        Some("person"),
    )?;
    assert_eq!(json(&first)?["plan"]["round"], 1);
    let second = setup.run(&["pr", "61", "--facts", facts, "--json"], Some("other"))?;
    assert_eq!(second.status.code(), Some(1));
    assert_eq!(json(&second)?["plan"]["type"], "skipped");
    Ok(())
}

#[test]
fn issue_previews_post_nothing_and_refuse_mismatched_drafts() -> TestResult {
    let setup = setup(true)?;
    let draft = setup.file(
        "new.json",
        r#"{"repository":"crabnebula/tauri-fixture","target":{"type":"new","title":"Export specta bindings","body":"Outcome and acceptance criteria."},"addLabels":["ready"],"blockedBy":[12]}"#,
    )?;
    let draft = draft.to_str().ok_or("path")?;
    let preview = setup.run(&["issue", "new", "--draft", draft], None)?;
    assert_eq!(preview.status.code(), Some(0), "{preview:?}");
    let text = String::from_utf8(preview.stdout)?;
    assert!(text.contains("1. Create issue \"Export specta bindings\""));
    assert!(text.contains("2. Add label \"ready\""));
    assert!(text.contains("3. Mark blocked by #12"));
    assert!(text.contains("Approve digest sha256:"));
    assert!(text.contains("Nothing was posted."));

    let open = setup.file(
        "open.json",
        r#"{"repository":"crabnebula/tauri-fixture","target":{"type":"refine","issue":72,"comment":"Sharper criteria."},"questions":["Labels or changed paths?"]}"#,
    )?;
    let unready = setup.run(
        &[
            "issue",
            "refine",
            "72",
            "--draft",
            open.to_str().ok_or("path")?,
        ],
        None,
    )?;
    assert_eq!(unready.status.code(), Some(1));
    assert!(String::from_utf8(unready.stdout)?.contains("Not ready. Decide first:"));
    // The draft must target what the command names.
    let wrong = setup.run(
        &[
            "issue",
            "refine",
            "73",
            "--draft",
            open.to_str().ok_or("path")?,
        ],
        None,
    )?;
    assert_eq!(wrong.status.code(), Some(2));
    let crossed = setup.run(&["issue", "refine", "72", "--draft", draft], None)?;
    assert_eq!(crossed.status.code(), Some(2));
    // A draft for another repository than the checkout's is refused.
    let foreign = setup.file(
        "foreign.json",
        r#"{"repository":"someone/else","target":{"type":"new","title":"t","body":"b"}}"#,
    )?;
    let refused = setup.run(
        &["issue", "new", "--draft", foreign.to_str().ok_or("path")?],
        None,
    )?;
    assert_eq!(refused.status.code(), Some(1));
    Ok(())
}

/// Every flag the skill tells an agent to pass exists on some command it
/// names, so the skill and the CLI cannot drift apart silently.
#[test]
fn the_skill_uses_only_flags_the_cli_accepts() -> TestResult {
    let mut help = String::new();
    for args in [
        vec!["work", "--help"],
        vec!["pr", "--help"],
        vec!["issue", "new", "--help"],
        vec!["issue", "refine", "--help"],
        vec!["hand-back", "--help"],
        vec!["house", "setup", "--help"],
        vec!["house", "doctor", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
            .args(&args)
            .output()?;
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        help.push_str(&String::from_utf8(output.stdout)?);
    }
    let flags: BTreeSet<&str> = SKILL
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '[' | ']' | '(' | ')' | ','))
        .filter(|word| {
            word.strip_prefix("--")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_lowercase()))
        })
        .map(|word| word.split('=').next().unwrap_or(word))
        .collect();
    assert!(flags.contains("--orca-status"));
    for flag in flags {
        assert!(help.contains(flag), "skill flag {flag} is not accepted");
    }
    assert!(SKILL.starts_with("---\nname: kitchn\n"));
    Ok(())
}
