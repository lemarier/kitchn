//! The owner entrypoint refuses worker context and headless confirmation.

use std::{fs, path::Path, process::Command};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn git(root: &Path, args: &[&str]) -> TestResult {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

fn preserve(
    cwd: &Path,
    target: &Path,
    registry: &Path,
    confirm: bool,
) -> TestResult<std::process::Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kitchn"));
    command
        .env_remove("ORCA_TERMINAL_HANDLE")
        .env_remove("ORCA_DISPATCH_ID")
        .current_dir(cwd)
        .args([
            "preserve",
            "task-1",
            "--pull-request",
            "1",
            "--head",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--holder",
            "person",
            "--registry",
        ])
        .arg(registry)
        .arg("--worktree")
        .arg(target);
    if confirm {
        command.arg("--confirm-preserved");
    }
    Ok(command.output()?)
}

#[test]
fn launched_worktree_and_headless_owner_confirmation_are_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let main = root.join("main");
    fs::create_dir(&main)?;
    git(&main, &["init", "-q"])?;
    git(
        &main,
        &["config", "--local", "extensions.worktreeConfig", "true"],
    )?;
    git(
        &main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    )?;
    let worker = root.join("worker");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-qb",
            "worker",
            worker.to_str().ok_or("path")?,
        ],
    )?;
    git(
        &worker,
        &[
            "config",
            "--worktree",
            "kitchen.launchBase",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ],
    )?;
    let registry = root.join("registry");
    let from_worker = preserve(&worker, &main, &registry, false)?;
    assert_eq!(
        from_worker.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&from_worker.stderr)
    );
    assert!(String::from_utf8_lossy(&from_worker.stderr).contains("invalid integration input"));
    let headless = preserve(&main, &worker, &registry, true)?;
    assert_eq!(
        headless.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&headless.stderr)
    );
    assert!(String::from_utf8_lossy(&headless.stderr).contains("invalid integration input"));
    Ok(())
}
