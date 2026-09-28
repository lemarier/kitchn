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
    /// The build recorded no Kitchen commit, so the embedded guidance cannot
    /// be labelled with one. Nothing was written.
    #[error(
        "this kitchen binary did not record the commit it was built from, so it cannot label its built-in guidance; install with `just install` from a clean checkout, or pass --bundle <path> with a verified instruction bundle"
    )]
    BuildCommitUnknown,
    /// `--kitchen` names a commit other than this build's. Nothing was
    /// written.
    #[error(
        "--kitchen is not the commit this binary was built from, so its built-in guidance cannot be pinned there; drop --kitchen or pass --bundle with a verified instruction bundle"
    )]
    KitchenNotThisBuild,
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
            Self::MissingAnswers(_)
            | Self::InvalidAnswer(_)
            | Self::BuildCommitUnknown
            | Self::KitchenNotThisBuild => ErrorClass::InvalidInput,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_build_commit_names_both_fixes() {
        let message = HouseInitError::BuildCommitUnknown.to_string();
        assert!(
            message.contains("install with `just install` from a clean checkout")
                && message.contains("pass --bundle <path>"),
            "{message}"
        );
        assert_eq!(
            HouseInitError::BuildCommitUnknown.class(),
            ErrorClass::InvalidInput
        );
    }
}
