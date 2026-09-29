//! Merge trains: overlapping ready pull requests are planned into one
//! stack, judged layer by layer at their exact stacked heads, and landed
//! bottom first through the gate's head-matched merge. Assembly runs
//! against a recording stack runner and the forge re-read against a
//! scripted transport; no forge or `gh stack` is contacted.

mod common;

use std::{
    cell::RefCell, collections::BTreeSet, collections::VecDeque, path::PathBuf, time::Duration,
};

use common::{TestResult, commit, house};
use kitchen::{
    BackendId, CredentialId, HouseId,
    contracts::{
        BranchName, EvidenceSubject, EvidenceVerdict, ExternalRef, GitHubAction, Grant,
        IssueNumber, MergeMethod, Permission, Repository,
    },
    house::{HouseConfig, MergeSubject},
    integrations::github::{
        CredentialRef, GitHubClient, GitHubReadTransport, HouseScope, IntegrationError,
        MergeStatusValue, PostingBudget, ReadLimits, ReadRequest,
    },
    workflows::{
        gate::{
            self, Admission, BaseTipRead, Checks, ExpectedReviewer, FixGrant, Gap, GateEvidence,
            GateGrants, GateHistory, GateMode, GateRun, MergeGrant, RecordedDecision,
            ReviewTriggers, ReviewerOutcome, SemanticReview, Verdict,
        },
        ready::{HeadEvidence, MergeReadiness, NotReady},
        stack::{MAX_STACK_LAYERS, StackCommand, StackLink, StackResult, StackRunner},
        train::{
            Deferral, LayerState, Touch, TrainCandidate, TrainDecision, TrainError, TrainLayer,
            TrainRefusal, evaluate_train, merge_train, plan_train,
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

fn numbers(values: &[u64]) -> TestResult<Vec<IssueNumber>> {
    values.iter().map(|value| number(*value)).collect()
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

/// A layer at `head` on `base` whose evidence is about an older subject.
fn stale(number_: u64, head: char, base: char, old: (char, char)) -> TestResult<TrainLayer> {
    Ok(TrainLayer {
        readiness: readiness(
            number_,
            (head, base),
            old,
            EvidenceVerdict::Pass,
            EvidenceVerdict::Pass,
        )?,
        branch: layer_branch(number_)?,
    })
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

/// The readiness-checked merge grant for pull request `number_` at `head`
/// on `base`.
fn grant(number_: u64, head: char, base: char) -> TestResult<MergeGrant> {
    let issued = merge_house(&house()?)?.issue_authority(&[], &[])?;
    Ok(MergeGrant::resolve(
        &issued,
        &merge_subject(number_, head, base)?,
        &BackendId::new("github")?,
    )?)
}

/// Gate evidence for pull request `number_` at `head` on the trunk at
/// `base`, with every rule met.
fn gate_evidence(number_: u64, head: char, base: char) -> TestResult<GateEvidence> {
    let (head, base) = (commit(head)?, commit(base)?);
    Ok(GateEvidence {
        house: house()?,
        repository: repo()?,
        number: number(number_)?,
        head: head.clone(),
        head_branch: Some(format!("lemarier/pr-{number_}")),
        base: base.clone(),
        base_branch: Some(trunk()?),
        base_tip: BaseTipRead::Read,
        head_age: Some(Duration::from_secs(3600)),
        open: Some(true),
        draft: Some(false),
        same_repository: Some(true),
        targets_default: Some(true),
        author_allowed: Some(true),
        merge_state: Some(MergeStatusValue::Clean),
        contains_base: Some(true),
        checks: Checks::Passed,
        reviewers: vec![ExpectedReviewer {
            name: "reviewer".into(),
            reviewed_head: Some(head.clone()),
            outcome: ReviewerOutcome::Clean,
        }],
        threads_resolved: Some(true),
        no_change_request: Some(true),
        semantic_review: SemanticReview::Clean,
        semantic_source: Some(ExternalRef::new(
            "https://github.com/lemarier/kitchen/pull/1#review",
        )?),
        verified_findings: Vec::new(),
        disproved_findings: Vec::new(),
        semantic_head: Some(head.clone()),
        semantic_base: Some(base.clone()),
        semantic_read_only: true,
        semantic_independent: true,
        acceptance_met: Some(true),
        hardware_complete: Some(true),
        risk_classes: Some(Vec::new()),
        risk_approval: None,
        writer_working: false,
        supporting_subject: Some((head, base)),
        reopen_event: None,
        follow_up: common::house_with_fix_rounds(None)?.follow_up_budget(),
    })
}

/// The gate's decision on `e` under a merge grant for its exact subject,
/// admitted for one submission as the durable store would admit it.
fn recorded(e: &GateEvidence) -> TestResult<RecordedDecision> {
    let head = e.head.as_str().chars().next().ok_or("empty head")?;
    let base = e.base.as_str().chars().next().ok_or("empty base")?;
    let grants = GateGrants {
        merge: grant(e.number.get(), head, base)?,
        fix_request: FixGrant::none(),
        review_triggers: ReviewTriggers::none(),
    };
    Ok(RecordedDecision {
        decision: gate::evaluate(e, grants, GateHistory::default()),
        mode: GateMode::Active,
        admission: Admission::Submit(kitchen::contracts::IdempotencyKey::from_ref(
            ExternalRef::new("fake:effect/train")?,
        )),
    })
}

/// The gate's recorded merge of pull request `number_` at `head` on the
/// trunk at `base`.
fn gate_merge(number_: u64, head: char, base: char) -> TestResult<RecordedDecision> {
    let decision = recorded(&gate_evidence(number_, head, base)?)?;
    assert_eq!(decision.decision.verdict, Verdict::Merge);
    Ok(decision)
}

/// A forge transport that answers reads from a script, in order.
struct Forge {
    responses: RefCell<VecDeque<serde_json::Value>>,
    reads: RefCell<usize>,
}

impl GitHubReadTransport for Forge {
    fn read(
        &self,
        _: &CredentialRef,
        _: &ReadRequest,
        _: Duration,
        _: usize,
    ) -> Result<Vec<u8>, IntegrationError> {
        *self.reads.borrow_mut() += 1;
        let next = self
            .responses
            .borrow_mut()
            .pop_front()
            .ok_or(IntegrationError::Unavailable)?;
        serde_json::to_vec(&next).map_err(|_| IntegrationError::Unknown)
    }
}

fn client(responses: Vec<serde_json::Value>) -> TestResult<GitHubClient<Forge>> {
    let requester = ExternalRef::new("train-reader")?;
    let scope = HouseScope::new(
        house()?,
        [repo()?],
        requester.clone(),
        CredentialRef::new(house()?, CredentialId::new("read")?, requester),
        PostingBudget::new(0)?,
        [],
    )?;
    Ok(GitHubClient::new(
        scope,
        Forge {
            responses: RefCell::new(responses.into()),
            reads: RefCell::new(0),
        },
        ReadLimits::default(),
    ))
}

/// The forge as it reads just before a merge: pull request `number_` open
/// at `head` against `base_ref`, and the trunk's tip at `tip`.
fn forge(number_: u64, head: char, base_ref: &str, tip: char) -> TestResult<GitHubClient<Forge>> {
    let (head, tip) = (commit(head)?, commit(tip)?);
    client(vec![
        serde_json::json!({
            "number": number_, "state": "open", "draft": false, "merged": false,
            "head": {"sha": head.as_str(), "ref": format!("lemarier/pr-{number_}"),
                     "repo": {"full_name": "lemarier/kitchen"}},
            "base": {"sha": tip.as_str(), "ref": base_ref},
            "mergeable": true, "user": {"login": "allowed"}
        }),
        serde_json::json!({"name": "main", "commit": {"sha": tip.as_str()}}),
    ])
}

/// A forge that must not be read: every read fails and is counted.
fn unread() -> TestResult<GitHubClient<Forge>> {
    client(Vec::new())
}

/// A stack runner that records every command and answers `Done`.
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

fn decision(layers: &[TrainLayer]) -> TestResult<TrainDecision> {
    Ok(evaluate_train(layers, &commit('0')?)?.0)
}

fn merging(ready: &[u64], deferred: &[u64]) -> TestResult<TrainDecision> {
    Ok(TrainDecision::Merge {
        ready: numbers(ready)?,
        deferred: numbers(deferred)?,
    })
}

#[test]
fn overlapping_ready_pull_requests_become_one_train_and_land_bottom_first() -> TestResult {
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
    assert_eq!(
        commands,
        vec![
            StackCommand::Adopt {
                trunk: trunk()?,
                branches: vec![layer_branch(11)?, layer_branch(12)?, layer_branch(13)?],
            },
            StackCommand::RebaseUpstack,
            StackCommand::Submit { ready: true },
        ]
    );
    for command in &commands {
        assert_eq!(runner.run(command), StackResult::Done);
    }
    assert_eq!(*runner.commands.borrow(), commands);

    // CI and review ran at every stacked head: #11 on trunk tip 0, #12 on
    // #11's head a, #13 on #12's head b.
    let train = [
        green(11, 'a', '0')?,
        green(12, 'b', 'a')?,
        green(13, 'c', 'b')?,
    ];
    let (outcome, states) = evaluate_train(&train, &commit('0')?)?;
    assert_eq!(outcome, merging(&[11, 12, 13], &[])?);
    assert_eq!(states, vec![LayerState::Ready; 3]);

    // The bottom layer lands through the gate's head-matched squash merge.
    let request = merge_train(
        &house()?,
        &train,
        &trunk()?,
        &gate_merge(11, 'a', '0')?,
        &grant(11, 'a', '0')?,
        &GateRun::new(),
        &forge(11, 'a', "main", '0')?,
    )?;
    assert_eq!(request.number, number(11)?);
    assert_eq!(request.match_head, commit('a')?);
    assert_eq!(request.checked_base, commit('0')?);
    assert_eq!(request.base_branch, trunk()?);
    assert_eq!(
        request.mutation().action,
        GitHubAction::MergePullRequest {
            number: number(11)?,
            expected_head: commit('a')?,
            expected_base: trunk()?,
            expected_base_commit: Some(commit('0')?),
            method: MergeMethod::Squash,
        }
    );

    // #11 squashed onto trunk as f. Rebased onto it, #12 and #13 have new
    // heads d and e; once their CI ran there, #12 is the next bottom.
    let next = [green(12, 'd', 'f')?, green(13, 'e', 'd')?];
    assert_eq!(
        evaluate_train(&next, &commit('f')?)?.0,
        merging(&[12, 13], &[])?
    );
    let request = merge_train(
        &house()?,
        &next,
        &trunk()?,
        &gate_merge(12, 'd', 'f')?,
        &grant(12, 'd', 'f')?,
        &GateRun::new(),
        &forge(12, 'd', "main", 'f')?,
    )?;
    assert_eq!(
        (request.number, request.match_head),
        (number(12)?, commit('d')?)
    );
    Ok(())
}

#[test]
fn a_pending_middle_layer_never_blocks_the_ready_layers_below() -> TestResult {
    let unreadable = [
        green(11, 'a', '0')?,
        layer(12, 'b', 'a', EvidenceVerdict::Unavailable)?,
        green(13, 'c', 'b')?,
    ];
    let (outcome, states) = evaluate_train(&unreadable, &commit('0')?)?;
    // #12 and everything above it wait for the next train; #11 merges.
    assert_eq!(outcome, merging(&[11], &[12, 13])?);
    assert_eq!(
        states,
        vec![
            LayerState::Ready,
            LayerState::Pending(vec![NotReady::ChecksNotGreen]),
            LayerState::Ready,
        ]
    );
    let request = merge_train(
        &house()?,
        &unreadable,
        &trunk()?,
        &gate_merge(11, 'a', '0')?,
        &grant(11, 'a', '0')?,
        &GateRun::new(),
        &forge(11, 'a', "main", '0')?,
    )?;
    assert_eq!(request.number, number(11)?);

    // A failure reported about another head is stale, not a failure here.
    let (outcome, states) = evaluate_train(
        &[
            green(11, 'a', '0')?,
            TrainLayer {
                readiness: readiness(
                    12,
                    ('b', 'a'),
                    ('8', 'a'),
                    EvidenceVerdict::Pass,
                    EvidenceVerdict::Fail,
                )?,
                branch: layer_branch(12)?,
            },
            green(13, 'c', 'b')?,
        ],
        &commit('0')?,
    )?;
    assert_eq!(outcome, merging(&[11], &[12, 13])?);
    assert!(matches!(states.get(1), Some(LayerState::Pending(_))));

    // A pending bottom layer holds everything: nothing below it can merge.
    let pending_bottom = [
        layer(11, 'a', '0', EvidenceVerdict::Unavailable)?,
        green(12, 'b', 'a')?,
    ];
    assert_eq!(decision(&pending_bottom)?, TrainDecision::Hold);
    assert_eq!(
        merge_train(
            &house()?,
            &pending_bottom,
            &trunk()?,
            &gate_merge(11, 'a', '0')?,
            &grant(11, 'a', '0')?,
            &GateRun::new(),
            &unread()?,
        ),
        Err(TrainRefusal::NotReady)
    );
    Ok(())
}

#[test]
fn a_failed_layer_is_deferred_and_a_failed_bottom_is_removed_by_its_owner() -> TestResult {
    let failed_middle = [
        green(11, 'a', '0')?,
        layer(12, 'b', 'a', EvidenceVerdict::Fail)?,
        green(13, 'c', 'b')?,
    ];
    let (outcome, states) = evaluate_train(&failed_middle, &commit('0')?)?;
    assert_eq!(outcome, merging(&[11], &[12, 13])?);
    assert_eq!(
        states.get(1),
        Some(&LayerState::Failed(vec![NotReady::ChecksNotGreen]))
    );
    assert_eq!(
        decision(&[
            green(11, 'a', '0')?,
            green(12, 'b', 'a')?,
            layer(13, 'c', 'b', EvidenceVerdict::Fail)?,
        ])?,
        merging(&[11, 12], &[13])?
    );
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
        merging(&[11], &[12])?
    );

    // A failed bottom layer is not reordered: its owner removes it.
    for failed_bottom in [
        vec![
            layer(11, 'a', '0', EvidenceVerdict::Fail)?,
            green(12, 'b', 'a')?,
        ],
        vec![
            layer(11, 'a', '0', EvidenceVerdict::Fail)?,
            layer(12, 'b', 'a', EvidenceVerdict::Fail)?,
        ],
    ] {
        assert_eq!(
            decision(&failed_bottom)?,
            TrainDecision::RemoveBottom {
                failed: number(11)?
            }
        );
        assert_eq!(
            merge_train(
                &house()?,
                &failed_bottom,
                &trunk()?,
                &gate_merge(11, 'a', '0')?,
                &grant(11, 'a', '0')?,
                &GateRun::new(),
                &unread()?,
            ),
            Err(TrainRefusal::NotReady)
        );
    }
    Ok(())
}

#[test]
fn a_lower_layer_change_invalidates_upper_layer_readiness() -> TestResult {
    // A new commit 1 lands on #11; #12 and #13 still sit on its old head.
    let changed = [
        stale(11, '1', '0', ('a', '0'))?,
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

    // Rebased onto 1, #12 and #13 have new heads; their old evidence is
    // stale until CI runs again, so only #11 may merge, and only under a
    // gate merge recorded for its new head.
    let rebased = [
        green(11, '1', '0')?,
        stale(12, '2', '1', ('b', 'a'))?,
        stale(13, '3', '2', ('c', 'b'))?,
    ];
    let (outcome, states) = evaluate_train(&rebased, &commit('0')?)?;
    assert_eq!(outcome, merging(&[11], &[12, 13])?);
    assert_eq!(
        states.get(1..),
        Some(
            &[
                LayerState::Pending(vec![NotReady::ReviewStale, NotReady::ChecksStale]),
                LayerState::Pending(vec![NotReady::ReviewStale, NotReady::ChecksStale]),
            ][..]
        )
    );
    assert_eq!(
        merge_train(
            &house()?,
            &rebased,
            &trunk()?,
            &gate_merge(11, 'a', '0')?,
            &grant(11, 'a', '0')?,
            &GateRun::new(),
            &unread()?,
        ),
        Err(TrainRefusal::NoGateMerge(number(11)?))
    );

    // A moved trunk invalidates the whole train.
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    let (outcome, states) = evaluate_train(&train, &commit('9')?)?;
    assert_eq!(outcome, TrainDecision::Hold);
    assert_eq!(states, vec![LayerState::LowerLayerChanged; 2]);
    Ok(())
}

#[test]
fn a_grant_and_passing_evidence_do_not_merge_a_layer_with_a_gate_gap() -> TestResult {
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    let attempt = |recorded: &RecordedDecision, grant: &MergeGrant, house: &HouseId| {
        let forge = unread()?;
        let result = merge_train(
            house,
            &train,
            &trunk()?,
            recorded,
            grant,
            &GateRun::new(),
            &forge,
        );
        // Every refusal here comes before any forge read.
        assert_eq!(*forge.transport().reads.borrow(), 0);
        Ok::<_, Box<dyn std::error::Error>>(result)
    };
    let granted = grant(11, 'a', '0')?;
    let refused = Err(TrainRefusal::NoGateMerge(number(11)?));

    // Review and checks pass and the grant covers the exact subject, but a
    // review thread is unresolved: the gate records no merge.
    let mut threads = gate_evidence(11, 'a', '0')?;
    threads.threads_resolved = Some(false);
    let gap = recorded(&threads)?;
    assert!(
        matches!(&gap.decision.verdict, Verdict::HandOver { gaps } if gaps.contains(&Gap::Threads))
    );
    assert_eq!(attempt(&gap, &granted, &house()?)?, refused);
    // The same for a missing independent semantic review or acceptance.
    let mut unreviewed = gate_evidence(11, 'a', '0')?;
    unreviewed.semantic_independent = false;
    assert_eq!(
        attempt(&recorded(&unreviewed)?, &granted, &house()?)?,
        refused
    );
    let mut unaccepted = gate_evidence(11, 'a', '0')?;
    unaccepted.acceptance_met = Some(false);
    assert_eq!(
        attempt(&recorded(&unaccepted)?, &granted, &house()?)?,
        refused
    );

    // A gate merge recorded for another subject does not count: another
    // head, base branch, pull request, repository, or house.
    let mut other_base_branch = gate_merge(11, 'a', '0')?;
    other_base_branch.decision.base_branch = Some(branch("release")?);
    let mut other_repository = gate_merge(11, 'a', '0')?;
    other_repository.decision.repository = Repository::new("lemarier/other")?;
    let mut other_house = gate_merge(11, 'a', '0')?;
    other_house.decision.house = common::other_house()?;
    for elsewhere in [
        gate_merge(11, 'f', '0')?,
        gate_merge(12, 'b', '0')?,
        other_base_branch,
        other_repository,
        other_house,
    ] {
        assert_eq!(attempt(&elsewhere, &granted, &house()?)?, refused);
    }
    // A gate merge on another trunk tip judges the train against that tip,
    // where the bottom layer's base is stale.
    assert_eq!(
        attempt(&gate_merge(11, 'a', '9')?, &granted, &house()?)?,
        Err(TrainRefusal::NotReady)
    );

    // With the gate merge recorded, the grant must still cover the subject.
    let merge = gate_merge(11, 'a', '0')?;
    let no_grant = Err(TrainRefusal::NoMergeGrant(number(11)?));
    assert_eq!(attempt(&merge, &MergeGrant::none(), &house()?)?, no_grant);
    assert_eq!(attempt(&merge, &grant(11, 'f', '0')?, &house()?)?, no_grant);
    let mut elsewhere = gate_merge(11, 'a', '0')?;
    elsewhere.decision.house = common::other_house()?;
    assert_eq!(
        attempt(&elsewhere, &granted, &common::other_house()?)?,
        no_grant
    );
    Ok(())
}

#[test]
fn the_forge_is_read_again_just_before_the_merge() -> TestResult {
    let train = [green(11, 'a', '0')?, green(12, 'b', 'a')?];
    let attempt = |forge: &GitHubClient<Forge>, recorded: &RecordedDecision| {
        Ok::<_, Box<dyn std::error::Error>>(
            merge_train(
                &house()?,
                &train,
                &trunk()?,
                recorded,
                &grant(11, 'a', '0')?,
                &GateRun::new(),
                forge,
            )
            .map(|request| request.match_head),
        )
    };
    let merge = gate_merge(11, 'a', '0')?;
    let stale = TrainRefusal::Forge(IntegrationError::StaleDecision);
    // A push after the train was read, a moved trunk, or a retargeted pull
    // request is refused.
    for (forge, expected) in [
        (forge(11, 'f', "main", '0')?, stale),
        (forge(11, 'a', "main", '9')?, stale),
        (forge(11, 'a', "release", '0')?, stale),
        (
            unread()?,
            TrainRefusal::Forge(IntegrationError::Unavailable),
        ),
    ] {
        assert_eq!(attempt(&forge, &merge)?, Err(expected));
    }
    // A recorded merge whose effect is already in flight, or a report-only
    // record, never submits again.
    let mut reconcile = gate_merge(11, 'a', '0')?;
    reconcile.admission = Admission::Reconcile(kitchen::contracts::IdempotencyKey::from_ref(
        ExternalRef::new("fake:effect/train")?,
    ));
    let mut report_only = gate_merge(11, 'a', '0')?;
    report_only.mode = GateMode::ReportOnly;
    for recorded in [reconcile, report_only] {
        assert_eq!(
            attempt(&forge(11, 'a', "main", '0')?, &recorded)?,
            Err(stale)
        );
    }
    assert_eq!(
        attempt(&forge(11, 'a', "main", '0')?, &merge)?,
        Ok(commit('a')?)
    );
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
            merge_train(
                &house()?,
                &bad,
                &trunk()?,
                &gate_merge(11, 'a', '0')?,
                &grant(11, 'a', '0')?,
                &GateRun::new(),
                &unread()?,
            ),
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
