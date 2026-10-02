//! Recovery signals of the Orca adapter against a simulated Orca runtime
//! (`orca_sim`): what a coordinator can tell about a worker without a person.
//!
//! These are simulated results about response mapping. The shapes they
//! simulate were read from Orca 1.4.212; the live smoke test in `orca_live`
//! observes signals of the workers it launches.

use crate::common;
use crate::orca_sim;

use std::time::Duration;

use common::{TestResult, house, task_id};
use kitchen::{
    BackendId, CredentialId,
    adapters::orca::{
        AgentPrompt, DispatchActivity, MAX_ARCHIVE_PAGES, OrcaBackend, OrcaConfig, OrcaError,
        ProviderErrorClass, SIGNAL_WINDOW_ROWS, StartOutcome, StartWindow, TerminalOwner,
        TranscriptProgress, WorkerSignals,
    },
    contracts::{
        AttemptNumber, BranchName, Effect, EffectExecutor, EffectFailure, EffectRequest,
        ExternalRef, IdempotencyKey, Liveness, NotAppliedReason, Operation, ResourceKind,
        ResourceRef, Role, Text, Timestamp, Workspace,
    },
    scheduling::AgentFamily,
};
use orca_sim::{Fault, SimMessage, SimOrca, SimOutput, SimWorker};

const DISPATCH: &str = "ctx_signals";

fn config(sim: &SimOrca) -> TestResult<OrcaConfig> {
    Ok(OrcaConfig {
        backend: BackendId::new("orca-local")?,
        house: house()?,
        credential: CredentialId::new("orca-host-session")?,
        run: ExternalRef::new("run_sim")?,
        coordinator: ExternalRef::new("term_coordinator")?,
        repo: ExternalRef::new("id:repo-1")?,
        base_branch: Some(ExternalRef::new("main")?),
        branch_prefix: Some(BranchName::new("lemarier")?),
        agent: AgentFamily::Claude,
        call_timeout: Duration::from_secs(5),
        launch_timeout: Duration::from_secs(60),
        runtime_dir: sim.runtime_dir()?,
        reservation_timeout: Duration::from_secs(10),
    })
}

fn worker(handle: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Worker,
        backend: BackendId::new("orca-local")?,
        handle: ExternalRef::new(handle)?,
    })
}

/// A launch recorded at 0 ms, judged at `now` ms, with a 60 s window.
fn window(now: u64) -> StartWindow {
    StartWindow::new(
        Timestamp::from_unix_millis(0),
        Timestamp::from_unix_millis(now),
        Duration::from_secs(60),
    )
}

fn message(role: &'static str, text: &str, at: u64) -> SimMessage {
    SimMessage {
        role,
        text: text.to_owned(),
        at,
    }
}

fn transcript(messages: Vec<SimMessage>) -> Option<SimOutput> {
    Some(SimOutput::Transcript {
        messages,
        complete: true,
    })
}

/// The signals of a worker the simulator holds as `sim_worker`, read at `now`.
fn signals_of(sim_worker: SimWorker, now: u64) -> TestResult<WorkerSignals> {
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, sim_worker);
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    backend
        .observe_signals(&worker(DISPATCH)?, &window(now))?
        .ok_or_else(|| "Orca has no record of the worker".into())
}

fn live() -> SimWorker {
    SimWorker::new("ready", "in_progress", "live", false)
}

#[test]
fn a_worker_that_took_its_launch_shows_a_turn_and_its_progress() -> TestResult {
    let sim = SimOrca::default();
    sim.set_worker(
        DISPATCH,
        SimWorker {
            activity: "working",
            output: transcript(vec![
                message("user", "do the task", 1_000),
                message("assistant", "on it", 2_000),
                message("tool", "ran", 3_000),
            ]),
            ..live()
        },
    );
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let before = sim.state().calls.len();
    let signals = backend
        .observe_signals(&worker(DISPATCH)?, &window(10_000))?
        .ok_or("no signals")?;
    assert_eq!(signals.dispatch, DispatchActivity::Active);
    assert_eq!(signals.liveness, Liveness::Live);
    assert_eq!(signals.start, StartOutcome::TurnObserved);
    assert_eq!(signals.prompt, AgentPrompt::Working);
    assert_eq!(signals.terminal, TerminalOwner::Supervised);
    assert_eq!(signals.provider_error, None);
    assert_eq!(
        signals.transcript,
        Some(TranscriptProgress {
            messages: 3,
            complete: true,
            last_activity: Some(Timestamp::from_unix_millis(3_000)),
            last_agent_activity: Some(Timestamp::from_unix_millis(3_000)),
            agent_spoke: true,
        })
    );
    assert!(signals.accepts_messages());
    // Two read-only calls, the second bounded to the newest 50 messages.
    let calls = sim.state().calls.split_off(before);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls.first().map(|call| &call[..2]),
        Some(&["orchestration".to_owned(), "worker-show".to_owned()][..])
    );
    assert_eq!(
        calls.get(1),
        Some(&vec![
            "orchestration".to_owned(),
            "worker-read".to_owned(),
            format!("--dispatch={DISPATCH}"),
            "--source=auto".to_owned(),
            "--limit=50".to_owned(),
            "--json".to_owned(),
        ])
    );
    Ok(())
}

#[test]
fn a_swallowed_launch_is_accepted_and_then_never_observed() -> TestResult {
    // A shell prompt took the launch input: Orca accepted it, but the agent
    // never reported a turn and its liveness cannot be verified.
    let swallowed = || SimWorker {
        output: Some(SimOutput::Terminal(vec![
            "$ claude".to_owned(),
            "zsh: command not found: claude".to_owned(),
        ])),
        ..SimWorker::new("ready", "in_progress", "unverifiable", false)
    };
    let inside = signals_of(swallowed(), 59_000)?;
    assert_eq!(inside.start, StartOutcome::Accepted);
    let after = signals_of(swallowed(), 60_000)?;
    assert_eq!(after.start, StartOutcome::NeverObserved);
    // Missing evidence stays missing: the liveness is not promoted, and
    // terminal output is not a transcript.
    assert_eq!(after.liveness, Liveness::Unverifiable);
    assert_eq!(after.transcript, None);
    assert_eq!(after.prompt, AgentPrompt::Unknown);
    assert_eq!(after.dispatch, DispatchActivity::Active);
    Ok(())
}

#[test]
fn a_launch_orca_recorded_as_failed_is_never_observed() -> TestResult {
    let failed = SimWorker {
        stage_detail: Some("agent_readiness"),
        ..SimWorker::new("failed", "failed", "exited", false)
    };
    let signals = signals_of(failed, 1_000)?;
    assert_eq!(signals.start, StartOutcome::NeverObserved);
    assert_eq!(signals.dispatch, DispatchActivity::Ended);
    assert_eq!(signals.liveness, Liveness::Exited);
    assert!(!signals.accepts_messages());
    // The same failure once the agent had reported is a turn that ended badly.
    let reported = SimWorker::new("failed", "failed", "exited", false);
    assert_eq!(
        signals_of(reported, 1_000)?.start,
        StartOutcome::TurnObserved
    );
    Ok(())
}

#[test]
fn a_worker_whose_process_exited_after_starting_is_not_a_failed_launch() -> TestResult {
    // An hour in, the agent's process ended without a report or a stop, and
    // its terminal is gone, so Orca refuses the read.
    let exited = SimWorker {
        stage_detail: Some("process_exited"),
        ..SimWorker::new("failed", "failed", "exited", false)
    };
    let signals = signals_of(exited, 3_600_000)?;
    assert_eq!(signals.start, StartOutcome::Unknown);
    assert_eq!(signals.transcript, None);
    assert_eq!(signals.liveness, Liveness::Exited);
    assert_eq!(signals.dispatch, DispatchActivity::Ended);
    Ok(())
}

#[test]
fn a_closed_window_with_an_unreadable_output_is_not_a_swallowed_launch() -> TestResult {
    // Orca refuses the read: nothing was seen, so nothing is concluded.
    let refused = SimWorker {
        output: None,
        ..SimWorker::new("ready", "in_progress", "unverifiable", false)
    };
    assert_eq!(signals_of(refused, 90_000)?.start, StartOutcome::Unknown);
    Ok(())
}

#[test]
fn an_agent_at_its_prompt_without_a_report_is_idle_not_done() -> TestResult {
    let idle = SimWorker {
        activity: "done",
        output: transcript(vec![
            message("user", "do the task", 1_000),
            message("assistant", "done, I think", 2_000),
        ]),
        ..live()
    };
    let signals = signals_of(idle, 300_000)?;
    assert_eq!(signals.prompt, AgentPrompt::AtPrompt);
    assert_eq!(signals.start, StartOutcome::TurnObserved);
    // Nothing settled the worker: the Dispatch is still the live attempt.
    assert_eq!(signals.dispatch, DispatchActivity::Active);
    // A first prompt that has not started a turn is at its prompt too, but
    // that is not a turn.
    let fresh = signals_of(
        SimWorker {
            activity: "idle",
            output: transcript(vec![message("user", "do the task", 1_000)]),
            ..live()
        },
        300_000,
    )?;
    assert_eq!(fresh.prompt, AgentPrompt::AtPrompt);
    assert_eq!(fresh.start, StartOutcome::NeverObserved);
    Ok(())
}

#[test]
fn a_parked_agent_awaits_a_person() -> TestResult {
    let parked = SimWorker {
        activity: "working",
        waiting: true,
        ..live()
    };
    assert_eq!(
        signals_of(parked, 1_000)?.prompt,
        AgentPrompt::AwaitingHuman
    );
    let blocked = SimWorker {
        activity: "blocked",
        ..live()
    };
    assert_eq!(
        signals_of(blocked, 1_000)?.prompt,
        AgentPrompt::AwaitingHuman
    );
    Ok(())
}

fn message_request(target: &ResourceRef) -> TestResult<EffectRequest> {
    message_request_keyed(target, "message-1")
}

fn message_request_keyed(target: &ResourceRef, key: &str) -> TestResult<EffectRequest> {
    Ok(EffectRequest::new(
        house()?,
        BackendId::new("orca-local")?,
        CredentialId::new("orca-host-session")?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new(key)?),
        Effect::Worker(Operation::MessageWorker {
            worker: target.clone(),
            body: Text::new("hello")?,
        }),
    ))
}

#[test]
fn a_terminal_a_person_holds_is_reported_and_gets_no_message() -> TestResult {
    let sim = SimOrca::default();
    sim.set_worker(
        DISPATCH,
        SimWorker {
            ownership: "user_owned",
            retained_reason: Some("user_takeover"),
            ..live()
        },
    );
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let target = worker(DISPATCH)?;
    let signals = backend
        .observe_signals(&target, &window(1_000))?
        .ok_or("no signals")?;
    assert_eq!(signals.terminal, TerminalOwner::Person);
    assert_eq!(signals.dispatch, DispatchActivity::Active);
    assert!(!signals.accepts_messages());
    // The signal agrees with what the adapter does.
    assert!(matches!(
        backend.execute(&message_request(&target)?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    ));
    assert!(sim.calls_to(&["orchestration", "send"]).is_empty());
    Ok(())
}

#[test]
fn a_message_to_an_ended_dispatch_is_refused_before_anything_is_queued() -> TestResult {
    // Orca answers `dispatch_inactive` for a Dispatch whose worker will never
    // read its mailbox, observed live on 1.4.212. Nothing is queued, so the
    // effect did not happen and needs no reconciliation.
    let sim = SimOrca::default();
    sim.set_worker(
        DISPATCH,
        SimWorker::new("stopped", "failed", "exited", false),
    );
    sim.set_worker(
        "ctx_fenced",
        SimWorker {
            fenced: true,
            ..live()
        },
    );
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    for (index, dispatch) in [DISPATCH, "ctx_fenced"].into_iter().enumerate() {
        let target = worker(dispatch)?;
        let signals = backend
            .observe_signals(&target, &window(1_000))?
            .ok_or("no signals")?;
        assert_eq!(signals.dispatch, DispatchActivity::Ended, "{dispatch}");
        assert!(!signals.accepts_messages(), "{dispatch}");
        let effects = sim.state().effects;
        assert_eq!(
            backend.execute(&message_request_keyed(&target, &format!("late-{index}"))?),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
            "{dispatch}"
        );
        assert_eq!(sim.state().effects, effects, "{dispatch}: nothing changed");
    }
    // An active Dispatch still takes the message.
    sim.set_worker("ctx_live", live());
    assert!(
        backend
            .execute(&message_request_keyed(&worker("ctx_live")?, "on-time")?)
            .is_ok()
    );
    // A refusal Kitchen does not recognize stays uncertain.
    sim.fault_on(&["orchestration", "send"], Fault::Refuse("runtime_error"));
    assert!(matches!(
        backend.execute(&message_request_keyed(&worker("ctx_live")?, "odd")?),
        Err(EffectFailure::Uncertain(_))
    ));
    Ok(())
}

#[test]
fn another_runs_dispatch_is_outside_the_run_and_gets_no_message() -> TestResult {
    let sim = SimOrca::default();
    sim.set_worker(
        DISPATCH,
        SimWorker {
            run: "run_other",
            ..live()
        },
    );
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let target = worker(DISPATCH)?;
    let signals = backend
        .observe_signals(&target, &window(1_000))?
        .ok_or("no signals")?;
    assert_eq!(signals.dispatch, DispatchActivity::OutsideRun);
    assert!(!signals.accepts_messages());
    assert!(matches!(
        backend.execute(&message_request(&target)?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    ));
    Ok(())
}

#[test]
fn a_dispatch_stops_being_active_when_the_worker_is_stopped() -> TestResult {
    let sim = SimOrca::default();
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let launch = EffectRequest::new(
        house()?,
        BackendId::new("orca-local")?,
        CredentialId::new("orca-host-session")?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("launch-1")?),
        Effect::Worker(Operation::LaunchWorker {
            role: Role::StationCook,
            workspace: Workspace::Isolated,
            brief: Text::new("do the task")?,
            branch: None,
            pinned: None,
            agent: None,
        }),
    );
    let receipt = backend.execute(&launch)?;
    let started = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .cloned()
        .ok_or("no worker")?;
    let fresh = backend
        .observe_signals(&started, &window(1_000))?
        .ok_or("no signals")?;
    assert_eq!(fresh.dispatch, DispatchActivity::Active);
    assert_eq!(fresh.start, StartOutcome::Accepted);
    assert!(fresh.accepts_messages());

    let cancel = EffectRequest::new(
        house()?,
        BackendId::new("orca-local")?,
        CredentialId::new("orca-host-session")?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("cancel-1")?),
        Effect::Worker(Operation::CancelWorker {
            worker: started.clone(),
        }),
    );
    backend.execute(&cancel)?;
    let stopped = backend
        .observe_signals(&started, &window(1_000))?
        .ok_or("no signals")?;
    assert_eq!(stopped.dispatch, DispatchActivity::Ended);
    assert!(!stopped.accepts_messages());
    // Kitchen's own stop says nothing about whether a turn happened.
    assert_eq!(stopped.start, StartOutcome::Unknown);
    assert_eq!(stopped.liveness, Liveness::Exited);
    // A fenced Dispatch is ended even when nothing else has changed yet.
    assert_eq!(
        signals_of(
            SimWorker {
                fenced: true,
                ..live()
            },
            1_000
        )?
        .dispatch,
        DispatchActivity::Ended
    );
    Ok(())
}

#[test]
fn provider_failures_are_classified_and_never_echoed() -> TestResult {
    let secret = "sk-ant-api03-SECRETSECRETSECRET";
    let auth = SimWorker {
        last_error: Some(
            "API Error: 401 authentication_error invalid x-api-key sk-ant-api03-SECRETSECRETSECRET",
        ),
        ..SimWorker::new("failed", "failed", "exited", false)
    };
    let quota = SimWorker {
        output: Some(SimOutput::Terminal(vec![
            "> continue".to_owned(),
            "You've hit your usage limit. Try again at 3pm.".to_owned(),
        ])),
        ..live()
    };
    let throttled = SimWorker {
        output: transcript(vec![
            message("user", "go", 1_000),
            message(
                "assistant",
                "API Error: 429 rate_limit_error (Bearer sk-ant-api03-SECRETSECRETSECRET)",
                2_000,
            ),
        ]),
        ..live()
    };
    let other = SimWorker {
        preview: Some("API Error: 500 internal server error"),
        ..live()
    };
    let cases = [
        (auth, Some(ProviderErrorClass::Auth)),
        (quota, Some(ProviderErrorClass::Quota)),
        (throttled, Some(ProviderErrorClass::RateLimit)),
        (other, Some(ProviderErrorClass::Other)),
        (live(), None),
    ];
    for (index, (sim_worker, expected)) in cases.into_iter().enumerate() {
        let signals = signals_of(sim_worker, 1_000)?;
        assert_eq!(signals.provider_error, expected, "case {index}");
        assert!(
            !format!("{signals:?}").contains("SECRET"),
            "case {index} exposes {secret}"
        );
    }
    Ok(())
}

#[test]
fn a_recorded_start_error_is_not_pushed_out_by_long_output() -> TestResult {
    // Orca recorded why the start failed; the terminal since printed far more
    // than the scanned window, none of it an error.
    let chatty = SimWorker {
        last_error: Some("API Error: 429 rate_limit_error"),
        output: Some(SimOutput::Terminal(vec!["progress line".repeat(2_000)])),
        ..SimWorker::new("failed", "failed", "exited", false)
    };
    assert_eq!(
        signals_of(chatty, 1_000)?.provider_error,
        Some(ProviderErrorClass::RateLimit)
    );
    Ok(())
}

#[test]
fn quoted_errors_in_tool_output_and_prompts_are_not_the_providers() -> TestResult {
    let quoting = SimWorker {
        output: transcript(vec![
            message(
                "user",
                "API Error: 429 rate_limit_error is what we saw",
                1_000,
            ),
            message("tool", "API Error: 401 authentication_error", 2_000),
            message(
                "assistant",
                "The API rate limit is 100 per minute; I am handling the oauth token refresh.",
                3_000,
            ),
        ]),
        preview: Some("You are not logged into any GitHub hosts. Run gh auth login"),
        ..live()
    };
    assert_eq!(signals_of(quoting, 1_000)?.provider_error, None);
    let prose = SimWorker {
        output: Some(SimOutput::Terminal(vec![
            "● Too many requests reach the cache, so I added a limiter.".to_owned(),
            "+    if body.contains(\"usage limit\") {".to_owned(),
        ])),
        ..live()
    };
    assert_eq!(signals_of(prose, 1_000)?.provider_error, None);
    Ok(())
}

#[test]
fn transcript_progress_is_a_lower_bound_when_the_window_clips() -> TestResult {
    let long: Vec<SimMessage> = (1..=60_u64)
        .map(|n| message("assistant", "step", n * 1_000))
        .collect();
    let clipped = signals_of(
        SimWorker {
            output: transcript(long),
            ..live()
        },
        1_000,
    )?;
    assert_eq!(
        clipped.transcript,
        Some(TranscriptProgress {
            messages: 50,
            complete: false,
            last_activity: Some(Timestamp::from_unix_millis(60_000)),
            last_agent_activity: Some(Timestamp::from_unix_millis(60_000)),
            agent_spoke: true,
        })
    );
    // Only the prompt sender has written: nothing from the agent yet.
    let prompt_only = signals_of(
        SimWorker {
            output: transcript(vec![message("user", "go", 1_000)]),
            ..live()
        },
        90_000,
    )?;
    assert_eq!(
        prompt_only.transcript,
        Some(TranscriptProgress {
            messages: 1,
            complete: true,
            last_activity: Some(Timestamp::from_unix_millis(1_000)),
            last_agent_activity: None,
            agent_spoke: false,
        })
    );
    assert_eq!(prompt_only.start, StartOutcome::NeverObserved);
    Ok(())
}

/// A released worker whose archive holds `count` messages, one a second.
fn released(count: u64) -> SimWorker {
    SimWorker {
        release_state: "released",
        ownership: "released",
        output: Some(SimOutput::Archived(
            (1..=count)
                .map(|n| message("assistant", "step", n * 1_000))
                .collect(),
        )),
        ..SimWorker::new("succeeded", "succeeded", "exited", false)
    }
}

/// The signals of `sim_worker` and the number of `worker-read` calls made.
fn read_signals(sim_worker: SimWorker) -> TestResult<(WorkerSignals, usize)> {
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, sim_worker);
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let signals = backend
        .observe_signals(&worker(DISPATCH)?, &window(1_000))?
        .ok_or("Orca has no record of the worker")?;
    Ok((
        signals,
        sim.calls_to(&["orchestration", "worker-read"]).len(),
    ))
}

#[test]
fn a_released_worker_is_read_from_its_newest_archived_page() -> TestResult {
    // 120 messages: the first page holds the oldest 50, so the cursor is
    // followed to the empty page after the last.
    let (signals, reads) = read_signals(released(120))?;
    assert_eq!(
        signals.transcript,
        Some(TranscriptProgress {
            messages: 50,
            complete: false,
            last_activity: Some(Timestamp::from_unix_millis(120_000)),
            last_agent_activity: Some(Timestamp::from_unix_millis(120_000)),
            agent_spoke: true,
        })
    );
    assert_eq!(reads, 4, "0-50, 50-100, 100-120, then an empty page");
    // An archive of one page: its end is still confirmed.
    let (signals, reads) = read_signals(released(30))?;
    assert_eq!(
        signals.transcript,
        Some(TranscriptProgress {
            messages: 30,
            complete: true,
            last_activity: Some(Timestamp::from_unix_millis(30_000)),
            last_agent_activity: Some(Timestamp::from_unix_millis(30_000)),
            agent_spoke: true,
        })
    );
    assert_eq!(reads, 2);
    // A live worker's read is already its newest window: one call.
    let (_, reads) = read_signals(SimWorker {
        output: transcript(vec![message("assistant", "on it", 1_000)]),
        ..live()
    })?;
    assert_eq!(reads, 1);
    Ok(())
}

#[test]
fn an_archive_that_does_not_end_in_bounds_has_no_transcript() -> TestResult {
    // The last read of the bound confirms the end with an empty page, so
    // the largest archive it reaches fills every page before it.
    let fits = u64::try_from((MAX_ARCHIVE_PAGES - 1) * SIGNAL_WINDOW_ROWS)?;
    let (signals, reads) = read_signals(released(fits))?;
    assert_eq!(
        signals
            .transcript
            .and_then(|progress| progress.last_activity),
        Some(Timestamp::from_unix_millis(fits * 1_000))
    );
    assert_eq!(reads, MAX_ARCHIVE_PAGES);
    // One message more and the newest page is never shown to be the last.
    let (signals, reads) = read_signals(released(fits + 1))?;
    assert_eq!(signals.transcript, None);
    assert_eq!(reads, MAX_ARCHIVE_PAGES);
    assert_eq!(signals.dispatch, DispatchActivity::Ended);
    Ok(())
}

#[test]
fn an_archive_page_without_a_cursor_must_prove_the_archive_complete() -> TestResult {
    // A limited first page with no cursor: more of the archive may exist.
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, released(120));
    sim.state().archive_cursor_ends_at = Some(0);
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let signals = backend
        .observe_signals(&worker(DISPATCH)?, &window(1_000))?
        .ok_or("Orca has no record of the worker")?;
    assert_eq!(signals.transcript, None, "the oldest page is not progress");
    assert_eq!(sim.calls_to(&["orchestration", "worker-read"]).len(), 1);
    // A later page with no cursor does not show it is the last.
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, released(120));
    sim.state().archive_cursor_ends_at = Some(50);
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let signals = backend
        .observe_signals(&worker(DISPATCH)?, &window(1_000))?
        .ok_or("Orca has no record of the worker")?;
    assert_eq!(signals.transcript, None, "the newest page is not shown");
    assert_eq!(signals.dispatch, DispatchActivity::Ended);
    assert_eq!(sim.calls_to(&["orchestration", "worker-read"]).len(), 2);
    // A whole archive in one page proves itself without a cursor.
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, released(30));
    sim.state().archive_cursor_ends_at = Some(0);
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let signals = backend
        .observe_signals(&worker(DISPATCH)?, &window(1_000))?
        .ok_or("Orca has no record of the worker")?;
    assert_eq!(
        signals.transcript,
        Some(TranscriptProgress {
            messages: 30,
            complete: true,
            last_activity: Some(Timestamp::from_unix_millis(30_000)),
            last_agent_activity: Some(Timestamp::from_unix_millis(30_000)),
            agent_spoke: true,
        })
    );
    assert_eq!(sim.calls_to(&["orchestration", "worker-read"]).len(), 1);
    Ok(())
}

#[test]
fn a_later_archive_page_that_fails_leaves_no_transcript() -> TestResult {
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, released(120));
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    // The oldest page is read; Orca refuses the next one.
    sim.state().reads_left = Some(1);
    let signals = backend
        .observe_signals(&worker(DISPATCH)?, &window(1_000))?
        .ok_or("Orca has no record of the worker")?;
    assert_eq!(signals.transcript, None, "the oldest page is not progress");
    assert_eq!(signals.terminal, TerminalOwner::Released);
    assert_eq!(sim.calls_to(&["orchestration", "worker-read"]).len(), 2);
    Ok(())
}

#[test]
fn a_worker_without_a_readable_output_still_has_its_other_signals() -> TestResult {
    // Orca refuses the read: no transcript, but the projection still speaks.
    let refused = SimWorker {
        activity: "working",
        output: None,
        ..live()
    };
    let signals = signals_of(refused, 1_000)?;
    assert_eq!(signals.transcript, None);
    assert_eq!(signals.provider_error, None);
    assert_eq!(signals.start, StartOutcome::TurnObserved);
    assert_eq!(signals.prompt, AgentPrompt::Working);
    Ok(())
}

#[test]
fn a_read_that_cannot_complete_is_an_error_not_an_empty_answer() -> TestResult {
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, live());
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    sim.fault_on(
        &["orchestration", "worker-read"],
        Fault::TimeoutBeforeEffect,
    );
    assert!(matches!(
        backend.observe_signals(&worker(DISPATCH)?, &window(1_000)),
        Err(OrcaError::Timeout)
    ));
    sim.fault_on(&["orchestration", "worker-show"], Fault::Garbage);
    assert!(matches!(
        backend.observe_signals(&worker(DISPATCH)?, &window(1_000)),
        Err(OrcaError::Malformed { .. })
    ));
    Ok(())
}

#[test]
fn a_worker_orca_does_not_know_has_no_signals() -> TestResult {
    let sim = SimOrca::default();
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    assert_eq!(
        backend.observe_signals(&worker("ctx_unknown")?, &window(1_000))?,
        None
    );
    // A reference from another backend is not this backend's to observe.
    let foreign = ResourceRef {
        backend: BackendId::new("orca-other")?,
        ..worker(DISPATCH)?
    };
    sim.set_worker(DISPATCH, live());
    assert_eq!(backend.observe_signals(&foreign, &window(1_000))?, None);
    // Nor is a resource that is not a worker.
    let branch = ResourceRef {
        kind: ResourceKind::Branch,
        ..worker(DISPATCH)?
    };
    assert_eq!(backend.observe_signals(&branch, &window(1_000))?, None);
    Ok(())
}
