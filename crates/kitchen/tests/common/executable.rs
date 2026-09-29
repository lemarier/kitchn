//! Writes a fake executable that a test then runs.
//!
//! Writing the file in this process and executing it can fail with `ETXTBSY`
//! on Linux: another test thread forks a child while the write descriptor is
//! open, and the child keeps its copy until it reaches `exec`. Renaming the
//! written file does not help, because the inode still has that writer. The
//! final inode is instead created by a short-lived `cp` child that has exited
//! before this returns, so no descriptor open for writing on it ever exists in
//! this process to be inherited.

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

/// Writes `contents` to `path` with mode `0o700`, ready to execute at once.
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let mut staging_name = std::ffi::OsString::from(".");
    staging_name.push(name);
    staging_name.push(".staging");
    let staging = path.with_file_name(staging_name);
    fs::write(&staging, contents)?;
    let copied = Command::new("cp")
        .arg(&staging)
        .arg(path)
        .stdin(Stdio::null())
        .status();
    fs::remove_file(&staging)?;
    if !copied?.success() {
        return Err(io::Error::other("cp could not create the executable"));
    }
    // A chmod opens no descriptor, so it cannot add a writer.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}
