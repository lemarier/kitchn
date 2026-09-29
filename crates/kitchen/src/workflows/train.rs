//! Merge trains: ready pull requests that overlap land together as one
//! stack instead of one at a time.
//!
//! Merging overlapping pull requests one by one forces a rebase and a CI
//! round on every other open one after each merge, and some of those
//! conflicts are semantic, such as a new required field or enum variant.
//! [`plan_train`] takes the house's ready pull requests that overlap in
//! files or contracts and orders them into one train; the house stack tool
//! ([`crate::workflows::stack`]) assembles it, so each conflict is resolved
//! once inside the stack. [`evaluate_train`] then judges every layer at its
//! exact stacked head and base: a failing layer moves to the top, a pending
//! one holds the train, and a lower layer that changed makes every layer
//! above it stale until its CI runs again. [`merge_train`] prepares the
//! stack tool's merge only when every merged layer is ready and a merge
//! grant covers its exact head and base.
//!
//! This module performs no I/O. Its commands run through the stack tool
//! ([`crate::workflows::stack::StackBoundary`]) like any other.

use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    HouseId,
    contracts::{BranchName, CommitId, EvidenceVerdict, IssueNumber, Repository},
    house::MergeSubject,
    workflows::{
        gate::MergeGrant,
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
    /// Its review or checks failed at its exact head and base: it moves to
    /// the top of the train.
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
    /// Merge every layer up to and including `top`, bottom to top, with
    /// [`merge_train`]. Failed layers above `top` stay for a later train.
    Merge {
        /// The highest layer to merge.
        top: IssueNumber,
    },
    /// Failed layers sit below others: adopt the branches again in `order`,
    /// with every failed layer moved to the top, then rebase and submit
    /// ([`reorder_commands`]). The moved layers' CI runs again.
    Reorder {
        /// The new order, bottom to top.
        order: Vec<BranchName>,
    },
    /// Nothing merges yet: a layer below the failed ones is pending or
    /// changed, or every layer failed.
    Hold,
}

/// Evaluate an assembled train, bottom to top, against `trunk_tip`, the
/// trunk's current head. Each layer is judged at its exact head and base,
/// and its base must be the current head of the layer below (the trunk tip
/// for the bottom layer): when a lower layer changes, every layer above it
/// is [`LayerState::LowerLayerChanged`] or has stale evidence until it is
/// rebased and its CI runs again.
///
/// A failed layer below any other layer is moved to the top. Otherwise,
/// the layers below the failed ones merge together once every one of them
/// is ready. A pending or changed layer among them holds the whole train
/// while its CI runs again, rather than merging the layers below it alone
/// and forcing another rebase and CI round on the rest; the caller bounds
/// how long it holds, as the gate does with [`crate::workflows::gate::STALL_TIME`].
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
    let first_failed = states
        .iter()
        .position(|state| matches!(state, LayerState::Failed(_)));
    let moved_up = first_failed.is_some_and(|first| {
        states
            .iter()
            .skip(first)
            .any(|state| !matches!(state, LayerState::Failed(_)))
    });
    let decision = if moved_up {
        let (failed, kept): (Vec<_>, Vec<_>) = layers
            .iter()
            .zip(&states)
            .partition(|(_, state)| matches!(state, LayerState::Failed(_)));
        TrainDecision::Reorder {
            order: kept
                .into_iter()
                .chain(failed)
                .map(|(layer, _)| layer.branch.clone())
                .collect(),
        }
    } else {
        let mergeable = first_failed.unwrap_or(states.len());
        let prefix = states.get(..mergeable).unwrap_or_default();
        match layers.get(..mergeable).and_then(<[TrainLayer]>::last) {
            Some(top) if prefix.iter().all(|state| state == &LayerState::Ready) => {
                TrainDecision::Merge {
                    top: top.readiness.pull_request,
                }
            }
            Some(_) | None => TrainDecision::Hold,
        }
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

/// The stack-tool commands that move a train into `order`: stop tracking
/// the current stack locally, adopt the branches again in the new order,
/// rebase them onto each other, and push and relink their pull requests.
#[must_use]
pub fn reorder_commands(trunk: &BranchName, order: &[BranchName]) -> Vec<StackCommand> {
    vec![
        StackCommand::Unstack,
        StackCommand::Adopt {
            trunk: trunk.clone(),
            branches: order.to_vec(),
        },
        StackCommand::RebaseUpstack,
        StackCommand::Submit { ready: true },
    ]
}

/// A prepared train merge. Only [`merge_train`] builds one, so the stack
/// tool's merge ([`StackCommand::Merge`]) always carries layers that were
/// ready and granted at their exact heads and bases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainMerge {
    top: IssueNumber,
    layers: Vec<MergeSubject>,
}

impl TrainMerge {
    /// The highest layer merged.
    #[must_use]
    pub const fn top(&self) -> IssueNumber {
        self.top
    }

    /// Every merged layer's exact subject, bottom to top.
    #[must_use]
    pub fn layers(&self) -> &[MergeSubject] {
        &self.layers
    }

    /// The stack-tool command that merges the train.
    #[must_use]
    pub fn command(&self) -> StackCommand {
        StackCommand::Merge(self.clone())
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
    /// No merge grant covers this layer at its exact head and base.
    #[error("no merge grant covers pull request {} at its exact head and base", .0.get())]
    NoMergeGrant(IssueNumber),
}

impl TrainRefusal {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(self) -> crate::ErrorClass {
        match self {
            Self::Invalid(error) => error.class(),
            Self::NotReady | Self::NoMergeGrant(_) => crate::ErrorClass::Refused,
        }
    }
}

/// Prepare the stack tool's merge of `layers` in `house`, read again right
/// before the merge. The train is evaluated again with `trunk_tip`, and
/// every layer up to the top it decides to merge must be covered by one of
/// `grants`, each resolved with [`MergeGrant::resolve`] for that layer's
/// exact pull request, head, and base (the head of the layer below, or the
/// trunk tip).
///
/// `gh stack merge` checks only that each pull request is open and not a
/// draft, not that its head is still the one read here. Read the layers
/// immediately before the merge, and after an uncertain result read the
/// pull requests again before planning another train: a merged layer can
/// never merge twice. `gh stack merge` also reads a bare number as a stack
/// number before a pull-request number; where a stack shares the top pull
/// request's number, the command names that stack, which this module
/// cannot detect.
///
/// # Errors
/// Returns [`TrainRefusal::Invalid`] for invalid layers,
/// [`TrainRefusal::NotReady`] unless the train decides to merge, and
/// [`TrainRefusal::NoMergeGrant`] for the lowest layer no grant covers.
pub fn merge_train(
    house: &HouseId,
    layers: &[TrainLayer],
    trunk_tip: &CommitId,
    grants: &[MergeGrant],
) -> Result<TrainMerge, TrainRefusal> {
    let (decision, _) = evaluate_train(layers, trunk_tip)?;
    let top = match decision {
        TrainDecision::Merge { top } => top,
        TrainDecision::Reorder { .. } | TrainDecision::Hold => {
            return Err(TrainRefusal::NotReady);
        }
    };
    let mut merged = Vec::with_capacity(layers.len());
    let mut below = trunk_tip;
    for layer in layers {
        let readiness = &layer.readiness;
        let covered = grants.iter().any(|grant| {
            grant.covers(
                house,
                &readiness.repository,
                readiness.pull_request,
                &readiness.subject.head,
                below,
            )
        });
        if !covered {
            return Err(TrainRefusal::NoMergeGrant(readiness.pull_request));
        }
        merged.push(MergeSubject {
            repository: readiness.repository.clone(),
            number: readiness.pull_request,
            head: readiness.subject.head.clone(),
            base: below.clone(),
        });
        below = &readiness.subject.head;
        if readiness.pull_request == top {
            break;
        }
    }
    Ok(TrainMerge {
        top,
        layers: merged,
    })
}
