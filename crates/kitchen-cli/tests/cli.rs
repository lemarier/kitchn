//! Process-level checks for the bootstrap command surface.

use std::error::Error;
use std::process::Command;

#[test]
fn help_and_default_invocation_explain_the_available_surface() -> Result<(), Box<dyn Error>> {
    for args in [vec![], vec!["--help"], vec!["-h"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
            .args(args)
            .output()?;
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        assert!(stdout.contains("Usage: kitchen"));
        assert!(stdout.contains("not implemented yet"));
        assert!(output.stderr.is_empty());
    }
    Ok(())
}

#[test]
fn version_is_machine_readable() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .arg("--version")
        .output()?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!("kitchen {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn unknown_commands_fail_without_claiming_execution() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .arg("pickup")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("unexpected argument"));
    Ok(())
}

#[test]
fn unknown_flags_are_rejected() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .arg("--unknown")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("unexpected argument"));
    Ok(())
}
