//! Template, rendering, and planning failures.

use std::{fmt, io, path::PathBuf};

use crate::{ErrorClass, HouseId};

use super::{TemplateName, VariableName};
use crate::adoption::RelativePath;

/// A size or count bound that a template or its output exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScaffoldLimit {
    /// The `template.toml` manifest size.
    ManifestBytes,
    /// The number of files under `files/`.
    TemplateFiles,
    /// The size of one source file.
    SourceBytes,
    /// The size of one rendered file.
    RenderedBytes,
    /// The combined size of all rendered files and the repository binding.
    TotalRenderedBytes,
    /// The number of declared or supplied variables.
    Variables,
    /// The size of one variable value.
    VariableBytes,
    /// Directory nesting under `files/`.
    TemplateDepth,
}

impl fmt::Display for ScaffoldLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ManifestBytes => "manifest size",
            Self::TemplateFiles => "template file count",
            Self::SourceBytes => "template source size",
            Self::RenderedBytes => "rendered file size",
            Self::TotalRenderedBytes => "total rendered size",
            Self::Variables => "variable count",
            Self::VariableBytes => "variable value size",
            Self::TemplateDepth => "template directory depth",
        })
    }
}

/// A filesystem operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScaffoldOperation {
    /// Reading a template directory or file.
    ReadTemplate,
    /// Inspecting the target repository.
    InspectTarget,
}

impl fmt::Display for ScaffoldOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadTemplate => "read template",
            Self::InspectTarget => "inspect target",
        })
    }
}

/// A template, rendering, or planning failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScaffoldError {
    /// A file under a template's `files/` directory has an unusable name.
    #[error("template entry {} has an invalid name", path.display())]
    InvalidSourceName {
        /// The rejected entry.
        path: PathBuf,
    },
    /// A template output path was rejected after rendering: it was empty,
    /// absolute, too long, escaped the target, addressed Git metadata, or
    /// contained unsafe characters.
    #[error("output path of template source {} is invalid", source_path.as_str())]
    InvalidOutputPath {
        /// The source whose output path was rejected.
        source_path: RelativePath,
    },
    /// `template.toml` does not match the schema.
    #[error("invalid template manifest: {message}")]
    Manifest {
        /// The parser's description.
        message: String,
    },
    /// The template's sources and manifest disagree.
    #[error("invalid template: {problem}")]
    Template {
        /// What is inconsistent.
        problem: TemplateProblem,
    },
    /// A source file failed to parse as a template.
    #[error("template source {} does not parse: {message}", path.as_str())]
    Syntax {
        /// The source file.
        path: RelativePath,
        /// The template engine's description.
        message: String,
    },
    /// A source file failed to render.
    #[error("template source {} failed to render: {message}", path.as_str())]
    Render {
        /// The source file.
        path: RelativePath,
        /// The template engine's description.
        message: String,
    },
    /// The selected guidance revision contains no template with this name.
    #[error("the pinned house guidance has no template named {name}")]
    TemplateNotFound {
        /// The requested template.
        name: TemplateName,
    },
    /// Required variables have no value and no default. Lists every one in
    /// manifest (name) order.
    #[error("missing values for template variables: {}", list_missing(variables))]
    MissingVariables {
        /// Each missing variable with its declared description.
        variables: Vec<MissingVariable>,
    },
    /// A variable assignment is not of the form `name=value`.
    #[error("variable assignments must have the form name=value")]
    MalformedAssignment,
    /// A variable was assigned more than once.
    #[error("template variable {name} is assigned more than once")]
    DuplicateVariable {
        /// The variable.
        name: VariableName,
    },
    /// `init` was pointed at a directory that already has content.
    #[error(
        "target {} is not empty; use `kitchen adopt` to add template files to an existing directory",
        path.display()
    )]
    TargetNotEmpty {
        /// The existing directory.
        path: PathBuf,
    },
    /// A supplied variable is not declared by the template.
    #[error("template does not declare variable {name}")]
    UnknownVariable {
        /// The variable.
        name: VariableName,
    },
    /// A variable name is not a lowercase identifier.
    #[error(
        "variable names are 1 to 64 bytes: a lowercase ASCII letter followed by lowercase letters, digits, or '_'"
    )]
    InvalidVariableName,
    /// A variable value violates its declared kind or contains a control character.
    #[error("value of template variable {name} violates its kind or contains a control character")]
    InvalidVariableValue {
        /// The variable.
        name: VariableName,
    },
    /// A bound was exceeded.
    #[error("template exceeds the {limit} limit")]
    Limit {
        /// The exceeded bound.
        limit: ScaffoldLimit,
    },
    /// The template belongs to a different house than the one selected.
    #[error("template belongs to house {template}, not the selected house {selected}")]
    CrossHouse {
        /// The house the caller selected.
        selected: HouseId,
        /// The house the template declares.
        template: HouseId,
    },
    /// The target directory is a symbolic link or not a directory.
    #[error("target {} is not a real directory", path.display())]
    UntrustedTarget {
        /// The rejected target.
        path: PathBuf,
    },
    /// The target path is relative; plans need an explicit absolute location.
    #[error("target path must be absolute")]
    RelativeTarget,
    /// A filesystem operation failed.
    #[error("{operation} failed at {}: {kind}", path.display())]
    Io {
        /// What was attempted.
        operation: ScaffoldOperation,
        /// Where.
        path: PathBuf,
        /// The I/O failure kind.
        kind: io::ErrorKind,
    },
}

impl ScaffoldError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidSourceName { .. }
            | Self::InvalidOutputPath { .. }
            | Self::Manifest { .. }
            | Self::Template { .. }
            | Self::Syntax { .. }
            | Self::Render { .. }
            | Self::TemplateNotFound { .. }
            | Self::MissingVariables { .. }
            | Self::MalformedAssignment
            | Self::DuplicateVariable { .. }
            | Self::TargetNotEmpty { .. }
            | Self::UnknownVariable { .. }
            | Self::InvalidVariableName
            | Self::InvalidVariableValue { .. }
            | Self::Limit { .. }
            | Self::RelativeTarget => ErrorClass::InvalidInput,
            Self::CrossHouse { .. } | Self::UntrustedTarget { .. } => ErrorClass::Refused,
            Self::Io { .. } => ErrorClass::Execution,
        }
    }

    pub(super) fn io(operation: ScaffoldOperation, path: PathBuf, error: &io::Error) -> Self {
        Self::Io {
            operation,
            path,
            kind: error.kind(),
        }
    }
}

/// A required template variable that has no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingVariable {
    /// The variable.
    pub name: VariableName,
    /// Its manifest description, shown so the caller knows what to supply.
    pub description: String,
}

fn list_missing(variables: &[MissingVariable]) -> String {
    variables
        .iter()
        .map(|variable| format!("{} ({:?})", variable.name, variable.description))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An inconsistency between a template's manifest and its source files.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TemplateProblem {
    /// A source listed in the manifest does not exist under `files/`.
    MissingSource(RelativePath),
    /// A file under `files/` is not listed in the manifest.
    UnlistedSource(RelativePath),
    /// A source is listed more than once.
    DuplicateSource(RelativePath),
    /// Two entries render to the same output path.
    DuplicateOutput(RelativePath),
    /// One output path is a directory of another.
    OutputNesting {
        /// The output used as a directory.
        parent: RelativePath,
        /// The output beneath it.
        child: RelativePath,
    },
    /// An entry under `files/` is a symbolic link or special file.
    NotRegularFile(RelativePath),
    /// A source file is not UTF-8.
    NotUtf8(RelativePath),
    /// The manifest schema version is not supported.
    UnsupportedSchema(u32),
    /// The template has no files.
    Empty,
    /// A marker would displace a shebang/front matter, or mark an executable.
    MarkerPlacement(RelativePath),
    /// The manifest declares a different name than the guidance directory
    /// the template was selected from.
    NameMismatch,
}

impl fmt::Display for TemplateProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource(path) => {
                write!(formatter, "listed source {} does not exist", path.as_str())
            }
            Self::UnlistedSource(path) => {
                write!(formatter, "source {} is not listed", path.as_str())
            }
            Self::DuplicateSource(path) => {
                write!(formatter, "source {} is listed twice", path.as_str())
            }
            Self::DuplicateOutput(path) => {
                write!(formatter, "output {} is produced twice", path.as_str())
            }
            Self::OutputNesting { parent, child } => write!(
                formatter,
                "output {} is nested under output {}",
                child.as_str(),
                parent.as_str()
            ),
            Self::NotRegularFile(path) => write!(
                formatter,
                "{} is a symbolic link or special file",
                path.as_str()
            ),
            Self::NotUtf8(path) => write!(formatter, "source {} is not UTF-8", path.as_str()),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "manifest schema {version} is not supported")
            }
            Self::Empty => formatter.write_str("template lists no files"),
            Self::MarkerPlacement(path) => write!(
                formatter,
                "{} cannot carry a first-line provenance marker",
                path.as_str()
            ),
            Self::NameMismatch => {
                formatter.write_str("manifest name differs from its guidance directory")
            }
        }
    }
}
