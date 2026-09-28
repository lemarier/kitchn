//! Loading a template directory and rendering it in memory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    HouseId,
    adoption::{FileMode, InstructionAsset, MAX_INSTALL_BYTES, MAX_INSTALL_FILES, RelativePath},
    contracts::CommitId,
    scaffold::{
        Manifest, MissingVariable, ScaffoldError, ScaffoldLimit, ScaffoldOperation, TemplateName,
        TemplateProblem, TemplateProvenance, VariableName, provenance::mark,
    },
};

/// Maximum size of `template.toml` in bytes.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Maximum number of files under a template's `files/` directory: one fewer
/// than the installer's batch, reserving a slot for the repository binding.
pub const MAX_TEMPLATE_FILES: usize = MAX_INSTALL_FILES - 1;
/// Maximum size of one source file in bytes.
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024;
/// Maximum size of one rendered file in bytes, including its marker.
pub const MAX_RENDERED_BYTES: usize = 256 * 1024;
/// Installer bytes reserved for the repository binding in every plan.
pub const MAX_BINDING_BYTES: usize = 64 * 1024;
/// Maximum combined size of a template's rendered files in bytes, leaving
/// [`MAX_BINDING_BYTES`] of the installer's batch for the repository binding.
pub const MAX_TEMPLATE_OUTPUT_BYTES: usize = MAX_INSTALL_BYTES - MAX_BINDING_BYTES;
/// Maximum size of one variable value in bytes.
pub const MAX_VARIABLE_BYTES: usize = 1024;
/// Maximum rendered output path length, matching the installer's path bound.
pub const MAX_OUTPUT_PATH_BYTES: usize = 1024;
/// Maximum directory nesting under a template's `files/` directory.
pub const MAX_TEMPLATE_DEPTH: usize = 16;

const MANIFEST_FILE: &str = "template.toml";
/// Directory of house guidance that holds one subdirectory per template.
const GUIDANCE_TEMPLATES: &str = "templates";
const SOURCES_DIR: &str = "files";

/// A loaded, validated house template.
///
/// A template directory holds `template.toml` and a `files/` tree. Every file
/// under `files/` must be listed in the manifest; symbolic links and special
/// files are rejected. Sources and output paths are parsed when the template
/// loads, so syntax errors surface before any rendering.
///
/// Rendering bounds output size and so memory use, but not CPU time: a house
/// is trusted to supply templates that terminate.
#[derive(Debug)]
pub struct Template {
    manifest: Manifest,
    engine: tera::Tera,
    sources: BTreeMap<RelativePath, String>,
}

impl Template {
    /// Load and validate a template directory.
    ///
    /// # Errors
    /// Returns [`ScaffoldError`] for unreadable, oversized, inconsistent, or
    /// unparsable templates.
    pub fn load(dir: &Path) -> Result<Self, ScaffoldError> {
        let manifest_path = dir.join(MANIFEST_FILE);
        let manifest = Manifest::parse(&read_bounded(
            &manifest_path,
            MAX_MANIFEST_BYTES,
            ScaffoldLimit::ManifestBytes,
        )?)?;
        let sources = read_sources(&dir.join(SOURCES_DIR))?;
        Self::from_parts(manifest, sources)
    }

    /// Load template `name` from a house's verified guidance assets.
    ///
    /// The template is `templates/<name>/template.toml` and every asset below
    /// `templates/<name>/files/`. Pass assets from the immutable snapshot for
    /// the guidance revision the output will be stamped with, so provenance
    /// markers can only name content that revision actually contains.
    ///
    /// # Errors
    /// Returns [`ScaffoldError::TemplateNotFound`] when the assets hold no
    /// manifest for `name`, [`TemplateProblem::NameMismatch`] when the manifest
    /// declares another name, and the same validation errors as [`Template::load`].
    pub fn from_guidance(
        assets: &[InstructionAsset],
        name: &TemplateName,
    ) -> Result<Self, ScaffoldError> {
        let prefix = format!("{GUIDANCE_TEMPLATES}/{name}/");
        let manifest_path = format!("{prefix}{MANIFEST_FILE}");
        let files_prefix = format!("{prefix}{SOURCES_DIR}/");
        let mut manifest = None;
        let mut sources = BTreeMap::new();
        for asset in assets {
            let path = asset.path.as_str();
            if path == manifest_path {
                if u64::try_from(asset.contents.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
                    return Err(ScaffoldError::Limit {
                        limit: ScaffoldLimit::ManifestBytes,
                    });
                }
                manifest = Some(Manifest::parse(&asset.contents)?);
            } else if let Some(source) = path.strip_prefix(&files_prefix) {
                // One component per directory level, plus the file itself.
                if source.split('/').count() > MAX_TEMPLATE_DEPTH + 1 {
                    return Err(ScaffoldError::Limit {
                        limit: ScaffoldLimit::TemplateDepth,
                    });
                }
                if sources.len() >= MAX_TEMPLATE_FILES {
                    return Err(ScaffoldError::Limit {
                        limit: ScaffoldLimit::TemplateFiles,
                    });
                }
                let source =
                    RelativePath::new(source).map_err(|_| ScaffoldError::InvalidSourceName {
                        path: PathBuf::from(path),
                    })?;
                sources.insert(source, asset.contents.clone());
            }
        }
        let manifest =
            manifest.ok_or_else(|| ScaffoldError::TemplateNotFound { name: name.clone() })?;
        if manifest.name != *name {
            return Err(template_problem(TemplateProblem::NameMismatch));
        }
        Self::from_parts(manifest, sources)
    }

    /// Build a template from a manifest and in-memory sources keyed by their
    /// path under `files/`.
    ///
    /// # Errors
    /// Returns [`ScaffoldError`] when sources and manifest disagree or a source
    /// does not parse.
    pub fn from_parts(
        manifest: Manifest,
        sources: BTreeMap<RelativePath, String>,
    ) -> Result<Self, ScaffoldError> {
        if sources.len() > MAX_TEMPLATE_FILES {
            return Err(ScaffoldError::Limit {
                limit: ScaffoldLimit::TemplateFiles,
            });
        }
        let listed: BTreeSet<&RelativePath> =
            manifest.files.iter().map(|entry| &entry.source).collect();
        if let Some(missing) = listed.iter().find(|source| !sources.contains_key(**source)) {
            return Err(template_problem(TemplateProblem::MissingSource(
                (*missing).clone(),
            )));
        }
        if let Some(unlisted) = sources.keys().find(|source| !listed.contains(source)) {
            return Err(template_problem(TemplateProblem::UnlistedSource(
                unlisted.clone(),
            )));
        }
        if sources
            .values()
            .any(|text| u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_SOURCE_BYTES)
        {
            return Err(ScaffoldError::Limit {
                limit: ScaffoldLimit::SourceBytes,
            });
        }
        for entry in &manifest.files {
            let mut seen = BTreeSet::new();
            if entry.requires.iter().any(|required| {
                *required == entry.source || !listed.contains(required) || !seen.insert(required)
            }) {
                return Err(template_problem(TemplateProblem::InvalidRequirement(
                    entry.source.clone(),
                )));
            }
        }
        let mut engine = tera::Tera::new();
        engine.autoescape_on(Vec::<&'static str>::new());
        for entry in &manifest.files {
            let syntax_error = |error: tera::Error| ScaffoldError::Syntax {
                path: entry.source.clone(),
                message: error.to_string(),
            };
            // Source paths cannot contain ':', so these names never collide.
            engine
                .add_raw_template(&path_template_name(&entry.source), entry.output_template())
                .map_err(syntax_error)?;
            if entry.render {
                let text = sources.get(&entry.source).ok_or_else(|| {
                    template_problem(TemplateProblem::MissingSource(entry.source.clone()))
                })?;
                engine
                    .add_raw_template(entry.source.as_str(), text)
                    .map_err(syntax_error)?;
            }
        }
        Ok(Self {
            manifest,
            engine,
            sources,
        })
    }

    /// The validated manifest.
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Render every file in memory. Nothing is written.
    ///
    /// Templates see `house`, `template`, `template_revision`, and
    /// `guidance_revision` under `kitchen`, and variable values under `vars`.
    /// Provenance records `guidance` as given; [`super::plan_repository`]
    /// passes the revision whose verified snapshot supplied the template.
    ///
    /// # Errors
    /// Returns [`ScaffoldError::CrossHouse`] when `house` does not own the
    /// template, variable errors for unknown, missing, or invalid values, and
    /// render or output-path errors.
    pub fn render(
        &self,
        house: &HouseId,
        guidance: &CommitId,
        variables: &BTreeMap<VariableName, String>,
    ) -> Result<RenderedTemplate, ScaffoldError> {
        if *house != self.manifest.house {
            return Err(ScaffoldError::CrossHouse {
                selected: house.clone(),
                template: self.manifest.house.clone(),
            });
        }
        let provenance = TemplateProvenance {
            house: house.clone(),
            template: self.manifest.name.clone(),
            revision: self.manifest.revision,
            guidance: guidance.clone(),
        };
        let mut context = tera::Context::new();
        context.insert(
            "kitchen",
            &KitchenContext {
                house: house.as_str(),
                template: provenance.template.as_str(),
                template_revision: provenance.revision.get(),
                guidance_revision: guidance.as_str(),
            },
        );
        context.insert("vars", &self.resolve_variables(variables)?);

        let mut files = Vec::with_capacity(self.manifest.files.len());
        let mut total = 0_usize;
        for entry in &self.manifest.files {
            let render_error = |error: tera::Error| ScaffoldError::Render {
                path: entry.source.clone(),
                message: error.to_string(),
            };
            let invalid_path = || ScaffoldError::InvalidOutputPath {
                source_path: entry.source.clone(),
            };
            let output = self
                .render_bounded(
                    &path_template_name(&entry.source),
                    &context,
                    MAX_OUTPUT_PATH_BYTES,
                )
                .map_err(|error| match error {
                    Bounded::Overflow => invalid_path(),
                    Bounded::Failed(error) => render_error(error),
                })?;
            let path = RelativePath::new(&output).map_err(|_| invalid_path())?;
            let body = if entry.render {
                self.render_bounded(entry.source.as_str(), &context, MAX_RENDERED_BYTES)
                    .map_err(|error| match error {
                        Bounded::Overflow => ScaffoldError::Limit {
                            limit: ScaffoldLimit::RenderedBytes,
                        },
                        Bounded::Failed(error) => render_error(error),
                    })?
            } else {
                self.sources.get(&entry.source).cloned().ok_or_else(|| {
                    template_problem(TemplateProblem::MissingSource(entry.source.clone()))
                })?
            };
            let contents = match entry.provenance {
                Some(style) => {
                    if entry.mode == FileMode::Executable
                        || body.starts_with("#!")
                        || body.starts_with("---")
                        || body.starts_with("+++")
                    {
                        return Err(template_problem(TemplateProblem::MarkerPlacement(
                            entry.source.clone(),
                        )));
                    }
                    mark(style, &provenance, &body)
                }
                None => body,
            };
            if contents.len() > MAX_RENDERED_BYTES {
                return Err(ScaffoldError::Limit {
                    limit: ScaffoldLimit::RenderedBytes,
                });
            }
            total = total
                .checked_add(contents.len())
                .filter(|total| *total <= MAX_TEMPLATE_OUTPUT_BYTES)
                .ok_or(ScaffoldError::Limit {
                    limit: ScaffoldLimit::TotalRenderedBytes,
                })?;
            files.push(RenderedFile {
                path,
                contents,
                mode: entry.mode,
                managed: entry.provenance.is_some(),
                requires: Vec::new(),
            });
        }
        check_outputs(&files)?;
        // Requirements name sources; plans compare output paths.
        let outputs: BTreeMap<&RelativePath, RelativePath> = self
            .manifest
            .files
            .iter()
            .zip(&files)
            .map(|(entry, file)| (&entry.source, file.path.clone()))
            .collect();
        for (entry, file) in self.manifest.files.iter().zip(files.iter_mut()) {
            file.requires = entry
                .requires
                .iter()
                .filter_map(|source| outputs.get(source).cloned())
                .collect();
        }
        Ok(RenderedTemplate { provenance, files })
    }

    /// Render one registered template, failing once output exceeds `limit`
    /// bytes so a runaway loop cannot exhaust memory.
    fn render_bounded(
        &self,
        name: &str,
        context: &tera::Context,
        limit: usize,
    ) -> Result<String, Bounded> {
        let mut buffer = BoundedBuffer {
            bytes: Vec::new(),
            limit,
            overflowed: false,
        };
        let result = self.engine.render_to(name, context, &mut buffer);
        if buffer.overflowed {
            return Err(Bounded::Overflow);
        }
        result.map_err(Bounded::Failed)?;
        String::from_utf8(buffer.bytes)
            .map_err(|_| Bounded::Failed(tera::Error::message("output is not UTF-8")))
    }

    fn resolve_variables<'a>(
        &'a self,
        supplied: &'a BTreeMap<VariableName, String>,
    ) -> Result<BTreeMap<&'a str, &'a str>, ScaffoldError> {
        if supplied.len() > super::MAX_VARIABLES {
            return Err(ScaffoldError::Limit {
                limit: ScaffoldLimit::Variables,
            });
        }
        if let Some(unknown) = supplied
            .keys()
            .find(|name| !self.manifest.variables.contains_key(*name))
        {
            return Err(ScaffoldError::UnknownVariable {
                name: unknown.clone(),
            });
        }
        let mut resolved = BTreeMap::new();
        let mut missing = Vec::new();
        for (name, spec) in &self.manifest.variables {
            let Some(value) = supplied.get(name).or(spec.default.as_ref()) else {
                missing.push(MissingVariable {
                    name: name.clone(),
                    description: spec.description.clone(),
                });
                continue;
            };
            if value.len() > MAX_VARIABLE_BYTES {
                return Err(ScaffoldError::Limit {
                    limit: ScaffoldLimit::VariableBytes,
                });
            }
            if value.chars().any(char::is_control) || !spec.kind.accepts(value) {
                return Err(ScaffoldError::InvalidVariableValue { name: name.clone() });
            }
            resolved.insert(name.as_str(), value.as_str());
        }
        if !missing.is_empty() {
            return Err(ScaffoldError::MissingVariables { variables: missing });
        }
        Ok(resolved)
    }
}

#[derive(Serialize)]
struct KitchenContext<'a> {
    house: &'a str,
    template: &'a str,
    template_revision: u32,
    guidance_revision: &'a str,
}

fn path_template_name(source: &RelativePath) -> String {
    format!("path:{}", source.as_str())
}

/// Whether `parent` is a strict ancestor directory of `child`.
fn is_ancestor(parent: &RelativePath, child: &RelativePath) -> bool {
    child
        .as_str()
        .strip_prefix(parent.as_str())
        .is_some_and(|rest| rest.starts_with('/'))
}

enum Bounded {
    Overflow,
    Failed(tera::Error),
}

/// Collects rendered output and fails once it exceeds `limit` bytes.
struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl io::Write for BoundedBuffer {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(data.len()) > self.limit {
            self.overflowed = true;
            return Err(io::Error::other("rendered output exceeds its limit"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_outputs(files: &[RenderedFile]) -> Result<(), ScaffoldError> {
    let mut paths: Vec<&RelativePath> = files.iter().map(|file| &file.path).collect();
    paths.sort();
    for pair in paths.windows(2) {
        let [first, second] = pair else { continue };
        if first == second {
            return Err(template_problem(TemplateProblem::DuplicateOutput(
                (*first).clone(),
            )));
        }
    }
    // A parent sorts before its children, but unrelated paths can sit between
    // them (`a/b` < `a/b.txt` < `a/b/c`), so compare against every later path.
    for (index, parent) in paths.iter().enumerate() {
        if let Some(child) = paths
            .iter()
            .skip(index + 1)
            .find(|path| is_ancestor(parent, path))
        {
            return Err(template_problem(TemplateProblem::OutputNesting {
                parent: (*parent).clone(),
                child: (*child).clone(),
            }));
        }
    }
    Ok(())
}

const fn template_problem(problem: TemplateProblem) -> ScaffoldError {
    ScaffoldError::Template { problem }
}

fn read_bounded(path: &Path, limit: u64, bound: ScaffoldLimit) -> Result<String, ScaffoldError> {
    let io_error = |error: &io::Error| {
        ScaffoldError::io(ScaffoldOperation::ReadTemplate, path.to_path_buf(), error)
    };
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(&error))?;
    if !metadata.is_file() {
        return Err(ScaffoldError::io(
            ScaffoldOperation::ReadTemplate,
            path.to_path_buf(),
            &io::Error::from(io::ErrorKind::InvalidInput),
        ));
    }
    let mut bytes = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(
            fs::File::open(path).map_err(|error| io_error(&error))?,
            limit.saturating_add(1),
        ),
        &mut bytes,
    )
    .map_err(|error| io_error(&error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(ScaffoldError::Limit { limit: bound });
    }
    String::from_utf8(bytes).map_err(|_| {
        ScaffoldError::io(
            ScaffoldOperation::ReadTemplate,
            path.to_path_buf(),
            &io::Error::from(io::ErrorKind::InvalidData),
        )
    })
}

/// Read every file under `root`, rejecting links and special files.
fn read_sources(root: &Path) -> Result<BTreeMap<RelativePath, String>, ScaffoldError> {
    let metadata = fs::symlink_metadata(root).map_err(|error| {
        ScaffoldError::io(ScaffoldOperation::ReadTemplate, root.to_path_buf(), &error)
    })?;
    if !metadata.is_dir() {
        return Err(ScaffoldError::io(
            ScaffoldOperation::ReadTemplate,
            root.to_path_buf(),
            &io::Error::from(io::ErrorKind::InvalidInput),
        ));
    }
    let mut sources = BTreeMap::new();
    let mut pending: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = pending.pop() {
        if depth > MAX_TEMPLATE_DEPTH {
            return Err(ScaffoldError::Limit {
                limit: ScaffoldLimit::TemplateDepth,
            });
        }
        let entries = fs::read_dir(&dir).map_err(|error| {
            ScaffoldError::io(ScaffoldOperation::ReadTemplate, dir.clone(), &error)
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ScaffoldError::io(ScaffoldOperation::ReadTemplate, dir.clone(), &error)
            })?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                ScaffoldError::io(
                    ScaffoldOperation::ReadTemplate,
                    entry.path(),
                    &io::Error::from(io::ErrorKind::InvalidData),
                )
            })?;
            let relative = RelativePath::new(&format!("{prefix}{name}"))
                .map_err(|_| ScaffoldError::InvalidSourceName { path: entry.path() })?;
            let file_type = entry.file_type().map_err(|error| {
                ScaffoldError::io(ScaffoldOperation::ReadTemplate, entry.path(), &error)
            })?;
            if file_type.is_dir() {
                pending.push((entry.path(), format!("{}/", relative.as_str()), depth + 1));
            } else if file_type.is_file() {
                if sources.len() >= MAX_TEMPLATE_FILES {
                    return Err(ScaffoldError::Limit {
                        limit: ScaffoldLimit::TemplateFiles,
                    });
                }
                let text =
                    read_bounded(&entry.path(), MAX_SOURCE_BYTES, ScaffoldLimit::SourceBytes)
                        .map_err(|error| match error {
                            ScaffoldError::Io {
                                kind: io::ErrorKind::InvalidData,
                                ..
                            } => template_problem(TemplateProblem::NotUtf8(relative.clone())),
                            other => other,
                        })?;
                sources.insert(relative, text);
            } else {
                return Err(template_problem(TemplateProblem::NotRegularFile(relative)));
            }
        }
    }
    Ok(sources)
}

/// A file rendered in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// Output path in the repository.
    pub path: RelativePath,
    /// Full contents, including any provenance marker.
    pub contents: String,
    /// Output permission class.
    pub mode: FileMode,
    /// Whether the file carries a provenance marker.
    pub managed: bool,
    /// Output paths that must be added or already present for this file to
    /// be added.
    pub requires: Vec<RelativePath>,
}

/// A template rendered in memory, ready to plan against a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTemplate {
    /// Where the content came from.
    pub provenance: TemplateProvenance,
    /// Rendered files in manifest order.
    pub files: Vec<RenderedFile>,
}
