//! Regenerable build output inside worktrees.
//!
//! A directory counts as build output only when all of these hold: it is a
//! top-level directory of the checkout (not a symlink), it carries a valid
//! `CACHEDIR.TAG` (which Cargo writes into `target/`), Git ignores it, and it
//! contains no tracked file. Nothing outside the checkout is read or removed.

use std::{
    fs,
    io::{self, Read},
    path::{Component, Path},
};

use serde::Serialize;

use super::git::{GitLimits, GitReadError, ignored_untracked};

/// The fixed signature that starts a `CACHEDIR.TAG` file.
pub const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";
/// Top-level entries examined per worktree.
pub const MAX_TOP_LEVEL_ENTRIES: usize = 4096;
/// Entries counted per size measurement before it stops as incomplete.
pub const MAX_MEASURED_ENTRIES: usize = 1_000_000;

/// Disk space used by a directory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsage {
    /// Bytes allocated on disk (apparent size where allocation is unknown).
    pub bytes: u64,
    /// False when the walk stopped at [`MAX_MEASURED_ENTRIES`] or could not
    /// read part of the tree; `bytes` is then a lower bound.
    pub complete: bool,
}

/// One build output directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildDirectory {
    /// Its name at the top of the checkout, such as `target`.
    pub name: String,
    /// Its measured size.
    pub usage: DiskUsage,
}

/// The build output directories at the top of the checkout at `worktree`,
/// sorted by name.
///
/// # Errors
/// Returns a [`GitReadError`] when the checkout cannot be listed or Git
/// cannot confirm a candidate is ignored and untracked.
pub(super) fn find(worktree: &Path, limits: &GitLimits) -> Result<Vec<String>, GitReadError> {
    let entries = fs::read_dir(worktree).map_err(|_| GitReadError::InvalidPath)?;
    let mut names = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_TOP_LEVEL_ENTRIES {
            return Err(GitReadError::OutputTooLarge);
        }
        let entry = entry.map_err(|_| GitReadError::InvalidPath)?;
        // `DirEntry::file_type` does not follow symlinks.
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_dir || name == ".git" || !is_cache_dir(&entry.path()) {
            continue;
        }
        if ignored_untracked(worktree, &name, limits)? {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Whether `dir` holds a regular `CACHEDIR.TAG` starting with the signature.
fn is_cache_dir(dir: &Path) -> bool {
    let tag = dir.join("CACHEDIR.TAG");
    if !fs::symlink_metadata(&tag).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut start = Vec::with_capacity(CACHEDIR_SIGNATURE.len());
    fs::File::open(&tag)
        .and_then(|file| {
            file.take(u64::try_from(CACHEDIR_SIGNATURE.len()).unwrap_or(u64::MAX))
                .read_to_end(&mut start)
        })
        .is_ok()
        && start == CACHEDIR_SIGNATURE
}

/// Measure the tree at `path` without following symlinks.
#[must_use]
pub fn disk_usage(path: &Path) -> DiskUsage {
    let mut usage = DiskUsage {
        bytes: 0,
        complete: true,
    };
    let mut pending = vec![path.to_path_buf()];
    let mut seen: usize = 0;
    while let Some(current) = pending.pop() {
        seen = seen.saturating_add(1);
        if seen > MAX_MEASURED_ENTRIES {
            usage.complete = false;
            break;
        }
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            usage.complete = false;
            continue;
        };
        usage.bytes = usage.bytes.saturating_add(allocated(&metadata));
        if !metadata.is_dir() {
            continue;
        }
        match fs::read_dir(&current) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => pending.push(entry.path()),
                        Err(_) => usage.complete = false,
                    }
                }
            }
            Err(_) => usage.complete = false,
        }
    }
    usage
}

#[cfg(unix)]
fn allocated(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

/// Remove the build output directory `name` of the checkout at `worktree`
/// after re-checking that it is a real top-level cache directory.
///
/// # Errors
/// Returns [`io::ErrorKind::InvalidInput`] when the checks fail, and the
/// removal's error otherwise; a partial removal is finished by a later run.
pub(super) fn remove(worktree: &Path, name: &str) -> io::Result<()> {
    let mut components = Path::new(name).components();
    let single = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    let path = worktree.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    if !single || name == ".git" || !metadata.is_dir() || !is_cache_dir(&path) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // `remove_dir_all` does not follow symlinks inside the tree.
    fs::remove_dir_all(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_tag_needs_the_exact_signature() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        assert!(!is_cache_dir(dir.path()));
        fs::write(dir.path().join("CACHEDIR.TAG"), b"Signature: wrong")?;
        assert!(!is_cache_dir(dir.path()));
        fs::write(
            dir.path().join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\n# cargo"].concat(),
        )?;
        assert!(is_cache_dir(dir.path()));
        Ok(())
    }

    #[test]
    fn removal_refuses_paths_and_non_cache_directories() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join("plain"))?;
        for name in ["plain", "../escape", "a/b", ".git", "missing"] {
            assert!(remove(dir.path(), name).is_err(), "{name}");
        }
        assert!(dir.path().join("plain").is_dir());
        Ok(())
    }

    #[test]
    fn usage_counts_files_without_following_symlinks() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a"), vec![0_u8; 10_000])?;
        let empty = disk_usage(&dir.path().join("missing"));
        assert!(!empty.complete);
        let usage = disk_usage(dir.path());
        assert!(usage.complete && usage.bytes >= 10_000);
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir()?;
            fs::write(outside.path().join("big"), vec![0_u8; 100_000])?;
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link"))?;
            assert!(disk_usage(dir.path()).bytes < usage.bytes + 100_000);
        }
        Ok(())
    }
}
