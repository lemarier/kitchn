//! The launch base recorded before a worker starts, outside its checkout.
//! A worker can edit its Git config and refs, so push reads this record from
//! the house runtime directory instead of trusting a worktree-supplied base.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    HouseId,
    contracts::{BranchName, CommitId, ExternalRef, IdempotencyKey},
};

const MAX_RECORD_BYTES: u64 = 2048;

/// Immutable launch base for one effect key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriterBase {
    /// House whose worker owns the worktree.
    pub house: HouseId,
    /// Orca's exact worktree handle.
    pub worktree: ExternalRef,
    /// Branch at the instant the worker was launched.
    pub branch: BranchName,
    /// Commit checked out before the worker received the task.
    pub base: CommitId,
    /// Whether this launch created and owns the worktree.
    pub created: bool,
}

/// A missing, incomplete, or different record never licenses a push.
#[derive(Debug, thiserror::Error)]
pub enum WriterBaseError {
    /// This launch predates writer-base recording.
    #[error("writer base record was not found")]
    NotFound,
    /// The runtime directory was invalid or unavailable.
    #[error("writer base runtime storage is unavailable")]
    Unavailable,
    /// A record for this launch key already names different facts.
    #[error("writer base record conflicts with the launch")]
    Conflict,
    /// The record is malformed or over the byte bound.
    #[error("writer base record is malformed")]
    Malformed,
}

fn path(root: &Path, key: &IdempotencyKey) -> Result<std::path::PathBuf, WriterBaseError> {
    if !root.is_absolute() {
        return Err(WriterBaseError::Unavailable);
    }
    let digest = Sha256::digest(key.as_str().as_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(root.join("writer-bases").join(format!("{name}.json")))
}

/// Read one complete record, refusing links and oversized files.
pub fn read_writer_base(root: &Path, key: &IdempotencyKey) -> Result<WriterBase, WriterBaseError> {
    let path = path(root, key)?;
    let meta = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WriterBaseError::NotFound
        } else {
            WriterBaseError::Unavailable
        }
    })?;
    if !meta.file_type().is_file() || meta.len() > MAX_RECORD_BYTES {
        return Err(WriterBaseError::Malformed);
    }
    let mut bytes = Vec::new();
    File::open(&path)
        .map_err(|_| WriterBaseError::Unavailable)?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| WriterBaseError::Unavailable)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(WriterBaseError::Malformed);
    }
    serde_json::from_slice(&bytes).map_err(|_| WriterBaseError::Malformed)
}

/// Create the base before starting a worker, or confirm the identical record
/// after an interrupted launch. A failed staged write leaves no base record.
pub fn record_writer_base(
    root: &Path,
    key: &IdempotencyKey,
    base: &WriterBase,
) -> Result<(), WriterBaseError> {
    let path = path(root, key)?;
    let dir = path.parent().ok_or(WriterBaseError::Unavailable)?;
    fs::create_dir_all(dir).map_err(|_| WriterBaseError::Unavailable)?;
    if fs::symlink_metadata(dir)
        .map_err(|_| WriterBaseError::Unavailable)?
        .file_type()
        .is_symlink()
    {
        return Err(WriterBaseError::Unavailable);
    }
    let bytes = serde_json::to_vec(base).map_err(|_| WriterBaseError::Malformed)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(WriterBaseError::Malformed);
    }
    let mut staged =
        tempfile::NamedTempFile::new_in(dir).map_err(|_| WriterBaseError::Unavailable)?;
    staged
        .write_all(&bytes)
        .map_err(|_| WriterBaseError::Unavailable)?;
    staged
        .as_file()
        .sync_all()
        .map_err(|_| WriterBaseError::Unavailable)?;
    match staged.persist_noclobber(&path) {
        Ok(_) => {
            File::open(dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| WriterBaseError::Unavailable)?;
            Ok(())
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_writer_base(root, key)? == *base {
                Ok(())
            } else {
                Err(WriterBaseError::Conflict)
            }
        }
        Err(_) => Err(WriterBaseError::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_base_is_immutable_and_read_back() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let key = IdempotencyKey::from_ref(ExternalRef::new("launch-1")?);
        let base = WriterBase {
            house: HouseId::new("acme")?,
            worktree: ExternalRef::new("wt-1")?,
            branch: BranchName::new("acme/issue-1")?,
            base: CommitId::new("1111111111111111111111111111111111111111")?,
            created: true,
        };
        record_writer_base(root.path(), &key, &base)?;
        record_writer_base(root.path(), &key, &base)?;
        assert_eq!(read_writer_base(root.path(), &key)?, base);
        let changed = WriterBase {
            base: CommitId::new("2222222222222222222222222222222222222222")?,
            ..base
        };
        assert!(matches!(
            record_writer_base(root.path(), &key, &changed),
            Err(WriterBaseError::Conflict)
        ));
        assert_ne!(read_writer_base(root.path(), &key)?, changed);
        Ok(())
    }
}
