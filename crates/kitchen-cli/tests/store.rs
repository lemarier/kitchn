//! The `kitchn store` process contract against a temporary house store. No
//! forge is contacted: without --gh nothing that depends on an issue or pull
//! request is removed, so these runs cover only store-local retention and
//! intake compaction. Intake effects are applied by the in-memory fake
//! backend.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    num::NonZeroU64,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};

use kitchen::{
    BackendId, CredentialId, HolderId, HouseId, TaskId, WorkflowId,
    contracts::{
        AttemptOutcome, AttemptStart, CapabilityRequirements, Claimant, Clock, CommitId, Effect,
        EvidenceRevision, EvidenceSubject, ExternalRef, GitHubEffect, Grant, HouseGrants,
        IssueNumber, LeaseTtl, Permission, PostingBudget, Provenance, Repository, RetryPolicy,
        Role, TaskAuthority, TaskSpec, Text, Timestamp, fake::FakeBackend,
    },
    integrations::github::IssueState,
    state::{
        EffectPlan, EffectState, HouseStore, MarkerFact, MarkerKey, MarkerSubject, StoreOptions,
        WorkItem, run_effect,
    },
    workflows::intake::{
        Classified, IntakeLedger, IntakeSource, IntakeSources, KnownIssue, PostingAuthority,
        PrivacyClass, ProblemKey, RawReport, ReadScope, SourceId, plan,
    },
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Kitchen {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Kitchen {
    fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        HouseStore::initialize(root.join("store"), house()?, StoreOptions::default())?;
        Ok(Self { _dir: dir, root })
    }

    fn store(&self) -> TestResult<HouseStore> {
        Ok(HouseStore::open(
            self.root.join("store"),
            house()?,
            StoreOptions::default(),
        )?)
    }

    fn store_arg(&self) -> TestResult<String> {
        path_arg(&self.root.join("store"))
    }

    fn run(&self, args: &[&str]) -> TestResult<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(args)
            .output()?)
    }

    fn retain(&self, extra: &[&str]) -> TestResult<Output> {
        let store = self.store_arg()?;
        let mut args = vec!["store", "retain", "--house", "origin89", "--store", &store];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

fn house() -> TestResult<HouseId> {
    Ok(HouseId::new("origin89")?)
}

fn path_arg(path: &Path) -> TestResult<String> {
    Ok(path.to_str().ok_or("non-UTF-8 path")?.to_owned())
}

fn ready_key(head: char) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new("ready-report")?,
        item: WorkItem::PullRequest {
            repository: Repository::new("origin89hq/km43")?,
            number: NonZeroU64::new(4).ok_or("zero")?,
        },
        subject: MarkerSubject::Git(EvidenceSubject {
            head: CommitId::new(&head.to_string().repeat(40))?,
            base: None,
        }),
    })
}

/// Two reports for the same pull request at an older and a newer head.
fn record_heads(kitchen: &Kitchen) -> TestResult {
    let store = kitchen.store()?;
    let recorder = Claimant::scheduled(HolderId::new("ready")?);
    for (head, at) in [('a', 1), ('b', 2)] {
        store.record_marker(
            ready_key(head)?,
            MarkerFact::workflow("ready-report/1".parse()?, &"delivered")?,
            &recorder,
            Timestamp::from_unix_millis(at),
        )?;
    }
    Ok(())
}

/// A clock stopped at one instant.
struct Fixed(Timestamp);

impl Clock for Fixed {
    fn now(&self) -> Timestamp {
        self.0
    }
}

fn intake_repository() -> TestResult<Repository> {
    Ok(Repository::new("origin89hq/km43")?)
}

/// One settled intake task whose comment the fake forge applied, leaving
/// one reservation marker under the `intake` workflow.
fn settled_intake(kitchen: &Kitchen) -> TestResult {
    let store = kitchen.store()?;
    let house = house()?;
    let repository = intake_repository()?;
    let backend_id = BackendId::new("github")?;
    let backend = FakeBackend::fully_capable(backend_id.clone(), house.clone());
    let grant = Grant::repository(
        Permission::PostComment,
        repository.clone(),
        backend_id.clone(),
        CredentialId::new("forge")?,
    );
    let grants = HouseGrants::new(house.clone(), [grant.clone()]);
    let authority = TaskAuthority::delegate(&grants, [grant])?;
    let task = TaskId::new("intake-1")?;
    let tick = Claimant::scheduled(HolderId::new("intake")?);
    let now = Timestamp::from_unix_millis(1_000);
    store.create_task(
        TaskSpec {
            id: task.clone(),
            role: Role::StationCook,
            repository: Some(repository.clone()),
            authority: authority.clone(),
            retry: RetryPolicy::new(1, Duration::from_secs(60))?,
            provenance: Provenance {
                kitchen: CommitId::new(&"a".repeat(40))?,
                house_guidance: CommitId::new(&"b".repeat(40))?,
                repository_instructions: None,
            },
            resources: BTreeSet::new(),
            requires: CapabilityRequirements::new(),
            agent: None,
        },
        &tick,
        now,
    )?;
    let fence = store
        .claim(&task, &tick, LeaseTtl::new(Duration::from_secs(60))?, now)?
        .fence();
    let AttemptStart::Started(attempt) = store.start_attempt(&task, fence, now)? else {
        return Err("attempt".into());
    };

    let sources = IntakeSources::new(
        house.clone(),
        [IntakeSource {
            id: SourceId::new("support")?,
            credential: CredentialId::new("support-token")?,
            scope: ReadScope::new([ExternalRef::new("inbox")?])?,
            privacy: PrivacyClass::Private,
        }],
    )?;
    let report = sources.accept(
        &SourceId::new("support")?,
        RawReport {
            id: ExternalRef::new("m1")?,
            channel: ExternalRef::new("inbox")?,
            link: None,
            reporter: ExternalRef::new("customer")?,
            text: Text::new("cannot log in")?,
            received_at: now,
        },
    )?;
    let problem = ProblemKey::new("login-timeout")?;
    let known = BTreeMap::from([(
        problem.clone(),
        KnownIssue {
            number: IssueNumber::new(3)?,
            state: IssueState::Open,
        },
    )]);
    let ledger = IntakeLedger::new(&store, WorkflowId::new("intake")?, repository.clone());
    let counted = ledger.counted(&task)?;
    let proposals = plan(
        &house,
        &repository,
        &[Classified { report, problem }],
        &known,
        &counted,
        &PostingAuthority::from_task(&authority, &grants, &repository, &backend_id)?,
    )?;
    let [proposal] = proposals.as_slice() else {
        return Err(format!("unexpected proposals: {proposals:?}").into());
    };
    let name = ledger.reserve(&counted, proposal, &task, &tick, now)?;
    let applied = run_effect(
        &store,
        &backend,
        &grants,
        EffectPlan {
            task: task.clone(),
            fence,
            name,
            decided_at: EvidenceRevision::INITIAL,
            effect: Effect::GitHub(GitHubEffect {
                requester: ExternalRef::new("kitchen-bot")?,
                mutation: proposal.mutation().ok_or("no mutation")?.clone(),
                posting_budget: PostingBudget::new(10)?,
            }),
            consent: None,
            basis: None,
        },
        &Fixed(now),
    )?;
    if !matches!(applied.state(), EffectState::Applied { .. }) {
        return Err("comment not applied".into());
    }
    store.finish_attempt(&task, fence, attempt, AttemptOutcome::Succeeded, now)?;
    Ok(())
}

#[test]
fn capacity_reports_each_table() -> TestResult {
    let kitchen = Kitchen::new()?;
    record_heads(&kitchen)?;
    let store = kitchen.store_arg()?;
    let output = kitchen.run(&[
        "store", "capacity", "--house", "origin89", "--store", &store,
    ])?;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("Workflow markers: 2 of 4096"), "{text}");
    assert!(text.contains("ready-report: 2"), "{text}");

    let output = kitchen.run(&[
        "store", "capacity", "--house", "origin89", "--store", &store, "--json",
    ])?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["markers"]["used"], 2);
    assert_eq!(json["tasks"]["limit"], 4096);
    Ok(())
}

#[test]
fn retain_previews_by_default_and_removes_only_with_apply() -> TestResult {
    let kitchen = Kitchen::new()?;
    record_heads(&kitchen)?;

    let preview = kitchen.retain(&["--json"])?;
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&preview.stdout)?;
    assert_eq!(json["retention"]["applied"], false);
    assert_eq!(json["retention"]["markers"][0]["reason"], "superseded");
    assert_eq!(json["observed"], 0);
    // Without --gh the pull request is a subject no lookup reached.
    assert_eq!(json["subjects"], 1);
    assert_eq!(json["notLookedUp"], 1);
    assert!(kitchen.store()?.marker(&ready_key('a')?)?.is_some());

    let applied = kitchen.retain(&["--apply"])?;
    assert!(applied.status.success());
    let text = String::from_utf8(applied.stdout)?;
    assert!(text.starts_with("Removed 1 marker(s)"), "{text}");
    assert!(
        text.contains("1 issue(s) and pull request(s) were not looked up this pass"),
        "{text}"
    );
    assert_eq!(kitchen.store()?.marker(&ready_key('a')?)?, None);
    assert!(kitchen.store()?.marker(&ready_key('b')?)?.is_some());
    Ok(())
}

#[test]
fn retain_refuses_invalid_arguments_before_any_write() -> TestResult {
    let kitchen = Kitchen::new()?;
    record_heads(&kitchen)?;
    for extra in [
        &["--window-days", "30", "--apply"][..],
        &["--max-lookups", "1001", "--apply"],
        &["--gh", "/usr/bin/gh", "--apply"],
        &["--registry", "registry", "--gh", "gh", "--apply"],
    ] {
        let output = kitchen.retain(extra)?;
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
    }
    let output = kitchen.run(&[
        "store", "capacity", "--house", "origin89", "--store", "store",
    ])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(kitchen.store()?.marker(&ready_key('a')?)?.is_some());
    Ok(())
}

#[test]
fn retain_compacts_settled_intake_reservations_only_with_apply() -> TestResult {
    let kitchen = Kitchen::new()?;
    settled_intake(&kitchen)?;
    let workflow = WorkflowId::new("intake")?;
    let before = kitchen.store()?.markers(&workflow)?;
    assert_eq!(before.len(), 1);

    let preview = kitchen.retain(&["--json"])?;
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&preview.stdout)?;
    let ledger = &json["retention"]["intake"][0];
    assert_eq!(ledger["workflow"], "intake");
    assert_eq!(ledger["repository"], "origin89hq/km43");
    assert_eq!(ledger["outcome"]["status"], "compacted");
    assert_eq!(ledger["outcome"]["folded"], 1);
    assert_eq!(json["retention"]["applied"], false);
    // The preview changed nothing.
    assert_eq!(kitchen.store()?.markers(&workflow)?, before);

    let applied = kitchen.retain(&["--apply"])?;
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let text = String::from_utf8(applied.stdout)?;
    assert!(
        text.contains("intake intake origin89hq/km43: folded 1 reservation(s), dropped 0, 0 kept"),
        "{text}"
    );
    // The reservation became one counted marker recorded by the pass.
    let after = kitchen.store()?.markers(&workflow)?;
    let [counted] = after.as_slice() else {
        return Err(format!("unexpected markers: {after:?}").into());
    };
    assert_ne!(Some(counted), before.first());
    assert_eq!(
        counted.recorded_by(),
        &Claimant::interactive(HolderId::new("kitchn-store-retain")?)
    );
    // The folded report still counts, and nothing is left to compact.
    let store = kitchen.store()?;
    let ledger = IntakeLedger::new(&store, workflow, intake_repository()?);
    let total = ledger
        .counted(&TaskId::new("intake-2")?)?
        .total(&ProblemKey::new("login-timeout")?);
    assert_eq!(total, 1);
    let again = kitchen.retain(&["--apply", "--json"])?;
    let json: serde_json::Value = serde_json::from_slice(&again.stdout)?;
    assert_eq!(json["retention"]["applied"], false);
    assert_eq!(json["retention"]["intake"][0]["outcome"]["folded"], 0);
    Ok(())
}
