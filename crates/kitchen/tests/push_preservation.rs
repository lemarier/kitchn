//! Git observations for checked pushes; no forge or worker is contacted.

use std::{fs, path::Path, process::Command};

use kitchen::workflows::push::{checkout_changes_except_report, checkout_clean_except_report};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn git(path: &Path, args: &[&str]) -> TestResult {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "commit.gpgsign=false"])
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
fn only_committed_ignore_rules_hide_untracked_work() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(
        root.join(".gitignore"),
        "ignored-by-tree\nreports/issue.md\n",
    )?;
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
            "ignore files",
        ],
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
    for name in ["ignored-by-repo", "ignored-by-global"] {
        assert!(changes.iter().any(|change| change == name), "{changes:?}");
    }
    assert!(!changes.iter().any(|change| change == "ignored-by-tree"));
    assert!(!changes.iter().any(|change| change == "reports/issue.md"));
    assert!(!checkout_clean_except_report(
        root,
        Path::new("reports/issue.md")
    )?);
    Ok(())
}

#[test]
fn repository_build_and_skill_caches_are_clean() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), include_str!("../../../.gitignore"))?;
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
            "ignore caches",
        ],
    )?;
    for path in [
        "target/debug/kitchen",
        ".origin89/engineering/versions/snapshot/skills/origin89-working/SKILL.md",
        ".agents/skills/origin89-working/SKILL.md",
        ".claude/skills/origin89-testing/SKILL.md",
    ] {
        let file = root.join(path);
        fs::create_dir_all(file.parent().ok_or("parent")?)?;
        fs::write(file, "cache")?;
    }
    assert_eq!(
        checkout_changes_except_report(root, Path::new("report.md"))?,
        [] as [&str; 0]
    );
    Ok(())
}

#[test]
fn working_tree_ignore_rules_cannot_hide_siblings() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), "target/\n")?;
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
            "ignore build",
        ],
    )?;
    fs::create_dir(root.join("nested"))?;
    fs::write(root.join("nested/.gitignore"), "*\n")?;
    fs::write(root.join("nested/work.txt"), "work")?;
    let changes = checkout_changes_except_report(root, Path::new("report.md"))?;
    assert!(
        changes.contains(&"nested/.gitignore".to_owned()),
        "{changes:?}"
    );
    assert!(
        changes.contains(&"nested/work.txt".to_owned()),
        "{changes:?}"
    );
    fs::write(root.join(".gitignore"), "*\n")?;
    let changes = checkout_changes_except_report(root, Path::new("report.md"))?;
    assert!(changes.contains(&".gitignore".to_owned()), "{changes:?}");
    assert!(
        changes.contains(&"nested/work.txt".to_owned()),
        "{changes:?}"
    );
    Ok(())
}

#[test]
fn committed_nested_ignore_and_negation_apply() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::create_dir(root.join("nested"))?;
    fs::write(root.join("nested/.gitignore"), "*.log\n!important.log\n")?;
    git(root, &["add", "nested/.gitignore"])?;
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "nested ignore",
        ],
    )?;
    fs::write(root.join("nested/cache.log"), "cache")?;
    fs::write(root.join("nested/café.log"), "cache")?;
    fs::write(root.join("nested/line\nbreak.log"), "cache")?;
    assert!(checkout_clean_except_report(root, Path::new("report.md"))?);
    fs::write(root.join("nested/important.log"), "work")?;
    assert_eq!(
        checkout_changes_except_report(root, Path::new("report.md"))?,
        ["nested/important.log"]
    );
    Ok(())
}

#[test]
fn only_the_report_file_is_exempt_in_its_directory() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), "reports/issue.md\n")?;
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

#[test]
fn worktree_config_cannot_redirect_cleanliness_to_another_checkout() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("launched");
    let clean = dir.path().join("clean");
    fs::create_dir(&root)?;
    fs::create_dir(&clean)?;
    git(&root, &["init", "-q"])?;
    git(&clean, &["init", "-q"])?;
    git(&root, &["config", "extensions.worktreeConfig", "true"])?;
    fs::write(root.join("worker-work.txt"), "must count")?;
    git(
        &root,
        &[
            "config",
            "--worktree",
            "core.worktree",
            clean.to_str().ok_or("path")?,
        ],
    )?;
    let result = checkout_changes_except_report(&root, Path::new("report.md"));
    assert!(result.is_err() || result?.iter().any(|path| path == "worker-work.txt"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn report_symlink_outside_checkout_is_work() -> TestResult {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir()?;
    let root = dir.path().join("launched");
    fs::create_dir(&root)?;
    git(&root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), "report.md\n")?;
    git(&root, &["add", ".gitignore"])?;
    git(
        &root,
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
    let outside = dir.path().join("outside.md");
    fs::write(&outside, "external")?;
    symlink(&outside, root.join("report.md"))?;
    assert_eq!(
        checkout_changes_except_report(&root, Path::new("report.md"))?,
        ["report.md"]
    );
    Ok(())
}
