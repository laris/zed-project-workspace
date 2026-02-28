//! Auto-discovery: find the workspace file to sync to.
//!
//! Priority chain:
//! 0. Check project_name in .zed/settings.json (format: {workspace_id}:{folder_name})
//! 1. Check legacy mapping files (.zed/zed-project-workspace.json)
//! 2. Scan opened folder roots for *.code-workspace files
//! 3. Check legacy v1 project_name format ({id}:{name}.code-workspace)
//! 4. Bootstrap: create .zed/settings.json and .code-workspace

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{OnceLock, RwLock};

use zed_prj_workspace::workspace_file;

/// Cached Zed DB path (found once at init).
static ZED_DB_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The resolved workspace file path and workspace_id.
pub static WORKSPACE_TARGET: RwLock<Option<(PathBuf, i64)>> = RwLock::new(None);

/// Timestamp of last discovery attempt (prevents hammering on failure).
pub static LAST_DISCOVERY_MS: AtomicU64 = AtomicU64::new(0);

/// Initialize the DB path at startup.
pub fn init_db_path() {
    let home = std::env::var("HOME").unwrap_or_default();
    let db_dir = PathBuf::from(&home).join("Library/Application Support/Zed/db");
    tracing::debug!("Searching for Zed DB in: {}", db_dir.display());
    let db_path = find_workspace_db(&db_dir);
    ZED_DB_PATH.get_or_init(|| db_path);
}

/// Get the cached DB path.
pub fn db_path() -> Option<&'static PathBuf> {
    ZED_DB_PATH.get().and_then(|opt| opt.as_ref())
}

/// Auto-discover which .code-workspace file to sync to.
/// Called from a background thread — never from the hot path.
pub fn discover_workspace_target() -> Result<Option<(PathBuf, i64)>, Box<dyn std::error::Error>> {
    let db_path = match ZED_DB_PATH.get() {
        Some(Some(path)) => path,
        _ => return Ok(None),
    };

    // Query DB for the most recent workspace entry
    let (workspace_id, folder_roots) = match query_latest_workspace(db_path)? {
        Some(entry) => entry,
        None => return Ok(None),
    };
    if folder_roots.is_empty() {
        return Ok(None);
    }

    tracing::info!(
        "Discovery: workspace_id={}, roots={:?}",
        workspace_id,
        folder_roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
    );

    // Priority 0 (NEW): Check project_name in .zed/settings.json
    for root in &folder_roots {
        if let Some(raw) = zed_prj_workspace::settings::read_project_name(root) {
            let (id_opt, name) = zed_prj_workspace::settings::parse_project_name(&raw);
            // Only use if it has a valid workspace_id and matches the current DB id
            if let Some(id) = id_opt {
                let ws_file = zed_prj_workspace::settings::workspace_file_for_name(root, &name);
                if ws_file.exists() {
                    tracing::info!(
                        "Priority 0: Found via project_name '{}' in {}: {}",
                        raw, root.display(), ws_file.display()
                    );
                    // Update project_name if workspace_id changed
                    if id != workspace_id {
                        let new_pn = zed_prj_workspace::settings::format_project_name(workspace_id, &name);
                        // Write only to this root (the primary root — it matched by folder name)
                        let _ = zed_prj_workspace::settings::write_project_name(root, &new_pn);
                        tracing::info!("Updated project_name: {} → {}", raw, new_pn);
                    }
                    // Clean up legacy mapping file
                    zed_prj_workspace::mapping::cleanup_legacy_mapping(root);
                    return Ok(Some((ws_file, workspace_id)));
                }
            }
        }
    }

    // Priority 1 (legacy): Check mapping files (.zed/zed-project-workspace.json) in roots
    for root in &folder_roots {
        if let Some(mapping) = zed_prj_workspace::mapping::WorkspaceMapping::read(root) {
            let ws_path = mapping.resolve_workspace_file(root);
            if ws_path.exists() {
                tracing::info!(
                    "Priority 0: Found via mapping in {}: {} (mapped_id={}, db_id={})",
                    root.display(), ws_path.display(), mapping.workspace_id, workspace_id
                );
                // Update mapping if workspace_id changed (Zed created new ID after path change)
                if mapping.workspace_id != workspace_id {
                    tracing::info!(
                        "Updating mapping workspace_id: {} → {}",
                        mapping.workspace_id, workspace_id
                    );
                    let mut updated = mapping;
                    updated.workspace_id = workspace_id;
                    updated.touch_sync_ts();
                    let _ = updated.write(root);
                }
                return Ok(Some((ws_path, workspace_id)));
            }
        }
    }

    // Priority 1: Search each folder root for *.code-workspace files
    tracing::debug!("Priority 1: Scanning {} root(s) for *.code-workspace files...", folder_roots.len());
    let candidates = find_workspace_files_in_roots(&folder_roots);
    tracing::debug!(
        "Priority 1: Found {} candidate(s): {:?}",
        candidates.len(),
        candidates.iter().map(|c| c.display().to_string()).collect::<Vec<_>>()
    );

    if candidates.len() == 1 {
        tracing::info!("Priority 2: Found single .code-workspace: {}", candidates[0].display());
        // Write clean project_name to all roots
        let _ = write_zed_settings_for_roots(workspace_id, &candidates[0], &folder_roots);
        // Also write legacy mapping for backward compat
        let channel = zed_prj_workspace::mapping::detect_zed_channel();
        let mapping = zed_prj_workspace::mapping::WorkspaceMapping::new(
            workspace_id,
            candidates[0].file_name().unwrap_or_default().to_str().unwrap_or(""),
            channel.as_deref(),
        );
        let _ = mapping.write_to_roots(&folder_roots, &candidates[0]);
        return Ok(Some((candidates[0].clone(), workspace_id)));
    }

    // Multiple candidates, no resolution — pick first alphabetically
    if candidates.len() > 1 {
        let mut sorted = candidates;
        sorted.sort();
        tracing::warn!(
            "Multiple .code-workspace files found, picking first: {:?}",
            sorted.iter().map(|c| c.display().to_string()).collect::<Vec<_>>()
        );
        let _ = write_zed_settings_for_roots(workspace_id, &sorted[0], &folder_roots);
        return Ok(Some((sorted[0].clone(), workspace_id)));
    }

    // Priority 3 (legacy): Check .zed/settings.json for v1 project_name format
    // This handles the old "{id}:{name}.code-workspace" format
    tracing::debug!("Priority 3: Checking .zed/settings.json for v1 project_name...");
    let mut settings_candidates: Vec<PathBuf> = Vec::new();
    for root in &folder_roots {
        if let Some(raw) = zed_prj_workspace::settings::read_project_name(root) {
            let (id_opt, name) = zed_prj_workspace::settings::parse_project_name(&raw);
            if let Some(id) = id_opt {
                if id == workspace_id {
                    // Try with .code-workspace extension (v1 stored the full filename)
                    let ws_path = root.join(format!("{}.code-workspace", name));
                    if ws_path.exists() {
                        settings_candidates.push(ws_path);
                    }
                }
            }
        }
    }

    if settings_candidates.len() == 1 {
        let target = &settings_candidates[0];
        tracing::info!("Priority 3: Found via v1 project_name: {}", target.display());
        // Migrate to clean format
        let _ = write_zed_settings_for_roots(workspace_id, target, &folder_roots);
        return Ok(Some((target.clone(), workspace_id)));
    }

    // Priority 4: Bootstrap — create .code-workspace and .zed/settings.json
    tracing::info!("Priority 4: No existing workspace file found — bootstrapping");
    match bootstrap_workspace(workspace_id, &folder_roots) {
        Ok(path) => Ok(Some((path, workspace_id))),
        Err(e) => {
            tracing::warn!("Bootstrap failed: {e}");
            Ok(None)
        }
    }
}

/// Query the most recently written workspace entry from Zed's DB.
fn query_latest_workspace(
    db_path: &Path,
) -> Result<Option<(i64, Vec<PathBuf>)>, Box<dyn std::error::Error>> {
    tracing::debug!("Opening DB read-only: {}", db_path.display());
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "busy_timeout", 500)?;

    let mut stmt = conn.prepare(
        "SELECT workspace_id, paths FROM workspaces ORDER BY timestamp DESC LIMIT 1",
    )?;

    let result = stmt
        .query_row(rusqlite::params![], |row| {
            let workspace_id: i64 = row.get(0)?;
            let raw_paths: String = row.get(1)?;
            tracing::debug!("DB query result: workspace_id={}, raw_paths={:?}", workspace_id, raw_paths);
            Ok((workspace_id, parse_workspace_paths(&raw_paths)))
        })
        .ok();

    if result.is_none() {
        tracing::debug!("No workspace entries in DB");
    }
    Ok(result)
}

/// Scan each folder root for *.code-workspace files (root-level only, non-recursive).
pub fn find_workspace_files_in_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for root in roots {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("code-workspace")
                    && path.is_file()
                {
                    candidates.push(path);
                }
            }
        }
    }
    candidates
}

/// Read .zed/settings.json in a folder root and extract the project_name field.
/// Expected format: `{ "project_name": "{workspace_id}:{filename}.code-workspace" }`
///
/// **Deprecated**: Use `zed_prj_workspace::settings::read_project_name()` +
/// `parse_project_name()` instead. Kept for test backward compat.
#[allow(dead_code)]
pub fn read_zed_settings_project_name(folder_root: &Path) -> Option<(i64, String)> {
    let settings_path = folder_root.join(".zed").join("settings.json");
    let content = std::fs::read_to_string(&settings_path).ok()?;

    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let project_name = value.get("project_name")?.as_str()?;

    // Parse "{id}:{filename}" format
    let colon_pos = project_name.find(':')?;
    let id_str = &project_name[..colon_pos];
    let filename = &project_name[colon_pos + 1..];

    let id: i64 = id_str.parse().ok()?;
    if filename.ends_with(".code-workspace") && !filename.is_empty() {
        Some((id, filename.to_string()))
    } else {
        None
    }
}

/// Write project_name to `.zed/settings.json` in the **primary root only**.
///
/// Format: `"{workspace_id}:{folder_name}"` (clean format, no .code-workspace extension).
/// The primary root is identified by `find_primary_root()` (folder_name == name).
/// Also cleans up stale project_name from non-primary roots.
fn write_zed_settings_for_roots(
    workspace_id: i64,
    ws_file: &Path,
    folder_roots: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    let name = ws_file
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let project_name = zed_prj_workspace::settings::format_project_name(workspace_id, name);

    // Find and write only to the primary root
    if let Some(primary) = zed_prj_workspace::settings::find_primary_root(folder_roots, &project_name) {
        zed_prj_workspace::settings::write_project_name(primary, &project_name)?;
        tracing::info!("Wrote project_name '{}' to primary root: {}", project_name, primary.display());

        // Clean up stale project_name from non-primary roots
        let _ = zed_prj_workspace::settings::cleanup_stale_project_names(folder_roots, primary);
    } else {
        // No root matches the name — write to the first root as fallback
        if let Some(first) = folder_roots.first() {
            zed_prj_workspace::settings::write_project_name(first, &project_name)?;
            tracing::info!("Wrote project_name '{}' to first root (no name match): {}", project_name, first.display());
        }
    }
    Ok(())
}

/// Bootstrap: create .zed/settings.json and .code-workspace file.
fn bootstrap_workspace(
    workspace_id: i64,
    folder_roots: &[PathBuf],
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if folder_roots.is_empty() {
        return Err("No folder roots to bootstrap".into());
    }

    let primary_root = &folder_roots[0];
    let folder_name = primary_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let ws_filename = format!("{}.code-workspace", folder_name);
    let ws_path = primary_root.join(&ws_filename);

    // Step 1: Create the .code-workspace file
    if !ws_path.exists() {
        let ws = workspace_file::CodeWorkspaceFile {
            folders: folder_roots
                .iter()
                .map(|root| {
                    let rel = if root == primary_root {
                        ".".to_string()
                    } else {
                        root.strip_prefix(primary_root)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| root.to_string_lossy().to_string())
                    };
                    workspace_file::WorkspaceFolder {
                        path: rel,
                        name: None,
                    }
                })
                .collect(),
            extra: serde_json::Map::new(),
        };
        ws.write_to_file(&ws_path)?;
        tracing::info!("Created {}", ws_path.display());
    }

    // Step 2: Write project_name to .zed/settings.json in each folder root (clean format)
    write_zed_settings_for_roots(workspace_id, &ws_path, folder_roots)?;

    // Step 3: Also write legacy mapping for backward compat (will be cleaned up on next discovery)
    let channel = zed_prj_workspace::mapping::detect_zed_channel();
    let mapping = zed_prj_workspace::mapping::WorkspaceMapping::new(
        workspace_id,
        &ws_filename,
        channel.as_deref(),
    );
    let _ = mapping.write_to_roots(folder_roots, &ws_path);

    Ok(ws_path)
}

/// Find the correct Zed DB file.
fn find_workspace_db(db_dir: &Path) -> Option<PathBuf> {
    let is_preview = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.contains("Zed Preview")))
        .unwrap_or(false);

    tracing::debug!("Zed variant: {}", if is_preview { "Preview" } else { "Stable" });

    let preferred = if is_preview { "0-preview" } else { "0-stable" };
    let preferred_db = db_dir.join(preferred).join("db.sqlite");
    if preferred_db.exists() {
        tracing::info!("Using DB: {}", preferred_db.display());
        return Some(preferred_db);
    }
    tracing::debug!("Preferred DB not found: {}", preferred_db.display());

    // Fallback: try any 0-* directory (excluding 0-global)
    tracing::debug!("Scanning {} for fallback DB...", db_dir.display());
    let entries = std::fs::read_dir(db_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("0-") && name_str != "0-global" {
            let candidate = entry.path().join("db.sqlite");
            if candidate.exists() {
                tracing::info!("Using DB (fallback): {}", candidate.display());
                return Some(candidate);
            }
        }
    }
    tracing::warn!("No Zed DB found in {}", db_dir.display());
    None
}

/// Parse workspace paths (newline-separated text).
fn parse_workspace_paths(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_zed_settings_project_name() {
        let dir = tempfile::tempdir().unwrap();
        let zed_dir = dir.path().join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{ "project_name": "84:myproject.code-workspace" }"#,
        )
        .unwrap();

        let result = read_zed_settings_project_name(dir.path());
        assert_eq!(result, Some((84, "myproject.code-workspace".to_string())));
    }

    #[test]
    fn test_read_zed_settings_no_project_name() {
        let dir = tempfile::tempdir().unwrap();
        let zed_dir = dir.path().join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(zed_dir.join("settings.json"), r#"{ "tab_size": 4 }"#).unwrap();

        let result = read_zed_settings_project_name(dir.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_read_zed_settings_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_zed_settings_project_name(dir.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_read_zed_settings_invalid_format() {
        let dir = tempfile::tempdir().unwrap();
        let zed_dir = dir.path().join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{ "project_name": "myproject.code-workspace" }"#,
        )
        .unwrap();

        let result = read_zed_settings_project_name(dir.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_read_zed_settings_non_workspace_extension() {
        let dir = tempfile::tempdir().unwrap();
        let zed_dir = dir.path().join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{ "project_name": "84:myproject.json" }"#,
        )
        .unwrap();

        let result = read_zed_settings_project_name(dir.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_workspace_files_in_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();

        let ws_file = root.join("project.code-workspace");
        std::fs::write(&ws_file, r#"{"folders":[{"path":"."}]}"#).unwrap();
        std::fs::write(root.join("readme.md"), "# Hello").unwrap();

        let candidates = find_workspace_files_in_roots(&[root]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], ws_file);
    }

    #[test]
    fn test_find_workspace_files_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();

        std::fs::write(root.join("a.code-workspace"), r#"{"folders":[{"path":"."}]}"#).unwrap();
        std::fs::write(root.join("b.code-workspace"), r#"{"folders":[{"path":"."}]}"#).unwrap();

        let candidates = find_workspace_files_in_roots(&[root]);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn test_find_workspace_files_empty() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = find_workspace_files_in_roots(&[dir.path().to_path_buf()]);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_bootstrap_single_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("my-project");
        std::fs::create_dir_all(&root).unwrap();

        let result = bootstrap_workspace(42, &[root.clone()]).unwrap();
        assert_eq!(result, root.join("my-project.code-workspace"));
        assert!(result.exists());

        let settings = std::fs::read_to_string(root.join(".zed/settings.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&settings).unwrap();
        assert_eq!(
            value["project_name"].as_str().unwrap(),
            "42:my-project"
        );

        let ws = workspace_file::CodeWorkspaceFile::from_file(&result).unwrap();
        assert_eq!(ws.folders.len(), 1);
        assert_eq!(ws.folders[0].path, ".");
    }

    #[test]
    fn test_bootstrap_multiple_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root1 = dir.path().join("frontend");
        let root2 = dir.path().join("backend");
        std::fs::create_dir_all(&root1).unwrap();
        std::fs::create_dir_all(&root2).unwrap();

        let result = bootstrap_workspace(99, &[root1.clone(), root2.clone()]).unwrap();
        assert_eq!(result, root1.join("frontend.code-workspace"));

        let ws = workspace_file::CodeWorkspaceFile::from_file(&result).unwrap();
        assert_eq!(ws.folders.len(), 2);

        // project_name written only to primary root (folder name == project name)
        assert!(root1.join(".zed/settings.json").exists());
        // Non-primary root should NOT have project_name
        assert!(!root2.join(".zed/settings.json").exists());

        let s1 = std::fs::read_to_string(root1.join(".zed/settings.json")).unwrap();
        let v1: serde_json::Value = serde_json::from_str(&s1).unwrap();
        assert_eq!(
            v1["project_name"].as_str().unwrap(),
            "99:frontend"
        );
    }

    #[test]
    fn test_bootstrap_does_not_overwrite_existing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("my-project");
        std::fs::create_dir_all(&root).unwrap();

        let ws_path = root.join("my-project.code-workspace");
        std::fs::write(
            &ws_path,
            r#"{"folders":[{"path":"."},{"path":"extra"}],"settings":{"tab_size":2}}"#,
        )
        .unwrap();

        let result = bootstrap_workspace(10, &[root.clone()]).unwrap();
        assert_eq!(result, ws_path);

        let ws = workspace_file::CodeWorkspaceFile::from_file(&result).unwrap();
        assert_eq!(ws.folders.len(), 2);
        assert!(ws.extra.contains_key("settings"));
    }

    #[test]
    fn test_write_zed_settings_merges_into_existing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let zed_dir = root.join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();

        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{ "tab_size": 4, "theme": "dark" }"#,
        )
        .unwrap();

        let ws_file = root.join("test.code-workspace");
        write_zed_settings_for_roots(50, &ws_file, &[root.clone()]).unwrap();

        let content = std::fs::read_to_string(zed_dir.join("settings.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(
            value["project_name"].as_str().unwrap(),
            "50:test"
        );
        assert_eq!(value["tab_size"].as_u64().unwrap(), 4);
        assert_eq!(value["theme"].as_str().unwrap(), "dark");
    }

    #[test]
    fn test_write_zed_settings_overwrites_project_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let zed_dir = root.join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();

        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{ "project_name": "10:original" }"#,
        )
        .unwrap();

        let ws_file = root.join("new.code-workspace");
        write_zed_settings_for_roots(99, &ws_file, &[root.clone()]).unwrap();

        let content = std::fs::read_to_string(zed_dir.join("settings.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        // New format always overwrites
        assert_eq!(value["project_name"].as_str().unwrap(), "99:new");
    }

    #[test]
    fn test_parse_workspace_paths() {
        let paths = parse_workspace_paths("/Users/test/project1\n/Users/test/project2");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("/Users/test/project1"));
        assert_eq!(paths[1], PathBuf::from("/Users/test/project2"));
    }

    #[test]
    fn test_parse_workspace_paths_empty() {
        assert!(parse_workspace_paths("").is_empty());
    }
}
