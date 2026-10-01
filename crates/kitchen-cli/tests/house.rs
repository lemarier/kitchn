//! Guided adoption and diagnostics exercised through the real CLI in disposable
//! roots. Bindings live in the registry; working trees must stay unchanged.
use kitchen::{
    adoption::{HouseRegistry, InstructionBundle},
    house::HouseConfig,
};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const BINDING: &str = "repositories/crabnebula/tauri-fixture.json";

fn initialize(root: &Path) -> TestResult<HouseRegistry> {
    let registry = HouseRegistry::new(root.join("registry"))?;
    let house: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula.json"
    ))?;
    let bundle: InstructionBundle = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/crabnebula-bundle.json"
    ))?;
    registry.initialize(&house)?;
    registry.sync(&house.house, &bundle)?;
    Ok(registry)
}
/// Register a second house that also claims the crabnebula fixture repository.
fn second_claimant(registry: &HouseRegistry) -> TestResult {
    let mut house: HouseConfig = serde_json::from_str(include_str!(
        "../../kitchen/tests/fixtures/house/origin89.json"
    ))?;
    house
        .repositories
        .insert("crabnebula/tauri-fixture".parse()?);
    registry.initialize(&house)?;
    Ok(())
}
fn git(path: &Path, args: &[&str]) -> TestResult<String> {
    // Fixture setup ignores the person's Git configuration, such as signing.
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Kitchen Test",
            "-c",
            "user.email=test@example.com",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}
/// A checkout with one commit whose `origin` names `url`.
fn checkout(path: &Path, url: &str) -> TestResult {
    fs::create_dir_all(path)?;
    git(path, &["init", "--quiet"])?;
    git(path, &["commit", "--quiet", "--allow-empty", "-m", "init"])?;
    git(path, &["remote", "add", "origin", url])?;
    Ok(())
}
/// Every path Git sees in the working tree, including ignored ones.
fn tree_status(path: &Path) -> TestResult<String> {
    git(
        path,
        &[
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
        ],
    )
}
fn kitchen(current_dir: &Path, registry: &HouseRegistry, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kitchn"));
    command
        .current_dir(current_dir)
        .args(args.iter().take(2))
        .arg("--registry")
        .arg(registry.root())
        .args(args.iter().skip(2));
    command
}
fn setup(current_dir: &Path, registry: &HouseRegistry, extra: &[&str]) -> TestResult<Output> {
    let mut args = vec![
        "house",
        "setup",
        "--house",
        "crabnebula",
        "--workflows",
        "none",
    ];
    args.extend_from_slice(extra);
    Ok(kitchen(current_dir, registry, &args).output()?)
}
fn json(output: &Output) -> TestResult<serde_json::Value> {
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn guided_setup_asks_two_choices_and_leaves_the_tree_unchanged() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    fs::write(registry.root().join("houses/other.pending"), "interrupted")?;
    fs::write(registry.root().join("houses/.DS_Store"), "stray")?;
    fs::write(registry.root().join("houses/broken.json"), "damaged")?;
    let consumer = root.join("consumer");
    checkout(&consumer, "git@github.com:crabnebula/tauri-fixture.git")?;
    fs::write(consumer.join("AGENTS.md"), "local instructions")?;
    let before = tree_status(&consumer)?;
    let mut child = kitchen(&consumer, &registry, &["house", "setup"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin unavailable")?
        .write_all(b"crabnebula\nnone\n")?;
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(0)); // Adoption succeeded; doctor still reports unknown access.
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("House (crabnebula):"));
    assert!(stderr.contains("House broken unavailable:"));
    assert!(stderr.contains("Workflows ("));
    assert!(stdout.contains("Bound crabnebula/tauri-fixture to house crabnebula"));
    assert!(stdout.contains("Doctor: setup incomplete"));
    assert!(stdout.contains("Next: Configure crabnebula access"));
    assert!(stdout.contains("Pinned instructions:"));
    assert!(!stdout.contains("Origin89"));
    assert_eq!(tree_status(&consumer)?, before);
    assert_eq!(
        fs::read_to_string(consumer.join("AGENTS.md"))?,
        "local instructions"
    );
    assert!(!consumer.join(".kitchen.json").exists());
    let binding = fs::read_to_string(registry.root().join(BINDING))?;
    assert!(binding.contains("\"schema\": 2"));
    assert!(!binding.contains("grants"));
    assert!(!registry.root().join("private").exists());
    Ok(())
}

#[test]
fn owner_previews_and_enables_worktree_config_for_the_named_checkout() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "https://github.com/crabnebula/tauri-fixture")?;
    let config = consumer.join(".git/config");
    let before = fs::read(&config)?;
    let preview = setup(
        &consumer,
        &registry,
        &["--enable-worktree-config", "--preview", "--json"],
    )?;
    assert_eq!(preview.status.code(), Some(0));
    assert_eq!(fs::read(&config)?, before);
    assert!(json(&preview)?["git_config_change"].as_str().is_some());
    let preview_text = String::from_utf8(preview.stdout)?;
    assert!(preview_text.contains("extensions.worktreeConfig=true"));
    assert!(preview_text.contains("crabnebula/tauri-fixture"));
    let applied = setup(
        &consumer,
        &registry,
        &["--enable-worktree-config", "--json"],
    )?;
    assert_eq!(applied.status.code(), Some(0));
    assert_eq!(
        git(
            &consumer,
            &[
                "config",
                "--local",
                "--bool",
                "--get",
                "extensions.worktreeConfig"
            ]
        )?
        .trim(),
        "true"
    );
    assert_ne!(fs::read(&config)?, before);
    Ok(())
}

#[test]
fn preview_is_read_only_and_json_doctor_does_not_claim_live_access() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "https://github.com/crabnebula/tauri-fixture")?;
    for preview in [true, false] {
        let mut args = vec![
            "house",
            "setup",
            "--house",
            "crabnebula",
            "--workflows",
            "pickup,gate",
            "--json",
        ];
        if preview {
            args.push("--preview");
        }
        let output = kitchen(&consumer, &registry, &args).output()?;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let report = json(&output)?;
        assert_eq!(report["access"], "unobserved");
        assert_eq!(report["preview"], preview);
        assert_eq!(report["binding"], "created");
        assert_eq!(report["written"], !preview);
        assert_eq!(report["labels"].as_array().ok_or("labels absent")?.len(), 5);
        assert_eq!(registry.root().join(BINDING).exists(), !preview);
    }
    let output = kitchen(&consumer, &registry, &["house", "doctor", "--json"]).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(json(&output)?["instructions"].is_object());
    assert_eq!(tree_status(&consumer)?, "");
    Ok(())
}

#[test]
fn invalid_secret_input_and_missing_house_fail_without_echo_or_writes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let source = root.join("invalid.json");
    fs::write(&source, br#"{"token":"SECRET-MUST-NOT-APPEAR"}"#)?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .args(["house", "init", "--registry"])
        .arg(root.join("registry"))
        .arg("--config")
        .arg(&source)
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8(output.stderr)?.contains("SECRET-MUST-NOT-APPEAR"));
    assert!(!root.join("registry").exists());
    let registry = initialize(&root)?;
    let output = kitchen(
        &root,
        &registry,
        &[
            "house",
            "setup",
            "--repository",
            "crabnebula/tauri-fixture",
            "--house",
            "unknown",
            "--workflows",
            "none",
        ],
    )
    .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(!registry.root().join("repositories").exists());
    Ok(())
}

#[test]
fn relative_registry_and_repository_path_are_resolved() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    initialize(&root)?;
    checkout(
        &root.join("consumer"),
        "https://github.com/crabnebula/tauri-fixture.git",
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
        .current_dir(&root)
        .args([
            "house",
            "setup",
            "--registry",
            "registry",
            "--repository-path",
            "consumer",
            "--house",
            "crabnebula",
            "--workflows",
            "none",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(root.join("registry").join(BINDING).is_file());
    assert!(String::from_utf8(output.stdout)?.contains("Doctor: setup incomplete"));
    assert_eq!(tree_status(&root.join("consumer"))?, "");
    Ok(())
}

#[test]
fn subdirectories_and_other_worktrees_resolve_the_same_house() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "https://github.com/crabnebula/tauri-fixture.git")?;
    fs::create_dir_all(consumer.join("src/deep"))?;
    let second = root.join("second");
    git(
        &consumer,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "other",
            second.to_str().ok_or("path")?,
        ],
    )?;
    let output = setup(&consumer.join("src/deep"), &registry, &[])?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    for start in [consumer.join("src/deep"), second.clone()] {
        let output = kitchen(&start, &registry, &["house", "doctor", "--json"]).output()?;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(json(&output)?["house"], "crabnebula");
        assert_eq!(json(&output)?["repository"], "crabnebula/tauri-fixture");
    }
    assert_eq!(tree_status(&consumer)?, "");
    assert_eq!(tree_status(&second)?, "");
    Ok(())
}

#[test]
fn two_claiming_houses_require_one_stored_choice() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    second_claimant(&registry)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "https://github.com/crabnebula/tauri-fixture.git")?;
    let output = kitchen(&consumer, &registry, &["house", "doctor"]).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)?.contains("crabnebula, origin89"));
    let mut child = kitchen(
        &consumer,
        &registry,
        &["house", "setup", "--workflows", "none"],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin unavailable")?
        .write_all(b"origin89\n")?;
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(String::from_utf8(output.stderr)?.contains("House (crabnebula, origin89):"));
    let output = kitchen(&consumer, &registry, &["house", "doctor", "--json"]).output()?;
    assert_eq!(json(&output)?["house"], "origin89");
    // The stored choice is not silently replaced by another house.
    let before = fs::read(registry.root().join(BINDING))?;
    for preview in [true, false] {
        let mut extra = vec!["--json"];
        if preview {
            extra.push("--preview");
        }
        let output = setup(&consumer, &registry, &extra)?;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(json(&output)?["binding"], "refused");
        assert_eq!(json(&output)?["written"], false);
        assert_eq!(fs::read(registry.root().join(BINDING))?, before);
    }
    assert_eq!(tree_status(&consumer)?, "");
    Ok(())
}

#[test]
fn unidentified_and_unclaimed_checkouts_are_refused_without_writes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let plain = root.join("plain");
    fs::create_dir(&plain)?;
    let output = setup(&plain, &registry, &[])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)?.contains("no Git remote"));
    let other = root.join("other");
    checkout(&other, "https://gitlab.com/crabnebula/tauri-fixture.git")?;
    assert_eq!(setup(&other, &registry, &[])?.status.code(), Some(1));
    let unclaimed = root.join("unclaimed");
    checkout(&unclaimed, "https://github.com/someone/else.git")?;
    assert_eq!(setup(&unclaimed, &registry, &[])?.status.code(), Some(1));
    assert!(!registry.root().join("repositories").exists());
    assert!(!plain.join(".kitchen.json").exists());
    // An explicit repository needs no checkout; still nothing lands in the tree.
    let output = setup(
        &plain,
        &registry,
        &["--repository", "crabnebula/tauri-fixture"],
    )?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(registry.root().join(BINDING).is_file());
    assert_eq!(fs::read_dir(&plain)?.count(), 0);
    Ok(())
}

#[test]
fn setup_json_distinguishes_unchanged_and_updated_without_preview_writes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "https://github.com/crabnebula/tauri-fixture.git")?;
    setup(&consumer, &registry, &[])?;
    let path = registry.root().join(BINDING);
    let original = fs::read(&path)?;
    let output = setup(&consumer, &registry, &["--json"])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json(&output)?["binding"], "unchanged");
    assert_eq!(json(&output)?["written"], false);
    let args = ["house", "setup", "--workflows", "pickup", "--json"];
    let output = kitchen(&consumer, &registry, &args)
        .arg("--preview")
        .output()?;
    assert_eq!(json(&output)?["binding"], "updated");
    assert_eq!(json(&output)?["written"], false);
    assert_eq!(fs::read(&path)?, original);
    let output = kitchen(&consumer, &registry, &args).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json(&output)?["binding"], "updated");
    assert_eq!(json(&output)?["written"], true);
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    assert_eq!(stored["workflows"], serde_json::json!(["pickup"]));
    assert_eq!(tree_status(&consumer)?, "");
    Ok(())
}

#[test]
fn doctor_reports_unbound_repositories_and_legacy_files() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    checkout(&consumer, "https://github.com/crabnebula/tauri-fixture.git")?;
    let output = kitchen(&consumer, &registry, &["house", "doctor"]).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stdout)?.contains("not set up"));
    let legacy = serde_json::json!({
        "schema": 1,
        "house": "crabnebula",
        "repository": "crabnebula/tauri-fixture",
        "workflows": ["pickup"],
        "additionalReviewers": [],
        "additionalChecks": ["local-check"],
    });
    let path = consumer.join(".kitchen.json");
    let bytes = serde_json::to_vec(&legacy)?;
    fs::write(&path, &bytes)?;
    let before = tree_status(&consumer)?;
    let output = kitchen(&consumer, &registry, &["house", "doctor"]).output()?;
    assert!(String::from_utf8(output.stdout)?.contains("Next: kitchn house import"));
    // Preview by default: nothing is stored, and everything to be stored shows.
    let output = kitchen(&consumer, &registry, &["house", "import"]).output()?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let preview = String::from_utf8(output.stdout)?;
    for shown in [
        "Would import",
        "crabnebula/tauri-fixture",
        "house crabnebula",
        "workflows: pickup",
        "additional reviewers: none",
        "additional checks: local-check",
        "--yes --digest ",
    ] {
        assert!(preview.contains(shown), "{shown}: {preview}");
    }
    assert!(!registry.root().join(BINDING).exists());
    let digest = preview
        .split("--digest ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or("no digest in the preview")?
        .to_owned();
    // Consent without the previewed digest, or with another one, applies nothing.
    let output = kitchen(&consumer, &registry, &["house", "import", "--yes"]).output()?;
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let output = kitchen(
        &consumer,
        &registry,
        &["house", "import", "--yes", "--digest", &"0".repeat(64)],
    )
    .output()?;
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(!registry.root().join(BINDING).exists());
    // The file changes after the preview: refused, nothing stored.
    let mut changed = legacy.clone();
    changed["additionalChecks"] = serde_json::json!(["local-check", "extra-check"]);
    fs::write(&path, serde_json::to_vec(&changed)?)?;
    let output = kitchen(
        &consumer,
        &registry,
        &["house", "import", "--yes", "--digest", &digest],
    )
    .output()?;
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(String::from_utf8(output.stderr)?.contains("preview"));
    assert!(!registry.root().join(BINDING).exists());
    fs::write(&path, &bytes)?;
    let output = kitchen(
        &consumer,
        &registry,
        &["house", "import", "--yes", "--digest", &digest, "--json"],
    )
    .output()?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(json(&output)?["status"], "created");
    assert_eq!(json(&output)?["binding"]["schema"], 2);
    assert_eq!(json(&output)?["digest"], digest);
    let output = kitchen(&consumer, &registry, &["house", "doctor", "--json"]).output()?;
    assert_eq!(output.status.code(), Some(1));
    let report = json(&output)?;
    assert!(
        report["findings"]
            .as_array()
            .ok_or("findings absent")?
            .iter()
            .any(|finding| finding["code"] == "legacy-binding")
    );
    assert_eq!(report["repository"], "crabnebula/tauri-fixture");
    // Kitchen never deletes or rewrites the file.
    assert_eq!(fs::read(&path)?, bytes);
    assert_eq!(tree_status(&consumer)?, before);
    Ok(())
}
