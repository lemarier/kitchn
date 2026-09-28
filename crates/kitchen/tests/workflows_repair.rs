//! Repair eligibility, stack order, slots, budgets, and the push check.
//! Sanitized fixtures, the fake backend, and a fake GitHub transport only.

mod common;
mod workflows_support;

use std::{cell::RefCell, collections::VecDeque, time::Duration};

use common::{TestResult, commit, scheduled, ttl};
use kitchen::{
    CredentialId, HouseId, TaskId,
    contracts::{
        CommitId, ExternalRef, IssueNumber, Permission, PostingBudget, ResourceKind, ResourceRef,
        RetryPolicy, Settlement, Workspace,
    },
    integrations::github::{
        CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError,
        PullRequest, ReadLimits, ReadRequest,
    },
    state::StateError,
    workflows::{
        coordination::{LaunchOutcome, launch_worker},
        pickup::FollowUpBudget,
        repair::{
            HandOver, MAX_CONCURRENT_REPAIRS, Mergeability, Observed, Ownership, PullRequestState,
            PullRequestView, PushIntent, PushObservation, PushRefusal, RepairCandidate,
            RepairDecision, RepairKind, RepairPolicy, Skip, StackLayer, WorktreeView, Writer,
            assess, check_push, observe_pull_request, plan, repair_spec, repair_task_id,
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

fn repair_policy() -> RepairPolicy {
    RepairPolicy {
        max_unknown_rechecks: 2,
        budget: FollowUpBudget {
            fix_rounds: 2,
            review_requests: 1,
        },
    }
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
    let policy = repair_policy();
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
    let policy = repair_policy();
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
    let policy = repair_policy();
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
        plan(&repair_policy(), &[upper, lower], 0),
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

fn open_pr(pr: u64, head: CommitId) -> TestResult<Observed<Option<PullRequestView>>> {
    Ok(Observed::Known(Some(PullRequestView {
        head,
        ..view(pr, PullRequestState::Open, Mergeability::Clean)?
    })))
}

#[test]
fn every_push_is_checked_against_fresh_pr_and_branch_state() -> TestResult {
    let pushed = commit('d')?;
    let intent = PushIntent {
        branch: branch("lemarier/issue-5")?,
        pull_request: Some(number(5)?),
        expected_remote: Some(pushed.clone()),
    };
    let open = PushObservation {
        pull_request: open_pr(5, pushed.clone())?,
        remote_head: Observed::Known(Some(pushed.clone())),
    };
    assert_eq!(
        check_push(&intent, &open),
        Ok(kitchen::workflows::repair::PushPermit {
            replaces: Some(pushed.clone())
        })
    );
    let with_state = |state| -> TestResult<PushObservation> {
        Ok(PushObservation {
            pull_request: Observed::Known(Some(view(5, state, Mergeability::Clean)?)),
            remote_head: Observed::Known(None),
        })
    };
    // The live incident: the PR merged and its branch was deleted.
    assert_eq!(
        check_push(&intent, &with_state(PullRequestState::Merged)?),
        Err(PushRefusal::Merged)
    );
    assert_eq!(
        check_push(&intent, &with_state(PullRequestState::Closed)?),
        Err(PushRefusal::Closed)
    );
    let deleted = PushObservation {
        remote_head: Observed::Known(None),
        ..open.clone()
    };
    assert_eq!(
        check_push(&intent, &deleted),
        Err(PushRefusal::BranchDeleted)
    );
    let moved = PushObservation {
        remote_head: Observed::Known(Some(commit('f')?)),
        ..open.clone()
    };
    assert_eq!(
        check_push(&intent, &moved),
        Err(PushRefusal::RemoteMoved {
            found: commit('f')?
        })
    );
    let unknown_pr = PushObservation {
        pull_request: Observed::Unknown,
        ..open.clone()
    };
    assert_eq!(check_push(&intent, &unknown_pr), Err(PushRefusal::Unknown));
    let unknown_remote = PushObservation {
        remote_head: Observed::Unknown,
        ..open.clone()
    };
    assert_eq!(
        check_push(&intent, &unknown_remote),
        Err(PushRefusal::Unknown)
    );
    let missing = PushObservation {
        pull_request: Observed::Known(None),
        ..open.clone()
    };
    assert_eq!(
        check_push(&intent, &missing),
        Err(PushRefusal::PullRequestMissing)
    );
    let renamed = PushObservation {
        pull_request: Observed::Known(Some(PullRequestView {
            head_branch: "orca/lemarier/issue-5".to_owned(),
            ..view(5, PullRequestState::Open, Mergeability::Clean)?
        })),
        ..open
    };
    assert_eq!(check_push(&intent, &renamed), Err(PushRefusal::WrongBranch));
    Ok(())
}

#[test]
fn a_first_push_must_not_find_an_existing_branch() -> TestResult {
    let intent = PushIntent {
        branch: branch("lemarier/issue-6")?,
        pull_request: None,
        expected_remote: None,
    };
    let absent = PushObservation {
        pull_request: Observed::Unknown,
        remote_head: Observed::Known(None),
    };
    assert_eq!(
        check_push(&intent, &absent),
        Ok(kitchen::workflows::repair::PushPermit { replaces: None })
    );
    let present = PushObservation {
        remote_head: Observed::Known(Some(commit('d')?)),
        ..absent
    };
    assert_eq!(
        check_push(&intent, &present),
        Err(PushRefusal::BranchExists)
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

struct Transport(RefCell<VecDeque<Result<Vec<u8>, IntegrationError>>>);

impl GitHubReadTransport for Transport {
    fn read(
        &self,
        _: &CredentialRef,
        _: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        self.0
            .borrow_mut()
            .pop_front()
            .unwrap_or(Err(IntegrationError::Unavailable))
    }
}

#[test]
fn a_failed_pull_request_read_is_unknown_and_refuses_the_push() -> TestResult {
    let house = HouseId::new("origin89")?;
    let requester = ExternalRef::new("origin89-bot")?;
    let scope = HouseScope::new(
        house.clone(),
        [repo()?],
        requester.clone(),
        CredentialRef::new(house.clone(), CredentialId::new("github-read")?, requester),
        PostingBudget::new(0)?,
        [Permission::PostComment],
    )?;
    let responses = VecDeque::from([
        Ok(serde_json::to_vec(&github_pr(
            true, "closed", None, "unknown",
        ))?),
        Err(IntegrationError::Timeout),
    ]);
    let client = GitHubClient::new(
        scope,
        Transport(RefCell::new(responses)),
        ReadLimits::new(Duration::from_secs(5), 1, 64 * 1024)?,
    );
    let intent = PushIntent {
        branch: branch("lemarier/issue-5")?,
        pull_request: Some(number(5)?),
        expected_remote: Some(commit('d')?),
    };
    let merged = observe_pull_request(&client, &house, &repo()?, number(5)?);
    let head = commit('d')?;
    let observation = |pull_request| PushObservation {
        pull_request,
        remote_head: Observed::Known(Some(head.clone())),
    };
    assert_eq!(
        check_push(
            &intent,
            &observation(match merged {
                Observed::Known(view) => Observed::Known(Some(view)),
                Observed::Unknown => Observed::Unknown,
            })
        ),
        Err(PushRefusal::Merged)
    );
    let failed = observe_pull_request(&client, &house, &repo()?, number(5)?);
    assert_eq!(failed, Observed::Unknown);
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
    );
    let tick = scheduled("repair-tick")?;
    store.create_task(spec.clone(), &tick, world.now())?;
    let lease = store.claim(&id, &tick, ttl(300)?, world.now())?;
    let mut repair_brief = brief(5)?;
    repair_brief.branch = branch("lemarier/issue-5")?;
    let text = repair_brief.render(&spec)?;

    let other = ResourceRef {
        handle: ExternalRef::new("worktree-someone-else")?,
        ..worktree.clone()
    };
    let refused = launch_worker(
        &world.ctx(),
        &id,
        lease.fence(),
        Workspace::Existing(other),
        text.clone(),
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
        text,
    )?;
    assert!(matches!(outcome, LaunchOutcome::Accepted { .. }));
    Ok(())
}
