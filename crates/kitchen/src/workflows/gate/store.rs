//! Durable [`GateMarkerStore`] over the house store.
//!
//! Verdicts are `gate.verdict/1` workflow markers keyed by the gate workflow,
//! the pull request, and its exact head and base. Effect intents are ordinary
//! house-store effects, persisted before the marker that references them.
//!
//! Ownership: new intents are persisted under the caller's claimed task and
//! fence, which needs a running attempt whose current evidence subject is the
//! verdict's exact head and base. The adapter refuses any other subject
//! before persisting anything. Effect identity is house-wide: before creating
//! an intent, the adapter looks for the same logical effect (name, repository,
//! and PR) in every task of the house, so a restart under another task or
//! attempt finds the earlier intent instead of submitting again.
//!
//! Fix delivery: a fix verdict messages the branch's worker, found as the
//! worker of the newest applied launch that created exactly the PR's head
//! branch, when the worker backend observes it live. The house store admits
//! that message only when the gate task owns the worker (it launched it, or
//! the worker was given to it); otherwise nothing is written. With no live
//! worker, the gate launches one on exactly that branch. A worker a person
//! took over, or whose state is unknown, receives nothing. Before persisting a
//! fix delivery, the owning task must hold [`Permission::PushBranch`] for the
//! repository on the forge backend; worker permissions alone do not suffice.

use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
};

use crate::{
    BackendId, EffectName, HouseId, IdentifierError, TaskId, WorkflowId,
    contracts::{
        BackendUnavailable, BranchName, Claimant, CommitId, ContractError, Effect, EffectExecutor,
        EvidenceSubject, ExternalRef, Fence, GitHubAction, GitHubEffect, GrantScope, HouseGrants,
        IdempotencyKey, IssueNumber, Operation, Permission, PostingBudget, Repository,
        ResourceKind, ResourceRef, Role, Timestamp, ValueKind, WorkerBackend, WorkerState,
        Workspace,
    },
    selection::AgentSelection,
    state::{
        EffectPlan, EffectRecord, EffectStart, EffectState, HouseStore, MarkerFact, MarkerKey,
        MarkerRecording, MarkerSchema, MarkerSubject, StateError, TaskRecord, WorkItem,
    },
};

use super::{
    GATE_VERDICT_SCHEMA, GATE_VERDICT_VERSION, Gap, GateDecision, GateEffectState, GateHistory,
    GateIntent, GateMarkerStore, GateVerdictRecord, MAX_BASE_READ_FAILURES, MergeGrant, Verdict,
    fix_brief, fix_marker, handover_comment, merge_mutation,
};

/// Workflow id under which gate verdict markers are recorded.
pub const GATE_WORKFLOW: &str = "merge-gate";
/// Workflow id under which failed base tip reads are counted per PR head,
/// apart from verdicts so verdict history reads only verdicts.
pub const GATE_BASE_READ_WORKFLOW: &str = "merge-gate-base-read";
const BASE_READ_SCHEMA: &str = "gate.base-read-failures";

/// Payload of the `gate.base-read-failures/1` marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaseReadFailures {
    failures: u8,
}

/// Why the durable gate store refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GateStoreError {
    /// The house store, a contract check, or an identifier failed.
    #[error(transparent)]
    Kitchen(#[from] crate::Error),
    /// The owning task's current evidence is not the verdict's head and base.
    #[error("the task's evidence is not at the verdict's head and base")]
    SubjectNotRecorded,
    /// The verdict has no effect this store can persist.
    #[error("the verdict has no house-store effect")]
    NoHouseStoreEffect,
    /// A fix verdict needs a worker backend and none was supplied.
    #[error("no worker backend for fix delivery")]
    NoWorkerBackend,
    /// A fix verdict lacks a valid head branch.
    #[error("a fix verdict lacks its head branch")]
    MissingHeadBranch,
    /// The branch's worker cannot receive a request now: a person took it
    /// over, or the backend cannot tell its state.
    #[error("the branch's worker cannot receive a request: {0:?}")]
    WorkerUnavailable(WorkerState),
    /// The worker backend could not be queried.
    #[error(transparent)]
    WorkerObservation(#[from] BackendUnavailable),
    /// A merge verdict is not covered by the readiness-checked merge grant.
    #[error("no readiness-checked merge grant covers this pull request and revision")]
    MergeNotGranted,
    /// A merge verdict lacks its validated base branch.
    #[error("a merge verdict lacks its base branch")]
    MissingBaseBranch,
    /// A marker names an effect key the house store does not hold.
    #[error("a gate marker names an unknown effect")]
    UnknownEffect,
    /// A marker's bounded history dropped facts, so round counts are unknown.
    #[error("gate marker history is incomplete")]
    HistoryIncomplete,
}

impl From<StateError> for GateStoreError {
    fn from(error: StateError) -> Self {
        Self::Kitchen(error.into())
    }
}

impl From<ContractError> for GateStoreError {
    fn from(error: ContractError) -> Self {
        Self::Kitchen(error.into())
    }
}

impl From<IdentifierError> for GateStoreError {
    fn from(error: IdentifierError) -> Self {
        Self::Kitchen(error.into())
    }
}

/// [`GateMarkerStore`] backed by one house's [`HouseStore`].
pub struct HouseGateStore<'a> {
    /// The house store; its house must be the evidence house.
    pub store: &'a HouseStore,
    /// Claimed task that owns new effect intents.
    pub task: TaskId,
    /// The task claim's fence.
    pub fence: Fence,
    /// Who records markers, under which trigger.
    pub claimant: Claimant,
    /// The house's current grants, checked when an intent is persisted.
    pub grants: &'a HouseGrants,
    /// The readiness-checked merge grant; a merge intent it does not cover
    /// is refused before anything is persisted.
    pub merge: &'a MergeGrant,
    /// The forge executor that will perform the effects; intents are checked
    /// against its own descriptor.
    pub backend: &'a dyn EffectExecutor,
    /// Authenticated forge requester persisted with each intent.
    pub requester: ExternalRef,
    /// The house's per-task forge posting ceiling.
    pub posting_budget: PostingBudget,
    /// The worker backend that delivers fix requests; `None` refuses them.
    pub workers: Option<&'a dyn WorkerBackend>,
}

impl fmt::Debug for HouseGateStore<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HouseGateStore")
            .field("store", &self.store)
            .field("task", &self.task)
            .field("fence", &self.fence)
            .field("claimant", &self.claimant)
            .field("backend", &self.backend.descriptor().backend)
            .field(
                "workers",
                &self.workers.map(|workers| &workers.descriptor().backend),
            )
            .finish_non_exhaustive()
    }
}

impl HouseGateStore<'_> {
    fn check_house(&self, house: &HouseId) -> Result<(), GateStoreError> {
        if house == self.store.house() {
            Ok(())
        } else {
            Err(ContractError::CrossHouse {
                expected: self.store.house().clone(),
                found: house.clone(),
            }
            .into())
        }
    }

    fn marker_key(
        repository: &Repository,
        number: IssueNumber,
        head: &CommitId,
        base: &CommitId,
    ) -> Result<MarkerKey, GateStoreError> {
        Ok(MarkerKey {
            workflow: WorkflowId::new(GATE_WORKFLOW)?,
            item: pull_request(repository, number)?,
            subject: MarkerSubject::Git(EvidenceSubject {
                head: head.clone(),
                base: Some(base.clone()),
            }),
        })
    }

    fn effect(
        &self,
        tasks: &[TaskRecord],
        agent: Option<AgentSelection>,
        record: &GateVerdictRecord,
        decision: &GateDecision,
    ) -> Result<Effect, GateStoreError> {
        let mutation = match &record.verdict {
            Verdict::Merge => {
                if !self.merge.covers(
                    &record.house,
                    &record.repository,
                    record.number,
                    &record.head,
                    &record.base,
                ) {
                    return Err(GateStoreError::MergeNotGranted);
                }
                merge_mutation(
                    &record.repository,
                    record.number,
                    &record.head,
                    &record.base,
                    decision
                        .base_branch
                        .as_ref()
                        .ok_or(GateStoreError::MissingBaseBranch)?,
                )
            }
            Verdict::HandOver { gaps } => handover_comment(
                &record.repository,
                record.number,
                &record.head,
                &record.base,
                gaps,
                &decision.verified_findings,
                decision.merged_elsewhere.as_ref(),
            )?,
            Verdict::FixRequest { gaps } => {
                return self
                    .fix_effect(tasks, agent, decision, gaps)
                    .map(Effect::Worker);
            }
            Verdict::Skip => return Err(GateStoreError::NoHouseStoreEffect),
        };
        Ok(Effect::GitHub(GitHubEffect {
            requester: self.requester.clone(),
            mutation,
            posting_budget: self.posting_budget,
        }))
    }

    /// Deliver a fix to the branch's live worker, or launch one on exactly
    /// the branch when none is live. The house store admits a message only
    /// when the gate task owns the worker: it launched it, or the worker was
    /// given to it.
    fn fix_effect(
        &self,
        tasks: &[TaskRecord],
        agent: Option<AgentSelection>,
        decision: &GateDecision,
        gaps: &[Gap],
    ) -> Result<Operation, GateStoreError> {
        let workers = self.workers.ok_or(GateStoreError::NoWorkerBackend)?;
        let branch = decision
            .head_branch
            .as_deref()
            .and_then(|branch| BranchName::new(branch).ok())
            .ok_or(GateStoreError::MissingHeadBranch)?;
        let brief = fix_brief(decision, gaps, &branch)?;
        let live = match branch_worker(tasks, &workers.descriptor().backend, &branch) {
            None => None,
            Some(worker) => match workers.observe_worker(&worker)? {
                WorkerState::Starting | WorkerState::Ready | WorkerState::AwaitingReply => {
                    Some(worker)
                }
                WorkerState::Settled(_) | WorkerState::Missing => None,
                state @ (WorkerState::UserTakeover | WorkerState::Unknown) => {
                    return Err(GateStoreError::WorkerUnavailable(state));
                }
            },
        };
        Ok(match live {
            Some(worker) => Operation::MessageWorker {
                worker,
                body: brief,
            },
            None => Operation::LaunchWorker {
                role: Role::StationCook,
                workspace: Workspace::Isolated,
                brief,
                branch: Some(branch),
                pinned: None,
                agent,
            },
        })
    }
}

/// The worker of the newest applied launch, in any task of the house, that
/// created exactly `branch` on `backend`.
fn branch_worker(
    tasks: &[TaskRecord],
    backend: &BackendId,
    branch: &BranchName,
) -> Option<ResourceRef> {
    tasks
        .iter()
        .flat_map(TaskRecord::effects)
        .filter_map(|effect| {
            let (
                Effect::Worker(Operation::LaunchWorker { .. }),
                EffectState::Applied { receipt, .. },
            ) = (effect.request().effect(), effect.state())
            else {
                return None;
            };
            Some((effect.intended_at(), receipt.created()))
        })
        .filter(|(_, created)| {
            created.iter().any(|resource| {
                resource.kind == ResourceKind::Branch
                    && &resource.backend == backend
                    && resource.handle.as_str() == branch.as_str()
            })
        })
        .max_by_key(|(at, _)| *at)
        .and_then(|(_, created)| {
            created
                .iter()
                .find(|resource| {
                    resource.kind == ResourceKind::Worker && &resource.backend == backend
                })
                .cloned()
        })
}

fn schema() -> Result<MarkerSchema, StateError> {
    let version = NonZeroU32::new(GATE_VERDICT_VERSION).ok_or(StateError::MarkerSchemaInvalid)?;
    MarkerSchema::new(GATE_VERDICT_SCHEMA, version)
}

fn pull_request(repository: &Repository, number: IssueNumber) -> Result<WorkItem, ContractError> {
    Ok(WorkItem::PullRequest {
        repository: repository.clone(),
        number: NonZeroU64::new(number.get()).ok_or(ContractError::InvalidValue {
            kind: ValueKind::Text,
        })?,
    })
}

/// Logical effect name: kind, round, refusal count, and the subject. Commit
/// prefixes keep the name within the identifier bound; the lookup also
/// matches the repository and PR the effect targets.
fn effect_name(record: &GateVerdictRecord) -> Result<EffectName, GateStoreError> {
    let kind = match record.verdict {
        Verdict::Merge => "merge",
        Verdict::HandOver { .. } => "handover",
        Verdict::FixRequest { .. } => "fix",
        Verdict::Skip => return Err(GateStoreError::NoHouseStoreEffect),
    };
    let prefix = |commit: &CommitId| {
        let text = commit.as_str();
        text.get(..12).unwrap_or(text).to_owned()
    };
    Ok(EffectName::new(&format!(
        "gate-{kind}-{}-{}-{}-{}",
        record.round,
        record.refused,
        prefix(&record.head),
        prefix(&record.base)
    ))?)
}

/// Whether a persisted effect is this record's forge effect on its PR, or
/// its fix delivery, whose brief ends with the subject's marker.
fn targets(effect: &EffectRecord, record: &GateVerdictRecord) -> bool {
    match effect.request().effect() {
        Effect::GitHub(github) => {
            github.mutation.repository == record.repository
                && match &github.mutation.action {
                    GitHubAction::MergePullRequest { number: pr, .. }
                    | GitHubAction::PostComment { issue: pr, .. } => *pr == record.number,
                    GitHubAction::CloseIssue { .. }
                    | GitHubAction::SetLabel { .. }
                    | GitHubAction::CreateIssue { .. }
                    | GitHubAction::LinkSubIssue { .. }
                    | GitHubAction::LinkDependency { .. }
                    | GitHubAction::CreateLabel { .. }
                    | GitHubAction::OpenPullRequest { .. }
                    | GitHubAction::ReviewPullRequest { .. }
                    | GitHubAction::ReplyToReviewThread { .. }
                    | GitHubAction::ResolveReviewThread { .. } => false,
                }
        }
        Effect::Worker(
            Operation::LaunchWorker { brief: text, .. }
            | Operation::MessageWorker { body: text, .. },
        ) => text.as_str().ends_with(&fix_marker(
            &record.repository,
            record.number,
            &record.head,
            &record.base,
        )),
        Effect::Worker(
            Operation::ReplyToWorker { .. }
            | Operation::CancelWorker { .. }
            | Operation::ReleaseResource { .. },
        )
        | Effect::Roger(_)
        | Effect::Schedule(_) => false,
    }
}

/// The latest same-named intent for this record's subject in any task,
/// preferring one that may have applied over a refused one.
fn find_effect<'a>(
    tasks: &'a [TaskRecord],
    name: &EffectName,
    record: &GateVerdictRecord,
) -> Option<&'a EffectRecord> {
    tasks
        .iter()
        .flat_map(TaskRecord::effects)
        .filter(|effect| effect.name() == name && targets(effect, record))
        .max_by_key(|effect| {
            (
                !matches!(effect.state(), EffectState::NotApplied { .. }),
                effect.intended_at(),
            )
        })
}

fn find_key<'a>(tasks: &'a [TaskRecord], key: &IdempotencyKey) -> Option<&'a EffectRecord> {
    tasks
        .iter()
        .flat_map(TaskRecord::effects)
        .find(|effect| effect.request().key() == key)
}

fn gate_state(state: &EffectState) -> GateEffectState {
    match state {
        EffectState::Intended => GateEffectState::Intended,
        EffectState::Uncertain { .. } | EffectState::Ended { .. } => GateEffectState::Uncertain,
        EffectState::Applied { receipt, .. } => match receipt.retarget() {
            Some(retarget) => GateEffectState::AppliedElsewhere(retarget.clone()),
            None => GateEffectState::Applied,
        },
        EffectState::NotApplied { .. } => GateEffectState::NotApplied,
        EffectState::Unresolvable { .. } | EffectState::Waived { .. } => {
            GateEffectState::HandedOver
        }
    }
}

impl GateMarkerStore for HouseGateStore<'_> {
    type Error = GateStoreError;

    fn history(
        &self,
        house: &HouseId,
        repository: &Repository,
        number: IssueNumber,
        head: &CommitId,
        base: &CommitId,
        now: Timestamp,
    ) -> Result<GateHistory, Self::Error> {
        self.check_house(house)?;
        let schema = schema()?;
        let item = pull_request(repository, number)?;
        let mut records = Vec::new();
        for marker in self.store.markers(&WorkflowId::new(GATE_WORKFLOW)?)? {
            if marker.key().item != item {
                continue;
            }
            if marker.dropped_history() > 0 {
                return Err(GateStoreError::HistoryIncomplete);
            }
            let MarkerSubject::Git(subject) = &marker.key().subject else {
                return Err(StateError::MarkerPayloadInvalid.into());
            };
            let current = marker.fact().decode::<GateVerdictRecord>(&schema)?;
            let superseded = marker
                .history()
                .iter()
                .map(|fact| fact.fact.decode::<GateVerdictRecord>(&schema));
            for (record, is_current) in std::iter::once(Ok(current))
                .chain(superseded)
                .zip(std::iter::once(true).chain(std::iter::repeat(false)))
            {
                let record = record?;
                // A payload must describe the subject it is keyed by.
                if &record.house != house
                    || &record.repository != repository
                    || record.number != number
                    || subject.head != record.head
                    || subject.base.as_ref() != Some(&record.base)
                {
                    return Err(StateError::MarkerPayloadInvalid.into());
                }
                records.push((record, is_current));
            }
        }
        let tasks = self.store.tasks()?;
        let mut counted = Vec::with_capacity(records.len());
        for (record, is_current) in &records {
            let refused = match &record.effect {
                None => false,
                Some(key) => {
                    let effect = find_key(&tasks, key).ok_or(GateStoreError::UnknownEffect)?;
                    gate_state(effect.state()) == GateEffectState::NotApplied
                }
            };
            if !refused {
                counted.push((record, *is_current));
            }
        }
        Ok(GateHistory::from_records(counted, head, base, now))
    }

    fn current(
        &self,
        house: &HouseId,
        repository: &Repository,
        number: IssueNumber,
        head: &CommitId,
        base: &CommitId,
    ) -> Result<Option<GateVerdictRecord>, Self::Error> {
        self.check_house(house)?;
        let schema = schema()?;
        let key = Self::marker_key(repository, number, head, base)?;
        Ok(self
            .store
            .marker(&key)?
            .map(|marker| marker.fact().decode(&schema))
            .transpose()?)
    }

    fn begin_effect(
        &mut self,
        record: &GateVerdictRecord,
        decision: &GateDecision,
    ) -> Result<GateIntent, Self::Error> {
        self.check_house(&record.house)?;
        let name = effect_name(record)?;
        let tasks = self.store.tasks()?;
        // An earlier intent is found before the effect is rebuilt: the
        // branch's worker may have changed since it was persisted.
        if let Some(existing) = find_effect(&tasks, &name, record) {
            return Ok(GateIntent::Existing(
                existing.request().key().clone(),
                gate_state(existing.state()),
            ));
        }
        let owner = tasks
            .iter()
            .find(|task| task.spec().id == self.task)
            .ok_or_else(|| StateError::TaskNotFound(self.task.clone()))?;
        // A fix request asks a worker to edit, commit, and push the branch.
        // The owning task must hold push authority for the repository on the
        // forge under the house's current grants, separately from the worker
        // permissions that deliver the request.
        if matches!(record.verdict, Verdict::FixRequest { .. }) {
            owner.spec().authority.authorize(
                self.grants,
                Permission::PushBranch,
                &GrantScope::Repository(record.repository.clone()),
                &self.backend.descriptor().backend,
            )?;
        }
        let effect = self.effect(
            &tasks,
            owner
                .spec()
                .agent
                .as_ref()
                .map(|resolved| resolved.selection.clone()),
            record,
            decision,
        )?;
        let subject = EvidenceSubject {
            head: record.head.clone(),
            base: Some(record.base.clone()),
        };
        if owner.evidence().subject() != Some(&subject) {
            return Err(GateStoreError::SubjectNotRecorded);
        }
        // Worker effects are authorized for and targeted at the worker
        // backend; forge effects at the forge.
        let backend = match (&effect, self.workers) {
            (Effect::Worker(_), Some(workers)) => workers as &dyn EffectExecutor,
            (Effect::Worker(_), None) => return Err(GateStoreError::NoWorkerBackend),
            (Effect::GitHub(_) | Effect::Roger(_) | Effect::Schedule(_), _) => self.backend,
        };
        let plan = EffectPlan {
            task: self.task.clone(),
            fence: self.fence,
            name,
            decided_at: owner.evidence().revision(),
            effect,
            consent: None,
            basis: None,
        };
        Ok(
            match self
                .store
                .begin_effect(plan, self.grants, backend, record.recorded_at)?
            {
                EffectStart::Execute(started) => {
                    GateIntent::Submit(started.request().key().clone())
                }
                EffectStart::ReconcileFirst(existing) | EffectStart::Resolved(existing) => {
                    GateIntent::Existing(
                        existing.request().key().clone(),
                        gate_state(existing.state()),
                    )
                }
            },
        )
    }

    fn effect_state(&self, key: &IdempotencyKey) -> Result<GateEffectState, Self::Error> {
        let tasks = self.store.tasks()?;
        find_key(&tasks, key)
            .map(|effect| gate_state(effect.state()))
            .ok_or(GateStoreError::UnknownEffect)
    }

    /// Counts stop at [`MAX_BASE_READ_FAILURES`], so later passes at the
    /// same head write nothing more. A concurrent count fails the pass with
    /// the store's conflict; the next pass counts again.
    fn record_base_read_failure(
        &mut self,
        house: &HouseId,
        repository: &Repository,
        number: IssueNumber,
        head: &CommitId,
        now: Timestamp,
    ) -> Result<u8, Self::Error> {
        self.check_house(house)?;
        let schema = MarkerSchema::new(BASE_READ_SCHEMA, NonZeroU32::MIN)?;
        let key = MarkerKey {
            workflow: WorkflowId::new(GATE_BASE_READ_WORKFLOW)?,
            item: pull_request(repository, number)?,
            subject: MarkerSubject::Git(EvidenceSubject {
                head: head.clone(),
                base: None,
            }),
        };
        let fact = |failures| MarkerFact::workflow(schema.clone(), &BaseReadFailures { failures });
        match self.store.marker(&key)? {
            None => {
                self.store
                    .record_marker(key, fact(1)?, &self.claimant, now)?;
                Ok(1)
            }
            Some(marker) => {
                let seen: BaseReadFailures = marker.fact().decode(&schema)?;
                if seen.failures >= MAX_BASE_READ_FAILURES {
                    return Ok(seen.failures);
                }
                let failures = seen.failures.saturating_add(1);
                self.store.supersede_marker(
                    &key,
                    marker.fact(),
                    fact(failures)?,
                    &self.claimant,
                    now,
                )?;
                Ok(failures)
            }
        }
    }

    fn record(
        &mut self,
        expected: Option<&GateVerdictRecord>,
        record: GateVerdictRecord,
    ) -> Result<bool, Self::Error> {
        self.check_house(&record.house)?;
        let schema = schema()?;
        let key = Self::marker_key(
            &record.repository,
            record.number,
            &record.head,
            &record.base,
        )?;
        let at = record.recorded_at;
        let fact = MarkerFact::workflow(schema.clone(), &record)?;
        let written = match expected {
            None => self.store.record_marker(key, fact, &self.claimant, at),
            Some(expected) => self.store.supersede_marker(
                &key,
                &MarkerFact::workflow(schema, expected)?,
                fact,
                &self.claimant,
                at,
            ),
        };
        match written {
            Ok(
                MarkerRecording::Recorded(_)
                | MarkerRecording::AlreadyRecorded(_)
                | MarkerRecording::Superseded(_),
            ) => Ok(true),
            // Another writer changed the marker first.
            Err(crate::Error::State(StateError::MarkerConflict | StateError::MarkerNotFound)) => {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
}
