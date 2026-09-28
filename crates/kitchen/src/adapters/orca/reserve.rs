//! Per-key reservations that serialize an effect's create window.
//!
//! Orca gives Kitchen no atomic "create unless it exists": a launch key is
//! only a Task title, and an automation only a name. Two callers that both
//! list before either creates would both create. A reservation is an
//! exclusive advisory lock on a file named after the key, held across the
//! list, the create, and the start (or read-back), so the second caller waits
//! and then finds what the first one made.
//!
//! [`crate::state::run_effect`] already serializes the effects of one task:
//! it persists intent first, and while an effect is unresolved the store
//! hands out no second submission until a lookup has run. That is not enough
//! here. A lookup that finds no Task cannot tell "never submitted" from "a
//! first submission still between its list and its create", and it is
//! followed by a resubmission that races that first one. The adapter is also
//! public and callable without the store, several Kitchen processes can
//! drive one Orca, and the store never holds its lock across a backend call.
//! So the adapter guards its own create window.
//!
//! The lock is released by the operating system when its holder exits, so a
//! crashed caller never wedges a key, and the wait is bounded. A reservation
//! that times out sent nothing to Orca; whether the effect is nevertheless in
//! flight under the holder is for the caller to reconcile.
//!
//! Lock files are empty and named by a digest of the key. A caller whose work
//! reached a state no later submission can repeat calls [`Reservation::settle`],
//! which removes the file, so files do not accumulate; a waiter that opened
//! the removed file detects it and reserves the new one.
//!
//! Like the state store, this trusts a directory private to the Kitchen user
//! and refuses a redirected directory or lock file, but cannot prevent a
//! process running as the same user from racing those checks.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use crate::adapters::orca::OrcaError;

/// Directory, inside the runtime directory, that holds the lock files.
const DIRECTORY: &str = "reservations";

/// Longest sleep between attempts to take a held lock.
const MAX_BACKOFF: Duration = Duration::from_millis(50);

/// An exclusive reservation of one key, released on drop.
#[derive(Debug)]
pub(crate) struct Reservation {
    /// Holding the open file holds the lock.
    _file: File,
    path: PathBuf,
    settled: bool,
}

impl Reservation {
    /// Reserve `stem` under `runtime_dir`, waiting at most `wait`.
    ///
    /// # Errors
    /// [`OrcaError::ReservationBusy`] when another holder keeps it for the
    /// whole wait, [`OrcaError::ReservationInsideRepository`] for a directory
    /// inside a Git checkout, [`OrcaError::ReservationRedirected`] for a
    /// symlinked directory or lock file, and [`OrcaError::ReservationUnavailable`] for
    /// other I/O failures. Nothing reached Orca in any of these cases.
    pub(crate) fn acquire(
        runtime_dir: &Path,
        stem: &str,
        wait: Duration,
    ) -> Result<Self, OrcaError> {
        let directory = runtime_dir.join(DIRECTORY);
        if crate::state::snapshot::inside_repository(&directory).map_err(unavailable)? {
            return Err(OrcaError::ReservationInsideRepository);
        }
        prepare_directory(&directory)?;
        let path = directory.join(format!("{stem}.lock"));
        let started = Instant::now();
        let mut backoff = Duration::from_millis(2);
        loop {
            refuse_redirected(&path)?;
            let file = open(&path).map_err(unavailable)?;
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) => {}
                    Err(TryLockError::Error(error)) => return Err(unavailable(error)),
                }
                let Some(remaining) = wait
                    .checked_sub(started.elapsed())
                    .filter(|left| !left.is_zero())
                else {
                    return Err(OrcaError::ReservationBusy);
                };
                thread::sleep(backoff.min(remaining));
                backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
            }
            // A holder that settled removed the file before releasing it, and
            // a lock on a removed file excludes nobody: reserve the new one.
            if is_current(&file, &path).map_err(unavailable)? {
                return Ok(Self {
                    _file: file,
                    path,
                    settled: false,
                });
            }
            if started.elapsed() >= wait {
                return Err(OrcaError::ReservationBusy);
            }
        }
    }

    /// Mark the reserved work as done in a way no later submission can repeat,
    /// such as a dispatched launch. The lock file is removed on release.
    pub(crate) const fn settle(&mut self) {
        self.settled = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Removal only keeps the directory small: the lock is released when
        // `_file` drops right after, and a file that stays is harmless.
        if self.settled && cfg!(unix) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn unavailable(error: io::Error) -> OrcaError {
    OrcaError::ReservationUnavailable(error.kind())
}

/// Create the directory (and missing parents) readable only by the owner,
/// refusing a symlink in its place.
fn prepare_directory(directory: &Path) -> Result<(), OrcaError> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(OrcaError::ReservationRedirected);
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(unavailable(error)),
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory).map_err(unavailable)
}

/// Refuse a lock file that exists but is not a regular file.
fn refuse_redirected(path: &Path) -> Result<(), OrcaError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(OrcaError::ReservationRedirected),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(unavailable(error)),
    }
}

/// Open the lock file, creating it readable only by the owner.
fn open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Whether `path` still names the file that `file` has open.
#[cfg(unix)]
fn is_current(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata()?;
    match fs::symlink_metadata(path) {
        Ok(named) => Ok(named.dev() == open.dev() && named.ino() == open.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Files are never removed on this platform, so an open file is current.
#[cfg(not(unix))]
fn is_current(_file: &File, _path: &Path) -> io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Result<tempfile::TempDir, io::Error> {
        tempfile::tempdir()
    }

    #[test]
    fn a_held_key_blocks_until_released() -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let first = Reservation::acquire(root.path(), "k", Duration::from_secs(1))?;
        assert!(matches!(
            Reservation::acquire(root.path(), "k", Duration::from_millis(50)),
            Err(OrcaError::ReservationBusy)
        ));
        // Other keys are independent.
        Reservation::acquire(root.path(), "other", Duration::from_millis(50))?;
        drop(first);
        Reservation::acquire(root.path(), "k", Duration::from_millis(50))?;
        Ok(())
    }

    #[test]
    fn a_runtime_directory_inside_a_checkout_is_refused_before_creation()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let checkout = root.path().join("checkout");
        fs::create_dir_all(checkout.join(".git"))?;
        assert!(matches!(
            Reservation::acquire(&checkout.join("runtime"), "k", Duration::ZERO),
            Err(OrcaError::ReservationInsideRepository)
        ));
        assert!(!checkout.join("runtime").exists());
        // A worktree marks its checkout with a `.git` file.
        let worktree = root.path().join("worktree");
        fs::create_dir(&worktree)?;
        fs::write(worktree.join(".git"), "gitdir: elsewhere")?;
        assert!(matches!(
            Reservation::acquire(&worktree, "k", Duration::ZERO),
            Err(OrcaError::ReservationInsideRepository)
        ));
        #[cfg(unix)]
        {
            let link = root.path().join("link");
            std::os::unix::fs::symlink(&checkout, &link)?;
            assert!(matches!(
                Reservation::acquire(&link.join("runtime"), "k", Duration::ZERO),
                Err(OrcaError::ReservationInsideRepository)
            ));
        }
        // A sibling outside the checkout is accepted.
        Reservation::acquire(&root.path().join("runtime"), "k", Duration::ZERO)?;
        Ok(())
    }

    #[test]
    fn a_zero_wait_still_takes_a_free_key() -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let held = Reservation::acquire(root.path(), "k", Duration::ZERO)?;
        assert!(matches!(
            Reservation::acquire(root.path(), "k", Duration::ZERO),
            Err(OrcaError::ReservationBusy)
        ));
        drop(held);
        Ok(())
    }

    #[test]
    fn settling_removes_the_file_and_an_unsettled_release_keeps_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let path = root.path().join(DIRECTORY).join("k.lock");
        drop(Reservation::acquire(
            root.path(),
            "k",
            Duration::from_secs(1),
        )?);
        assert!(
            path.exists(),
            "a failed submission leaves the key reserved-able"
        );
        let mut settled = Reservation::acquire(root.path(), "k", Duration::from_secs(1))?;
        settled.settle();
        drop(settled);
        assert!(!path.exists(), "a settled key leaves nothing behind");
        Ok(())
    }

    #[test]
    fn a_waiter_on_a_removed_file_reserves_the_new_one() -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let mut first = Reservation::acquire(root.path(), "k", Duration::from_secs(1))?;
        thread::scope(|scope| -> Result<(), Box<dyn std::error::Error>> {
            let waiter = scope.spawn(|| {
                Reservation::acquire(root.path(), "k", Duration::from_secs(5)).map(|held| {
                    // Reserved the file that exists now, not the removed one.
                    held.path.exists()
                })
            });
            thread::sleep(Duration::from_millis(100));
            first.settle();
            drop(first);
            assert_eq!(waiter.join().map_err(|_| "waiter panicked")?, Ok(true));
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn redirected_paths_are_refused() -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let elsewhere = dir()?;
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join(DIRECTORY))?;
        assert!(matches!(
            Reservation::acquire(root.path(), "k", Duration::from_millis(50)),
            Err(OrcaError::ReservationRedirected)
        ));

        let root = dir()?;
        let directory = root.path().join(DIRECTORY);
        fs::create_dir(&directory)?;
        std::os::unix::fs::symlink(elsewhere.path().join("target"), directory.join("k.lock"))?;
        assert!(matches!(
            Reservation::acquire(root.path(), "k", Duration::from_millis(50)),
            Err(OrcaError::ReservationRedirected)
        ));
        assert!(
            !elsewhere.path().join("target").exists(),
            "nothing was created through it"
        );
        Ok(())
    }

    #[test]
    fn an_unusable_directory_is_unavailable_not_busy() -> Result<(), Box<dyn std::error::Error>> {
        let root = dir()?;
        let file = root.path().join("occupied");
        fs::write(&file, b"")?;
        // A regular file where the runtime directory should be.
        assert!(matches!(
            Reservation::acquire(&file, "k", Duration::from_millis(50)),
            Err(OrcaError::ReservationUnavailable(_))
        ));
        Ok(())
    }
}
