//! Workspace discovery: find the `.code-workspace` file and DB mapping for a set of worktree roots.
//!
//! Discovery priority:
//! 0. Check `project_name` in `.zed/settings.json` → parse workspace_id → validate in DB.
//! 1. Check `.zed/zed-project-workspace.json` legacy mapping file (backward compat).
//! 2. Scan roots for `*.code-workspace` → auto-set project_name if exactly one found.
//! 3. Bootstrap: create `{primary_root_name}.code-workspace` + set project_name in all roots.
//!
//! This module was extracted from the hook's `discovery.rs` to be shared by both
//! the hook and MCP crates.

use crate::mapping::{WorkspaceMapping, channel_from_db_path};
use crate::workspace_db::{self, ZedDbReader};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] workspace_db::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no workspace roots provided")]
    NoRoots,
    #[error("workspace file not found after discovery")]
    NotFound,
}

/// Result of workspace discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryResult {
    /// The Zed workspace_id (from DB or project_name).
    pub workspace_id: i64,
    /// The project name (folder name of target root, e.g., "zed-project-workspace").
    pub project_name: String,
    /// Absolute path to the `.code-workspace` file.
    pub workspace_file: PathBuf,
    /// The mapping that was found or created (legacy — will be deprecated).
    pub mapping: WorkspaceMapping,
    /// Path to the Zed DB used.
    pub db_path: PathBuf,
    /// Zed channel ("preview" or "stable").
    pub zed_channel: Option<String>,
}

/// Run the full discovery chain for a set of worktree roots.
///
/// `db_path_hint`: optional explicit DB path (used by hook which knows the DB location).
/// `channel_hint`: optional Zed channel hint ("preview" or "stable").
pub fn discover(
    roots: &[PathBuf],
    db_path_hint: Option<&Path>,
    channel_hint: Option<&str>,
) -> Result<DiscoveryResult, Error> {
    if roots.is_empty() {
        return Err(Error::NoRoots);
    }

    // Resolve DB path
    let db_path = match db_path_hint {
        Some(p) => p.to_path_buf(),
        None => workspace_db::default_db_path(channel_hint)?,
    };
    let zed_channel = channel_hint
        .map(|s| s.to_string())
        .or_else(|| channel_from_db_path(&db_path));

    // Step 0 (NEW): Check project_name in .zed/settings.json
    for root in roots {
        if let Some(raw) = crate::settings::read_project_name(root) {
            let (id_opt, name) = crate::settings::parse_project_name(&raw);
            if let Some(workspace_id) = id_opt {
                // Validate workspace_id in DB
                let valid_id = if let Ok(reader) = ZedDbReader::open(&db_path) {
                    if reader.find_by_id(workspace_id)?.is_some() {
                        workspace_id
                    } else {
                        // workspace_id stale, re-discover
                        rediscover_workspace_id(&reader, roots)?
                    }
                } else {
                    workspace_id
                };

                // Derive .code-workspace path from name
                let ws_file = crate::settings::workspace_file_for_name(root, &name);
                let ws_file_exists = ws_file.exists();

                // Update project_name if workspace_id changed
                if valid_id != workspace_id {
                    let new_pn = crate::settings::format_project_name(valid_id, &name);
                    let _ = crate::settings::find_primary_root(roots, &new_pn)
                        .map(|r| crate::settings::write_project_name(r, &new_pn))
                        .unwrap_or(Ok(()));
                }

                // Clean up legacy mapping file if it exists
                crate::mapping::cleanup_legacy_mapping(root);

                let mapping = WorkspaceMapping::new(
                    valid_id,
                    &format!("{}.code-workspace", name),
                    zed_channel.as_deref(),
                );

                if ws_file_exists {
                    tracing::info!(
                        "Step 0: Found via project_name '{}' in {}: {}",
                        raw, root.display(), ws_file.display()
                    );
                    return Ok(DiscoveryResult {
                        workspace_id: valid_id,
                        project_name: name,
                        workspace_file: ws_file,
                        mapping,
                        db_path,
                        zed_channel,
                    });
                }
                // project_name found but .code-workspace doesn't exist yet — fall through
                // (MCP will create it on sync)
                tracing::debug!(
                    "Step 0: project_name '{}' found but {} doesn't exist yet",
                    raw, ws_file.display()
                );
            }
        }
    }

    // Step 1 (legacy): Check mapping file in any root
    for root in roots {
        if let Some(mapping) = WorkspaceMapping::read(root) {
            let ws_file = mapping.resolve_workspace_file(root);
            if ws_file.exists() {
                // Validate workspace_id still exists in DB (if accessible)
                let workspace_id = if let Ok(reader) = ZedDbReader::open(&db_path) {
                    if reader.find_by_id(mapping.workspace_id)?.is_some() {
                        mapping.workspace_id
                    } else {
                        // workspace_id stale, re-discover from paths
                        rediscover_workspace_id(&reader, roots)?
                    }
                } else {
                    mapping.workspace_id // can't validate, trust the mapping
                };

                // Derive project_name from workspace file
                let name = ws_file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workspace")
                    .to_string();

                // Migrate: write project_name to settings.json + clean up mapping file
                let pn = crate::settings::format_project_name(workspace_id, &name);
                let _ = crate::settings::find_primary_root(roots, &pn)
                    .map(|r| crate::settings::write_project_name(r, &pn))
                    .unwrap_or(Ok(()));
                crate::mapping::cleanup_legacy_mapping(root);

                tracing::info!(
                    "Step 1 (legacy migration): {} → project_name '{}'",
                    WorkspaceMapping::file_path(root).display(), pn
                );

                return Ok(DiscoveryResult {
                    workspace_id,
                    project_name: name,
                    workspace_file: ws_file,
                    mapping: WorkspaceMapping {
                        workspace_id,
                        ..mapping
                    },
                    db_path,
                    zed_channel,
                });
            }
            // Mapping exists but file doesn't — fall through to scanning
            tracing::warn!(
                "mapping file at {} points to non-existent workspace file: {}",
                WorkspaceMapping::file_path(root).display(),
                ws_file.display()
            );
        }
    }

    // Step 2: Scan roots for *.code-workspace files
    let mut found_files: Vec<PathBuf> = Vec::new();
    for root in roots {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("code-workspace") {
                    found_files.push(path);
                }
            }
        }
    }

    if found_files.len() == 1 {
        let ws_file = found_files.into_iter().next().unwrap();
        let reader = ZedDbReader::open(&db_path)?;
        let workspace_id = rediscover_workspace_id(&reader, roots)?;

        let name = ws_file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace")
            .to_string();

        // Write project_name to all roots
        let pn = crate::settings::format_project_name(workspace_id, &name);
        let _ = crate::settings::find_primary_root(roots, &pn)
                    .map(|r| crate::settings::write_project_name(r, &pn))
                    .unwrap_or(Ok(()));

        // Also write legacy mapping for backward compat (will be cleaned up on next discovery)
        let mapping = WorkspaceMapping::new(
            workspace_id,
            &ws_file.file_name().unwrap_or_default().to_string_lossy(),
            zed_channel.as_deref(),
        );
        mapping.write_to_roots(roots, &ws_file)?;

        return Ok(DiscoveryResult {
            workspace_id,
            project_name: name,
            workspace_file: ws_file,
            mapping,
            db_path,
            zed_channel,
        });
    }

    if found_files.len() > 1 {
        tracing::warn!(
            "multiple .code-workspace files found in roots, cannot auto-select: {:?}",
            found_files
        );
        // Don't auto-select — require explicit mapping
    }

    // Step 3: Bootstrap — create new workspace file
    let primary_root = &roots[0];
    let name = primary_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let ws_filename = format!("{}.code-workspace", name);
    let ws_file = primary_root.join(&ws_filename);

    // Create the workspace file with all roots
    let ws = crate::workspace_file::CodeWorkspaceFile {
        folders: roots
            .iter()
            .map(|r| {
                let rel = crate::paths::relative_path(primary_root, r);
                crate::workspace_file::WorkspaceFolder {
                    path: rel.to_string_lossy().to_string(),
                    name: None,
                }
            })
            .collect(),
        extra: serde_json::Map::new(),
    };
    let json = serde_json::to_string_pretty(&ws)
        .map_err(std::io::Error::other)?;
    std::fs::write(&ws_file, json)?;

    // Get workspace_id from DB
    let reader = ZedDbReader::open(&db_path)?;
    let workspace_id = rediscover_workspace_id(&reader, roots)?;

    // Write project_name to all roots
    let pn = crate::settings::format_project_name(workspace_id, &name);
    let _ = crate::settings::find_primary_root(roots, &pn)
                    .map(|r| crate::settings::write_project_name(r, &pn))
                    .unwrap_or(Ok(()));

    // Also write legacy mapping for backward compat
    let mapping = WorkspaceMapping::new(workspace_id, &ws_filename, zed_channel.as_deref());
    mapping.write_to_roots(roots, &ws_file)?;

    tracing::info!(
        "bootstrapped workspace: project_name='{}', file={}, workspace_id={}",
        pn, ws_file.display(), workspace_id
    );

    Ok(DiscoveryResult {
        workspace_id,
        project_name: name,
        workspace_file: ws_file,
        mapping,
        db_path,
        zed_channel,
    })
}

/// Find workspace_id from DB by matching the set of roots.
fn rediscover_workspace_id(reader: &ZedDbReader, roots: &[PathBuf]) -> Result<i64, Error> {
    // Try exact path match first (Zed's own identity)
    if let Some(record) = reader.find_by_paths(roots)? {
        return Ok(record.workspace_id);
    }
    // Fallback: try matching by first root (less precise but handles path differences)
    if let Some(first) = roots.first()
        && let Some(record) = reader.find_by_folder(&first.to_string_lossy())? {
            return Ok(record.workspace_id);
        }
    // Last resort: use latest workspace
    if let Some(record) = reader.latest_workspace()? {
        tracing::warn!(
            "could not match roots to DB workspace, using latest (workspace_id={})",
            record.workspace_id
        );
        return Ok(record.workspace_id);
    }
    Err(Error::NotFound)
}

/// Migrate from v1 format (project_name hack) to v2 (mapping file).
///
/// Checks for the old `"{workspace_id}:{filename}"` pattern in `.zed/settings.json`
/// and creates a mapping file from it.
pub fn migrate_from_v1(roots: &[PathBuf], db_path: &Path) -> Option<WorkspaceMapping> {
    let zed_channel = channel_from_db_path(db_path);

    if let Some(root) = roots.iter().next() {
        let settings_path = root.join(".zed/settings.json");
        let content = std::fs::read_to_string(&settings_path).ok()?;

        // Look for "project_name": "{id}:{filename}" pattern
        let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
        let project_name = parsed.get("project_name")?.as_str()?.to_string();

        // Check if it matches the v1 pattern: "{number}:{filename}"
        let parts: Vec<&str> = project_name.splitn(2, ':').collect();
        if parts.len() != 2 {
            return None;
        }
        let workspace_id: i64 = parts[0].parse().ok()?;
        let filename = parts[1].to_string();

        // Create new mapping file
        let mapping = WorkspaceMapping::new(workspace_id, &filename, zed_channel.as_deref());
        if let Err(e) = mapping.write(root) {
            tracing::error!("failed to write mapping during v1 migration: {}", e);
            return None;
        }

        // Reset project_name to just the display name (filename without extension)
        let display_name = Path::new(&filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&filename)
            .to_string();

        // Update settings.json — preserve other fields
        if let serde_json::Value::Object(mut map) = parsed {
            map.insert(
                "project_name".to_string(),
                serde_json::Value::String(display_name.clone()),
            );
            if let Ok(new_json) = serde_json::to_string_pretty(&map)
                && let Err(e) = std::fs::write(&settings_path, new_json) {
                    tracing::error!("failed to update settings.json during v1 migration: {}", e);
                }
        }

        tracing::info!(
            "migrated v1 mapping: workspace_id={}, file={}, display_name={}",
            workspace_id,
            filename,
            display_name
        );

        return Some(mapping);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_from_mapping_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create .code-workspace file
        let ws_file = root.join("test.code-workspace");
        std::fs::write(&ws_file, r#"{"folders":[{"path":"."}]}"#).unwrap();

        // Create test DB
        let db_path = root.join("db.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE workspaces (
                workspace_id INTEGER PRIMARY KEY,
                paths TEXT,
                paths_order TEXT DEFAULT '',
                remote_connection_id INTEGER,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO workspaces (workspace_id, paths, paths_order)
            VALUES (42, '{}', '0');",
            root.to_string_lossy()
        ))
        .unwrap();
        drop(conn);

        // Create mapping file
        let mapping = WorkspaceMapping::new(42, "test.code-workspace", Some("preview"));
        mapping.write(&root).unwrap();

        // Discover — should find via mapping file
        let result = discover(&[root.clone()], Some(&db_path), None).unwrap();
        assert_eq!(result.workspace_id, 42);
        assert_eq!(result.workspace_file, ws_file);
    }

    #[test]
    fn discover_by_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create .code-workspace file (no mapping)
        let ws_file = root.join("project.code-workspace");
        std::fs::write(&ws_file, r#"{"folders":[{"path":"."}]}"#).unwrap();

        // Create test DB
        let db_path = root.join("db.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE workspaces (
                workspace_id INTEGER PRIMARY KEY,
                paths TEXT,
                paths_order TEXT DEFAULT '',
                remote_connection_id INTEGER,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO workspaces (workspace_id, paths, paths_order)
            VALUES (99, '{}', '0');",
            root.to_string_lossy()
        ))
        .unwrap();
        drop(conn);

        // Discover — should find by scanning, then create mapping
        let result = discover(&[root.clone()], Some(&db_path), None).unwrap();
        assert_eq!(result.workspace_id, 99);
        assert_eq!(result.workspace_file, ws_file);

        // Mapping file should have been created
        assert!(WorkspaceMapping::read(&root).is_some());
    }

    #[test]
    fn discover_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("my-project");
        std::fs::create_dir_all(&root).unwrap();

        // Create test DB with this root
        let db_path = dir.path().join("db.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE workspaces (
                workspace_id INTEGER PRIMARY KEY,
                paths TEXT,
                paths_order TEXT DEFAULT '',
                remote_connection_id INTEGER,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO workspaces (workspace_id, paths, paths_order)
            VALUES (7, '{}', '0');",
            root.to_string_lossy()
        ))
        .unwrap();
        drop(conn);

        // No .code-workspace file exists — should bootstrap
        let result = discover(&[root.clone()], Some(&db_path), None).unwrap();
        assert_eq!(result.workspace_id, 7);
        assert!(result.workspace_file.exists());
        assert!(result
            .workspace_file
            .to_string_lossy()
            .contains("my-project.code-workspace"));

        // Mapping file should have been created
        let mapping = WorkspaceMapping::read(&root).unwrap();
        assert_eq!(mapping.workspace_id, 7);
    }

    #[test]
    fn migrate_v1_format() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db_path = dir.path().join("db.sqlite");

        // Create .zed/settings.json with v1 format
        let zed_dir = root.join(".zed");
        std::fs::create_dir_all(&zed_dir).unwrap();
        std::fs::write(
            zed_dir.join("settings.json"),
            r#"{"project_name": "42:my-project.code-workspace", "tab_size": 4}"#,
        )
        .unwrap();

        let mapping = migrate_from_v1(&[root.clone()], &db_path).unwrap();
        assert_eq!(mapping.workspace_id, 42);
        assert_eq!(mapping.workspace_file, "my-project.code-workspace");

        // Mapping file should exist
        assert!(WorkspaceMapping::read(&root).is_some());

        // settings.json should have cleaned project_name
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(zed_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["project_name"], "my-project");
        // Other settings preserved
        assert_eq!(settings["tab_size"], 4);
    }
}
