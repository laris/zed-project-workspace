//! Workspace mapping file: `.zed/zed-project-workspace.json`
//!
//! Machine-readable mapping between Zed's workspace_id and the `.code-workspace` file.
//! Replaces the old `project_name` hack. Uses relative paths for portability.
//!
//! The mapping file is stored at `{worktree_root}/.zed/zed-project-workspace.json`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The mapping file name within `.zed/` directory.
pub const MAPPING_FILENAME: &str = "zed-project-workspace.json";

/// Workspace mapping: ties a Zed workspace_id to a `.code-workspace` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMapping {
    /// Zed's internal workspace_id (machine-local, auto-increment integer).
    pub workspace_id: i64,

    /// Relative path from the worktree root to the `.code-workspace` file.
    /// Example: `"my-project.code-workspace"` or `"../my-project.code-workspace"`.
    pub workspace_file: String,

    /// Which Zed channel this workspace_id belongs to (`"preview"` or `"stable"`).
    /// Prevents cross-channel confusion when both are installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zed_channel: Option<String>,

    /// Last successful sync timestamp (ISO 8601).
    /// Used for conflict detection: compare against DB timestamp and file mtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_ts: Option<String>,
}

impl WorkspaceMapping {
    /// Read the mapping file from a worktree root's `.zed/` directory.
    ///
    /// Returns `None` if the file doesn't exist or can't be parsed.
    pub fn read(worktree_root: &Path) -> Option<Self> {
        let path = Self::file_path(worktree_root);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Write the mapping file to a worktree root's `.zed/` directory.
    ///
    /// Creates the `.zed/` directory if it doesn't exist.
    pub fn write(&self, worktree_root: &Path) -> std::io::Result<()> {
        let dir = worktree_root.join(".zed");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(MAPPING_FILENAME);
        let json = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Get the full path to the mapping file for a given worktree root.
    pub fn file_path(worktree_root: &Path) -> PathBuf {
        worktree_root.join(".zed").join(MAPPING_FILENAME)
    }

    /// Resolve the `.code-workspace` file path from the relative `workspace_file` field.
    ///
    /// The relative path is resolved against the worktree root (NOT the `.zed/` directory).
    pub fn resolve_workspace_file(&self, worktree_root: &Path) -> PathBuf {
        let target = PathBuf::from(&self.workspace_file);
        if target.is_absolute() {
            target
        } else {
            worktree_root.join(target)
        }
    }

    /// Check if the mapping is valid: workspace_file resolves to an existing file.
    pub fn is_valid(&self, worktree_root: &Path) -> bool {
        self.resolve_workspace_file(worktree_root).exists()
    }

    /// Update the last_sync_ts to the current time.
    pub fn touch_sync_ts(&mut self) {
        self.last_sync_ts = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Create a new mapping for a workspace file in the same directory as the worktree root.
    pub fn new(workspace_id: i64, workspace_file: &str, zed_channel: Option<&str>) -> Self {
        Self {
            workspace_id,
            workspace_file: workspace_file.to_string(),
            zed_channel: zed_channel.map(|s| s.to_string()),
            last_sync_ts: None,
        }
    }

    /// Write this mapping to ALL worktree roots (for multi-root workspaces).
    ///
    /// Each root gets its own copy with the `workspace_file` path adjusted
    /// relative to that root.
    pub fn write_to_roots(
        &self,
        roots: &[PathBuf],
        workspace_file_abs: &Path,
    ) -> std::io::Result<()> {
        for root in roots {
            let rel = crate::paths::relative_path(root, workspace_file_abs);
            let mapping = WorkspaceMapping {
                workspace_id: self.workspace_id,
                workspace_file: rel.to_string_lossy().to_string(),
                zed_channel: self.zed_channel.clone(),
                last_sync_ts: self.last_sync_ts.clone(),
            };
            mapping.write(root)?;
        }
        Ok(())
    }
}

/// Remove the legacy mapping file (`.zed/zed-project-workspace.json`) if it exists.
///
/// Called during migration to project_name-based identity.
/// Best-effort: logs a warning on failure, does not return an error.
pub fn cleanup_legacy_mapping(root: &Path) {
    let path = WorkspaceMapping::file_path(root);
    if path.exists() {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!("Removed legacy mapping file: {}", path.display()),
            Err(e) => tracing::warn!("Failed to remove legacy mapping file {}: {}", path.display(), e),
        }
    }
}

/// Detect the Zed channel from the running executable path.
///
/// Returns `"preview"` for Zed Preview, `"stable"` for Zed Stable, `None` if unknown.
pub fn detect_zed_channel() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe_str = exe.to_string_lossy();
    if exe_str.contains("Zed Preview") || exe_str.contains("zed-preview") {
        Some("preview".to_string())
    } else if exe_str.contains("Zed.app") || exe_str.contains("zed-stable") {
        Some("stable".to_string())
    } else {
        // Check the DB directory for hints
        None
    }
}

/// Detect the Zed channel from a DB path.
pub fn channel_from_db_path(db_path: &Path) -> Option<String> {
    let path_str = db_path.to_string_lossy();
    if path_str.contains("0-preview") {
        Some("preview".to_string())
    } else if path_str.contains("0-stable") {
        Some("stable".to_string())
    } else {
        None
    }
}

/// Return the CLI command name for a given Zed channel.
///
/// Maps `"preview"` → `"zed-preview"`, anything else → `"zed"`.
pub fn zed_cli_command(channel: Option<&str>) -> &'static str {
    match channel {
        Some("preview") => "zed-preview",
        _ => "zed",
    }
}

/// Compute the hook socket path for a given channel and PID.
///
/// Pattern: `/tmp/zed-prj-workspace-{channel}-{pid}.sock`
pub fn hook_socket_path(channel: &str, pid: u32) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/zed-prj-workspace-{channel}-{pid}.sock"
    ))
}

/// Scan for any active hook socket matching the given channel.
///
/// Globs `/tmp/zed-prj-workspace-{channel}-*.sock` and returns the first
/// socket that exists. The caller should verify connectivity with a ping.
pub fn find_hook_socket(channel: &str) -> Option<PathBuf> {
    let prefix = format!("/tmp/zed-prj-workspace-{channel}-");
    let suffix = ".sock";
    let dir = Path::new("/tmp");

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(suffix) {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    // Sort by mtime descending (most recent first)
    candidates.sort_by(|a, b| {
        let ma = std::fs::metadata(a).and_then(|m| m.modified()).ok();
        let mb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_mapping() {
        let m = WorkspaceMapping::new(110, "my-project.code-workspace", Some("preview"));
        assert_eq!(m.workspace_id, 110);
        assert_eq!(m.workspace_file, "my-project.code-workspace");
        assert_eq!(m.zed_channel.as_deref(), Some("preview"));
        assert!(m.last_sync_ts.is_none());
    }

    #[test]
    fn resolve_relative() {
        let m = WorkspaceMapping::new(1, "project.code-workspace", None);
        let resolved = m.resolve_workspace_file(Path::new("/home/user/codes/myproject"));
        assert_eq!(
            resolved,
            PathBuf::from("/home/user/codes/myproject/project.code-workspace")
        );
    }

    #[test]
    fn resolve_parent_relative() {
        let m = WorkspaceMapping::new(1, "../shared.code-workspace", None);
        let resolved = m.resolve_workspace_file(Path::new("/home/user/codes/myproject"));
        assert_eq!(
            resolved,
            PathBuf::from("/home/user/codes/myproject/../shared.code-workspace")
        );
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let mut mapping = WorkspaceMapping::new(42, "test.code-workspace", Some("preview"));
        mapping.touch_sync_ts();
        mapping.write(root).unwrap();

        let read_back = WorkspaceMapping::read(root).unwrap();
        assert_eq!(read_back.workspace_id, 42);
        assert_eq!(read_back.workspace_file, "test.code-workspace");
        assert_eq!(read_back.zed_channel.as_deref(), Some("preview"));
        assert!(read_back.last_sync_ts.is_some());
    }

    #[test]
    fn read_nonexistent_returns_none() {
        assert!(WorkspaceMapping::read(Path::new("/nonexistent/path")).is_none());
    }

    #[test]
    fn file_path_correct() {
        let path = WorkspaceMapping::file_path(Path::new("/home/user/project"));
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/.zed/zed-project-workspace.json")
        );
    }

    #[test]
    fn is_valid_true_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let ws_file = dir.path().join("test.code-workspace");
        std::fs::write(&ws_file, "{}").unwrap();

        let m = WorkspaceMapping::new(1, "test.code-workspace", None);
        assert!(m.is_valid(dir.path()));
    }

    #[test]
    fn is_valid_false_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let m = WorkspaceMapping::new(1, "nonexistent.code-workspace", None);
        assert!(!m.is_valid(dir.path()));
    }

    #[test]
    fn channel_from_db_path_preview() {
        assert_eq!(
            channel_from_db_path(Path::new("/Users/x/Library/Application Support/Zed/db/0-preview/db.sqlite")),
            Some("preview".to_string())
        );
    }

    #[test]
    fn channel_from_db_path_stable() {
        assert_eq!(
            channel_from_db_path(Path::new("/Users/x/Library/Application Support/Zed/db/0-stable/db.sqlite")),
            Some("stable".to_string())
        );
    }

    #[test]
    fn zed_cli_command_preview() {
        assert_eq!(zed_cli_command(Some("preview")), "zed-preview");
    }

    #[test]
    fn zed_cli_command_stable() {
        assert_eq!(zed_cli_command(Some("stable")), "zed");
    }

    #[test]
    fn zed_cli_command_none() {
        assert_eq!(zed_cli_command(None), "zed");
    }

    #[test]
    fn hook_socket_path_format() {
        let p = hook_socket_path("preview", 12345);
        assert_eq!(
            p,
            PathBuf::from("/tmp/zed-prj-workspace-preview-12345.sock")
        );
    }
}
