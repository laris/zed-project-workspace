# Architecture: zed-project-workspace (v3)

Updated: 2026-02-28 — reflects hook socket API, channel-aware CLI, stale process cleanup.

> Event-driven bidirectional sync between `.code-workspace` files and Zed's internal sqlite3 database.

## Design Principle: Event-Driven, Not Polling

| Direction | Event Source | Mechanism |
|---|---|---|
| **Zed → File** | `sqlite3_prepare_v2` hook | AtomicPtr detour (lock-free hot path), per-workspace_id debounce |
| **File → Zed** | MCP tool call → hook socket | 9 tools → Unix socket inside Zed → `current_exe()` CLI (channel-correct) |

No polling loops. Every sync action is triggered by a real event.

## System Overview

```
┌─────────────────────────────────────────────────────────┐
│                    Zed Preview (1 process, N windows)    │
│                                                          │
│  ┌──────────────┐  ┌──────────────┐                     │
│  │  Window 1     │  │  Window 2     │                    │
│  │  workspace 110│  │  workspace 94 │                    │
│  └──────┬───────┘  └──────┬───────┘                     │
│         │                  │                             │
│  ┌──────▼──────────────────▼──────┐                     │
│  │  sqlite3_prepare_v2 detour     │ ← cdylib hook       │
│  │  (AtomicPtr, lock-free)        │   (1 instance)      │
│  └──────┬─────────────────────────┘                     │
│         │ workspace write detected                      │
│  ┌──────▼─────────────────────────┐                     │
│  │  per-workspace_id debounce     │                     │
│  │  → read DB (ZedDbReader)       │                     │
│  │  → flock + atomic_write        │                     │
│  │  → update .code-workspace      │                     │
│  │  → update mapping last_sync_ts │                     │
│  └────────────────────────────────┘                     │
│                                                          │
│  ┌──────────────────────────────────┐                   │
│  │  Socket Server (hook thread)     │ ← NEW             │
│  │  /tmp/zed-prj-workspace-         │                   │
│  │    preview-{pid}.sock            │                   │
│  │  JSON-line protocol:             │                   │
│  │    ping, add_folders,            │                   │
│  │    reuse_folders, status         │                   │
│  │  Executes via current_exe()      │                   │
│  │  (inherently channel-correct)    │                   │
│  └──────────────────────────────────┘                   │
│                                                          │
│  ┌──────────────────────────────────┐                   │
│  │  SQLite DB (WAL mode)            │                   │
│  │  ~/Library/.../Zed/db/0-preview/ │                   │
│  └──────────────────────────────────┘                   │
└─────────────────────────────────────────────────────────┘
          ▲ Unix socket
          │ (hook-first, CLI fallback)
   ┌──────┴───────────────────┐    ┌──────────────────────┐
   │  MCP: zed-prj-workspace- │    │  MCP: zed-prj-       │
   │  mcp (Window 1)          │    │  workspace-mcp       │
   │  9 tools, stdio          │    │  (Window 2)          │
   │  Parent PID watchdog     │    │  9 tools, stdio      │
   │  Stale cleanup on start  │    │  Parent PID watchdog │
   └──────────┬───────────────┘    └──────────┬───────────┘
              │                               │
              ▼                               ▼
   ┌──────────────────────────────────────────────┐
   │  .code-workspace files (JSON)                │
   │  .zed/zed-project-workspace.json (mapping)   │
   │  Protected by flock advisory lock             │
   └──────────────────────────────────────────────┘
```

## Crate Layout

```
zed-project-workspace/           (Cargo workspace root)
├── src/                         zed-prj-workspace (shared library)
│   ├── paths.rs                 Path normalization, relative paths, parse_workspace_paths
│   ├── mapping.rs               .zed/zed-project-workspace.json, channel detection, socket discovery
│   ├── hook_client.rs           HookClient: Unix socket client, invoke_zed_add/reuse with fallback
│   ├── lock.rs                  flock advisory locking, atomic_write
│   ├── discovery.rs             Mapping-first discovery chain, v1 migration
│   ├── workspace_db.rs          ZedDbReader: find_by_id, find_by_paths, ordered_paths
│   ├── workspace_file.rs        .code-workspace parse/write/diff
│   └── sync_engine.rs           Sync actions, execute_sync (delegates to hook_client)
│
├── zed-prj-workspace-hook/      cdylib injected into Zed
│   └── src/
│       ├── hooks/sqlite3_prepare.rs   AtomicPtr detour, is_workspace_write
│       ├── socket_server.rs           Unix socket server (ping, add_folders, reuse_folders)
│       ├── ffi/dispatch.rs            macOS dispatch_async_f bindings
│       ├── sync.rs                    Per-workspace_id debounce, shared lib usage
│       ├── discovery.rs               Hook-specific discovery (wraps shared)
│       ├── config.rs                  Env vars, delays, cooldowns
│       ├── logging.rs                 File-based tracing
│       └── symbols.rs                 Frida-Gum symbol lookup
│
├── zed-prj-workspace-mcp/       MCP server binary: zed-prj-workspace-mcp (9 tools)
│   └── src/main.rs              rmcp, hook-first routing, parent PID watchdog, stale cleanup
│
└── xtask/                       ~50 lines, uses dylib-kit SDK
        └── src/main.rs              HookProject + TargetApp config → dylib_patcher::cli::run()

External dependency:
~/codes/dylib-kit/               Standalone SDK (shared with zed-yolo-hook)
├── crates/dylib-hook-registry/  Multi-hook coordination registry
└── crates/dylib-patcher/        Build, inject, sign, verify, restore, auto-restart
```

## Deployment (cargo patch)

```
cargo patch                   # build + quit Zed + patch + relaunch
cargo patch --no-build        # skip build, use existing dylib
cargo patch --verify          # + wait 15s + check hook health markers
cargo patch verify            # verify only (Zed must be running)
cargo patch status            # show registry, artifact hashes, stale check
cargo patch remove            # remove this hook, keep others, relaunch
cargo patch restore           # restore original binary, relaunch
```

Smart detection: if running from Zed's terminal, spawns a detached process
that survives the app quit, patches, and relaunches automatically.

## Key Design Decisions

### 1. Mapping File Over project_name Hack

**Old:** `"project_name": "110:my-project.code-workspace"` in `.zed/settings.json` — polluted Zed UI.

**New:** `.zed/zed-project-workspace.json` — separate file, relative paths, machine-local workspace_id.

### 2. Lock-Free Hot Path

**Old:** `Mutex<Option<PrepareV2Fn>>` — acquired on every SQL call Zed makes.

**New:** `AtomicPtr<c_void>` — set once at install, lock-free reads forever. Plus `AtomicU8` state check for fast skip.

### 3. Order-Aware Sync

**Old:** Set-based diff only (membership). Reordering not detected.

**New:** Reads `paths_order` from DB, reconstructs user-visible order, detects reorder as distinct `SyncAction::ReorderFile`.

**`paths_order` semantics:** `order[lex_index] = user_position` (not the inverse). Zed's `PathList::ordered_paths()` zips order values with lex-sorted paths and sorts by the order value. See `crates/util/src/path_list.rs`.

### 4. Mapping-First Discovery

**Old:** Scan roots first, then check `project_name`.

**New:** Check `.zed/zed-project-workspace.json` first (authoritative), scan only for first-time setup, bootstrap as last resort.

### 5. Concurrency Safety

**Old:** Global sync — one debounce timer for all workspaces.

**New:** Per-workspace_id debounce map. File locking (`flock`) for concurrent hook + MCP writes. Atomic file writes (tmp + rename).

### 6. Hook Socket API (Option C) — Bypass CLI

**Old:** MCP calls `Command::new("zed")` — always invokes stable CLI, wrong channel for Preview.

**New:** Hook exposes a Unix socket at `/tmp/zed-prj-workspace-{channel}-{pid}.sock`. MCP connects via `HookClient`, sends JSON commands. Hook executes via `current_exe()` (inherently channel-correct). Falls back to channel-aware CLI (`zed-preview` or `zed`) if socket unavailable.

```
Fallback chain:
  1. Hook socket (in-process, channel-correct)
  2. Channel-aware CLI (zed-preview or zed)
  3. Generic "zed" CLI (last resort)
```

### 7. MCP Process Lifecycle

**Old:** No cleanup — stale MCP processes accumulated after Zed crashes/restarts.

**New:**
- **Parent PID watchdog**: checks `getppid()` every 5s, exits if parent dies (reparented to init/launchd)
- **Stale cleanup on startup**: kills orphaned `zed-prj-workspace-mcp` / `zed-workspace-sync` processes whose parent is not a Zed process
- **Binary renamed**: `zed-prj-workspace-mcp` (aligned with crate name)

## Discovery Flow (v2)

```
1. Check .zed/zed-project-workspace.json (any root)
   → mapping exists AND workspace_file resolves?
   → YES: validate workspace_id in DB → USE
   → NO: fall through

2. Scan roots for *.code-workspace
   → exactly 1 found?
   → YES: create mapping → USE
   → NO: fall through (multiple = warn, 0 = bootstrap)

3. Bootstrap
   → Create {root_name}.code-workspace with all roots
   → Create mapping in all roots
   → Set project_name (display only, if not already set)
```

## Sync Flow

### Zed → File

```
sqlite3_prepare_v2 detour fires
  → AtomicU8 TARGET_STATE check (skip if NONE)
  → is_workspace_write() → INSERT/UPDATE/DELETE on workspaces
  → on_workspace_write_detected(sql, state)
    → SYNC_PENDING atomic CAS (deduplicate threads)
    → spawn thread: sleep(300ms)
    → if TARGET_PENDING: run discovery
    → if TARGET_SET: run_sync()
      → per-workspace_id cooldown check
      → ZedDbReader::find_by_id(workspace_id) → ordered_paths()
      → with_workspace_lock(file) {
          read .code-workspace
          diff membership + order
          add/remove/reorder folders
          atomic_write
          update last_sync_ts
        }
```

### File → Zed

```
MCP tool call (e.g. workspace_folders_sync / add / remove)
  → mapping → workspace_id → find_by_id()
  → with_workspace_lock(file) {
      compute_sync_actions(file_folders, db_folders, direction)
      apply file-side actions
      atomic_write
    }
  → hook_client::invoke_zed_add(path, channel, existing_root)
      1. Try hook socket: connect to /tmp/zed-prj-workspace-{channel}-*.sock
         → send {"cmd":"add_folders","paths":[existing_root, new_path]}
         → hook calls current_exe() --add (channel-correct)
      2. Fallback: zed-preview --add (or zed --add for stable)
  → hook_client::invoke_zed_reuse(paths, channel)  (reconcile mode)
      Same fallback chain as above, with --reuse
      NOTE: --reuse with already-open folders is a no-op (cannot reorder panel)
```
