//! Orca's worker signals as the coordinator's backend-neutral recovery
//! evidence.
//!
//! [`WorkerSignals::recovery`] keeps every "cannot tell" a "cannot tell", and
//! only adds caution: evidence that could let the coordinator stop a worker
//! (a terminal known to be the agent's) needs Orca's positive `live` verdict,
//! and a Dispatch outside this Run yields no evidence at all.

use crate::{
    adapters::orca::{
        AgentPrompt, DispatchActivity, ProviderErrorClass, StartOutcome, TerminalOwner,
        WorkerSignals,
    },
    contracts::Liveness,
    workflows::recovery::{
        PromptState, ProviderInterruption, RecoverySignals, StartEvidence, TerminalHolder,
        TranscriptProgress,
    },
};

impl WorkerSignals {
    /// The coordinator's recovery evidence for this worker.
    ///
    /// `None` when the Dispatch belongs to another Run: the adapter acts on
    /// none of them, so neither may the coordinator. Otherwise:
    ///
    /// - A launch Orca accepted whose window is still open is not yet
    ///   evidence of anything, so [`StartOutcome::Accepted`] reads as
    ///   [`StartEvidence::Unknown`].
    /// - The terminal is the agent's only while Orca reports it supervised
    ///   and live. An external, released, or unverifiable terminal is not
    ///   known to be the agent's, so it can never be stopped as stalled. A
    ///   person's terminal stays theirs whatever the liveness.
    /// - A provider failure other than an auth, quota, or rate limit does not
    ///   stop the agent until the provider recovers, so it is no
    ///   interruption.
    #[must_use]
    pub fn recovery(&self) -> Option<RecoverySignals> {
        match self.dispatch {
            DispatchActivity::OutsideRun => return None,
            DispatchActivity::Active | DispatchActivity::Ended | DispatchActivity::Unknown => {}
        }
        Some(RecoverySignals {
            worker: self.worker.clone(),
            start: match self.start {
                StartOutcome::TurnObserved => StartEvidence::TurnObserved,
                StartOutcome::NeverObserved => StartEvidence::NoTurn,
                StartOutcome::Accepted | StartOutcome::Unknown => StartEvidence::Unknown,
            },
            prompt: match self.prompt {
                AgentPrompt::Working => PromptState::Working,
                AgentPrompt::AtPrompt => PromptState::Idle,
                AgentPrompt::AwaitingHuman => PromptState::AwaitingHuman,
                AgentPrompt::Unknown => PromptState::Unknown,
            },
            transcript: self.transcript.map(|progress| TranscriptProgress {
                agent_spoke: progress.agent_spoke,
                last_activity: progress.last_activity,
            }),
            terminal: match (self.terminal, self.liveness) {
                (TerminalOwner::Person, _) => TerminalHolder::Person,
                (TerminalOwner::Supervised, Liveness::Live) => TerminalHolder::Agent,
                (TerminalOwner::Supervised, Liveness::Exited | Liveness::Unverifiable)
                | (TerminalOwner::External | TerminalOwner::Released | TerminalOwner::Unknown, _) => {
                    TerminalHolder::Unknown
                }
            },
            provider: self.provider_error.and_then(|class| match class {
                ProviderErrorClass::Auth => Some(ProviderInterruption::Auth),
                ProviderErrorClass::Quota => Some(ProviderInterruption::Quota),
                ProviderErrorClass::RateLimit => Some(ProviderInterruption::RateLimit),
                ProviderErrorClass::Other => None,
            }),
        })
    }
}
