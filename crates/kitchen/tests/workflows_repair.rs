//! Repair eligibility, stack order, slots, budgets, and the GitHub read
//! mapping. Sanitized fixtures and the fake backend only; pushes are tested
//! in `workflows_push`.

mod common;
mod workflows_support;

use std::time::Duration;

use common::{TestResult, commit, house_with_fix_rounds, scheduled, ttl};
use kitchen::{
    TaskId,
    contracts::{
        ExternalRef, IssueNumber, ResourceKind, ResourceRef, RetryPolicy, Settlement, Workspace,
    },
    integrations::github::PullRequest,
    state::StateError,
    workflows::{
        coordination::{LaunchOutcome, launch_worker},
        repair::{
            HandOver, MAX_CONCURRENT_REPAIRS, Mergeability, Observed, Ownership, PullRequestState,
            PullRequestView, RepairCandidate, RepairDecision, RepairKind, RepairPolicy, Skip,
            StackLayer, WorktreeView, Writer, assess, plan, repair_spec, repair_task_id,
        },
    },
};
use serde_json::json;
use workflows_support::{World, branch, brief, provenance, repo, template};

fn number(value: u64) -> TestResult<IssueNumber> {
    Ok(IssueNumber::new(value)?)
}

fn task(value: &str) -> TestResult<TaskId> {
    Ok(TaskId::new(value)?)
}

type Edit = Box<dyn Fn(&mut RepairCandidate) -> TestResult>;

fn repair_policy() -> TestResult<RepairPolicy> {
    Ok(RepairPolicy::for_house(&house_with_fix_rounds(None)?, 2))
}

fn view(
    pr: u64,
    state: PullRequestState,
    mergeability: Mergeability,
) -> TestResult<PullRequestView> {
    Ok(PullRequestView {
        number: number(pr)?,
        state,
        head: commit('d')?,
        head_branch: format!("lemarier/issue-{pr}"),
        base_branch: "main".to_owned(),
        mergeability,
    })
}

/// A settled, owned, conflicting pull request with a clean worktree.
fn conflicting(pr: u64) -> TestResult<RepairCandidate> {
    Ok(RepairCandidate {
        repository: repo()?,
        pull_request: view(pr, PullRequestState::Open, Mergeability::Conflicting)?,
        branch: branch(&format!("lemarier/issue-{pr}"))?,
        ownership: Ownership::Settled {
            task: task(&format!("issue-owner-{pr}"))?,
            settlement: Settlement::Succeeded,
        },
        writer: Writer::None,
        worktree: WorktreeView {
            dirty: Observed::Known(false),
            unpushed: Observed::Known(false),
        },
        stack: None,
        unknown_rechecks: 0,
        rounds_used: 0,
    })
}

#[test]
fn repair_only_touches_settled_owned_branches_with_preserved_work() -> TestResult {
    let policy = repair_policy()?;
    let decide = |edit: &dyn Fn(&mut RepairCandidate) -> TestResult| -> TestResult<RepairDecision> {
        let mut candidate = conflicting(1)?;
        edit(&mut candidate)?;
        Ok(assess(&policy, &candidate))
    };
    let cases: Vec<(RepairDecision, Edit)> = vec![
        (
            RepairDecision::Repair(RepairKind::Conflict),
            Box::new(|_| Ok(())),
        ),
        (
            RepairDecision::Skip(Skip::Finished),
            Box::new(|c| {
                c.pull_request.state = PullRequestState::Merged;
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::Finished),
            Box::new(|c| {
                c.pull_request.state = PullRequestState::Closed;
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::NotOwned),
            Box::new(|c| {
                c.ownership = Ownership::Foreign;
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::Unsuccessful),
            Box::new(|c| {
                c.ownership = Ownership::Settled {
                    task: task("issue-owner-1")?,
                    settlement: Settlement::Failed,
                };
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::WriterActive),
            Box::new(|c| {
                c.ownership = Ownership::Unsettled(task("issue-owner-1")?);
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::WriterActive),
            Box::new(|c| {
                c.writer = Writer::Task(task("repair-1")?);
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::WriterActive),
            Box::new(|c| {
                c.writer = Writer::Unknown;
                Ok(())
            }),
        ),
        (
            RepairDecision::HandOver(HandOver::PersonOwnsTerminal),
            Box::new(|c| {
                c.writer = Writer::Person;
                Ok(())
            }),
        ),
        (
            RepairDecision::HandOver(HandOver::PreserveWork),
            Box::new(|c| {
                c.worktree.dirty = Observed::Known(true);
                Ok(())
            }),
        ),
        (
            RepairDecision::HandOver(HandOver::PreserveWork),
            Box::new(|c| {
                c.worktree.unpushed = Observed::Known(true);
                Ok(())
            }),
        ),
        (
            RepairDecision::HandOver(HandOver::WorktreeUnknown),
            Box::new(|c| {
                c.worktree.dirty = Observed::Unknown;
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::Healthy),
            Box::new(|c| {
                c.pull_request.mergeability = Mergeability::Clean;
                Ok(())
            }),
        ),
        (
            RepairDecision::Skip(Skip::Healthy),
            Box::new(|c| {
                c.pull_request.mergeability = Mergeability::Behind;
                Ok(())
            }),
        ),
        (
            RepairDecision::Recheck,
            Box::new(|c| {
                c.pull_request.mergeability = Mergeability::Unknown;
                c.unknown_rechecks = 1;
                Ok(())
            }),
        ),
        (
            RepairDecision::HandOver(HandOver::MergeabilityUnknown),
            Box::new(|c| {
                c.pull_request.mergeability = Mergeability::Unknown;
                c.unknown_rechecks = 2;
                Ok(())
            }),
        ),
        (
            RepairDecision::Repair(RepairKind::Conflict),
            Box::new(|c| {
                c.rounds_used = 1;
                Ok(())
            }),
        ),
        (
            RepairDecision::HandOver(HandOver::BudgetExhausted),
            Box::new(|c| {
                c.rounds_used = 2;
                Ok(())
            }),
        ),
    ];
    for (index, (expected, edit)) in cases.iter().enumerate() {
        assert_eq!(decide(edit.as_ref())?, *expected, "case {index}");
    }
    Ok(())
}

fn layer(stack: &str, depth: u8, contains: Observed<bool>) -> TestResult<StackLayer> {
    Ok(StackLayer {
        stack: task(stack)?,
        depth,
        lower_merged: true,
        contains_lower_merge: contains,
    })
}

#[test]
fn a_layer_is_restacked_only_after_its_lower_layer_merged() -> TestResult {
    let policy = repair_policy()?;
    let mut candidate = conflicting(2)?;
    candidate.pull_request.mergeability = Mergeability::Clean;
    candidate.stack = Some(layer("stack-a", 2, Observed::Known(false))?);
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::Repair(RepairKind::Restack)
    );
    candidate.stack = Some(layer("stack-a", 2, Observed::Known(true))?);
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::Skip(Skip::Healthy)
    );
    candidate.stack = Some(layer("stack-a", 2, Observed::Unknown)?);
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::HandOver(HandOver::StackUnknown)
    );
    // An unmerged lower layer leaves ordinary conflict handling in place.
    candidate.pull_request.mergeability = Mergeability::Conflicting;
    candidate.stack = Some(StackLayer {
        lower_merged: false,
        ..layer("stack-a", 2, Observed::Unknown)?
    });
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::Repair(RepairKind::Conflict)
    );
    Ok(())
}

#[test]
fn repairs_use_their_own_bounded_slots_and_one_writer_per_stack() -> TestResult {
    let policy = repair_policy()?;
    // Issue capacity is irrelevant here: repair slots are separate.
    let candidates = [conflicting(3)?, conflicting(1)?, conflicting(2)?];
    let decisions = plan(&policy, &candidates, 0);
    assert_eq!(
        decisions,
        vec![
            (number(1)?, RepairDecision::Repair(RepairKind::Conflict)),
            (number(2)?, RepairDecision::Repair(RepairKind::Conflict)),
            (number(3)?, RepairDecision::Skip(Skip::NoSlot)),
        ]
    );
    let full = plan(&policy, &candidates, MAX_CONCURRENT_REPAIRS);
    assert!(
        full.iter()
            .all(|(_, decision)| *decision == RepairDecision::Skip(Skip::NoSlot))
    );

    let mut lower = conflicting(10)?;
    lower.stack = Some(StackLayer {
        lower_merged: false,
        ..layer("stack-b", 1, Observed::Known(true))?
    });
    let mut upper = conflicting(11)?;
    upper.pull_request.mergeability = Mergeability::Clean;
    upper.stack = Some(layer("stack-b", 2, Observed::Known(false))?);
    let mut other = conflicting(12)?;
    other.stack = Some(layer("stack-c", 2, Observed::Known(false))?);
    assert_eq!(
        plan(&policy, &[upper, other, lower], 0),
        vec![
            (number(10)?, RepairDecision::Repair(RepairKind::Conflict)),
            (number(11)?, RepairDecision::Skip(Skip::LowerLayerFirst)),
            (number(12)?, RepairDecision::Repair(RepairKind::Restack)),
        ]
    );
    Ok(())
}

#[test]
fn a_lower_layer_waiting_for_a_person_blocks_the_layers_above() -> TestResult {
    let mut lower = conflicting(20)?;
    lower.writer = Writer::Person;
    lower.stack = Some(StackLayer {
        lower_merged: false,
        ..layer("stack-d", 1, Observed::Known(true))?
    });
    let mut upper = conflicting(21)?;
    upper.stack = Some(StackLayer {
        lower_merged: false,
        ..layer("stack-d", 2, Observed::Known(true))?
    });
    assert_eq!(
        plan(&repair_policy()?, &[upper, lower], 0),
        vec![
            (
                number(20)?,
                RepairDecision::HandOver(HandOver::PersonOwnsTerminal)
            ),
            (number(21)?, RepairDecision::Skip(Skip::LowerLayerFirst)),
        ]
    );
    Ok(())
}

fn github_pr(
    merged: bool,
    state: &str,
    mergeable: Option<bool>,
    detail: &str,
) -> serde_json::Value {
    json!({
        "number": 5,
        "state": state,
        "draft": false,
        "merged": merged,
        "head": {"sha": "d".repeat(40), "ref": "lemarier/issue-5"},
        "base": {"sha": "e".repeat(40), "ref": "main"},
        "mergeable": mergeable,
        "mergeable_state": detail,
    })
}

#[test]
fn github_pull_requests_map_to_the_repair_view_without_optimism() -> TestResult {
    let cases = [
        (
            github_pr(false, "open", Some(true), "clean"),
            PullRequestState::Open,
            Mergeability::Clean,
        ),
        (
            github_pr(false, "open", Some(false), "dirty"),
            PullRequestState::Open,
            Mergeability::Conflicting,
        ),
        (
            github_pr(false, "open", Some(true), "behind"),
            PullRequestState::Open,
            Mergeability::Behind,
        ),
        (
            github_pr(false, "open", None, "unknown"),
            PullRequestState::Open,
            Mergeability::Unknown,
        ),
        (
            github_pr(true, "closed", None, "unknown"),
            PullRequestState::Merged,
            Mergeability::Unknown,
        ),
        (
            github_pr(false, "closed", Some(true), "clean"),
            PullRequestState::Closed,
            Mergeability::Clean,
        ),
    ];
    for (json, state, mergeability) in cases {
        let pull_request: PullRequest = serde_json::from_value(json)?;
        let view = PullRequestView::from_github(&pull_request);
        assert_eq!((view.state, view.mergeability), (state, mergeability));
        assert_eq!(view.head_branch, "lemarier/issue-5");
        assert_eq!(view.head, commit('d')?);
    }
    Ok(())
}

#[test]
fn a_repair_writer_works_only_in_the_worktree_it_was_given() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let worktree = ResourceRef {
        kind: ResourceKind::Worktree,
        backend: common::backend_id()?,
        handle: ExternalRef::new("worktree-issue-5")?,
    };
    let id = repair_task_id(&repo()?, number(5)?, 1)?;
    assert_ne!(id, repair_task_id(&repo()?, number(5)?, 2)?);
    let base = template()?;
    let spec = repair_spec(
        id.clone(),
        repo()?,
        worktree.clone(),
        base.authority,
        RetryPolicy::new(1, Duration::from_secs(3600))?,
        provenance('a')?,
        None,
    );
    assert_eq!(spec.agent, None);
    let tick = scheduled("repair-tick")?;
    store.create_task(spec.clone(), &tick, world.now())?;
    let lease = store.claim(&id, &tick, ttl(300)?, world.now())?;
    let mut repair_brief = brief(5)?;
    repair_brief.branch = branch("lemarier/issue-5")?;

    let other = ResourceRef {
        handle: ExternalRef::new("worktree-someone-else")?,
        ..worktree.clone()
    };
    let refused = launch_worker(
        &world.ctx(),
        &id,
        lease.fence(),
        Workspace::Existing(other),
        &repair_brief,
    )
    .err()
    .ok_or("repair launched into a foreign worktree")?;
    assert!(matches!(
        refused,
        kitchen::Error::State(StateError::ResourceNotOwned)
    ));
    assert_eq!(world.backend.execute_calls(), 0);

    let outcome = launch_worker(
        &world.ctx(),
        &id,
        lease.fence(),
        Workspace::Existing(worktree),
        &repair_brief,
    )?;
    assert!(matches!(outcome, LaunchOutcome::Accepted { .. }));
    Ok(())
}

#[test]
fn the_branch_writer_comes_from_the_backend_observation() -> TestResult {
    use kitchen::contracts::{BackendUnavailable, WorkerOutcome, WorkerState};
    let owner = task("issue-7")?;
    // A takeover is the person's, and repair hands the branch over.
    let writer = Writer::observed(&owner, Ok(WorkerState::UserTakeover));
    assert_eq!(writer, Writer::Person);
    let mut candidate = conflicting(7)?;
    candidate.writer = writer;
    assert_eq!(
        assess(&repair_policy()?, &candidate),
        RepairDecision::HandOver(HandOver::PersonOwnsTerminal)
    );
    for running in [
        WorkerState::Starting,
        WorkerState::Ready,
        WorkerState::AwaitingReply,
    ] {
        assert_eq!(
            Writer::observed(&owner, Ok(running)),
            Writer::Task(owner.clone())
        );
    }
    assert_eq!(
        Writer::observed(&owner, Ok(WorkerState::Settled(WorkerOutcome::Succeeded))),
        Writer::None
    );
    // Absence of evidence is never an absent writer.
    for unknown in [
        Ok(WorkerState::Missing),
        Ok(WorkerState::Unknown),
        Err(BackendUnavailable::Timeout),
    ] {
        assert_eq!(Writer::observed(&owner, unknown), Writer::Unknown);
    }
    Ok(())
}

#[test]
fn a_restacked_layer_still_gets_its_conflicts_repaired() -> TestResult {
    let policy = repair_policy()?;
    // The layer already contains its merged lower layer, but now conflicts
    // with its new base.
    let mut candidate = conflicting(2)?;
    candidate.stack = Some(layer("stack-a", 2, Observed::Known(true))?);
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::Repair(RepairKind::Conflict)
    );
    // Unknown mergeability is rechecked, then handed over, as for any
    // other pull request.
    candidate.pull_request.mergeability = Mergeability::Unknown;
    assert_eq!(assess(&policy, &candidate), RepairDecision::Recheck);
    candidate.unknown_rechecks = policy.max_unknown_rechecks;
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::HandOver(HandOver::MergeabilityUnknown)
    );
    // A restack still wins over a conflict: the restack resolves the base.
    candidate.pull_request.mergeability = Mergeability::Conflicting;
    candidate.stack = Some(layer("stack-a", 2, Observed::Known(false))?);
    assert_eq!(
        assess(&policy, &candidate),
        RepairDecision::Repair(RepairKind::Restack)
    );
    Ok(())
}

#[test]
fn a_repair_task_carries_the_selection_the_house_policy_resolves() -> TestResult {
    let policy = workflows_support::agent_policy()?;
    let worktree = ResourceRef {
        kind: ResourceKind::Worktree,
        backend: common::backend_id()?,
        handle: ExternalRef::new("worktree-issue-5")?,
    };
    let spec = repair_spec(
        repair_task_id(&repo()?, number(5)?, 1)?,
        repo()?,
        worktree,
        template()?.authority,
        RetryPolicy::new(1, Duration::from_secs(3600))?,
        provenance('a')?,
        Some(&policy),
    );
    assert_eq!(
        spec.agent,
        Some(policy.resolve(&kitchen::selection::SelectionRequest {
            repository: Some(repo()?),
            ..kitchen::selection::SelectionRequest::new(kitchen::contracts::Role::StationCook)
        }))
    );
    assert_eq!(
        spec.agent.map(|resolved| resolved.source),
        Some(kitchen::selection::SelectionSource::Repository {
            repository: repo()?
        })
    );
    Ok(())
}

#[test]
fn repair_takes_its_round_budget_from_the_house() -> TestResult {
    let base: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    let house = |follow_up: Option<serde_json::Value>| -> TestResult<kitchen::house::HouseConfig> {
        let mut json = base.clone();
        if let Some(follow_up) = follow_up {
            json["followUp"] = follow_up;
        }
        Ok(serde_json::from_value(json)?)
    };
    let after_two_rounds = |policy: &RepairPolicy| -> TestResult<RepairDecision> {
        let mut candidate = conflicting(1)?;
        candidate.rounds_used = 2;
        Ok(assess(policy, &candidate))
    };
    // Without a policy the library default of two rounds is spent.
    let default = RepairPolicy::for_house(&house(None)?, 2);
    assert_eq!(default.budget().fix_rounds(), 2);
    assert_eq!(default.max_unknown_rechecks, 2);
    assert_eq!(
        after_two_rounds(&default)?,
        RepairDecision::HandOver(HandOver::BudgetExhausted)
    );
    // A house that allows three keeps repairing after two.
    let generous = RepairPolicy::for_house(&house(Some(json!({ "fixRounds": 3 })))?, 2);
    assert_eq!(
        after_two_rounds(&generous)?,
        RepairDecision::Repair(RepairKind::Conflict)
    );
    // A house that allows none hands over before the first round.
    let none = RepairPolicy::for_house(&house(Some(json!({ "fixRounds": 0 })))?, 2);
    let mut fresh = conflicting(1)?;
    fresh.rounds_used = 0;
    assert_eq!(
        assess(&none, &fresh),
        RepairDecision::HandOver(HandOver::BudgetExhausted)
    );
    Ok(())
}
