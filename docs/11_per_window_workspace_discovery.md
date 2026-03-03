# Design: Per-Window workspace_id Discovery via sqlite3_bind_int64

> Date: 2026-03-04
> Updated: 2026-03-04 (synced to implementation)
> Scope: Hook sqlite3_bind_int64 to capture workspace_id from bound SQL parameters
> Target: Zed Preview v0.226.2, macOS aarch64

---

## Problem

When multiple Zed windows are open, the hook's `query_latest_workspace()` uses:

```sql
SELECT workspace_id, paths FROM workspaces ORDER BY timestamp DESC LIMIT 1
```

This always picks the most recently written workspace. Other windows' workspaces never get discovered or synced. For example, workspace 132 (`_topic-wechat`) always wins over workspace 115 (`zed-project-workspace`).

### Why LIMIT 1 Existed

A multi-root workspace has multiple `.zed/settings.json` files, each potentially containing a different `project_name` with a different `workspace_id`. LIMIT 1 was chosen to avoid this ambiguity. The triple-match validation (folder_name == name, workspace file exists, ID validates in DB) ensures only the primary root is authoritative.

### Root Cause

The `workspace_id` is a **bound parameter** (`?1`) in the SQL — invisible to `sqlite3_prepare_v2`. The hook only sees `INSERT INTO workspaces(workspace_id, ...) VALUES (?1, ...)` but cannot extract the actual value.

### How Zed Maps workspace_id to Windows

Each Zed window holds a `Workspace` struct with `database_id: Option<WorkspaceId>` (workspace.rs:1270). On startup, `workspace_for_roots()` matches folder paths against the DB to find the workspace_id (persistence.rs:1004). On serialization, `serialize_workspace_internal()` writes that specific `database_id` as `?1` in the SQL (persistence.rs:5951-6098).

---

## Solution: Hook sqlite3_bind_int64

### How Zed's SQL Binding Works

Zed's sqlez crate flow:
1. `Statement::prepare()` → calls `sqlite3_prepare_v2` in a loop for multi-statement SQL
2. `statement.with_bindings(&workspace_id)` → calls `sqlite3_bind_int64(stmt, 1, id)` for each sub-statement
3. `statement.exec()` → calls `sqlite3_step`

The `WorkspaceId(i64)` implements the `Bind` trait, which delegates to `bind_int64()` (persistence.rs:601-606, statement.rs:194-199).

### End-to-End Flow

```
┌─ Zed Window (workspace_id=115) ─────────────────────────────────────────┐
│                                                                          │
│  Workspace::serialize_workspace_internal()                               │
│    → persistence::DB.save_workspace(SerializedWorkspace { id: 115, .. }) │
│      → conn.exec_bound(sql!(                                            │
│            DELETE FROM pane_groups WHERE workspace_id = ?1;              │
│            DELETE FROM panes WHERE workspace_id = ?1;                    │
│        ))?(workspace.id)                                                 │
│      → conn.exec_bound(sql!(                                            │
│            INSERT INTO workspaces(workspace_id, ...) VALUES (?1, ...)   │
│            ON CONFLICT DO UPDATE SET ...                                 │
│        ))?(workspace.id, paths, ...)                                     │
└──────────────────────────────────────────────────────────────────────────┘
                            │
                ┌───────────▼───────────┐
                │  sqlite3_prepare_v2   │  ← Hook 1 (sqlite3_prepare.rs)
                │  SQL: "DELETE FROM    │
                │  workspaces WHERE     │
                │  workspace_id = ?1"   │
                └───────────┬───────────┘
                            │ is_workspace_write() → true
                            │ store *pp_stmt in PENDING_WORKSPACE_STMT
                            │
                ┌───────────▼───────────┐
                │  sqlite3_bind_int64   │  ← Hook 2 (sqlite3_bind.rs)
                │  stmt=matching, idx=1 │
                │  value=115            │
                └───────────┬───────────┘
                            │ clear PENDING_WORKSPACE_STMT
                            │ call enqueue_workspace_sync(115)
                            │
                ┌───────────▼───────────┐
                │  SYNC_QUEUE           │  (sync.rs)
                │  [115]                │  deduplicated VecDeque<i64>
                └───────────┬───────────┘
                            │ spawn drain thread (if not running)
                            │ sleep 300ms (SYNC_DELAY)
                            │
                ┌───────────▼───────────┐
                │  process_workspace_   │  (sync.rs)
                │  sync(115)            │
                │  ├ debounce check     │
                │  ├ discover_for_      │
                │  │ workspace_id(115)  │  ← discovery.rs (NO LIMIT 1)
                │  │ └ find_by_id(115)  │     direct DB lookup
                │  │ └ priority chain:  │     project_name → mapping → scan
                │  │ └ returns (path,   │
                │  │    workspace_id)   │
                │  ├ do_event_driven_   │
                │  │ sync(ws_file, 115) │  query DB → diff → update file
                │  └ check pinning      │
                └───────────────────────┘
```

### Two Code Paths

| Path | Trigger | Discovery | Sync |
|------|---------|-----------|------|
| **New** (bind hook) | `sqlite3_bind_int64` captures workspace_id | `discover_for_workspace_id(wid)` — direct `find_by_id` | Queue-based, multi-workspace |
| **Legacy** (no bind) | `sqlite3_prepare_v2` detects workspace write SQL | `discover_workspace_target()` — `LIMIT 1` | Single `SYNC_PENDING` flag |

The prepare hook checks `sqlite3_bind::is_installed()` to choose which path. If `sqlite3_bind_int64` symbol was not found at init, the legacy path is used unchanged.

### Hot Path Performance

The bind_int64 hook adds one atomic load per `sqlite3_bind_int64` call. In the common case (no pending workspace write), `PENDING_WORKSPACE_STMT` is null → single pointer comparison → return. Same overhead profile as the prepare hook's `TARGET_STATE` check.

---

## Implementation

### Files Modified

| File | Change |
|------|--------|
| `hooks/sqlite3_bind.rs` | **New** — bind_int64 hook with `PENDING_WORKSPACE_STMT` correlation |
| `hooks/sqlite3_prepare.rs` | Store `*pp_stmt` in `PENDING_WORKSPACE_STMT` on workspace write |
| `hooks/mod.rs` | Add `pub mod sqlite3_bind;` |
| `sync.rs` | Add `SYNC_QUEUE`, `enqueue_workspace_sync()`, `drain_sync_queue()`, `process_workspace_sync()` |
| `discovery.rs` | Add `discover_for_workspace_id()`, `update_workspace_target_cached()` |
| `symbols.rs` | Add `find_sqlite3_bind_int64()` |
| `lib.rs` | Find symbol, install hook, register in hook registry |

### Key Functions

- **`sqlite3_bind::bind_int64_detour(stmt, index, value)`** — calls original, checks PENDING_WORKSPACE_STMT match + index==1, enqueues workspace_id
- **`sync::enqueue_workspace_sync(workspace_id)`** — dedup push to `SYNC_QUEUE`, spawn drain thread
- **`sync::drain_sync_queue()`** — sleep SYNC_DELAY, pop+process loop with double-check on exit
- **`sync::process_workspace_sync(wid)`** — debounce → discover → sync → pin
- **`discovery::discover_for_workspace_id(wid)`** — `find_by_id(wid)` → priority chain → returns `(PathBuf, i64)`
- **`discovery::update_workspace_target_cached(ws_file, wid)`** — updates `WORKSPACE_TARGET` RwLock + picker sort target

---

## Concurrency Analysis

| Race | Mitigation |
|------|-----------|
| Non-workspace bind fires with PENDING set | Different stmt pointer → check fails |
| Two workspace writes overlap | Zed DB is single-writer, sequential per window |
| Same workspace_id enqueued twice | `queue.contains()` deduplication |
| Drain thread exits while new items added | Double-check pattern: re-check queue after `DRAIN_RUNNING.store(false)` |
| Multi-statement SQL (DELETE+INSERT) binds same ID twice | First bind captures + clears PENDING; second bind sees null → no-op |

---

## Backward Compatibility

- If `sqlite3_bind_int64` symbol not found → prepare hook falls through to `on_workspace_write_detected()` (legacy LIMIT 1 path)
- `WORKSPACE_TARGET` RwLock still updated by `update_workspace_target_cached()` for picker_sort and legacy sync consumers
- `TARGET_STATE` atomic still functional for legacy path
- All existing tests pass (35/35)

---

## Verification

1. `cargo build --release` — clean build (no new warnings)
2. `cargo test --release` — 35 tests pass
3. Restart Zed Preview with `DYLD_INSERT_LIBRARIES` pointing to new dylib
4. Open two windows (e.g., workspace 115 + workspace 132)
5. Check `~/Library/Logs/Zed/zed-prj-workspace-hook.*.log`:
   - `"Found sqlite3_bind_int64 at ..."` — symbol found
   - `"Hook installed: sqlite3_bind_int64"` — hook active
   - `"Captured workspace_id=115 from bind_int64"` — ID captured
   - `"Captured workspace_id=132 from bind_int64"` — both windows
   - `"Enqueued workspace_id=115"` + `"Enqueued workspace_id=132"`
   - `"discover_for_workspace_id(115): found via project_name ..."` — targeted discovery
   - `"Sync completed for workspace_id=115"` — both synced
6. Verify both `.code-workspace` files updated correctly
7. Use `mcp workspace_folders_list` to confirm sync state
