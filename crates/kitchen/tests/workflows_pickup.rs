//! Pickup eligibility, priority, capacity, and durable claims shared by
//! scheduled and interactive triggers. Fake backend and temporary stores
//! only: simulated evidence.

mod common;
mod workflows_support;

use common::{TestResult, at, interactive, scheduled, ttl};
use kitchen::{
    ErrorClass,
    contracts::{IssueNumber, Repository, Settlement, Text, Trigger},
    state::{OwnershipEvent, StateError},
    workflows::{
        coordination::CoordinationError,
        pickup::{
            Base, Blocker, Blockers, BranchName, ClaimOutcome, Exclusion, IssueRef, LinkedWork,
            MAX_STACK_DEPTH, Overlap, Precheck, Readiness, claim_issue, issue_task_id, select,
        },
    },
};
use workflows_support::{
    World, brief, issue, policy, provenance, ready, repo, template, template_with, under_consumer,
};

#[test]
fn ineligible_issues_are_excluded_with_their_reason() -> TestResult {
    let world = World::new()?;
    let mut candidates = Vec::new();
    let mut expected = Vec::new();
    let mut add = |number: u64,
                   edit: &dyn Fn(&mut kitchen::workflows::pickup::Candidate) -> TestResult,
                   reason: Exclusion|
     -> TestResult {
        let mut candidate = ready(number)?;
        edit(&mut candidate)?;
        candidates.push(candidate);
        expected.push((issue(number)?, reason));
        Ok(())
    };
    add(
        1,
        &|c| {
            c.readiness = Readiness::NotReady;
            Ok(())
        },
        Exclusion::NotReady,
    )?;
    add(
        2,
        &|c| {
            c.readiness = Readiness::NeedsSpec;
            Ok(())
        },
        Exclusion::NeedsSpec,
    )?;
    add(
        3,
        &|c| {
            c.human_only = true;
            Ok(())
        },
        Exclusion::HumanOnly,
    )?;
    add(
        4,
        &|c| {
            c.assigned = true;
            Ok(())
        },
        Exclusion::Assigned,
    )?;
    add(
        5,
        &|c| {
            c.blockers = Blockers::Known(vec![
                Blocker {
                    issue: issue(90)?,
                    open: false,
                },
                Blocker {
                    issue: issue(91)?,
                    open: true,
                },
            ]);
            Ok(())
        },
        Exclusion::BlockedBy(vec![issue(91)?]),
    )?;
    add(
        6,
        &|c| {
            c.blockers = Blockers::Unknown;
            Ok(())
        },
        Exclusion::BlockersUnknown,
    )?;
    add(
        7,
        &|c| {
            c.prose_dependencies = vec![issue(92)?];
            Ok(())
        },
        Exclusion::ProseDependency(vec![issue(92)?]),
    )?;
    add(
        8,
        &|c| {
            c.linked = LinkedWork::Worktree;
            Ok(())
        },
        Exclusion::ExistingWorktree,
    )?;
    add(
        9,
        &|c| {
            c.linked = LinkedWork::PullRequest(IssueNumber::new(40)?);
            Ok(())
        },
        Exclusion::ExistingPullRequest(IssueNumber::new(40)?),
    )?;
    add(
        10,
        &|c| {
            c.linked = LinkedWork::Unknown;
            Ok(())
        },
        Exclusion::LinkedWorkUnknown,
    )?;
    add(
        11,
        &|c| {
            c.overlap = Overlap::InFlight(issue(93)?);
            Ok(())
        },
        Exclusion::OverlapsInFlight(issue(93)?),
    )?;
    add(
        12,
        &|c| {
            c.overlap = Overlap::Unknown;
            Ok(())
        },
        Exclusion::OverlapUnknown,
    )?;
    add(
        13,
        &|c| {
            c.issue.repository = Repository::new("origin89hq/other")?;
            Ok(())
        },
        Exclusion::OutsideScope,
    )?;
    let outside = IssueRef {
        repository: Repository::new("origin89hq/other")?,
        number: IssueNumber::new(13)?,
    };
    if let Some(last) = expected.last_mut() {
        last.0 = outside;
    }
    let selection = select(
        &policy(3)?,
        &candidates,
        &world.fixture.store.tasks()?,
        world.now(),
    )?;
    assert!(selection.picks.is_empty());
    assert_eq!(selection.precheck, Precheck::Idle);
    let mut excluded = selection.excluded;
    let key = |entry: &(IssueRef, Exclusion)| entry.0.number.get();
    excluded.sort_by_key(key);
    expected.sort_by_key(key);
    assert_eq!(excluded, expected);
    // Selection is a pure decision: an idle tick writes nothing.
    assert!(world.fixture.store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn prerequisites_come_first_then_milestone_then_age() -> TestResult {
    let world = World::new()?;
    let old = ready(1)?;
    let mut prerequisite = ready(2)?;
    prerequisite.blocks_open = 2;
    let mut milestone = ready(3)?;
    milestone.milestone_due = Some(at(10));
    let newest = ready(4)?;
    let selection = select(
        &policy(3)?,
        &[newest, old, milestone, prerequisite],
        &world.fixture.store.tasks()?,
        world.now(),
    )?;
    let order: Vec<u64> = selection
        .picks
        .iter()
        .map(|pick| pick.issue.number.get())
        .collect();
    assert_eq!(order, vec![2, 3, 1]);
    assert_eq!(
        selection.excluded,
        vec![(issue(4)?, Exclusion::CapacityFull)]
    );
    assert_eq!(selection.precheck, Precheck::Actionable);
    Ok(())
}

#[test]
fn capacity_counts_durable_claims_including_waiting_workers() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    for number in 1..=3 {
        let outcome = claim_issue(
            store,
            &template()?,
            &issue(number)?,
            &claimant,
            ttl(600)?,
            world.now(),
        )?;
        assert!(matches!(outcome, ClaimOutcome::Claimed(_)));
    }
    // One of them waits for a reply: its claim still occupies a slot.
    let selection = select(&policy(3)?, &[ready(4)?], &store.tasks()?, world.now())?;
    assert_eq!(selection.active, 3);
    assert!(selection.picks.is_empty());
    assert_eq!(
        selection.excluded,
        vec![(issue(4)?, Exclusion::CapacityFull)]
    );
    assert_eq!(selection.precheck, Precheck::Idle);
    // Already-claimed issues are refused whatever the capacity.
    let selection = select(&policy(10)?, &[ready(1)?], &store.tasks()?, world.now())?;
    assert_eq!(
        selection.excluded,
        vec![(
            issue(1)?,
            Exclusion::Claimed {
                trigger: Trigger::Scheduled
            }
        )]
    );
    Ok(())
}

#[test]
fn focused_interactive_work_does_not_use_a_scheduled_slot() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let person = interactive("david")?;
    claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &person,
        ttl(600)?,
        world.now(),
    )?;
    let selection = select(
        &policy(1)?,
        &[ready(1)?, ready(2)?],
        &store.tasks()?,
        world.now(),
    )?;
    assert_eq!(selection.active, 0);
    assert_eq!(selection.picks.len(), 1);
    assert_eq!(
        selection.picks.first().map(|pick| pick.issue.number.get()),
        Some(2)
    );
    assert_eq!(
        selection.excluded,
        vec![(
            issue(1)?,
            Exclusion::Claimed {
                trigger: Trigger::Interactive
            }
        )]
    );
    Ok(())
}

#[test]
fn scheduled_and_interactive_work_share_one_durable_claim() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let (tick, _) = under_consumer(&world, "coordinator")?;
    let person = interactive("david")?;
    let target = issue(7)?;

    // The person takes the issue first; the scheduled tick is refused.
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &template()?,
        &target,
        &person,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("interactive claim failed".into());
    };
    assert_eq!(
        claim_issue(store, &template()?, &target, &tick, ttl(600)?, world.now())?,
        ClaimOutcome::Held {
            trigger: Trigger::Interactive
        }
    );

    // Ownership moves only through a recorded relinquish and adoption.
    store.relinquish(&issue_task_id(&target)?, lease.fence(), world.now())?;
    let adopted = claim_issue(store, &template()?, &target, &tick, ttl(600)?, world.now())?;
    assert!(matches!(adopted, ClaimOutcome::Adopted(_)));
    let record = store.task(&issue_task_id(&target)?)?;
    assert!(matches!(
        record.ownership(),
        [
            OwnershipEvent::Claimed {
                trigger: Trigger::Interactive,
                ..
            },
            OwnershipEvent::Relinquished { .. },
            OwnershipEvent::Adopted {
                trigger: Trigger::Scheduled,
                ..
            },
        ]
    ));
    // Now the person is refused in turn.
    assert_eq!(
        claim_issue(
            store,
            &template()?,
            &target,
            &person,
            ttl(600)?,
            world.now()
        )?,
        ClaimOutcome::Held {
            trigger: Trigger::Scheduled
        }
    );
    Ok(())
}

#[test]
fn a_claim_under_a_superseded_consumer_is_refused() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let (first, lease) = under_consumer(&world, "coordinator-a")?;
    store.release_consumer(&workflows_support::consumer()?, lease.fence(), world.now())?;
    let (_second, _) = under_consumer(&world, "coordinator-b")?;
    let error = claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &first,
        ttl(600)?,
        world.now(),
    )
    .err()
    .ok_or("stale consumer claimed an issue")?;
    assert!(matches!(
        error,
        kitchen::Error::State(StateError::StaleFence { .. })
    ));
    assert!(store.tasks()?.is_empty());
    Ok(())
}

#[test]
fn expired_and_settled_claims_are_not_picked_again() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let tick = scheduled("tick")?;
    claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &tick,
        ttl(60)?,
        world.now(),
    )?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &template()?,
        &issue(2)?,
        &tick,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let task = issue_task_id(&issue(2)?)?;
    store.request_cancel(&task, &common::holder("david")?, world.now())?;
    store.settle_cancelled(&task, lease.fence(), world.now())?;
    world.clock.advance(120);

    let selection = select(
        &policy(5)?,
        &[ready(1)?, ready(2)?],
        &store.tasks()?,
        world.now(),
    )?;
    assert!(selection.picks.is_empty());
    assert!(
        selection
            .excluded
            .contains(&(issue(1)?, Exclusion::OwnerUncertain))
    );
    assert!(
        selection
            .excluded
            .contains(&(issue(2)?, Exclusion::Settled(Settlement::Cancelled)))
    );
    // The expired claim still occupies a slot: its worker may be running.
    assert_eq!(selection.active, 1);
    let other = scheduled("other-tick")?;
    assert_eq!(
        claim_issue(
            store,
            &template()?,
            &issue(1)?,
            &other,
            ttl(60)?,
            world.now()
        )?,
        ClaimOutcome::OwnerUncertain
    );
    assert_eq!(
        claim_issue(
            store,
            &template()?,
            &issue(2)?,
            &other,
            ttl(60)?,
            world.now()
        )?,
        ClaimOutcome::Settled(Settlement::Cancelled)
    );
    Ok(())
}

#[test]
fn an_existing_task_keeps_its_pinned_instructions() -> TestResult {
    let world = World::new()?;
    let store = &world.fixture.store;
    let tick = scheduled("tick")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &template()?,
        &issue(1)?,
        &tick,
        ttl(600)?,
        world.now(),
    )?
    else {
        return Err("claim failed".into());
    };
    let task = issue_task_id(&issue(1)?)?;
    store.relinquish(&task, lease.fence(), world.now())?;
    let updated = template_with(3, provenance('e')?)?;
    let outcome = claim_issue(store, &updated, &issue(1)?, &tick, ttl(600)?, world.now())?;
    assert!(matches!(outcome, ClaimOutcome::Adopted(_)));
    assert_eq!(store.task(&task)?.spec().provenance, provenance('a')?);
    Ok(())
}

#[test]
fn overlapping_settled_work_stacks_up_to_the_depth_limit() -> TestResult {
    let world = World::new()?;
    let lower = BranchName::new("lemarier/driver")?;
    let mut shallow = ready(1)?;
    shallow.overlap = Overlap::SettledPullRequest {
        pull_request: IssueNumber::new(30)?,
        branch: lower.clone(),
        depth: MAX_STACK_DEPTH - 1,
    };
    let mut deep = ready(2)?;
    deep.overlap = Overlap::SettledPullRequest {
        pull_request: IssueNumber::new(31)?,
        branch: lower.clone(),
        depth: MAX_STACK_DEPTH,
    };
    let selection = select(
        &policy(5)?,
        &[shallow, deep],
        &world.fixture.store.tasks()?,
        world.now(),
    )?;
    assert_eq!(
        selection.picks.first().map(|pick| pick.base.clone()),
        Some(Base::Stack {
            pull_request: IssueNumber::new(30)?,
            branch: lower,
            depth: MAX_STACK_DEPTH,
        })
    );
    assert_eq!(
        selection.excluded,
        vec![(issue(2)?, Exclusion::StackTooDeep)]
    );
    Ok(())
}

#[test]
fn task_ids_are_stable_and_distinguish_similar_repositories() -> TestResult {
    let kitchen = IssueRef {
        repository: Repository::new("lemarier/kitchen")?,
        number: IssueNumber::new(8)?,
    };
    // Pinned: scheduled and interactive processes of any release must agree.
    assert_eq!(
        issue_task_id(&kitchen)?.as_str(),
        "issue-1252879d18666e31-8"
    );
    let dotted = IssueRef {
        repository: Repository::new("a/b.c")?,
        number: IssueNumber::new(1)?,
    };
    let dashed = IssueRef {
        repository: Repository::new("a/b-c")?,
        number: IssueNumber::new(1)?,
    };
    assert_ne!(issue_task_id(&dotted)?, issue_task_id(&dashed)?);
    let longest = IssueRef {
        repository: repo()?,
        number: IssueNumber::new(u64::try_from(i64::MAX)?)?,
    };
    assert!(issue_task_id(&longest)?.as_str().len() <= 64);
    Ok(())
}

#[test]
fn branch_names_are_exact_and_validated() -> TestResult {
    let name = BranchName::new("lemarier/pickup-coordination")?;
    assert_eq!(name.as_str(), "lemarier/pickup-coordination");
    assert_eq!(name.verify_observed("lemarier/pickup-coordination"), Ok(()));
    for observed in [
        "orca/lemarier/pickup-coordination",
        "lemarier/pickup-coordination-2",
        "Lemarier/pickup-coordination",
    ] {
        assert_eq!(
            name.verify_observed(observed),
            Err(CoordinationError::BranchMismatch)
        );
    }
    for invalid in [
        "",
        "@",
        "-x",
        "/x",
        "x/",
        "x.",
        "a..b",
        "a//b",
        "a/.hidden",
        "a.lock",
        "a b",
        "a~b",
        "a^b",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
        "a\\b",
        "a@{b",
        "é",
    ] {
        assert_eq!(
            BranchName::new(invalid),
            Err(CoordinationError::InvalidBranchName)
        );
    }
    assert!(BranchName::new(&"a".repeat(BranchName::MAX_BYTES)).is_ok());
    assert!(BranchName::new(&"a".repeat(BranchName::MAX_BYTES + 1)).is_err());
    Ok(())
}

#[test]
fn a_brief_is_standalone_and_bound_to_its_task() -> TestResult {
    let spec = template()?.spec_for(&issue(5)?)?;
    let text = brief(5)?.render(&spec)?;
    let text = text.as_str();
    assert!(text.contains("create exactly `lemarier/issue-5`"));
    assert!(text.contains(&provenance('a')?.kitchen.to_string()));
    assert!(text.contains(&common::commit('c')?.to_string()));
    assert!(text.contains("- The firmware builds with the new driver."));
    assert!(text.contains("launch-worker, message-worker, cancel-worker, release-resource, ask-human. Nothing else is granted."));
    assert!(text.contains("3 attempt(s), 2 review-fix round(s), 1 review request(s)"));
    assert!(text.contains("reports/issue.md"));

    let mut stale = brief(5)?;
    stale.instructions.provenance = provenance('e')?;
    let mut empty = brief(5)?;
    empty.acceptance.clear();
    let mut foreign = brief(5)?;
    foreign.instructions.house = common::other_house()?;
    let mut elsewhere = brief(5)?;
    elsewhere.issue.repository = Repository::new("origin89hq/km43")?;
    for invalid in [stale, empty, foreign, elsewhere] {
        let error = invalid
            .render(&spec)
            .err()
            .ok_or("mismatched brief rendered")?;
        assert_eq!(error.class(), ErrorClass::InvalidInput);
        assert!(matches!(
            error,
            kitchen::Error::Coordination(CoordinationError::BriefMismatch)
        ));
    }
    let mut huge = brief(5)?;
    huge.acceptance = vec![Text::new(&"x".repeat(60 * 1024))?; 2];
    assert!(huge.render(&spec).is_err());
    Ok(())
}
