//! What a launch surface can honor, and the gaps that reject a selection.

use std::fmt;

use crate::{contracts::Capability, scheduling::AgentFamily};

use super::{AgentSelection, SelectionError};

/// Whether a launch surface accepts a reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSupport {
    /// No effort can be passed.
    Unsupported,
    /// Effort is accepted only together with an explicit model.
    WithModel,
    /// Effort is accepted with or without a model.
    Always,
}

/// One part of a selection a launch surface cannot provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelectionGap {
    /// The surface cannot launch this agent family.
    Family(AgentFamily),
    /// The surface cannot select a model.
    Model,
    /// The surface cannot select an effort.
    Effort,
    /// The surface accepts an effort only with an explicit model.
    EffortWithoutModel,
}

impl SelectionGap {
    /// The backend capability whose absence this gap represents.
    #[must_use]
    pub const fn capability(self) -> Capability {
        match self {
            Self::Family(_) => Capability::AgentSelectFamily,
            Self::Model | Self::Effort | Self::EffortWithoutModel => Capability::AgentSelectModel,
        }
    }
}

impl fmt::Display for SelectionGap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Family(agent) => write!(
                formatter,
                "{}: agent family {} cannot be launched",
                self.capability(),
                agent.as_str()
            ),
            Self::Model => write!(formatter, "{}: no model can be selected", self.capability()),
            Self::Effort => write!(
                formatter,
                "{}: no effort can be selected",
                self.capability()
            ),
            Self::EffortWithoutModel => write!(
                formatter,
                "{}: an effort needs an explicit model",
                self.capability()
            ),
        }
    }
}

/// What one launch surface of a backend, such as worker launches or
/// scheduled runs, can honor. Adapters declare it; Kitchen checks a
/// selection against it before launching and never substitutes another
/// family, model, or effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSupport {
    /// Families the surface can launch.
    pub families: &'static [AgentFamily],
    /// Whether a model can be selected.
    pub model: bool,
    /// Whether and how an effort can be selected.
    pub effort: EffortSupport,
}

impl SelectionSupport {
    /// Every gap between `selection` and this surface.
    #[must_use]
    pub fn gaps(&self, selection: &AgentSelection) -> Vec<SelectionGap> {
        let mut gaps = Vec::new();
        if !self.families.contains(&selection.agent) {
            gaps.push(SelectionGap::Family(selection.agent));
        }
        if selection.model.is_some() && !self.model {
            gaps.push(SelectionGap::Model);
        }
        if selection.effort.is_some() {
            match self.effort {
                EffortSupport::Unsupported => gaps.push(SelectionGap::Effort),
                EffortSupport::WithModel if selection.model.is_none() => {
                    gaps.push(SelectionGap::EffortWithoutModel);
                }
                EffortSupport::WithModel | EffortSupport::Always => {}
            }
        }
        gaps
    }

    /// Accept `selection` only when the surface can provide all of it.
    ///
    /// # Errors
    /// [`SelectionError::Unsupported`] naming every gap.
    pub fn check(&self, selection: &AgentSelection) -> Result<(), SelectionError> {
        let gaps = self.gaps(selection);
        if gaps.is_empty() {
            Ok(())
        } else {
            Err(SelectionError::Unsupported {
                agent: selection.agent,
                gaps,
            })
        }
    }
}
