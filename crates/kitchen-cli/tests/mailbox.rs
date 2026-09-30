//! The `kitchn mailbox` process contract against a temporary house store:
//! a worker posts and reads answers for its own task only, and a person or
//! coordinator answers. Tasks and attempts are set up through the library;
//! no worker backend is contacted.

use std::{
    collections::BTreeSet,
    error::Error,
    path::PathBuf,
    process::{Command, Output},
    time::Duration,
};

use kitchen::{
    HolderId, HouseId, TaskId,
    contracts::{
        AttemptStart, CapabilityRequirements, Claimant, CommitId, Fence, LeaseTtl, Provenance,
        RetryPolicy, Role, TaskAuthority, TaskSpec, Timestamp,
    },
    state::{HouseStore, MAX_MAIL_BODY_BYTES, StoreOptions},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Kitchen {
    _dir: tempfile::TempDir,
    store: PathBuf,
}

impl Kitchen {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let store = dir.path().canonicalize()?.join("store");
        HouseStore::initialize(&store, house()?, StoreOptions::default())?;
        Ok(Self { _dir: dir, store })
    }

    fn open(&self) -> TestResult<HouseStore> {
        Ok(HouseStore::open(
            &self.store,
            house()?,
            StoreOptions::default(),
        )?)
    }

    /// A claimed task with a running attempt; returns its claim fence.
    fn running(&self, id: &str) -> TestResult<Fence> {
        let store = self.open()?;
        let task = TaskId::new(id)?;
        let tick = Claimant::scheduled(HolderId::new("coordinator")?);
        let now = Timestamp::from_unix_millis(1_000);
        store.create_task(
            TaskSpec {
                id: task.clone(),
                role: Role::StationCook,
                repository: None,
                authority: TaskAuthority::delegate(
                    &kitchen::contracts::HouseGrants::new(house()?, []),
                    [],
                )?,
                retry: RetryPolicy::new(1, Duration::from_secs(3600))?,
                provenance: Provenance {
                    kitchen: CommitId::new(&"a".repeat(40))?,
                    house_guidance: CommitId::new(&"b".repeat(40))?,
                    repository_instructions: None,
                },
                resources: BTreeSet::new(),
                requires: CapabilityRequirements::new(),
                agent: None,
                work_type: None,
            },
            &tick,
            now,
        )?;
        let fence = store
            .claim(&task, &tick, LeaseTtl::new(Duration::from_secs(3600))?, now)?
            .fence();
        let AttemptStart::Started(_) = store.start_attempt(&task, fence, now)? else {
            return Err("attempt".into());
        };
        Ok(fence)
    }

    fn run(&self, args: &[&str]) -> TestResult<Output> {
        let store = self.store.to_str().ok_or("non-UTF-8 path")?;
        let mut all = vec!["mailbox", args.first().copied().ok_or("no subcommand")?];
        all.extend_from_slice(&["--house", "origin89", "--store", store]);
        all.extend_from_slice(args.get(1..).unwrap_or_default());
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(&all)
            .output()?)
    }

    fn worker(
        &self,
        command: &str,
        task: &str,
        fence: Fence,
        extra: &[&str],
    ) -> TestResult<Output> {
        let fence = fence.get().to_string();
        let mut args = vec![command, "--task", task, "--fence", &fence];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

fn house() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn stdout(output: &Output) -> TestResult<String> {
    Ok(String::from_utf8(output.stdout.clone())?)
}

/// The message id a post printed after `label: `.
fn posted(output: &Output, label: &str) -> TestResult<String> {
    let text = stdout(output)?;
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .ok_or_else(|| format!("no {label} in {text:?}"))?
        .trim()
        .to_owned())
}

#[test]
fn a_worker_asks_and_reads_a_persons_answer() -> TestResult {
    let kitchen = Kitchen::new()?;
    let fence = kitchen.running("task-1")?;
    let asked = kitchen.worker("ask", "task-1", fence, &["--body", "Keep the old API?"])?;
    assert!(asked.status.success(), "{asked:?}");
    let question = posted(&asked, "question: ")?;
    assert!(stdout(&asked)?.contains("answer: pending"));

    let listed = kitchen.run(&["questions"])?;
    assert!(listed.status.success());
    assert!(stdout(&listed)?.contains(&format!(
        "{question} task task-1 attempt 1: Keep the old API?"
    )));

    let replied = kitchen.run(&[
        "reply",
        "--question",
        &question,
        "--body",
        "Yes, keep it.",
        "--by",
        "person",
    ])?;
    assert!(replied.status.success(), "{replied:?}");
    assert_eq!(stdout(&replied)?, format!("answered: {question}\n"));
    let again = kitchen.run(&[
        "reply",
        "--question",
        &question,
        "--body",
        "Yes, keep it.",
        "--by",
        "person",
    ])?;
    assert_eq!(stdout(&again)?, format!("already answered: {question}\n"));
    let different = kitchen.run(&[
        "reply",
        "--question",
        &question,
        "--body",
        "No.",
        "--by",
        "person",
    ])?;
    assert_eq!(different.status.code(), Some(1), "a conflicting answer");

    let answer = kitchen.worker("answer", "task-1", fence, &["--question", &question])?;
    assert!(answer.status.success());
    assert_eq!(stdout(&answer)?, "answer:\nYes, keep it.\n");
    // The person's answer is recorded as their time on the asking attempt.
    let record = kitchen.open()?.task(&TaskId::new("task-1")?)?;
    let replies = record.attempts().first().ok_or("an attempt")?.replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies.first().map(|reply| reply.question.as_str()),
        Some(question.as_str())
    );
    Ok(())
}

#[test]
fn a_worker_reports_and_escalates_only_for_its_own_task() -> TestResult {
    let kitchen = Kitchen::new()?;
    let one = kitchen.running("task-1")?;
    let two = kitchen.running("task-2")?;
    let report = kitchen.worker(
        "report",
        "task-1",
        one,
        &[
            "--outcome",
            "succeeded",
            "--clean",
            "yes",
            "--pushed",
            "no",
            "--body",
            "Done.",
        ],
    )?;
    assert!(report.status.success(), "{report:?}");
    posted(&report, "report: ")?;
    let escalated = kitchen.worker("escalate", "task-1", one, &["--body", "Base moved."])?;
    posted(&escalated, "escalation: ")?;
    let asked = kitchen.worker("ask", "task-1", one, &["--body", "Which?"])?;
    let question = posted(&asked, "question: ")?;

    // Task two's fence cannot post for task one, and task two's worker
    // cannot read task one's question.
    let foreign = kitchen.worker("escalate", "task-1", two, &["--body", "Help."])?;
    assert_eq!(foreign.status.code(), Some(1));
    assert!(String::from_utf8(foreign.stderr)?.contains("fence does not belong"));
    let peek = kitchen.worker("answer", "task-2", two, &["--question", &question])?;
    assert_eq!(peek.status.code(), Some(2));
    assert!(peek.stdout.is_empty(), "nothing of task one is printed");
    Ok(())
}

#[test]
fn invalid_worker_input_is_refused_before_anything_is_stored() -> TestResult {
    let kitchen = Kitchen::new()?;
    let fence = kitchen.running("task-1")?;
    let long = "x".repeat(MAX_MAIL_BODY_BYTES + 1);
    let oversized = kitchen.worker("escalate", "task-1", fence, &["--body", &long])?;
    assert_eq!(oversized.status.code(), Some(2));
    let wait = kitchen.worker(
        "ask",
        "task-1",
        fence,
        &["--body", "Q", "--wait-secs", "901"],
    )?;
    assert_eq!(wait.status.code(), Some(2));
    let outcome = kitchen.worker(
        "report",
        "task-1",
        fence,
        &["--outcome", "maybe", "--body", "?"],
    )?;
    assert_eq!(outcome.status.code(), Some(2));
    let checkout = kitchen.worker(
        "report",
        "task-1",
        fence,
        &["--outcome", "succeeded", "--clean", "mostly", "--body", "?"],
    )?;
    assert_eq!(checkout.status.code(), Some(2));
    let listed = kitchen.run(&["questions"])?;
    assert_eq!(stdout(&listed)?, "mailbox: 0 of 512\nno open questions\n");
    Ok(())
}

#[test]
fn another_houses_store_is_refused() -> TestResult {
    let kitchen = Kitchen::new()?;
    let fence = kitchen.running("task-1")?;
    let store = kitchen.store.to_str().ok_or("non-UTF-8 path")?;
    let fence = fence.get().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .args([
            "mailbox", "escalate", "--house", "acme", "--store", store, "--task", "task-1",
            "--fence", &fence, "--body", "Help.",
        ])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(kitchen.open()?.mailbox_usage()?.used, 0);
    Ok(())
}
