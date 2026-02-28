//! Event-driven sync logic: detects workspace writes and syncs to .code-workspace file.
//!
//! Refactored:
//! - Uses shared library `ZedDbReader` instead of raw rusqlite
//! - Uses shared `paths::parse_workspace_paths` (deduplicated)
//! - Per-workspace_id debounce (not global)
//! - File locking via shared `lock` module
//! - Self-write detection via mapping `last_sync_ts`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use zed_prj_workspace::{lock, mapping::WorkspaceMapping, workspace_db, workspace_file};

use crate::config::{DISCOVERY_COOLDOWN, SYNC_COOLDOWN, SYNC_DELAY};
use crate::discovery;
use crate::hooks::sqlite3_prepare::{TARGET_PENDING, TARGET_SET, TARGET_STATE};

/// Per-workspace_id debounce map: tracks last sync time per workspace.
static DEBOUNCE_MAP: Mutex<Option<HashMap<i64, Instant>>> = Mutex::new(None);

/// Whether a sync is already pending (prevents spawning duplicate sync threads).
static SYNC_PENDING: AtomicBool = AtomicBool::new(false);

/// Whether the target root needs to be re-pinned at index 0 after sync.
/// Set during sync if target root is not first; acted on after sync completes.
static NEEDS_REPIN: AtomicBool = AtomicBool::new(false);


/// Called when a workspace write query is detected.
/// Schedules a debounced sync — only one sync thread runs at a time.
pub fn on_workspace_write_detected(_sql: &str, state: u8) {
    tracing::info!("Workspace write detected (state={})", state);

    // Atomic check-and-set: only spawn one sync thread at a time
    if SYNC_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        tracing::debug!("Spawning sync thread (delay={}ms)", SYNC_DELAY.as_millis());
        std::thread::spawn(move || {
            // Wait for the transaction to commit
            std::thread::sleep(SYNC_DELAY);

            // If discovery hasn't happened yet, do it now
            if state == TARGET_PENDING {
                run_discovery();
            }

            // Now check if we have a target and should sync
            let current_state = TARGET_STATE.load(Ordering::Acquire);
            if current_state == TARGET_SET {
                run_sync();
            } else {
                tracing::debug!("Skipping sync (state={})", current_state);
            }

            SYNC_PENDING.store(false, Ordering::Release);
        });
    } else {
        tracing::debug!("Sync already pending, coalescing this event");
    }
}

/// Run workspace discovery (first-time or re-discovery).
fn run_discovery() {
    let now = Instant::now();
    let last_discovery_ms = discovery::LAST_DISCOVERY_MS.load(Ordering::Relaxed);
    if last_discovery_ms > 0 {
        let elapsed_ms = epoch_ms().saturating_sub(last_discovery_ms);
        if elapsed_ms < DISCOVERY_COOLDOWN.as_millis() as u64 {
            tracing::debug!("Discovery cooldown active, skipping");
            SYNC_PENDING.store(false, Ordering::Release);
            return;
        }
    }

    tracing::debug!("Starting workspace auto-discovery...");
    discovery::LAST_DISCOVERY_MS.store(epoch_ms(), Ordering::Relaxed);

    match discovery::discover_workspace_target() {
        Ok(Some((path, wid))) => {
            tracing::info!(
                "Auto-discovered sync target: {} (workspace_id={})",
                path.display(),
                wid
            );
            *discovery::WORKSPACE_TARGET.write().unwrap() = Some((path.clone(), wid));
            TARGET_STATE.store(TARGET_SET, Ordering::Release);

            // Install deferred picker sort hook if it was waiting for target name
            if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                crate::try_deferred_picker_install(name);
            }

        }
        Ok(None) => {
            tracing::info!(
                "No workspace file found — will retry after {}s cooldown",
                DISCOVERY_COOLDOWN.as_secs()
            );
        }
        Err(e) => {
            tracing::warn!(
                "Discovery failed: {e} — will retry after {}s cooldown",
                DISCOVERY_COOLDOWN.as_secs()
            );
        }
    }
    let _ = now; // suppress unused warning
}

/// Run the actual sync for the discovered workspace.
fn run_sync() {
    let (ws_file_path, workspace_id) = {
        let guard = discovery::WORKSPACE_TARGET.read().unwrap();
        match &*guard {
            Some((path, wid)) => (path.clone(), *wid),
            None => {
                tracing::debug!("No workspace target set, skipping sync");
                return;
            }
        }
    };

    // Per-workspace_id cooldown check
    {
        let mut map = DEBOUNCE_MAP.lock().unwrap();
        let map = map.get_or_insert_with(HashMap::new);
        if let Some(last) = map.get(&workspace_id)
            && last.elapsed() < SYNC_COOLDOWN {
                tracing::debug!(
                    "Per-workspace cooldown active for workspace_id={}, skipping",
                    workspace_id
                );
                return;
            }
    }

    tracing::debug!(
        "Sync target: {} (workspace_id={})",
        ws_file_path.display(),
        workspace_id
    );

    // If file disappeared, trigger re-discovery
    if !ws_file_path.exists() {
        tracing::warn!(
            "Workspace file disappeared: {} — triggering re-discovery",
            ws_file_path.display()
        );
        *discovery::WORKSPACE_TARGET.write().unwrap() = None;
        TARGET_STATE.store(TARGET_PENDING, Ordering::Release);
        return;
    }

    match do_event_driven_sync(&ws_file_path, workspace_id) {
        Ok(synced) => {
            if synced {
                tracing::info!("Sync completed — file was modified");
            } else {
                tracing::debug!("Sync completed — no changes needed");
            }
            // Update per-workspace cooldown
            let mut map = DEBOUNCE_MAP.lock().unwrap();
            let map = map.get_or_insert_with(HashMap::new);
            map.insert(workspace_id, Instant::now());
        }
        Err(e) => {
            tracing::warn!("Sync failed: {e}");
        }
    }

    // Check if root pinning is needed (deferred from sync to avoid recursion)
    if NEEDS_REPIN
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        run_pinning(&ws_file_path, workspace_id);
    }

    // NOTE: startup pin (reuse_folders) removed — `cli --reuse` with already-open
    // folders is a no-op in Zed; it cannot reorder panel worktrees. The panel order
    // is determined by WorktreeStore insertion order during workspace restore.
}

/// Perform the actual sync: read DB, read file, diff, update file if needed.
/// Returns Ok(true) if the file was modified.
///
/// Uses shared library `ZedDbReader` and file locking.
fn do_event_driven_sync(
    ws_file_path: &PathBuf,
    workspace_id: i64,
) -> Result<bool, Box<dyn std::error::Error>> {
    let db_path = match discovery::db_path() {
        Some(path) => path,
        None => {
            tracing::debug!("No DB path available, skipping sync");
            return Ok(false);
        }
    };

    // Step 1: Query DB for current workspace paths using shared library
    let reader = workspace_db::ZedDbReader::open(db_path)?;
    let (record, workspace_id) = match reader.find_by_id(workspace_id)? {
        Some(r) => (r, workspace_id),
        None => {
            // workspace_id is stale — Zed creates a new one when paths change.
            // Re-discover by finding the latest workspace that contains our roots.
            tracing::info!(
                "workspace_id={} not found in DB — re-discovering",
                workspace_id
            );
            match rediscover_workspace_id(&reader, ws_file_path, workspace_id)? {
                Some((r, new_id)) => (r, new_id),
                None => {
                    tracing::debug!("Re-discovery failed, triggering full re-discovery");
                    *discovery::WORKSPACE_TARGET.write().unwrap() = None;
                    TARGET_STATE.store(TARGET_PENDING, Ordering::Release);
                    return Ok(false);
                }
            }
        }
    };

    // Use ordered paths (respects paths_order column)
    let db_paths = record.ordered_paths();
    tracing::debug!(
        "DB paths ({}, ordered): {:?}",
        db_paths.len(),
        db_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
    );

    // Step 2-4: Read file, diff, update — all under file lock
    let result = lock::with_workspace_lock(ws_file_path, || -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let ws = workspace_file::CodeWorkspaceFile::from_file(ws_file_path)?;
        let resolved = ws.resolve(ws_file_path)?;
        let file_paths = resolved.folders;
        tracing::debug!(
            "File paths ({}): {:?}",
            file_paths.len(),
            file_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
        );

        // Check for membership diff
        let diff = workspace_file::diff_folders(&file_paths, &db_paths);
        let order_matches = workspace_file::folders_match_ordered(&file_paths, &db_paths);

        if diff.added.is_empty() && diff.removed.is_empty() && order_matches {
            tracing::debug!("Already in sync ({} folders)", file_paths.len());
            return Ok(false);
        }

        tracing::info!(
            "Zed event sync: +{} folders, -{} folders, reorder={}",
            diff.added.len(),
            diff.removed.len(),
            !order_matches && diff.added.is_empty() && diff.removed.is_empty()
        );

        // Update the .code-workspace file (DB is source of truth)
        let mut ws = workspace_file::CodeWorkspaceFile::from_file(ws_file_path)?;
        let mut modified = false;

        for path in &diff.added {
            if ws.add_folder(ws_file_path, path)? {
                tracing::info!("  + {}", path.display());
                modified = true;
            }
        }
        for path in &diff.removed {
            if ws.remove_folder(ws_file_path, path)? {
                tracing::info!("  - {}", path.display());
                modified = true;
            }
        }

        // Handle reorder (same membership, different order)
        if !modified && !order_matches {
            ws.set_folders_from_absolute(ws_file_path, &db_paths)?;
            modified = true;
            tracing::info!("  reordered folders to match Zed UI order");
        }

        if modified {
            let json = ws.to_json_pretty()?;
            lock::atomic_write(ws_file_path, &json)?;
            tracing::info!("Workspace file updated: {}", ws_file_path.display());

            // Update mapping last_sync_ts (legacy, kept for backward compat)
            let ws_dir = ws_file_path.parent().unwrap_or(std::path::Path::new("/"));
            if let Some(mut mapping) = WorkspaceMapping::read(ws_dir) {
                mapping.touch_sync_ts();
                let _ = mapping.write(ws_dir);
            }
        }

        // Check if target root needs pinning at index 0
        check_pinning_needed(&db_paths, ws_file_path);

        Ok(modified)
    });

    match result {
        Ok(modified) => Ok(modified),
        Err(lock::LockError::Inner(e)) => Err(e),
        Err(lock::LockError::Io(io_err, path)) => {
            Err(format!("lock IO error on {}: {}", path.display(), io_err).into())
        }
    }
}

/// Check if the target root (from project_name) needs to be pinned at index 0.
///
/// Reads project_name from any root's .zed/settings.json, determines the target root,
/// and sets NEEDS_REPIN if it's not already at index 0 in the DB order.
fn check_pinning_needed(db_paths: &[PathBuf], ws_file_path: &PathBuf) {
    let ws_dir = ws_file_path.parent().unwrap_or(std::path::Path::new("/"));

    // Read project_name from any root
    let project_name = db_paths
        .iter()
        .find_map(|root| zed_prj_workspace::settings::read_project_name(root))
        .or_else(|| zed_prj_workspace::settings::read_project_name(ws_dir));

    let project_name = match project_name {
        Some(pn) => pn,
        None => return, // No project_name → can't determine target root
    };

    if let Some(target) = zed_prj_workspace::pinning::determine_target_root(db_paths, &project_name) {
        if zed_prj_workspace::pinning::ensure_target_root_first(db_paths, &target).is_some() {
            tracing::info!(
                "Target root {} is NOT at index 0 — scheduling re-pin",
                target.display()
            );
            NEEDS_REPIN.store(true, Ordering::Release);
        }
    }
}

/// Execute root pinning: correct paths_order in DB + call reuse_folders.
///
/// Called from run_sync() AFTER the sync loop completes (deferred to avoid recursion).
fn run_pinning(ws_file_path: &PathBuf, workspace_id: i64) {
    let db_path = match discovery::db_path() {
        Some(p) => p,
        None => return,
    };

    // Read current record from DB
    let reader = match workspace_db::ZedDbReader::open(db_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Pinning: failed to open DB: {}", e);
            return;
        }
    };

    let record = match reader.find_by_id(workspace_id) {
        Ok(Some(r)) => r,
        _ => return,
    };

    let db_paths = record.ordered_paths();

    // Read project_name to determine target
    let project_name = db_paths
        .iter()
        .find_map(|root| zed_prj_workspace::settings::read_project_name(root));

    let project_name = match project_name {
        Some(pn) => pn,
        None => return,
    };

    let target = match zed_prj_workspace::pinning::determine_target_root(&db_paths, &project_name) {
        Some(t) => t,
        None => return,
    };

    // Layer 1: Correct paths_order directly in the DB
    let paths_order_str = record
        .paths_order
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");

    if let Some(new_order) =
        zed_prj_workspace::pinning::correct_paths_order(&record.paths, &paths_order_str, &target)
    {
        tracing::info!("Pinning: correcting paths_order in DB: {} → {}", paths_order_str, new_order);
        if let Err(e) = update_paths_order_in_db(db_path, workspace_id, &new_order) {
            tracing::warn!("Pinning: DB update failed: {}", e);
        }
    }

    // Layer 3: Call reuse_folders to fix in-memory state for current session
    let channel = zed_prj_workspace::mapping::detect_zed_channel();
    match zed_prj_workspace::pinning::pin_target_root(&db_paths, &target, channel.as_deref()) {
        Ok(true) => tracing::info!("Pinning: reuse_folders called — target root pinned at index 0"),
        Ok(false) => tracing::debug!("Pinning: target root already at index 0"),
        Err(e) => tracing::warn!("Pinning: reuse_folders failed: {}", e),
    }
}

/// Directly update paths_order in Zed's SQLite DB.
///
/// Opens a READ-WRITE connection (separate from Zed's own connection).
/// Safe with WAL mode — concurrent reads continue to work.
fn update_paths_order_in_db(
    db_path: &std::path::Path,
    workspace_id: i64,
    new_order: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "busy_timeout", 1000)?;

    let updated = conn.execute(
        "UPDATE workspaces SET paths_order = ?1 WHERE workspace_id = ?2",
        rusqlite::params![new_order, workspace_id],
    )?;

    if updated > 0 {
        tracing::info!(
            "DB paths_order updated for workspace_id={}: {}",
            workspace_id, new_order
        );
    } else {
        tracing::warn!(
            "DB update: no rows matched workspace_id={}",
            workspace_id
        );
    }

    Ok(())
}

/// Re-discover the workspace_id when the old one is stale.
///
/// When Zed adds/removes folders, it creates a new workspace_id (the path set is the identity).
/// We read the .code-workspace file to get the expected folders, then search the DB for the
/// most recent workspace that contains those folders. If found, update the mapping and
/// WORKSPACE_TARGET.
fn rediscover_workspace_id(
    reader: &workspace_db::ZedDbReader,
    ws_file_path: &PathBuf,
    old_workspace_id: i64,
) -> Result<Option<(workspace_db::WorkspaceRecord, i64)>, Box<dyn std::error::Error>> {
    // Read the workspace file to get expected folders
    let ws = workspace_file::CodeWorkspaceFile::from_file(ws_file_path)?;
    let resolved = ws.resolve(ws_file_path)?;
    if resolved.folders.is_empty() {
        return Ok(None);
    }

    // Strategy 1: find by exact path set match
    if let Ok(Some(record)) = reader.find_by_paths(&resolved.folders) {
        let new_id = record.workspace_id;
        tracing::info!(
            "Re-discovered workspace_id: {} → {} (by path match)",
            old_workspace_id, new_id
        );
        update_workspace_target(ws_file_path, new_id);
        return Ok(Some((record, new_id)));
    }

    // Strategy 2: find by first folder (less precise but catches partial matches)
    if let Some(first) = resolved.folders.first() {
        if let Ok(Some(record)) = reader.find_by_folder(&first.to_string_lossy()) {
            let new_id = record.workspace_id;
            tracing::info!(
                "Re-discovered workspace_id: {} → {} (by first folder match)",
                old_workspace_id, new_id
            );
            update_workspace_target(ws_file_path, new_id);
            return Ok(Some((record, new_id)));
        }
    }

    Ok(None)
}

/// Update the WORKSPACE_TARGET, project_name, and legacy mapping with a new workspace_id.
fn update_workspace_target(ws_file_path: &PathBuf, new_workspace_id: i64) {
    // Update in-memory target
    *discovery::WORKSPACE_TARGET.write().unwrap() = Some((ws_file_path.clone(), new_workspace_id));

    let ws_dir = ws_file_path.parent().unwrap_or(std::path::Path::new("/"));

    // Update project_name in settings.json (primary identity)
    if let Some(raw) = zed_prj_workspace::settings::read_project_name(ws_dir) {
        let (_old_id, name) = zed_prj_workspace::settings::parse_project_name(&raw);
        let new_pn = zed_prj_workspace::settings::format_project_name(new_workspace_id, &name);
        if let Err(e) = zed_prj_workspace::settings::write_project_name(ws_dir, &new_pn) {
            tracing::warn!("Failed to update project_name: {}", e);
        } else {
            tracing::info!("Updated project_name: {} → {}", raw, new_pn);
        }
    }

    // Also update legacy mapping file if it exists (backward compat)
    if let Some(mut mapping) = WorkspaceMapping::read(ws_dir) {
        let old_id = mapping.workspace_id;
        mapping.workspace_id = new_workspace_id;
        mapping.touch_sync_ts();
        if let Err(e) = mapping.write(ws_dir) {
            tracing::warn!("Failed to update mapping: {}", e);
        } else {
            tracing::info!(
                "Updated legacy mapping: workspace_id {} → {} in {}",
                old_id, new_workspace_id, ws_dir.display()
            );
        }
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
