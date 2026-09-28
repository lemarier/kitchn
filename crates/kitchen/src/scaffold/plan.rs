//! Classifying rendered files against a target directory, the preview, and
//! the additions-only apply step.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    adoption::{self, FileMode, FileStatus, InstallReport, NewFile, SafeInstaller},
    house::HouseError,
    scaffold::{
        ManagedMarker, ManagedState, RenderedFile, RenderedTemplate, ScaffoldError,
        ScaffoldOperation, TemplateProvenance, inspect_managed,
    },
};

/// Whether the plan creates a repository or adopts an existing directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanKind {
    /// The target does not exist yet.
    NewRepository,
    /// The target is an existing directory.
    Adoption,
}

/// Why an existing path blocks a planned file. Conflicts are never applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// A local file with different content and no provenance marker.
    Unmanaged,
    /// An earlier render that was not edited; a later update may replace it.
    ManagedPristine(Box<ManagedMarker>),
    /// An earlier render with local edits, which must be preserved.
    ManagedEdited(Box<ManagedMarker>),
    /// Identical content with a different permission class.
    ModeDiffers {
        /// The planned mode.
        planned: FileMode,
    },
    /// The path exists as a directory or special file.
    NotRegularFile,
}

impl fmt::Display for Conflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmanaged => formatter.write_str("existing file differs"),
            Self::ManagedPristine(marker) => write!(
                formatter,
                "managed by {} revision {} and unedited; upstream content or provenance differs; reconcile manually",
                marker.provenance.template, marker.provenance.revision
            ),
            Self::ManagedEdited(marker) => write!(
                formatter,
                "managed by {} revision {} with local edits",
                marker.provenance.template, marker.provenance.revision
            ),
            Self::ModeDiffers { planned } => write!(
                formatter,
                "content matches but the file is {}executable",
                match planned {
                    FileMode::Executable => "not ",
                    FileMode::Regular => "",
                }
            ),
            Self::NotRegularFile => formatter.write_str("path is a directory or special file"),
        }
    }
}

/// What applying the plan would do with one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// Create the file; nothing exists at its path.
    Add,
    /// The file already exists with the planned content and mode.
    Unchanged,
    /// Something exists that the plan must not overwrite.
    Conflict(Conflict),
}

/// One rendered file and its classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// The rendered file.
    pub file: RenderedFile,
    /// What applying would do.
    pub action: PlanAction,
}

/// A validated, in-memory plan. Building it writes nothing.
///
/// Classification comes from the create-only installer's preview, so the plan
/// and the apply step use the same path checks. [`FilePlan::apply`] passes only
/// the additions to the installer, which rechecks every path when it writes.
/// Conflicts are reported, never overwritten, and nothing is deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    provenance: TemplateProvenance,
    target: PathBuf,
    kind: PlanKind,
    files: Vec<PlannedFile>,
}

impl FilePlan {
    /// Classify rendered files against `target`.
    ///
    /// `target` must be absolute. It may be missing (a new repository) or a
    /// real directory. A target that is a file or symbolic link, or that is
    /// reached through one, is refused; so is a planned path that passes
    /// through a symbolic link or special file.
    ///
    /// # Errors
    /// Returns [`ScaffoldError::RelativeTarget`] or
    /// [`ScaffoldError::UntrustedTarget`], and the installer's refusals for
    /// redirected paths, invalid batches, or I/O failures.
    pub fn new(rendered: RenderedTemplate, target: &Path) -> crate::Result<Self> {
        if !target.is_absolute() {
            return Err(ScaffoldError::RelativeTarget.into());
        }
        let kind = match fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_dir() => PlanKind::Adoption,
            Ok(_) => {
                return Err(ScaffoldError::UntrustedTarget {
                    path: target.to_path_buf(),
                }
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => PlanKind::NewRepository,
            Err(error) => {
                return Err(ScaffoldError::io(
                    ScaffoldOperation::InspectTarget,
                    target.to_path_buf(),
                    &error,
                )
                .into());
            }
        };
        let preview = SafeInstaller::preview(target, &new_files(rendered.files.iter()))?;
        let mut files = Vec::with_capacity(rendered.files.len());
        for (file, decision) in rendered.files.into_iter().zip(preview.files) {
            let action = match decision.status {
                FileStatus::Created => PlanAction::Add,
                FileStatus::AlreadyIdentical => PlanAction::Unchanged,
                FileStatus::Conflict => PlanAction::Conflict(explain(target, &file)?),
            };
            files.push(PlannedFile { file, action });
        }
        Ok(Self {
            provenance: rendered.provenance,
            target: target.to_path_buf(),
            kind,
            files,
        })
    }

    /// Where the content comes from.
    #[must_use]
    pub const fn provenance(&self) -> &TemplateProvenance {
        &self.provenance
    }

    /// The target directory.
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// New repository or adoption.
    #[must_use]
    pub const fn kind(&self) -> PlanKind {
        self.kind
    }

    /// Every planned file in template order.
    #[must_use]
    pub fn files(&self) -> &[PlannedFile] {
        &self.files
    }

    /// Files the apply step would create.
    pub fn additions(&self) -> impl Iterator<Item = &RenderedFile> {
        self.files
            .iter()
            .filter(|planned| planned.action == PlanAction::Add)
            .map(|planned| &planned.file)
    }

    /// Files left untouched because something else is in the way.
    pub fn conflicts(&self) -> impl Iterator<Item = (&RenderedFile, &Conflict)> {
        self.files
            .iter()
            .filter_map(|planned| match &planned.action {
                PlanAction::Conflict(conflict) => Some((&planned.file, conflict)),
                PlanAction::Add | PlanAction::Unchanged => None,
            })
    }

    /// Whether applying would change nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.additions().next().is_none()
    }

    /// Create the planned additions through the create-only installer.
    ///
    /// Call this only after the preview was shown and confirmed. Conflicting
    /// files are not passed to the installer and stay untouched. If anything
    /// appeared at an added path since planning, the installer reports a
    /// conflict and writes nothing. On an I/O failure it removes only the
    /// files and directories this call created. A rerun of the same plan is
    /// safe: files already created read as identical.
    ///
    /// # Errors
    /// Returns the installer's refusals and failures, including
    /// [`HouseError::PartialInstallation`] when rollback could not remove
    /// everything it created.
    pub fn apply(&self) -> crate::Result<InstallReport> {
        // Recheck files classified as unchanged before applying additions. This
        // includes the repository binding, so stale house selection is refused.
        let unchanged: Vec<_> = self
            .files
            .iter()
            .filter(|planned| planned.action == PlanAction::Unchanged)
            .map(|planned| &planned.file)
            .collect();
        let preview = SafeInstaller::preview(&self.target, &new_files(unchanged.into_iter()))?;
        if preview
            .files
            .iter()
            .any(|file| file.status != FileStatus::AlreadyIdentical)
        {
            return Err(HouseError::Conflict.into());
        }
        Ok(adoption::install_new_files(
            &self.target,
            &new_files(self.additions()),
        )?)
    }
}

fn new_files<'a>(files: impl Iterator<Item = &'a RenderedFile>) -> Vec<NewFile<'a>> {
    files
        .map(|file| NewFile {
            path: &file.path,
            contents: file.contents.as_bytes(),
            mode: file.mode,
        })
        .collect()
}

/// Explain an installer conflict. Existing content is only read.
fn explain(target: &Path, file: &RenderedFile) -> crate::Result<Conflict> {
    match adoption::read_bounded(&target.join(file.path.as_path())) {
        Ok(existing) if existing == file.contents.as_bytes() => {
            Ok(Conflict::ModeDiffers { planned: file.mode })
        }
        Ok(existing) => Ok(
            match String::from_utf8(existing).as_deref().map(inspect_managed) {
                Ok(ManagedState::Pristine(marker)) => Conflict::ManagedPristine(Box::new(marker)),
                Ok(ManagedState::Edited(marker)) => Conflict::ManagedEdited(Box::new(marker)),
                Ok(ManagedState::Unmanaged) | Err(_) => Conflict::Unmanaged,
            },
        ),
        // The preview already rejected links, so this is a directory or special file.
        Err(HouseError::RedirectedPath) => Ok(Conflict::NotRegularFile),
        // A file larger than the installer reads is certainly not the planned content.
        Err(HouseError::InvalidInput) => Ok(Conflict::Unmanaged),
        Err(error) => Err(error.into()),
    }
}

/// A human-readable preview listing every file and what would happen to it.
impl fmt::Display for FilePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let provenance = &self.provenance;
        writeln!(
            formatter,
            "Template {}/{} revision {}, guidance {}",
            provenance.house, provenance.template, provenance.revision, provenance.guidance
        )?;
        writeln!(
            formatter,
            "Target {} ({})",
            self.target.display(),
            match self.kind {
                PlanKind::NewRepository => "new repository",
                PlanKind::Adoption => "existing directory",
            }
        )?;
        let (mut added, mut unchanged, mut conflicts) = (0_usize, 0_usize, 0_usize);
        for planned in &self.files {
            let path = planned.file.path.as_str();
            let mode = match planned.file.mode {
                FileMode::Executable => " (executable)",
                FileMode::Regular => "",
            };
            match &planned.action {
                PlanAction::Add => {
                    added += 1;
                    writeln!(formatter, "  add        {path}{mode}")?;
                }
                PlanAction::Unchanged => {
                    unchanged += 1;
                    writeln!(formatter, "  unchanged  {path}")?;
                }
                PlanAction::Conflict(conflict) => {
                    conflicts += 1;
                    writeln!(formatter, "  conflict   {path}: {conflict}; left untouched")?;
                }
            }
        }
        write!(
            formatter,
            "{added} to add, {unchanged} unchanged, {conflicts} conflicts. Nothing is written until the plan is applied; existing files are never overwritten or deleted."
        )
    }
}
