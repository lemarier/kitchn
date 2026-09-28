//! Process-level checks for the offline pickup diagnostics.

use std::error::Error;
use std::process::{Command, Output};

fn kitchen(args: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args(args)
        .output()
}

#[test]
fn task_id_is_the_shared_claim_identity() -> Result<(), Box<dyn Error>> {
    let output = kitchen(&["pickup", "task-id", "lemarier/kitchen", "8"])?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "issue-1252879d18666e31-8\n"
    );
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn task_id_rejects_invalid_repository_and_zero_issue() -> Result<(), Box<dyn Error>> {
    for args in [
        ["pickup", "task-id", "not-a-repository", "8"],
        ["pickup", "task-id", "lemarier/kitchen", "0"],
    ] {
        let output = kitchen(&args)?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.starts_with("error: "));
    }
    Ok(())
}

#[test]
fn check_branch_accepts_exact_names_and_rejects_invalid_ones() -> Result<(), Box<dyn Error>> {
    let output = kitchen(&["pickup", "check-branch", "lemarier/pickup-coordination"])?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "lemarier/pickup-coordination\n"
    );
    for name in [
        "lemarier/../x",
        "lemarier/x.lock",
        "-x",
        "a b",
        "a$(id)",
        "a`id`",
        "a;b",
    ] {
        let output = kitchen(&["pickup", "check-branch", name])?;
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert_eq!(stderr, "error: invalid branch name\n");
        assert!(!stderr.contains(name));
    }
    Ok(())
}
