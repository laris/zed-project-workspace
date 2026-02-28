# Deep Research: Syncing .code-workspace with Zed's Internal State via macOS Hooking

> Date: 2026-02-25 (Updated: 2026-02-25 v2)
> Scope: macOS Zed Preview (arm64), no source modification/rebuild
> Assumption: **SIP disabled accepted, ad-hoc re-signing of Zed Preview per version accepted**
> Goal: Event-driven bidirectional sync of .code-workspace file <-> Zed sqlite3 DB workspace/project folder list

---

## Table of Contents

1. [Current .code-workspace Support in Zed (PR #46225)](#1-current-code-workspace-support-in-zed)
2. [Zed Internal Architecture: Worktree & Persistence](#2-zed-internal-architecture)
3. [Zed MCP/LSP/Extension External Interfaces](#3-zed-external-interfaces)
4. [macOS Hooking Approaches](#4-macos-hooking-approaches)
5. [macOS Non-Hooking Monitoring Approaches](#5-macos-non-hooking-monitoring)
6. [Feasibility Analysis & Recommendation](#6-feasibility-analysis)
7. [Proposed Architecture](#7-proposed-architecture)
8. [**RECOMMENDED: Best Solution with SIP Disabled + Re-sign**](#8-recommended-solution)
9. [SIP Disable Guide & Security Considerations](#9-sip-disable-guide)

---

## 1. Current .code-workspace Support in Zed

### Commit: 8cabb7f661 (preview-with-code-workspace)

The commit adds `.code-workspace` file support across 8 files with 621 insertions:

| File | Role |
|------|------|
| `workspace_file.rs` (new, 258 lines) | Parses `.code-workspace` JSON, resolves relative/absolute folder paths |
| `open_listener.rs` (+175 lines) | `derive_paths_with_position()` detects workspace files, parses folders, opens as worktrees |
| `workspace.rs` (+34 lines) | Added `workspace_file_source` field to `Workspace` struct |
| `persistence.rs` (+76 lines) | Added `workspace_file_path` and `workspace_file_kind` columns to sqlite |
| `model.rs` (+42 lines) | Added `SerializedWorkspaceLocation::LocalFromFile` variant |
| `recent_projects.rs` (+68 lines) | Shows workspace file name in recent projects |
| `welcome.rs` (+10 lines) | Workspace file support in welcome screen |
| `main.rs` (+14 lines) | Entry point changes |

### Current Flow (One-Way, Read-Only at Open Time)

```
CLI: zed my-project.code-workspace
    |
    v
open_listener.rs: derive_paths_with_position()
    |-- Detects .code-workspace extension
    |-- Reads and parses JSON (WorkspaceFileSource::parse())
    |-- Resolves folder paths (relative -> absolute)
    |-- Returns (Vec<PathWithPosition>, Some(WorkspaceFileSource))
    |
    v
workspace.rs: open_paths() -> find_or_create_worktree() for each folder
    |
    v
persistence.rs: Saves to sqlite with workspace_file_path + workspace_file_kind columns
```

### Key Limitation: No Bidirectional Sync

- The `.code-workspace` file is only read at open time
- No file watcher on the workspace file
- Adding/removing folders via UI does NOT update the `.code-workspace` file
- The sqlite DB and `.code-workspace` file immediately diverge after any folder change

---

## 2. Zed Internal Architecture

### 2.1 Worktree Event Flow

**WorktreeStoreEvent** (`worktree_store.rs:82-91`):
```rust
pub enum WorktreeStoreEvent {
    WorktreeAdded(Entity<Worktree>),
    WorktreeRemoved(EntityId, WorktreeId),
    WorktreeReleased(EntityId, WorktreeId),
    WorktreeOrderChanged,
    WorktreeUpdateSent(Entity<Worktree>),
    WorktreeUpdatedEntries(WorktreeId, UpdatedEntriesSet),
    WorktreeUpdatedGitRepositories(WorktreeId, UpdatedGitRepositoriesSet),
    WorktreeDeletedEntry(WorktreeId, ProjectEntryId),
}
```

**Add Folder Flow**:
1. User: "Add Folder to Project" action
2. `workspace.rs:3191` -> `add_folder_to_project()` -> prompts for directory
3. `workspace.rs:3220` -> `open_paths()` with selected paths
4. `project.rs:4472` -> `find_or_create_worktree()` -> `worktree_store.rs:656` -> `create_local_worktree()`
5. `worktree_store.rs:693` -> `add()` emits `WorktreeStoreEvent::WorktreeAdded`
6. `workspace.rs:1343` handles `project::Event::WorktreeAdded(id)` -> calls `serialize_workspace()`

**Remove Folder Flow**:
1. `worktree_store.rs:755` -> `remove_worktree()` emits `WorktreeStoreEvent::WorktreeRemoved`
2. `workspace.rs:1343` handles `project::Event::WorktreeRemoved(id)` -> calls `serialize_workspace()`

### 2.2 Persistence / SQLite

**Database Location** (`paths.rs:102-107`, `db.rs:38`):
```
macOS:     ~/Library/Application Support/Zed/db/0-<scope>/db.sqlite
Preview:   ~/Library/Application Support/Zed/db/0-<scope>/db.sqlite
           (same base dir, RELEASE_CHANNEL_NAME differentiates the socket, not the DB dir)
```

The socket for IPC is: `~/Library/Application Support/Zed/zed-preview.sock`

**DB Configuration** (`db.rs:25-34`):
```sql
PRAGMA journal_mode=WAL;      -- Write-Ahead Logging (concurrent reads)
PRAGMA busy_timeout=500;       -- 500ms busy retry
PRAGMA synchronous=NORMAL;
PRAGMA case_sensitive_like=TRUE;
```

**Serialization Trigger** (`workspace.rs:5890`):
- `serialize_workspace()` is called on `WorktreeAdded`, `WorktreeRemoved`, `WorktreeUpdatedEntries`, pane changes, etc.
- **Throttled**: `SERIALIZATION_THROTTLE_TIME = 200ms` debounce
- Writes to sqlite via `WORKSPACE_DB.save_workspace()`

**Workspace Table Schema** (from persistence.rs migrations):
- `workspace_id` (primary key)
- `paths` (serialized path list)
- `paths_order` (ordering)
- `remote_connection_id` (nullable)
- `workspace_file_path` TEXT (nullable) -- added by PR #46225
- `workspace_file_kind` TEXT (nullable) -- added by PR #46225
- Various dock/pane state columns
- `session_id`, `window_id`, `timestamp`

### 2.3 IPC Mechanism

**CLI -> Zed Communication** (`open_listener.rs:279-301`):
- Unix datagram socket at `~/Library/Application Support/Zed/zed-preview.sock`
- Simple URL-based protocol: sends URL strings (max 1024 bytes per datagram)
- For more complex operations: `ipc-channel` crate for CLI handshake
- `CliRequest::Open { paths, urls, open_new_workspace, ... }` via IPC

**Key Insight**: The socket only accepts URL strings, not structured commands. The IPC channel is created per-CLI-invocation, not persistent.

---

## 3. Zed External Interfaces (No Hooking)

### 3.1 MCP (Context Server) -- NOT Viable for Workspace Mutation

| Capability | Status |
|---|---|
| `tools/call` | MCP server -> Zed agent only (tools invoked BY AI, not FROM external) |
| `roots/list` | Types exist but `roots: None` in ClientCapabilities (`protocol.rs:43`) |
| `roots/list_changed` notification | Type exists, never sent |

MCP tools run inside Zed's AI agent context. There is no mechanism for an MCP server to push workspace mutations TO Zed.

### 3.2 LSP -- Outbound Only

`lsp.rs:1580-1679` has `add_workspace_folder()`, `remove_workspace_folder()`, `set_workspace_folders()` but these are Zed -> LSP server notifications (DidChangeWorkspaceFolders). No LSP mechanism for server -> client workspace modification.

### 3.3 Extension API -- No Workspace Mutation

The extension API (`extension_api.rs`) exposes language server lifecycle, slash commands, and context servers, but **no workspace/worktree mutation functions**.

### 3.4 CLI -- Best Existing External Path

```bash
zed --add /path/to/folder    # adds to existing workspace
zed /path/to/file.code-workspace  # opens workspace file
```

**Limitations**:
- No `--remove-folder` CLI flag
- `--add` opens files/folders but doesn't sync back to `.code-workspace`
- No structured API, just path-based

---

## 4. macOS Hooking Approaches

### 4.1 DYLD_INSERT_LIBRARIES

**How it works**: macOS dynamic linker loads specified dylibs before the app's own libraries, allowing function interposition.

**Blockers for Zed**:
- Zed Preview is a **signed, hardened-runtime, notarized** app from the App Store / Homebrew cask
- SIP (System Integrity Protection) blocks `DYLD_INSERT_LIBRARIES` for hardened-runtime binaries
- Even with SIP disabled, `CS_REQUIRE_LV` (Library Validation) blocks dylibs not signed with the same key
- On arm64 (Apple Silicon), arm64e pointer authentication adds another layer

**Verdict**: **Not feasible** without disabling SIP + re-signing the binary, which breaks notarization.

**Sources**:
- [SpecterOps: ARM-ed and Dangerous](https://specterops.io/blog/2025/08/21/armed-and-dangerous-dylib-injection-on-macos/)
- [theevilbit: DYLD_INSERT_LIBRARIES deep dive](https://theevilbit.github.io/posts/dyld_insert_libraries_dylib_injection_in_macos_osx_deep_dive/)
- [HackTricks: macOS Library Injection](https://book.hacktricks.xyz/macos-hardening/macos-security-and-privilege-escalation/macos-proces-abuse/macos-library-injection)

### 4.2 insert_dylib (Mach-O Binary Patching)

**Reference**: [cocoa-xu/insert_dylib_rs](https://github.com/cocoa-xu/insert_dylib_rs) (Rust rewrite of [Tyilo/insert_dylib](https://github.com/tyilo/insert_dylib))

**How it works**: Modifies the Mach-O binary to add an `LC_LOAD_DYLIB` (or `LC_LOAD_WEAK_DYLIB`) load command. The modified binary automatically loads the specified dylib at startup.

**Process** (from insert_dylib_rs source):
1. Copy binary -> output binary
2. Parse Mach-O header (supports fat binaries, arm64/x86_64)
3. Strip existing `LC_CODE_SIGNATURE` (required with `--strip-codesign`)
4. Append new `LC_LOAD_DYLIB` / `LC_LOAD_WEAK_DYLIB` command
5. Fix `__LINKEDIT` segment and `LC_SYMTAB`
6. Re-sign with `codesign -fs -` (ad-hoc)

**Feasibility for Zed Preview**:
- **Works technically**: Can patch `/Applications/Zed Preview.app/Contents/MacOS/zed`
- **Breaks notarization**: The app will show a macOS Gatekeeper warning
- **Requires re-signing**: `codesign -fs - --deep "/Applications/Zed Preview.app"`
- **Survives updates?** No -- every Zed update replaces the binary, must re-patch
- **The injected dylib** can use `__attribute__((constructor))` or Rust `#[ctor]` to run code at Zed startup
- **Can hook internal functions** via fishhook or Frida Gum once loaded inside the process

**The dylib could**:
- Hook `find_or_create_worktree` / `remove_worktree` to detect folder add/remove
- Hook sqlite3 write calls to detect DB updates
- Monitor the `WorktreeStoreEvent` internally

**Verdict**: **Technically feasible but fragile**. Breaks notarization, breaks on every update, requires careful Mach-O patching.

### 4.3 Frida (Dynamic Instrumentation)

**Reference**: [frida/frida-rust](https://github.com/frida/frida-rust) -- Rust bindings for Frida

**How it works**: Frida attaches to a running process and injects a JavaScript engine (V8) or native code. The `frida-gum` library provides:
- `Interceptor::replace()` -- replace function implementations
- `Module::find_export_by_name()` -- find exported symbols
- Process injection via `inject_library_file_sync(pid, path, entry, data)`

**Example from frida-rust** (`examples/gum/hook_open/src/lib.rs`):
```rust
#[ctor]
fn init() {
    let gum = Gum::obtain();
    let module = Module::load(&gum, "libc.so.6");
    let mut interceptor = Interceptor::obtain(&gum);
    let open = module.find_export_by_name("open").unwrap();
    // Replace open() with our detour, save original
    interceptor.replace(open, NativePointer(open_detour as *mut c_void), ...);
}
```

**Feasibility for Zed**:
- **SIP blocks `task_for_pid()`** on hardened-runtime binaries -- Frida **cannot attach** to Zed Preview without SIP disabled
- **With SIP disabled**: Can attach, but Rust symbols are mangled and many are stripped in release builds
- **Alternative**: Use insert_dylib to inject a frida-gum-based dylib (combines 4.2 + 4.3)
- **Can hook C-level calls**: sqlite3_exec, sqlite3_prepare, open(), etc.

**Verdict**: **Blocked by SIP** for direct attachment. Possible via insert_dylib injection path.

### 4.4 LLDB Scripting

**How it works**: Attach LLDB to running Zed process, set breakpoints with Python callbacks.

```python
# LLDB Python script
target = lldb.debugger.GetSelectedTarget()
bp = target.BreakpointCreateByName("sqlite3_exec")
bp.SetScriptCallbackFunction("my_module.on_sqlite_exec")
```

**Feasibility**:
- **Requires `com.apple.security.cs.debugger` entitlement** or SIP disabled to debug hardened-runtime apps
- **Massive performance impact**: Breakpoints halt the entire UI thread
- **Not suitable for production use**, only for debugging/research

**Sources**:
- [LLDB Python Reference](https://lldb.llvm.org/use/python-reference.html)
- [LLDB Breakpoint-Triggered Scripts](https://lldb.llvm.org/use/tutorials/breakpoint-triggered-scripts.html)
- [Kodeco: Attaching with LLDB](https://www.kodeco.com/books/advanced-apple-debugging-reverse-engineering/v3.0/chapters/3-attaching-with-lldb)

**Verdict**: **Research tool only**, not viable for production sync.

---

## 5. macOS Non-Hooking Monitoring

### 5.1 FSEvents / kqueue -- Watch the SQLite Database File

**How it works**: Monitor `~/Library/Application Support/Zed/db/0-*/db.sqlite` for write events.

| Method | Granularity | Performance | Suitability |
|--------|-------------|-------------|-------------|
| FSEvents | Directory-level, ~1s latency | Excellent | Good for detecting "something changed" |
| kqueue (EVFILT_VNODE, NOTE_WRITE) | File-level, immediate | Good for single files | Best for watching specific db.sqlite |
| fswatch | Cross-platform wrapper | Good | Convenient CLI tool |

**Feasibility**:
- **kqueue on the WAL file**: SQLite with WAL mode writes to `db.sqlite-wal` first, then checkpoints to `db.sqlite`
- Can detect when Zed writes to the database
- Then read the database externally using sqlite3 to check current workspace paths
- **Race condition risk**: Reading while Zed is writing (mitigated by WAL mode supporting concurrent reads)

**Sources**:
- [Apple: Kernel Queues](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/KernelQueues/KernelQueues.html)
- [SQLite Forum: Detecting database changes](https://sqlite.org/forum/forumpost/2798df4be8)
- [fswatch](https://github.com/emcrisostomo/fswatch)

**Verdict**: **Most practical non-invasive approach**. Can detect DB changes, read current state, and sync.

### 5.2 Direct SQLite Database Reading/Writing

Since we know the database schema (from 2.2 above), we can:

**Read** (safe with WAL mode):
```sql
SELECT workspace_id, paths, paths_order, workspace_file_path, workspace_file_kind
FROM workspaces
WHERE workspace_file_path IS NOT NULL
ORDER BY timestamp DESC;
```

**Write** (DANGEROUS -- could corrupt Zed's state):
```sql
UPDATE workspaces
SET paths = ?, paths_order = ?
WHERE workspace_id = ?;
```

**Risk Assessment**:
- SQLite WAL mode allows concurrent readers, so READ is safe
- WRITE is risky because Zed's in-memory state won't reflect the DB change
- Zed only reads the DB on startup/restore, not continuously
- Writing to DB while Zed is running = the changes are invisible until restart

### 5.3 CLI-Based Sync (zed --add)

- Monitor `.code-workspace` for changes via kqueue/FSEvents
- On change, diff the folder list
- For **additions**: `zed --add /new/path`
- For **removals**: **No CLI support** -- cannot remove folders externally

---

## 6. Feasibility Analysis

### Comparison Matrix

| Approach | SIP Required? | Breaks Notarization? | Survives Updates? | Add Folders? | Remove Folders? | Sync .code-workspace? | Complexity |
|---|---|---|---|---|---|---|---|
| **A. insert_dylib + hook** | No* | Yes | No (re-patch) | Yes | Yes | Yes | Very High |
| **B. Frida direct attach** | Yes (disable) | No | Yes | Yes | Yes | Yes | High |
| **C. insert_dylib + Frida Gum** | No* | Yes | No (re-patch) | Yes | Yes | Yes | Very High |
| **D. kqueue + sqlite read + CLI** | No | No | Yes | Yes (--add) | No | Partial | Medium |
| **E. kqueue + sqlite read/write** | No | No | Yes | Yes (on restart) | Yes (on restart) | Full (on restart) | Medium |
| **F. kqueue + sqlite read + CLI + file watch** | No | No | Yes | Yes (--add) | Partial (close+reopen) | Yes | Medium-High |

*insert_dylib doesn't need SIP disabled but re-signing breaks notarization

### Recommended Approach

**If SIP disabled + re-sign accepted**: See **[Section 8: insert_dylib + Frida Gum](#8-recommended-solution)** -- best event-driven, real-time, bidirectional sync with folder add AND remove support.

**If SIP must stay enabled**: Use **Hybrid D+E+F** below (non-invasive, no folder remove from .code-workspace → Zed).

### Fallback Approach: **Hybrid D+E+F** (No SIP Change)

A practical solution combining non-invasive techniques:

#### Direction 1: .code-workspace -> Zed (Add folders to running Zed)
1. **Watch** `.code-workspace` file with kqueue for changes
2. **Parse** the new folder list
3. **Diff** against current DB state (read sqlite)
4. For **new folders**: Use `zed --add /path` CLI
5. For **removed folders**: Write to a "pending removals" sidecar file; on next Zed restart, the workspace file will be re-read

#### Direction 2: Zed -> .code-workspace (Detect folder changes in Zed)
1. **Watch** `~/Library/Application Support/Zed/db/0-*/db.sqlite-wal` with kqueue
2. On write event, **read** the workspaces table for entries with `workspace_file_path IS NOT NULL`
3. **Compare** the `paths` column against the `.code-workspace` file content
4. **Update** the `.code-workspace` file JSON with the new folder list

---

## 7. Proposed Architecture

### MCP Server: `zed-workspace-sync`

```
+------------------+       kqueue        +------------------+
| .code-workspace  | <--- file watch --- | MCP Server       |
| (JSON file)      | --- parse/diff ---> | (zed-workspace-  |
+------------------+                     |  sync)           |
                                         |                  |
+------------------+       kqueue        |  Components:     |
| Zed sqlite DB    | <--- db watch  --- |  1. File watcher |
| (db.sqlite)      | --- sql read  ---> |  2. DB reader    |
+------------------+                     |  3. CLI invoker  |
                                         |  4. MCP tools    |
+------------------+       CLI IPC       +------------------+
| Running Zed      | <--- zed --add --- |
| (Zed Preview)    |                    |
+------------------+                    |
                                        |
+------------------+                    |
| AI Agent         | --- tool call ---> |
| (in Zed)         | <-- result     --- |
+------------------+
```

### MCP Tools Exposed

```json
{
  "tools": [
    {
      "name": "workspace_folders_list",
      "description": "List current workspace folders from .code-workspace and Zed DB",
      "input_schema": { "workspace_file": "string (path to .code-workspace)" }
    },
    {
      "name": "workspace_folders_add",
      "description": "Add a folder to the workspace (updates .code-workspace + invokes zed --add)",
      "input_schema": { "workspace_file": "string", "folder_path": "string" }
    },
    {
      "name": "workspace_folders_remove",
      "description": "Remove a folder from .code-workspace (takes effect on next Zed restart/reopen)",
      "input_schema": { "workspace_file": "string", "folder_path": "string" }
    },
    {
      "name": "workspace_folders_sync",
      "description": "Force sync between .code-workspace and Zed DB state",
      "input_schema": { "workspace_file": "string", "direction": "file_to_zed | zed_to_file | bidirectional" }
    }
  ]
}
```

### Background Daemon Components

1. **File Watcher** (kqueue on `.code-workspace`):
   - Detect external edits to the workspace file
   - Trigger `zed --add` for new folders
   - Queue removals for next restart

2. **DB Watcher** (kqueue on `db.sqlite-wal`):
   - Detect Zed's workspace saves (throttled by 200ms in Zed)
   - Read workspace paths from sqlite
   - Update `.code-workspace` JSON

3. **CLI Invoker**:
   - Uses `zed --add /path` via the existing Unix socket IPC
   - Handles error cases (folder doesn't exist, already in workspace, etc.)

4. **Sync Engine**:
   - Maintains a "last known state" to compute diffs
   - Handles conflict resolution (both sides changed simultaneously)
   - Debounces to avoid feedback loops (watch DB -> update file -> watch file -> update DB)

### Key Technical Details

- **Zed DB path**: `~/Library/Application Support/Zed/db/0-workspace/db.sqlite`
- **Zed socket**: `~/Library/Application Support/Zed/zed-preview.sock`
- **WAL mode**: Safe for concurrent read access from external process
- **Serialization throttle**: 200ms debounce in Zed -- sync should wait at least 300ms after detecting a change
- **Feedback loop prevention**: Use a lock file or generation counter to distinguish self-initiated changes

---

## 8. RECOMMENDED: Best Solution with SIP Disabled + Re-sign

### 8.1 Decision: **insert_dylib + Frida Gum (In-Process Hooking)**

Given the constraints (SIP disabled accepted, re-signing per version accepted), the **best event-driven solution** is:

**inject a Rust dylib into Zed's Mach-O binary using insert_dylib_rs, with the dylib using frida-gum to hook C-level sqlite3 functions for real-time, event-driven monitoring of workspace changes.**

### 8.2 Why This Is the Best Approach

| Criterion | Frida Direct Attach | insert_dylib + Frida Gum | kqueue + sqlite read | DYLD_INSERT_LIBRARIES |
|---|---|---|---|---|
| **Real-time events** | Yes | **Yes** | ~300ms delay | Yes |
| **Folder add detection** | Yes | **Yes** | Yes (indirect) | Yes |
| **Folder remove detection** | Yes | **Yes** | Yes (indirect) | Yes |
| **No external process needed** | No (Frida host) | **Yes (runs inside Zed)** | No (daemon) | Yes |
| **Survives Zed restart** | No (re-attach) | **Yes (built into binary)** | Yes | Yes |
| **Access to SQL content** | Via hooking | **Direct hook on sqlite3 calls** | External read only | Direct hook |
| **macOS 14.4+ compatible** | **Problematic** (task_for_pid blocked) | **Yes** (no runtime attach) | Yes | Needs re-sign |
| **Stability** | Can crash target | **Very stable (gum is robust)** | Very stable | Stable |
| **Per-update effort** | None | **Re-patch + re-sign** | None | Re-sign only |

**Key advantages over Frida direct attach**:
- Frida direct attach uses `task_for_pid()` which has [ongoing issues on macOS 14.4+](https://github.com/frida/frida-core/issues/524) requiring additional NVRAM boot args (`amfi_get_out_of_my_way=1`)
- insert_dylib doesn't need `task_for_pid()` -- the dylib loads as part of the process naturally
- No external Frida host process needed

**Key advantages over pure kqueue/sqlite monitoring**:
- True event-driven: hooks fire at the exact moment Zed writes, no polling delay
- Access to the actual SQL statements being executed -- can see precisely what changed
- Can intercept both the workspace paths AND the workspace_file_path/kind columns
- Can trigger sync BEFORE Zed's sqlite write completes (to avoid race conditions)

### 8.3 Hooking Targets: C-Level sqlite3 Functions

Zed uses `libsqlite3-sys` (C FFI) through its `sqlez` crate. The key C functions called are:

| C Function | Zed Usage Location | What It Tells Us |
|---|---|---|
| `sqlite3_exec()` | `sqlez/src/migrations.rs:19` | Schema migrations (DDL) |
| `sqlite3_prepare_v2()` | `sqlez/src/connection.rs:123,148`, `sqlez/src/statement.rs:59` | Every SQL query preparation |
| `sqlite3_step()` | `sqlez/src/statement.rs:274` | Each row execution step |
| `sqlite3_bind_text()` | `sqlez/src/statement.rs:223` | Text parameter binding (paths!) |
| `sqlite3_bind_blob()` | `sqlez/src/statement.rs:139` | Blob parameter binding |
| `sqlite3_open_v2()` | `sqlez/src/connection.rs:31` | Database open |

**Best hook targets for workspace sync**:

1. **`sqlite3_prepare_v2()`** -- intercept the SQL text to detect workspace-related queries
   - Filter for queries containing `INSERT INTO workspaces` or `UPDATE workspaces`
   - This catches all workspace persistence writes

2. **`sqlite3_bind_text()`** -- intercept bound path values
   - When workspace queries are detected, capture the path values being bound
   - This gives us the actual folder paths being saved

These are **standard C symbols** exported by `libsqlite3.dylib` -- no Rust symbol mangling issues, no stripped symbol problems. They are stable across Zed versions because Zed always uses sqlite3 through the same C FFI.

### 8.4 Detailed Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Zed Preview Process                       │
│                                                             │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐  │
│  │ Workspace    │───>│ WorktreeStore│───>│ serialize_    │  │
│  │ UI Actions   │    │ Events       │    │ workspace()  │  │
│  │ (add/remove) │    │ Added/Removed│    │ (200ms       │  │
│  └──────────────┘    └──────────────┘    │  debounce)   │  │
│                                          └──────┬───────┘  │
│                                                 │           │
│                                          ┌──────▼───────┐  │
│                                          │ sqlez crate   │  │
│                                          │ (Rust)        │  │
│                                          └──────┬───────┘  │
│                                                 │           │
│  ┌──────────────────────────────────────────────▼───────┐  │
│  │              libsqlite3 (C FFI)                       │  │
│  │  sqlite3_prepare_v2() ◄── HOOK HERE                  │  │
│  │  sqlite3_bind_text()  ◄── HOOK HERE                  │  │
│  │  sqlite3_step()       ◄── HOOK HERE (optional)       │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │        libzed_workspace_sync.dylib (INJECTED)        │  │
│  │                                                       │  │
│  │  #[ctor] init():                                      │  │
│  │    1. Gum::obtain()                                   │  │
│  │    2. Find sqlite3_prepare_v2 in libsqlite3.dylib     │  │
│  │    3. Interceptor::replace() with our detour          │  │
│  │    4. Start background sync thread                    │  │
│  │                                                       │  │
│  │  on_sqlite_prepare(sql_text):                         │  │
│  │    if sql contains "workspaces":                      │  │
│  │      → capture paths from bind_text calls             │  │
│  │      → send to sync thread via channel                │  │
│  │                                                       │  │
│  │  sync_thread:                                         │  │
│  │    - Also watches .code-workspace via kqueue          │  │
│  │    - Diff workspace file vs intercepted DB state      │  │
│  │    - Update .code-workspace JSON on Zed changes       │  │
│  │    - For file→Zed: use zed CLI --add                  │  │
│  │    - For file→Zed removals: (see 8.6)                 │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  MCP Context Server (runs as Zed extension)           │  │
│  │    - Exposes tools to AI agent                        │  │
│  │    - Communicates with sync dylib via shared memory   │  │
│  │      or Unix socket                                   │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
          │                                    ▲
          ▼                                    │
┌──────────────────┐                 ┌──────────────────┐
│ .code-workspace  │ ◄── kqueue ──── │ sync_thread      │
│ (JSON file)      │ ── parse ────>  │ (inside dylib)   │
└──────────────────┘                 └──────────────────┘
```

### 8.5 Implementation: The Injected Dylib (Rust Pseudocode)

```rust
// lib.rs — libzed_workspace_sync.dylib
use ctor::ctor;
use frida_gum::{interceptor::Interceptor, Gum, Module, NativePointer};
use std::sync::{OnceLock, Mutex, mpsc};
use std::ffi::{CStr, c_char, c_int, c_void};

// Channel for sending intercepted workspace changes to sync thread
static CHANGE_TX: OnceLock<Mutex<mpsc::Sender<WorkspaceChange>>> = OnceLock::new();

// Original function pointers
static ORIG_PREPARE: OnceLock<Mutex<Option<PrepareFunc>>> = OnceLock::new();

type PrepareFunc = unsafe extern "C" fn(
    *mut c_void,      // sqlite3*
    *const c_char,     // sql
    c_int,             // nByte
    *mut *mut c_void,  // ppStmt
    *mut *const c_char // pzTail
) -> c_int;

enum WorkspaceChange {
    PathsUpdated { workspace_id: i64, paths: Vec<String> },
    WorkspaceFileUpdated { path: String, kind: String },
}

/// Detour for sqlite3_prepare_v2 — fires on every SQL query
unsafe extern "C" fn prepare_detour(
    db: *mut c_void,
    sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut c_void,
    pz_tail: *mut *const c_char,
) -> c_int {
    // Call original first
    let result = ORIG_PREPARE.get().unwrap().lock().unwrap()
        .unwrap()(db, sql, n_byte, pp_stmt, pz_tail);

    // Check if this is a workspace-related query
    if let Ok(sql_str) = CStr::from_ptr(sql).to_str() {
        if sql_str.contains("workspaces") &&
           (sql_str.contains("INSERT") || sql_str.contains("UPDATE")) {
            // Signal the sync thread that a workspace write is happening
            if let Some(tx) = CHANGE_TX.get() {
                // The actual path extraction happens via sqlite3_bind_text hook
                // Here we just flag that a workspace write is in progress
            }
        }
    }
    result
}

#[ctor]
fn init() {
    static GUM: OnceLock<Gum> = OnceLock::new();
    let gum = GUM.get_or_init(|| Gum::obtain());

    // Set up inter-thread channel
    let (tx, rx) = mpsc::channel::<WorkspaceChange>();
    CHANGE_TX.get_or_init(|| Mutex::new(tx));
    ORIG_PREPARE.get_or_init(|| Mutex::new(None));

    // Find sqlite3_prepare_v2 in the loaded libsqlite3
    let mut interceptor = Interceptor::obtain(gum);

    // On macOS, sqlite3 is in /usr/lib/libsqlite3.dylib (system) or bundled
    let sqlite_module = Module::load(gum, "libsqlite3.dylib");
    let prepare_fn = sqlite_module.find_export_by_name("sqlite3_prepare_v2").unwrap();

    unsafe {
        *ORIG_PREPARE.get().unwrap().lock().unwrap() = Some(std::mem::transmute(
            interceptor.replace(
                prepare_fn,
                NativePointer(prepare_detour as *mut c_void),
                NativePointer(std::ptr::null_mut()),
            ).unwrap().0
        ));
    }

    // Start background sync thread
    std::thread::spawn(move || {
        sync_loop(rx);
    });
}

fn sync_loop(rx: mpsc::Receiver<WorkspaceChange>) {
    // 1. Set up kqueue watcher on .code-workspace file
    // 2. Listen for intercepted DB changes via rx channel
    // 3. On DB change → update .code-workspace JSON
    // 4. On file change → invoke `zed --add` for new folders
    // 5. Debounce to avoid feedback loops
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(change) => handle_db_change(change),
            Err(mpsc::RecvTimeoutError::Timeout) => check_file_changes(),
            Err(_) => break,
        }
    }
}
```

### 8.6 Solving the "Remove Folder" Problem

With in-process hooking, we can solve the folder removal problem that the non-invasive approach couldn't:

**Option A: Hook the remove_worktree Rust function**
- Requires finding the mangled symbol via binary analysis (Ghidra/IDA)
- Fragile across Zed versions (symbol offsets change)
- Pattern: `Module::find_base_address()` + known offset

**Option B: Detect removal via SQL diff (recommended)**
- On each `sqlite3_prepare_v2` for workspace UPDATE:
  - Read previous paths from .code-workspace
  - Compare with new paths being written
  - Paths present in .code-workspace but absent from new DB write = removed
  - Update .code-workspace to remove those paths
- This is purely C-level hooking, no Rust symbols needed

**Option C: For .code-workspace → Zed removals**
- The dylib runs inside Zed's process but can't easily call Rust APIs
- Best approach: write a small sidecar file (e.g., `.zed-sync-pending-removes`)
- On next `serialize_workspace()` hook, inject the removal into the Zed state
- OR: simply close and reopen the workspace file via CLI: `zed my.code-workspace`

### 8.7 Per-Version Patching Workflow

```bash
#!/bin/bash
# patch-zed-preview.sh — Run after each Zed Preview update

ZED_APP="/Applications/Zed Preview.app"
ZED_BIN="$ZED_APP/Contents/MacOS/zed"
DYLIB_PATH="/usr/local/lib/libzed_workspace_sync.dylib"

# 1. Verify Zed Preview exists
if [ ! -f "$ZED_BIN" ]; then
    echo "Zed Preview not found"
    exit 1
fi

# 2. Backup original binary
cp "$ZED_BIN" "$ZED_BIN.original"

# 3. Inject the dylib load command
insert_dylib_rs \
    --binary "$ZED_BIN" \
    --dylib "$DYLIB_PATH" \
    --weak --strip-codesign

# 4. Re-sign with ad-hoc signature
codesign -fs - --deep "$ZED_APP"

# 5. Verify
echo "Patched Zed Preview successfully"
codesign -dv "$ZED_BIN" 2>&1 | grep "Signature"
```

This can be automated via a Homebrew post-install hook or launchd watcher on the Zed app bundle.

### 8.8 Comparison: Why Not Just Frida Direct Attach?

While Frida direct attach (without insert_dylib) seems simpler, it has critical issues on modern macOS:

1. **macOS 14.4+ broke `task_for_pid()`** ([frida-core#524](https://github.com/frida/frida-core/issues/524)): Even with SIP disabled, Frida needs additional NVRAM args: `amfi_get_out_of_my_way=1 thid_should_crash=0 tss_should_crash=0`
2. **Requires a persistent Frida host process** running alongside Zed
3. **Re-attach after every Zed restart** — vs insert_dylib which loads automatically
4. **arm64e issues** ([frida-gum#1025](https://github.com/frida/frida-gum/issues/1025)): Pointer authentication can cause crashes during interception

The insert_dylib approach avoids all of these because the dylib loads as a natural part of the Mach-O binary — no `task_for_pid()`, no external process, no runtime attachment.

---

## 9. SIP Disable Guide & Security Considerations

### 9.1 Checking Current SIP Status

```bash
csrutil status
# Expected output: "System Integrity Protection status: enabled."
```

### 9.2 Disabling SIP on Apple Silicon (M1/M2/M3/M4)

1. **Shut down** your Mac completely
2. **Press and hold** the power button until "Loading startup options" appears
3. Select **Options** → continue
4. Open **Terminal** from the Utilities menu
5. Run: `csrutil disable`
6. (Optional, for Frida compatibility): `sudo nvram boot-args="-arm64e_preview_abi"`
7. Restart

### 9.3 Verifying SIP is Disabled

```bash
csrutil status
# "System Integrity Protection status: disabled."
```

### 9.4 Security Implications

| Risk | Mitigation |
|---|---|
| Malware can modify system files | Only disable on development machine, not production |
| Kernel extensions can load freely | Keep macOS Gatekeeper enabled |
| Code injection possible on all apps | Our use is targeted to Zed only |
| No protection for system binaries | Standard macOS firewall + antivirus still active |

**Best Practice**: Only keep SIP disabled on machines used for development where this sync functionality is needed. Re-enable SIP (`csrutil enable` from Recovery) when no longer needed.

### 9.5 Alternative: Selective SIP Disable (if supported)

Some macOS versions support partial SIP:
```bash
csrutil enable --without debug    # Allow debugging only
csrutil enable --without fs       # Allow filesystem modifications only
```

Check `csrutil status` after to confirm which protections are active. Selective disable may be sufficient if it allows `task_for_pid()` and ad-hoc code signing.

**Sources**:
- [Apple: Disabling and Enabling SIP](https://developer.apple.com/documentation/security/disabling-and-enabling-system-integrity-protection)
- [macOS SIP Overview](https://www.cleverfiles.com/help/system-integrity-protection.html)
- [Frida macOS 14.4+ issues](https://github.com/frida/frida-core/issues/524)

---

## Appendix A: insert_dylib Approach Details (For Reference)

If the hooking path is ever desired (e.g., for full bidirectional real-time sync including folder removal), here's the specific procedure:

### Step 1: Patch the Zed Binary

Using `insert_dylib_rs`:
```bash
insert_dylib_rs \
    --binary "/Applications/Zed Preview.app/Contents/MacOS/zed" \
    --dylib @rpath/libzed_workspace_sync.dylib \
    --weak --strip-codesign
```

### Step 2: Build the Dylib (Rust)

```rust
// libzed_workspace_sync.dylib
use ctor::ctor;
use frida_gum::{interceptor::Interceptor, Gum, Module};

#[ctor]
fn init() {
    // Hook sqlite3_exec to detect workspace writes
    // Hook Zed's internal functions via symbol scanning
    // Start background sync thread
}
```

### Step 3: Re-sign

```bash
codesign -fs - --deep "/Applications/Zed Preview.app"
```

### Drawbacks
- Breaks notarization (Gatekeeper warning)
- Must re-patch on every Zed update
- Rust symbols are mangled and often stripped in release builds
- Complex to maintain

---

## Appendix B: Reference Projects

- **insert_dylib_rs**: [cocoa-xu/insert_dylib_rs](https://github.com/cocoa-xu/insert_dylib_rs) -- Rust Mach-O patcher, handles fat binaries, LC_CODE_SIGNATURE stripping
- **insert_dylib** (original): [Tyilo/insert_dylib](https://github.com/tyilo/insert_dylib) -- C implementation
- **frida-rust**: [frida/frida-rust](https://github.com/frida/frida-rust) -- Rust bindings, includes `gum/hook_open` example for function interception
- **fswatch**: [emcrisostomo/fswatch](https://github.com/emcrisostomo/fswatch) -- Cross-platform file change monitor

---

## Appendix C: Zed Source Code Key Paths

| Component | File | Line(s) |
|---|---|---|
| .code-workspace parser | `crates/workspace/src/workspace_file.rs` | Full file |
| Worktree add/remove events | `crates/project/src/worktree_store.rs` | L82-91, L693-752, L755+ |
| Workspace serialize trigger | `crates/workspace/src/workspace.rs` | L1343-1357, L5890-5904 |
| Serialize throttle (200ms) | `crates/workspace/src/workspace.rs` | L155 |
| Add folder to project action | `crates/workspace/src/workspace.rs` | L3191-3239 |
| SQLite DB open/config | `crates/db/src/db.rs` | L25-80 |
| Database directory path | `crates/paths/src/paths.rs` | L214-217 |
| Data directory (macOS) | `crates/paths/src/paths.rs` | L102-107 |
| CLI IPC socket | `crates/zed/src/zed/open_listener.rs` | L279-301 |
| CLI request structure | `crates/cli/src/cli.rs` | L12-25 |
| LSP workspace folders | `crates/lsp/src/lsp.rs` | L1580-1679 |
| MCP roots (unimplemented) | `crates/context_server/src/protocol.rs` | L43 |
| Workspace DB schema migration | `crates/workspace/src/persistence.rs` | L967+ |
| sqlite3_exec (C FFI) | `crates/sqlez/src/migrations.rs` | L11, L19 |
| sqlite3_prepare_v2 (C FFI) | `crates/sqlez/src/connection.rs` | L123, L148 |
| sqlite3_step (C FFI) | `crates/sqlez/src/statement.rs` | L274 |
| sqlite3_bind_text (C FFI) | `crates/sqlez/src/statement.rs` | L223 |
| sqlite3_bind_blob (C FFI) | `crates/sqlez/src/statement.rs` | L139 |
| sqlite3_open_v2 (C FFI) | `crates/sqlez/src/connection.rs` | L31 |
| frida-gum hook_open example | `frida-rust/examples/gum/hook_open/src/lib.rs` | Full file |
| frida inject_lib_file example | `frida-rust/examples/core/inject_lib_file/src/main.rs` | Full file |
| insert_dylib_rs main | `insert_dylib_rs/src/main.rs` | L154 (insert_dylib fn) |
