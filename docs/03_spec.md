# Specification: zed-prj-workspace-mcp (v3)

Updated: 2026-02-28 — reflects hook socket API, channel-aware routing, process lifecycle.

---

## MCP Server: `zed-prj-workspace-mcp`

Binary: `zed-prj-workspace-mcp` (installed to `~/.mcp/bin/`)
Transport: stdio (standard for Zed MCP context servers)
Lifecycle: parent PID watchdog (exits when Zed dies), stale process cleanup on startup

### Tools (9 total)

#### 1. `workspace_folders_list`

List folders in a `.code-workspace` file and compare with Zed DB state. Shows membership and order sync status.

**Input:**
```json
{
  "workspace_file": "string (absolute path to .code-workspace file)"
}
```

**Output:** JSON with `file_folders`, `db_folders`, `workspace_id`, `in_sync`, `membership_match`, `order_match`, and diff details.

#### 2. `workspace_folders_add`

Add a folder to the `.code-workspace` file and add to running Zed. Uses hook socket first (channel-correct), falls back to channel-aware CLI. Uses file locking.

**Input:**
```json
{
  "workspace_file": "string",
  "folder_path": "string (absolute path)",
  "position": "number (optional, 0-indexed insert position)"
}
```

**Output:** `{ "added": bool, "zed_add_result": "ok (hook)" | "ok (cli)" | error string }`

#### 3. `workspace_folders_remove`

Remove a folder from the `.code-workspace` file. Optionally reconcile with running Zed via hook socket or `--reuse` CLI.

**Input:**
```json
{
  "workspace_file": "string",
  "folder_path": "string (absolute path)",
  "reconcile": "bool (optional, default false — if true, uses zed --reuse)"
}
```

**Output:** `{ "removed": bool, "pending_restart": bool, "reconcile": "ok" | error }`

#### 4. `workspace_folders_sync`

Sync workspace folders between `.code-workspace` file and Zed DB. Detects membership changes AND reordering.

**Input:**
```json
{
  "workspace_file": "string",
  "direction": "file_to_zed | zed_to_file | bidirectional"
}
```

**Output:** `{ "actions_taken": [...], "reordered": bool, "file_folders_after": [...], "db_folders": [...] }`

#### 5. `workspace_discover` (NEW)

Run the full discovery chain: find the `.code-workspace` file and Zed DB mapping for a set of roots.

**Input:**
```json
{
  "workspace_file": "string (optional)",
  "folder_path": "string (optional — alternative to workspace_file)"
}
```

**Output:** `{ "workspace_id": number, "workspace_file": "path", "db_path": "path", "zed_channel": "preview"|"stable", "mapping": {...} }`

#### 6. `workspace_status` (NEW)

Diagnostic tool: shows DB path, channel, mapping state, whether Zed is running.

**Input:**
```json
{
  "workspace_file": "string (optional)"
}
```

**Output:** `{ "db_path", "zed_channel", "db_accessible", "zed_running", "hook_available", "mapping": {...} | null }`

#### 7. `workspace_open` (NEW)

Open a `.code-workspace` file in Zed. Resolves folders and routes via hook socket (add/reuse modes) or channel-aware CLI (new_window mode).

**Input:**
```json
{
  "workspace_file": "string",
  "mode": "new_window | add | reuse (optional, default new_window)"
}
```

**Output:** `{ "mode", "folders_opened": [...], "success": bool }`

#### 8. `workspace_bootstrap` (NEW)

Create a new `.code-workspace` file with given folders and set up mapping files.

**Input:**
```json
{
  "folder_paths": ["string", ...],
  "workspace_name": "string (optional)"
}
```

**Output:** `{ "workspace_file": "path", "workspace_name": "string", "mapping": {...} | null }`

#### 9. `workspace_folders_reorder` (NEW)

Reorder folders in the `.code-workspace` file. Must provide all existing folders in the new order.

**Input:**
```json
{
  "workspace_file": "string",
  "order": ["string (abs path)", ...]
}
```

**Output:** `{ "reordered": true, "new_order": [...] }`

---

## Workspace Mapping File: `.zed/zed-project-workspace.json`

**Location:** `{worktree_root}/.zed/zed-project-workspace.json`

**Schema:**
```json
{
  "workspace_id": 110,
  "workspace_file": "my-project.code-workspace",
  "zed_channel": "preview",
  "last_sync_ts": "2026-02-28T01:00:00Z"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `workspace_id` | i64 | Machine-local integer from Zed's SQLite DB |
| `workspace_file` | string | **Relative path** from worktree root to `.code-workspace` file |
| `zed_channel` | string? | `"preview"` or `"stable"` — which Zed DB this ID belongs to |
| `last_sync_ts` | string? | ISO 8601 timestamp of last successful sync |

**Key properties:**
- Relative paths only — survives folder moves
- Machine-specific (`workspace_id` is per-machine) — `.gitignore`'d
- Written to ALL worktree roots for multi-root workspaces

---

## Discovery Protocol (v2)

Priority order (mapping-first):

```
1. Check .zed/zed-project-workspace.json
   → If mapping exists AND workspace_file resolves → USE IT
   → If workspace_id stale → re-discover from paths

2. Scan roots for *.code-workspace
   → If exactly one found → auto-create mapping → USE IT
   → If multiple found → log warning, don't auto-select

3. Bootstrap
   → Create {primary_root_name}.code-workspace
   → Create mapping files in all roots
```

**v1 Migration:** Detects old `"project_name": "{id}:{filename}"` in `.zed/settings.json`, creates mapping file, resets `project_name` to just the display name.

---

## Zed Database Schema

**Table: `workspaces`**

| Column | Type | Description |
|--------|------|-------------|
| workspace_id | INTEGER PK | Auto-increment ID |
| paths | TEXT | Newline-separated absolute paths in **lexicographic** order |
| paths_order | TEXT | Comma-separated permutation for user-visible order |
| remote_connection_id | INTEGER? | NULL for local workspaces |
| timestamp | DATETIME | Last update time |
| workspace_file_path | TEXT? | `.code-workspace` path (PR #46225, if merged) |
| workspace_file_kind | TEXT? | `"code-workspace"` (PR #46225, if merged) |

**Identity:** `(paths, remote_connection_id)` — unique index. `workspace_id` is an internal handle.

**DB Location:**
- Zed Preview: `~/Library/Application Support/Zed/db/0-preview/db.sqlite`
- Zed Stable: `~/Library/Application Support/Zed/db/0-stable/db.sqlite`

---

## Sync Protocol

### Zed → File (Hook-Driven)

```
sqlite3_prepare_v2 hook fires
  → AtomicPtr function pointer (lock-free hot path)
  → AtomicU8 state check (TARGET_NONE = skip)
  → is_workspace_write() matches INSERT/UPDATE/DELETE on workspaces
  → per-workspace_id debounce (300ms)
  → Read DB via ZedDbReader (paths + paths_order → ordered_paths)
  → with_workspace_lock(file) {
      Read .code-workspace
      Diff (membership + order)
      atomic_write if changed
      Update mapping.last_sync_ts
    }
```

### File → Zed (MCP Tool via Hook Socket)

```
Tool call (workspace_folders_sync / add / remove)
  → Read mapping → workspace_id → find_by_id()
  → Resolve channel from mapping (preview/stable)
  → with_workspace_lock(file) {
      Read file, read DB
      Compute diff
      Apply file changes (add/remove/reorder)
      atomic_write
    }
  → hook_client::invoke_zed_add(path, channel, existing_root)
      1. Try hook socket: /tmp/zed-prj-workspace-{channel}-*.sock
         → {"cmd":"add_folders","paths":[existing_root, new_path]}
         → Hook executes via current_exe() (channel-correct)
      2. Fallback: zed-preview --add (or zed --add)
  → hook_client::invoke_zed_reuse(paths, channel)
      Same fallback chain, with --reuse flag
```

### Hook Socket Protocol

Socket path: `/tmp/zed-prj-workspace-{channel}-{pid}.sock`
Transport: Unix stream socket, JSON lines, one request per connection.

| Command | Request | Response |
|---------|---------|----------|
| `ping` | `{"cmd":"ping"}` | `{"ok":true,"pid":N,"channel":"preview","version":"0.1.0"}` |
| `add_folders` | `{"cmd":"add_folders","paths":["/root","/new"]}` | `{"ok":true}` or `{"ok":false,"error":"..."}` |
| `reuse_folders` | `{"cmd":"reuse_folders","paths":["/a","/b"]}` | `{"ok":true}` or `{"ok":false,"error":"..."}` |
| `status` | `{"cmd":"status"}` | `{"ok":true,"pid":N,"channel":"preview","version":"0.1.0"}` |

### Concurrency Safety

| Mechanism | Purpose |
|-----------|---------|
| `AtomicPtr<PrepareV2Fn>` | Lock-free original function pointer (hot path) |
| `AtomicU8 TARGET_STATE` | Fast-path skip when no target |
| Per-workspace_id debounce map | Isolates cooldowns between workspaces |
| `flock` advisory lock | Prevents concurrent file writes (hook + MCP) |
| `atomic_write` (tmp + rename) | Prevents partial writes visible to readers |
| `last_sync_ts` in mapping | Self-write detection, conflict detection |
| `AtomicBool SYNC_PENDING` | Prevents duplicate sync threads |

---

## Shared Library Modules (`zed-prj-workspace`)

| Module | Purpose |
|--------|---------|
| `paths.rs` | normalize_path, relative_path, paths_equal, parse_workspace_paths, reconstruct_ordered_paths |
| `mapping.rs` | WorkspaceMapping read/write, channel detection, zed_cli_command, find_hook_socket |
| `hook_client.rs` | HookClient (Unix socket), invoke_zed_add/reuse with hook-first + CLI fallback |
| `lock.rs` | flock advisory locking, atomic_write, with_workspace_lock |
| `discovery.rs` | Mapping-first discovery chain, scan, bootstrap, v1 migration |
| `workspace_db.rs` | ZedDbReader: find_by_id, find_by_paths, ordered_paths, channel-aware default_db_path |
| `workspace_file.rs` | CodeWorkspaceFile parse/write/diff, folders_match_ordered/set |
| `sync_engine.rs` | compute_sync_actions, execute_sync (delegates to hook_client for CLI) |
