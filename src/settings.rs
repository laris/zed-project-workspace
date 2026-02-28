//! Read/write/parse `project_name` from `.zed/settings.json`.
//!
//! `project_name` is the single identity anchor for workspace mapping:
//!   - Format: `"{workspace_id}:{root_folder_name}"` (e.g., `"117:zed-project-workspace"`)
//!   - Zed displays it in the title bar and "Recent Projects" list
//!   - The workspace_id maps to Zed's SQLite DB
//!   - The name portion = root folder name = deterministic `.code-workspace` filename
//!
//! Legacy v1 format: `"{workspace_id}:{name}.code-workspace"` (e.g., `"97:my-project.code-workspace"`)
//! This module handles both formats transparently.

use std::path::{Path, PathBuf};

/// Read the raw `project_name` value from `.zed/settings.json` in a worktree root.
///
/// Returns `None` if the file doesn't exist, can't be parsed, or has no `project_name`.
pub fn read_project_name(root: &Path) -> Option<String> {
    let settings_path = root.join(".zed").join("settings.json");
    let content = std::fs::read_to_string(&settings_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed
        .get("project_name")?
        .as_str()
        .map(|s| s.to_string())
}

/// Parse a `project_name` value into `(workspace_id, name)`.
///
/// Handles three formats:
/// - `"117:zed-project-workspace"` → `(Some(117), "zed-project-workspace")`
/// - `"97:my-project.code-workspace"` (v1) → `(Some(97), "my-project")`
/// - `"my-project"` (no id) → `(None, "my-project")`
pub fn parse_project_name(raw: &str) -> (Option<i64>, String) {
    // Try to split on first ':'
    if let Some(colon_pos) = raw.find(':') {
        let id_part = &raw[..colon_pos];
        let name_part = &raw[colon_pos + 1..];

        if let Ok(id) = id_part.parse::<i64>() {
            // Strip .code-workspace extension if present (v1 format)
            let name = name_part
                .strip_suffix(".code-workspace")
                .unwrap_or(name_part);
            return (Some(id), name.to_string());
        }
    }
    // No valid id prefix — treat entire string as name
    (None, raw.to_string())
}

/// Format a `project_name` value from workspace_id and name.
///
/// Returns `"{workspace_id}:{name}"`, e.g., `"117:zed-project-workspace"`.
pub fn format_project_name(workspace_id: i64, name: &str) -> String {
    format!("{}:{}", workspace_id, name)
}

/// Write `project_name` into `.zed/settings.json`, preserving all other keys.
///
/// Creates the `.zed/` directory and `settings.json` if they don't exist.
pub fn write_project_name(root: &Path, project_name: &str) -> std::io::Result<()> {
    let zed_dir = root.join(".zed");
    std::fs::create_dir_all(&zed_dir)?;
    let settings_path = zed_dir.join("settings.json");

    let mut map = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content)
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    map.insert(
        "project_name".to_string(),
        serde_json::Value::String(project_name.to_string()),
    );

    let json = serde_json::to_string_pretty(&map).map_err(std::io::Error::other)?;
    std::fs::write(&settings_path, json)
}

/// Write `project_name` to `.zed/settings.json` in ALL worktree roots.
///
/// **DEPRECATED**: Use `write_project_name()` on the primary root only.
/// This function pollutes non-primary roots with identity that doesn't belong.
#[deprecated(note = "write only to the primary root via write_project_name()")]
pub fn write_project_name_to_roots(
    roots: &[PathBuf],
    project_name: &str,
) -> std::io::Result<()> {
    for root in roots {
        write_project_name(root, project_name)?;
    }
    Ok(())
}

/// Find the primary root among `roots` for a given `project_name`.
///
/// The primary root is the one whose folder name matches the name portion
/// of `project_name` (e.g., `"117:zed-project-workspace"` → folder `zed-project-workspace`).
///
/// Applies triple-match validation:
/// 1. `root.folder_name == name` (root IS the named project)
/// 2. `root/{name}.code-workspace` exists (workspace file present)
///
/// Returns the first root passing check 1 (check 2 is defense-in-depth).
/// Falls back to the first root passing only check 1 if no root passes both.
pub fn find_primary_root<'a>(roots: &'a [PathBuf], project_name: &str) -> Option<&'a Path> {
    let (_id, name) = parse_project_name(project_name);
    if name.is_empty() {
        return None;
    }

    let mut folder_match: Option<&Path> = None;

    for root in roots {
        let folder_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if folder_name == name {
            // Check 2: does .code-workspace exist?
            let ws_file = root.join(format!("{}.code-workspace", name));
            if ws_file.exists() {
                return Some(root); // Full triple-match
            }
            // Folder matches but no .code-workspace — remember as fallback
            if folder_match.is_none() {
                folder_match = Some(root);
            }
        }
    }

    folder_match
}

/// Remove `project_name` from `.zed/settings.json` in all roots EXCEPT `primary_root`.
///
/// Used to clean up trash left by the old `write_project_name_to_roots()` behavior.
pub fn cleanup_stale_project_names(roots: &[PathBuf], primary_root: &Path) -> std::io::Result<()> {
    for root in roots {
        if root.as_path() == primary_root {
            continue;
        }
        let settings_path = root.join(".zed").join("settings.json");
        if !settings_path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&settings_path)?;
        if let Ok(mut map) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content)
        {
            if map.remove("project_name").is_some() {
                if map.is_empty() {
                    // settings.json would be empty `{}` — remove the file
                    std::fs::remove_file(&settings_path)?;
                } else {
                    let json = serde_json::to_string_pretty(&map)
                        .map_err(std::io::Error::other)?;
                    std::fs::write(&settings_path, json)?;
                }
                tracing::info!(
                    "Cleaned stale project_name from {}",
                    root.display()
                );
            }
        }
    }
    Ok(())
}

/// Derive the `.code-workspace` file path from a root and a project name.
///
/// Convention: `{root}/{name}.code-workspace`
pub fn workspace_file_for_name(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{}.code-workspace", name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_current_format() {
        let (id, name) = parse_project_name("117:zed-project-workspace");
        assert_eq!(id, Some(117));
        assert_eq!(name, "zed-project-workspace");
    }

    #[test]
    fn parse_v1_format() {
        let (id, name) = parse_project_name("97:my-project.code-workspace");
        assert_eq!(id, Some(97));
        assert_eq!(name, "my-project");
    }

    #[test]
    fn parse_clean_name_no_id() {
        let (id, name) = parse_project_name("my-project");
        assert_eq!(id, None);
        assert_eq!(name, "my-project");
    }

    #[test]
    fn parse_name_with_colon_but_not_id() {
        let (id, name) = parse_project_name("foo:bar");
        assert_eq!(id, None);
        assert_eq!(name, "foo:bar");
    }

    #[test]
    fn format_roundtrip() {
        let formatted = format_project_name(117, "zed-project-workspace");
        assert_eq!(formatted, "117:zed-project-workspace");
        let (id, name) = parse_project_name(&formatted);
        assert_eq!(id, Some(117));
        assert_eq!(name, "zed-project-workspace");
    }

    #[test]
    fn workspace_file_path() {
        let path = workspace_file_for_name(Path::new("/home/user/codes/my-project"), "my-project");
        assert_eq!(
            path,
            PathBuf::from("/home/user/codes/my-project/my-project.code-workspace")
        );
    }

    #[test]
    fn write_and_read_project_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_project_name(root, "117:my-project").unwrap();

        let raw = read_project_name(root).unwrap();
        assert_eq!(raw, "117:my-project");
    }

    #[test]
    fn write_preserves_existing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let zed_dir = root.join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{"tab_size": 4, "theme": "dark"}"#,
        )
        .unwrap();

        write_project_name(root, "42:test").unwrap();

        let content = std::fs::read_to_string(zed_dir.join("settings.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["project_name"], "42:test");
        assert_eq!(value["tab_size"], 4);
        assert_eq!(value["theme"], "dark");
    }

    #[test]
    fn write_overwrites_existing_project_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_project_name(root, "10:old-name").unwrap();
        write_project_name(root, "20:new-name").unwrap();

        let raw = read_project_name(root).unwrap();
        assert_eq!(raw, "20:new-name");
    }

    #[test]
    fn write_to_multiple_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root1 = dir.path().join("root1");
        let root2 = dir.path().join("root2");
        std::fs::create_dir_all(&root1).unwrap();
        std::fs::create_dir_all(&root2).unwrap();

        write_project_name_to_roots(&[root1.clone(), root2.clone()], "99:shared").unwrap();

        assert_eq!(read_project_name(&root1).unwrap(), "99:shared");
        assert_eq!(read_project_name(&root2).unwrap(), "99:shared");
    }

    #[test]
    fn read_nonexistent_returns_none() {
        assert!(read_project_name(Path::new("/nonexistent/path")).is_none());
    }

    #[test]
    fn read_no_project_name_field() {
        let dir = tempfile::tempdir().unwrap();
        let zed_dir = dir.path().join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(zed_dir.join("settings.json"), r#"{"tab_size": 4}"#).unwrap();

        assert!(read_project_name(dir.path()).is_none());
    }
}
