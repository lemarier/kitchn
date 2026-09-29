//! The fake-executable helper: freshly written scripts must run at once, even
//! while other threads keep forking children.
#![cfg(unix)]

mod common;

use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

use common::{TestResult, executable::write_executable};

fn run(path: &std::path::Path) -> std::io::Result<String> {
    let output = Command::new(path).stdin(Stdio::null()).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn a_written_script_runs_immediately_with_owner_only_mode() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tool");
    write_executable(&path, "#!/bin/sh\nprintf ran\n")?;
    assert_eq!(run(&path)?, "ran");
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o700);
    Ok(())
}

#[test]
fn rewriting_replaces_the_contents_and_leaves_no_staging_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tool");
    write_executable(&path, "#!/bin/sh\nprintf first\n")?;
    write_executable(&path, "#!/bin/sh\nprintf second\n")?;
    assert_eq!(run(&path)?, "second");
    let names: Vec<_> = fs::read_dir(dir.path())?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<Result<_, _>>()?;
    assert_eq!(names, vec![std::ffi::OsString::from("tool")]);
    Ok(())
}

#[test]
fn an_empty_script_body_is_still_written() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("empty");
    write_executable(&path, "")?;
    assert_eq!(fs::metadata(&path)?.len(), 0);
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o700);
    Ok(())
}

#[test]
fn a_missing_directory_or_file_name_is_an_error_and_leaves_nothing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("absent").join("tool");
    assert!(write_executable(&missing, "#!/bin/sh\n").is_err());
    assert!(write_executable(std::path::Path::new("/"), "#!/bin/sh\n").is_err());
    assert_eq!(fs::read_dir(dir.path())?.count(), 0);
    Ok(())
}

/// Regression for the Linux `ETXTBSY` race: forking threads must not make a
/// script fail with `ETXTBSY` right after it is written. The pre-fix pattern
/// (`fs::write` then run) fails this on Linux.
#[test]
fn scripts_start_while_other_threads_fork_children() -> TestResult {
    const WRITERS: usize = 4;
    const ROUNDS: usize = 25;
    let stop = AtomicBool::new(false);
    let dir = tempfile::tempdir()?;
    let failures = thread::scope(|scope| {
        let forkers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("/bin/true").status();
                    }
                })
            })
            .collect();
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let dir = dir.path();
                scope.spawn(move || {
                    (0..ROUNDS)
                        .filter(|round| {
                            let path = dir.join(format!("script-{writer}-{round}"));
                            write_executable(&path, "#!/bin/sh\nexit 0\n").is_err()
                                || matches!(
                                    Command::new(&path).stdin(Stdio::null()).status(),
                                    Err(e) if e.kind() == ErrorKind::ExecutableFileBusy
                                )
                        })
                        .count()
                })
            })
            .collect();
        let failures: usize = writers
            .into_iter()
            .map(|h| h.join().unwrap_or(usize::MAX))
            .sum();
        stop.store(true, Ordering::Relaxed);
        for forker in forkers {
            let _ = forker.join();
        }
        failures
    });
    assert_eq!(failures, 0);
    Ok(())
}
