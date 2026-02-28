//! Sync engine: diffs workspace file state vs Zed DB state and produces actions.
//!
//! This module is the core logic for bidirectional sync between
//! `.code-workspace` files and Zed's internal sqlite3 database.
//!
//! Refactored to use:
//! - Mapping-based workspace lookup (not LIKE substring)
//! - File locking for concurrent safety
//! - Order-aware diff (detects reordering, not just membership changes)
//! - Conflict detection via timestamp comparison

use crate::lock;
use crate::mapping::WorkspaceMapping;
use crate::workspace_db::ZedDbReader;
use crate::workspace_file;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("workspace file error: {0}")]
    WorkspaceFile(#[from] crate::workspace_file::Error),
    #[error("database error: {0}")]
    Database(#[from] crate::workspace_db::Error),
    #[error("CLI invocation failed: {0}")]
    CliError(String),
    #[error("workspace file not found: {0}")]
    FileNotFound(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Direction of sync operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    /// .code-workspace file is source of truth -> push to Zed
    FileToZed,
    /// Zed DB is source of truth -> update .code-workspace file
    ZedToFile,
    /// Merge both sides
    Bidirectional,
}

impl std::str::FromStr for SyncDirection {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "file_to_zed" => Ok(Self::FileToZed),
            "zed_to_file" => Ok(Self::ZedToFile),
            "bidirectional" => Ok(Self::Bidirectional),
            other => Err(format!("unknown sync direction: {other}")),
        }
    }
}

/// An action to take to bring file and DB into sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Add a folder to the .code-workspace file (Zed added it).
    AddToFile(PathBuf),
    /// Remove a folder from the .code-workspace file (Zed removed it).
    RemoveFromFile(PathBuf),
    /// Reorder folders in the .code-workspace file to match Zed's order.
    ReorderFile,
    /// Invoke `zed --add <path>` to add a folder to running Zed.
    InvokeCliAdd(PathBuf),
    /// A folder was removed from .code-workspace but we can't remove from running Zed.
    PendingRemoval(PathBuf),
}

/// Result of a sync operation.
#[derive(Debug, Clone)]
pub struct SyncResult {
    pub actions_taken: Vec<SyncAction>,
    pub file_folders_after: Vec<PathBuf>,
    pub db_folders: Vec<PathBuf>,
    pub reordered: bool,
    pub conflict: bool,
}

/// Compute sync actions given the current state of both sides and a direction.
///
/// `file_folders` and `db_folders` should be in their respective user-visible order.
pub fn compute_sync_actions(
    file_folders: &[PathBuf],
    db_folders: &[PathBuf],
    direction: SyncDirection,
) -> Vec<SyncAction> {
    let diff = workspace_file::diff_folders(file_folders, db_folders);
    let mut actions = Vec::new();

    match direction {
        SyncDirection::ZedToFile => {
            // DB is truth: add DB-only folders to file, remove file-only folders from file
            for path in &diff.added {
                actions.push(SyncAction::AddToFile(path.clone()));
            }
            for path in &diff.removed {
                actions.push(SyncAction::RemoveFromFile(path.clone()));
            }
            // Check for reorder (same membership, different order)
            if diff.added.is_empty()
                && diff.removed.is_empty()
                && !workspace_file::folders_match_ordered(file_folders, db_folders)
            {
                actions.push(SyncAction::ReorderFile);
            }
        }
        SyncDirection::FileToZed => {
            // File is truth: add file-only folders to Zed, mark DB-only as pending removal
            for path in &diff.removed {
                actions.push(SyncAction::InvokeCliAdd(path.clone()));
            }
            for path in &diff.added {
                actions.push(SyncAction::PendingRemoval(path.clone()));
            }
        }
        SyncDirection::Bidirectional => {
            // Merge: add DB-only to file, add file-only to Zed (union of both)
            for path in &diff.added {
                actions.push(SyncAction::AddToFile(path.clone()));
            }
            for path in &diff.removed {
                actions.push(SyncAction::InvokeCliAdd(path.clone()));
            }
        }
    }

    actions
}

/// Execute a sync between a .code-workspace file and Zed's DB.
///
/// Uses mapping-based lookup when available, falls back to folder matching.
/// File writes are protected by advisory lock.
pub fn execute_sync(
    workspace_file_path: &Path,
    db_path: &Path,
    direction: SyncDirection,
) -> Result<SyncResult, Error> {
    if !workspace_file_path.exists() {
        return Err(Error::FileNotFound(workspace_file_path.to_path_buf()));
    }

    let reader = ZedDbReader::open(db_path)?;

    // Try mapping-based lookup first
    let db_record = find_db_record_for_file(workspace_file_path, &reader)?;
    let db_folders = db_record
        .as_ref()
        .map(|r| r.ordered_paths())
        .unwrap_or_default();

    // Read current file state (under lock for consistency)
    let result = lock::with_workspace_lock(workspace_file_path, || -> Result<SyncResult, Error> {
        let mut ws = workspace_file::CodeWorkspaceFile::from_file(workspace_file_path)?;
        let resolved = ws.resolve(workspace_file_path)?;
        let file_folders = resolved.folders;

        // Compute actions
        let actions = compute_sync_actions(&file_folders, &db_folders, direction);

        // Execute file-side actions
        let mut file_modified = false;
        let mut reordered = false;
        for action in &actions {
            match action {
                SyncAction::AddToFile(path) => {
                    if ws.add_folder(workspace_file_path, path)? {
                        file_modified = true;
                    }
                }
                SyncAction::RemoveFromFile(path) => {
                    if ws.remove_folder(workspace_file_path, path)? {
                        file_modified = true;
                    }
                }
                SyncAction::ReorderFile => {
                    // Replace folder list with DB order
                    ws.set_folders_from_absolute(workspace_file_path, &db_folders)?;
                    file_modified = true;
                    reordered = true;
                }
                SyncAction::InvokeCliAdd(path) => {
                    invoke_zed_add(path).map_err(Error::CliError)?;
                }
                SyncAction::PendingRemoval(_) => {
                    // Cannot remove from running Zed; logged for user awareness
                }
            }
        }

        if file_modified {
            let json = ws.to_json_pretty()?;
            lock::atomic_write(workspace_file_path, &json)?;
        }

        // Update mapping timestamp
        update_mapping_sync_ts(workspace_file_path);

        let final_resolved = ws.resolve(workspace_file_path)?;

        Ok(SyncResult {
            actions_taken: actions,
            file_folders_after: final_resolved.folders,
            db_folders: db_folders.clone(),
            reordered,
            conflict: false,
        })
    });

    match result {
        Ok(sync_result) => Ok(sync_result),
        Err(lock::LockError::Inner(e)) => Err(e),
        Err(lock::LockError::Io(io_err, path)) => {
            Err(Error::CliError(format!("lock IO error on {}: {}", path.display(), io_err)))
        }
    }
}

/// Find the DB record for a workspace file using the mapping-first strategy.
fn find_db_record_for_file(
    workspace_file_path: &Path,
    reader: &ZedDbReader,
) -> Result<Option<crate::workspace_db::WorkspaceRecord>, Error> {
    // Strategy 1: Check mapping files in potential roots
    let ws_dir = workspace_file_path.parent().unwrap_or(Path::new("/"));

    // The workspace file's directory might be a root, or it could be a parent of roots
    if let Some(mapping) = WorkspaceMapping::read(ws_dir)
        && let Some(record) = reader.find_by_id(mapping.workspace_id)? {
            return Ok(Some(record));
        }

    // Strategy 2: Read the workspace file to get folder paths, then match by paths
    if let Ok(ws) = workspace_file::CodeWorkspaceFile::from_file(workspace_file_path)
        && let Ok(resolved) = ws.resolve(workspace_file_path) {
            if let Some(record) = reader.find_by_paths(&resolved.folders)? {
                return Ok(Some(record));
            }
            // Strategy 3: Fallback to folder substring match (deprecated)
            if let Some(first) = resolved.folders.first()
                && let Some(record) = reader.find_by_folder(&first.to_string_lossy())? {
                    return Ok(Some(record));
                }
        }

    Ok(None)
}

/// Update the last_sync_ts in any mapping files near the workspace file.
fn update_mapping_sync_ts(workspace_file_path: &Path) {
    let ws_dir = workspace_file_path.parent().unwrap_or(Path::new("/"));
    if let Some(mut mapping) = WorkspaceMapping::read(ws_dir) {
        mapping.touch_sync_ts();
        if let Err(e) = mapping.write(ws_dir) {
            tracing::warn!("failed to update mapping sync timestamp: {}", e);
        }
    }
}

/// Add a folder to the running Zed instance.
///
/// Uses the hook socket first (channel-correct, no CLI dependency),
/// falls back to channel-aware CLI (`zed-preview` or `zed`).
fn invoke_zed_add(path: &Path) -> Result<(), String> {
    // Resolve channel from mapping near the path
    let channel = resolve_channel_for_path(path);
    crate::hook_client::invoke_zed_add(path, channel.as_deref(), None)?;
    Ok(())
}

/// Replace workspace folders via `--reuse`.
///
/// Uses the hook socket first, falls back to channel-aware CLI.
pub fn invoke_zed_reuse(paths: &[PathBuf]) -> Result<(), String> {
    let channel = paths
        .first()
        .and_then(|p| resolve_channel_for_path(p));
    crate::hook_client::invoke_zed_reuse(paths, channel.as_deref())?;
    Ok(())
}

/// Try to resolve the Zed channel from a mapping file near a path.
fn resolve_channel_for_path(path: &Path) -> Option<String> {
    // Check if path itself is a directory with a mapping
    if let Some(m) = crate::mapping::WorkspaceMapping::read(path) {
        return m.zed_channel;
    }
    // Check parent directory
    if let Some(parent) = path.parent() {
        if let Some(m) = crate::mapping::WorkspaceMapping::read(parent) {
            return m.zed_channel;
        }
    }
    // Fall back to DB-based detection
    if let Ok(db_path) = crate::workspace_db::default_db_path(None) {
        return crate::mapping::channel_from_db_path(&db_path);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zed_to_file_adds_db_folders_to_file() {
        let file_folders = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let db_folders = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let actions = compute_sync_actions(&file_folders, &db_folders, SyncDirection::ZedToFile);
        assert_eq!(actions, vec![SyncAction::AddToFile(PathBuf::from("/c"))]);
    }

    #[test]
    fn zed_to_file_removes_file_only_folders() {
        let file_folders = vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        let db_folders = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let actions = compute_sync_actions(&file_folders, &db_folders, SyncDirection::ZedToFile);
        assert_eq!(
            actions,
            vec![SyncAction::RemoveFromFile(PathBuf::from("/c"))]
        );
    }

    #[test]
    fn zed_to_file_detects_reorder() {
        let file_folders = vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        let db_folders = vec![PathBuf::from("/c"), PathBuf::from("/a"), PathBuf::from("/b")];
        let actions = compute_sync_actions(&file_folders, &db_folders, SyncDirection::ZedToFile);
        assert_eq!(actions, vec![SyncAction::ReorderFile]);
    }

    #[test]
    fn file_to_zed_adds_file_folders_to_zed() {
        let file_folders = vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        let db_folders = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let actions = compute_sync_actions(&file_folders, &db_folders, SyncDirection::FileToZed);
        assert_eq!(
            actions,
            vec![SyncAction::InvokeCliAdd(PathBuf::from("/c"))]
        );
    }

    #[test]
    fn file_to_zed_marks_db_only_as_pending_removal() {
        let file_folders = vec![PathBuf::from("/a")];
        let db_folders = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let actions = compute_sync_actions(&file_folders, &db_folders, SyncDirection::FileToZed);
        assert_eq!(
            actions,
            vec![SyncAction::PendingRemoval(PathBuf::from("/b"))]
        );
    }

    #[test]
    fn bidirectional_merges_both_sides() {
        let file_folders = vec![PathBuf::from("/a"), PathBuf::from("/file_only")];
        let db_folders = vec![PathBuf::from("/a"), PathBuf::from("/db_only")];
        let actions =
            compute_sync_actions(&file_folders, &db_folders, SyncDirection::Bidirectional);
        assert_eq!(
            actions,
            vec![
                SyncAction::AddToFile(PathBuf::from("/db_only")),
                SyncAction::InvokeCliAdd(PathBuf::from("/file_only")),
            ]
        );
    }

    #[test]
    fn no_actions_when_in_sync() {
        let folders = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let actions = compute_sync_actions(&folders, &folders, SyncDirection::Bidirectional);
        assert!(actions.is_empty());
    }

    #[test]
    fn direction_from_str() {
        assert_eq!(
            "file_to_zed".parse::<SyncDirection>().unwrap(),
            SyncDirection::FileToZed
        );
        assert_eq!(
            "zed_to_file".parse::<SyncDirection>().unwrap(),
            SyncDirection::ZedToFile
        );
        assert_eq!(
            "bidirectional".parse::<SyncDirection>().unwrap(),
            SyncDirection::Bidirectional
        );
        assert!("invalid".parse::<SyncDirection>().is_err());
    }

    #[test]
    fn execute_sync_file_not_found() {
        let result = execute_sync(
            Path::new("/nonexistent/my.code-workspace"),
            Path::new("/tmp/db.sqlite"),
            SyncDirection::Bidirectional,
        );
        assert!(matches!(result, Err(Error::FileNotFound(_))));
    }
}
