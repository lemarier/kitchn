//! Interactive entrypoints: house resolution, orchestrator evidence, `work`
//! and `pr` claims shared with scheduled runs, and approved issue drafts.
//! Temporary registries and stores, sanitized Orca captures, the fake worker
//! backend, and an in-memory forge only: simulated evidence, not live
//! runtime evidence.

mod common;
mod workflows_support;

use std::{cell::RefCell, collections::BTreeSet, fs, path::Path, process::Command};

use common::{
    TestResult, backend_id, credential, holder, house, house_with_fix_rounds, interactive,
    scheduled, ttl,
};
use kitchen::{
    Error, ErrorClass,
    adoption::{HouseRegistry, InstructionBundle},
    contracts::{
        Authorization, BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, CommitId,
        Effect, EffectExecutor, EffectFailure, EffectRequest, ExternalRef, GitHubAction,
        GitHubEffect, GitHubMutation, Grant, HouseGrants, IdempotencyKey, IssueNumber, Lookup,
        NotAppliedReason, Permission, PostingBudget, Provenance, Receipt, Repository, Settlement,
        Trigger, UncertainReason,
    },
    house::{HouseConfig, HouseError, RepositoryConfig, Workflow},
    selection::{AgentModel, WorkType},
    state::{EffectOutcome, RiskAction, RiskDecision, TaskState},
    workflows::{
        interactive::{
            AcknowledgeOutcome, AcknowledgeReason, AcknowledgeReport, Acknowledgement,
            ClaimRefusal, Decomposer, DraftApproval, DraftOptions, DraftOutcome, DraftTarget,
            DraftWriter, ExecutionMode, ForgeWriter, HandBack, HouseResolution, Idle, IssueDraft,
            IssueFacts, IssueStatus, MAX_ACKNOWLEDGE_REASON_BYTES, MAX_DRAFT_LABELS, NoDecomposer,
            Orchestrator, PlannedWrite, PrFacts, PrIntent, PrPlan, PrRequest, ReadBack,
            ReviewState, SoloReason, SubIssue, Unavailable, WorkPlan, WorkRequest, WriteReadBack,
            acknowledge_draft, apply_draft, draft_preview, draft_task_id, execution_mode,
            hand_back, pull_request, resolve_house, work,
        },
        pickup::{ClaimOutcome, DEFAULT_FIX_ROUNDS, FollowUpBudget, claim_issue, issue_task_id},
        repair::{Mergeability, PullRequestState, PullRequestView, repair_task_id},
    },
};
use workflows_support::{World, issue, provenance, repo, template};

// ---------------------------------------------------------------- house

fn house_config(name: &str) -> TestResult<HouseConfig> {
    Ok(serde_json::from_str(match name {
        "origin89" => include_str!("fixtures/house/origin89.json"),
        _ => include_str!("fixtures/house/crabnebula.json"),
    })?)
}

fn house_bundle(name: &str) -> TestResult<InstructionBundle> {
    Ok(serde_json::from_str(match name {
        "origin89" => include_str!("fixtures/house/origin89-bundle.json"),
        _ => include_str!("fixtures/house/crabnebula-bundle.json"),
    })?)
}

/// A Git checkout at `path` whose `origin` names `repository` on GitHub.
fn checkout(path: &Path, repository: &str) -> TestResult {
    fs::create_dir_all(path)?;
    for args in [
        vec!["init", "--quiet"],
        vec![
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{repository}.git"),
        ],
    ] {
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
            .args(&args)
            .status()?;
        if !status.success() {
            return Err(format!("git {args:?} failed").into());
        }
    }
    Ok(())
}

fn revision() -> TestResult<CommitId> {
    Ok(CommitId::new(&"d".repeat(40))?)
}

#[test]
fn a_bound_repository_resolves_its_house_with_pinned_rules() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let crab = house_config("crabnebula")?;
    registry.initialize(&crab)?;
    registry.sync(&crab.house, &house_bundle("crabnebula")?)?;
    let consumer = root.join("tauri");
    checkout(&consumer, "crabnebula/tauri-fixture")?;

    // Claimed by exactly one house but not bound: the person must set it up.
    let HouseResolution::NeedsSetup { repository, house } =
        resolve_house(&registry, &consumer, revision()?)?
    else {
        return Err("an unbound repository must need setup".into());
    };
    assert_eq!(repository.as_str(), "crabnebula/tauri-fixture");
    assert_eq!(house, crab.house);

    registry.bind_repository(&RepositoryConfig {
        schema: 2,
        house: crab.house.clone(),
        repository,
        workflows: BTreeSet::from([Workflow::Pickup]),
        additional_reviewers: BTreeSet::new(),
        additional_checks: BTreeSet::new(),
    })?;
    let HouseResolution::Ready(resolved) = resolve_house(&registry, &consumer, revision()?)? else {
        return Err("a bound repository must resolve".into());
    };
    assert_eq!(resolved.binding.house, crab.house);
    assert_eq!(
        resolved.instructions.provenance.repository_instructions,
        Some(revision()?)
    );
    // House rules come from the pinned snapshot: Tauri, not another house's.
    let rules = fs::read_to_string(&resolved.instructions.entrypoint)?;
    assert!(rules.contains("Tauri"));
    assert!(!rules.contains("bench evidence"));
    // Nothing was written into the working tree.
    assert_eq!(fs::read_dir(&consumer)?.count(), 1);
    Ok(())
}

#[test]
fn missing_or_ambiguous_house_selection_fails_closed() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    let crab = house_config("crabnebula")?;
    registry.initialize(&crab)?;

    let stranger = root.join("stranger");
    checkout(&stranger, "someone/else")?;
    assert!(matches!(
        resolve_house(&registry, &stranger, revision()?),
        Err(Error::House(HouseError::HouseSelection))
    ));

    // A second house claiming the same repository makes it ambiguous.
    let mut origin = house_config("origin89")?;
    origin
        .repositories
        .insert("crabnebula/tauri-fixture".parse()?);
    registry.initialize(&origin)?;
    let consumer = root.join("tauri");
    checkout(&consumer, "crabnebula/tauri-fixture")?;
    let refused = resolve_house(&registry, &consumer, revision()?);
    assert!(matches!(
        refused,
        Err(Error::House(HouseError::AmbiguousHouse { .. }))
    ));

    // Outside any checkout the repository cannot be identified.
    let bare = root.join("plain");
    fs::create_dir_all(&bare)?;
    assert!(resolve_house(&registry, &bare, revision()?).is_err());
    Ok(())
}

// --------------------------------------------------------- orchestrator

const WORKTREE: &str = r#"{"id":"w","ok":true,"result":{"worktree":{"id":"x","projectId":"github:origin89hq/firmware","path":"/tmp/x"}}}"#;

fn status(state: &str, reachable: bool, version: &str, features: &[&str]) -> String {
    serde_json::json!({
        "id": "local-status",
        "ok": true,
        "result": {"runtime": {
            "state": state,
            "reachable": reachable,
            "appVersion": version,
            "capabilities": features,
        }},
    })
    .to_string()
}

fn ready_status() -> String {
    status(
        "ready",
        true,
        "1.4.216",
        &[
            "orchestration.contract.v1",
            "orchestration.worker-stop-verdict.v1",
            "browser.v1",
        ],
    )
}

#[test]
fn fan_out_needs_positive_orca_evidence_for_this_repository() -> TestResult {
    let orca = Orchestrator::from_orca(ready_status().as_bytes(), WORKTREE.as_bytes());
    assert!(matches!(
        orca,
        Orchestrator::Orca {
            project: Some(_),
            ..
        }
    ));
    assert_eq!(execution_mode(&repo()?, &orca), ExecutionMode::FanOut);
    // Case-insensitive match: GitHub names differ only in case.
    assert_eq!(
        execution_mode(&Repository::new("Origin89HQ/Firmware")?, &orca),
        ExecutionMode::FanOut
    );
    // The same runtime in another repository's worktree is not evidence here.
    assert_eq!(
        execution_mode(&Repository::new("origin89hq/other")?, &orca),
        ExecutionMode::Solo {
            reason: SoloReason::ProjectMismatch
        }
    );
    Ok(())
}

#[test]
fn missing_or_unusable_orchestrators_are_reported_not_faked() -> TestResult {
    let firmware = repo()?;
    let solo = |orca: &Orchestrator| Some(execution_mode(&firmware, orca));
    let unavailable = |reason| {
        Some(ExecutionMode::Solo {
            reason: SoloReason::Unavailable { reason },
        })
    };
    assert_eq!(
        solo(&Orchestrator::Unavailable(Unavailable::NotObserved)),
        unavailable(Unavailable::NotObserved)
    );
    let cases = [
        (
            status("starting", true, "1.4.216", &REQUIRED),
            Unavailable::RuntimeNotReady,
        ),
        (
            status("ready", false, "1.4.216", &REQUIRED),
            Unavailable::RuntimeNotReady,
        ),
        (
            status("ready", true, "1.5.0", &REQUIRED),
            Unavailable::UnsupportedVersion,
        ),
        (
            status("ready", true, "1.4.211", &REQUIRED),
            Unavailable::UnsupportedVersion,
        ),
        (
            status("ready", true, "1.4.216", &["orchestration.contract.v1"]),
            Unavailable::MissingRuntimeFeature,
        ),
        ("not json".to_owned(), Unavailable::Unreadable),
        (
            r#"{"ok":false,"result":null}"#.to_owned(),
            Unavailable::Unreadable,
        ),
    ];
    for (capture, reason) in cases {
        let orca = Orchestrator::from_orca(capture.as_bytes(), WORKTREE.as_bytes());
        assert_eq!(orca, Orchestrator::Unavailable(reason), "{capture}");
        assert_eq!(solo(&orca), unavailable(reason));
    }
    // Oversized captures are refused before parsing.
    let huge = format!(
        "{}{}",
        ready_status(),
        " ".repeat(kitchen::workflows::interactive::MAX_ORCA_OUTPUT_BYTES)
    );
    assert_eq!(
        Orchestrator::from_orca(huge.as_bytes(), WORKTREE.as_bytes()),
        Orchestrator::Unavailable(Unavailable::Unreadable)
    );
    // A worktree without a GitHub project names no repository.
    let local = r#"{"ok":true,"result":{"worktree":{"projectId":"local:abc"}}}"#;
    let orca = Orchestrator::from_orca(ready_status().as_bytes(), local.as_bytes());
    assert_eq!(
        solo(&orca),
        Some(ExecutionMode::Solo {
            reason: SoloReason::ProjectUnknown
        })
    );
    // A backend without cancellation cannot supervise workers.
    let partial = Orchestrator::Orca {
        project: Some(repo()?),
        capabilities: CapabilitySet::supporting([
            Capability::WorkerLaunchIsolated,
            Capability::WorkerLaunchReadiness,
            Capability::WorkerMessaging,
            Capability::WorkerStatusAndOutcome,
            Capability::WorkerDeliveries,
        ]),
    };
    assert_eq!(
        solo(&partial),
        Some(ExecutionMode::Solo {
            reason: SoloReason::MissingCapabilities {
                missing: vec![Capability::WorkerCancel]
            }
        })
    );
    // Nor can one whose coordinator never receives questions or reports.
    let silent = Orchestrator::Orca {
        project: Some(repo()?),
        capabilities: CapabilitySet::supporting([
            Capability::WorkerLaunchIsolated,
            Capability::WorkerLaunchReadiness,
            Capability::WorkerMessaging,
            Capability::WorkerStatusAndOutcome,
            Capability::WorkerCancel,
        ]),
    };
    assert_eq!(
        solo(&silent),
        Some(ExecutionMode::Solo {
            reason: SoloReason::MissingCapabilities {
                missing: vec![Capability::WorkerDeliveries]
            }
        })
    );
    Ok(())
}

const REQUIRED: [&str; 2] = [
    "orchestration.contract.v1",
    "orchestration.worker-stop-verdict.v1",
];

// ----------------------------------------------------------------- work

fn open_issue() -> IssueFacts {
    IssueFacts {
        status: IssueStatus::Open,
        sub_issues: Vec::new(),
        independent_parts: false,
    }
}

fn sub(number: u64, status: IssueStatus, blocked: bool) -> TestResult<SubIssue> {
    Ok(SubIssue {
        number: IssueNumber::new(number)?,
        status,
        blocked,
    })
}

fn solo() -> ExecutionMode {
    ExecutionMode::Solo {
        reason: SoloReason::Unavailable {
            reason: Unavailable::NotObserved,
        },
    }
}

struct WorkCase<'a> {
    world: &'a World,
    facts: IssueFacts,
    mode: ExecutionMode,
    take_over: bool,
}

impl WorkCase<'_> {
    fn run(
        &self,
        claimant: &kitchen::contracts::Claimant,
    ) -> Result<(WorkPlan, Option<kitchen::state::Lease>), Error> {
        let template = template().map_err(|_| Error::from(kitchen::IdentifierError::Characters))?;
        let issue = issue(7).map_err(|_| Error::from(kitchen::IdentifierError::Characters))?;
        work(&WorkRequest {
            store: &self.world.fixture.store,
            template: &template,
            issue: &issue,
            facts: &self.facts,
            claimant,
            ttl: ttl(600).map_err(|_| Error::from(kitchen::IdentifierError::Characters))?,
            now: self.world.now(),
            mode: &self.mode,
            take_over: self.take_over,
        })
    }
}

fn case(world: &World, facts: IssueFacts, mode: ExecutionMode) -> WorkCase<'_> {
    WorkCase {
        world,
        facts,
        mode,
        take_over: false,
    }
}

#[test]
fn work_implements_a_single_issue_on_the_shared_pickup_task() -> TestResult {
    let world = World::new()?;
    let (plan, lease) = case(&world, open_issue(), solo()).run(&interactive("person")?)?;
    let task = issue_task_id(&issue(7)?)?;
    assert_eq!(
        plan,
        WorkPlan::Implement {
            task: task.clone(),
            adopted: false
        }
    );
    let lease = lease.ok_or("implement must hold a claim")?;
    assert_eq!(lease.trigger(), &Trigger::Interactive);
    let record = world.fixture.store.task(&task)?;
    assert_eq!(record.created_by().trigger, Trigger::Interactive);
    // Scheduled pickup derives the same task and is refused while the person
    // holds it, and is told the holder is interactive.
    assert_eq!(
        claim_issue(
            &world.fixture.store,
            &template()?,
            &issue(7)?,
            &scheduled("pickup-tick")?,
            ttl(600)?,
            world.now(),
        )?,
        ClaimOutcome::Held {
            trigger: Trigger::Interactive
        }
    );
    Ok(())
}

#[test]
fn the_same_person_rerunning_work_continues_their_claim() -> TestResult {
    let world = World::new()?;
    let (_, first) = case(&world, open_issue(), solo()).run(&interactive("person")?)?;
    let first = first.ok_or("first run must claim")?;
    world.clock.advance(30);
    let (plan, again) = case(&world, open_issue(), solo()).run(&interactive("person")?)?;
    assert!(matches!(plan, WorkPlan::Implement { adopted: false, .. }));
    let again = again.ok_or("a rerun must keep the claim")?;
    // Renewed, not replaced: same fence, later expiry.
    assert_eq!(again.fence(), first.fence());
    assert!(again.expires_at() > first.expires_at());
    // Another person is still refused.
    let (other, _) = case(&world, open_issue(), solo()).run(&interactive("other")?)?;
    assert!(matches!(other, WorkPlan::Skipped { .. }));
    Ok(())
}

#[test]
fn work_on_closed_or_finished_issues_is_idle_and_claims_nothing() -> TestResult {
    let world = World::new()?;
    let closed = IssueFacts {
        status: IssueStatus::Closed,
        ..open_issue()
    };
    let (plan, lease) = case(&world, closed, solo()).run(&interactive("person")?)?;
    assert_eq!(
        plan,
        WorkPlan::Idle {
            reason: Idle::IssueClosed
        }
    );
    assert!(lease.is_none());
    let done = IssueFacts {
        sub_issues: vec![sub(8, IssueStatus::Closed, false)?],
        ..open_issue()
    };
    let (plan, _) = case(&world, done, solo()).run(&interactive("person")?)?;
    assert_eq!(
        plan,
        WorkPlan::Idle {
            reason: Idle::SubIssuesDone
        }
    );
    assert!(world.fixture.store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn work_coordinates_sub_issues_and_fans_out_only_with_evidence() -> TestResult {
    let facts = IssueFacts {
        sub_issues: vec![
            sub(8, IssueStatus::Open, false)?,
            sub(9, IssueStatus::Open, true)?,
            sub(10, IssueStatus::Closed, false)?,
            sub(11, IssueStatus::Open, false)?,
        ],
        ..open_issue()
    };
    for (mode, fan_out) in [(ExecutionMode::FanOut, true), (solo(), false)] {
        let world = World::new()?;
        let (plan, _) = case(&world, facts.clone(), mode).run(&interactive("person")?)?;
        assert_eq!(
            plan,
            WorkPlan::Coordinate {
                task: issue_task_id(&issue(7)?)?,
                adopted: false,
                ready: vec![IssueNumber::new(8)?, IssueNumber::new(11)?],
                waiting: vec![IssueNumber::new(9)?],
                fan_out,
            }
        );
    }
    Ok(())
}

#[test]
fn work_proposes_a_split_that_needs_the_decomposition_workflow() -> TestResult {
    let world = World::new()?;
    let facts = IssueFacts {
        independent_parts: true,
        ..open_issue()
    };
    let (plan, _) = case(&world, facts, ExecutionMode::FanOut).run(&interactive("person")?)?;
    assert!(matches!(plan, WorkPlan::ProposeSplit { fan_out: true, .. }));
    // Existing sub-issues win over a proposed split.
    let world = World::new()?;
    let facts = IssueFacts {
        independent_parts: true,
        sub_issues: vec![sub(8, IssueStatus::Open, false)?],
        ..open_issue()
    };
    let (plan, _) = case(&world, facts, solo()).run(&interactive("person")?)?;
    assert!(matches!(plan, WorkPlan::Coordinate { .. }));
    // Without the workflow, a split is refused rather than faked.
    let refused = NoDecomposer.preview(&issue(7)?, b"{}");
    assert!(refused.is_err_and(|error| error.class() == ErrorClass::Refused));
    Ok(())
}

#[test]
fn work_skips_an_issue_scheduled_pickup_holds_until_it_is_handed_back() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let tick = scheduled("pickup-tick")?;
    let ClaimOutcome::Claimed(_) = claim_issue(
        store,
        &template()?,
        &issue(7)?,
        &tick,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("scheduled pickup must claim first".into());
    };
    let (plan, lease) = case(&world, open_issue(), solo()).run(&interactive("person")?)?;
    assert_eq!(
        plan,
        WorkPlan::Skipped {
            refusal: ClaimRefusal::Held {
                trigger: Trigger::Scheduled
            }
        }
    );
    assert!(lease.is_none());
    Ok(())
}

#[test]
fn a_hand_back_moves_ownership_with_a_recorded_adoption() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let task = issue_task_id(&issue(7)?)?;
    case(&world, open_issue(), solo()).run(&interactive("person")?)?;

    // Only the interactive holder can hand it back.
    assert_eq!(
        hand_back(store, &task, &holder("someone-else")?, world.now())?,
        HandBack::NotHeld
    );
    assert_eq!(
        hand_back(store, &task, &holder("person")?, world.now())?,
        HandBack::Released
    );
    assert_eq!(
        hand_back(store, &task, &holder("person")?, world.now())?,
        HandBack::NotHeld
    );
    // Scheduled pickup adopts it, recorded as an adoption.
    assert!(matches!(
        claim_issue(
            store,
            &template()?,
            &issue(7)?,
            &scheduled("pickup-tick")?,
            ttl(600)?,
            world.now()
        )?,
        ClaimOutcome::Adopted(_)
    ));
    // A scheduled holder's claim cannot be handed back from a session.
    assert_eq!(
        hand_back(store, &task, &holder("pickup-tick")?, world.now())?,
        HandBack::NotHeld
    );
    Ok(())
}

#[test]
fn a_person_adopts_handed_back_scheduled_work() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let tick = scheduled("pickup-tick")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &template()?,
        &issue(7)?,
        &tick,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("scheduled pickup must claim first".into());
    };
    store.relinquish(&issue_task_id(&issue(7)?)?, lease.fence(), world.now())?;
    let (plan, _) = case(&world, open_issue(), solo()).run(&interactive("person")?)?;
    assert!(matches!(plan, WorkPlan::Implement { adopted: true, .. }));
    Ok(())
}

#[test]
fn an_expired_claim_moves_only_through_an_explicit_takeover() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    claim_issue(
        store,
        &template()?,
        &issue(7)?,
        &scheduled("pickup-tick")?,
        ttl(60)?,
        world.now(),
    )?;
    world.clock.advance(120);
    let (plan, _) = case(&world, open_issue(), solo()).run(&interactive("person")?)?;
    assert_eq!(
        plan,
        WorkPlan::Skipped {
            refusal: ClaimRefusal::OwnerUncertain
        }
    );
    let mut takeover = case(&world, open_issue(), solo());
    takeover.take_over = true;
    let (plan, lease) = takeover.run(&interactive("person")?)?;
    assert!(matches!(plan, WorkPlan::Implement { .. }));
    assert!(lease.is_some());
    let record = store.task(&issue_task_id(&issue(7)?)?)?;
    assert!(matches!(
        record.ownership().last(),
        Some(kitchen::state::OwnershipEvent::TakenOver { .. })
    ));
    Ok(())
}

#[test]
fn entrypoints_refuse_unattended_claimants() -> TestResult {
    let world = World::new()?;
    let refused = case(&world, open_issue(), solo()).run(&scheduled("pickup-tick")?);
    assert!(matches!(
        refused,
        Err(Error::Interactive(
            kitchen::workflows::interactive::InteractiveError::NeedsPerson
        ))
    ));
    assert!(world.fixture.store.tasks()?.is_empty());
    Ok(())
}

// ------------------------------------------------------------------- pr

fn pr_facts(
    state: PullRequestState,
    mergeability: Mergeability,
    review: ReviewState,
) -> TestResult<PrFacts> {
    Ok(PrFacts {
        view: PullRequestView {
            number: IssueNumber::new(5)?,
            state,
            head: CommitId::new(&"e".repeat(40))?,
            head_branch: "lemarier/feature".to_owned(),
            base_branch: "main".to_owned(),
            mergeability,
        },
        review,
    })
}

fn run_pr(
    world: &World,
    facts: &PrFacts,
    intent: Option<PrIntent>,
    claimant: &kitchen::contracts::Claimant,
) -> TestResult<(PrPlan, Option<kitchen::state::Lease>)> {
    run_pr_within(world, facts, intent, claimant, None)
}

/// The budget of a house whose policy sets none.
fn house_budget() -> TestResult<FollowUpBudget> {
    budget_of(None)
}

/// The budget of a house allowing `fix_rounds`, taken from its config as in
/// production.
fn budget_of(fix_rounds: Option<u8>) -> TestResult<FollowUpBudget> {
    Ok(house_with_fix_rounds(fix_rounds)?.follow_up_budget())
}

fn run_pr_within(
    world: &World,
    facts: &PrFacts,
    intent: Option<PrIntent>,
    claimant: &kitchen::contracts::Claimant,
    fix_rounds: Option<u8>,
) -> TestResult<(PrPlan, Option<kitchen::state::Lease>)> {
    run_pr_with_house(world, facts, intent, claimant, fix_rounds, house_budget()?)
}

fn run_pr_with_house(
    world: &World,
    facts: &PrFacts,
    intent: Option<PrIntent>,
    claimant: &kitchen::contracts::Claimant,
    fix_rounds: Option<u8>,
    follow_up: FollowUpBudget,
) -> TestResult<(PrPlan, Option<kitchen::state::Lease>)> {
    Ok(pull_request(&PrRequest {
        store: &world.fixture.store,
        template: &template()?,
        repository: &repo()?,
        facts,
        intent,
        follow_up,
        fix_rounds,
        claimant,
        ttl: ttl(600)?,
        now: world.now(),
        take_over: false,
    })?)
}

/// Record writer round `round` of pull request #5 as scheduled repair would:
/// claimed by a scheduled claimant and, when `settle` is set, settled.
fn scheduled_round(world: &World, round: u8, settle: bool) -> TestResult<kitchen::TaskId> {
    let id = repair_task_id(&repo()?, IssueNumber::new(5)?, round)?;
    let store = &world.fixture.store;
    let claimant = scheduled("repair-tick")?;
    store.create_task(
        kitchen::contracts::TaskSpec {
            id: id.clone(),
            ..common::spec("placeholder")?
        },
        &claimant,
        world.now(),
    )?;
    let lease = store.claim(&id, &claimant, ttl(600)?, world.now())?;
    if settle {
        let kitchen::contracts::AttemptStart::Started(attempt) =
            store.start_attempt(&id, lease.fence(), world.now())?
        else {
            return Err("the round must start an attempt".into());
        };
        store.finish_attempt(
            &id,
            lease.fence(),
            attempt,
            kitchen::contracts::AttemptOutcome::Succeeded,
            world.now(),
        )?;
    }
    Ok(id)
}

#[test]
fn pr_routes_to_the_station_at_the_exact_head() -> TestResult {
    let world = World::new()?;
    let head = CommitId::new(&"e".repeat(40))?;
    let person = interactive("person")?;
    let open = |mergeability, review| pr_facts(PullRequestState::Open, mergeability, review);
    let (review, _) = run_pr(
        &world,
        &open(Mergeability::Clean, ReviewState::Unreviewed)?,
        None,
        &person,
    )?;
    assert_eq!(review, PrPlan::Review { head: head.clone() });
    let (gate, _) = run_pr(
        &world,
        &open(Mergeability::Behind, ReviewState::Reviewed)?,
        None,
        &person,
    )?;
    assert_eq!(gate, PrPlan::Gate { head: head.clone() });
    let (recheck, _) = run_pr(
        &world,
        &open(Mergeability::Unknown, ReviewState::Reviewed)?,
        None,
        &person,
    )?;
    assert_eq!(recheck, PrPlan::Recheck { head: head.clone() });
    // Read-only stations take no claim.
    assert!(world.fixture.store.tasks()?.is_empty());

    let (follow_up, lease) = run_pr(
        &world,
        &open(Mergeability::Clean, ReviewState::ChangesRequested)?,
        None,
        &person,
    )?;
    assert_eq!(
        follow_up,
        PrPlan::FollowUp {
            head,
            task: repair_task_id(&repo()?, IssueNumber::new(5)?, 1)?,
            round: 1
        }
    );
    assert!(lease.is_some());
    Ok(())
}

#[test]
fn pr_repair_claims_the_round_scheduled_repair_would_use() -> TestResult {
    let world = World::new()?;
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Conflicting,
        ReviewState::Reviewed,
    )?;
    let (plan, _) = run_pr(&world, &facts, None, &interactive("person")?)?;
    let task = repair_task_id(&repo()?, IssueNumber::new(5)?, 1)?;
    assert!(matches!(&plan, PrPlan::Repair { task: claimed, round: 1, .. } if claimed == &task));
    // Scheduled repair cannot claim the same round while the person holds it.
    let refused =
        world
            .fixture
            .store
            .claim(&task, &scheduled("repair-tick")?, ttl(600)?, world.now());
    assert!(matches!(
        refused,
        Err(Error::State(kitchen::state::StateError::ClaimHeld { .. }))
    ));
    // A second session is skipped, not doubled up.
    let (second, lease) = run_pr(&world, &facts, None, &interactive("other-person")?)?;
    assert_eq!(
        second,
        PrPlan::Skipped {
            refusal: ClaimRefusal::Held {
                trigger: Trigger::Interactive
            }
        }
    );
    assert!(lease.is_none());
    Ok(())
}

#[test]
fn pr_repair_round_carries_the_house_policy_agent() -> TestResult {
    let world = World::new()?;
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Conflicting,
        ReviewState::Reviewed,
    )?;
    let policy = workflows_support::agent_policy()?;
    let mut with_policy = template()?;
    with_policy.agents = Some(policy.clone());
    let (plan, _) = pull_request(&PrRequest {
        store: &world.fixture.store,
        template: &with_policy,
        repository: &repo()?,
        facts: &facts,
        intent: None,
        follow_up: house_budget()?,
        fix_rounds: None,
        claimant: &interactive("person")?,
        ttl: ttl(600)?,
        now: world.now(),
        take_over: false,
    })?;
    let PrPlan::Repair { task, .. } = plan else {
        return Err(format!("expected a repair round, got {plan:?}").into());
    };
    let expected = policy.resolve(&kitchen::selection::SelectionRequest {
        work_type: Some(WorkType::new("fix")?),
        repository: Some(repo()?),
        ..kitchen::selection::SelectionRequest::new(kitchen::contracts::Role::StationCook)
    });
    let recorded = world.fixture.store.task(&task)?;
    assert_eq!(recorded.spec().agent.as_ref(), Some(&expected));
    // A house without a policy still records no selection.
    let bare = World::new()?;
    let (bare_plan, _) = run_pr(&bare, &facts, None, &interactive("person")?)?;
    let PrPlan::Repair {
        task: bare_task, ..
    } = bare_plan
    else {
        return Err("expected a repair round".into());
    };
    assert_eq!(bare.fixture.store.task(&bare_task)?.spec().agent, None);
    Ok(())
}

#[test]
fn pr_repair_and_follow_up_rounds_record_the_fix_work_type_for_trust() -> TestResult {
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Conflicting,
        ReviewState::Reviewed,
    )?;
    let mut with_policy = template()?;
    with_policy.agents = Some(workflows_support::work_type_policy()?);
    for intent in [PrIntent::Repair, PrIntent::FollowUp] {
        let world = World::new()?;
        let (plan, _) = pull_request(&PrRequest {
            store: &world.fixture.store,
            template: &with_policy,
            repository: &repo()?,
            facts: &facts,
            intent: Some(intent),
            follow_up: house_budget()?,
            fix_rounds: None,
            claimant: &interactive("person")?,
            ttl: ttl(600)?,
            now: world.now(),
            take_over: false,
        })?;
        let task = match plan {
            PrPlan::Repair { task, .. } | PrPlan::FollowUp { task, .. } => task,
            other => return Err(format!("expected a writer round, got {other:?}").into()),
        };
        let stored = world.fixture.store.task(&task)?;
        let spec = stored.spec();
        let fix = WorkType::new("fix")?;
        assert_eq!(spec.work_type.as_ref(), Some(&fix), "{intent:?}");
        // Only the fix rule names this model: the selection was resolved for
        // the work type the task records.
        assert_eq!(
            spec.agent
                .as_ref()
                .and_then(|resolved| resolved.selection.model.as_ref()),
            Some(&AgentModel::new("sonnet")?),
            "{intent:?}"
        );
        assert!(matches!(
            workflows_support::bind_for_trust(&world.fixture, spec)?,
            Ok(true)
        ));
        assert_eq!(kitchen::trust::StationScope::of_task(spec)?.work_type, fix);
    }
    Ok(())
}

#[test]
fn pr_idle_budget_and_refusal_paths() -> TestResult {
    let world = World::new()?;
    let person = interactive("person")?;
    for (state, reason) in [
        (PullRequestState::Merged, Idle::Merged),
        (PullRequestState::Closed, Idle::Closed),
    ] {
        let facts = pr_facts(state, Mergeability::Conflicting, ReviewState::Unreviewed)?;
        let (plan, _) = run_pr(&world, &facts, Some(PrIntent::Repair), &person)?;
        assert_eq!(plan, PrPlan::Idle { reason });
    }
    let clean = pr_facts(
        PullRequestState::Open,
        Mergeability::Clean,
        ReviewState::Reviewed,
    )?;
    let (plan, _) = run_pr(&world, &clean, Some(PrIntent::Repair), &person)?;
    assert_eq!(
        plan,
        PrPlan::Idle {
            reason: Idle::NothingToRepair
        }
    );
    let refused = pull_request(&PrRequest {
        store: &world.fixture.store,
        template: &template()?,
        repository: &repo()?,
        facts: &clean,
        intent: None,
        follow_up: house_budget()?,
        fix_rounds: None,
        claimant: &scheduled("repair-tick")?,
        ttl: ttl(600)?,
        now: world.now(),
        take_over: false,
    });
    assert!(refused.is_err_and(|error| error.class() == ErrorClass::Refused));
    Ok(())
}

#[test]
fn pr_takes_the_round_from_durable_history_not_the_session() -> TestResult {
    let world = World::new()?;
    let person = interactive("person")?;
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Clean,
        ReviewState::ChangesRequested,
    )?;
    // Scheduled repair already spent round 1. The facts carry no round
    // count, so nothing the session read can send the person back to it.
    let spent = scheduled_round(&world, 1, true)?;
    let (plan, lease) = run_pr(&world, &facts, None, &person)?;
    let next = repair_task_id(&repo()?, IssueNumber::new(5)?, 2)?;
    assert_eq!(
        plan,
        PrPlan::FollowUp {
            head: facts.view.head.clone(),
            task: next.clone(),
            round: 2
        }
    );
    assert!(lease.is_some());
    assert!(matches!(
        world.fixture.store.task(&spent)?.state(),
        TaskState::Settled { .. }
    ));
    Ok(())
}

#[test]
fn pr_skips_a_round_scheduled_repair_is_writing() -> TestResult {
    let world = World::new()?;
    let person = interactive("person")?;
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Conflicting,
        ReviewState::Reviewed,
    )?;
    scheduled_round(&world, 1, true)?;
    let writing = scheduled_round(&world, 2, false)?;
    let (plan, lease) = run_pr(&world, &facts, Some(PrIntent::Repair), &person)?;
    assert_eq!(
        plan,
        PrPlan::Skipped {
            refusal: ClaimRefusal::Held {
                trigger: Trigger::Scheduled
            }
        }
    );
    assert!(lease.is_none());
    // No second writer round was opened beside the scheduled one.
    assert_eq!(world.fixture.store.tasks()?.len(), 2);
    let record = world.fixture.store.task(&writing)?;
    let TaskState::Claimed { lease } = record.state() else {
        return Err("scheduled repair must keep its claim".into());
    };
    assert_eq!(*lease.trigger(), Trigger::Scheduled);
    Ok(())
}

#[test]
fn pr_fix_rounds_can_only_lower_the_house_budget() -> TestResult {
    let world = World::new()?;
    let person = interactive("person")?;
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Clean,
        ReviewState::ChangesRequested,
    )?;
    // Asking for more rounds than the house allows is refused outright.
    let raised = run_pr_within(
        &world,
        &facts,
        None,
        &person,
        Some(DEFAULT_FIX_ROUNDS.saturating_add(1)),
    );
    let Err(error) = raised else {
        return Err("a raised budget must be refused".into());
    };
    assert_eq!(
        error.downcast_ref::<Error>().map(Error::class),
        Some(ErrorClass::Refused)
    );
    assert!(world.fixture.store.tasks()?.is_empty());

    // A zero budget is spent before any round.
    let (plan, lease) = run_pr_within(&world, &facts, None, &person, Some(0))?;
    assert_eq!(plan, PrPlan::BudgetExhausted { rounds_used: 0 });
    assert!(lease.is_none());

    // A lowered budget counts the rounds already in the store.
    scheduled_round(&world, 1, true)?;
    let (plan, _) = run_pr_within(&world, &facts, None, &person, Some(1))?;
    assert_eq!(plan, PrPlan::BudgetExhausted { rounds_used: 1 });

    // The house budget is the ceiling without the flag.
    for round in 2..=DEFAULT_FIX_ROUNDS {
        scheduled_round(&world, round, true)?;
    }
    let (plan, lease) = run_pr(&world, &facts, None, &person)?;
    assert_eq!(
        plan,
        PrPlan::BudgetExhausted {
            rounds_used: DEFAULT_FIX_ROUNDS
        }
    );
    assert!(lease.is_none());
    assert_eq!(
        world.fixture.store.tasks()?.len(),
        usize::from(DEFAULT_FIX_ROUNDS)
    );
    Ok(())
}

#[test]
fn pr_reads_the_house_fix_round_budget() -> TestResult {
    let world = World::new()?;
    let person = interactive("person")?;
    let facts = pr_facts(
        PullRequestState::Open,
        Mergeability::Clean,
        ReviewState::ChangesRequested,
    )?;
    let house = |fix_rounds| budget_of(Some(fix_rounds));
    // A session cannot raise the house's own budget, only the default's.
    let raised = run_pr_with_house(&world, &facts, None, &person, Some(4), house(3)?);
    let Err(error) = raised else {
        return Err("a budget above the house's must be refused".into());
    };
    assert_eq!(
        error.downcast_ref::<Error>().map(Error::class),
        Some(ErrorClass::Refused)
    );
    assert!(world.fixture.store.tasks()?.is_empty());

    // Rounds beyond the library default stay open while the house allows them.
    for round in 1..=DEFAULT_FIX_ROUNDS {
        scheduled_round(&world, round, true)?;
    }
    let (plan, lease) = run_pr_with_house(&world, &facts, None, &person, None, house(3)?)?;
    assert!(
        matches!(plan, PrPlan::FollowUp { round: 3, .. }),
        "{plan:?}"
    );
    assert!(lease.is_some());

    // A house that allows fewer rounds than the default is spent earlier.
    let strict = World::new()?;
    scheduled_round(&strict, 1, true)?;
    let (plan, lease) = run_pr_with_house(&strict, &facts, None, &person, None, house(1)?)?;
    assert_eq!(plan, PrPlan::BudgetExhausted { rounds_used: 1 });
    assert!(lease.is_none());
    Ok(())
}

// --------------------------------------------------------------- drafts

#[derive(Clone, Copy)]
enum Fault {
    Reject,
    LoseAfterApply,
    LoseBeforeApply,
}

#[derive(Default)]
struct Forge {
    applied: Vec<(IdempotencyKey, GitHubAction, Receipt)>,
    calls: u32,
    next_issue: u64,
    faults: Vec<Option<Fault>>,
    /// Lookups cannot establish any outcome.
    blind: bool,
}

/// An in-memory forge: applies GitHub mutations, answers lookups by key.
struct MemoryForge {
    descriptor: BackendDescriptor,
    budget: u32,
    state: RefCell<Forge>,
}

impl MemoryForge {
    fn new(budget: u32) -> TestResult<Self> {
        Ok(Self {
            descriptor: BackendDescriptor {
                backend: backend_id()?,
                house: house()?,
                capabilities: CapabilitySet::supporting([
                    Capability::ForgeMutation,
                    Capability::EffectLookup,
                ]),
                worker_selection: None,
            },
            budget,
            state: RefCell::new(Forge {
                next_issue: 100,
                ..Forge::default()
            }),
        })
    }

    /// Faults for the next submissions, in order; `None` succeeds.
    fn plan_faults(&self, faults: Vec<Option<Fault>>) {
        self.state.borrow_mut().faults = faults;
    }

    fn actions(&self) -> Vec<GitHubAction> {
        self.state
            .borrow()
            .applied
            .iter()
            .map(|(_, action, _)| action.clone())
            .collect()
    }

    fn calls(&self) -> u32 {
        self.state.borrow().calls
    }

    fn blind(&self, blind: bool) {
        self.state.borrow_mut().blind = blind;
    }
}

impl EffectExecutor for MemoryForge {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let rejected = EffectFailure::NotApplied(NotAppliedReason::Rejected);
        let Effect::GitHub(effect) = request.effect() else {
            return Err(rejected);
        };
        let mut state = self.state.borrow_mut();
        state.calls += 1;
        let fault = if state.faults.is_empty() {
            None
        } else {
            state.faults.remove(0)
        };
        match fault {
            Some(Fault::Reject) => return Err(rejected),
            Some(Fault::LoseBeforeApply) => {
                return Err(EffectFailure::Uncertain(UncertainReason::Timeout));
            }
            Some(Fault::LoseAfterApply) | None => {}
        }
        let reference = match &effect.mutation.action {
            GitHubAction::CreateIssue { .. } => {
                state.next_issue += 1;
                format!(
                    "https://github.com/{}/issues/{}",
                    effect.mutation.repository, state.next_issue
                )
            }
            _ => format!("forge-{}", state.applied.len()),
        };
        let receipt = Receipt::new(
            ExternalRef::new(&reference).map_err(|_| rejected)?,
            Vec::new(),
            Vec::new(),
        )
        .map_err(|_| rejected)?;
        state.applied.push((
            request.key().clone(),
            effect.mutation.action.clone(),
            receipt.clone(),
        ));
        if matches!(fault, Some(Fault::LoseAfterApply)) {
            return Err(EffectFailure::Uncertain(UncertainReason::ResponseLost));
        }
        Ok(receipt)
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        if self.state.borrow().blind {
            return Ok(Lookup::Unknown);
        }
        Ok(self
            .state
            .borrow()
            .applied
            .iter()
            .find(|(key, _, _)| key == request.key())
            .map_or(Lookup::Absent, |(_, _, receipt)| {
                Lookup::Applied(receipt.clone())
            }))
    }
}

impl ForgeWriter for MemoryForge {
    fn github_effect(&self, mutation: GitHubMutation) -> kitchen::Result<GitHubEffect> {
        Ok(GitHubEffect {
            requester: ExternalRef::new("sample-bot")?,
            mutation,
            posting_budget: PostingBudget::new(self.budget)?,
        })
    }
}

/// Policy limits that let a person approve forge writes; no standing grants.
fn forge_limits() -> TestResult<HouseGrants> {
    let limits = [
        Permission::CreateIssue,
        Permission::PostComment,
        Permission::EditLabels,
        Permission::EditIssueRelationships,
    ]
    .into_iter()
    .map(|permission| Grant::house(permission, backend_id().ok()?, credential().ok()?).into())
    .collect::<Option<Vec<Grant>>>()
    .ok_or("grant")?;
    Ok(HouseGrants::with_limits(house()?, limits, [])?)
}

fn new_issue_draft() -> TestResult<IssueDraft> {
    Ok(IssueDraft {
        repository: repo()?,
        target: DraftTarget::New {
            title: "Add flash retry".to_owned(),
            body: "## Outcome\nRetries flashing once.\n\n## Acceptance criteria\n- [ ] Retry once."
                .to_owned(),
        },
        add_labels: vec!["ready".to_owned(), "firmware".to_owned()],
        remove_labels: Vec::new(),
        blocked_by: vec![IssueNumber::new(12)?],
        questions: Vec::new(),
    })
}

fn refine_draft() -> TestResult<IssueDraft> {
    Ok(IssueDraft {
        repository: repo()?,
        target: DraftTarget::Refine {
            issue: IssueNumber::new(72)?,
            comment: "Sharpened acceptance criteria:\n- [ ] Derive work type from labels."
                .to_owned(),
        },
        add_labels: vec!["ready".to_owned()],
        remove_labels: vec!["needs-spec".to_owned()],
        blocked_by: Vec::new(),
        questions: Vec::new(),
    })
}

fn approval(draft: &IssueDraft) -> TestResult<DraftApproval> {
    Ok(DraftApproval {
        id: ExternalRef::new("approval-1")?,
        given_by: holder("person")?,
        digest: draft_preview(draft)?.digest,
    })
}

fn options() -> TestResult<DraftOptions> {
    Ok(DraftOptions {
        provenance: provenance('a')?,
        lease: ttl(600)?,
    })
}

struct Desk {
    world: World,
    forge: MemoryForge,
    grants: HouseGrants,
}

impl Desk {
    fn new(budget: u32) -> TestResult<Self> {
        Ok(Self {
            world: World::new()?,
            forge: MemoryForge::new(budget)?,
            grants: forge_limits()?,
        })
    }

    fn apply(
        &self,
        draft: &IssueDraft,
        approval: Option<&DraftApproval>,
    ) -> kitchen::Result<kitchen::workflows::interactive::DraftReport> {
        let writer = DraftWriter {
            store: &self.world.fixture.store,
            forge: &self.forge,
            grants: &self.grants,
            clock: &self.world.clock,
        };
        let person = kitchen::contracts::Claimant::interactive(kitchen::HolderId::new("person")?);
        let options = DraftOptions {
            provenance: Provenance {
                kitchen: CommitId::new(&"a".repeat(40))?,
                house_guidance: CommitId::new(&"b".repeat(40))?,
                repository_instructions: None,
            },
            lease: kitchen::contracts::LeaseTtl::new(std::time::Duration::from_secs(600))?,
        };
        apply_draft(&writer, draft, approval, &person, &options)
    }

    /// Acknowledge `task` as `claimant`, re-reading through the forge when
    /// `reread` is set.
    fn acknowledge(
        &self,
        task: &kitchen::TaskId,
        reason: &str,
        accept_unknown: bool,
        claimant: &kitchen::contracts::Claimant,
        reread: bool,
    ) -> kitchen::Result<AcknowledgeReport> {
        let forge: &dyn EffectExecutor = &self.forge;
        acknowledge_draft(
            &self.world.fixture.store,
            reread.then_some(forge),
            task,
            &Acknowledgement {
                reason: reason.parse()?,
                accept_unknown,
            },
            claimant,
            &self.world.clock,
        )
    }
}

#[test]
fn a_preview_lists_every_write_in_order_and_binds_a_digest() -> TestResult {
    let preview = draft_preview(&new_issue_draft()?)?;
    assert_eq!(
        preview.writes,
        vec![
            PlannedWrite::CreateIssue {
                title: "Add flash retry".to_owned(),
                body: "## Outcome\nRetries flashing once.\n\n## Acceptance criteria\n- [ ] Retry once.".to_owned(),
            },
            PlannedWrite::Label {
                label: "ready".to_owned(),
                present: true
            },
            PlannedWrite::Label {
                label: "firmware".to_owned(),
                present: true
            },
            PlannedWrite::BlockedBy {
                blocker: IssueNumber::new(12)?
            },
        ]
    );
    assert!(preview.ready());
    let rendered = preview.render();
    assert!(rendered.contains("Create issue \"Add flash retry\""));
    assert!(rendered.contains("Mark blocked by #12"));
    assert!(rendered.contains(preview.digest.as_str()));
    // Any change to what would be written changes the digest.
    let mut edited = new_issue_draft()?;
    edited.add_labels.pop();
    assert_ne!(draft_preview(&edited)?.digest, preview.digest);
    // The same draft always previews the same.
    assert_eq!(draft_preview(&new_issue_draft()?)?, preview);
    Ok(())
}

#[test]
fn malformed_drafts_are_refused_by_field() -> TestResult {
    let field = |draft: &IssueDraft| match draft_preview(draft) {
        Err(Error::Interactive(
            kitchen::workflows::interactive::InteractiveError::InvalidDraft(field),
        )) => Some(field),
        _ => None,
    };
    let mut draft = new_issue_draft()?;
    draft.target = DraftTarget::New {
        title: " padded ".to_owned(),
        body: "b".to_owned(),
    };
    assert_eq!(field(&draft), Some("title"));
    draft.target = DraftTarget::New {
        title: "t".to_owned(),
        body: "   ".to_owned(),
    };
    assert_eq!(field(&draft), Some("body"));
    let mut draft = new_issue_draft()?;
    draft.remove_labels = vec!["needs-spec".to_owned()];
    assert_eq!(field(&draft), Some("removeLabels"));
    let mut draft = refine_draft()?;
    draft.add_labels.push("needs-spec".to_owned());
    assert_eq!(field(&draft), Some("labels"));
    let mut draft = refine_draft()?;
    draft.blocked_by = vec![IssueNumber::new(72)?];
    assert_eq!(field(&draft), Some("blockedBy"));
    let mut draft = refine_draft()?;
    draft.blocked_by = vec![IssueNumber::new(3)?, IssueNumber::new(3)?];
    assert_eq!(field(&draft), Some("blockedBy"));
    let mut draft = refine_draft()?;
    draft.questions = vec!["line\nbreak".to_owned()];
    assert_eq!(field(&draft), Some("questions"));
    // Boundary: exactly the label bound passes, one more is refused.
    let mut draft = refine_draft()?;
    draft.remove_labels.clear();
    draft.add_labels = (0..MAX_DRAFT_LABELS).map(|n| format!("l{n}")).collect();
    assert!(draft_preview(&draft).is_ok());
    draft.add_labels.push("one-more".to_owned());
    assert_eq!(field(&draft), Some("labels"));
    // Unknown JSON keys are refused at the boundary.
    let json = r#"{"repository":"origin89hq/firmware","target":{"type":"new","title":"t","body":"b"},"grants":[]}"#;
    assert!(serde_json::from_str::<IssueDraft>(json).is_err());
    Ok(())
}

#[test]
fn a_new_issue_is_written_only_after_approval_with_consent_per_write() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;

    // Declined: nothing is written, no task exists.
    let declined = desk.apply(&draft, None)?;
    assert_eq!(declined.outcome, DraftOutcome::Declined);
    assert_eq!(desk.forge.calls(), 0);
    assert!(desk.world.fixture.store.tasks()?.is_empty());

    let report = desk.apply(&draft, Some(&approval(&draft)?))?;
    assert_eq!(report.outcome, DraftOutcome::Completed);
    let created = IssueNumber::new(101)?;
    assert_eq!(report.issue, Some(created));
    assert_eq!(
        desk.forge.actions(),
        vec![
            GitHubAction::CreateIssue {
                title: kitchen::contracts::Text::new("Add flash retry")?,
                body: kitchen::contracts::Text::new(
                    "## Outcome\nRetries flashing once.\n\n## Acceptance criteria\n- [ ] Retry once."
                )?,
            },
            GitHubAction::SetLabel {
                issue: created,
                label: "ready".to_owned(),
                present: true
            },
            GitHubAction::SetLabel {
                issue: created,
                label: "firmware".to_owned(),
                present: true
            },
            GitHubAction::LinkDependency {
                issue: created,
                blocker: IssueNumber::new(12)?
            },
        ]
    );
    // Every write is authorized by the person's consent; the task holds no
    // standing authority, so nothing became a grant.
    let task = desk
        .world
        .fixture
        .store
        .task(&report.task.clone().ok_or("task")?)?;
    assert_eq!(task.spec().authority.grants().count(), 0);
    let person = holder("person")?;
    assert_eq!(task.effects().len(), 4);
    assert!(task.effects().iter().all(|effect| matches!(
        effect.authorization(),
        Authorization::Consent { given_by, .. } if given_by == &person
    )));
    // Each write names the approved preview it rested on.
    let digest = ExternalRef::new(draft_preview(&draft)?.digest.as_str())?;
    assert!(
        task.effects()
            .iter()
            .all(|effect| effect.basis() == Some(&digest))
    );
    assert!(matches!(
        task.state(),
        TaskState::Settled {
            settlement: Settlement::Succeeded,
            ..
        }
    ));
    // A rerun of the completed draft posts nothing new.
    let rerun = desk.apply(&draft, Some(&approval(&draft)?))?;
    assert_eq!(rerun.outcome, DraftOutcome::Completed);
    assert!(rerun.written.iter().all(|written| written.reused));
    assert_eq!(rerun.issue, Some(created));
    assert_eq!(desk.forge.actions().len(), 4);
    Ok(())
}

#[test]
fn stale_or_unready_approvals_write_nothing() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = refine_draft()?;
    let mut edited = refine_draft()?;
    edited.add_labels.push("firmware".to_owned());
    // The person approved the earlier text; the draft changed since.
    let stale = desk.apply(&edited, Some(&approval(&draft)?))?;
    assert_eq!(stale.outcome, DraftOutcome::StaleApproval);
    let mut open = refine_draft()?;
    open.questions = vec!["Derive work type from labels or changed paths?".to_owned()];
    let unready = desk.apply(&open, Some(&approval(&open)?))?;
    assert_eq!(unready.outcome, DraftOutcome::NotReady);
    let tight = Desk::new(2)?;
    let over = tight.apply(&draft, Some(&approval(&draft)?))?;
    assert_eq!(
        over.outcome,
        DraftOutcome::OverBudget {
            needed: 3,
            limit: 2
        }
    );
    assert_eq!(desk.forge.calls() + tight.forge.calls(), 0);
    Ok(())
}

#[test]
fn a_rerun_after_partial_posting_completes_without_duplicates() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = refine_draft()?;
    let approved = approval(&draft)?;
    // The comment lands, then the forge refuses the first label.
    desk.forge.plan_faults(vec![None, Some(Fault::Reject)]);
    let partial = desk.apply(&draft, Some(&approved))?;
    assert!(matches!(partial.outcome, DraftOutcome::NotApplied { .. }));
    assert_eq!(desk.forge.actions().len(), 1);

    desk.world.clock.advance(1);
    let rerun = desk.apply(&draft, Some(&approved))?;
    assert_eq!(rerun.outcome, DraftOutcome::Completed);
    let comments = desk
        .forge
        .actions()
        .iter()
        .filter(|action| matches!(action, GitHubAction::PostComment { .. }))
        .count();
    assert_eq!(comments, 1, "the comment must not be posted twice");
    assert_eq!(desk.forge.actions().len(), 3);
    assert_eq!(
        rerun
            .written
            .iter()
            .map(|written| written.reused)
            .collect::<Vec<_>>(),
        vec![true, false, false]
    );
    Ok(())
}

#[test]
fn a_lost_response_is_reconciled_before_anything_is_resubmitted() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let approved = approval(&draft)?;
    // The issue is created but the response is lost.
    desk.forge.plan_faults(vec![Some(Fault::LoseAfterApply)]);
    let lost = desk.apply(&draft, Some(&approved))?;
    assert!(matches!(lost.outcome, DraftOutcome::Uncertain { .. }));
    assert_eq!(desk.forge.actions().len(), 1);

    desk.world.clock.advance(1);
    let rerun = desk.apply(&draft, Some(&approved))?;
    assert_eq!(rerun.outcome, DraftOutcome::Completed);
    let creates = desk
        .forge
        .actions()
        .iter()
        .filter(|action| matches!(action, GitHubAction::CreateIssue { .. }))
        .count();
    assert_eq!(creates, 1, "reconciliation must find the created issue");
    assert_eq!(rerun.issue, Some(IssueNumber::new(101)?));

    // A request lost before it applied is proven absent and resubmitted once.
    let desk = Desk::new(10)?;
    desk.forge.plan_faults(vec![Some(Fault::LoseBeforeApply)]);
    let lost = desk.apply(&draft, Some(&approved))?;
    assert!(matches!(lost.outcome, DraftOutcome::Uncertain { .. }));
    desk.world.clock.advance(1);
    let rerun = desk.apply(&draft, Some(&approved))?;
    assert_eq!(rerun.outcome, DraftOutcome::Completed);
    assert_eq!(desk.forge.actions().len(), 4);
    Ok(())
}

#[test]
fn a_revised_draft_waits_for_the_unfinished_one_on_the_same_issue() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = refine_draft()?;
    desk.forge.plan_faults(vec![None, Some(Fault::Reject)]);
    let partial = desk.apply(&draft, Some(&approval(&draft)?))?;
    assert!(matches!(partial.outcome, DraftOutcome::NotApplied { .. }));

    let mut revised = refine_draft()?;
    revised.add_labels = vec!["ready".to_owned(), "firmware".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(
        blocked.outcome,
        DraftOutcome::EarlierUnfinished {
            task: draft_task_id(&draft_preview(&draft)?)?
        }
    );
    assert_eq!(desk.forge.actions().len(), 1);

    // Another issue is a different subject and is not blocked.
    let mut other = refine_draft()?;
    other.target = DraftTarget::Refine {
        issue: IssueNumber::new(73)?,
        comment: "Refined.".to_owned(),
    };
    let independent = desk.apply(&other, Some(&approval(&other)?))?;
    assert_eq!(independent.outcome, DraftOutcome::Completed);

    // Once the first draft completes, a revision may proceed.
    desk.apply(&draft, Some(&approval(&draft)?))?;
    let after = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(after.outcome, DraftOutcome::Completed);
    Ok(())
}

#[test]
fn a_draft_that_settled_after_writing_keeps_its_subject() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = refine_draft()?;
    let approved = approval(&draft)?;
    // The comment lands, then the forge refuses the first label on every
    // attempt until the task exhausts its attempts and settles.
    desk.forge.plan_faults(vec![None, Some(Fault::Reject)]);
    desk.apply(&draft, Some(&approved))?;
    let settled = loop {
        desk.world.clock.advance(1);
        desk.forge.plan_faults(vec![Some(Fault::Reject)]);
        let rerun = desk.apply(&draft, Some(&approved))?;
        if let DraftOutcome::Settled { settlement } = rerun.outcome {
            break settlement;
        }
        if desk.forge.calls() > 10 {
            return Err("the draft never settled".into());
        }
    };
    assert_eq!(settled, Settlement::Exhausted);
    let posted = desk.forge.actions().len();
    assert_eq!(posted, 1, "only the comment reached the forge");
    desk.forge.plan_faults(Vec::new());

    // A revision would post the same comment again; it is refused and names
    // the settled task and its applied write, never the refused label.
    let mut revised = refine_draft()?;
    revised.add_labels = vec!["ready".to_owned(), "firmware".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(
        blocked.outcome,
        DraftOutcome::EarlierSettledWithWrites {
            task: draft_task_id(&draft_preview(&draft)?)?,
            settlement: Settlement::Exhausted,
            writes: vec![kitchen::EffectName::new("comment")?],
        }
    );
    assert_eq!(desk.forge.actions().len(), posted);
    Ok(())
}

#[test]
fn a_draft_that_settled_without_writing_frees_its_subject() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = refine_draft()?;
    let approved = approval(&draft)?;
    // The forge refuses the comment on every attempt: nothing was posted.
    let settled = loop {
        desk.forge.plan_faults(vec![Some(Fault::Reject)]);
        let run = desk.apply(&draft, Some(&approved))?;
        if let DraftOutcome::Settled { settlement } = run.outcome {
            break settlement;
        }
        if desk.forge.calls() > 10 {
            return Err("the draft never settled".into());
        }
        desk.world.clock.advance(1);
    };
    assert_eq!(settled, Settlement::Exhausted);
    assert!(desk.forge.actions().is_empty());
    desk.forge.plan_faults(Vec::new());

    let mut revised = refine_draft()?;
    revised.add_labels = vec!["ready".to_owned(), "firmware".to_owned()];
    let after = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(after.outcome, DraftOutcome::Completed);
    Ok(())
}

#[test]
fn drafts_refuse_unattended_claimants() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = refine_draft()?;
    let writer = DraftWriter {
        store: &desk.world.fixture.store,
        forge: &desk.forge,
        grants: &desk.grants,
        clock: &desk.world.clock,
    };
    let refused = apply_draft(
        &writer,
        &draft,
        Some(&approval(&draft)?),
        &scheduled("gardener-tick")?,
        &options()?,
    );
    assert!(refused.is_err_and(|error| error.class() == ErrorClass::Refused));
    assert_eq!(desk.forge.calls(), 0);
    Ok(())
}

/// Settle a new-issue draft whose issue was created but whose first label
/// the forge refuses on every attempt, and return its task.
fn settle_after_create(desk: &Desk, draft: &IssueDraft) -> TestResult<kitchen::TaskId> {
    let approved = approval(draft)?;
    desk.forge.plan_faults(vec![None, Some(Fault::Reject)]);
    desk.apply(draft, Some(&approved))?;
    loop {
        desk.world.clock.advance(1);
        desk.forge.plan_faults(vec![Some(Fault::Reject)]);
        if let DraftOutcome::Settled { settlement } = desk.apply(draft, Some(&approved))?.outcome {
            assert_eq!(settlement, Settlement::Exhausted);
            break;
        }
        if desk.forge.calls() > 10 {
            return Err("the draft never settled".into());
        }
    }
    desk.forge.plan_faults(Vec::new());
    Ok(draft_task_id(&draft_preview(draft)?)?)
}

#[test]
fn a_stuck_new_issue_draft_blocks_only_its_own_title() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let stuck = settle_after_create(&desk, &draft)?;

    // A revision of the same new issue could create it twice: refused.
    let mut revised = new_issue_draft()?;
    revised.add_labels = vec!["ready".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(
        blocked.outcome,
        DraftOutcome::EarlierSettledWithWrites {
            task: stuck,
            settlement: Settlement::Exhausted,
            writes: vec![kitchen::EffectName::new("create")?],
        }
    );

    // A different new issue in the same repository is not that draft.
    let mut other = new_issue_draft()?;
    other.target = DraftTarget::New {
        title: "Log flash attempts".to_owned(),
        body: "## Outcome\nLogs each attempt.".to_owned(),
    };
    let independent = desk.apply(&other, Some(&approval(&other)?))?;
    assert_eq!(independent.outcome, DraftOutcome::Completed);
    Ok(())
}

fn created(number: u64) -> TestResult<ReadBack> {
    Ok(ReadBack::Applied {
        reference: ExternalRef::new(&format!("https://github.com/{}/issues/{number}", repo()?))?,
    })
}

#[test]
fn an_owner_acknowledgement_releases_a_settled_draft_subject() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let stuck = settle_after_create(&desk, &draft)?;
    let mut revised = new_issue_draft()?;
    revised.add_labels = vec!["ready".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert!(matches!(
        blocked.outcome,
        DraftOutcome::EarlierSettledWithWrites { .. }
    ));

    let person = interactive("person")?;
    let reason = "Closed #101 by hand; post the revision as a new issue.";
    desk.world.clock.advance(5);
    let at = kitchen::contracts::Clock::now(&desk.world.clock);
    let report = desk.acknowledge(&stuck, reason, false, &person, true)?;
    assert_eq!(report.task, stuck);
    assert_eq!(
        report.writes,
        vec![WriteReadBack {
            effect: kitchen::EffectName::new("create")?,
            state: created(101)?,
        }]
    );
    let AcknowledgeOutcome::Recorded(recorded) = report.outcome else {
        return Err(format!("not recorded: {:?}", report.outcome).into());
    };
    assert_eq!(recorded.by, holder("person")?);
    assert_eq!(recorded.at, at);
    assert_eq!(recorded.reason.as_str(), reason);
    assert!(recorded.unresolved.is_empty());
    // The store holds it on the settled task itself.
    let store = &desk.world.fixture.store;
    assert_eq!(store.task(&stuck)?.write_acknowledgement(), Some(&recorded));

    // The subject is released: the revised draft, a new digest, proceeds.
    let after = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(after.outcome, DraftOutcome::Completed);
    assert_eq!(after.issue, Some(IssueNumber::new(102)?));

    // The acknowledgement is durable and recorded once: a repeat keeps who,
    // when, and why of the first.
    desk.world.clock.advance(60);
    let again = desk.acknowledge(
        &stuck,
        "Another reason.",
        false,
        &interactive("other")?,
        true,
    )?;
    let AcknowledgeOutcome::AlreadyRecorded(first) = again.outcome else {
        return Err(format!("not kept: {:?}", again.outcome).into());
    };
    assert_eq!(first, recorded);
    Ok(())
}

#[test]
fn scheduled_claimants_cannot_acknowledge_a_draft() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let stuck = settle_after_create(&desk, &draft)?;
    let refused = desk.acknowledge(
        &stuck,
        "Tick cleanup.",
        true,
        &scheduled("gardener-tick")?,
        true,
    );
    assert!(refused.is_err_and(|error| error.class() == ErrorClass::Refused));
    // Nothing was recorded: a revision is still refused.
    let mut revised = new_issue_draft()?;
    revised.add_labels = vec!["ready".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert!(matches!(
        blocked.outcome,
        DraftOutcome::EarlierSettledWithWrites { .. }
    ));
    Ok(())
}

/// A new-issue draft whose create response was lost (`fault`, before or
/// after the forge applied it) and that its owner handed over and
/// cancelled: settled, with the create's outcome unknown.
fn settle_with_unknown_create(
    desk: &Desk,
    draft: &IssueDraft,
    fault: Fault,
) -> TestResult<kitchen::TaskId> {
    desk.forge.plan_faults(vec![Some(fault)]);
    let lost = desk.apply(draft, Some(&approval(draft)?))?;
    assert!(matches!(lost.outcome, DraftOutcome::Uncertain { .. }));
    let id = draft_task_id(&draft_preview(draft)?)?;
    let store = &desk.world.fixture.store;
    let now = kitchen::contracts::Clock::now(&desk.world.clock);
    let lease = store.claim(&id, &interactive("person")?, ttl(600)?, now)?;
    let record = store.task(&id)?;
    let effect = record.effects().first().ok_or("effect")?;
    store.record_effect_outcome(
        &id,
        lease.fence(),
        effect.seq(),
        EffectOutcome::Unresolvable,
        now,
    )?;
    store.accept_risk(
        &id,
        lease.fence(),
        effect.seq(),
        RiskDecision {
            effect: effect.request().key().clone(),
            decided_by: holder("person")?,
            revision: record.evidence().revision(),
            action: RiskAction::SettleUnsuccessfully,
        },
        now,
    )?;
    store.request_cancel(&id, &holder("person")?, now)?;
    store.settle_cancelled(&id, lease.fence(), now)?;
    Ok(id)
}

#[test]
fn an_unknown_write_after_rereading_needs_explicit_acceptance() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let stuck = settle_with_unknown_create(&desk, &draft, Fault::LoseAfterApply)?;
    let mut revised = new_issue_draft()?;
    revised.add_labels = vec!["ready".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(
        blocked.outcome,
        DraftOutcome::EarlierSettledWithWrites {
            task: stuck.clone(),
            settlement: Settlement::Cancelled,
            writes: vec![kitchen::EffectName::new("create")?],
        }
    );
    let person = interactive("person")?;
    let create = vec![kitchen::EffectName::new("create")?];

    // The forge cannot tell, and no forge at all proves nothing either: the
    // write stays unknown and nothing is recorded without acceptance.
    desk.forge.blind(true);
    for reread in [true, false] {
        let report = desk.acknowledge(&stuck, "Checked by hand.", false, &person, reread)?;
        assert_eq!(
            report.outcome,
            AcknowledgeOutcome::Unknown {
                writes: create.clone()
            }
        );
        assert_eq!(
            report.writes.first().map(|write| &write.state),
            Some(&ReadBack::Unknown)
        );
    }
    let still = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert!(matches!(
        still.outcome,
        DraftOutcome::EarlierSettledWithWrites { .. }
    ));

    // Accepting the unknown write, with a reason, releases the subject.
    let reason = "No issue titled Add flash retry exists; the create never landed.";
    let report = desk.acknowledge(&stuck, reason, true, &person, true)?;
    let AcknowledgeOutcome::Recorded(recorded) = report.outcome else {
        return Err(format!("not recorded: {:?}", report.outcome).into());
    };
    assert_eq!(recorded.unresolved, create);
    desk.forge.blind(false);
    let after = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(after.outcome, DraftOutcome::Completed);

    // Where the forge proves the outcome, the re-read resolves it and no
    // acceptance is needed; the proof is recorded on the task.
    let desk = Desk::new(10)?;
    let stuck = settle_with_unknown_create(&desk, &draft, Fault::LoseAfterApply)?;
    let report = desk.acknowledge(&stuck, "Keep #101.", false, &person, true)?;
    assert_eq!(
        report.writes.first().map(|write| write.state.clone()),
        Some(created(101)?)
    );
    let AcknowledgeOutcome::Recorded(recorded) = report.outcome else {
        return Err(format!("not recorded: {:?}", report.outcome).into());
    };
    assert!(recorded.unresolved.is_empty());
    let record = desk.world.fixture.store.task(&stuck)?;
    assert!(
        record
            .effects()
            .iter()
            .all(|effect| matches!(effect.state(), kitchen::state::EffectState::Applied { .. }))
    );
    Ok(())
}

#[test]
fn a_reread_that_proves_every_write_absent_frees_the_subject() -> TestResult {
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let stuck = settle_with_unknown_create(&desk, &draft, Fault::LoseBeforeApply)?;
    let report = desk.acknowledge(&stuck, "Checked.", false, &interactive("person")?, true)?;
    assert_eq!(report.outcome, AcknowledgeOutcome::NotHeld);
    assert_eq!(
        report.writes,
        vec![WriteReadBack {
            effect: kitchen::EffectName::new("create")?,
            state: ReadBack::Absent,
        }]
    );
    // Nothing was acknowledged, and nothing needs to be: the create never
    // landed, so the revision posts the issue once.
    let store = &desk.world.fixture.store;
    assert_eq!(store.task(&stuck)?.write_acknowledgement(), None);
    let mut revised = new_issue_draft()?;
    revised.add_labels = vec!["ready".to_owned()];
    let after = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(after.outcome, DraftOutcome::Completed);
    // The lost create never made #101, so the revision's create is the first.
    assert_eq!(after.issue, Some(IssueNumber::new(101)?));
    Ok(())
}

#[test]
fn acknowledgements_refuse_bad_reasons_and_tasks_that_hold_nothing() -> TestResult {
    for bad in ["", " padded ", "two\nlines"] {
        assert!(bad.parse::<AcknowledgeReason>().is_err(), "{bad:?}");
    }
    let longest = "r".repeat(MAX_ACKNOWLEDGE_REASON_BYTES);
    assert!(longest.parse::<AcknowledgeReason>().is_ok());
    assert!(format!("{longest}r").parse::<AcknowledgeReason>().is_err());

    let desk = Desk::new(10)?;
    let person = interactive("person")?;
    // An unfinished draft is rerun or handed back, not acknowledged.
    let draft = refine_draft()?;
    desk.forge.plan_faults(vec![None, Some(Fault::Reject)]);
    desk.apply(&draft, Some(&approval(&draft)?))?;
    let unfinished = draft_task_id(&draft_preview(&draft)?)?;
    let report = desk.acknowledge(&unfinished, "Done.", true, &person, true)?;
    assert_eq!(report.outcome, AcknowledgeOutcome::Unsettled);
    // A completed draft holds nothing.
    desk.forge.plan_faults(Vec::new());
    desk.world.clock.advance(1);
    desk.apply(&draft, Some(&approval(&draft)?))?;
    let report = desk.acknowledge(&unfinished, "Done.", true, &person, true)?;
    assert_eq!(report.outcome, AcknowledgeOutcome::NotHeld);
    // Only draft tasks can be acknowledged.
    let pickup = issue_task_id(&issue(7)?)?;
    let refused = desk.acknowledge(&pickup, "Done.", true, &person, true);
    assert!(refused.is_err_and(|error| error.class() == ErrorClass::InvalidInput));
    Ok(())
}

#[test]
fn a_forged_marker_does_not_release_a_settled_draft() -> TestResult {
    use kitchen::{
        WorkflowId,
        state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem},
    };
    let desk = Desk::new(10)?;
    let draft = new_issue_draft()?;
    let stuck = settle_after_create(&desk, &draft)?;
    let store = &desk.world.fixture.store;
    let now = kitchen::contracts::Clock::now(&desk.world.clock);
    // What the public marker API lets any library caller write: an
    // acknowledgement-shaped fact for this task, recorded under an
    // interactive claimant without acknowledge_draft's checks.
    let fact = MarkerFact::workflow(
        MarkerSchema::new(
            "interactive-draft.acknowledgement",
            std::num::NonZeroU32::MIN,
        )?,
        &serde_json::json!({
            "task": stuck.as_str(),
            "reason": "Written straight to the store.",
            "applied": [],
            "absent": [],
            "unknown": ["create"],
        }),
    )?;
    let key = MarkerKey {
        workflow: WorkflowId::new("interactive-draft")?,
        item: WorkItem::Repository {
            repository: repo()?,
        },
        subject: MarkerSubject::Observation(ExternalRef::new(stuck.as_str())?),
    };
    for claimant in [interactive("person")?, scheduled("gardener-tick")?] {
        store.record_marker(key.clone(), fact.clone(), &claimant, now)?;
    }

    let mut revised = new_issue_draft()?;
    revised.add_labels = vec!["ready".to_owned()];
    let blocked = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert!(matches!(
        blocked.outcome,
        DraftOutcome::EarlierSettledWithWrites { .. }
    ));
    assert_eq!(store.task(&stuck)?.write_acknowledgement(), None);

    // The forged markers do not stand in for the real record either.
    let report = desk.acknowledge(
        &stuck,
        "Checked #101.",
        false,
        &interactive("person")?,
        true,
    )?;
    assert!(matches!(report.outcome, AcknowledgeOutcome::Recorded(_)));
    let after = desk.apply(&revised, Some(&approval(&revised)?))?;
    assert_eq!(after.outcome, DraftOutcome::Completed);
    Ok(())
}
