//! Durable external effects, split by the executor family that performs them.
//!
//! Every effect goes through the same path: the state store persists an
//! [`crate::contracts::EffectRequest`] carrying an [`Effect`], then an
//! [`crate::contracts::EffectExecutor`] performs it. Each payload maps
//! exhaustively to the [`Permission`] and [`Capability`] it needs and to the
//! scope its authority is checked against. Every submission runs the
//! payload's check ([`Effect::check`]), and a new intent also runs its
//! admission hook ([`Effect::admit`]); both see an [`EffectContext`] computed
//! in the same store transaction, so per-task budgets hold even under
//! concurrent callers.
//!
//! The payload modules are owned by the workflows that use them: `github`
//! and `roger` by #7, `schedule` by #6. They start with the minimal payloads
//! needed to exercise the mechanism.

mod github;
mod roger;
mod schedule;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use github::{
    CloseReason, GitHubAction, GitHubEffect, GitHubMutation, IssueNumber, LabelDefinition,
    MergeMethod, PostingBudget,
};
pub use roger::{
    AskKind, AskRisk, DecisionBinding, DecisionOwner, MAX_ASKS_PER_TASK, RogerAsk, RogerEffect,
};
pub use schedule::ScheduleEffect;

use crate::{
    HouseId, TaskId,
    contracts::{
        Capability, ContractError, EvidenceRevision, EvidenceSubject, GrantScope, Operation,
        Permission, ResourceRef, ValueKind,
    },
};

/// The executor family that performs an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutorKind {
    /// An orchestrator running workers.
    Worker,
    /// A forge such as GitHub.
    #[serde(rename = "github")]
    GitHub,
    /// A human-decision service such as Roger.
    Roger,
    /// A scheduler.
    Schedule,
}

closed_names! {
    /// The kind of an effect, one per payload variant. Executors declare
    /// lookup and idempotency per kind.
    pub enum EffectKind(ValueKind::EffectKind) {
        /// The `launch_worker` effect.
        LaunchWorker = "launch_worker",
        /// The `message_worker` effect.
        MessageWorker = "message_worker",
        /// The `reply_to_worker` effect.
        ReplyToWorker = "reply_to_worker",
        /// The `cancel_worker` effect.
        CancelWorker = "cancel_worker",
        /// The `release_resource` effect.
        ReleaseResource = "release_resource",
        /// The `create_label` effect.
        CreateLabel = "create_label",
        /// The `post_comment` effect.
        PostComment = "post_comment",
        /// The `set_label` effect.
        SetLabel = "set_label",
        /// The `create_issue` effect.
        CreateIssue = "create_issue",
        /// The `link_sub_issue` effect.
        LinkSubIssue = "link_sub_issue",
        /// The `link_dependency` effect.
        LinkDependency = "link_dependency",
        /// The `merge_pull_request` effect.
        MergePullRequest = "merge_pull_request",
        /// The `close_issue` effect.
        CloseIssue = "close_issue",
        /// The `open_pull_request` effect.
        OpenPullRequest = "open_pull_request",
        /// The `ask` effect.
        Ask = "ask",
        /// The `install_disabled_schedule` effect.
        InstallDisabledSchedule = "install_disabled_schedule",
        /// The `set_schedule_state` effect.
        SetScheduleState = "set_schedule_state",
        /// The `remove_schedule` effect.
        RemoveSchedule = "remove_schedule",
        /// The `trial_schedule` effect.
        TrialSchedule = "trial_schedule",
    }
}

impl EffectKind {
    /// The capability that declares lookup for this kind.
    #[must_use]
    pub const fn lookup_capability(self) -> Capability {
        match self {
            Self::LaunchWorker => Capability::LookupLaunchWorker,
            Self::MessageWorker => Capability::LookupMessageWorker,
            Self::ReplyToWorker => Capability::LookupReplyToWorker,
            Self::CancelWorker => Capability::LookupCancelWorker,
            Self::ReleaseResource => Capability::LookupReleaseResource,
            Self::CreateLabel => Capability::LookupCreateLabel,
            Self::PostComment => Capability::LookupPostComment,
            Self::SetLabel => Capability::LookupSetLabel,
            Self::CreateIssue => Capability::LookupCreateIssue,
            Self::LinkSubIssue => Capability::LookupLinkSubIssue,
            Self::LinkDependency => Capability::LookupLinkDependency,
            Self::MergePullRequest => Capability::LookupMergePullRequest,
            Self::CloseIssue => Capability::LookupCloseIssue,
            Self::OpenPullRequest => Capability::LookupOpenPullRequest,
            Self::Ask => Capability::LookupAsk,
            Self::InstallDisabledSchedule => Capability::LookupInstallDisabledSchedule,
            Self::SetScheduleState => Capability::LookupSetScheduleState,
            Self::RemoveSchedule => Capability::LookupRemoveSchedule,
            Self::TrialSchedule => Capability::LookupTrialSchedule,
        }
    }

    /// The capability that declares same-key idempotency for this kind.
    #[must_use]
    pub const fn idempotency_capability(self) -> Capability {
        match self {
            Self::LaunchWorker => Capability::IdempotentLaunchWorker,
            Self::MessageWorker => Capability::IdempotentMessageWorker,
            Self::ReplyToWorker => Capability::IdempotentReplyToWorker,
            Self::CancelWorker => Capability::IdempotentCancelWorker,
            Self::ReleaseResource => Capability::IdempotentReleaseResource,
            Self::CreateLabel => Capability::IdempotentCreateLabel,
            Self::PostComment => Capability::IdempotentPostComment,
            Self::SetLabel => Capability::IdempotentSetLabel,
            Self::CreateIssue => Capability::IdempotentCreateIssue,
            Self::LinkSubIssue => Capability::IdempotentLinkSubIssue,
            Self::LinkDependency => Capability::IdempotentLinkDependency,
            Self::MergePullRequest => Capability::IdempotentMergePullRequest,
            Self::CloseIssue => Capability::IdempotentCloseIssue,
            Self::OpenPullRequest => Capability::IdempotentOpenPullRequest,
            Self::Ask => Capability::IdempotentAsk,
            Self::InstallDisabledSchedule => Capability::IdempotentInstallDisabledSchedule,
            Self::SetScheduleState => Capability::IdempotentSetScheduleState,
            Self::RemoveSchedule => Capability::IdempotentRemoveSchedule,
            Self::TrialSchedule => Capability::IdempotentTrialSchedule,
        }
    }
}

/// One external effect, persisted with its intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "executor", content = "effect", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Effect {
    /// A worker operation.
    Worker(Operation),
    /// A forge mutation.
    #[serde(rename = "github")]
    GitHub(GitHubEffect),
    /// A human decision request.
    Roger(RogerEffect),
    /// A schedule change.
    Schedule(ScheduleEffect),
}

impl Effect {
    /// The executor family that performs this effect.
    #[must_use]
    pub const fn executor(&self) -> ExecutorKind {
        match self {
            Self::Worker(_) => ExecutorKind::Worker,
            Self::GitHub(_) => ExecutorKind::GitHub,
            Self::Roger(_) => ExecutorKind::Roger,
            Self::Schedule(_) => ExecutorKind::Schedule,
        }
    }

    /// The effect's kind.
    #[must_use]
    pub const fn kind(&self) -> EffectKind {
        match self {
            Self::Worker(operation) => match operation {
                Operation::LaunchWorker { .. } => EffectKind::LaunchWorker,
                Operation::MessageWorker { .. } => EffectKind::MessageWorker,
                Operation::ReplyToWorker { .. } => EffectKind::ReplyToWorker,
                Operation::CancelWorker { .. } => EffectKind::CancelWorker,
                Operation::ReleaseResource { .. } => EffectKind::ReleaseResource,
            },
            Self::GitHub(effect) => match effect.mutation.action {
                GitHubAction::CreateLabel { .. } => EffectKind::CreateLabel,
                GitHubAction::PostComment { .. } => EffectKind::PostComment,
                GitHubAction::SetLabel { .. } => EffectKind::SetLabel,
                GitHubAction::CreateIssue { .. } => EffectKind::CreateIssue,
                GitHubAction::LinkSubIssue { .. } => EffectKind::LinkSubIssue,
                GitHubAction::LinkDependency { .. } => EffectKind::LinkDependency,
                GitHubAction::MergePullRequest { .. } => EffectKind::MergePullRequest,
                GitHubAction::CloseIssue { .. } => EffectKind::CloseIssue,
                GitHubAction::OpenPullRequest { .. } => EffectKind::OpenPullRequest,
            },
            Self::Roger(_) => EffectKind::Ask,
            Self::Schedule(effect) => match effect {
                ScheduleEffect::InstallDisabled { .. } => EffectKind::InstallDisabledSchedule,
                ScheduleEffect::SetState { .. } => EffectKind::SetScheduleState,
                ScheduleEffect::Remove { .. } => EffectKind::RemoveSchedule,
                ScheduleEffect::Trial { .. } => EffectKind::TrialSchedule,
            },
        }
    }

    /// The executor capability this effect needs.
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::Worker(operation) => operation.required_capability(),
            Self::GitHub(effect) => effect.required_capability(),
            Self::Roger(effect) => effect.required_capability(),
            Self::Schedule(effect) => effect.required_capability(),
        }
    }

    /// The capabilities the effect's workflow declares for the executor
    /// beyond [`Self::required_capability`], such as a schedule install's
    /// [`ScheduleSpec::requires`](crate::scheduling::ScheduleSpec::requires).
    pub fn workflow_requirements(&self) -> impl Iterator<Item = Capability> + '_ {
        let declared = match self {
            Self::Schedule(effect) => effect.workflow_requirements(),
            Self::Worker(_) | Self::GitHub(_) | Self::Roger(_) => None,
        };
        declared.into_iter().flatten().copied()
    }

    /// The task permission this effect needs.
    #[must_use]
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::Worker(operation) => operation.required_permission(),
            Self::GitHub(effect) => effect.required_permission(),
            Self::Roger(effect) => effect.required_permission(),
            Self::Schedule(effect) => effect.required_permission(),
        }
    }

    /// The scope authority is checked against. Worker and Roger effects act
    /// within the task's scope; a forge effect names its repository; a
    /// schedule is house-wide. The store refuses an effect whose scope the
    /// task's own scope does not cover.
    #[must_use]
    pub fn scope(&self, task_scope: &GrantScope) -> GrantScope {
        match self {
            Self::Worker(_) | Self::Roger(_) => task_scope.clone(),
            Self::GitHub(effect) => effect.scope(),
            Self::Schedule(effect) => effect.scope(),
        }
    }

    /// The existing resource this effect acts on, if any.
    #[must_use]
    pub const fn target(&self) -> Option<&ResourceRef> {
        match self {
            Self::Worker(operation) => operation.target(),
            Self::GitHub(_) | Self::Roger(_) | Self::Schedule(_) => None,
        }
    }

    /// The per-submission check, run inside the store transaction before
    /// every submission: a new intent and a same-key retry of an existing
    /// one. Read-only reconciliation does not run it.
    ///
    /// # Errors
    /// Returns the payload's refusal, such as
    /// [`ContractError::DecisionBindingMismatch`].
    pub fn check(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::Worker(_) => Ok(()),
            Self::GitHub(effect) => effect.check(context),
            Self::Roger(effect) => effect.check(context),
            Self::Schedule(effect) => effect.check(context),
        }
    }

    /// The admission hook, run inside the transaction that persists a new
    /// effect's intent, after [`Self::check`]. It reserves capacity, such as a
    /// per-task budget; a same-key retry of an existing intent does not run
    /// it, so the retry does not count against itself.
    ///
    /// # Errors
    /// Returns the payload's refusal, such as
    /// [`ContractError::EffectBudgetExhausted`].
    pub fn admit(&self, context: &EffectContext<'_>) -> Result<(), ContractError> {
        match self {
            Self::Worker(_) => Ok(()),
            Self::GitHub(effect) => effect.admit(context),
            Self::Roger(effect) => effect.admit(context),
            Self::Schedule(effect) => effect.admit(context),
        }
    }
}

impl From<Operation> for Effect {
    fn from(operation: Operation) -> Self {
        Self::Worker(operation)
    }
}

impl From<GitHubEffect> for Effect {
    fn from(effect: GitHubEffect) -> Self {
        Self::GitHub(effect)
    }
}

impl From<RogerEffect> for Effect {
    fn from(effect: RogerEffect) -> Self {
        Self::Roger(effect)
    }
}

impl From<ScheduleEffect> for Effect {
    fn from(effect: ScheduleEffect) -> Self {
        Self::Schedule(effect)
    }
}

/// Effects of one task that were already submitted, or may have been:
/// every recorded effect except those established as not applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmittedEffects {
    counts: BTreeMap<ExecutorKind, u32>,
}

impl SubmittedEffects {
    /// Count one more effect for `executor`.
    pub(crate) fn add(&mut self, executor: ExecutorKind) {
        let count = self.counts.entry(executor).or_default();
        *count = count.saturating_add(1);
    }

    /// All submitted effects of the task.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.counts
            .values()
            .fold(0, |total, count| total.saturating_add(*count))
    }

    /// Submitted effects of the task for one executor family.
    #[must_use]
    pub fn for_executor(&self, executor: ExecutorKind) -> u32 {
        self.counts.get(&executor).copied().unwrap_or(0)
    }
}

/// What an admission hook sees, computed in the same store transaction that
/// persists the new intent.
#[derive(Debug, Clone, Copy)]
pub struct EffectContext<'a> {
    /// The store's house.
    pub house: &'a HouseId,
    /// The task.
    pub task: &'a TaskId,
    /// The task's scope.
    pub task_scope: &'a GrantScope,
    /// The task's current evidence revision.
    pub revision: EvidenceRevision,
    /// The exact subject (head and base) the current evidence is about;
    /// `None` before any evidence was recorded.
    pub subject: Option<&'a EvidenceSubject>,
    /// The task's effects already submitted, before this one.
    pub submitted: &'a SubmittedEffects,
}
