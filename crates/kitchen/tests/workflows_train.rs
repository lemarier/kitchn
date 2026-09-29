//! Merge trains: overlapping ready pull requests are planned into one
//! stack, judged layer by layer at their exact stacked heads, and merged
//! together under per-layer merge grants. Commands run against a recording
//! stack runner and a stand-in `gh`; no forge or `gh stack` is contacted.

mod common;
mod workflows_support;

use std::{cell::RefCell, collections::BTreeSet, path::PathBuf, time::Duration};

use common::{TestResult, commit, house};
use kitchen::{
    BackendId, CredentialId, HouseId,
    contracts::{
        BranchName, EvidenceSubject, EvidenceVerdict, ExternalRef, Grant, IssueNumber, Permission,
        Repository,
    },
    house::{HouseConfig, MergeSubject},
    workflows::{
        gate::MergeGrant,
        ready::{HeadEvidence, MergeReadiness, NotReady},
        stack::{GhStack, MAX_STACK_LAYERS, StackCommand, StackLink, StackResult, StackRunner},
        train::{
            Deferral, LayerState, Touch, TrainCandidate, TrainDecision, TrainError, TrainLayer,
            TrainRefusal, evaluate_train, merge_train, plan_train, reorder_commands,
        },
    },
};

fn repo() -> TestResult<Repository> {
    Ok(Repository::new("lemarier/kitchen")?)
}

fn branch(name: &str) -> TestResult<BranchName> {
    Ok(BranchName::new(name)?)
}

fn trunk() -> TestResult<BranchName> {
    branch("main")
}

fn number(value: u64) -> TestResult<IssueNumber> {
    Ok(IssueNumber::new(value)?)
}

fn layer_branch(number: u64) -> TestResult<BranchName> {
    branch(&format!("lemarier/pr-{number}"))
}

fn subject(head: char, base: char) -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit(head)?,
        base: Some(commit(base)?),
    })
}

fn evidence(verdict: EvidenceVerdict, about: EvidenceSubject) -> TestResult<HeadEvidence> {
    Ok(HeadEvidence {
        verdict,
        subject: about,
        source: ExternalRef::new("https://github.com/lemarier/kitchen/actions/runs/1")?,
    })
}

/// Readiness of pull request `number` at `head` on `base`, with its review
/// and checks about `evidence_head` on `evidence_base`.
fn readiness(
    number_: u64,
    (head, base): (char, char),
    (evidence_head, evidence_base): (char, char),
    review: EvidenceVerdict,
    checks: EvidenceVerdict,
) -> TestResult<MergeReadiness> {
    let about = subject(evidence_head, evidence_base)?;
    Ok(MergeReadiness {
        repository: repo()?,
        pull_request: number(number_)?,
        subject: subject(head, base)?,
        review: evidence(review, about.clone())?,
        checks: evidence(checks, about)?,
    })
}

fn files(paths: &[&str]) -> BTreeSet<Touch> {
    paths
        .iter()
        .map(|path| Touch::File(PathBuf::from(path)))
        .collect()
}

/// A candidate whose checks have `checks` at its own head on trunk tip `0`.
fn candidate(
    number_: u64,
    touches: BTreeSet<Touch>,
    checks: EvidenceVerdict,
) -> TestResult<TrainCandidate> {
    let head =
        char::from_digit(u32::try_from(number_ % 9)?.saturating_add(1), 10).ok_or("no digit")?;
    Ok(TrainCandidate {
        readiness: readiness(
            number_,
            (head, '0'),
            (head, '0'),
            EvidenceVerdict::Pass,
            checks,
        )?,
        branch: layer_branch(number_)?,
        touches,
    })
}

fn ready_candidate(number_: u64, touches: BTreeSet<Touch>) -> TestResult<TrainCandidate> {
    candidate(number_, touches, EvidenceVerdict::Pass)
}

/// A stacked layer at `head` on `base` with review and checks about the
/// same subject.
fn layer(number_: u64, head: char, base: char, checks: EvidenceVerdict) -> TestResult<TrainLayer> {
    Ok(TrainLayer {
        readiness: readiness(
            number_,
            (head, base),
            (head, base),
            EvidenceVerdict::Pass,
            checks,
        )?,
        branch: layer_branch(number_)?,
    })
}

fn green(number_: u64, head: char, base: char) -> TestResult<TrainLayer> {
    layer(number_, head, base, EvidenceVerdict::Pass)
}

fn merge_subject(number_: u64, head: char, base: char) -> TestResult<MergeSubject> {
    Ok(MergeSubject {
        repository: repo()?,
        number: number(number_)?,
        head: commit(head)?,
        base: commit(base)?,
    })
}

/// A house with a standing merge grant on the repository and no readiness
/// policy.
fn merge_house(id: &HouseId) -> TestResult<HouseConfig> {
    let mut config: HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    let merge = Grant::repository(
        Permission::Merge,
        repo()?,
        BackendId::new("github")?,
        CredentialId::new("gate-credential")?,
    );
    config.house = id.clone();
    config.repositories = [repo()?].into();
    config.posting_destinations = [repo()?].into();
    config.grants = [merge.clone()].into();
    config.policy_limits = [merge].into();
    Ok(config)
}

/// The readiness-checked merge grant for each `(number, head, base)`.
fn grants(subjects: &[(u64, char, char)]) -> TestResult<Vec<MergeGrant>> {
    let issued = merge_house(&house()?)?.issue_authority(&[], &[])?;
    subjects
        .iter()
        .map(|(number_, head, base)| {
            Ok(MergeGrant::resolve(
                &issued,
                &merge_subject(*number_, *head, *base)?,
                &BackendId::new("github")?,
            )?)
        })
        .collect()
}

/// A stack runner that records every command and link and answers `Done`.
#[derive(Default)]
struct Recording {
    commands: RefCell<Vec<StackCommand>>,
}

impl StackRunner for Recording {
    fn run(&self, command: &StackCommand) -> StackResult {
        self.commands.borrow_mut().push(command.clone());
        StackResult::Done
    }

    fn link(&self, _: &StackLink) -> StackResult {
        StackResult::Done
    }
}

fn run_all(runner: &Recording, commands: &[StackCommand]) -> Vec<StackResult> {
    commands.iter().map(|command| runner.run(command)).collect()
}

fn assembly(trunk: &BranchName, numbers: &[u64]) -> TestResult<Vec<StackCommand>> {
    Ok(vec![
        StackCommand::Adopt {
            trunk: trunk.clone(),
            branches: numbers
                .iter()
                .map(|number_| layer_branch(*number_))
                .collect::<TestResult<_>>()?,
        },
        StackCommand::RebaseUpstack,
        StackCommand::Submit { ready: true },
    ])
}

fn decision(layers: &[TrainLayer]) -> TestResult<TrainDecision> {
    Ok(evaluate_train(layers, &commit('0')?)?.0)
}

#[test]
fn three_overlapping_ready_pull_requests_become_one_train_and_merge_together() -> TestResult {
    let candidates = [
        ready_candidate(12, files(&["src/error.rs"]))?,
        ready_candidate(13, files(&["src/a.rs", "src/b.rs"]))?,
        ready_candidate(11, files(&["src/a.rs", "src/error.rs"]))?,
        // Ready, but it overlaps nothing: it merges alone through the gate.
        ready_candidate(14, files(&["src/c.rs"]))?,
        // Overlapping but failing: it waits for a later train.
        candidate(15, files(&["src/error.rs"]), EvidenceVerdict::Fail)?,
    ];
    let plan = plan_train(&repo()?, &trunk()?, &candidates)?;
    // #11 overlaps both others, so it goes lowest; ties by number.
    assert_eq!(
        plan.layers,
        vec![
            (number(11)?, layer_branch(11)?),
            (number(12)?, layer_branch(12)?),
            (number(13)?, layer_branch(13)?),
        ]
    );
    assert_eq!(
        plan.deferred,
        vec![
            (
                number(15)?,
                Deferral::NotReady(vec![NotReady::ChecksNotGreen])
            ),
            (number(14)?, Deferral::NoOverlap),
        ]
    );

    // The stack tool assembles the train in that order.
    let runner = Recording::default();
    let commands = plan.assembly();
    assert_eq!(commands, assembly(&trunk()?, &[11, 12, 13])?);
    assert_eq!(run_all(&runner, &commands), vec![StackResult::Done; 3]);

    // CI and review ran at every stacked head: #11 on trunk tip 0, #12 on
    // #11's head a, #13 on #12's head b.
    let train = [
        green(11, 'a', '0')?,
        green(12, 'b', 'a')?,
        green(13, 'c', 'b')?,
    ];
    let (decision, states) = evaluate_train(&train, &commit('0')?)?;
    assert_eq!(decision, TrainDecision::Merge { top: number(13)? });
    assert_eq!(states, vec![LayerState::Ready; 3]);

    let merge = merge_train(
        &house()?,
        &train,
        &commit('0')?,
        &grants(&[(11, 'a', '0'), (12, 'b', 'a'), (13, 'c', 'b')])?,
    )?;
    assert_eq!(merge.top(), number(13)?);
    assert_eq!(
        merge.layers(),
        [
            merge_subject(11, 'a', '0')?,
            merge_subject(12, 'b', 'a')?,
            merge_subject(13, 'c', 'b')?,
        ]
    );
    // One stack-tool merge lands the whole train.
    assert_eq!(runner.run(&merge.command()), StackResult::Done);
    assert_eq!(
        runner.commands.borrow().last(),
        Some(&StackCommand::Merge(merge.clone()))
    );
    assert_eq!(merge.command().permissions(), [Permission::Merge]);
    Ok(())
}

#[test]
fn a_failing_middle_layer_is_moved_up_and_the_rest_merge() -> TestResult {
    let train = [
        green(11, 'a', '0')?,
        layer(12, 'b', 'a', EvidenceVerdict::Fail)?,
        green(13, 'c', 'b')?,
    ];
    let (first, states) = evaluate_train(&train, &commit('0')?)?;
    assert_eq!(
        first,
        TrainDecision::Reorder {
            order: vec![layer_branch(11)?, layer_branch(13)?, layer_branch(12)?],
        }
    );
    assert_eq!(
        states.get(1),
        Some(&LayerState::Failed(vec![NotReady::ChecksNotGreen]))
    );
    // Nothing merges while the failing layer sits below another.
    let all = grants(&[(11, 'a', '0'), (12, 'b', 'a'), (13, 'c', 'b')])?;
    assert_eq!(
        merge_train(&house()?, &train, &commit('0')?, &all),
        Err(TrainRefusal::NotReady)
    );
    let TrainDecision::Reorder { order } = first else {
        return Err("expected a reorder".into());
    };
    let runner = Recording::default();
    let commands = reorder_commands(&trunk()?, &order);
    let mut expected = vec![StackCommand::Unstack];
    expected.extend(assembly(&trunk()?, &[11, 13, 12])?);
    assert_eq!(commands, expected);
    assert_eq!(run_all(&runner, &commands), vec![StackResult::Done; 4]);

    // After the rebase #13 sits on #11 at a new head d, and #12 on top at e.
    // While #12's CI runs again the train holds.
    let pending_top = TrainLayer {
        readiness: readiness(
            12,
            ('e', 'd'),
            ('b', 'a'),
            EvidenceVerdict::Pass,
            EvidenceVerdict::Fail,
        )?,
        branch: layer_branch(12)?,
    };
    let reordered = [green(11, 'a', '0')?, green(13, 'd', 'a')?, pending_top];
    assert_eq!(decision(&reordered)?, TrainDecision::Hold);

    // #12 fails again at its new head: the layers below it merge.
    let reordered = [
        green(11, 'a', '0')?,
        green(13, 'd', 'a')?,
        layer(12, 'e', 'd', EvidenceVerdict::Fail)?,
    ];
    assert_eq!(
        decision(&reordered)?,
        TrainDecision::Merge { top: number(13)? }
    );
    let merge = merge_train(
        &house()?,
        &reordered,
        &commit('0')?,
        &grants(&[(11, 'a', '0'), (13, 'd', 'a')])?,
    )?;
    assert_eq!(merge.top(), number(13)?);
    assert_eq!(
        merge.layers(),
        [merge_subject(11, 'a', '0')?, merge_subject(13, 'd', 'a')?]
    );
    // #13's grant must name its new base; one for its old place is not enough.
    assert_eq!(
        merge_train(
            &house()?,
            &reordered,
            &commit('0')?,
            &grants(&[(11, 'a', '0'), (13, 'c', 'b')])?,
        ),
        Err(TrainRefusal::NoMergeGrant(number(13)?))
    );
    Ok(())
}

#[test]
fn a_lower_layer_change_invalidates_upper_layer_readiness() -> TestResult {
    let before = [
        green(11, 'a', '0')?,
        green(12, 'b', 'a')?,
        green(13, 'c', 'b')?,
    ];
    assert_eq!(
        decision(&before)?,
        TrainDecision::Merge { top: number(13)? }
    );

    // A new commit 1 lands on #11; #12 and #13 still sit on its old head.
    let moved_bottom = TrainLayer {
        readiness: readiness(
            11,
            ('1', '0'),
            ('a', '0'),
            EvidenceVerdict::Pass,
            EvidenceVerdict::Pass,
        )?,
        branch: layer_branch(11)?,
    };
    let changed = [
        moved_bottom.clone(),
        green(12, 'b', 'a')?,
        green(13, 'c', 'b')?,
    ];
    let (outcome, states) = evaluate_train(&changed, &commit('0')?)?;
    assert_eq!(outcome, TrainDecision::Hold);
    assert_eq!(
        states,
        vec![
            LayerState::Pending(vec![NotReady::ReviewStale, NotReady::ChecksStale]),
            LayerState::LowerLayerChanged,
            LayerState::LowerLayerChanged,
        ]
    );
    let old_grants = grants(&[(11, 'a', '0'), (12, 'b', 'a'), (13, 'c', 'b')])?;
    assert_eq!(
        merge_train(&house()?, &changed, &commit('0')?, &old_grants),
        Err(TrainRefusal::NotReady)
    );

    // Rebased onto 1, #12 and #13 have new heads; their old evidence is
    // stale until CI runs again.
    let stale = |number_: u64, head: char, base: char, old: (char, char)| {
        Ok::<_, Box<dyn std::error::Error>>(TrainLayer {
            readiness: readiness(
                number_,
                (head, base),
                old,
                EvidenceVerdict::Pass,
                EvidenceVerdict::Pass,
            )?,
            branch: layer_branch(number_)?,
        })
    };
    let rebased = [
        green(11, '1', '0')?,
        stale(12, '2', '1', ('b', 'a'))?,
        stale(13, '3', '2', ('c', 'b'))?,
    ];
    let (outcome, states) = evaluate_train(&rebased, &commit('0')?)?;
    assert_eq!(outcome, TrainDecision::Hold);
    assert_eq!(
        states.get(1..),
        Some(
            &[
                LayerState::Pending(vec![NotReady::ReviewStale, NotReady::ChecksStale]),
                LayerState::Pending(vec![NotReady::ReviewStale, NotReady::ChecksStale]),
            ][..]
        )
    );

    // Once CI ran at every new head, the train merges, but only under grants
    // for the new heads.
    let rerun = [
        green(11, '1', '0')?,
        green(12, '2', '1')?,
        green(13, '3', '2')?,
    ];
    assert_eq!(decision(&rerun)?, TrainDecision::Merge { top: number(13)? });
    assert_eq!(
        merge_train(&house()?, &rerun, &commit('0')?, &old_grants),
        Err(TrainRefusal::NoMergeGrant(number(11)?))
    );
    let merge = merge_train(
        &house()?,
        &rerun,
        &commit('0')?,
        &grants(&[(11, '1', '0'), (12, '2', '1'), (13, '3', '2')])?,
    )?;
    assert_eq!(merge.layers().len(), 3);
    Ok(())
}

#[test]
fn a_moved_trunk_invalidates_the_whole_train() -> TestResult {
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    let (outcome, states) = evaluate_train(&train, &commit('9')?)?;
    assert_eq!(outcome, TrainDecision::Hold);
    assert_eq!(states, vec![LayerState::LowerLayerChanged; 2]);
    Ok(())
}

#[test]
fn failed_layers_on_top_never_block_the_layers_below() -> TestResult {
    let failed_top = [
        green(11, 'a', '0')?,
        green(12, 'b', 'a')?,
        layer(13, 'c', 'b', EvidenceVerdict::Fail)?,
    ];
    assert_eq!(
        decision(&failed_top)?,
        TrainDecision::Merge { top: number(12)? }
    );
    let merge = merge_train(
        &house()?,
        &failed_top,
        &commit('0')?,
        &grants(&[(11, 'a', '0'), (12, 'b', 'a')])?,
    )?;
    assert_eq!(merge.top(), number(12)?);
    assert_eq!(merge.layers().len(), 2);

    // A failed review counts like failed checks.
    let review_failed = TrainLayer {
        readiness: readiness(
            12,
            ('b', 'a'),
            ('b', 'a'),
            EvidenceVerdict::Fail,
            EvidenceVerdict::Pass,
        )?,
        branch: layer_branch(12)?,
    };
    assert_eq!(
        decision(&[green(11, 'a', '0')?, review_failed])?,
        TrainDecision::Merge { top: number(11)? }
    );

    // Every layer failed: nothing to merge.
    let all_failed = [
        layer(11, 'a', '0', EvidenceVerdict::Fail)?,
        layer(12, 'b', 'a', EvidenceVerdict::Fail)?,
    ];
    assert_eq!(decision(&all_failed)?, TrainDecision::Hold);
    assert_eq!(
        merge_train(&house()?, &all_failed, &commit('0')?, &[]),
        Err(TrainRefusal::NotReady)
    );
    Ok(())
}

#[test]
fn unreadable_evidence_holds_the_train_and_is_never_a_failure() -> TestResult {
    let train = [
        green(11, 'a', '0')?,
        layer(12, 'b', 'a', EvidenceVerdict::Unavailable)?,
        green(13, 'c', 'b')?,
    ];
    let (outcome, states) = evaluate_train(&train, &commit('0')?)?;
    assert_eq!(outcome, TrainDecision::Hold);
    assert_eq!(
        states.get(1),
        Some(&LayerState::Pending(vec![NotReady::ChecksNotGreen]))
    );
    // A failure reported about another head is stale, not a failure here.
    let old_failure = TrainLayer {
        readiness: readiness(
            12,
            ('b', 'a'),
            ('8', 'a'),
            EvidenceVerdict::Pass,
            EvidenceVerdict::Fail,
        )?,
        branch: layer_branch(12)?,
    };
    let (outcome, states) = evaluate_train(
        &[green(11, 'a', '0')?, old_failure, green(13, 'c', 'b')?],
        &commit('0')?,
    )?;
    assert_eq!(outcome, TrainDecision::Hold);
    assert!(matches!(states.get(1), Some(LayerState::Pending(_))));
    Ok(())
}

#[test]
fn a_merge_needs_a_grant_for_every_layer_in_this_house() -> TestResult {
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    assert_eq!(
        merge_train(
            &house()?,
            &train,
            &commit('0')?,
            &grants(&[(11, 'a', '0')])?
        ),
        Err(TrainRefusal::NoMergeGrant(number(12)?))
    );
    assert_eq!(
        merge_train(&house()?, &train, &commit('0')?, &[]),
        Err(TrainRefusal::NoMergeGrant(number(11)?))
    );
    // A grant resolved without a standing merge permission covers nothing.
    assert_eq!(
        merge_train(
            &house()?,
            &train,
            &commit('0')?,
            &[MergeGrant::none(), MergeGrant::none()],
        ),
        Err(TrainRefusal::NoMergeGrant(number(11)?))
    );
    // Grants from this house do not merge a train in another.
    let both = grants(&[(11, 'a', '0'), (12, 'b', 'a')])?;
    assert_eq!(
        merge_train(
            &HouseId::new(common::OTHER_HOUSE)?,
            &train,
            &commit('0')?,
            &both
        ),
        Err(TrainRefusal::NoMergeGrant(number(11)?))
    );
    assert!(merge_train(&house()?, &train, &commit('0')?, &both).is_ok());
    Ok(())
}

#[test]
fn invalid_train_input_is_refused() -> TestResult {
    let ok = ready_candidate(11, files(&["src/a.rs"]))?;
    let mut other_repo = ready_candidate(12, files(&["src/a.rs"]))?;
    other_repo.readiness.repository = Repository::new("lemarier/other")?;
    let mut same_number = ready_candidate(12, files(&["src/a.rs"]))?;
    same_number.readiness.pull_request = number(11)?;
    let mut same_branch = ready_candidate(12, files(&["src/a.rs"]))?;
    same_branch.branch = layer_branch(11)?;
    let mut on_trunk = ready_candidate(12, files(&["src/a.rs"]))?;
    on_trunk.branch = trunk()?;
    for bad in [other_repo, same_number, same_branch, on_trunk] {
        assert_eq!(
            plan_train(&repo()?, &trunk()?, &[ok.clone(), bad]),
            Err(TrainError::InvalidLayers)
        );
    }

    let too_many: Vec<TrainLayer> = (0..=MAX_STACK_LAYERS)
        .map(|index| green(u64::try_from(index)?.saturating_add(1), 'a', '0'))
        .collect::<TestResult<_>>()?;
    let mut repeated = green(12, 'b', 'a')?;
    repeated.branch = layer_branch(11)?;
    let mut elsewhere = green(12, 'b', 'a')?;
    elsewhere.readiness.repository = Repository::new("lemarier/other")?;
    for bad in [
        Vec::new(),
        too_many,
        vec![green(11, 'a', '0')?, repeated],
        vec![green(11, 'a', '0')?, green(11, 'b', 'a')?],
        vec![green(11, 'a', '0')?, elsewhere],
    ] {
        assert_eq!(
            evaluate_train(&bad, &commit('0')?),
            Err(TrainError::InvalidLayers)
        );
        assert_eq!(
            merge_train(&house()?, &bad, &commit('0')?, &[]),
            Err(TrainRefusal::Invalid(TrainError::InvalidLayers))
        );
    }
    Ok(())
}

#[test]
fn a_train_is_bounded_and_needs_two_overlapping_ready_pull_requests() -> TestResult {
    let shared = files(&["src/error.rs"]);
    let crowd: Vec<TrainCandidate> = (1..=10)
        .map(|number_| ready_candidate(number_, shared.clone()))
        .collect::<TestResult<_>>()?;
    let plan = plan_train(&repo()?, &trunk()?, &crowd)?;
    assert_eq!(plan.layers.len(), MAX_STACK_LAYERS);
    assert_eq!(plan.layers.first().map(|(n, _)| n.get()), Some(1));
    assert_eq!(
        plan.deferred,
        vec![(number(9)?, Deferral::Full), (number(10)?, Deferral::Full)]
    );

    // Contracts overlap like files do.
    let contract = |name: &str| [Touch::Contract(name.to_owned())].into_iter().collect();
    let plan = plan_train(
        &repo()?,
        &trunk()?,
        &[
            ready_candidate(21, contract("kitchen::Error"))?,
            ready_candidate(22, contract("kitchen::Error"))?,
        ],
    )?;
    assert_eq!(plan.layers.len(), 2);

    // One ready pull request, or none that overlap, is no train.
    for alone in [
        vec![ready_candidate(31, shared.clone())?],
        vec![
            ready_candidate(31, files(&["src/a.rs"]))?,
            ready_candidate(32, files(&["src/b.rs"]))?,
        ],
        Vec::new(),
    ] {
        let plan = plan_train(&repo()?, &trunk()?, &alone)?;
        assert!(plan.layers.is_empty());
        assert!(plan.assembly().is_empty());
    }
    Ok(())
}

#[test]
fn gh_stack_merges_the_train_by_its_top_pull_request_without_prompting() -> TestResult {
    let temp = tempfile::tempdir()?;
    let gh = GhStack::new(
        "/usr/bin/gh".into(),
        "/tmp/checkout".into(),
        "upstream",
        workflows_support::isolated_config(temp.path(), &[])?,
        Duration::from_secs(5),
    )?;
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    let merge = merge_train(
        &house()?,
        &train,
        &commit('0')?,
        &grants(&[(11, 'a', '0'), (12, 'b', 'a')])?,
    )?;
    assert_eq!(
        gh.args(&merge.command()).join(" "),
        "stack merge 12 --yes --squash"
    );
    assert_eq!(
        gh.args(&StackCommand::Unstack).join(" "),
        "stack unstack --local"
    );
    assert_eq!(
        StackCommand::Unstack.permissions(),
        [Permission::PushBranch]
    );
    assert!(!merge.command().touches_upstack());
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_failed_merge_call_is_uncertain_and_a_refusal_is_not() -> TestResult {
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    let merge = merge_train(
        &house()?,
        &train,
        &commit('0')?,
        &grants(&[(11, 'a', '0'), (12, 'b', 'a')])?,
    )?;
    for (code, expected) in [
        (0, StackResult::Done),
        // A generic or API failure may follow a merge that landed.
        (1, StackResult::Uncertain),
        (5, StackResult::Rejected),
        (8, StackResult::Locked),
    ] {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().canonicalize()?;
        let gh_path = dir.join("gh");
        common::executable::write_executable(&gh_path, format!("#!/bin/sh\nexit {code}\n"))?;
        let kitchen = tempfile::tempdir()?;
        let gh = GhStack::new(
            gh_path,
            dir.clone(),
            "origin",
            workflows_support::isolated_config(kitchen.path(), &[])?,
            Duration::from_secs(5),
        )?;
        assert_eq!(gh.run(&merge.command()), expected, "exit {code}");
    }
    Ok(())
}
