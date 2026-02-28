//! Read-only access to Zed's internal sqlite3 workspace database.
//!
//! Zed stores workspace state in `~/Library/Application Support/Zed/db/{scope}/db.sqlite`
//! where `{scope}` is `0-preview` for Zed Preview or `0-stable` for Zed Stable.
//! WAL journal mode allows safe concurrent reads from external processes.
//!
//! **Important:** Zed stores paths as newline-separated plain text, NOT JSON arrays.
//! Path ordering is stored separately in `paths_order` (comma-separated permutation).

use crate::paths;
use rusqlite::{Connection, OpenFlags, params};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database not found at {0}")]
    NotFound(PathBuf),
    #[error("failed to locate Zed data directory")]
    NoDataDir,
}

/// A workspace record from Zed's database.
#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub workspace_id: i64,
    /// Paths in lexicographic order (as stored in DB `paths` column).
    pub paths: Vec<PathBuf>,
    /// Ordering permutation (from DB `paths_order` column).
    /// Each value is a lex-index; position in the vec is user-position.
    pub paths_order: Vec<usize>,
    pub timestamp: String,
    /// Workspace file path from PR #46225 column (if available).
    pub workspace_file_path: Option<String>,
}

impl WorkspaceRecord {
    /// Reconstruct paths in user-visible order (applying paths_order permutation).
    pub fn ordered_paths(&self) -> Vec<PathBuf> {
        let order_str = self
            .paths_order
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        paths::reconstruct_ordered_paths(&self.paths, &order_str)
    }
}

/// Read-only handle to Zed's workspace database.
pub struct ZedDbReader {
    conn: Connection,
}

impl ZedDbReader {
    /// Open the Zed database at a specific path (read-only, WAL-safe).
    pub fn open(db_path: &Path) -> Result<Self, Error> {
        if !db_path.exists() {
            return Err(Error::NotFound(db_path.to_path_buf()));
        }
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "busy_timeout", 500)?;
        Ok(Self { conn })
    }

    /// Open the default Zed database for the current user on macOS.
    pub fn open_default() -> Result<Self, Error> {
        let db_path = default_db_path(None)?;
        Self::open(&db_path)
    }

    /// Check if the `workspace_file_path` column exists (PR #46225).
    fn has_workspace_file_column(&self) -> bool {
        self.conn
            .prepare("SELECT workspace_file_path FROM workspaces LIMIT 0")
            .is_ok()
    }

    /// Build a WorkspaceRecord from a row, handling optional columns.
    fn record_from_row(
        &self,
        workspace_id: i64,
        paths_str: &str,
        paths_order_str: &str,
        timestamp: &str,
    ) -> WorkspaceRecord {
        let paths_order: Vec<usize> = if paths_order_str.is_empty() {
            Vec::new()
        } else {
            paths_order_str
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect()
        };

        WorkspaceRecord {
            workspace_id,
            paths: paths::parse_workspace_paths(paths_str),
            paths_order,
            timestamp: timestamp.to_string(),
            workspace_file_path: None,
        }
    }

    /// List all workspaces with non-empty paths, ordered by most recent first.
    pub fn all_workspaces(&self) -> Result<Vec<WorkspaceRecord>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, paths, COALESCE(paths_order, '') as paths_order, timestamp
             FROM workspaces
             WHERE paths IS NOT NULL AND paths != ''
             ORDER BY timestamp DESC",
        )?;

        let records = stmt
            .query_map(params![], |row| {
                let workspace_id: i64 = row.get(0)?;
                let paths_str: String = row.get(1)?;
                let paths_order_str: String = row.get(2)?;
                let timestamp: String = row.get(3)?;

                Ok((workspace_id, paths_str, paths_order_str, timestamp))
            })?
            .filter_map(|r| r.ok())
            .map(|(id, ps, po, ts)| self.record_from_row(id, &ps, &po, &ts))
            .collect();

        Ok(records)
    }

    /// Find a workspace by workspace_id.
    pub fn find_by_id(&self, workspace_id: i64) -> Result<Option<WorkspaceRecord>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, paths, COALESCE(paths_order, '') as paths_order, timestamp
             FROM workspaces
             WHERE workspace_id = ?1",
        )?;

        let result = stmt
            .query_row(params![workspace_id], |row| {
                let workspace_id: i64 = row.get(0)?;
                let paths_str: String = row.get(1)?;
                let paths_order_str: String = row.get(2)?;
                let timestamp: String = row.get(3)?;
                Ok((workspace_id, paths_str, paths_order_str, timestamp))
            })
            .ok()
            .map(|(id, ps, po, ts)| self.record_from_row(id, &ps, &po, &ts));

        // Try to fill workspace_file_path if column exists
        if let Some(mut record) = result {
            if self.has_workspace_file_column() {
                record.workspace_file_path = self
                    .conn
                    .query_row(
                        "SELECT workspace_file_path FROM workspaces WHERE workspace_id = ?1",
                        params![workspace_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten();
            }
            return Ok(Some(record));
        }

        Ok(None)
    }

    /// Find a workspace by exact canonical path set (Zed's own identity strategy).
    ///
    /// Sorts the input paths lexicographically and joins with newline to match
    /// Zed's `paths` column format. This is the same logic Zed uses in
    /// `workspace_for_roots_internal()`.
    pub fn find_by_paths(&self, folder_paths: &[PathBuf]) -> Result<Option<WorkspaceRecord>, Error> {
        let mut sorted: Vec<String> = folder_paths
            .iter()
            .map(|p| paths::normalize_path(p).to_string_lossy().to_string())
            .collect();
        sorted.sort();
        let serialized = sorted.join("\n");

        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, paths, COALESCE(paths_order, '') as paths_order, timestamp
             FROM workspaces
             WHERE paths IS ?1 AND remote_connection_id IS NULL",
        )?;

        let result = stmt
            .query_row(params![serialized], |row| {
                let workspace_id: i64 = row.get(0)?;
                let paths_str: String = row.get(1)?;
                let paths_order_str: String = row.get(2)?;
                let timestamp: String = row.get(3)?;
                Ok((workspace_id, paths_str, paths_order_str, timestamp))
            })
            .ok()
            .map(|(id, ps, po, ts)| self.record_from_row(id, &ps, &po, &ts));

        Ok(result)
    }

    /// Find a workspace whose paths contain a given folder path (LIKE substring match).
    ///
    /// **Deprecated**: Use `find_by_id()` with mapping file or `find_by_paths()` instead.
    /// This is kept for backward compatibility during migration.
    pub fn find_by_folder(&self, folder_path: &str) -> Result<Option<WorkspaceRecord>, Error> {
        let pattern = format!("%{}%", folder_path);
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, paths, COALESCE(paths_order, '') as paths_order, timestamp
             FROM workspaces
             WHERE paths LIKE ?1
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let result = stmt
            .query_row(params![pattern], |row| {
                let workspace_id: i64 = row.get(0)?;
                let paths_str: String = row.get(1)?;
                let paths_order_str: String = row.get(2)?;
                let timestamp: String = row.get(3)?;
                Ok((workspace_id, paths_str, paths_order_str, timestamp))
            })
            .ok()
            .map(|(id, ps, po, ts)| self.record_from_row(id, &ps, &po, &ts));

        Ok(result)
    }

    /// Get the most recently written workspace.
    pub fn latest_workspace(&self) -> Result<Option<WorkspaceRecord>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, paths, COALESCE(paths_order, '') as paths_order, timestamp
             FROM workspaces
             WHERE paths IS NOT NULL AND paths != ''
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let result = stmt
            .query_row(params![], |row| {
                let workspace_id: i64 = row.get(0)?;
                let paths_str: String = row.get(1)?;
                let paths_order_str: String = row.get(2)?;
                let timestamp: String = row.get(3)?;
                Ok((workspace_id, paths_str, paths_order_str, timestamp))
            })
            .ok()
            .map(|(id, ps, po, ts)| self.record_from_row(id, &ps, &po, &ts));

        Ok(result)
    }
}

/// Get the default Zed database path on macOS.
///
/// If `channel_hint` is provided (`"preview"` or `"stable"`), prefer that channel.
/// Otherwise, detect from the running executable or fall back to preference order.
pub fn default_db_path(channel_hint: Option<&str>) -> Result<PathBuf, Error> {
    let home = dirs::home_dir().ok_or(Error::NoDataDir)?;
    let db_dir = home.join("Library/Application Support/Zed/db");

    // If a specific channel is requested, try it first
    if let Some(channel) = channel_hint {
        let scope = format!("0-{}", channel);
        let candidate = db_dir.join(&scope).join("db.sqlite");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // Try to detect from executable path
    if let Some(detected) = crate::mapping::detect_zed_channel() {
        let scope = format!("0-{}", detected);
        let candidate = db_dir.join(&scope).join("db.sqlite");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // Fallback: try preview first, then stable
    for scope in &["0-preview", "0-stable"] {
        let candidate = db_dir.join(scope).join("db.sqlite");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // Last resort: any 0-* directory (excluding 0-global)
    if let Ok(entries) = std::fs::read_dir(&db_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("0-") && name_str != "0-global" {
                let candidate = entry.path().join("db.sqlite");
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(Error::NotFound(db_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_nonexistent_db_errors() {
        let result = ZedDbReader::open(Path::new("/nonexistent/path/db.sqlite"));
        assert!(matches!(result, Err(Error::NotFound(_))));
    }

    #[test]
    fn create_test_db_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (
                workspace_id INTEGER PRIMARY KEY,
                paths TEXT,
                paths_order TEXT DEFAULT '',
                remote_connection_id INTEGER,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO workspaces (workspace_id, paths, paths_order, timestamp)
            VALUES (1, '/Users/test/backend\n/Users/test/frontend', '1,0', '2026-01-01 00:00:00');
            INSERT INTO workspaces (workspace_id, paths, paths_order, timestamp)
            VALUES (2, '/Users/test/solo-project', '0', '2026-01-02 00:00:00');",
        )
        .unwrap();
        drop(conn);

        let reader = ZedDbReader::open(&db_path).unwrap();

        // all_workspaces
        let all = reader.all_workspaces().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].workspace_id, 2); // most recent first

        // find_by_id with paths_order
        // DB stores paths sorted lexicographically: backend < frontend
        let ws1 = reader.find_by_id(1).unwrap().unwrap();
        assert_eq!(ws1.paths.len(), 2);
        assert_eq!(ws1.paths[0], PathBuf::from("/Users/test/backend"));
        assert_eq!(ws1.paths[1], PathBuf::from("/Users/test/frontend"));
        assert_eq!(ws1.paths_order, vec![1, 0]);

        // ordered_paths applies the permutation: order[0]=1 → frontend, order[1]=0 → backend
        let ordered = ws1.ordered_paths();
        assert_eq!(ordered[0], PathBuf::from("/Users/test/frontend"));
        assert_eq!(ordered[1], PathBuf::from("/Users/test/backend"));

        // find_by_paths (exact match)
        let found = reader
            .find_by_paths(&[
                PathBuf::from("/Users/test/frontend"),
                PathBuf::from("/Users/test/backend"),
            ])
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().workspace_id, 1);

        // find_by_paths with different order (should still match because we sort)
        let found = reader
            .find_by_paths(&[
                PathBuf::from("/Users/test/backend"),
                PathBuf::from("/Users/test/frontend"),
            ])
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().workspace_id, 1);

        // find_by_paths no match
        let not_found = reader
            .find_by_paths(&[PathBuf::from("/Users/test/nonexistent")])
            .unwrap();
        assert!(not_found.is_none());

        // find_by_folder (deprecated, backward compat)
        let found = reader.find_by_folder("/Users/test/frontend").unwrap();
        assert!(found.is_some());

        // latest_workspace
        let latest = reader.latest_workspace().unwrap().unwrap();
        assert_eq!(latest.workspace_id, 2);
    }
}
