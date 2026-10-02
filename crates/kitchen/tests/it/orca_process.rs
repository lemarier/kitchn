//! Bounded subprocess behavior of the Orca runner, using `/bin/sh` as a
//! stand-in executable. The adapter itself never invokes a shell.

use std::{io, time::Duration};

use kitchen::adapters::orca::{Invocation, OrcaError, OrcaRunner, SystemRunner};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn sh(script: &str, extra: &[&str], deadline: Duration) -> Invocation {
    let mut args = vec!["-c".to_owned(), script.to_owned(), "sh".to_owned()];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    Invocation::new(args, deadline)
}

#[test]
fn arguments_reach_the_process_unsplit_and_unexpanded() -> TestResult {
    let runner = SystemRunner::new("/bin/sh");
    let output = runner.run(&sh(
        r#"printf '%s|' "$@""#,
        &["a b", "--body=--help", "$(touch /tmp/x)", "it's"],
        Duration::from_secs(10),
    ))?;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "a b|--body=--help|$(touch /tmp/x)|it's|"
    );
    Ok(())
}

#[test]
fn terminal_identity_is_not_inherited() -> TestResult {
    let runner = SystemRunner::new("/bin/sh");
    let output = runner.run(&sh(
        r#"printf '%s' "${ORCA_TERMINAL_HANDLE}${ORCA_PANE_KEY}${ORCA_AGENT_HOOK_TOKEN}""#,
        &[],
        Duration::from_secs(10),
    ))?;
    assert_eq!(output.stdout, b"");
    Ok(())
}

#[test]
fn exit_codes_are_reported_not_hidden() -> TestResult {
    let runner = SystemRunner::new("/bin/sh");
    let output = runner.run(&sh("printf out; exit 3", &[], Duration::from_secs(10)))?;
    assert_eq!(output.exit_code, Some(3));
    assert_eq!(output.stdout, b"out");
    Ok(())
}

#[test]
fn deadline_kills_the_call() {
    let runner = SystemRunner::new("/bin/sh");
    let started = std::time::Instant::now();
    let result = runner.run(&sh("sleep 30", &[], Duration::from_millis(200)));
    assert_eq!(result, Err(OrcaError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn oversized_output_is_refused() {
    let runner = SystemRunner::new("/bin/sh").with_max_stdout(1024);
    let result = runner.run(&sh(
        "while :; do printf 0123456789; done",
        &[],
        Duration::from_secs(10),
    ));
    assert_eq!(result, Err(OrcaError::OutputLimit { limit: 1024 }));
    let exact = SystemRunner::new("/bin/sh").with_max_stdout(10);
    assert!(
        exact
            .run(&sh("printf 0123456789", &[], Duration::from_secs(10)))
            .is_ok()
    );
}

#[test]
fn a_missing_executable_never_starts() {
    let runner = SystemRunner::new("/nonexistent/orca");
    assert_eq!(
        runner.run(&Invocation::new(
            vec!["status".to_owned()],
            Duration::from_secs(1)
        )),
        Err(OrcaError::Spawn(io::ErrorKind::NotFound))
    );
}
