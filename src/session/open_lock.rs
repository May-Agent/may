//! Per-session open lock: one `may` process owns a session at a time.
//!
//! The lock is an exclusive non-blocking `flock` on
//! `<session dir>/session.lock` via `fd-lock`. The kernel releases it when
//! the holder's process dies, so no heartbeat or cleanup file is needed.
//! The holder writes its pid and start time into the file so a refused open
//! can say who has it.

use std::io::Write as _;
use std::path::Path;

/// The session lock file name inside a session directory.
pub const SESSION_LOCK_FILE: &str = "session.lock";

/// Error opening a session's lock.
#[derive(Debug, thiserror::Error)]
pub enum SessionOpenLockError {
    /// Another process holds the session's lock.
    #[error("session {session_id} is already open in another may process{suffix}")]
    Locked {
        /// Session that could not be opened.
        session_id: String,
        /// Holder info read back from the lock file, formatted for display.
        suffix: String,
    },
    /// Lock file IO failed.
    #[error("failed to lock the session: {0}")]
    Io(#[from] std::io::Error),
}

fn locked_error(session_id: &str, lock_path: &Path) -> SessionOpenLockError {
    let suffix = std::fs::read_to_string(lock_path)
        .ok()
        .map(|info| format!(" ({})", info.trim()))
        .filter(|info| info.len() > 2)
        .unwrap_or_default();
    SessionOpenLockError::Locked {
        session_id: session_id.to_string(),
        suffix,
    }
}

/// Exclusive open lock on a session directory. Dropping it closes the lock
/// file, which releases the `flock` held on the open file description.
#[derive(Debug)]
pub struct SessionOpenLock {
    session_id: String,
    // The write guard was consumed so the lock stays held; what matters is
    // the file staying open inside this `RwLock` until drop.
    _lock: fd_lock::RwLock<std::fs::File>,
}

impl SessionOpenLock {
    /// Session this lock covers.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Acquire the session's open lock at `session_root` (the session
    /// directory). Creates the directory when absent.
    ///
    /// # Errors
    /// Returns [`SessionOpenLockError::Locked`] when another process holds
    /// the lock, or [`SessionOpenLockError::Io`] on filesystem failure.
    pub fn acquire(session_id: &str, session_root: &Path) -> Result<Self, SessionOpenLockError> {
        std::fs::create_dir_all(session_root)?;
        let lock_path = session_root.join(SESSION_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        let mut lock = fd_lock::RwLock::new(file);
        let mut guard = match lock.try_write() {
            Ok(guard) => guard,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(locked_error(session_id, &lock_path));
            }
            Err(error) => return Err(error.into()),
        };
        let argv0 = std::env::args().next().unwrap_or_else(|| "may".to_string());
        let holder = format!(
            "pid {}, opened {}, {}",
            std::process::id(),
            jiff::Timestamp::now(),
            argv0
        );
        guard.set_len(0)?;
        guard.write_all(holder.as_bytes())?;
        // The flock lives on the open file description; forgetting the guard
        // keeps it held while `lock` (and its file) stays open.
        std::mem::forget(guard);
        Ok(Self {
            session_id: session_id.to_string(),
            _lock: lock,
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{SessionOpenLock, SessionOpenLockError};

    #[test]
    fn second_acquire_fails_while_first_guard_lives() {
        let dir = tempdir().expect("temp dir");
        let _first = SessionOpenLock::acquire("session-a", dir.path()).expect("first acquire");
        let second = SessionOpenLock::acquire("session-a", dir.path());
        assert!(
            matches!(second, Err(SessionOpenLockError::Locked { .. })),
            "second acquire must be refused: {second:?}"
        );
    }

    #[test]
    fn acquire_succeeds_after_first_guard_drops() {
        let dir = tempdir().expect("temp dir");
        let first = SessionOpenLock::acquire("session-a", dir.path()).expect("first acquire");
        drop(first);
        let second = SessionOpenLock::acquire("session-a", dir.path());
        assert!(second.is_ok(), "re-acquire must succeed: {second:?}");
    }

    #[test]
    fn lock_file_records_holder_info() {
        let dir = tempdir().expect("temp dir");
        let _lock = SessionOpenLock::acquire("session-a", dir.path()).expect("acquire");
        let info =
            std::fs::read_to_string(dir.path().join(super::SESSION_LOCK_FILE)).expect("lock file");
        assert!(info.contains("pid "), "holder info was written: {info}");
    }
}
