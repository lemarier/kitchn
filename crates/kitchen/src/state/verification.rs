//! Running verification through a backend and recording its result.

use crate::{
    Error, TaskId,
    contracts::{
        Clock, Evidence, EvidenceKind, EvidenceSubject, Fence, HouseGrants, VerificationError,
        VerificationExecutor, VerificationTarget, authorize_access,
    },
    state::HouseStore,
};

type Result<T> = std::result::Result<T, Error>;

/// One verification run a task owner asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationPlan {
    /// The task the run verifies.
    pub task: TaskId,
    /// The fence of the caller's live claim on the task.
    pub fence: Fence,
    /// Where to run the software.
    pub target: VerificationTarget,
    /// The exact revision to run.
    pub subject: EvidenceSubject,
}

/// Run the plan's subject on its target and record the backend's verdict as
/// [`EvidenceKind::AuthorizedVerification`] evidence.
///
/// This is the only way that evidence kind enters the house store, so the
/// store's [`crate::contracts::RecordedEvidence`] for a task holds only
/// verdicts a backend reported for a run it was asked to perform. The
/// authority and scope come from the stored task, not the caller, and access
/// is authorized against the house's `current` grants immediately before the
/// run. The caller's claim must be live and the task not cancelling.
///
/// # Errors
/// Returns a [`crate::state::StateError`] for a missing task, stale or
/// expired claim, or cancellation, and any error from [`authorize_access`],
/// all before the backend is called;
/// [`VerificationError::Unavailable`] when the backend cannot run it; and a
/// storage error when recording fails, in which case the result is not
/// recorded and the run must be repeated.
pub fn run_verification(
    store: &HouseStore,
    executor: &(impl VerificationExecutor + ?Sized),
    current: &HouseGrants,
    plan: &VerificationPlan,
    clock: &dyn Clock,
) -> Result<Evidence> {
    let task = store.verification_task(&plan.task, plan.fence, clock.now())?;
    let spec = task.spec();
    let access = authorize_access(
        &spec.authority,
        current,
        executor,
        &plan.target,
        &spec.scope(),
    )?;
    let result = executor
        .verify(&access, &plan.subject)
        .map_err(VerificationError::Unavailable)?;
    let evidence = Evidence {
        kind: EvidenceKind::AuthorizedVerification(access),
        verdict: result.verdict,
        subject: plan.subject.clone(),
        source: result.source,
        observed_at: clock.now(),
    };
    store.record_verification(&plan.task, plan.fence, evidence.clone(), clock.now())?;
    Ok(evidence)
}
