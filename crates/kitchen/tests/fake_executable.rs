//! The fake-executable helper: freshly written scripts must run at once, even
//! while other threads keep forking children.
#![cfg(unix)]

mod common;

use std::{
    fs,
    io::ErrorKind,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    process::{Command, ExitStatus, Stdio},
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
#[derive(Debug, PartialEq, Eq)]
enum Round {
    /// The script launched and exited successfully.
    Succeeded,
    /// The script launched but did not exit successfully, so it proves nothing
    /// about the race.
    Unsuccessful(String),
    /// The Linux race: the script was busy because a forked child held the
    /// write descriptor. This fails the test.
    Busy,
    /// A write or spawn failed for another reason, such as `fork` or `cp`
    /// running out of resources on a loaded host. Not the regression.
    Other(String),
}

fn classify(spawned: std::io::Result<ExitStatus>) -> Round {
    match spawned {
        Ok(status) if status.success() => Round::Succeeded,
        Ok(status) => Round::Unsuccessful(format!("exit {status}")),
        Err(e) if e.kind() == ErrorKind::ExecutableFileBusy => Round::Busy,
        Err(e) => Round::Other(format!("spawn {:?}: {e}", e.kind())),
    }
}

fn round(path: &std::path::Path) -> Round {
    if let Err(e) = write_executable(path, "#!/bin/sh\nexit 0\n") {
        return Round::Other(format!("write {:?}: {e}", e.kind()));
    }
    classify(Command::new(path).stdin(Stdio::null()).status())
}

/// Judge every writer's rounds. `None` is a writer thread that panicked.
/// Passes only when each writer finished all `rounds_per_writer` rounds, none
/// hit `ETXTBSY`, and at least one script ran to a successful exit.
fn judge(writers: &[Option<Vec<Round>>], rounds_per_writer: usize) -> Result<(), String> {
    let mut succeeded = 0;
    for (writer, rounds) in writers.iter().enumerate() {
        let Some(rounds) = rounds else {
            return Err(format!("writer {writer} panicked"));
        };
        if rounds.len() != rounds_per_writer {
            return Err(format!(
                "writer {writer} ran {} of {rounds_per_writer} rounds",
                rounds.len()
            ));
        }
        for round in rounds {
            match round {
                Round::Busy => return Err("ETXTBSY after write: the Linux race is back".into()),
                Round::Succeeded => succeeded += 1,
                Round::Unsuccessful(why) | Round::Other(why) => {
                    eprintln!("not the ETXTBSY regression (resource failure?): {why}");
                }
            }
        }
    }
    if succeeded == 0 {
        return Err("no script exited successfully; the host cannot run this test".into());
    }
    Ok(())
}

/// Regression for the Linux `ETXTBSY` race: forking threads must not make a
/// script fail with `ETXTBSY` right after it is written. The pre-fix pattern
/// (`fs::write` then run) fails this on Linux. Only `ExecutableFileBusy`
/// counts as the regression; other write or spawn errors and unsuccessful
/// exits come from resource exhaustion on a loaded host, so they are printed
/// and tolerated as long as every writer finished all its rounds and some
/// script exited successfully. A writer panic fails the test.
#[test]
fn scripts_start_while_other_threads_fork_children() -> TestResult {
    const WRITERS: usize = 4;
    const ROUNDS: usize = 25;
    let stop = AtomicBool::new(false);
    let dir = tempfile::tempdir()?;
    let writers: Vec<Option<Vec<Round>>> = thread::scope(|scope| {
        let forkers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("/bin/true").status();
                    }
                })
            })
            .collect();
        let handles: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let dir = dir.path();
                scope.spawn(move || {
                    (0..ROUNDS)
                        .map(|n| round(&dir.join(format!("script-{writer}-{n}"))))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let writers = handles.into_iter().map(|h| h.join().ok()).collect();
        stop.store(true, Ordering::Relaxed);
        for forker in forkers {
            let _ = forker.join();
        }
        writers
    });
    judge(&writers, ROUNDS)?;
    Ok(())
}

fn exit(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

fn full(round: fn() -> Round, count: usize) -> Option<Vec<Round>> {
    Some((0..count).map(|_| round()).collect())
}

#[test]
fn only_a_successful_exit_counts_as_a_started_script() {
    assert_eq!(classify(Ok(exit(0))), Round::Succeeded);
    assert!(matches!(classify(Ok(exit(1))), Round::Unsuccessful(_)));
    assert!(matches!(classify(Ok(exit(127))), Round::Unsuccessful(_)));
}

#[test]
fn a_busy_executable_is_the_regression_and_other_spawn_errors_are_not() {
    let busy = std::io::Error::from(ErrorKind::ExecutableFileBusy);
    assert_eq!(classify(Err(busy)), Round::Busy);
    let other = std::io::Error::from(ErrorKind::PermissionDenied);
    assert!(matches!(classify(Err(other)), Round::Other(_)));
}

#[test]
fn a_panicked_writer_fails_even_when_another_writer_succeeded() {
    let writers = vec![full(|| Round::Succeeded, 3), None];
    let error = judge(&writers, 3).err().unwrap_or_default();
    assert!(error.contains("writer 1 panicked"), "{error}");
}

#[test]
fn a_writer_that_stopped_early_fails() {
    let writers = vec![full(|| Round::Succeeded, 3), full(|| Round::Succeeded, 2)];
    let error = judge(&writers, 3).err().unwrap_or_default();
    assert!(error.contains("2 of 3"), "{error}");
}

#[test]
fn a_run_without_a_successful_exit_fails() {
    let writers = vec![
        full(|| Round::Unsuccessful("exit 1".into()), 2),
        full(|| Round::Other("write Other".into()), 2),
    ];
    let error = judge(&writers, 2).err().unwrap_or_default();
    assert!(error.contains("no script exited successfully"), "{error}");
}

#[test]
fn a_busy_round_fails_even_among_successes() {
    let mut rounds = vec![Round::Succeeded, Round::Busy];
    rounds.push(Round::Succeeded);
    let error = judge(&[Some(rounds)], 3).err().unwrap_or_default();
    assert!(error.contains("ETXTBSY"), "{error}");
}

#[test]
fn tolerated_failures_pass_with_at_least_one_success() {
    let rounds = vec![
        Round::Succeeded,
        Round::Unsuccessful("exit 1".into()),
        Round::Other("spawn WouldBlock".into()),
    ];
    assert_eq!(judge(&[Some(rounds)], 3), Ok(()));
}
