//! Parse, write, and diff `.code-workspace` files (VS Code / Cursor format).
//!
//! The `.code-workspace` format is a JSON file with at minimum a `folders` array:
//! ```json
//! {
//!   "folders": [
//!     { "path": "." },
//!     { "path": "../shared-lib" }
//!   ],
//!   "settings": {}
//! }
//! ```

use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to parse workspace file: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace file has no parent directory")]
    NoParentDir,
}

/// A folder entry in the workspace file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceFolder {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Raw `.code-workspace` JSON structure.
/// Uses `serde_json::Value` for `settings` and other unknown fields to preserve them on round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeWorkspaceFile {
    pub folders: Vec<WorkspaceFolder>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Resolved workspace: folders as absolute paths.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    pub file_path: PathBuf,
    pub folders: Vec<PathBuf>,
}

/// Result of diffing two folder lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderDiff {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
}

impl CodeWorkspaceFile {
    /// Parse from JSON string.
    pub fn parse(content: &str) -> Result<Self, Error> {
        Ok(serde_json::from_str(content)?)
    }

    /// Parse from a file path.
    pub fn from_file(path: &Path) -> Result<Self, Error> {
        let content = std::fs::read_to_string(path)?;
        Self::parse(&content)
    }

    /// Serialize to pretty JSON string.
    pub fn to_json_pretty(&self) -> Result<String, Error> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Write to a file path.
    pub fn write_to_file(&self, path: &Path) -> Result<(), Error> {
        let json = self.to_json_pretty()?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Resolve folder paths relative to the workspace file's parent directory.
    ///
    /// All resolved paths are normalized (`.` and `..` resolved) for correct comparison.
    pub fn resolve(&self, workspace_file_path: &Path) -> Result<ResolvedWorkspace, Error> {
        let base_dir = workspace_file_path.parent().ok_or(Error::NoParentDir)?;
        let folders = self
            .folders
            .iter()
            .map(|f| {
                let p = PathBuf::from(&f.path);
                let abs = if p.is_absolute() {
                    p
                } else {
                    base_dir.join(p)
                };
                paths::normalize_path(&abs)
            })
            .collect();
        Ok(ResolvedWorkspace {
            file_path: workspace_file_path.to_path_buf(),
            folders,
        })
    }

    /// Set the folder list from absolute paths, making them relative to workspace file location.
    pub fn set_folders_from_absolute(
        &mut self,
        workspace_file_path: &Path,
        absolute_paths: &[PathBuf],
    ) -> Result<(), Error> {
        let base_dir = workspace_file_path.parent().ok_or(Error::NoParentDir)?;
        self.folders = absolute_paths
            .iter()
            .map(|abs| {
                let rel = paths::relative_path(base_dir, abs);
                WorkspaceFolder {
                    path: rel.to_string_lossy().to_string(),
                    name: None,
                }
            })
            .collect();
        Ok(())
    }

    /// Add a folder (as absolute path) if not already present.
    pub fn add_folder(&mut self, workspace_file_path: &Path, abs_path: &Path) -> Result<bool, Error> {
        let base_dir = workspace_file_path.parent().ok_or(Error::NoParentDir)?;
        let resolved = self.resolve(workspace_file_path)?;
        let normalized_target = paths::normalize_path(abs_path);
        if resolved.folders.iter().any(|f| paths::paths_equal(f, &normalized_target)) {
            return Ok(false);
        }
        let rel = paths::relative_path(base_dir, abs_path);
        self.folders.push(WorkspaceFolder {
            path: rel.to_string_lossy().to_string(),
            name: None,
        });
        Ok(true)
    }

    /// Remove a folder (by absolute path) if present.
    pub fn remove_folder(&mut self, workspace_file_path: &Path, abs_path: &Path) -> Result<bool, Error> {
        let base_dir = workspace_file_path.parent().ok_or(Error::NoParentDir)?;
        let normalized_target = paths::normalize_path(abs_path);
        let initial_len = self.folders.len();
        self.folders.retain(|f| {
            let resolved = if PathBuf::from(&f.path).is_absolute() {
                PathBuf::from(&f.path)
            } else {
                base_dir.join(&f.path)
            };
            !paths::paths_equal(&paths::normalize_path(&resolved), &normalized_target)
        });
        Ok(self.folders.len() != initial_len)
    }
}

/// Compute the diff between two sets of absolute folder paths.
///
/// This compares membership (set difference). Use `diff_folders_ordered` to also detect reordering.
pub fn diff_folders(old: &[PathBuf], new: &[PathBuf]) -> FolderDiff {
    let old_normalized: Vec<PathBuf> = old.iter().map(|p| paths::normalize_path(p)).collect();
    let new_normalized: Vec<PathBuf> = new.iter().map(|p| paths::normalize_path(p)).collect();

    let old_set: BTreeSet<&Path> = old_normalized.iter().map(|p| p.as_path()).collect();
    let new_set: BTreeSet<&Path> = new_normalized.iter().map(|p| p.as_path()).collect();

    let added = new_set
        .difference(&old_set)
        .map(|p| p.to_path_buf())
        .collect();
    let removed = old_set
        .difference(&new_set)
        .map(|p| p.to_path_buf())
        .collect();
    let unchanged = old_set
        .intersection(&new_set)
        .map(|p| p.to_path_buf())
        .collect();

    FolderDiff {
        added,
        removed,
        unchanged,
    }
}

/// Check if two folder lists have the same membership AND the same order.
pub fn folders_match_ordered(a: &[PathBuf], b: &[PathBuf]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| paths::paths_equal(x, y))
}

/// Check if two folder lists have the same membership (ignoring order).
pub fn folders_match_set(a: &[PathBuf], b: &[PathBuf]) -> bool {
    let diff = diff_folders(a, b);
    diff.added.is_empty() && diff.removed.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let json = r#"{"folders": [{"path": "."}]}"#;
        let ws = CodeWorkspaceFile::parse(json).unwrap();
        assert_eq!(ws.folders.len(), 1);
        assert_eq!(ws.folders[0].path, ".");
    }

    #[test]
    fn parse_with_settings_preserved() {
        let json = r#"{
            "folders": [{"path": "src"}],
            "settings": {"editor.tabSize": 2}
        }"#;
        let ws = CodeWorkspaceFile::parse(json).unwrap();
        assert_eq!(ws.folders.len(), 1);
        assert!(ws.extra.contains_key("settings"));

        // Round-trip preserves settings
        let output = ws.to_json_pretty().unwrap();
        let ws2 = CodeWorkspaceFile::parse(&output).unwrap();
        assert!(ws2.extra.contains_key("settings"));
    }

    #[test]
    fn parse_with_names() {
        let json = r#"{"folders": [{"name": "Frontend", "path": "packages/frontend"}]}"#;
        let ws = CodeWorkspaceFile::parse(json).unwrap();
        assert_eq!(ws.folders[0].name.as_deref(), Some("Frontend"));
        assert_eq!(ws.folders[0].path, "packages/frontend");
    }

    #[test]
    fn resolve_relative_paths() {
        let json = r#"{"folders": [
            {"path": "."},
            {"path": "packages/frontend"},
            {"path": "../shared"}
        ]}"#;
        let ws = CodeWorkspaceFile::parse(json).unwrap();
        let resolved = ws
            .resolve(Path::new("/projects/my-project.code-workspace"))
            .unwrap();
        assert_eq!(resolved.folders[0], PathBuf::from("/projects"));
        assert_eq!(
            resolved.folders[1],
            PathBuf::from("/projects/packages/frontend")
        );
        // normalize_path resolves "../shared" relative to /projects → /shared
        assert_eq!(resolved.folders[2], PathBuf::from("/shared"));
    }

    #[test]
    fn resolve_absolute_paths() {
        let json = r#"{"folders": [{"path": "/absolute/path"}]}"#;
        let ws = CodeWorkspaceFile::parse(json).unwrap();
        let resolved = ws
            .resolve(Path::new("/projects/my.code-workspace"))
            .unwrap();
        assert_eq!(resolved.folders[0], PathBuf::from("/absolute/path"));
    }

    #[test]
    fn diff_folders_works() {
        let old = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let new = vec![
            PathBuf::from("/b"),
            PathBuf::from("/c"),
            PathBuf::from("/d"),
        ];
        let diff = diff_folders(&old, &new);
        assert_eq!(diff.added, vec![PathBuf::from("/d")]);
        assert_eq!(diff.removed, vec![PathBuf::from("/a")]);
        assert_eq!(
            diff.unchanged,
            vec![PathBuf::from("/b"), PathBuf::from("/c")]
        );
    }

    #[test]
    fn add_folder_dedup() {
        let json = r#"{"folders": [{"path": "src"}]}"#;
        let mut ws = CodeWorkspaceFile::parse(json).unwrap();
        let ws_path = Path::new("/projects/my.code-workspace");

        // Adding existing folder returns false
        let added = ws.add_folder(ws_path, Path::new("/projects/src")).unwrap();
        assert!(!added);
        assert_eq!(ws.folders.len(), 1);

        // Adding new folder returns true
        let added = ws
            .add_folder(ws_path, Path::new("/projects/tests"))
            .unwrap();
        assert!(added);
        assert_eq!(ws.folders.len(), 2);
    }

    #[test]
    fn remove_folder_works() {
        let json = r#"{"folders": [{"path": "src"}, {"path": "tests"}]}"#;
        let mut ws = CodeWorkspaceFile::parse(json).unwrap();
        let ws_path = Path::new("/projects/my.code-workspace");

        let removed = ws
            .remove_folder(ws_path, Path::new("/projects/src"))
            .unwrap();
        assert!(removed);
        assert_eq!(ws.folders.len(), 1);
        assert_eq!(ws.folders[0].path, "tests");
    }

    #[test]
    fn roundtrip_json() {
        let json = r#"{
  "folders": [
    {
      "path": "."
    },
    {
      "path": "packages/frontend"
    }
  ],
  "settings": {
    "editor.tabSize": 2
  }
}"#;
        let ws = CodeWorkspaceFile::parse(json).unwrap();
        let output = ws.to_json_pretty().unwrap();
        let ws2 = CodeWorkspaceFile::parse(&output).unwrap();
        assert_eq!(ws.folders.len(), ws2.folders.len());
        assert_eq!(ws.folders[0].path, ws2.folders[0].path);
    }

    #[test]
    fn parse_invalid_json_errors() {
        let result = CodeWorkspaceFile::parse("not json");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_folders_errors() {
        let result = CodeWorkspaceFile::parse(r#"{"settings": {}}"#);
        assert!(result.is_err());
    }
}
