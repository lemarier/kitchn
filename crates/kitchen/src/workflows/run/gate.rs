//! The scheduled gate pass: evaluate the open pull requests of settled
//! scheduled tasks at their exact heads, and merge one whose verdict is a
//! merge through the gate's head-matched [`MergeRequest`].
//!
//! The forge supplies most of the evidence. The rest, the independent
//! review, acceptance, hardware, and risk facts, comes from the
//! attestation recorded for exactly the pull request's head and base
//! ([`super::gate_attestation`]). The attestation counts only when the
//! house's forge shows the review it names approved on that head by the
//! claimed login, the forge links every commit of the pull request to its
//! author's and committer's logins, and the reviewer is none of them nor the
//! pull request's author; otherwise the pull request is only reported. So is
//! a branch a person wrote and a pull request of more than
//! [`MAX_PULL_REQUEST_COMMITS`](crate::integrations::github::MAX_PULL_REQUEST_COMMITS)
//! commits.
//! A merge also needs the house's readiness-checked [`MergeGrant`] for that
//! exact subject.
//!
//! Only a pull request whose verdict, evaluated without history, is a
//! merge is recorded. It gets a gate task, claimed under this pass's lease,
//! whose evidence subject is the verdict's head and base. The task's id
//! carries a digest of its specification (the pinned revisions and the
//! delegated grants) and a generation, so a house whose revisions or grants
//! changed gets a new task, and so does a pull request whose task under the
//! current specification settled or has little ownership history left. The
//! pull request's other gate tasks are reconciled and settled first, and
//! nothing is recorded or merged until they are. An effect of any gate task
//! of the pull request, settled or not, whose outcome the forge has not
//! proven bars every merge of that pull request: a risk decision about it
//! lets its own task settle, and says nothing about a merge request that may
//! still land. Only a lookup that proves the effect applied or absent lifts
//! that. A pass that does not merge
//! leaves the task's attempt interrupted, and the next pass continues that
//! attempt instead of spending another, until the task's retry deadline:
//! past it the attempt ends and the task settles as exhausted. The pull
//! request is then only reported while its head and base stay the ones that
//! task last judged; a new head or base is new evidence and gets a new task.
//! The verdict and
//! its merge intent are persisted through [`HouseGateStore`], the provider's
//! head and the base branch tip are read again ([`GateRun::next_merge`]),
//! and only then is that exact intent submitted; a moved head or base
//! records the intent as not applied. A merge counts once the forge reads
//! it back as merged at that head. Fix requests and hand-overs are never
//! performed here: they stay with a person. This pass never merges through
//! the stack tool.

use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
};

use super::{
    KitchenPullRequest, Outcome, Pass, Refusal, RunError, attestation, gate_attestation,
    kitchen_pull_requests, repair_of, take_for_pass,
};
use crate::workflows::known;
use crate::workflows::pickup::stable_hash;
use crate::workflows::tick::PassRun;
use crate::{
    BackendId, TaskId, WorkflowId,
    contracts::{
        AttemptOutcome, AttemptStart, CapabilityRequirements, Claimant, Clock, CommitId,
        ContractError, EffectExecutor, EffectFailure, Evidence, EvidenceKind, EvidenceSubject,
        EvidenceVerdict, ExternalRef, Fence, IdempotencyKey, IssueNumber, Lookup, NotAppliedReason,
        Provenance, Repository, RetryPolicy, Role, Settlement, TaskAuthority, TaskSpec, Timestamp,
        ValueKind,
    },
    house::{HouseConfig, HouseError, MergeSubject},
    integrations::github::{
        GitHubClient, GitHubExecutor, GitHubMutationTransport, IntegrationError, Observation,
        ReadLimits,
    },
    state::{
        EffectOutcome, EffectState, HouseStore, MAX_OWNERSHIP_HISTORY, MarkerFact, MarkerKey,
        MarkerSchema, MarkerSubject, StateError, TaskRecord, TaskState, WorkItem, reconcile,
        reread_settled,
    },
    workflows::{
        gate::{
            Admission, FixGrant, ForgeGatePolicy, Gap, GateEvidence, GateGrants, GateHistory,
            GateMode, GateRun, GateStoreError, GateSupplement, HouseGateStore, MergeGrant,
            MergeRequest, ReviewTriggers, SemanticReview, Verdict, collect_forge_evidence,
            evaluate,
        },
        pickup::derived_task_id,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Pull requests one gate pass evaluates, as the gate's own bound.
pub const MAX_GATE_PULL_REQUESTS: usize = 3;

/// One scheduled gate pass over one repository.
pub struct GatePass<'a, T> {
    /// The house store.
    pub store: &'a HouseStore,
    /// The house configuration.
    pub house: &'a HouseConfig,
    /// The house's forge: its reads, and its writes for a granted merge.
    pub forge: &'a GitHubClient<T>,
    /// The forge backend of the house's forge binding, where merges run.
    pub forge_backend: &'a BackendId,
    /// The pinned revisions a gate task records.
    pub provenance: &'a Provenance,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// The repository.
    pub repository: &'a Repository,
    /// The house's forge logins: the pull request authors eligible for
    /// unattended merge.
    pub authors: &'a [String],
    /// Take over an expired pass lease, or an expired claim on a gate task,
    /// instead of stopping.
    pub take_over: bool,
    /// The house tick's run this pass serves, if a tick started it: each
    /// task it assesses, and each gate task before its claim, is recorded
    /// on it, and it is renewed with the pass lease.
    pub tick: Option<&'a PassRun>,
}

/// Why a pull request was only reported: nothing was recorded or merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportReason {
    /// No attestation is recorded for the exact head and base.
    Unattested,
    /// A branch writer's forge login is unknown, so the reviewer cannot be
    /// told apart from the writers: the house records show a person wrote
    /// the branch, or the forge links a commit's author or committer to no
    /// account.
    WriterIdentityUnknown,
    /// The pull request has more than
    /// [`MAX_PULL_REQUEST_COMMITS`](crate::integrations::github::MAX_PULL_REQUEST_COMMITS)
    /// commits, or their list exceeds the read budget, so its writers were
    /// not read.
    CommitsOverBound,
    /// The attestation's reviewer is the pull request's author or the
    /// forge login of a commit's author or committer, or the forge does not
    /// name the pull request's author.
    NotIndependent,
    /// The attestation was recorded by a holder or worker that wrote the
    /// branch.
    AttestedByWriter,
    /// The forge does not show the attestation's review: none with its id,
    /// or not by the claimed login, not approved, or not on this head.
    ReviewUnverified,
    /// The house has no standing merge grant for the repository on the
    /// forge.
    NoMergeGrant,
    /// House readiness policy refuses unattended merges here without an
    /// owner's approval for this pull request.
    BelowReadiness,
    /// The verdict is not a merge. Fix requests and hand-overs stay with a
    /// person.
    NotMerge,
    /// The gate task, or an earlier gate task of the pull request that must
    /// settle first, is held by another holder or a pass still running, or
    /// cannot change hands again; or every generation of the task is spent.
    TaskHeld,
    /// The claim on the gate task, or on an earlier gate task of the pull
    /// request, expired without a release; rerun with a takeover.
    TaskUncertain,
    /// A gate task of the pull request spent its retries on exactly this
    /// head and base. The same evidence is not retried; a person decides.
    ExhaustedForSubject,
}

/// Why a recorded merge verdict did not merge in this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotMerged {
    /// The recorded verdict is not a merge, or its effect needs nothing
    /// now: the gate's history at this subject changed it.
    Recorded,
    /// The head or base moved after the verdict; the intent was recorded
    /// as not applied and nothing was submitted.
    Moved,
    /// The forge could not be read again before submitting; nothing was
    /// submitted.
    Unread,
    /// The forge refused the merge.
    Refused,
    /// The merge's outcome is unknown; the next pass reconciles it.
    Uncertain,
    /// The forge accepted the merge but does not read it back as merged at
    /// the head yet.
    Unconfirmed,
    /// An earlier effect of the gate task, or of another gate task of the
    /// pull request, settled or not, has no proven outcome, whatever risk
    /// decision covers it; it is looked up, never repeated.
    Reconciling,
    /// The gate task's attempts are spent, or its retry deadline passed.
    Exhausted,
}

/// What the gate pass did about one pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateResult {
    /// Only reported.
    ReportOnly(ReportReason),
    /// Merged at the judged head and read back as merged.
    Merged,
    /// A merge verdict was recorded but nothing merged.
    NotMerged(NotMerged),
}

/// The verdict on one pull request at one head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateAction {
    /// The pull request.
    pub pull_request: IssueNumber,
    /// The task whose branch it is.
    pub task: TaskId,
    /// The head judged.
    pub head: CommitId,
    /// The verdict.
    pub verdict: Verdict,
    /// What the pass did.
    pub result: GateResult,
}

impl fmt::Display for GateAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "pull request #{} task {} at {}: {:?}, {:?}",
            self.pull_request.get(),
            self.task,
            self.head,
            self.verdict,
            self.result
        )
    }
}

impl<T: GitHubMutationTransport + Clone> GatePass<'_, T> {
    /// Run one pass under the repository's gate lease.
    ///
    /// # Errors
    /// Refuses a repository outside the house before taking the lease.
    /// Returns forge read failures, which stop the pass, and store, gate
    /// store, and authority failures.
    pub fn run(&self) -> Result<Outcome<GateAction>> {
        if !self.house.repositories.contains(self.repository) {
            return Err(RunError::RepositoryOutsideHouse.into());
        }
        let consumer = Pass::Gate.consumer(self.repository)?;
        super::under_lease(self.store, &consumer, self.take_over, self.clock, |lease| {
            let claimant = super::run_claimant()?.under(consumer.clone(), lease);
            self.pass(&claimant, &consumer, lease)
        })
    }

    fn pass(
        &self,
        claimant: &Claimant,
        consumer: &crate::ConsumerId,
        lease: Fence,
    ) -> Result<Vec<GateAction>> {
        let renew = || super::renew(self.store, consumer, lease, self.tick, self.clock);
        let found = kitchen_pull_requests(self.store, self.forge, self.repository, &renew)?;
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let policy = ForgeGatePolicy::for_house(
            self.house,
            self.authors.to_vec(),
            self.house.required_reviewers.iter().cloned().collect(),
        );
        let tasks = self.store.tasks()?;
        let mut run = GateRun::new();
        let mut actions = Vec::new();
        for pull_request in found.into_iter().take(MAX_GATE_PULL_REQUESTS) {
            renew()?;
            super::record(self.store, self.tick, &pull_request.task, self.clock)?;
            actions.push(self.judge(&mut run, claimant, &pull_request, &tasks, &policy)?);
        }
        Ok(actions)
    }

    /// Evaluate one pull request, and merge it when its attested verdict is
    /// a merge the house grants.
    fn judge(
        &self,
        run: &mut GateRun,
        claimant: &Claimant,
        found: &KitchenPullRequest,
        tasks: &[TaskRecord],
        policy: &ForgeGatePolicy,
    ) -> Result<GateAction> {
        let number = found.pull_request.number;
        let mut evidence = collect_forge_evidence(
            self.forge,
            self.store.house(),
            self.repository,
            number,
            policy,
            unattested(),
            self.clock.now(),
        )?;
        // A repair round on the branch, scheduled or a person's, is a
        // writer still working.
        evidence.writer_working = tasks.iter().any(|record| {
            repair_of(record, self.repository).is_some_and(|(pull, _)| pull == number)
                && !matches!(record.state(), TaskState::Settled { .. })
        });
        let head = evidence.head.clone();
        let action = |verdict: Verdict, result: GateResult| GateAction {
            pull_request: number,
            task: found.task.clone(),
            head: head.clone(),
            verdict,
            result,
        };
        let report = |evidence: &GateEvidence, grants: GateGrants, reason| {
            action(
                evaluate(evidence, grants, GateHistory::default()).verdict,
                GateResult::ReportOnly(reason),
            )
        };
        let Some(attested) = gate_attestation(
            self.store,
            self.repository,
            number,
            &evidence.head,
            &evidence.base,
        )?
        else {
            return Ok(report(
                &evidence,
                GateGrants::default(),
                ReportReason::Unattested,
            ));
        };
        let author = found
            .pull_request
            .user
            .as_ref()
            .map(|user| user.login.as_str());
        let recorded_writers =
            attestation::BranchWriters::of(tasks, self.repository, number, &found.branch);
        // A writer of this branch is refused here whatever branch the
        // record-time check saw. Review claims are compared below.
        if recorded_writers.includes(attested.recorded_by.as_str()) {
            return Ok(report(
                &evidence,
                GateGrants::default(),
                ReportReason::AttestedByWriter,
            ));
        }
        let same_principal = attested
            .recorded_by
            .as_str()
            .eq_ignore_ascii_case(&attested.attestation.forge_review.reviewer);
        let attested = attested.attestation;
        // A person's branch session has no reliable forge login binding.
        if recorded_writers.person() {
            return Ok(report(
                &evidence,
                GateGrants::default(),
                ReportReason::WriterIdentityUnknown,
            ));
        }
        // The writers are whoever the forge attributes the commits to: a
        // worker can push with credentials of its own, so the house records
        // do not say which login pushed.
        let commits = match self.forge.pull_request_commits(
            self.store.house(),
            self.repository,
            number,
            &evidence.head,
        ) {
            Observation::Unavailable(IntegrationError::LimitExceeded) => {
                return Ok(report(
                    &evidence,
                    GateGrants::default(),
                    ReportReason::CommitsOverBound,
                ));
            }
            read => known(read)?,
        };
        let Some(writers) = attestation::commit_logins(&commits) else {
            return Ok(report(
                &evidence,
                GateGrants::default(),
                ReportReason::WriterIdentityUnknown,
            ));
        };
        if !attestation::independent(&attested.forge_review.reviewer, author, &writers) {
            return Ok(report(
                &evidence,
                GateGrants::default(),
                ReportReason::NotIndependent,
            ));
        }
        // The record proves nothing by itself: the forge must show the
        // review it names, by that login, approved at this exact head.
        let reviews = known(
            self.forge
                .reviews(self.store.house(), self.repository, number),
        )?;
        if !reviews.iter().any(|review| {
            attestation::review_verified(
                std::slice::from_ref(review),
                &attested.forge_review,
                &evidence.head,
            ) && attestation::claims_match(review, &attested)
        })
            // Markers written by the public low-level API must name the
            // review author as their recorded principal too.
            || !same_principal
        {
            return Ok(report(
                &evidence,
                GateGrants::default(),
                ReportReason::ReviewUnverified,
            ));
        }
        let source = review_source(&attested.forge_review)?;
        attest(&mut evidence, &attested, source.clone());
        let subject = MergeSubject {
            repository: self.repository.clone(),
            number,
            head: evidence.head.clone(),
            base: evidence.base.clone(),
        };
        let merge = match MergeGrant::resolve(
            &self.house.issue_authority(&[], &[])?,
            &subject,
            self.forge_backend,
        ) {
            Ok(merge) => merge,
            Err(HouseError::BelowReadiness { .. }) => {
                return Ok(report(
                    &evidence,
                    GateGrants::default(),
                    ReportReason::BelowReadiness,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let grants = GateGrants {
            merge: merge.clone(),
            fix_request: FixGrant::none(),
            review_triggers: ReviewTriggers::none(),
        };
        // History only turns a merge into a skip, so a verdict that is not a
        // merge without history is never recorded here.
        let predicted = evaluate(&evidence, grants.clone(), GateHistory::default()).verdict;
        match &predicted {
            Verdict::Merge => {}
            Verdict::HandOver { gaps } if gaps.as_slice() == [Gap::MergeGrant] => {
                return Ok(action(
                    predicted,
                    GateResult::ReportOnly(ReportReason::NoMergeGrant),
                ));
            }
            Verdict::Skip | Verdict::HandOver { .. } | Verdict::FixRequest { .. } => {
                return Ok(action(
                    predicted,
                    GateResult::ReportOnly(ReportReason::NotMerge),
                ));
            }
        }
        let budget_start = self.subject_budget_start(claimant, tasks, &evidence)?;
        let Some(spec) = self.gate_task(tasks, &evidence)? else {
            return Ok(action(
                predicted,
                GateResult::ReportOnly(ReportReason::TaskHeld),
            ));
        };
        let task = spec.id.clone();
        // Another gate task of the pull request, under an earlier
        // specification or generation, may have sent a merge whose outcome
        // is not proven. Settled or not, it bars this one; an unsettled task
        // is also reconciled and settled before this one takes its place.
        for earlier in tasks.iter().filter(|record| {
            gate_of(record, self.repository) == Some(number) && record.spec().id != task
        }) {
            let standing = match earlier.state() {
                TaskState::Settled { .. } if self.unproven_settled(earlier, &merge)? => {
                    Some(GateResult::NotMerged(NotMerged::Reconciling))
                }
                TaskState::Settled { settlement, .. } => (*settlement == Settlement::Exhausted
                    && earlier.evidence().subject().is_some_and(|judged| {
                        judged.head == evidence.head && judged.base.as_ref() == Some(&evidence.base)
                    }))
                .then_some(GateResult::ReportOnly(ReportReason::ExhaustedForSubject)),
                TaskState::Open | TaskState::Claimed { .. } => {
                    self.retire(earlier, claimant, &merge, budget_start, &evidence)?
                }
            };
            if let Some(standing) = standing {
                return Ok(action(predicted, standing));
            }
        }
        // A legacy head returning after its deadline has no task at this
        // subject to finish. Do not create a fresh generation for it.
        if past_deadline(budget_start, self.clock.now())
            && !tasks.iter().any(|record| {
                record.spec().id == task
                    && matches!(record.state(), TaskState::Open | TaskState::Claimed { .. })
            })
        {
            return Ok(action(
                predicted,
                GateResult::ReportOnly(ReportReason::ExhaustedForSubject),
            ));
        }
        super::record(self.store, self.tick, &task, self.clock)?;
        match self.store.create_task(spec, claimant, self.clock.now()) {
            Ok(_) => {}
            // The id carries the specification's digest, so only a digest
            // collision gets here.
            Err(crate::Error::State(StateError::TaskConflict(_))) => {
                return Ok(action(
                    predicted,
                    GateResult::ReportOnly(ReportReason::TaskHeld),
                ));
            }
            Err(error) => return Err(error),
        }
        let fence = match take_for_pass(
            self.store,
            &task,
            claimant,
            self.take_over,
            self.clock.now(),
        )? {
            Ok(fence) => fence,
            Err(refusal) => {
                return Ok(action(predicted, GateResult::ReportOnly(refused(refusal))));
            }
        };
        let merged = self.merge(
            run,
            &Owned {
                task: &task,
                fence,
                claimant,
                budget_start,
            },
            &evidence,
            grants,
            &merge,
            &source,
        );
        // The task goes back unless it settled, so the next pass adopts it
        // and reconciles anything left unresolved.
        if matches!(
            self.store.task(&task)?.state(),
            TaskState::Claimed { lease } if lease.fence() == fence
        ) {
            self.store.relinquish(&task, fence, self.clock.now())?;
        }
        let (verdict, merge) = merged?;
        Ok(action(
            verdict,
            match merge {
                Merge::Done => GateResult::Merged,
                Merge::Not(reason) => GateResult::NotMerged(reason),
            },
        ))
    }

    /// Record the merge verdict under the gate task and submit its intent
    /// once the forge still shows the judged head and base.
    fn merge(
        &self,
        run: &mut GateRun,
        owned: &Owned<'_>,
        evidence: &GateEvidence,
        grants: GateGrants,
        merge: &MergeGrant,
        source: &ExternalRef,
    ) -> Result<(Verdict, Merge)> {
        let (task, fence) = (owned.task, owned.fence);
        let now = self.clock.now();
        let executor = self.executor(merge);
        if self.unproven(&executor, task, fence)? {
            return Ok((Verdict::Merge, Merge::Not(NotMerged::Reconciling)));
        }
        // Every pass that does not merge relinquishes the task, which
        // interrupts its attempt. That attempt is continued: a new one per
        // pass would spend the task's attempts on passes that submitted
        // nothing.
        let attempt = match self.store.continue_attempt(task, fence, now)? {
            Some(attempt) => attempt,
            None => match self.store.start_attempt(task, fence, now)? {
                AttemptStart::Started(attempt) | AttemptStart::AlreadyRunning(attempt) => attempt,
                AttemptStart::Exhausted => {
                    return Ok((Verdict::Merge, Merge::Not(NotMerged::Exhausted)));
                }
            },
        };
        // The verdict's effect is admitted only while the task's evidence is
        // at exactly this head and base. It is recorded once per subject, so
        // repeated passes do not fill the evidence log.
        let subject = EvidenceSubject {
            head: evidence.head.clone(),
            base: Some(evidence.base.clone()),
        };
        if self.store.task(task)?.evidence().subject() != Some(&subject) {
            self.store.record_evidence(
                task,
                fence,
                Evidence {
                    kind: EvidenceKind::Check,
                    verdict: EvidenceVerdict::Pass,
                    subject,
                    source: source.clone(),
                    observed_at: now,
                },
                now,
            )?;
        }
        // The store checks the retry deadline only when an attempt starts.
        // A continued attempt past it ends here, before any new verdict or
        // intent, which settles the task as exhausted. Its evidence names
        // the subject it was exhausted on.
        if past_deadline(owned.budget_start, now) {
            self.store
                .finish_attempt_exhausted(task, fence, attempt, now)?;
            return Ok((Verdict::Merge, Merge::Not(NotMerged::Exhausted)));
        }
        let house_grants = super::standing_grants(self.house)?;
        let mut markers = HouseGateStore {
            store: self.store,
            task: task.clone(),
            fence,
            claimant: owned.claimant.clone(),
            grants: &house_grants,
            merge,
            backend: &executor,
            requester: self.forge.scope().requester().clone(),
            posting_budget: self.forge.scope().budget(),
            workers: None,
        };
        let Some(recorded) = run
            .evaluate_next(&mut markers, evidence, grants, GateMode::Active, now)
            .map_err(gate_error)?
        else {
            return Ok((Verdict::Merge, Merge::Not(NotMerged::Recorded)));
        };
        let verdict = recorded.decision.verdict.clone();
        let key = match (&verdict, &recorded.admission) {
            (Verdict::Merge, Admission::Submit(key)) => key.clone(),
            (_, Admission::Reconcile(_)) => {
                return Ok((verdict, Merge::Not(NotMerged::Reconciling)));
            }
            // Only a merge verdict is submitted here. Any other admitted
            // effect, such as a hand-over after a merge landed elsewhere, is
            // recorded as not applied: it stays with a person.
            (_, Admission::Submit(key)) => {
                self.abandon(task, fence, key)?;
                return Ok((verdict, Merge::Not(NotMerged::Recorded)));
            }
            (_, Admission::None | Admission::Satisfied) => {
                return Ok((verdict, Merge::Not(NotMerged::Recorded)));
            }
        };
        let request = match run.next_merge(&recorded, merge, self.forge) {
            Ok(request) => request,
            Err(error) => {
                self.abandon(task, fence, &key)?;
                let reason = match error {
                    IntegrationError::StaleDecision | IntegrationError::ScopeMismatch => {
                        NotMerged::Moved
                    }
                    IntegrationError::Unknown
                    | IntegrationError::Unavailable
                    | IntegrationError::Timeout
                    | IntegrationError::NotFound
                    | IntegrationError::InvalidInput
                    | IntegrationError::LimitExceeded
                    | IntegrationError::PermissionDenied
                    | IntegrationError::BudgetExhausted => NotMerged::Unread,
                };
                return Ok((verdict, Merge::Not(reason)));
            }
        };
        let state = self.submit(&executor, task, fence, &key, &request)?;
        Ok((
            verdict,
            match state {
                EffectState::Applied { .. } => match run.confirm_merge(&request, self.forge) {
                    Ok(()) => {
                        self.store.finish_attempt(
                            task,
                            fence,
                            attempt,
                            AttemptOutcome::Succeeded,
                            self.clock.now(),
                        )?;
                        Merge::Done
                    }
                    Err(_) => Merge::Not(NotMerged::Unconfirmed),
                },
                EffectState::NotApplied { .. } => Merge::Not(NotMerged::Refused),
                EffectState::Intended
                | EffectState::Uncertain { .. }
                | EffectState::Unresolvable { .. }
                | EffectState::Waived { .. } => Merge::Not(NotMerged::Uncertain),
            },
        ))
    }

    /// Submit the persisted intent `key`, which must be exactly `request`'s
    /// merge, and record what the forge answered.
    fn submit(
        &self,
        executor: &dyn EffectExecutor,
        task: &TaskId,
        fence: Fence,
        key: &IdempotencyKey,
        request: &MergeRequest,
    ) -> Result<EffectState> {
        let record = self.store.task(task)?;
        let intent = record
            .effects()
            .iter()
            .find(|effect| effect.request().key() == key)
            .ok_or(RunError::GateRecords)?;
        let persisted = match intent.request().effect() {
            crate::contracts::Effect::GitHub(github) => &github.mutation,
            crate::contracts::Effect::Worker(_)
            | crate::contracts::Effect::Roger(_)
            | crate::contracts::Effect::Schedule(_) => return Err(RunError::GateRecords.into()),
        };
        if &request.key != key || persisted != &request.mutation() {
            return Err(RunError::GateRecords.into());
        }
        let outcome = match executor.execute(intent.request()) {
            Ok(receipt) => EffectOutcome::Applied(receipt),
            Err(EffectFailure::NotApplied(reason)) => EffectOutcome::NotApplied(reason),
            Err(EffectFailure::Uncertain(reason)) => EffectOutcome::Uncertain(reason),
        };
        Ok(self
            .store
            .record_submission_outcome(
                task,
                fence,
                intent.seq(),
                intent.submissions(),
                outcome,
                self.clock.now(),
            )?
            .state()
            .clone())
    }

    /// Record the never-submitted intent `key` as not applied, so the task
    /// is not blocked by it and the gate counts it as refused.
    fn abandon(&self, task: &TaskId, fence: Fence, key: &IdempotencyKey) -> Result<()> {
        let record = self.store.task(task)?;
        let intent = record
            .effects()
            .iter()
            .find(|effect| effect.request().key() == key)
            .ok_or(RunError::GateRecords)?;
        self.store.record_effect_outcome(
            task,
            fence,
            intent.seq(),
            EffectOutcome::NotApplied(NotAppliedReason::ConfirmedAbsent),
            self.clock.now(),
        )?;
        Ok(())
    }

    /// The forge executor merges run on, holding `merge`.
    fn executor(&self, merge: &MergeGrant) -> GitHubExecutor<T> {
        GitHubExecutor::new(
            self.forge_backend.clone(),
            self.forge.scope().clone(),
            self.forge.transport().clone(),
            ReadLimits::default(),
        )
        .with_merge_grant(merge.clone())
    }

    /// Reconcile the effects of gate task `task`, held under `fence`, and
    /// say whether any still lacks a proven outcome. An effect under a risk
    /// decision counts: the decision is about its task, and the request may
    /// still land. Such an effect is looked up too, and only a conclusive
    /// answer is recorded.
    fn unproven(&self, executor: &dyn EffectExecutor, task: &TaskId, fence: Fence) -> Result<bool> {
        let reconciled = reconcile(self.store, executor, task, fence, self.clock)?;
        if !reconciled.unresolved.is_empty() || !reconciled.foreign.is_empty() {
            return Ok(true);
        }
        // What `reconcile` leaves unproven is waived under a current
        // decision, which it does not look up.
        let descriptor = executor.descriptor();
        let mut unproven = false;
        for effect in self.store.task(task)?.effects() {
            if effect.state().is_resolved() {
                continue;
            }
            let request = effect.request();
            let found = if request.backend() == &descriptor.backend
                && descriptor.supports_lookup(request.effect())
            {
                executor.lookup(request)
            } else {
                Ok(Lookup::Unknown)
            };
            let outcome = match found {
                Ok(Lookup::Applied(receipt)) => EffectOutcome::Applied(receipt),
                Ok(Lookup::Absent) => EffectOutcome::NotApplied(NotAppliedReason::ConfirmedAbsent),
                Ok(Lookup::Unknown) | Err(_) => {
                    unproven = true;
                    continue;
                }
            };
            let recorded = self.store.record_submission_outcome(
                task,
                fence,
                effect.seq(),
                effect.submissions(),
                outcome,
                self.clock.now(),
            )?;
            unproven |= !recorded.state().is_resolved();
        }
        Ok(unproven)
    }

    /// Whether `settled`, a settled gate task of the pull request, has an
    /// effect the forge still cannot prove applied or absent. Each such
    /// effect is looked up again, and a conclusive answer is recorded.
    fn unproven_settled(&self, settled: &TaskRecord, merge: &MergeGrant) -> Result<bool> {
        if settled
            .effects()
            .iter()
            .all(|effect| effect.state().is_resolved())
        {
            return Ok(false);
        }
        let reread = reread_settled(
            self.store,
            &self.executor(merge),
            &settled.spec().id,
            self.clock,
        )?;
        Ok(!reread.unresolved.is_empty() || !reread.foreign.is_empty())
    }

    /// Reconcile and settle `stale`, an earlier gate task of the pull
    /// request. `Some` says why it still stands; no task replaces it until
    /// it settled, and it never settles with an effect whose outcome is not
    /// proven.
    fn retire(
        &self,
        stale: &TaskRecord,
        claimant: &Claimant,
        merge: &MergeGrant,
        budget_start: Timestamp,
        evidence: &GateEvidence,
    ) -> Result<Option<GateResult>> {
        // Taking it would fail on its full ownership history and stop the
        // pass; a person settles it.
        if !room(stale, PASS_EVENTS) {
            return Ok(Some(GateResult::ReportOnly(ReportReason::TaskHeld)));
        }
        let expired_here = past_deadline(budget_start, self.clock.now())
            && stale.evidence().subject().is_some_and(|subject| {
                subject.head == evidence.head && subject.base.as_ref() == Some(&evidence.base)
            });
        let stale = &stale.spec().id;
        super::record(self.store, self.tick, stale, self.clock)?;
        let fence = match take_for_pass(
            self.store,
            stale,
            claimant,
            self.take_over,
            self.clock.now(),
        )? {
            Ok(fence) => fence,
            Err(refusal) => return Ok(Some(GateResult::ReportOnly(refused(refusal)))),
        };
        let outcome = self
            .unproven(&self.executor(merge), stale, fence)
            .and_then(|unproven| {
                if unproven {
                    return Ok(Some(GateResult::NotMerged(NotMerged::Reconciling)));
                }
                if expired_here {
                    let attempt =
                        match self
                            .store
                            .continue_attempt(stale, fence, self.clock.now())?
                        {
                            Some(attempt) => Some(attempt),
                            None => {
                                match self.store.start_attempt(stale, fence, self.clock.now())? {
                                    AttemptStart::Started(attempt)
                                    | AttemptStart::AlreadyRunning(attempt) => Some(attempt),
                                    AttemptStart::Exhausted => None,
                                }
                            }
                        };
                    if let Some(attempt) = attempt {
                        self.store.finish_attempt_exhausted(
                            stale,
                            fence,
                            attempt,
                            self.clock.now(),
                        )?;
                    }
                    return Ok(Some(GateResult::ReportOnly(
                        ReportReason::ExhaustedForSubject,
                    )));
                }
                match self.store.settle_cancelled(stale, fence, self.clock.now()) {
                    Ok(()) => Ok(None),
                    Err(crate::Error::State(StateError::UnresolvedEffects { .. })) => {
                        Ok(Some(GateResult::NotMerged(NotMerged::Reconciling)))
                    }
                    Err(error) => Err(error),
                }
            });
        // A task that still stands goes back for the next pass.
        if matches!(self.store.task(stale)?.state(), TaskState::Claimed { .. }) {
            self.store.relinquish(stale, fence, self.clock.now())?;
        }
        outcome
    }

    /// The specification of the gate task this pass uses for pull request
    /// `number`: the first generation whose task does not exist yet, or is
    /// unsettled with room in its ownership history for this pass and for
    /// settling it later. A task that settled, such as the one of an
    /// earlier specification the house returned to, is never reused.
    /// `None` when every generation is spent.
    fn gate_task(&self, tasks: &[TaskRecord], evidence: &GateEvidence) -> Result<Option<TaskSpec>> {
        for generation in 0..=u8::MAX {
            let spec = self.gate_spec(evidence.number, generation)?;
            let usable = tasks
                .iter()
                .find(|record| record.spec().id == spec.id)
                .is_none_or(|existing| {
                    !matches!(existing.state(), TaskState::Settled { .. })
                        && room(existing, 2 * PASS_EVENTS)
                        && existing.evidence().subject().is_none_or(|subject| {
                            subject.head == evidence.head
                                && subject.base.as_ref() == Some(&evidence.base)
                        })
                });
            if usable {
                return Ok(Some(spec));
            }
        }
        Ok(None)
    }

    /// Generation `generation` of the gate task of one pull request: the
    /// house's standing grants delegated whole, and no worker. Its id
    /// carries a digest of the rest of the specification and the
    /// generation, so a changed specification is another task and never
    /// conflicts with the one an earlier pass created.
    fn gate_spec(&self, number: IssueNumber, generation: u8) -> Result<TaskSpec> {
        let grants = super::standing_grants(self.house)?;
        let mut spec = TaskSpec {
            id: derived_task_id(GATE_TASK, self.repository, number)?,
            role: Role::Expediter,
            repository: Some(self.repository.clone()),
            authority: TaskAuthority::delegate(&grants, self.house.grants.iter().cloned())?,
            retry: RetryPolicy::new(super::ATTEMPTS, super::RETRY_BUDGET)?,
            provenance: self.provenance.clone(),
            resources: std::collections::BTreeSet::new(),
            requires: CapabilityRequirements::new(),
            agent: None,
            work_type: None,
        };
        // A specification always encodes; a failure is refused like any
        // other task the gate could not build.
        let mut encoded = serde_json::to_vec(&spec).map_err(|_| RunError::GateRefused)?;
        encoded.push(generation);
        spec.id = derived_task_id(
            &format!("{GATE_TASK}{:016x}", stable_hash(&encoded)),
            self.repository,
            number,
        )?;
        Ok(spec)
    }

    /// Persist the first time this exact PR, head, and base entered the gate.
    /// A pre-marker task may have judged several heads while keeping one
    /// attempt. Its first attempt bounds every unmarked subject of this PR.
    fn subject_budget_start(
        &self,
        claimant: &Claimant,
        tasks: &[TaskRecord],
        evidence: &GateEvidence,
    ) -> Result<Timestamp> {
        let subject = EvidenceSubject {
            head: evidence.head.clone(),
            base: Some(evidence.base.clone()),
        };
        let key = self.budget_key(evidence.number, subject.clone())?;
        let schema = MarkerSchema::new(
            GATE_BUDGET_SCHEMA,
            NonZeroU32::new(1).ok_or(StateError::MarkerSchemaInvalid)?,
        )?;
        if let Some(marker) = self.store.marker(&key)? {
            let saved: SubjectBudget = marker.fact().decode(&schema)?;
            return Ok(saved.started_at);
        }
        let now = self.clock.now();
        let started_at = tasks
            .iter()
            .filter(|record| gate_of(record, self.repository) == Some(evidence.number))
            .map(|record| -> Result<Option<Timestamp>> {
                let Some(first) = record
                    .attempts()
                    .first()
                    .map(|attempt| attempt.started_at())
                else {
                    return Ok(None);
                };
                let Some(recorded) = record.evidence().subject() else {
                    return Ok(None);
                };
                let marker = self
                    .store
                    .marker(&self.budget_key(evidence.number, recorded.clone())?)?;
                let legacy = marker.is_none_or(|marker| first < marker.recorded_at());
                Ok((legacy || recorded == &subject).then_some(first))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now);
        let fact = MarkerFact::workflow(schema, &SubjectBudget { started_at })?;
        self.store.record_marker(key, fact, claimant, now)?;
        Ok(started_at)
    }

    fn budget_key(&self, number: IssueNumber, subject: EvidenceSubject) -> Result<MarkerKey> {
        Ok(MarkerKey {
            workflow: WorkflowId::new(GATE_BUDGET_WORKFLOW)?,
            item: WorkItem::PullRequest {
                repository: self.repository.clone(),
                number: NonZeroU64::new(number.get()).ok_or(ContractError::InvalidValue {
                    kind: ValueKind::Text,
                })?,
            },
            subject: MarkerSubject::Git(subject),
        })
    }
}

/// The kind every gate task id starts with.
const GATE_TASK: &str = "gate";
const GATE_BUDGET_WORKFLOW: &str = "merge-gate-budget";
const GATE_BUDGET_SCHEMA: &str = "gate.subject-budget";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubjectBudget {
    started_at: Timestamp,
}

/// Ownership events one pass may add to a gate task: a claim, or a
/// relinquish and a claim when it moves the task off an ended pass, and the
/// relinquish or release that ends its hold.
const PASS_EVENTS: usize = 3;

/// Whether `record`'s ownership history has room for `events` more. Every
/// pass that holds a gate task adds to it, and a full history refuses the
/// next claim.
fn room(record: &TaskRecord, events: usize) -> bool {
    record.ownership().len().saturating_add(events) <= MAX_OWNERSHIP_HISTORY
}

/// Whether `record`'s retry deadline passed at `now`: its first attempt
/// started longer ago than its retry policy allows, the rule the store
/// applies when an attempt starts.
fn past_deadline(started_at: Timestamp, now: Timestamp) -> bool {
    now.saturating_since(started_at) > super::RETRY_BUDGET
}

/// The pull request a gate task of `repository` was created for, under any
/// specification and generation, confirmed by deriving the task id again;
/// `None` for another kind of task.
fn gate_of(record: &TaskRecord, repository: &Repository) -> Option<IssueNumber> {
    let spec = record.spec();
    if spec.role != Role::Expediter || spec.repository.as_ref() != Some(repository) {
        return None;
    }
    let (kind, rest) = spec.id.as_str().split_once('-')?;
    let (_, number) = rest.rsplit_once('-')?;
    let number = IssueNumber::new(number.parse().ok()?).ok()?;
    (kind.starts_with(GATE_TASK)
        && derived_task_id(kind, repository, number).ok().as_ref() == Some(&spec.id))
    .then_some(number)
}

/// Why a pass that could not take a gate task only reports.
const fn refused(refusal: Refusal) -> ReportReason {
    match refusal {
        Refusal::Held => ReportReason::TaskHeld,
        Refusal::Uncertain => ReportReason::TaskUncertain,
    }
}

/// The gate task this pass holds.
struct Owned<'a> {
    task: &'a TaskId,
    fence: Fence,
    claimant: &'a Claimant,
    budget_start: Timestamp,
}

/// What submitting a recorded merge verdict came to.
#[derive(Debug, Clone, Copy)]
enum Merge {
    Done,
    Not(NotMerged),
}

/// The attested facts, applied to forge evidence of the same subject. The
/// reviewer's independence was checked by the caller.
fn attest(evidence: &mut GateEvidence, attested: &super::GateAttestation, source: ExternalRef) {
    evidence.semantic_review = attested.review.clone();
    evidence.semantic_source = Some(source);
    evidence.semantic_head = Some(attested.head.clone());
    evidence.semantic_base = Some(attested.base.clone());
    evidence.semantic_read_only = attested.read_only;
    evidence.semantic_independent = true;
    evidence.acceptance_met = Some(attested.acceptance_met);
    evidence.hardware_complete = Some(attested.hardware_complete);
    evidence.risk_classes = Some(attested.risk_classes.clone());
    evidence.supporting_subject = Some((attested.head.clone(), attested.base.clone()));
}

/// Where the attestation's forge review is read: the pull request review
/// with its id.
fn review_source(review: &super::ForgeReview) -> Result<ExternalRef> {
    Ok(ExternalRef::new(&format!(
        "pull-request-review-{}",
        review.id
    ))?)
}

/// A gate store failure as the pass's error.
fn gate_error(error: GateStoreError) -> crate::Error {
    match error {
        GateStoreError::Kitchen(error) => error,
        GateStoreError::UnknownEffect | GateStoreError::HistoryIncomplete => {
            RunError::GateRecords.into()
        }
        GateStoreError::SubjectNotRecorded
        | GateStoreError::NoHouseStoreEffect
        | GateStoreError::NoWorkerBackend
        | GateStoreError::MissingHeadBranch
        | GateStoreError::WorkerUnavailable(_)
        | GateStoreError::WorkerObservation(_)
        | GateStoreError::MergeNotGranted
        | GateStoreError::MissingBaseBranch => RunError::GateRefused.into(),
    }
}

/// No independent review, acceptance, hardware, or risk evidence yet: the
/// forge evidence is collected first, and a recorded attestation for its
/// exact head and base fills these in.
fn unattested() -> GateSupplement {
    GateSupplement {
        semantic_review: SemanticReview::Unavailable,
        semantic_source: None,
        verified_findings: Vec::new(),
        disproved_findings: Vec::new(),
        semantic_head: None,
        semantic_base: None,
        semantic_read_only: false,
        semantic_independent: false,
        acceptance_met: None,
        hardware_complete: None,
        risk_classes: None,
        risk_approval: None,
        writer_working: false,
        subject: None,
    }
}
