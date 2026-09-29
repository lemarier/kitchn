//! Orca worker signals mapped to the coordinator's recovery evidence, against
//! a simulated Orca runtime (`orca_sim`). Simulated results only; the shapes
//! were read from Orca 1.4.212.

mod common;
mod orca_sim;

use std::time::Duration;

use common::{TestResult, house};
use kitchen::{
    BackendId, CredentialId,
    adapters::orca::{OrcaBackend, OrcaConfig},
    contracts::{BranchName, ExternalRef, ResourceKind, ResourceRef, Timestamp},
    scheduling::AgentFamily,
    workflows::recovery::{
        PromptState, ProviderInterruption, RecoverySignals, StartEvidence, TerminalHolder,
        TranscriptProgress,
    },
};
use orca_sim::{SimMessage, SimOrca, SimOutput, SimWorker};

const DISPATCH: &str = "ctx_recovery";

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

fn worker() -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        kind: ResourceKind::Worker,
        backend: BackendId::new("orca-local")?,
        handle: ExternalRef::new(DISPATCH)?,
    })
}

/// The recovery evidence for `sim_worker`, launched at 0 ms with a 60 s
/// start window and observed at `now` ms.
fn recovery_of(sim_worker: SimWorker, now: u64) -> TestResult<Option<RecoverySignals>> {
    let sim = SimOrca::default();
    sim.set_worker(DISPATCH, sim_worker);
    let backend = OrcaBackend::connect(config(&sim)?, &sim)?;
    let window = kitchen::adapters::orca::StartWindow::new(
        Timestamp::from_unix_millis(0),
        Timestamp::from_unix_millis(now),
        Duration::from_secs(60),
    );
    let signals = backend
        .observe_signals(&worker()?, &window)?
        .ok_or("Orca has no record of the worker")?;
    Ok(signals.recovery())
}

fn recovered(sim_worker: SimWorker, now: u64) -> TestResult<RecoverySignals> {
    recovery_of(sim_worker, now)?.ok_or_else(|| "no recovery evidence".into())
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

/// A live agent Orca reports at its prompt, where a provider refusal has
/// stopped it.
fn idle() -> SimWorker {
    SimWorker {
        activity: "idle",
        ..live()
    }
}

fn live() -> SimWorker {
    SimWorker::new("ready", "in_progress", "live", false)
}

/// A live agent sitting at its first prompt: it was sent the task and never
/// answered.
fn unanswered(liveness: &'static str) -> SimWorker {
    SimWorker {
        activity: "idle",
        output: transcript(vec![message("user", "do the task", 1_000)]),
        ..SimWorker::new("ready", "in_progress", liveness, false)
    }
}

#[test]
fn a_working_agent_maps_every_signal() -> TestResult {
    let working = SimWorker {
        activity: "working",
        output: transcript(vec![
            message("user", "do the task", 1_000),
            message("assistant", "on it", 2_000),
        ]),
        ..live()
    };
    assert_eq!(
        recovered(working, 10_000)?,
        RecoverySignals {
            worker: worker()?,
            start: StartEvidence::TurnObserved,
            prompt: PromptState::Working,
            transcript: Some(TranscriptProgress {
                complete: true,
                agent_spoke: true,
                last_activity: Some(Timestamp::from_unix_millis(2_000)),
            }),
            terminal: TerminalHolder::Agent,
            provider: None,
        }
    );
    Ok(())
}

#[test]
fn a_launch_that_never_started_is_proven_only_after_its_window_closes() -> TestResult {
    let closed = recovered(unanswered("live"), 300_000)?;
    assert_eq!(closed.start, StartEvidence::NoTurn);
    assert_eq!(closed.prompt, PromptState::Idle);
    assert!(closed.proves_never_started());
    // Inside the window, Orca's "accepted" is not yet evidence.
    let open = recovered(unanswered("live"), 1_000)?;
    assert_eq!(open.start, StartEvidence::Unknown);
    assert!(!open.proves_never_started());
    Ok(())
}

#[test]
fn a_truncated_transcript_without_an_agent_message_proves_nothing() -> TestResult {
    // Only the newest page was read, and it holds no agent message: the agent
    // may have spoken earlier.
    let truncated = SimWorker {
        activity: "idle",
        output: Some(SimOutput::Transcript {
            messages: vec![message("user", "nudge", 9_000)],
            complete: false,
        }),
        ..SimWorker::new("ready", "in_progress", "live", false)
    };
    let signals = recovered(truncated, 300_000)?;
    assert_eq!(signals.start, StartEvidence::Unknown);
    assert_eq!(signals.prompt, PromptState::Idle);
    assert_eq!(signals.terminal, TerminalHolder::Agent);
    assert_eq!(
        signals.transcript,
        Some(TranscriptProgress {
            complete: false,
            agent_spoke: false,
            // The prompt sender's message is not the agent's progress.
            last_activity: None,
        })
    );
    assert!(!signals.proves_never_started());
    assert_eq!(signals.idle_since(), None);
    Ok(())
}

#[test]
fn only_the_agents_own_messages_count_as_its_activity() -> TestResult {
    // The prompt sender wrote last; the agent's newest message is older. Its
    // idle clock starts at its own message, not at the later nudge.
    let nudged = SimWorker {
        activity: "idle",
        output: transcript(vec![
            message("user", "do the task", 1_000),
            message("assistant", "on it", 2_000),
            message("user", "status?", 9_000),
        ]),
        ..live()
    };
    let signals = recovered(nudged, 300_000)?;
    assert_eq!(
        signals.idle_since(),
        Some(Timestamp::from_unix_millis(2_000))
    );
    // A tool result is the agent's work too.
    let tooling = SimWorker {
        activity: "idle",
        output: transcript(vec![
            message("user", "do the task", 1_000),
            message("tool", "cargo test", 4_000),
        ]),
        ..live()
    };
    assert_eq!(
        recovered(tooling, 300_000)?.idle_since(),
        Some(Timestamp::from_unix_millis(4_000))
    );
    Ok(())
}

#[test]
fn an_unanswered_prompt_has_no_idle_clock() -> TestResult {
    let signals = recovered(unanswered("live"), 1_000)?;
    assert_eq!(signals.prompt, PromptState::Idle);
    assert_eq!(signals.idle_since(), None);
    Ok(())
}

#[test]
fn a_terminal_without_positive_liveness_is_never_the_agents() -> TestResult {
    // The same unanswered prompt, but Orca cannot verify the process.
    let unverified = recovered(unanswered("unverifiable"), 300_000)?;
    assert_eq!(unverified.terminal, TerminalHolder::Unknown);
    assert!(!unverified.proves_never_started());
    assert_eq!(unverified.idle_since(), None);
    Ok(())
}

#[test]
fn an_idle_agent_reports_when_it_last_made_progress() -> TestResult {
    let idle = SimWorker {
        activity: "done",
        output: transcript(vec![
            message("user", "do the task", 1_000),
            message("assistant", "done, I think", 2_000),
        ]),
        ..live()
    };
    let signals = recovered(idle, 300_000)?;
    assert_eq!(signals.prompt, PromptState::Idle);
    assert_eq!(signals.start, StartEvidence::TurnObserved);
    assert_eq!(
        signals.idle_since(),
        Some(Timestamp::from_unix_millis(2_000))
    );
    assert!(!signals.proves_never_started());
    Ok(())
}

#[test]
fn a_parked_agent_awaits_a_person_and_is_not_idle() -> TestResult {
    let parked = SimWorker {
        activity: "blocked",
        output: transcript(vec![message("assistant", "may I?", 2_000)]),
        ..live()
    };
    let signals = recovered(parked, 300_000)?;
    assert_eq!(signals.prompt, PromptState::AwaitingHuman);
    assert_eq!(signals.idle_since(), None);
    Ok(())
}

#[test]
fn a_persons_terminal_stays_theirs_whatever_the_liveness() -> TestResult {
    for liveness in ["live", "unverifiable"] {
        let taken = SimWorker {
            ownership: "user_owned",
            retained_reason: Some("user_takeover"),
            ..SimWorker::new("ready", "in_progress", liveness, false)
        };
        assert_eq!(
            recovered(taken, 1_000)?.terminal,
            TerminalHolder::Person,
            "{liveness}"
        );
    }
    Ok(())
}

#[test]
fn a_dispatch_of_another_run_yields_no_evidence() -> TestResult {
    let foreign = SimWorker {
        run: "run_other",
        ..unanswered("live")
    };
    assert_eq!(recovery_of(foreign, 300_000)?, None);
    Ok(())
}

#[test]
fn a_launch_orca_failed_before_the_agent_was_ready_is_no_turn_on_an_unknown_terminal() -> TestResult
{
    // The Dispatch ended and the process exited: the start evidence carries
    // over, but an exited terminal is not the agent's to stop.
    let failed = SimWorker {
        stage_detail: Some("agent_readiness"),
        ..SimWorker::new("failed", "failed", "exited", false)
    };
    let signals = recovered(failed, 1_000)?;
    assert_eq!(signals.start, StartEvidence::NoTurn);
    assert_eq!(signals.terminal, TerminalHolder::Unknown);
    assert!(!signals.proves_never_started());
    Ok(())
}

#[test]
fn only_provider_refusals_that_stop_the_agent_interrupt_it() -> TestResult {
    let auth = SimWorker {
        last_error: Some("API Error: 401 authentication_error invalid x-api-key"),
        ..idle()
    };
    let quota = SimWorker {
        output: Some(SimOutput::Terminal(vec![
            "You've hit your usage limit. Try again at 3pm.".to_owned(),
        ])),
        ..idle()
    };
    let throttled = SimWorker {
        output: transcript(vec![message(
            "assistant",
            "API Error: 429 rate_limit_error",
            2_000,
        )]),
        ..idle()
    };
    let other = SimWorker {
        preview: Some("API Error: 500 internal server error"),
        ..idle()
    };
    let cases = [
        (auth, Some(ProviderInterruption::Auth)),
        (quota, Some(ProviderInterruption::Quota)),
        (throttled, Some(ProviderInterruption::RateLimit)),
        (other, None),
        (idle(), None),
    ];
    for (index, (sim_worker, expected)) in cases.into_iter().enumerate() {
        assert_eq!(
            recovered(sim_worker, 1_000)?.provider,
            expected,
            "case {index}"
        );
    }
    Ok(())
}

#[test]
fn a_provider_error_line_never_parks_a_worker_orca_reports_as_working() -> TestResult {
    // The line is scrollback: the agent has moved on and Orca says it works.
    let error = "API Error: 429 rate_limit_error";
    for (index, activity) in ["working", "blocked", "unknown"].into_iter().enumerate() {
        let sim_worker = SimWorker {
            activity,
            preview: Some(error),
            output: Some(SimOutput::Terminal(vec![error.to_owned()])),
            last_error: Some(error),
            ..live()
        };
        assert_eq!(recovered(sim_worker, 1_000)?.provider, None, "case {index}");
    }
    Ok(())
}
