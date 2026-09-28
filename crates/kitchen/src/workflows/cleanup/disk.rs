//! Free-space measurement for disk-pressure inspections.
//!
//! Low free space only starts a preview-only inspection: it never makes a
//! resource eligible, approves a step, or deletes anything.

use std::{
    io,
    num::NonZeroU64,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Free space on the filesystem holding a path, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeSpace {
    /// Bytes an unprivileged process may still write.
    pub available: u64,
    /// The filesystem's size.
    pub total: u64,
}

/// Why free space could not be measured. Nothing is inferred from a failed
/// measurement: it is neither pressure nor its absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// This platform has no free-space probe.
    #[error("free space cannot be measured on this platform")]
    Unsupported,
    /// The measurement did not finish before its deadline.
    #[error("free-space measurement timed out")]
    Timeout,
    /// An earlier measurement that timed out has not finished yet, so no
    /// second one was started.
    #[error("an earlier free-space measurement is still running")]
    Pending,
    /// The filesystem reported sizes that do not fit in bytes.
    #[error("the filesystem reported an unrepresentable size")]
    Overflow,
    /// The measurement failed.
    #[error("free-space measurement failed: {0}")]
    Io(io::ErrorKind),
}

/// Measures free space. Implementations must be bounded in time.
pub trait FreeSpaceProbe {
    /// Free space on the filesystem holding `path`.
    ///
    /// # Errors
    /// A [`ProbeError`] when it cannot be measured.
    fn free_space(&self, path: &Path) -> Result<FreeSpace, ProbeError>;
}

/// The host's free space through `statvfs`, on a helper thread with a
/// deadline. A call that misses the deadline, such as on a hung network
/// mount, returns [`ProbeError::Timeout`]; its thread is left to finish on
/// its own and holds nothing but the path. Until it does, this probe and its
/// clones start no other measurement and return [`ProbeError::Pending`], so
/// at most one thread is ever blocked.
#[derive(Debug, Clone)]
pub struct StatvfsProbe {
    timeout: Duration,
    running: Arc<AtomicBool>,
    measure: fn(&Path) -> Result<FreeSpace, ProbeError>,
}

impl StatvfsProbe {
    /// A probe that waits at most `timeout` for each measurement.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            running: Arc::new(AtomicBool::new(false)),
            measure,
        }
    }
}

impl FreeSpaceProbe for StatvfsProbe {
    fn free_space(&self, path: &Path) -> Result<FreeSpace, ProbeError> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(ProbeError::Pending);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let path = path.to_path_buf();
        let running = Arc::clone(&self.running);
        let measure = self.measure;
        let spawned = thread::Builder::new()
            .name("kitchen-free-space".to_owned())
            .spawn(move || {
                let result = measure(&path);
                running.store(false, Ordering::Release);
                // The receiver is gone only after a timeout; nothing to report.
                let _ = sender.send(result);
            });
        if let Err(error) = spawned {
            self.running.store(false, Ordering::Release);
            return Err(ProbeError::Io(error.kind()));
        }
        receiver
            .recv_timeout(self.timeout)
            .map_err(|_| ProbeError::Timeout)?
    }
}

#[cfg(unix)]
fn measure(path: &Path) -> Result<FreeSpace, ProbeError> {
    let stats =
        rustix::fs::statvfs(path).map_err(|error| ProbeError::Io(io::Error::from(error).kind()))?;
    let bytes = |blocks: u64| {
        blocks
            .checked_mul(stats.f_frsize)
            .ok_or(ProbeError::Overflow)
    };
    Ok(FreeSpace {
        available: bytes(stats.f_bavail)?,
        total: bytes(stats.f_blocks)?,
    })
}

#[cfg(not(unix))]
fn measure(_: &Path) -> Result<FreeSpace, ProbeError> {
    Err(ProbeError::Unsupported)
}

/// House policy for disk pressure: below this much free space, the
/// dishwasher starts a preview-only inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiskPressurePolicy {
    /// Free bytes below which the filesystem is under pressure.
    pub min_free_bytes: NonZeroU64,
}

impl DiskPressurePolicy {
    /// Whether `free` is under this policy's threshold.
    #[must_use]
    pub const fn under_pressure(&self, free: FreeSpace) -> bool {
        free.available < self.min_free_bytes.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_probe_measures_a_real_directory() -> Result<(), ProbeError> {
        let probe = StatvfsProbe::new(Duration::from_secs(10));
        let free = probe.free_space(&std::env::temp_dir())?;
        assert!(free.total > 0);
        assert!(free.available <= free.total);
        Ok(())
    }

    #[test]
    fn the_host_probe_reports_a_missing_path() {
        let probe = StatvfsProbe::new(Duration::from_secs(10));
        let missing = std::env::temp_dir().join("kitchen-no-such-directory-for-statvfs");
        assert_eq!(
            probe.free_space(&missing),
            Err(ProbeError::Io(io::ErrorKind::NotFound))
        );
    }

    fn slow(_: &Path) -> Result<FreeSpace, ProbeError> {
        thread::sleep(Duration::from_millis(300));
        Ok(FreeSpace {
            available: 1,
            total: 2,
        })
    }

    #[test]
    fn a_hung_measurement_times_out_and_blocks_no_second_thread() {
        let probe = StatvfsProbe {
            timeout: Duration::from_millis(20),
            running: Arc::new(AtomicBool::new(false)),
            measure: slow,
        };
        let path = std::env::temp_dir();
        assert_eq!(probe.free_space(&path), Err(ProbeError::Timeout));
        // A clone shares the running measurement.
        assert_eq!(probe.clone().free_space(&path), Err(ProbeError::Pending));
        // Once the first one finishes, measuring works again.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while probe.running.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let patient = StatvfsProbe {
            timeout: Duration::from_secs(10),
            ..probe
        };
        assert_eq!(
            patient.free_space(&path),
            Ok(FreeSpace {
                available: 1,
                total: 2,
            })
        );
    }

    #[test]
    fn pressure_is_strictly_below_the_threshold() {
        let policy = DiskPressurePolicy {
            min_free_bytes: NonZeroU64::MIN.saturating_add(99),
        };
        let free = |available| FreeSpace {
            available,
            total: 1_000,
        };
        assert!(policy.under_pressure(free(0)));
        assert!(policy.under_pressure(free(99)));
        assert!(!policy.under_pressure(free(100)));
        assert!(!policy.under_pressure(free(1_000)));
    }

    #[test]
    fn a_policy_needs_a_positive_threshold() {
        assert!(serde_json::from_str::<DiskPressurePolicy>(r#"{"minFreeBytes":0}"#).is_err());
        assert!(
            serde_json::from_str::<DiskPressurePolicy>(r#"{"minFreeBytes":1,"extra":1}"#).is_err()
        );
        assert_eq!(
            serde_json::from_str::<DiskPressurePolicy>(r#"{"minFreeBytes":1024}"#).ok(),
            Some(DiskPressurePolicy {
                min_free_bytes: NonZeroU64::MIN.saturating_add(1023),
            })
        );
    }
}
