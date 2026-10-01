//! Git observations for checked pushes; no forge or worker is contacted.

use std::{fs, path::Path, process::Command};

use kitchen::workflows::push::{checkout_changes_except_report, checkout_clean_except_report};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn git(path: &Path, args: &[&str]) -> TestResult {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

#[test]
fn report_is_the_only_ignored_status_path() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join("tracked.txt"), "first")?;
    git(root, &["add", "tracked.txt"])?;
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "initial",
        ],
    )?;
    fs::create_dir(root.join("reports"))?;
    fs::write(root.join("reports/issue.md"), "evidence")?;
    let report = Path::new("reports/issue.md");
    assert!(checkout_clean_except_report(root, report)?);
    fs::write(root.join("tracked.txt"), "changed")?;
    assert_eq!(
        checkout_changes_except_report(root, report)?,
        ["tracked.txt"]
    );
    fs::write(root.join("other.txt"), "untracked")?;
    assert_eq!(
        checkout_changes_except_report(root, report)?,
        ["tracked.txt", "other.txt"]
    );
    fs::remove_file(root.join("other.txt"))?;
    git(root, &["checkout", "--", "tracked.txt"])?;
    fs::remove_file(root.join("reports/issue.md"))?;
    git(root, &["mv", "tracked.txt", "reports/issue.md"])?;
    assert!(!checkout_clean_except_report(root, report)?);
    git(root, &["reset", "--hard", "HEAD"])?;
    git(root, &["update-index", "--assume-unchanged", "tracked.txt"])?;
    fs::write(root.join("tracked.txt"), "hidden edit")?;
    assert_eq!(
        checkout_changes_except_report(root, report)?,
        ["1 tracked path(s) hidden from Git status"]
    );
    Ok(())
}

#[test]
fn invalid_checkout_is_not_reported_clean() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(checkout_clean_except_report(dir.path(), Path::new("report.md")).is_err());
    Ok(())
}

#[test]
fn every_git_exclude_source_still_counts_untracked_work() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(
        root.join(".gitignore"),
        "ignored-by-tree\nreports/issue.md\n",
    )?;
    fs::write(root.join(".git/info/exclude"), "ignored-by-repo\n")?;
    let global = root.join("global-excludes");
    fs::write(&global, "ignored-by-global\n")?;
    git(
        root,
        &[
            "config",
            "core.excludesFile",
            global.to_str().ok_or("path")?,
        ],
    )?;
    fs::write(root.join("ignored-by-tree"), "work")?;
    fs::write(root.join("ignored-by-repo"), "work")?;
    fs::write(root.join("ignored-by-global"), "work")?;
    fs::create_dir(root.join("reports"))?;
    fs::write(root.join("reports/issue.md"), "report")?;
    let changes = checkout_changes_except_report(root, Path::new("reports/issue.md"))?;
    for name in ["ignored-by-tree", "ignored-by-repo", "ignored-by-global"] {
        assert!(changes.iter().any(|change| change == name), "{changes:?}");
    }
    assert!(!changes.iter().any(|change| change == "reports/issue.md"));
    assert!(!checkout_clean_except_report(
        root,
        Path::new("reports/issue.md")
    )?);
    Ok(())
}

#[test]
fn ignored_report_directory_is_clean_only_when_it_contains_the_report() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), "reports/\n")?;
    git(root, &["add", ".gitignore"])?;
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "ignore reports",
        ],
    )?;
    fs::create_dir(root.join("reports"))?;
    fs::write(root.join("reports/issue.md"), "report")?;
    let report = Path::new("reports/issue.md");
    assert!(checkout_clean_except_report(root, report)?);
    fs::write(root.join("reports/other.md"), "work")?;
    assert!(!checkout_clean_except_report(root, report)?);
    Ok(())
}
