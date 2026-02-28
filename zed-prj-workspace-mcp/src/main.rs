//! MCP server for workspace folder sync.
//! Exposes tools to list, add, remove, sync, discover, status, open, bootstrap,
//! and reorder workspace folders between .code-workspace files and Zed's internal database.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::Parameters,
    },
    model::*,
    schemars,
    tool, tool_handler, tool_router,
    transport::stdio,
};

use std::path::{Path, PathBuf};
use zed_prj_workspace::{
    discovery, hook_client, lock, mapping, pinning, settings, sync_engine, workspace_db,
    workspace_file,
};

fn err(msg: impl std::fmt::Display) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(msg.to_string(), None)
}

fn to_json(val: &serde_json::Value) -> String {
    serde_json::to_string_pretty(val).unwrap_or_default()
}

fn ok_text(text: String) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

// --- Input schemas ---

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListFoldersInput {
    /// Path to the .code-workspace file
    workspace_file: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AddFolderInput {
    /// Path to the .code-workspace file
    workspace_file: String,
    /// Absolute path of the folder to add
    folder_path: String,
    /// Position to insert at (0-indexed). If omitted, appends to end.
    position: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RemoveFolderInput {
    /// Path to the .code-workspace file
    workspace_file: String,
    /// Absolute path of the folder to remove
    folder_path: String,
    /// If true, use `zed --reuse` to apply removal to running Zed (disruptive)
    reconcile: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SyncInput {
    /// Path to the .code-workspace file
    workspace_file: String,
    /// Sync direction: file_to_zed, zed_to_file, or bidirectional
    direction: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DiscoverInput {
    /// Path to a .code-workspace file (optional — will scan if not provided)
    workspace_file: Option<String>,
    /// Absolute path of a folder in the workspace (alternative to workspace_file)
    folder_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct StatusInput {
    /// Path to the .code-workspace file (optional — auto-discovers if omitted)
    workspace_file: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenInput {
    /// Path to the .code-workspace file
    workspace_file: String,
    /// Open mode: "new_window" (default), "add" (add to existing), "reuse" (replace existing)
    mode: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BootstrapInput {
    /// Absolute paths of folders to include in the workspace
    folder_paths: Vec<String>,
    /// Workspace name (becomes the .code-workspace filename). If omitted, uses first folder name.
    workspace_name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReorderInput {
    /// Path to the .code-workspace file
    workspace_file: String,
    /// New folder order as absolute paths (must contain all existing folders)
    order: Vec<String>,
}

// --- Server ---

#[derive(Clone)]
struct WorkspaceSyncServer {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl WorkspaceSyncServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    // ========================================================================
    // Existing tools (refactored)
    // ========================================================================

    #[tool(description = "List folders in a .code-workspace file and compare with Zed DB state. Shows membership and order sync status.")]
    fn workspace_folders_list(
        &self,
        Parameters(input): Parameters<ListFoldersInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ws_path = Path::new(&input.workspace_file);
        let ws = workspace_file::CodeWorkspaceFile::from_file(ws_path).map_err(err)?;
        let resolved = ws.resolve(ws_path).map_err(err)?;
        let file_folders: Vec<String> = resolved
            .folders
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        // Try mapping-based DB lookup first, then fallback
        let db_info = try_db_lookup(ws_path, &resolved.folders);

        let mut output = serde_json::json!({
            "workspace_file": input.workspace_file,
            "file_folders": file_folders,
        });

        if let Some((db_folders, workspace_id, order_match)) = db_info {
            let db_strs: Vec<String> = db_folders.iter().map(|p| p.to_string_lossy().to_string()).collect();
            let set_match = workspace_file::folders_match_set(&resolved.folders, &db_folders);
            output["db_folders"] = serde_json::json!(db_strs);
            output["workspace_id"] = serde_json::json!(workspace_id);
            output["in_sync"] = serde_json::json!(set_match && order_match);
            output["membership_match"] = serde_json::json!(set_match);
            output["order_match"] = serde_json::json!(order_match);
            if !set_match {
                let diff = workspace_file::diff_folders(&resolved.folders, &db_folders);
                output["added_in_zed"] = serde_json::json!(diff.added.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>());
                output["removed_in_zed"] = serde_json::json!(diff.removed.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>());
            }
        }

        ok_text(to_json(&output))
    }

    #[tool(description = "Add a folder to the .code-workspace file and invoke zed --add. Waits for result.")]
    fn workspace_folders_add(
        &self,
        Parameters(input): Parameters<AddFolderInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ws_path = Path::new(&input.workspace_file);
        let folder = Path::new(&input.folder_path);

        let result = lock::with_workspace_lock(ws_path, || -> Result<bool, String> {
            let mut ws = workspace_file::CodeWorkspaceFile::from_file(ws_path)
                .map_err(|e| e.to_string())?;

            let added = if let Some(pos) = input.position {
                // Insert at specific position
                let base_dir = ws_path.parent().ok_or("no parent dir")?;
                let resolved = ws.resolve(ws_path).map_err(|e| e.to_string())?;
                let norm = zed_prj_workspace::paths::normalize_path(folder);
                if resolved.folders.iter().any(|f| zed_prj_workspace::paths::paths_equal(f, &norm)) {
                    false
                } else {
                    let rel = zed_prj_workspace::paths::relative_path(base_dir, folder);
                    let entry = workspace_file::WorkspaceFolder {
                        path: rel.to_string_lossy().to_string(),
                        name: None,
                    };
                    let pos = pos.min(ws.folders.len());
                    ws.folders.insert(pos, entry);
                    true
                }
            } else {
                ws.add_folder(ws_path, folder).map_err(|e| e.to_string())?
            };

            if added {
                let json = ws.to_json_pretty().map_err(|e| e.to_string())?;
                lock::atomic_write(ws_path, &json).map_err(|e| e.to_string())?;
            }
            Ok(added)
        })
        .map_err(|e| err(format!("{e:?}")))?;

        let added = result;

        if added {
            // Add folder to running Zed via hook socket (or CLI fallback)
            let channel = resolve_channel(ws_path);
            let zed_status = match hook_client::invoke_zed_add(
                Path::new(&input.folder_path),
                channel.as_deref(),
                ws_path.parent(), // existing root for window targeting
            ) {
                Ok(used_hook) => {
                    if used_hook { "ok (hook)" } else { "ok (cli)" }.to_string()
                }
                Err(e) => format!("failed: {e}"),
            };

            ok_text(to_json(&serde_json::json!({
                "added": true,
                "folder": input.folder_path,
                "zed_add_result": zed_status,
            })))
        } else {
            ok_text(to_json(&serde_json::json!({
                "added": false,
                "reason": "folder already in workspace",
            })))
        }
    }

    #[tool(description = "Remove a folder from the .code-workspace file. Use reconcile=true to also apply to running Zed via --reuse.")]
    fn workspace_folders_remove(
        &self,
        Parameters(input): Parameters<RemoveFolderInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ws_path = Path::new(&input.workspace_file);
        let folder = Path::new(&input.folder_path);

        let remaining = lock::with_workspace_lock(ws_path, || -> Result<Option<Vec<PathBuf>>, String> {
            let mut ws = workspace_file::CodeWorkspaceFile::from_file(ws_path)
                .map_err(|e| e.to_string())?;
            let removed = ws.remove_folder(ws_path, folder).map_err(|e| e.to_string())?;
            if removed {
                let json = ws.to_json_pretty().map_err(|e| e.to_string())?;
                lock::atomic_write(ws_path, &json).map_err(|e| e.to_string())?;
                let resolved = ws.resolve(ws_path).map_err(|e| e.to_string())?;
                Ok(Some(resolved.folders))
            } else {
                Ok(None)
            }
        })
        .map_err(|e| err(format!("{e:?}")))?;

        match remaining {
            Some(remaining_folders) => {
                let reconcile = input.reconcile.unwrap_or(false);
                let mut output = serde_json::json!({
                    "removed": true,
                    "folder": input.folder_path,
                    "remaining_folders": remaining_folders.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
                });

                if reconcile {
                    let channel = resolve_channel(ws_path);
                    let reuse_result = hook_client::invoke_zed_reuse(&remaining_folders, channel.as_deref());
                    output["reconcile"] = serde_json::json!(match reuse_result {
                        Ok(used_hook) => if used_hook { "ok (hook)" } else { "ok (cli)" }.to_string(),
                        Err(e) => format!("failed: {e}"),
                    });
                } else {
                    output["pending_restart"] = serde_json::json!(true);
                    output["hint"] = serde_json::json!("Folder removed from file. Use reconcile=true or restart Zed to apply.");
                }

                ok_text(to_json(&output))
            }
            None => ok_text(to_json(&serde_json::json!({
                "removed": false,
                "reason": "folder not found in workspace",
            }))),
        }
    }

    #[tool(description = "Sync workspace folders between .code-workspace file and Zed DB. Directions: file_to_zed, zed_to_file, bidirectional")]
    fn workspace_folders_sync(
        &self,
        Parameters(input): Parameters<SyncInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let direction = input.direction.parse::<sync_engine::SyncDirection>().map_err(|_| {
            rmcp::ErrorData::invalid_params(
                format!("Invalid direction '{}'. Use: file_to_zed, zed_to_file, or bidirectional", input.direction),
                None,
            )
        })?;

        let ws_path = Path::new(&input.workspace_file);
        let db_path = workspace_db::default_db_path(None).map_err(err)?;

        let result = sync_engine::execute_sync(ws_path, &db_path, direction).map_err(err)?;

        // Pin target root after sync
        let pinned = try_pin_target_root(ws_path, &result.file_folders_after);

        ok_text(to_json(&serde_json::json!({
            "direction": input.direction,
            "actions_taken": result.actions_taken.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>(),
            "reordered": result.reordered,
            "pinned": pinned,
            "file_folders_after": result.file_folders_after.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "db_folders": result.db_folders.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        })))
    }

    // ========================================================================
    // New tools
    // ========================================================================

    #[tool(description = "Discover workspace mapping: find the .code-workspace file and Zed DB record for a set of roots. Returns workspace_id, file path, channel, and sync state.")]
    fn workspace_discover(
        &self,
        Parameters(input): Parameters<DiscoverInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let roots: Vec<PathBuf> = if let Some(ws_file) = &input.workspace_file {
            // Read workspace file to get roots
            let ws_path = Path::new(ws_file);
            let ws = workspace_file::CodeWorkspaceFile::from_file(ws_path).map_err(err)?;
            let resolved = ws.resolve(ws_path).map_err(err)?;
            resolved.folders
        } else if let Some(folder) = &input.folder_path {
            vec![PathBuf::from(folder)]
        } else {
            return Err(rmcp::ErrorData::invalid_params(
                "Provide either workspace_file or folder_path",
                None,
            ));
        };

        let result = discovery::discover(&roots, None, None).map_err(err)?;

        ok_text(to_json(&serde_json::json!({
            "workspace_id": result.workspace_id,
            "project_name": result.project_name,
            "workspace_file": result.workspace_file.to_string_lossy(),
            "db_path": result.db_path.to_string_lossy(),
            "zed_channel": result.zed_channel,
            "mapping": {
                "workspace_file": result.mapping.workspace_file,
                "last_sync_ts": result.mapping.last_sync_ts,
            },
            "roots": roots.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        })))
    }

    #[tool(description = "Show diagnostic status of the workspace sync system: workspace_id, DB path, channel, mapping state, sync timestamps.")]
    fn workspace_status(
        &self,
        Parameters(input): Parameters<StatusInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut output = serde_json::json!({});

        // DB info
        match workspace_db::default_db_path(None) {
            Ok(db_path) => {
                output["db_path"] = serde_json::json!(db_path.to_string_lossy());
                output["zed_channel"] = serde_json::json!(mapping::channel_from_db_path(&db_path));
                output["db_accessible"] = serde_json::json!(workspace_db::ZedDbReader::open(&db_path).is_ok());
            }
            Err(e) => {
                output["db_error"] = serde_json::json!(e.to_string());
            }
        }

        // Zed process check: try hook ping first, fall back to pgrep
        let channel = output["zed_channel"]
            .as_str()
            .unwrap_or("preview")
            .to_string();
        let (zed_running, hook_available) =
            if let Some(client) = hook_client::HookClient::connect(Some(&channel)) {
                match client.ping() {
                    Ok(resp) if resp.ok => (true, true),
                    _ => (true, false), // socket exists but ping failed
                }
            } else {
                // No hook socket — fall back to pgrep
                let running = std::process::Command::new("pgrep")
                    .arg("-x")
                    .arg("zed")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                (running, false)
            };
        output["zed_running"] = serde_json::json!(zed_running);
        output["hook_available"] = serde_json::json!(hook_available);

        // Workspace file info
        if let Some(ws_file) = &input.workspace_file {
            let ws_path = Path::new(ws_file);
            output["workspace_file"] = serde_json::json!(ws_file);
            output["workspace_file_exists"] = serde_json::json!(ws_path.exists());

            let ws_dir = ws_path.parent().unwrap_or(Path::new("/"));
            if let Some(m) = mapping::WorkspaceMapping::read(ws_dir) {
                output["mapping"] = serde_json::json!({
                    "workspace_id": m.workspace_id,
                    "workspace_file": m.workspace_file,
                    "zed_channel": m.zed_channel,
                    "last_sync_ts": m.last_sync_ts,
                });
            } else {
                output["mapping"] = serde_json::json!(null);
            }
        }

        ok_text(to_json(&output))
    }

    #[tool(description = "Open a .code-workspace file in Zed. Modes: new_window (default), add (to existing), reuse (replace existing).")]
    fn workspace_open(
        &self,
        Parameters(input): Parameters<OpenInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ws_path = Path::new(&input.workspace_file);
        let ws = workspace_file::CodeWorkspaceFile::from_file(ws_path).map_err(err)?;
        let resolved = ws.resolve(ws_path).map_err(err)?;

        if resolved.folders.is_empty() {
            return Err(rmcp::ErrorData::invalid_params("workspace file has no folders", None));
        }

        let mode = input.mode.as_deref().unwrap_or("new_window");
        let folder_strs: Vec<String> = resolved.folders.iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let channel = resolve_channel(ws_path);
        let cli_cmd = mapping::zed_cli_command(channel.as_deref());

        // For "add" and "reuse", try hook socket first
        let result = match mode {
            "add" => {
                match hook_client::invoke_zed_add(
                    &resolved.folders[0],
                    channel.as_deref(),
                    None,
                ) {
                    Ok(_) => Ok(std::process::ExitStatus::default()),
                    Err(e) => Err(std::io::Error::other(e)),
                }
            }
            "reuse" => {
                match hook_client::invoke_zed_reuse(&resolved.folders, channel.as_deref()) {
                    Ok(_) => Ok(std::process::ExitStatus::default()),
                    Err(e) => Err(std::io::Error::other(e)),
                }
            }
            _ => {
                // "new_window": always use CLI (no hook equivalent)
                let mut cmd = std::process::Command::new(cli_cmd);
                for f in &folder_strs {
                    cmd.arg(f);
                }
                cmd.output().map(|o| o.status)
            }
        };

        let result = result.map_err(|e| err(format!("failed to run zed: {e}")))?;

        ok_text(to_json(&serde_json::json!({
            "mode": mode,
            "folders_opened": folder_strs,
            "success": result.success(),
            "exit_code": result.code(),
        })))
    }

    #[tool(description = "Create a new .code-workspace file with given folders and set up mapping. Optionally sets project_name for Zed display.")]
    fn workspace_bootstrap(
        &self,
        Parameters(input): Parameters<BootstrapInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if input.folder_paths.is_empty() {
            return Err(rmcp::ErrorData::invalid_params("folder_paths cannot be empty", None));
        }

        let roots: Vec<PathBuf> = input.folder_paths.iter().map(PathBuf::from).collect();
        let primary = &roots[0];

        // Determine workspace name and file path
        let ws_name = input.workspace_name.unwrap_or_else(|| {
            primary.file_name().unwrap_or_default().to_string_lossy().to_string()
        });
        let ws_filename = format!("{}.code-workspace", ws_name);
        let ws_path = primary.join(&ws_filename);

        if ws_path.exists() {
            return Err(rmcp::ErrorData::invalid_params(
                format!("workspace file already exists: {}", ws_path.display()),
                None,
            ));
        }

        // Create workspace file
        let ws = workspace_file::CodeWorkspaceFile {
            folders: roots.iter().map(|r| {
                let rel = zed_prj_workspace::paths::relative_path(primary, r);
                workspace_file::WorkspaceFolder {
                    path: rel.to_string_lossy().to_string(),
                    name: None,
                }
            }).collect(),
            extra: serde_json::Map::new(),
        };
        let json = ws.to_json_pretty().map_err(err)?;
        std::fs::write(&ws_path, json).map_err(err)?;

        // Try to create mapping (needs DB access)
        let mapping_info = match workspace_db::default_db_path(None) {
            Ok(db_path) => {
                match discovery::discover(&roots, Some(&db_path), None) {
                    Ok(result) => Some(serde_json::json!({
                        "workspace_id": result.workspace_id,
                        "mapping_created": true,
                    })),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        };

        ok_text(to_json(&serde_json::json!({
            "workspace_file": ws_path.to_string_lossy(),
            "workspace_name": ws_name,
            "folders": input.folder_paths,
            "mapping": mapping_info,
        })))
    }

    #[tool(description = "Reorder folders in the .code-workspace file. Provide all folder paths in the desired new order.")]
    fn workspace_folders_reorder(
        &self,
        Parameters(input): Parameters<ReorderInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let ws_path = Path::new(&input.workspace_file);
        let new_order: Vec<PathBuf> = input.order.iter().map(PathBuf::from).collect();

        lock::with_workspace_lock(ws_path, || -> Result<(), String> {
            let mut ws = workspace_file::CodeWorkspaceFile::from_file(ws_path)
                .map_err(|e| e.to_string())?;
            let resolved = ws.resolve(ws_path).map_err(|e| e.to_string())?;

            // Validate: new order must contain exactly the same folders
            if !workspace_file::folders_match_set(&resolved.folders, &new_order) {
                return Err("new order must contain exactly the same folders as the current workspace".into());
            }

            ws.set_folders_from_absolute(ws_path, &new_order)
                .map_err(|e| e.to_string())?;
            let json = ws.to_json_pretty().map_err(|e| e.to_string())?;
            lock::atomic_write(ws_path, &json).map_err(|e| e.to_string())?;
            Ok(())
        })
        .map_err(|e| err(format!("{e:?}")))?;

        ok_text(to_json(&serde_json::json!({
            "reordered": true,
            "new_order": input.order,
        })))
    }
}

// --- Helpers ---

/// Try to look up DB record for a workspace file using project_name-first strategy.
/// Returns (ordered_db_folders, workspace_id, order_matches).
fn try_db_lookup(ws_path: &Path, file_folders: &[PathBuf]) -> Option<(Vec<PathBuf>, i64, bool)> {
    let db_path = workspace_db::default_db_path(None).ok()?;
    let reader = workspace_db::ZedDbReader::open(&db_path).ok()?;

    let ws_dir = ws_path.parent()?;

    // Strategy 0: project_name in settings.json
    if let Some(raw) = settings::read_project_name(ws_dir) {
        let (id_opt, _name) = settings::parse_project_name(&raw);
        if let Some(id) = id_opt {
            if let Ok(Some(record)) = reader.find_by_id(id) {
                let ordered = record.ordered_paths();
                let order_match = workspace_file::folders_match_ordered(file_folders, &ordered);
                return Some((ordered, record.workspace_id, order_match));
            }
        }
    }

    // Strategy 1: legacy mapping file
    if let Some(m) = mapping::WorkspaceMapping::read(ws_dir)
        && let Ok(Some(record)) = reader.find_by_id(m.workspace_id) {
            let ordered = record.ordered_paths();
            let order_match = workspace_file::folders_match_ordered(file_folders, &ordered);
            return Some((ordered, record.workspace_id, order_match));
        }

    // Strategy 2: exact path match
    if let Ok(Some(record)) = reader.find_by_paths(file_folders) {
        let ordered = record.ordered_paths();
        let order_match = workspace_file::folders_match_ordered(file_folders, &ordered);
        return Some((ordered, record.workspace_id, order_match));
    }

    // Strategy 3: first folder match (deprecated)
    if let Some(first) = file_folders.first()
        && let Ok(Some(record)) = reader.find_by_folder(&first.to_string_lossy()) {
            let ordered = record.ordered_paths();
            let order_match = workspace_file::folders_match_ordered(file_folders, &ordered);
            return Some((ordered, record.workspace_id, order_match));
        }

    None
}

/// Try to pin the target root at index 0 after a sync/add/reorder operation.
///
/// Reads project_name from any root, determines target root, calls reuse_folders if needed.
/// Returns true if pinning was performed.
fn try_pin_target_root(ws_path: &Path, folders: &[PathBuf]) -> bool {
    if folders.is_empty() {
        return false;
    }

    // Read project_name from any folder root
    let project_name = folders
        .iter()
        .find_map(|root| settings::read_project_name(root))
        .or_else(|| {
            ws_path
                .parent()
                .and_then(|d| settings::read_project_name(d))
        });

    let project_name = match project_name {
        Some(pn) => pn,
        None => return false,
    };

    let target = match pinning::determine_target_root(folders, &project_name) {
        Some(t) => t,
        None => return false,
    };

    let channel = resolve_channel(ws_path);
    match pinning::pin_target_root(folders, &target, channel.as_deref()) {
        Ok(true) => {
            tracing::info!("MCP: pinned target root at index 0: {}", target.display());
            true
        }
        Ok(false) => false,
        Err(e) => {
            tracing::warn!("MCP: pinning failed: {}", e);
            false
        }
    }
}

/// Resolve the Zed channel for a workspace file by reading its mapping file.
fn resolve_channel(ws_path: &Path) -> Option<String> {
    let ws_dir = ws_path.parent()?;
    if let Some(m) = mapping::WorkspaceMapping::read(ws_dir) {
        return m.zed_channel;
    }
    // Fallback: detect from DB path
    workspace_db::default_db_path(None)
        .ok()
        .and_then(|p| mapping::channel_from_db_path(&p))
}

#[tool_handler]
impl ServerHandler for WorkspaceSyncServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Manages workspace folders: list, add, remove, sync, discover, status, open, \
                 bootstrap, and reorder between .code-workspace files and Zed's internal database."
                    .into(),
            ),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // Clean up stale MCP processes from previous Zed sessions
    cleanup_stale_processes();

    // Start parent PID watchdog — exits if parent (Zed) dies
    start_parent_watchdog();

    let service = WorkspaceSyncServer::new()
        .serve(stdio())
        .await
        .inspect_err(|e| {
            tracing::error!("serving error: {:?}", e);
        })?;
    service.waiting().await?;
    Ok(())
}

/// Spawn a background task that monitors the parent process.
/// If the parent PID changes (parent died, we got reparented to init/launchd),
/// exit gracefully. Checks every 5 seconds.
fn start_parent_watchdog() {
    let initial_ppid = unsafe { libc::getppid() };
    tracing::info!("Parent PID watchdog started (ppid={})", initial_ppid);

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let current_ppid = unsafe { libc::getppid() };
            if current_ppid != initial_ppid || current_ppid <= 1 {
                tracing::info!(
                    "Parent process died (was {}, now {}), exiting",
                    initial_ppid, current_ppid
                );
                std::process::exit(0);
            }
        }
    });
}

/// Kill stale MCP processes from previous Zed sessions.
///
/// Finds processes with our binary name whose parent is NOT a Zed process.
/// Only kills processes owned by the current user.
fn cleanup_stale_processes() {
    let my_pid = std::process::id();
    let binary_names = ["zed-prj-workspace-mcp", "zed-workspace-sync"];

    for binary_name in &binary_names {
        let Ok(output) = std::process::Command::new("pgrep")
            .arg("-x")
            .arg(binary_name)
            .output()
        else {
            continue;
        };

        if !output.status.success() {
            continue;
        }

        let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .filter(|&pid| pid != my_pid)
            .collect();

        for pid in pids {
            if !is_parent_zed(pid) {
                tracing::info!("Killing stale {} process (pid={})", binary_name, pid);
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
    }
}

/// Check if a process's parent is a Zed process.
fn is_parent_zed(pid: u32) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };

    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() {
        return false;
    }

    // Parse "  99580 /Applications/Zed Preview.app/Contents/MacOS/zed"
    let parts: Vec<&str> = line.splitn(2, |c: char| c.is_whitespace()).collect();
    if parts.len() < 2 {
        return false;
    }
    let ppid_str = parts[0].trim();
    let comm = parts[1].trim();

    // Parent is launchd/init → orphaned
    if ppid_str == "1" || ppid_str == "0" {
        return false;
    }

    // Parent command contains "zed" or "Zed" → legitimate
    comm.contains("zed") || comm.contains("Zed")
}
