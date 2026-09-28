//! Stack-tool enforcement: plain operations on a dependent branch are
//! refused, the typed `gh stack` adapter runs non-interactively with an
//! explicit remote, doctor reports a missing tool, and a merged base yields
//! per-writer retarget steps. The adapter runs a recording stand-in for
//! `gh`, not the real extension; everything else uses temporary stores.

mod common;
mod workflows_support;

use std::{cell::RefCell, collections::BTreeSet, time::Duration};

use common::{TestResult, commit, ttl};
use kitchen::{
    BackendId, TaskId,
    contracts::{Fence, Grant, HouseGrants, IssueNumber, Permission, TaskAuthority},
    house::{DoctorCode, StackTool, StackToolStatus, Workflow, stack_tool_finding},
    state::StateError,
    workflows::{
        pickup::{ClaimOutcome, TaskTemplate, claim_issue, issue_task_id},
        stack::{
            BranchLayer, BranchOperation, Dependent, GhStack, MAX_STACK_LAYERS, MergedBase,
            RetargetStep, StackBoundary, StackCommand, StackOutcome, StackRefusal, StackResult,
            StackRunner, Upstack, check_plain, plan_retarget,
        },
    },
};
use workflows_support::{World, branch, issue, template, under_consumer};

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

struct Recording {
    commands: RefCell<Vec<StackCommand>>,
    answer: StackResult,
}

impl Recording {
    fn answering(answer: StackResult) -> Self {
        Self {
            commands: RefCell::new(Vec::new()),
            answer,
        }
    }
}

impl StackRunner for Recording {
    fn run(&self, command: &StackCommand) -> StackResult {
        self.commands.borrow_mut().push(command.clone());
        self.answer.clone()
    }
}

struct Stacking {
    world: World,
    task: TaskId,
    fence: Fence,
    github: BackendId,
}

fn stacking_with(grants: &HouseGrants, requested: Vec<Grant>) -> TestResult<Stacking> {
    let mut world = World::new()?;
    world.grants = grants.clone();
    let mut base: TaskTemplate = template()?;
    base.authority = TaskAuthority::delegate(grants, requested)?;
    let (claimant, _) = under_consumer(&world, "coordinator")?;
    let ClaimOutcome::Claimed(lease) = claim_issue(
        &world.fixture.store,
        &base,
        &issue(5)?,
        &claimant,
        ttl(300)?,
        world.now(),
    )?
    else {
        return Err("issue not claimed".into());
    };
    Ok(Stacking {
        world,
        task: issue_task_id(&issue(5)?)?,
        fence: lease.fence(),
        github: BackendId::new("github")?,
    })
}

fn push_grant() -> TestResult<Grant> {
    Ok(Grant::repository(
        Permission::PushBranch,
        workflows_support::repo()?,
        BackendId::new("github")?,
        common::credential()?,
    ))
}

fn stacking() -> TestResult<Stacking> {
    let grant = push_grant()?;
    let grants = HouseGrants::new(common::house()?, [grant.clone()]);
    stacking_with(&grants, vec![grant])
}

fn run(
    setup: &Stacking,
    upstack: Upstack,
    runner: &Recording,
    command: &StackCommand,
) -> TestResult<StackOutcome> {
    let own = branch("lemarier/issue-5")?;
    Ok(StackBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        branch: &own,
        upstack,
        clock: &setup.world.clock,
        runner,
    }
    .run(&setup.task, setup.fence, command)?)
}

#[test]
fn the_stack_tool_path_runs_commands_bound_to_the_tasks_branch() -> TestResult {
    let setup = stacking()?;
    let runner = Recording::answering(StackResult::Done);
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
        (Upstack::Unknown, adopt.clone()),
        (
            Upstack::Busy,
            StackCommand::Add {
                branch: branch("lemarier/issue-5")?,
            },
        ),
        (Upstack::Busy, StackCommand::View),
        (Upstack::Top, StackCommand::RebaseUpstack),
        (Upstack::Idle, StackCommand::Push),
        (Upstack::Idle, StackCommand::Submit { ready: false }),
    ];
    for (upstack, command) in &accepted {
        assert_eq!(
            run(&setup, *upstack, &runner, command)?,
            StackOutcome::Ran(StackResult::Done)
        );
    }
    assert_eq!(
        runner.commands.borrow().as_slice(),
        accepted
            .iter()
            .map(|(_, command)| command.clone())
            .collect::<Vec<_>>()
            .as_slice()
    );
    Ok(())
}

#[test]
fn the_stack_boundary_refuses_other_branches_busy_layers_and_bad_chains() -> TestResult {
    let setup = stacking()?;
    let runner = Recording::answering(StackResult::Done);
    let too_many: Vec<_> = (0..=MAX_STACK_LAYERS)
        .map(|layer| branch(&format!("lemarier/layer-{layer}")))
        .chain([branch("lemarier/issue-5")])
        .collect::<TestResult<_>>()?;
    let refused = [
        (
            Upstack::Top,
            StackCommand::Add {
                branch: branch("lemarier/issue-6")?,
            },
            StackRefusal::ForeignBranch,
        ),
        (
            Upstack::Top,
            StackCommand::Adopt {
                trunk: branch("main")?,
                branches: vec![branch("lemarier/issue-4")?],
            },
            StackRefusal::ForeignBranch,
        ),
        (
            Upstack::Top,
            StackCommand::Adopt {
                trunk: branch("main")?,
                branches: Vec::new(),
            },
            StackRefusal::InvalidLayers,
        ),
        (
            Upstack::Top,
            StackCommand::Adopt {
                trunk: branch("main")?,
                branches: vec![branch("lemarier/issue-5")?, branch("lemarier/issue-5")?],
            },
            StackRefusal::InvalidLayers,
        ),
        (
            Upstack::Top,
            StackCommand::Adopt {
                trunk: branch("main")?,
                branches: too_many,
            },
            StackRefusal::InvalidLayers,
        ),
        // Never rewrite or push layers another writer is working on.
        (
            Upstack::Busy,
            StackCommand::RebaseUpstack,
            StackRefusal::UpstackBusy,
        ),
        (
            Upstack::Unknown,
            StackCommand::Push,
            StackRefusal::UpstackBusy,
        ),
        (
            Upstack::Busy,
            StackCommand::Submit { ready: true },
            StackRefusal::UpstackBusy,
        ),
    ];
    for (upstack, command, refusal) in refused {
        assert_eq!(
            run(&setup, upstack, &runner, &command)?,
            StackOutcome::Refused(refusal)
        );
    }
    assert!(runner.commands.borrow().is_empty());
    Ok(())
}

#[test]
fn the_stack_boundary_needs_the_live_claim_and_the_push_grant() -> TestResult {
    let runner = Recording::answering(StackResult::Done);
    // No delegated push grant.
    let other = Grant::repository(
        Permission::PushBranch,
        kitchen::contracts::Repository::new("origin89hq/other")?,
        BackendId::new("github")?,
        common::credential()?,
    );
    let grants = HouseGrants::new(common::house()?, [push_grant()?, other.clone()]);
    let unauthorized = stacking_with(&grants, vec![other])?;
    assert!(run(&unauthorized, Upstack::Top, &runner, &StackCommand::Push).is_err());
    // An expired claim is stale.
    let setup = stacking()?;
    setup.world.clock.advance(301);
    let own = branch("lemarier/issue-5")?;
    let result = StackBoundary {
        store: &setup.world.fixture.store,
        grants: &setup.world.grants,
        destination: &setup.github,
        branch: &own,
        upstack: Upstack::Top,
        clock: &setup.world.clock,
        runner: &runner,
    }
    .run(&setup.task, setup.fence, &StackCommand::Push);
    assert!(matches!(
        result,
        Err(kitchen::Error::State(StateError::StaleFence { .. }))
    ));
    assert!(runner.commands.borrow().is_empty());
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
