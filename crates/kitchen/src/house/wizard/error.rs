use super::InitQuestion;
use crate::{ErrorClass, house::HouseError};

/// Guided `house init` failures. Messages name questions and flags, never
/// the text a person typed.
#[derive(Debug, thiserror::Error)]
pub enum HouseInitError {
    /// Standard input is not a terminal and these answers have neither a
    /// flag nor a default. Nothing was written.
    #[error(
        "standard input is not a terminal, so kitchen cannot ask; pass {} (or register a reviewed file with --config)",
        flags(.0)
    )]
    MissingAnswers(Vec<InitQuestion>),
    /// A flag value, or every attempt at a prompt, was invalid. Nothing was
    /// written.
    #[error("invalid answer for {0}: expected {hint}", hint = .0.hint())]
    InvalidAnswer(InitQuestion),
    /// The house was registered, but pinning its guidance failed. Rerunning
    /// `house init` with the same answers resumes: identical content is kept.
    #[error(
        "registered the house, but its guidance was not pinned ({source}); rerun house init with the same answers"
    )]
    GuidanceNotPinned {
        /// Why the snapshot was not installed.
        source: HouseError,
    },
    /// Registry validation or storage failed.
    #[error(transparent)]
    House(#[from] HouseError),
    /// Reading an answer or writing a prompt failed.
    #[error("prompt input or output failed ({0:?})")]
    Io(std::io::ErrorKind),
}

impl HouseInitError {
    /// Common CLI/recovery handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::MissingAnswers(_) | Self::InvalidAnswer(_) => ErrorClass::InvalidInput,
            Self::GuidanceNotPinned { source } | Self::House(source) => source.class(),
            Self::Io(_) => ErrorClass::Execution,
        }
    }
}

impl From<std::io::Error> for HouseInitError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}

fn flags(questions: &[InitQuestion]) -> String {
    questions
        .iter()
        .map(|question| question.flag())
        .collect::<Vec<_>>()
        .join(", ")
}
