# Design: Per-Window workspace_id Discovery via sqlite3_bind_int64

> Date: 2026-03-03
> Scope: Hook sqlite3_bind_int64 to capture workspace_id from bound SQL parameters
> Target: Zed Preview v0.226.2, macOS aarch64

---

## Problem

When multiple Zed windows are open, the hook's `query_latest_workspace()` uses:

```sql
SELECT workspace_id, paths FROM workspaces ORDER BY timestamp DESC LIMIT 1
```

This always picks the most recently written workspace. Other windows' workspaces never get discovered or synced. For example, workspace 132 (`_topic-wechat`) always wins over workspace 115 (`zed-project-workspace`).

### Why LIMIT 1 Exists

A multi-root workspace has multiple `.zed/settings.json` files, each potentially containing a different `project_name` with a different `workspace_id`. LIMIT 1 was chosen to avoid this ambiguity. The triple-match validation (folder_name == name, workspace file exists, ID validates in DB) ensures only the primary root is authoritative.

### Root Cause

The `workspace_id` is a **bound parameter** (`?1`) in the SQL — invisible to `sqlite3_prepare_v2`. The hook only sees `INSERT INTO workspaces(workspace_id, ...) VALUES (?1, ...)` but cannot extract the actual value.

---

## Solution: Hook sqlite3_bind_int64

### Architecture

Zed's `save_workspace()` generates SQL with workspace_id as bound parameter 1:

```
sqlite3_prepare_v2(db, "INSERT INTO workspaces(workspace_id, ...) VALUES (?1, ...)", ...)
sqlite3_bind_int64(stmt, 1, 115)   // ← workspace_id captured here
sqlite3_step(stmt)
```

By hooking `sqlite3_bind_int64` in addition to `sqlite3_prepare_v2`, we capture the actual workspace_id each window writes.

### Data Flow

```
sqlite3_prepare_v2("DELETE FROM workspaces WHERE workspace_id = ?1; ...")
  → prepare_v2_detour: detect workspace write, store stmt in PENDING_WORKSPACE_STMT

sqlite3_bind_int64(stmt, 1, 115)
  → bind_int64_detour: stmt matches PENDING, index==1
  → enqueue_workspace_sync(115)

drain thread (after 300ms):
  → process_workspace_sync(115)
  → discover_for_workspace_id(115)   // direct DB lookup, NO LIMIT 1
  → do_event_driven_sync(ws_file, 115)
```

### Hot Path Performance

The bind_int64 hook adds one atomic load per `sqlite3_bind_int64` call. In the common case (no pending workspace write), the check exits after a single null pointer comparison — same overhead profile as the existing prepare hook's `TARGET_STATE` check.

---

## Changes

### 1. New hook: `sqlite3_bind.rs`

- `ORIG_BIND: AtomicPtr<c_void>` — original function pointer
- `PENDING_WORKSPACE_STMT: AtomicPtr<c_void>` — set by prepare hook
- `bind_int64_detour()`: call original first, then check if stmt matches and index==1
- `enqueue_workspace_sync(workspace_id)` called on match

### 2. Modified: `sqlite3_prepare.rs`

When `is_workspace_write(sql)` is true:
- Store the prepared stmt pointer in `PENDING_WORKSPACE_STMT`
- If bind hook is installed, skip `on_workspace_write_detected` (sync driven by bind hook)
- If not installed, fall through to existing LIMIT 1 path (backward compat)

### 3. Queue-based sync: `sync.rs`

Replace single `SYNC_PENDING: AtomicBool` with `SYNC_QUEUE: Mutex<VecDeque<i64>>`:
- `enqueue_workspace_sync(workspace_id)`: push (deduplicate), spawn drain thread
- `drain_sync_queue()`: process each workspace_id sequentially
- `process_workspace_sync(wid)`: per-workspace debounce → discover → sync → pin

### 4. Targeted discovery: `discovery.rs`

New `discover_for_workspace_id(workspace_id)`:
- `find_by_id(workspace_id)` — direct DB lookup (no LIMIT 1)
- Get folder roots → run priority chain (project_name → mapping → scan → bootstrap)
- Returns `Option<(PathBuf, i64)>`

---

## Concurrency Analysis

| Race | Mitigation |
|------|-----------|
| Non-workspace bind fires with PENDING set | Different stmt pointer → check fails |
| Two workspace writes overlap | Zed DB is single-writer, sequential |
| Same workspace_id enqueued twice | Queue deduplicates via `contains()` |
| Drain thread exits while new items added | Double-check pattern after setting DRAIN_RUNNING=false |

---

## Backward Compatibility

- If `sqlite3_bind_int64` symbol not found → falls back to existing LIMIT 1 discovery
- `on_workspace_write_detected()` remains as legacy path
- `WORKSPACE_TARGET` updated for picker_sort compatibility

---

## Verification

1. `cargo build --release`
2. Restart Zed with two windows open
3. Check log for both workspace_ids captured and synced
4. Verify both `.code-workspace` files updated correctly
