//! Disk-exhaustion classification shared by the database, shells and daemon.

/// Why a home-environment write or container runtime call failed on storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskExhaustionCause {
    /// `ENOSPC` / `SQLITE_FULL` — no free blocks or inodes remain.
    NoSpaceLeft,
    /// `EDQUOT` — user, group, or project filesystem quota reached.
    QuotaExceeded,
    /// `EIO` — underlying storage or virtualized container disk I/O error.
    RuntimeIoError,
}

/// Classifies an [`std::io::Error`] caused by home-environment disk exhaustion.
#[doc(hidden)]
#[must_use]
pub fn classify_io_error(error: &std::io::Error) -> Option<DiskExhaustionCause> {
    if error.kind() == std::io::ErrorKind::StorageFull {
        return Some(DiskExhaustionCause::NoSpaceLeft);
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        if code == libc::ENOSPC {
            return Some(DiskExhaustionCause::NoSpaceLeft);
        }
        if code == libc::EDQUOT {
            return Some(DiskExhaustionCause::QuotaExceeded);
        }
        if code == libc::EIO {
            return Some(DiskExhaustionCause::RuntimeIoError);
        }
    }
    classify_error_text(&error.to_string())
}

/// Classifies a driver, SQLite, tool, or container runtime error string caused
/// by disk exhaustion or storage I/O failure.
#[must_use]
pub fn classify_error_text(text: &str) -> Option<DiskExhaustionCause> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("no space left on device")
        || lower.contains("database or disk is full")
        || lower.contains("sqlite_full")
        || lower.contains("os error 28")
        || lower.contains("enospc")
    {
        return Some(DiskExhaustionCause::NoSpaceLeft);
    }
    if lower.contains("disk quota exceeded")
        || lower.contains("filesystem quota exceeded")
        || lower.contains("edquot")
        || lower.contains("os error 69")
        || lower.contains("os error 122")
    {
        return Some(DiskExhaustionCause::QuotaExceeded);
    }
    if lower.contains("input/output error")
        || lower.contains("disk i/o error")
        || lower.contains("sqlite_ioerr")
        || lower.contains("os error 5")
    {
        return Some(DiskExhaustionCause::RuntimeIoError);
    }
    None
}

impl DiskExhaustionCause {
    /// The phrase alerts and notices use for this cause.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoSpaceLeft => "no space left on device",
            Self::QuotaExceeded => "disk quota exceeded",
            Self::RuntimeIoError => "storage input/output error",
        }
    }
}
