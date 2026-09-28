//! Process-level checks for the bootstrap command surface.

use std::error::Error;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn spawn_guard() -> MutexGuard<'static, ()> {
    SPAWN_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

#[test]
fn help_and_default_invocation_explain_the_available_surface() -> Result<(), Box<dyn Error>> {
    let _spawn_guard = spawn_guard();
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
    let _spawn_guard = spawn_guard();
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
    let _spawn_guard = spawn_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .arg("pickup")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("unrecognized subcommand"));
    Ok(())
}

#[test]
fn unknown_flags_are_rejected() -> Result<(), Box<dyn Error>> {
    let _spawn_guard = spawn_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
        .arg("--unknown")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("unexpected argument"));
    Ok(())
}

#[test]
fn identifiers_are_validated_by_the_library() -> Result<(), Box<dyn Error>> {
    let _spawn_guard = spawn_guard();
    for command in ["validate-house", "validate-task"] {
        for id in [
            "a".to_owned(),
            "A".repeat(64),
            "42".to_owned(),
            "9-task".to_owned(),
            "a-".to_owned(),
            "a_".to_owned(),
            "a--b".to_owned(),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
                .args([command, &id])
                .output()?;
            assert_eq!(output.status.code(), Some(0));
            assert_eq!(String::from_utf8(output.stdout)?, format!("{id}\n"));
            assert!(output.stderr.is_empty());
        }
        for (id, diagnostic) in [
            (String::new(), "1 to 64 bytes"),
            ("a".repeat(65), "1 to 64 bytes"),
            ("private/secret".to_owned(), "ASCII"),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
                .args([command, &id])
                .output()?;
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr)?;
            assert!(stderr.contains(diagnostic));
            assert!(!stderr.contains("private/secret"));
        }
        let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
            .arg(command)
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.contains("required arguments"));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn closed_output_is_a_failure() -> Result<(), Box<dyn Error>> {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    // Another test must not fork while the socket reader is open, before we drop it.
    let _spawn_guard = spawn_guard();

    for args in [
        vec![],
        vec!["--help"],
        vec!["--version"],
        vec!["validate-house", "--help"],
        vec!["validate-house", "home"],
    ] {
        let (writer, reader) = UnixStream::pair()?;
        drop(reader);
        let fd: OwnedFd = writer.into();
        let output = Command::new(env!("CARGO_BIN_EXE_kitchen"))
            .args(args)
            .stdout(Stdio::from(fd))
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("failed to write command output"));
        assert!(stderr.contains("BrokenPipe"));
    }
    Ok(())
}
