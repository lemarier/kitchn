//! Merge trains: ready pull requests that overlap are assembled into one
//! stack, so each conflict between them is resolved once.
//!
//! Merging overlapping pull requests one by one forces a rebase and a CI
//! round on every other open one after each merge, and some of those
//! conflicts are semantic, such as a new required field or enum variant.
//! [`plan_train`] takes the house's ready pull requests that overlap in
//! files or contracts and orders them into one train; the house stack tool
//! ([`crate::workflows::stack`]) assembles it, so each conflict is resolved
//! once inside the stack. [`evaluate_train`] then judges every layer at its
//! exact stacked head and base: the ready layers from the bottom may merge,
//! the first layer that is not ready and everything above it wait for the
//! next train, and a lower layer that changed makes every layer above it
//! stale until its CI runs again. [`merge_train`] prepares the gate's
//! head-matched merge of the bottom layer, and only when the gate recorded
//! a merge for its exact head and base.
//!
//! This module performs no I/O besides the forge re-read in
//! [`merge_train`]. Its assembly commands run through the stack tool
//! ([`crate::workflows::stack::StackBoundary`]) like any other; it never
//! reorders or merges a stack through the stack tool.

use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    HouseId,
    contracts::{BranchName, CommitId, EvidenceVerdict, IssueNumber, Repository},
    integrations::github::{GitHubClient, GitHubReadTransport, IntegrationError},
    workflows::{
        gate::{GateRun, MergeGrant, MergeRequest, RecordedDecision, Verdict},
        ready::{MergeReadiness, NotReady},
        stack::{MAX_STACK_LAYERS, StackCommand},
    },
};

/// Something a pull request changes that another may change too.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Touch {
    /// A file, by its path in the repository.
    File(PathBuf),
    /// A contract other code depends on, such as a shared type, enum, or
    /// schema, by name.
    Contract(String),
}

/// A pull request that may join a train.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainCandidate {
    /// Its readiness at its current head and base.
    pub readiness: MergeReadiness,
    /// Its head branch.
    pub branch: BranchName,
    /// What it changes.
    pub touches: BTreeSet<Touch>,
}

/// Why a pull request is not in the planned train.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deferral {
    /// It is not ready; it waits for a later train.
    NotReady(Vec<NotReady>),
    /// It overlaps no other ready pull request, so it merges on its own
    /// through the gate.
    NoOverlap,
    /// The train already has [`MAX_STACK_LAYERS`] layers.
    Full,
}

/// A train to assemble, bottom to top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainPlan {
    /// The branch the bottom layer is based on.
    pub trunk: BranchName,
    /// The layers, bottom to top: each pull request and its branch.
    pub layers: Vec<(IssueNumber, BranchName)>,
    /// The pull requests left out, and why.
    pub deferred: Vec<(IssueNumber, Deferral)>,
}

impl TrainPlan {
    /// The stack-tool commands that assemble the train: adopt the branches
    /// in train order, rebase them onto each other, and push and link them
    /// as ready pull requests. A rebase that stops on a conflict is resolved
    /// once in the stack, and the rebase continues. Empty when the plan has
    /// no train.
    #[must_use]
    pub fn assembly(&self) -> Vec<StackCommand> {
        if self.layers.is_empty() {
            return Vec::new();
        }
        vec![
            StackCommand::Adopt {
                trunk: self.trunk.clone(),
                branches: self
                    .layers
                    .iter()
                    .map(|(_, branch)| branch.clone())
                    .collect(),
            },
            StackCommand::RebaseUpstack,
            StackCommand::Submit { ready: true },
        ]
    }
}

/// Invalid train input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrainError {
    /// The layers are empty or too many, name a pull request or branch
    /// twice, span repositories, or include the trunk.
    #[error("the train's layers are empty, too many, repeated, or span repositories")]
    InvalidLayers,
}

impl TrainError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(self) -> crate::ErrorClass {
        match self {
            Self::InvalidLayers => crate::ErrorClass::InvalidInput,
        }
    }
}

/// Plan one train in `repository` onto `trunk` from `candidates`.
///
/// Every ready candidate that overlaps another ready one in a file or
/// contract joins the train, up to [`MAX_STACK_LAYERS`]. Each overlapping
/// pair is resolved once, in the upper of the two layers, whatever the
/// order; the candidate that overlaps the most goes lowest, so the widest
/// change never rebases and every later layer resolves against it once.
/// Ties go to the lower pull-request number. Candidates that are not ready
/// wait for a later train rather than joining this one, so a branch its
/// writer may still change is never rewritten; ready candidates that
/// overlap nothing merge on their own. A plan with fewer than two layers
/// has no train.
///
/// # Errors
/// Returns [`TrainError::InvalidLayers`] when a candidate is in another
/// repository, its branch is the trunk, or two candidates share a pull
/// request or branch.
pub fn plan_train(
    repository: &Repository,
    trunk: &BranchName,
    candidates: &[TrainCandidate],
) -> Result<TrainPlan, TrainError> {
    for (index, candidate) in candidates.iter().enumerate() {
        let repeated = candidates
            .iter()
            .skip(index.saturating_add(1))
            .any(|other| {
                other.readiness.pull_request == candidate.readiness.pull_request
                    || other.branch == candidate.branch
            });
        if &candidate.readiness.repository != repository || &candidate.branch == trunk || repeated {
            return Err(TrainError::InvalidLayers);
        }
    }
    let mut deferred = Vec::new();
    let mut ready = Vec::new();
    for candidate in candidates {
        let reasons = candidate.readiness.not_ready();
        if reasons.is_empty() {
            ready.push(candidate);
        } else {
            deferred.push((
                candidate.readiness.pull_request,
                Deferral::NotReady(reasons),
            ));
        }
    }
    let mut overlapping: Vec<(usize, &TrainCandidate)> = Vec::with_capacity(ready.len());
    for candidate in &ready {
        let overlaps = ready
            .iter()
            .filter(|other| {
                other.readiness.pull_request != candidate.readiness.pull_request
                    && !other.touches.is_disjoint(&candidate.touches)
            })
            .count();
        if overlaps == 0 {
            deferred.push((candidate.readiness.pull_request, Deferral::NoOverlap));
        } else {
            overlapping.push((overlaps, candidate));
        }
    }
    overlapping.sort_by_key(|(overlaps, candidate)| {
        (
            std::cmp::Reverse(*overlaps),
            candidate.readiness.pull_request.get(),
        )
    });
    let mut layers = Vec::with_capacity(overlapping.len().min(MAX_STACK_LAYERS));
    for (_, candidate) in overlapping {
        if layers.len() < MAX_STACK_LAYERS {
            layers.push((candidate.readiness.pull_request, candidate.branch.clone()));
        } else {
            deferred.push((candidate.readiness.pull_request, Deferral::Full));
        }
    }
    Ok(TrainPlan {
        trunk: trunk.clone(),
        layers,
        deferred,
    })
}

/// One assembled layer, read at its current stacked head and base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainLayer {
    /// Its readiness: `subject` is the layer's current head and the head of
    /// the layer below it (or the trunk tip) as its base.
    pub readiness: MergeReadiness,
    /// Its head branch.
    pub branch: BranchName,
}

/// How one layer stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerState {
    /// Its final review is clean and its required checks are green at its
    /// exact head and base.
    Ready,
    /// Its review or checks failed at its exact head and base: it waits
    /// for a later train, or is removed from the stack when it is the
    /// bottom layer.
    Failed(Vec<NotReady>),
    /// Its evidence is about another head or base, or could not be read:
    /// its CI and review must run again at this head.
    Pending(Vec<NotReady>),
    /// Its base is not the current head of the layer below (or the trunk
    /// tip), or a layer below it is in this state: a lower layer changed,
    /// so it must be rebased and its CI run again.
    LowerLayerChanged,
}

/// What to do with an assembled train.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrainDecision {
    /// The layers in `ready`, bottom to top, are ready at their exact heads
    /// and bases. [`merge_train`] merges the bottom one; after it lands, the
    /// rest are rebased onto the trunk and evaluated again. The layers in
    /// `deferred` sit above the first layer that is not ready and wait for
    /// the next train, so they never block the layers below them.
    Merge {
        /// The ready layers from the bottom, lowest first; never empty.
        ready: Vec<IssueNumber>,
        /// The first layer that is not ready and every layer above it.
        deferred: Vec<IssueNumber>,
    },
    /// The bottom layer is pending or must be rebased: nothing merges until
    /// its CI and review run again at a new head, or the stack's owner
    /// rebases it onto the moved trunk.
    Hold,
    /// The bottom layer failed at its exact head and base. Kitchen does not
    /// reorder a stack: the stack's owner removes this pull request from
    /// the stack, and the rest is planned again as a new train.
    RemoveBottom {
        /// The failed bottom layer.
        failed: IssueNumber,
    },
}

/// Evaluate an assembled train, bottom to top, against `trunk_tip`, the
/// trunk's current head. Each layer is judged at its exact head and base,
/// and its base must be the current head of the layer below (the trunk tip
/// for the bottom layer): when a lower layer changes, every layer above it
/// is [`LayerState::LowerLayerChanged`] or has stale evidence until it is
/// rebased and its CI runs again.
///
/// The ready layers from the bottom up to the first layer that is not
/// ready may merge; that layer and every one above it are deferred to the
/// next train, whether it failed or is still pending.
///
/// # Errors
/// Returns [`TrainError::InvalidLayers`] for no layers, more than
/// [`MAX_STACK_LAYERS`], layers in different repositories, or a pull
/// request or branch named twice.
pub fn evaluate_train(
    layers: &[TrainLayer],
    trunk_tip: &CommitId,
) -> Result<(TrainDecision, Vec<LayerState>), TrainError> {
    check_layers(layers)?;
    let mut states = Vec::with_capacity(layers.len());
    let mut below = trunk_tip;
    for layer in layers {
        // A layer that must be rebased moves every layer above it too.
        let state = match states.last() {
            Some(LayerState::LowerLayerChanged) => LayerState::LowerLayerChanged,
            Some(LayerState::Ready | LayerState::Failed(_) | LayerState::Pending(_)) | None => {
                layer_state(layer, below)
            }
        };
        states.push(state);
        below = &layer.readiness.subject.head;
    }
    let ready_count = states
        .iter()
        .take_while(|state| **state == LayerState::Ready)
        .count();
    let numbers = layers.iter().map(|layer| layer.readiness.pull_request);
    let decision = match (ready_count, layers.first(), states.first()) {
        (0, Some(bottom), Some(LayerState::Failed(_))) => TrainDecision::RemoveBottom {
            failed: bottom.readiness.pull_request,
        },
        (0, _, _) => TrainDecision::Hold,
        (count, _, _) => TrainDecision::Merge {
            ready: numbers.clone().take(count).collect(),
            deferred: numbers.skip(count).collect(),
        },
    };
    Ok((decision, states))
}

fn check_layers(layers: &[TrainLayer]) -> Result<(), TrainError> {
    let Some(bottom) = layers.first() else {
        return Err(TrainError::InvalidLayers);
    };
    if layers.len() > MAX_STACK_LAYERS {
        return Err(TrainError::InvalidLayers);
    }
    for (index, layer) in layers.iter().enumerate() {
        let repeated = layers.iter().skip(index.saturating_add(1)).any(|other| {
            other.readiness.pull_request == layer.readiness.pull_request
                || other.branch == layer.branch
        });
        if layer.readiness.repository != bottom.readiness.repository || repeated {
            return Err(TrainError::InvalidLayers);
        }
    }
    Ok(())
}

fn layer_state(layer: &TrainLayer, below: &CommitId) -> LayerState {
    let readiness = &layer.readiness;
    if readiness.subject.base.as_ref() != Some(below) {
        return LayerState::LowerLayerChanged;
    }
    let reasons = readiness.not_ready();
    if reasons.is_empty() {
        return LayerState::Ready;
    }
    // Only a result about this exact head and base is a failure of this
    // layer; stale or unreadable evidence says nothing about it yet.
    let failed_here = [&readiness.review, &readiness.checks]
        .into_iter()
        .any(|evidence| {
            evidence.verdict == EvidenceVerdict::Fail && evidence.subject == readiness.subject
        });
    if failed_here {
        LayerState::Failed(reasons)
    } else {
        LayerState::Pending(reasons)
    }
}

/// Why a train merge was not prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrainRefusal {
    /// The layers are not a valid train.
    #[error(transparent)]
    Invalid(#[from] TrainError),
    /// [`evaluate_train`] does not decide to merge the train as read.
    #[error("the train is not ready to merge")]
    NotReady,
    /// The gate has not recorded a merge of this layer at its exact pull
    /// request, head, and base, targeting the trunk, in this house.
    #[error("no recorded gate merge for pull request {} at its exact head and base", .0.get())]
    NoGateMerge(IssueNumber),
    /// No merge grant covers this layer at its exact head and base.
    #[error("no merge grant covers pull request {} at its exact head and base", .0.get())]
    NoMergeGrant(IssueNumber),
    /// The forge re-read before the merge failed, or found the pull request
    /// or trunk moved, closed, or retargeted.
    #[error(transparent)]
    Forge(#[from] IntegrationError),
}

impl TrainRefusal {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(self) -> crate::ErrorClass {
        match self {
            Self::Invalid(error) => error.class(),
            Self::NotReady | Self::NoGateMerge(_) | Self::NoMergeGrant(_) => {
                crate::ErrorClass::Refused
            }
            Self::Forge(error) => error.class(),
        }
    }
}

/// Prepare the merge of the train's bottom layer in `house` onto `trunk`.
///
/// A train lands one layer at a time, bottom to top, through the gate's
/// head-matched squash merge ([`crate::workflows::gate::MergeRequest`]),
/// never through the stack tool's merge, which cannot pin each layer's
/// head. The train must decide to merge ([`evaluate_train`]), and `recorded`
/// must be the gate's recorded [`Verdict::Merge`] for exactly the bottom
/// layer: this house, repository, pull request, and head, with `trunk` as
/// its base branch and the bottom layer's base as its base. The train is
/// judged against that base as the trunk tip, and `grant` must cover the
/// same subject. Every mismatch is refused before any forge call. The
/// forge is then read again through `run`: a pull request that moved,
/// closed, or was retargeted, or a trunk whose tip is no longer that base,
/// is refused, and the forge itself enforces the expected head when it
/// receives the merge.
///
/// After the bottom layer lands, the stack's owner rebases the rest onto
/// the trunk; their CI runs again at the new heads and the gate records a
/// new decision for the next bottom layer. A layer above the bottom
/// targets another layer's branch, so the gate never records a merge for
/// it until it becomes the bottom.
///
/// # Errors
/// Returns [`TrainRefusal::Invalid`] for invalid layers,
/// [`TrainRefusal::NotReady`] unless the train decides to merge,
/// [`TrainRefusal::NoGateMerge`] without a matching recorded gate merge,
/// [`TrainRefusal::NoMergeGrant`] without a matching grant, and
/// [`TrainRefusal::Forge`] when the re-read fails or finds a change.
pub fn merge_train<T: GitHubReadTransport>(
    house: &HouseId,
    layers: &[TrainLayer],
    trunk: &BranchName,
    recorded: &RecordedDecision,
    grant: &MergeGrant,
    run: &GateRun,
    client: &GitHubClient<T>,
) -> Result<MergeRequest, TrainRefusal> {
    let gate = &recorded.decision;
    let trunk_tip = &gate.base;
    let (decision, _) = evaluate_train(layers, trunk_tip)?;
    let bottom = match (decision, layers.first()) {
        (TrainDecision::Merge { .. }, Some(bottom)) => &bottom.readiness,
        (
            TrainDecision::Merge { .. } | TrainDecision::Hold | TrainDecision::RemoveBottom { .. },
            _,
        ) => {
            return Err(TrainRefusal::NotReady);
        }
    };
    let recorded_here = gate.verdict == Verdict::Merge
        && &gate.house == house
        && gate.repository == bottom.repository
        && gate.number == bottom.pull_request
        && gate.head == bottom.subject.head
        && gate.base_branch.as_ref() == Some(trunk);
    if !recorded_here {
        return Err(TrainRefusal::NoGateMerge(bottom.pull_request));
    }
    if !grant.covers(
        house,
        &bottom.repository,
        bottom.pull_request,
        &bottom.subject.head,
        trunk_tip,
    ) {
        return Err(TrainRefusal::NoMergeGrant(bottom.pull_request));
    }
    Ok(run.next_merge(recorded, grant, client)?)
}
