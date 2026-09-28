//! Guided adoption and diagnostics exercised through the real CLI in disposable roots.
use kitchen::{
    adoption::{HouseRegistry, InstructionBundle},
    house::HouseConfig,
};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn initialize(root: &Path) -> Result<HouseRegistry, Box<dyn std::error::Error>> {
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
#[test]
fn guided_setup_asks_two_choices_and_ends_with_exact_next_steps() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    fs::write(registry.root().join("houses/other.pending"), "interrupted")?;
    fs::write(registry.root().join("houses/.DS_Store"), "stray")?;
    fs::write(registry.root().join("houses/broken.json"), "damaged")?;
    let consumer = root.join("consumer");
    fs::create_dir(&consumer)?;
    fs::write(consumer.join("AGENTS.md"), "local instructions")?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args(["house", "setup", "--registry"])
        .arg(registry.root())
        .arg("--repository-path")
        .arg(&consumer)
        .args(["--repository", "crabnebula/tauri-fixture"])
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
    assert!(stdout.contains("Doctor: setup incomplete"));
    assert!(stdout.contains("Next: Configure crabnebula access"));
    assert!(stdout.contains("Pinned instructions:"));
    assert!(!stdout.contains("Origin89"));
    assert_eq!(
        fs::read_to_string(consumer.join("AGENTS.md"))?,
        "local instructions"
    );
    let binding = fs::read_to_string(consumer.join(".kitchen.json"))?;
    assert!(!binding.contains("grants"));
    assert!(!registry.root().join("private").exists());
    Ok(())
}
#[test]
fn preview_is_read_only_and_json_doctor_does_not_claim_live_access() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    fs::create_dir(&consumer)?;
    for preview in [true, false] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kitchen"));
        command
            .args(["house", "setup", "--registry"])
            .arg(registry.root())
            .arg("--repository-path")
            .arg(&consumer)
            .args([
                "--repository",
                "crabnebula/tauri-fixture",
                "--house",
                "crabnebula",
                "--workflows",
                "pickup,gate",
                "--json",
            ]);
        if preview {
            command.arg("--preview");
        }
        let output = command.output()?;
        assert_eq!(output.status.code(), Some(0));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["access"], "unobserved");
        assert_eq!(report["preview"], preview);
        assert_eq!(report["binding"], "created");
        assert_eq!(report["written"], !preview);
        assert_eq!(report["labels"].as_array().ok_or("labels absent")?.len(), 5);
        if preview {
            assert!(!consumer.join(".kitchen.json").exists());
        } else {
            assert!(consumer.join(".kitchen.json").exists());
        }
    }
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args(["house", "doctor", "--registry"])
        .arg(registry.root())
        .arg("--repository-path")
        .arg(&consumer)
        .arg("--json")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)?["instructions"].is_object()
    );
    Ok(())
}
#[test]
fn invalid_secret_input_and_missing_house_fail_without_echo_or_writes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let source = root.join("invalid.json");
    fs::write(&source, br#"{"token":"SECRET-MUST-NOT-APPEAR"}"#)?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args(["house", "init", "--registry"])
        .arg(root.join("registry"))
        .arg("--config")
        .arg(&source)
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8(output.stderr)?.contains("SECRET-MUST-NOT-APPEAR"));
    assert!(!root.join("registry").exists());
    let registry = initialize(&root)?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .args(["house", "setup", "--registry"])
        .arg(registry.root())
        .arg("--repository-path")
        .arg(root.join("consumer"))
        .args([
            "--repository",
            "crabnebula/tauri-fixture",
            "--house",
            "unknown",
            "--workflows",
            "none",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(!root.join("consumer").exists());
    Ok(())
}

#[test]
fn relative_registry_is_resolved_without_erasing_redirects() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    initialize(&root)?;
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .current_dir(&root)
        .args([
            "house",
            "setup",
            "--registry",
            "registry",
            "--repository-path",
            "consumer",
            "--repository",
            "crabnebula/tauri-fixture",
            "--house",
            "crabnebula",
            "--workflows",
            "none",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(root.join("consumer/.kitchen.json").is_file());
    assert!(String::from_utf8(output.stdout)?.contains("Doctor: setup incomplete"));
    Ok(())
}

fn setup_command(root: &Path, registry: &HouseRegistry) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kitchen"));
    command
        .current_dir(root)
        .args(["house", "setup", "--registry"])
        .arg(registry.root())
        .args([
            "--repository",
            "crabnebula/tauri-fixture",
            "--house",
            "crabnebula",
            "--workflows",
            "none",
        ]);
    command
}

#[test]
fn setup_and_doctor_find_the_git_root_from_subdirectories() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join("src/deep"))?;
    fs::create_dir(consumer.join(".git"))?;
    let output = setup_command(&consumer.join("src/deep"), &registry).output()?;
    assert!(consumer.join(".kitchen.json").is_file(), "{output:?}");
    assert!(!consumer.join("src/deep/.kitchen.json").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .current_dir(consumer.join("src/deep"))
        .args(["house", "doctor", "--registry"])
        .arg(registry.root())
        .arg("--json")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout)?["house"],
        "crabnebula"
    );
    Ok(())
}

#[test]
fn preview_refuses_rebinding_without_promising_adoption() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join(".git"))?;
    setup_command(&consumer, &registry).output()?;
    let path = consumer.join(".kitchen.json");
    let mut binding: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    binding["house"] = serde_json::json!("other-house");
    fs::write(&path, serde_json::to_vec(&binding)?)?;
    let before = fs::read(&path)?;
    let output = setup_command(&consumer, &registry)
        .arg("--preview")
        .output()?;
    assert!(!String::from_utf8(output.stdout)?.contains("Would adopt"));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read(path)?, before);
    Ok(())
}

#[test]
fn implicit_setup_requires_git_but_explicit_uninitialized_directory_is_allowed() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let output = setup_command(&root, &registry).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(!root.join(".kitchen.json").exists());
    let output = setup_command(&root, &registry)
        .args(["--repository-path", "consumer"])
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(root.join("consumer/.kitchen.json").is_file());
    Ok(())
}

#[test]
fn setup_json_distinguishes_unchanged_updated_and_refused_without_preview_writes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join(".git"))?;
    setup_command(&consumer, &registry).output()?;
    let path = consumer.join(".kitchen.json");
    let original = fs::read(&path)?;
    let output = setup_command(&consumer, &registry).arg("--json").output()?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(report["binding"], "unchanged");
    assert_eq!(report["written"], false);
    let mut binding: serde_json::Value = serde_json::from_slice(&original)?;
    binding["workflows"] = serde_json::json!(["pickup"]);
    fs::write(&path, serde_json::to_vec(&binding)?)?;
    let before = fs::read(&path)?;
    let output = setup_command(&consumer, &registry)
        .args(["--json", "--preview"])
        .output()?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["binding"], "updated");
    assert_eq!(report["written"], false);
    assert_eq!(fs::read(&path)?, before);
    let output = setup_command(&consumer, &registry).arg("--json").output()?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["binding"], "updated");
    assert_eq!(report["written"], true);
    assert_eq!(fs::read(&path)?, original);
    binding["house"] = serde_json::json!("different");
    fs::write(&path, serde_json::to_vec(&binding)?)?;
    let before = fs::read(&path)?;
    for preview in [true, false] {
        let mut command = setup_command(&consumer, &registry);
        command.arg("--json");
        if preview {
            command.arg("--preview");
        }
        let output = command.output()?;
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(report["binding"], "refused");
        assert_eq!(report["written"], false);
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn setup_respects_nested_worktree_git_file() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let registry = initialize(&root)?;
    let parent = root.join("parent");
    fs::create_dir_all(parent.join(".git"))?;
    let child = parent.join("child");
    fs::create_dir_all(child.join("src"))?;
    fs::write(child.join(".git"), "gitdir: /external/worktree-metadata")?;
    let output = setup_command(&child.join("src"), &registry).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(child.join(".kitchen.json").exists());
    assert!(!parent.join(".kitchen.json").exists());
    assert!(!child.join("src/.kitchen.json").exists());
    Ok(())
}
