use crate::house::HouseError;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// Error type shared by adoption and house configuration.
pub type AdoptionError = HouseError;
/// Maximum total bytes in one installation (including its manifest).
pub const MAX_INSTALL_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of files in one installation.
pub const MAX_INSTALL_FILES: usize = 256;

/// A validated, portable relative destination, excluding Git metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelativePath(String);
impl RelativePath {
    /// Reject traversal, aliases, empty components, Git metadata and unsafe names.
    pub fn new(value: &str) -> Result<Self, AdoptionError> {
        if value.is_empty()
            || value.len() > 1024
            || value.contains(['\\', '\0', ':'])
            || value.chars().any(char::is_control)
            || value.split('/').any(|part| {
                part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
            })
            || !Path::new(value)
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(HouseError::InvalidInput);
        }
        Ok(Self(value.to_owned()))
    }
    /// Borrow the portable path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Borrow as a native path.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}
impl TryFrom<String> for RelativePath {
    type Error = AdoptionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}
impl From<RelativePath> for String {
    fn from(value: RelativePath) -> Self {
        value.0
    }
}

/// Installation mode; existing modes are never changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileMode {
    /// Data or instructions, owner-readable/writable on Unix.
    Regular,
    /// Executable, owner-readable/writable/executable on Unix.
    Executable,
}
/// One borrowed file to install. Rendering belongs to the caller.
#[derive(Debug, Clone, Copy)]
pub struct NewFile<'a> {
    /// Validated relative destination.
    pub path: &'a RelativePath,
    /// Exact bytes, bounded across the whole batch.
    pub contents: &'a [u8],
    /// Desired mode for a new file.
    pub mode: FileMode,
}
/// Outcome for one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileStatus {
    /// Preview would create, or apply created, the path.
    Created,
    /// Existing regular file has identical bytes and executable status.
    AlreadyIdentical,
    /// Existing content, kind or mode differs; preserved untouched.
    Conflict,
}
/// Per-file result, shared by preview and application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    /// Relative destination.
    pub path: RelativePath,
    /// Decision or result.
    pub status: FileStatus,
}
/// Complete create-only preview/result. A conflict prevents all writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReport {
    /// Every requested destination, in caller order.
    pub files: Vec<InstalledFile>,
}
impl InstallReport {
    /// Whether any destination prevents application.
    pub fn has_conflicts(&self) -> bool {
        self.files
            .iter()
            .any(|file| file.status == FileStatus::Conflict)
    }
}

/// Create-only installer for trusted, locally owned trees. Cooperating callers
/// must serialize writes to a root. Every path is checked again before effects;
/// this does not defend against a malicious process running as the same user
/// racing filesystem checks. Symlinks are never replaced or traversed.
pub struct SafeInstaller;
impl SafeInstaller {
    /// Inspect all paths without changing files or directories.
    pub fn preview(root: &Path, files: &[NewFile<'_>]) -> Result<InstallReport, AdoptionError> {
        validate_files(files)?;
        check_path(root)?;
        let mut report = InstallReport {
            files: Vec::with_capacity(files.len()),
        };
        for file in files {
            let target = root.join(file.path.as_path());
            check_path(&target)?;
            let status = match fs::symlink_metadata(&target) {
                Ok(metadata) => {
                    if !metadata.is_file() {
                        FileStatus::Conflict
                    } else if read_bounded(&target)? == file.contents
                        && mode_matches(&metadata, file.mode)
                    {
                        FileStatus::AlreadyIdentical
                    } else {
                        FileStatus::Conflict
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileStatus::Created,
                Err(error) => return Err(error.into()),
            };
            report.files.push(InstalledFile {
                path: file.path.clone(),
                status,
            });
        }
        Ok(report)
    }
    /// Apply the create-only plan, rechecking immediately before writes.
    pub fn apply(root: &Path, files: &[NewFile<'_>]) -> Result<InstallReport, AdoptionError> {
        install_new_files(root, files)
    }
}

/// Install without overwriting or deleting any pre-existing path. A conflicting
/// batch returns its preview with no effects. On I/O failure, roll back only
/// unchanged files created by this call and empty directories created by it;
/// [`HouseError::PartialInstallation`] lists any paths that could not be removed.
/// A process crash can leave create-only files; an identical rerun resumes safely.
pub fn install_new_files(
    root: &Path,
    files: &[NewFile<'_>],
) -> Result<InstallReport, AdoptionError> {
    install_with_checkpoint(root, files, |_| Ok(()))
}

fn install_with_checkpoint(
    root: &Path,
    files: &[NewFile<'_>],
    mut checkpoint: impl FnMut(usize) -> Result<(), AdoptionError>,
) -> Result<InstallReport, AdoptionError> {
    let report = SafeInstaller::preview(root, files)?;
    if report.has_conflicts() {
        return Ok(report);
    }
    let mut created: Vec<(PathBuf, &[u8], File, usize)> = Vec::new();
    let mut dirs = Vec::new();
    let result = (|| {
        for (index, (file, decision)) in files.iter().zip(&report.files).enumerate() {
            checkpoint(index)?;
            let target = root.join(file.path.as_path());
            if decision.status == FileStatus::AlreadyIdentical {
                check_path(&target)?;
                if read_bounded(&target)? != file.contents
                    || !mode_matches(&fs::symlink_metadata(&target)?, file.mode)
                {
                    return Err(HouseError::Conflict);
                }
                continue;
            }
            if let Some(parent) = target.parent() {
                create_dirs(parent, &mut dirs)?;
            }
            check_path(&target)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(match file.mode {
                    FileMode::Regular => 0o600,
                    FileMode::Executable => 0o700,
                });
            }
            let output = options.open(&target)?;
            created.push((target.clone(), file.contents, output, 0));
            if let Some((_, _, output, written)) = created.last_mut() {
                while *written < file.contents.len() {
                    let count = output.write(&file.contents[*written..])?;
                    if count == 0 {
                        return Err(HouseError::Io(std::io::ErrorKind::WriteZero));
                    }
                    *written += count;
                }
                output.sync_all()?;
            }
        }
        // Sync directory entries before reporting a durable installation.
        let mut parents: BTreeSet<PathBuf> = created
            .iter()
            .filter_map(|(path, _, _, _)| path.parent().map(Path::to_path_buf))
            .collect();
        parents.extend(
            dirs.iter()
                .filter_map(|dir: &CreatedDirectory| dir.path.parent().map(Path::to_path_buf)),
        );
        for dir in parents {
            File::open(dir)?.sync_all()?;
        }
        if root.is_dir() {
            File::open(root)?.sync_all()?;
        }
        Ok::<(), AdoptionError>(())
    })();
    if let Err(error) = result {
        let mut remaining = Vec::new();
        for (path, expected, handle, written) in created.iter().rev() {
            // The open handle proves identity even when a write was partial.
            let removable = check_path(path).is_ok()
                && same_file(path, handle)
                && read_bounded(path).is_ok_and(|bytes| bytes == expected[..*written]);
            if !removable || fs::remove_file(path).is_err() {
                remaining.push(path.clone());
            }
        }
        for dir in dirs.iter().rev() {
            if check_path(&dir.path).is_err()
                || !dir
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| same_metadata(&dir.path, metadata))
                || fs::remove_dir(&dir.path).is_err()
            {
                remaining.push(dir.path.clone());
            }
        }
        if !remaining.is_empty() {
            return Err(HouseError::PartialInstallation { remaining });
        }
        return Err(error);
    }
    Ok(report)
}

fn validate_files(files: &[NewFile<'_>]) -> Result<(), AdoptionError> {
    let total = files
        .iter()
        .try_fold(0usize, |sum, file| sum.checked_add(file.contents.len()));
    if files.len() > MAX_INSTALL_FILES || total.is_none_or(|total| total > MAX_INSTALL_BYTES) {
        return Err(HouseError::InvalidInput);
    }
    let mut paths = BTreeSet::new();
    for file in files {
        // Case-insensitive collision rejection keeps bundles portable.
        if !paths.insert(file.path.as_str().to_lowercase()) {
            return Err(HouseError::InvalidInput);
        }
    }
    for path in &paths {
        if paths
            .iter()
            .any(|other| other != path && other.starts_with(&format!("{path}/")))
        {
            return Err(HouseError::InvalidInput);
        }
    }
    Ok(())
}

pub(crate) fn check_path(path: &Path) -> Result<(), AdoptionError> {
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(HouseError::RedirectedPath);
    }
    let mut walked = PathBuf::new();
    for component in path.components() {
        walked.push(component);
        match fs::symlink_metadata(&walked) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(HouseError::RedirectedPath);
            }
            Ok(metadata) if !metadata.is_file() && !metadata.is_dir() => {
                return Err(HouseError::RedirectedPath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) struct CreatedDirectory {
    path: PathBuf,
    metadata: Option<fs::Metadata>,
}

pub(crate) fn create_dirs(
    path: &Path,
    created: &mut Vec<CreatedDirectory>,
) -> Result<(), AdoptionError> {
    check_path(path)?;
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        create_dirs(parent, created)?;
    }
    check_path(path)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    created.push(CreatedDirectory {
        path: path.to_path_buf(),
        metadata: None,
    });
    let metadata = fs::symlink_metadata(path)?;
    if let Some(dir) = created.last_mut() {
        dir.metadata = Some(metadata);
    }
    Ok(())
}

/// Read bounded regular-file content without following redirected paths.
pub fn read_bounded(path: &Path) -> Result<Vec<u8>, AdoptionError> {
    check_path(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(HouseError::RedirectedPath);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_INSTALL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INSTALL_BYTES {
        return Err(HouseError::InvalidInput);
    }
    Ok(bytes)
}

fn mode_matches(metadata: &fs::Metadata, mode: FileMode) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (metadata.permissions().mode() & 0o111 != 0) == (mode == FileMode::Executable)
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, mode);
        true
    }
}
pub(crate) fn same_file(path: &Path, handle: &File) -> bool {
    handle
        .metadata()
        .is_ok_and(|metadata| same_metadata(path, &metadata))
}
fn same_metadata(path: &Path, created: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path)
            .is_ok_and(|current| current.dev() == created.dev() && current.ino() == created.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, created);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_failure_rolls_back_only_this_calls_files() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        fs::write(root.join("local"), b"retain")?;
        let first = RelativePath::new("nested/first")?;
        let second = RelativePath::new("second")?;
        let files = [
            NewFile {
                path: &first,
                contents: b"created",
                mode: FileMode::Regular,
            },
            NewFile {
                path: &second,
                contents: b"later",
                mode: FileMode::Regular,
            },
        ];
        let result = install_with_checkpoint(&root, &files, |index| {
            if index == 1 {
                Err(HouseError::Io(std::io::ErrorKind::StorageFull))
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            result,
            Err(HouseError::Io(std::io::ErrorKind::StorageFull))
        ));
        assert!(!root.join("nested").exists());
        assert_eq!(fs::read(root.join("local"))?, b"retain");
        assert!(!install_new_files(&root, &files)?.has_conflicts());
        Ok(())
    }
    #[test]
    fn changed_file_is_retained_and_reported_during_rollback()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let first = RelativePath::new("first")?;
        let second = RelativePath::new("second")?;
        let files = [
            NewFile {
                path: &first,
                contents: b"created",
                mode: FileMode::Regular,
            },
            NewFile {
                path: &second,
                contents: b"later",
                mode: FileMode::Regular,
            },
        ];
        let result = install_with_checkpoint(&root, &files, |index| {
            if index == 1 {
                fs::write(root.join("first"), b"other owner")?;
                Err(HouseError::Conflict)
            } else {
                Ok(())
            }
        });
        match result {
            Err(HouseError::PartialInstallation { remaining }) => {
                assert_eq!(remaining, vec![root.join("first")])
            }
            other => return Err(format!("unexpected result: {other:?}").into()),
        }
        assert_eq!(fs::read(root.join("first"))?, b"other owner");
        Ok(())
    }
}
