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
    assert_eq!(output.status.code(), Some(1)); // Unknown access must remain a gap.
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("House (crabnebula):"));
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
        assert_eq!(output.status.code(), Some(1));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["access"], "unobserved");
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
