//! File-level advisory locking for `.code-workspace` writes.
//!
//! Both the hook (inside Zed process) and MCP (separate processes) may write to the
//! same `.code-workspace` file concurrently. This module provides a file lock +
//! atomic write pattern to prevent corruption.

use fs2::FileExt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Get the lock file path for a given workspace file.
fn lock_path(workspace_file: &Path) -> PathBuf {
    let mut lock = workspace_file.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Execute a closure while holding an exclusive lock on the workspace file.
///
/// The lock is advisory (flock-based) and works across processes on the same machine.
/// The closure receives the workspace file path and should perform its read-modify-write
/// operations within it.
///
/// # Errors
/// Returns an error if the lock cannot be acquired or if the closure returns an error.
pub fn with_workspace_lock<T, E>(
    workspace_file: &Path,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, LockError<E>>
where
    E: std::fmt::Debug,
{
    let lock_file_path = lock_path(workspace_file);
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_file_path)
        .map_err(|e| LockError::Io(e, lock_file_path.clone()))?;

    lock_file
        .lock_exclusive()
        .map_err(|e| LockError::Io(e, lock_file_path.clone()))?;

    let result = f().map_err(LockError::Inner)?;

    // Unlock (also happens on drop, but explicit is clearer)
    lock_file.unlock().ok();
    // Clean up lock file (best-effort)
    fs::remove_file(&lock_file_path).ok();

    Ok(result)
}

/// Try to acquire the lock without blocking. Returns `None` if the lock is held by another.
pub fn try_workspace_lock<T, E>(
    workspace_file: &Path,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<Option<T>, LockError<E>>
where
    E: std::fmt::Debug,
{
    let lock_file_path = lock_path(workspace_file);
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_file_path)
        .map_err(|e| LockError::Io(e, lock_file_path.clone()))?;

    match lock_file.try_lock_exclusive() {
        Ok(()) => {
            let result = f().map_err(LockError::Inner)?;
            lock_file.unlock().ok();
            fs::remove_file(&lock_file_path).ok();
            Ok(Some(result))
        }
        Err(_) => Ok(None), // Lock held by another
    }
}

/// Write a file atomically: write to `.tmp`, then rename over the target.
///
/// This prevents partial writes from being visible to concurrent readers.
pub fn atomic_write(path: &Path, content: &str) -> io::Result<()> {
    let tmp = path.with_extension("code-workspace.tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Lock error type.
#[derive(Debug)]
pub enum LockError<E: std::fmt::Debug> {
    Io(io::Error, PathBuf),
    Inner(E),
}

impl<E: std::fmt::Debug + std::fmt::Display> std::fmt::Display for LockError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Io(e, path) => write!(f, "lock IO error on {}: {}", path.display(), e),
            LockError::Inner(e) => write!(f, "inner error: {}", e),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for LockError<E> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn lock_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let ws_file = dir.path().join("test.code-workspace");
        fs::write(&ws_file, "initial").unwrap();

        let result: Result<(), LockError<io::Error>> =
            with_workspace_lock(&ws_file, || {
                atomic_write(&ws_file, "updated")?;
                Ok(())
            });
        assert!(result.is_ok());
        assert_eq!(fs::read_to_string(&ws_file).unwrap(), "updated");

        // Lock file should be cleaned up
        assert!(!lock_path(&ws_file).exists());
    }

    #[test]
    fn try_lock_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let ws_file = dir.path().join("test.code-workspace");
        fs::write(&ws_file, "data").unwrap();

        let result: Result<Option<String>, LockError<io::Error>> =
            try_workspace_lock(&ws_file, || Ok("got lock".to_string()));
        assert_eq!(result.unwrap(), Some("got lock".to_string()));
    }

    #[test]
    fn concurrent_writes_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let ws_file = dir.path().join("test.code-workspace");
        fs::write(&ws_file, "0").unwrap();

        let ws_file = Arc::new(ws_file);
        let barrier = Arc::new(Barrier::new(10));

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let ws = ws_file.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    let result: Result<(), LockError<io::Error>> =
                        with_workspace_lock(&ws, || {
                            // Read-modify-write under lock
                            let current: i32 =
                                fs::read_to_string(ws.as_ref())?.trim().parse().unwrap_or(0);
                            atomic_write(&ws, &(current + 1).to_string())?;
                            Ok(())
                        });
                    assert!(result.is_ok(), "thread {} failed: {:?}", i, result.err());
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // All 10 increments should have been applied
        let final_val: i32 = fs::read_to_string(ws_file.as_ref())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(final_val, 10);
    }

    #[test]
    fn atomic_write_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.code-workspace");

        atomic_write(&path, "complete content").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "complete content");

        // Temp file should not exist after rename
        assert!(!path.with_extension("code-workspace.tmp").exists());
    }
}
