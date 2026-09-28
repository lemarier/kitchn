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

use std::num::{NonZeroU32, NonZeroU64};

use crate::{
    EffectName, HouseId, IdentifierError, TaskId, WorkflowId,
    contracts::{
        BackendDescriptor, Claimant, CommitId, ContractError, Effect, EvidenceSubject, ExternalRef,
        Fence, GitHubAction, GitHubEffect, HouseGrants, IdempotencyKey, IssueNumber, PostingBudget,
        Repository, Timestamp, ValueKind,
    },
    state::{
        EffectPlan, EffectRecord, EffectStart, EffectState, HouseStore, MarkerFact, MarkerKey,
        MarkerRecording, MarkerSchema, MarkerSubject, StateError, TaskRecord, WorkItem,
    },
};

use super::{
    GATE_VERDICT_SCHEMA, GATE_VERDICT_VERSION, GateDecision, GateEffectState, GateHistory,
    GateIntent, GateMarkerStore, GateVerdictRecord, Verdict, handover_comment, merge_mutation,
};

/// Workflow id under which gate verdict markers are recorded.
pub const GATE_WORKFLOW: &str = "merge-gate";

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
    /// The verdict has no effect this store can persist. Fix delivery waits
    /// for an owned worker-message path.
    #[error("the verdict has no house-store effect")]
    NoHouseStoreEffect,
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
#[derive(Debug)]
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
    /// The forge backend that will perform the effects.
    pub backend: &'a BackendDescriptor,
    /// Authenticated forge requester persisted with each intent.
    pub requester: ExternalRef,
    /// The house's per-task forge posting ceiling.
    pub posting_budget: PostingBudget,
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
        record: &GateVerdictRecord,
        decision: &GateDecision,
    ) -> Result<Effect, GateStoreError> {
        let mutation = match &record.verdict {
            Verdict::Merge => merge_mutation(
                &record.repository,
                record.number,
                &record.head,
                &record.base,
                decision
                    .base_branch
                    .as_ref()
                    .ok_or(GateStoreError::MissingBaseBranch)?,
            ),
            Verdict::HandOver { gaps } => handover_comment(
                &record.repository,
                record.number,
                &record.head,
                &record.base,
                gaps,
                &decision.verified_findings,
            )?,
            Verdict::Skip | Verdict::FixRequest { .. } => {
                return Err(GateStoreError::NoHouseStoreEffect);
            }
        };
        Ok(Effect::GitHub(GitHubEffect {
            requester: self.requester.clone(),
            mutation,
            posting_budget: self.posting_budget,
        }))
    }
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
        Verdict::Skip | Verdict::FixRequest { .. } => {
            return Err(GateStoreError::NoHouseStoreEffect);
        }
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

/// Whether a persisted effect is a forge effect on this PR.
fn targets(effect: &EffectRecord, repository: &Repository, number: IssueNumber) -> bool {
    let Effect::GitHub(github) = effect.request().effect() else {
        return false;
    };
    &github.mutation.repository == repository
        && match &github.mutation.action {
            GitHubAction::MergePullRequest { number: pr, .. }
            | GitHubAction::PostComment { issue: pr, .. } => *pr == number,
            GitHubAction::CloseIssue { .. }
            | GitHubAction::SetLabel { .. }
            | GitHubAction::CreateIssue { .. }
            | GitHubAction::LinkSubIssue { .. }
            | GitHubAction::LinkDependency { .. }
            | GitHubAction::CreateLabel { .. } => false,
        }
}

/// The latest same-named intent on this PR in any task, preferring one that
/// may have applied over a refused one.
fn find_effect<'a>(
    tasks: &'a [TaskRecord],
    name: &EffectName,
    repository: &Repository,
    number: IssueNumber,
) -> Option<&'a EffectRecord> {
    tasks
        .iter()
        .flat_map(TaskRecord::effects)
        .filter(|effect| effect.name() == name && targets(effect, repository, number))
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

const fn gate_state(state: &EffectState) -> GateEffectState {
    match state {
        EffectState::Intended => GateEffectState::Intended,
        EffectState::Uncertain { .. } => GateEffectState::Uncertain,
        EffectState::Applied { .. } => GateEffectState::Applied,
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
        let effect = self.effect(record, decision)?;
        let tasks = self.store.tasks()?;
        if let Some(existing) = find_effect(&tasks, &name, &record.repository, record.number) {
            return Ok(GateIntent::Existing(
                existing.request().key().clone(),
                gate_state(existing.state()),
            ));
        }
        let owner = tasks
            .iter()
            .find(|task| task.spec().id == self.task)
            .ok_or_else(|| StateError::TaskNotFound(self.task.clone()))?;
        let subject = EvidenceSubject {
            head: record.head.clone(),
            base: Some(record.base.clone()),
        };
        if owner.evidence().subject() != Some(&subject) {
            return Err(GateStoreError::SubjectNotRecorded);
        }
        let plan = EffectPlan {
            task: self.task.clone(),
            fence: self.fence,
            name,
            decided_at: owner.evidence().revision(),
            effect,
            consent: None,
        };
        Ok(
            match self
                .store
                .begin_effect(plan, self.grants, self.backend, record.recorded_at)?
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
