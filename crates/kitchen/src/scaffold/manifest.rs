//! The `template.toml` manifest: identity, variables, and file entries.

use std::{collections::BTreeMap, fmt, num::NonZeroU32, str::FromStr};

use serde::Deserialize;

use crate::{
    HouseId,
    adoption::{FileMode, RelativePath},
    scaffold::{ScaffoldError, ScaffoldLimit, TemplateProblem},
};

/// The only supported manifest schema version.
pub const TEMPLATE_SCHEMA: u32 = 1;
/// Maximum number of declared or supplied variables.
pub const MAX_VARIABLES: usize = 64;

/// A template's name within its house.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct TemplateName(String);

impl TemplateName {
    /// Validate a template name. It uses the common identifier syntax.
    ///
    /// # Errors
    /// Returns [`crate::IdentifierError`] without echoing the input.
    pub fn new(value: &str) -> Result<Self, crate::IdentifierError> {
        crate::id::validate_identifier(value)?;
        Ok(Self(value.to_owned()))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TemplateName {
    type Error = crate::IdentifierError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        crate::id::validate_identifier(&value)?;
        Ok(Self(value))
    }
}

impl fmt::Display for TemplateName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A template's revision, increased by its house whenever the template changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(transparent)]
pub struct TemplateRevision(NonZeroU32);

impl TemplateRevision {
    /// Wrap a positive revision number.
    #[must_use]
    pub const fn new(revision: NonZeroU32) -> Self {
        Self(revision)
    }

    /// The revision number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for TemplateRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A template variable name: a lowercase ASCII letter followed by lowercase
/// letters, digits, or `_`, up to 64 bytes, so it is also a template identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct VariableName(String);

impl VariableName {
    /// Validate a variable name.
    ///
    /// # Errors
    /// Returns [`ScaffoldError::InvalidVariableName`] without echoing the input.
    pub fn new(value: &str) -> Result<Self, ScaffoldError> {
        validate_variable_name(value)?;
        Ok(Self(value.to_owned()))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_variable_name(value: &str) -> Result<(), ScaffoldError> {
    let mut bytes = value.bytes();
    let starts_with_letter = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
    if !starts_with_letter
        || value.len() > 64
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ScaffoldError::InvalidVariableName);
    }
    Ok(())
}

impl TryFrom<String> for VariableName {
    type Error = ScaffoldError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_variable_name(&value)?;
        Ok(Self(value))
    }
}

impl FromStr for VariableName {
    type Err = ScaffoldError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for VariableName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A declared template variable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableSpec {
    /// What the value means, for prompts and previews.
    pub description: String,
    /// The value used when none is supplied; without one the variable is required.
    #[serde(default)]
    pub default: Option<String>,
}

/// How a rendered file records its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MarkerStyle {
    /// A first line `<!-- kitchen-managed: … -->`, for Markdown and HTML.
    HtmlComment,
    /// A first line `# kitchen-managed: …`, for TOML, YAML, shell, and justfiles.
    HashComment,
}

/// One file the template produces.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Source file under the template's `files/` directory.
    pub source: RelativePath,
    /// Output path in the repository; itself rendered, then validated.
    /// Defaults to `source` without a trailing `.tera`.
    #[serde(default)]
    pub path: Option<String>,
    /// Output permission class; regular unless stated.
    #[serde(default = "regular")]
    pub mode: FileMode,
    /// Whether the source is rendered as a template or copied unchanged.
    #[serde(default = "render_by_default")]
    pub render: bool,
    /// Provenance marker style; `None` leaves the file unmarked.
    #[serde(default)]
    pub provenance: Option<MarkerStyle>,
}

const fn render_by_default() -> bool {
    true
}

const fn regular() -> FileMode {
    FileMode::Regular
}

impl FileEntry {
    /// The unrendered output path text.
    #[must_use]
    pub fn output_template(&self) -> &str {
        self.path.as_deref().unwrap_or_else(|| {
            let source = self.source.as_str();
            source.strip_suffix(".tera").unwrap_or(source)
        })
    }
}

/// A parsed and validated `template.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Manifest schema version; must equal [`TEMPLATE_SCHEMA`].
    pub schema: u32,
    /// The template name.
    pub name: TemplateName,
    /// The house that owns the template.
    pub house: HouseId,
    /// The template revision.
    pub revision: TemplateRevision,
    /// A one-line description.
    pub description: String,
    /// Declared variables.
    #[serde(default)]
    pub variables: BTreeMap<VariableName, VariableSpec>,
    /// Produced files, in output order.
    pub files: Vec<FileEntry>,
}

impl Manifest {
    /// Parse and validate manifest text.
    ///
    /// # Errors
    /// Returns [`ScaffoldError::Manifest`] for schema mismatches and
    /// [`ScaffoldError::Template`] or [`ScaffoldError::Limit`] for invalid content.
    pub fn parse(text: &str) -> Result<Self, ScaffoldError> {
        let manifest: Self = toml::from_str(text).map_err(|error| ScaffoldError::Manifest {
            message: error.message().to_owned(),
        })?;
        if manifest.schema != TEMPLATE_SCHEMA {
            return Err(ScaffoldError::Template {
                problem: TemplateProblem::UnsupportedSchema(manifest.schema),
            });
        }
        if manifest.variables.len() > MAX_VARIABLES {
            return Err(ScaffoldError::Limit {
                limit: ScaffoldLimit::Variables,
            });
        }
        if manifest.files.is_empty() {
            return Err(ScaffoldError::Template {
                problem: TemplateProblem::Empty,
            });
        }
        let mut sources = std::collections::BTreeSet::new();
        for entry in &manifest.files {
            if !sources.insert(&entry.source) {
                return Err(ScaffoldError::Template {
                    problem: TemplateProblem::DuplicateSource(entry.source.clone()),
                });
            }
        }
        Ok(manifest)
    }
}
