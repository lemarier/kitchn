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

#[test]
fn a_directory_destination_is_refused_and_left_unchanged() -> TestResult {
    let dir = tempfile::tempdir()?;
    let target = dir.path().join("tool");
    fs::create_dir(&target)?;
    let error = write_executable(&target, "#!/bin/sh\n")
        .err()
        .ok_or("a directory destination was accepted")?;
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(fs::read_dir(&target)?.count(), 0);
    assert_eq!(fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

/// What one write-then-run round did under fork pressure.
enum Round {
    Started,
    /// The Linux race: the script was busy because a forked child held the
    /// write descriptor. This is the only outcome that fails the test.
    Busy,
    /// A write or spawn failed for another reason, such as `fork` or `cp`
    /// running out of resources on a loaded host. Not the regression.
    Other(String),
}

fn round(path: &std::path::Path) -> Round {
    if let Err(e) = write_executable(path, "#!/bin/sh\nexit 0\n") {
        return Round::Other(format!("write {:?}: {e}", e.kind()));
    }
    match Command::new(path).stdin(Stdio::null()).status() {
        Ok(_) => Round::Started,
        Err(e) if e.kind() == ErrorKind::ExecutableFileBusy => Round::Busy,
        Err(e) => Round::Other(format!("spawn {:?}: {e}", e.kind())),
    }
}

/// Regression for the Linux `ETXTBSY` race: forking threads must not make a
/// script fail with `ETXTBSY` right after it is written. The pre-fix pattern
/// (`fs::write` then run) fails this on Linux. Only `ExecutableFileBusy`
/// counts as the regression; other write or spawn errors come from resource
/// exhaustion on a loaded host, so they are printed with their kind and
/// tolerated as long as some scripts still started.
#[test]
fn scripts_start_while_other_threads_fork_children() -> TestResult {
    const WRITERS: usize = 4;
    const ROUNDS: usize = 25;
    let stop = AtomicBool::new(false);
    let dir = tempfile::tempdir()?;
    let rounds: Vec<Round> = thread::scope(|scope| {
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
                        .map(|n| round(&dir.join(format!("script-{writer}-{n}"))))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let rounds = writers
            .into_iter()
            .flat_map(|h| {
                h.join()
                    .unwrap_or_else(|_| vec![Round::Other("writer thread panicked".into())])
            })
            .collect();
        stop.store(true, Ordering::Relaxed);
        for forker in forkers {
            let _ = forker.join();
        }
        rounds
    });
    let busy = rounds.iter().filter(|r| matches!(r, Round::Busy)).count();
    let started = rounds
        .iter()
        .filter(|r| matches!(r, Round::Started))
        .count();
    for other in rounds.iter().filter_map(|r| match r {
        Round::Other(why) => Some(why),
        Round::Started | Round::Busy => None,
    }) {
        eprintln!("not the ETXTBSY regression (resource failure?): {other}");
    }
    assert_eq!(busy, 0, "ETXTBSY after write: the Linux race is back");
    assert!(
        started > 0,
        "no script started; the host cannot run this test"
    );
    Ok(())
}
