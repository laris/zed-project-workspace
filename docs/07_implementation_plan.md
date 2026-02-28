# Implementation Plan: Workspace Sync Refactor

Date: 2026-02-27
Updated: 2026-02-28
Status: **Phases 1-5 COMPLETE. Phase 4 (live test) pending.**
Depends on: `05_research report.md`, `06_proposal_design.md`

## Implementation Status Summary

| Phase | Description | Status | Tests |
|-------|-------------|--------|-------|
| 1 | Shared library: paths, mapping, lock, discovery, workspace_db, workspace_file, sync_engine | DONE | 65 pass |
| 2 | Hook refactor: AtomicPtr, per-workspace_id dispatch, shared lib usage | DONE | 25 pass |
| 3 | MCP: 9 tools (4 refactored + 5 new) | DONE | Builds clean |
| 4 | Live test with Zed Preview | PENDING | — |
| 5 | Hook socket API + channel-aware CLI + MCP process lifecycle | DONE | Verified live |
| 6 | Picker pin hook (Layer 4) + project_name write scope fix | IN PROGRESS | — |

**Additional work completed (beyond original plan):**
- `dylib-kit` SDK: standalone crate at `~/codes/dylib-kit/` with `dylib-hook-registry` + `dylib-patcher`
- Both xtasks migrated to SDK (55/51 lines, was 416/411)
- Artifact tracking: SHA-256 hash + git commit stored per-hook at patch time
- Health check verification: `cargo patch verify` checks log markers after app launch
- Stale detection: `cargo patch status` warns if dylib rebuilt since patching
- `zed-yolo-hook` updated: registry integration + SDK xtask migration
- **Phase 5**: Hook socket API (Option C), channel-aware CLI fallback, MCP binary rename, parent PID watchdog, stale process cleanup
- **Phase 6**: Picker pin hook + project_name write scope fix (see `10_picker_pin_design.md`)

---

## Phase 6: Picker Pin Hook + project_name Write Scope Fix

Design: `10_picker_pin_design.md`
Updated pinning strategy: `09_workspace_identity_pinning.md` (Four-Layer Defense)

### 6a. Picker Sort Hook (Layer 4)

Replace the alphabetical sort comparator in `get_open_folders()` via Frida to pin the target folder to the top of the project picker dropdown.

| Step | File | What |
|---|---|---|
| 1 | `hook/src/hooks/picker_sort.rs` | NEW — comparator detour, SharedString reader, symbol constants |
| 2 | `hook/src/hooks/mod.rs` | Add `pub mod picker_sort;` |
| 3 | `hook/src/lib.rs` | Symbol lookup + install/deferred in `init_inner()` |
| 4 | `hook/src/sync.rs` | Call `try_deferred_picker_install()` after discovery |

### 6b. project_name Write Scope Fix

Write `project_name` only to the primary root (where folder_name == name). Clean up trash from non-primary roots.

| Step | File | What |
|---|---|---|
| 5 | `src/settings.rs` | Add `find_primary_root()`, `cleanup_stale_project_names()` |
| 6 | `src/discovery.rs` | 4 call sites: write to primary root only |
| 7 | `hook/src/discovery.rs` | 6 call sites: write to primary root only |
| 8 | One-time cleanup | Remove `project_name` from dylib-kit, _my__zed-api-key, gh-zed-industries__zed, zed-yolo-hook |

---

## 1. Runtime Topology & Concurrency Model

### 1.1 Actual Process Layout (Verified)

```
macOS
 └── Zed Preview (1 process, N windows)
      ├── cdylib hook (1 instance, loaded once, shared across ALL windows)
      │   └── intercepts sqlite3_prepare_v2 for the entire process
      │
      ├── Window 1 (workspace_id=110)
      │   ├── MCP: zed-workspace-sync (separate process, pid A)
      │   └── .code-workspace file: ~/codes/project-a.code-workspace
      │
      ├── Window 2 (workspace_id=94)
      │   ├── MCP: zed-workspace-sync (separate process, pid B)
      │   └── .code-workspace file: ~/codes/project-b.code-workspace
      │
      └── SQLite DB (one file, WAL mode, shared by all)
           ~/Library/Application Support/Zed/db/0-preview/db.sqlite
```

**Key facts** (verified on live system):
- **1 Zed process** hosts ALL windows (not one process per window)
- **1 hook cdylib** loaded into that process (via `DYLD_INSERT_LIBRARIES` or `insert_dylib`)
- **N MCP processes** — Zed spawns one `zed-workspace-sync` per workspace/window
- **1 SQLite DB** shared by all — WAL mode allows concurrent reads
- Stale MCP processes accumulate (observed 11 processes for 2 active windows)

### 1.2 Concurrency Hazards

| Hazard | Scenario | Risk |
|--------|----------|------|
| **H1: Hook fires for wrong workspace** | Window 1 saves → hook fires → hook reads "latest workspace" → picks Window 2's workspace | Wrong .code-workspace updated |
| **H2: Concurrent file writes** | Hook (Zed→File) and MCP (File→Zed) both write .code-workspace simultaneously | Corrupted file |
| **H3: Self-write loop** | Hook writes .code-workspace → Zed's file watcher detects change → triggers re-read | Infinite loop (mitigated by cooldown, but wasteful) |
| **H4: Stale MCP processes** | Old MCP process from closed window still running, receives tool call | Operates on stale state |
| **H5: DB read during write** | Hook reads DB while Zed is mid-write (between BEGIN and COMMIT) | Inconsistent read (mitigated by WAL, but TOCTOU possible) |
| **H6: Multiple MCP sync calls** | User triggers sync from two agents simultaneously | Race condition on file + DB |

### 1.3 Concurrency Solutions

**S1: Per-workspace_id dispatch in hook (solves H1)**

Replace the global "latest workspace" approach with per-workspace_id tracking:

```rust
// In hook: track pending syncs per workspace_id
static PENDING_SYNCS: LazyLock<Mutex<HashMap<i64, Instant>>> = ...;

fn on_workspace_write(workspace_id: i64) {
    let mut pending = PENDING_SYNCS.lock();
    pending.insert(workspace_id, Instant::now());
    // Debounce: only sync after 300ms of no writes for THIS workspace_id
    schedule_sync(workspace_id, Duration::from_millis(300));
}
```

To get workspace_id: hook `sqlite3_bind_int64` on the prepared statement (Refinement #8 from proposal) or parse it from the SQL VALUES clause.

**S2: File-level advisory lock (solves H2, H6)**

Use `flock()` (or `fs2::FileExt::lock_exclusive()`) when writing `.code-workspace`:

```rust
fn write_workspace_file(path: &Path, content: &str) -> Result<()> {
    let lock_path = path.with_extension("code-workspace.lock");
    let lock_file = File::create(&lock_path)?;
    lock_file.lock_exclusive()?;  // blocks until acquired

    // Write atomically: write to .tmp, then rename
    let tmp = path.with_extension("code-workspace.tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;

    lock_file.unlock()?;
    fs::remove_file(&lock_path).ok();  // cleanup
    Ok(())
}
```

Both hook and MCP use this same function from the shared library.

**S3: Self-write detection via generation counter (solves H3)**

```rust
// In shared library
static WRITE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn write_workspace_file(...) {
    WRITE_GENERATION.fetch_add(1, Ordering::SeqCst);
    // ... write file ...
}

fn should_sync_file_change(file_mtime: SystemTime) -> bool {
    // Compare file mtime against our last write time
    // If we just wrote it, skip
    let gen = WRITE_GENERATION.load(Ordering::SeqCst);
    // ... check if mtime matches our last write ...
}
```

For cross-process detection (MCP wrote, hook checks): store generation in the lock file or mapping file's `last_sync_ts`.

**S4: MCP process lifecycle (solves H4)**

MCP servers run as long as Zed keeps the stdio pipe open. When Zed closes a window, it drops the pipe → MCP receives EOF → exits. Stale processes are from Zed not cleaning up properly.

Mitigations:
- MCP should exit on stdin EOF (already standard rmcp behavior)
- Add a periodic liveness check: if parent PID changes or dies, exit
- Tool calls should validate workspace state before operating (check workspace_id still exists in DB)

**S5: DB read consistency (solves H5)**

SQLite WAL mode guarantees snapshot isolation for reads. Our reads see a consistent state even during concurrent writes. No additional locking needed for reads.

For writes (which we don't do to Zed's DB), we'd need transactions. Since we only READ Zed's DB, WAL is sufficient.

**S6: MCP tool-level mutex (solves H6)**

Use file-based mutex per workspace_file for tool operations:

```rust
fn with_workspace_lock<T>(workspace_file: &Path, f: impl FnOnce() -> T) -> T {
    let lock = workspace_file.with_extension("code-workspace.lock");
    let file = File::create(&lock).unwrap();
    file.lock_exclusive().unwrap();
    let result = f();
    file.unlock().unwrap();
    result
}
```

---

## 2. Architecture (Post-Refactor)

### 2.1 Crate Layout (Unchanged)

```
zed-project-workspace/
├── Cargo.toml                    # Workspace root
├── src/
│   ├── lib.rs                    # Re-exports
│   ├── workspace_file.rs         # .code-workspace parse/write/diff
│   ├── workspace_db.rs           # Zed DB reader (refactored)
│   ├── sync_engine.rs            # Sync logic (refactored)
│   ├── mapping.rs                # NEW: .zed/zed-project-workspace.json read/write
│   ├── discovery.rs              # NEW: moved from hook, shared discovery logic
│   ├── paths.rs                  # NEW: path normalization, relative path computation
│   └── lock.rs                   # NEW: file locking utilities
├── zed-prj-workspace-hook/
│   └── src/
│       ├── lib.rs                # Hook entry (ctor)
│       ├── hooks/
│       │   └── sqlite3_prepare.rs  # Refactored: OnceLock, per-workspace dispatch
│       ├── sync.rs               # Simplified: delegates to shared library
│       └── config.rs / logging.rs
├── zed-prj-workspace-mcp/
│   └── src/
│       └── main.rs               # 8 MCP tools (refactored)
└── xtask/
    └── src/main.rs               # Build/deploy helper
```

### 2.2 Data Flow

```
                    ┌─────────────────────────────┐
                    │     Zed Process (single)     │
                    │                              │
                    │  Window 1      Window 2      │
                    │  (ws_id=110)   (ws_id=94)    │
                    │      │              │        │
                    │      ▼              ▼        │
                    │  ┌──────────────────────┐    │
                    │  │  sqlite3_prepare_v2  │    │
                    │  │  (our hook detour)   │    │
                    │  └──────────┬───────────┘    │
                    │             │                 │
                    └─────────────┼─────────────────┘
                                  │ workspace write detected
                                  ▼
                    ┌─────────────────────────────┐
                    │  Per-workspace_id dispatch   │
                    │  (debounce 300ms per ws_id)  │
                    └──────────┬──────────────────┘
                               │
                    ┌──────────▼──────────────────┐
                    │  Sync: Zed → File            │
                    │  1. Read DB (paths+order)    │
                    │  2. Read mapping file        │
                    │  3. Acquire file lock        │
                    │  4. Read .code-workspace     │
                    │  5. Diff (ordered)           │
                    │  6. Write .code-workspace    │
                    │  7. Update last_sync_ts      │
                    │  8. Release file lock        │
                    └─────────────────────────────┘

        ┌─────────────────────────────────────────────┐
        │  MCP Process (one per window)                │
        │                                              │
        │  Tool call: workspace_folders_add            │
        │  1. Read mapping file → workspace_id         │
        │  2. Acquire file lock                        │
        │  3. Read .code-workspace                     │
        │  4. Add folder                               │
        │  5. Write .code-workspace                    │
        │  6. Release file lock                        │
        │  7. zed --add <folder> (wait for exit)       │
        │  8. Update last_sync_ts                      │
        └─────────────────────────────────────────────┘
```

### 2.3 Shared Library API Surface

```rust
// mapping.rs
pub struct WorkspaceMapping {
    pub workspace_id: i64,
    pub workspace_file: String,       // relative path
    pub zed_channel: Option<String>,  // "preview" | "stable"
    pub last_sync_ts: Option<String>, // ISO 8601
}
impl WorkspaceMapping {
    pub fn read(worktree_root: &Path) -> Option<Self>;
    pub fn write(worktree_root: &Path, mapping: &Self) -> Result<()>;
    pub fn resolve_workspace_file(&self, worktree_root: &Path) -> PathBuf;
}

// discovery.rs (moved from hook, now shared)
pub struct DiscoveryResult {
    pub workspace_id: i64,
    pub workspace_file: PathBuf,
    pub mapping: WorkspaceMapping,
    pub db_path: PathBuf,
    pub zed_channel: String,
}
pub fn discover(roots: &[PathBuf], db_path: Option<&Path>) -> Result<DiscoveryResult>;

// workspace_db.rs (refactored)
pub struct WorkspaceRecord {
    pub workspace_id: i64,
    pub paths: Vec<PathBuf>,          // lexicographic order
    pub paths_order: Vec<usize>,      // user order permutation
    pub timestamp: String,
    pub workspace_file_path: Option<String>,  // PR #46225 column, if exists
}
impl WorkspaceRecord {
    pub fn ordered_paths(&self) -> Vec<PathBuf>;  // reconstructed user order
}
impl ZedDbReader {
    pub fn find_by_id(&self, id: i64) -> Option<WorkspaceRecord>;
    pub fn find_by_paths(&self, paths: &[PathBuf]) -> Option<WorkspaceRecord>;
    pub fn default_db_path(channel_hint: Option<&str>) -> Option<PathBuf>;
}

// sync_engine.rs (refactored)
pub enum SyncDirection { ZedToFile, FileToZed, Bidirectional }
pub struct SyncResult {
    pub direction: SyncDirection,
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub reordered: bool,
    pub conflict: bool,
}
pub fn sync(mapping: &WorkspaceMapping, db: &ZedDbReader, direction: SyncDirection) -> Result<SyncResult>;

// lock.rs
pub fn with_workspace_lock<T>(workspace_file: &Path, f: impl FnOnce() -> Result<T>) -> Result<T>;

// paths.rs
pub fn normalize_path(path: &Path) -> PathBuf;
pub fn relative_path(from: &Path, to: &Path) -> PathBuf;
```

---

## 3. Implementation Phases

### Phase 1: Shared Library Foundation (No Behavior Change)

**Goal**: Refactor shared library internals without changing external behavior. All existing tests pass.

**Tasks**:
1. Add `mapping.rs` — read/write `.zed/zed-project-workspace.json`
2. Add `paths.rs` — `normalize_path()`, `relative_path()` (use `pathdiff` crate)
3. Add `lock.rs` — file-level advisory locking
4. Refactor `workspace_db.rs`:
   - Add `WorkspaceRecord.paths_order` and `ordered_paths()`
   - Add `find_by_id()` and `find_by_paths()` (exact match)
   - Unify `default_db_path()` with exe-based channel detection
   - Make `parse_workspace_paths()` pub
5. Refactor `workspace_file.rs`:
   - Use `paths::normalize_path()` in `resolve()`
   - Use `paths::relative_path()` in `pathdiff_relative()`
   - Make diff compare ordered sequences, not just sets
6. Move shared discovery logic from hook into `discovery.rs`

**Tests**:
- Unit tests for `mapping.rs` (read/write/resolve)
- Unit tests for `paths.rs` (normalize, relative with `..`)
- Unit tests for `workspace_db.rs` (find_by_id, find_by_paths, ordered_paths)
- Integration test: round-trip mapping write→read
- Integration test: workspace_file diff detects reorder

### Phase 2: Hook Refactor

**Goal**: Hook uses shared library, per-workspace dispatch, no Mutex on hot path.

**Tasks**:
1. Replace `Mutex<Option<PrepareV2Fn>>` with `OnceLock<PrepareV2Fn>`
2. Implement per-workspace_id debounce (HashMap behind Mutex, only locked on workspace writes)
3. Replace raw rusqlite calls with `ZedDbReader` from shared library
4. Remove duplicate `parse_workspace_paths()` (use shared)
5. Use `lock::with_workspace_lock()` for file writes
6. Use `mapping.rs` for discovery instead of `project_name` hack
7. Add self-write detection via `last_sync_ts` comparison
8. Remove dead `SELF_WRITE_GENERATION` code

**Tests**:
- Unit test: `is_workspace_write()` with various SQL patterns (INSERT, UPDATE, DELETE)
- Unit test: per-workspace_id debounce (rapid fire → single sync)
- Integration test: hook discovery creates mapping file
- Integration test: hook syncs DB change to .code-workspace (end-to-end)
- Stress test: rapid workspace writes don't cause panics or data corruption

### Phase 3: MCP Tools Refactor

**Goal**: 8 tools with proper error handling, workspace_id-based lookup, file locking.

**Tasks**:
1. Refactor existing 4 tools to use mapping-based lookup
2. Add `workspace_discover` tool
3. Add `workspace_folders_reorder` tool
4. Add `workspace_status` tool
5. Add `workspace_open` tool
6. All tools use `lock::with_workspace_lock()` for writes
7. All tools wait for `zed --add`/`--reuse` exit code
8. Add `workspace_bootstrap` tool

**Tests**:
- Unit test for each tool: valid input → expected output
- Unit test: tool with missing mapping file → auto-discovery
- Unit test: concurrent tool calls → file lock serializes
- Integration test: `workspace_folders_add` adds to file AND Zed
- Integration test: `workspace_open` opens workspace in Zed

### Phase 4: File Watcher (Optional, Post-MVP)

**Goal**: Hook watches .code-workspace file for external changes, triggers File→Zed sync.

**Tasks**:
1. Add `notify` crate dependency to hook
2. Spawn watcher thread after discovery
3. On file change: check `last_sync_ts` to detect self-writes
4. If external change: trigger File→Zed sync via `zed --add` / `zed --reuse`

**Tests**:
- Integration test: external edit to .code-workspace → Zed adds folder
- Test: self-write does NOT trigger re-sync

### Phase 5: Hook Socket API + MCP Lifecycle (DONE)

**Goal**: Eliminate CLI binary dependency for workspace mutations. Fix channel mismatch bug
(MCP calling `zed` stable instead of `zed-preview`). Prevent stale MCP process accumulation.

**Problem**: `Command::new("zed")` always invoked the stable CLI, even for Zed Preview
workspaces. No cleanup when Zed crashed/restarted.

**Solution**: Option C — hook dylib exposes a Unix socket server inside Zed's process.
MCP connects via `HookClient`, sends JSON commands. Hook executes via `current_exe()`
which is inherently channel-correct. Falls back to channel-aware CLI if unavailable.

**New files created:**
1. `zed-prj-workspace-hook/src/socket_server.rs` — Unix socket server inside Zed
   - Socket path: `/tmp/zed-prj-workspace-{channel}-{pid}.sock`
   - JSON-line protocol: `ping`, `add_folders`, `reuse_folders`, `status`
   - Executes via `current_exe()` (inherently channel-correct)
   - `atexit()` cleanup for socket file
2. `zed-prj-workspace-hook/src/ffi/dispatch.rs` — macOS `dispatch_async_f` bindings
   (for Phase 2 OpenListener injection, not yet used)
3. `src/hook_client.rs` — Unix socket client + CLI fallback
   - `HookClient::connect(channel)` — finds socket via `find_hook_socket()`
   - `invoke_zed_add(path, channel, existing_root)` — hook-first + CLI fallback
   - `invoke_zed_reuse(paths, channel)` — hook-first + CLI fallback

**Files modified:**
4. `src/mapping.rs` — Added `zed_cli_command()`, `hook_socket_path()`, `find_hook_socket()`
5. `src/sync_engine.rs` — `invoke_zed_add/reuse` now delegate to `hook_client`
6. `zed-prj-workspace-mcp/src/main.rs` — All 4 CLI sites replaced with hook-first routing,
   added parent PID watchdog (5s check, exits if parent dies), stale process cleanup on startup
7. `zed-prj-workspace-mcp/Cargo.toml` — Binary renamed to `zed-prj-workspace-mcp`, added `libc`
8. `zed-prj-workspace-hook/src/lib.rs` — Starts socket server on init

**Fallback chain** (all invocation sites):
```
1. Hook socket (in-process, channel-correct, ~1ms)
2. Channel-aware CLI (zed-preview or zed, ~200ms)
3. Generic "zed" CLI (last resort)
```

**Verified live:**
- Hook socket responds to ping: `{"ok":true,"pid":99580,"channel":"preview"}`
- MCP `workspace_status` shows `hook_available: true`
- Parent PID watchdog active (checks every 5s)
- Stale process cleanup works (killed 6 orphaned processes on first run)

---

## 4. Test Methodology

### 4.1 Test Levels

| Level | What | How | Where |
|-------|------|-----|-------|
| **Unit** | Individual functions | `#[cfg(test)]` modules | Each `.rs` file |
| **Integration** | Multi-module workflows | `tests/` directory | `tests/integration.rs` |
| **End-to-end** | Full hook + MCP + Zed | Shell scripts in `xtask` | Manual + scripted |
| **Stress** | Concurrency correctness | Multi-threaded test harness | `tests/stress.rs` |

### 4.2 Test Fixtures

```
tests/fixtures/
├── sample.code-workspace       # Valid workspace file (3 folders, relative paths)
├── sample-absolute.code-workspace  # With absolute paths
├── sample-with-names.code-workspace  # With folder display names
├── sample-with-settings.code-workspace  # With settings block
├── empty.code-workspace        # { "folders": [] }
├── zed-preview.db              # Snapshot of real Zed DB (sanitized paths)
└── mapping.json                # Sample mapping file
```

### 4.3 Key Test Scenarios

**Concurrency tests** (critical):
```rust
#[test]
fn concurrent_file_writes_are_serialized() {
    // Spawn 10 threads all writing to same .code-workspace
    // Assert: file is never corrupted, all writes complete
}

#[test]
fn hook_debounce_per_workspace_id() {
    // Simulate rapid workspace writes for workspace_id=110
    // Interleaved with writes for workspace_id=94
    // Assert: exactly 1 sync per workspace_id after debounce
}

#[test]
fn self_write_detection() {
    // Write .code-workspace via hook
    // Immediately check should_sync_file_change()
    // Assert: returns false (self-write detected)
}
```

**Discovery tests**:
```rust
#[test]
fn discovery_mapping_file_first() {
    // Create mapping file + 2 .code-workspace files in roots
    // Assert: discovery uses mapping file, not scanning
}

#[test]
fn discovery_scan_fallback() {
    // Create 1 .code-workspace file, no mapping file
    // Assert: discovery finds file by scanning, creates mapping
}

#[test]
fn discovery_bootstrap() {
    // No .code-workspace files, no mapping
    // Assert: discovery creates both
}
```

**Sync correctness tests**:
```rust
#[test]
fn sync_detects_folder_reorder() {
    // DB has paths_order [2,0,1], file has order [0,1,2]
    // Assert: sync detects reorder, updates file order
}

#[test]
fn sync_conflict_both_changed() {
    // Set last_sync_ts to T
    // DB timestamp = T+1, file mtime = T+2
    // Assert: conflict detected, logged, DB preferred
}

#[test]
fn sync_preserves_unknown_fields() {
    // .code-workspace has "settings" and "extensions" blocks
    // Sync adds a folder
    // Assert: "settings" and "extensions" preserved in output
}
```

### 4.4 End-to-End Test Script

```bash
#!/bin/bash
# e2e_test.sh — requires Zed Preview running
set -euo pipefail

WORKSPACE_FILE="/tmp/test-e2e.code-workspace"
FOLDER1="/tmp/test-e2e-folder1"
FOLDER2="/tmp/test-e2e-folder2"

# Setup
mkdir -p "$FOLDER1" "$FOLDER2"
cat > "$WORKSPACE_FILE" << 'EOF'
{ "folders": [{ "path": "/tmp/test-e2e-folder1" }] }
EOF

# Test 1: MCP tool adds folder
echo "Test 1: workspace_folders_add"
# (invoke MCP tool via stdin/stdout)

# Test 2: Verify Zed opened the folder
echo "Test 2: Check Zed DB for added folder"
sqlite3 "$DB_PATH" "SELECT paths FROM workspaces WHERE paths LIKE '%test-e2e%';"

# Test 3: Add folder in Zed UI, verify .code-workspace updated
echo "Test 3: Zed→File sync"
# (manually add folder in Zed, wait for hook, check file)

# Cleanup
rm -rf "$FOLDER1" "$FOLDER2" "$WORKSPACE_FILE"
```

---

## 5. Migration Strategy (From Current POC)

### 5.1 Breaking Changes

| Change | Old Behavior | New Behavior | Migration |
|--------|-------------|-------------|-----------|
| Mapping storage | `project_name` in `.zed/settings.json` | `.zed/zed-project-workspace.json` | Auto-migrate: parse old `project_name`, write mapping file, reset `project_name` |
| DB lookup | `LIKE "%folder%"` | `find_by_id()` via mapping | Transparent (mapping file provides workspace_id) |
| Discovery order | Scan first | Mapping first | No user action needed |
| File write | Direct write | Lock + atomic rename | Transparent |

### 5.2 Auto-Migration on First Run

```rust
fn migrate_from_v1(roots: &[PathBuf], db: &ZedDbReader) -> Option<WorkspaceMapping> {
    for root in roots {
        let settings_path = root.join(".zed/settings.json");
        if let Ok(content) = fs::read_to_string(&settings_path) {
            // Check for old "{workspace_id}:{filename}" pattern in project_name
            if let Some(caps) = regex!(r#""project_name"\s*:\s*"(\d+):(.+)""#).captures(&content) {
                let workspace_id: i64 = caps[1].parse().ok()?;
                let filename = &caps[2];

                // Create new mapping file
                let mapping = WorkspaceMapping {
                    workspace_id,
                    workspace_file: filename.to_string(),
                    zed_channel: detect_zed_channel(),
                    last_sync_ts: None,
                };
                mapping.write(root).ok()?;

                // Reset project_name to just the display name
                let display_name = Path::new(filename)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(filename);
                // Update settings.json: replace "{id}:{file}" with just display name
                // ... (preserve formatting as much as possible)

                return Some(mapping);
            }
        }
    }
    None
}
```

---

## 6. Dependency Changes

### 6.1 New Crate Dependencies

```toml
# In workspace Cargo.toml [workspace.dependencies]
pathdiff = "0.2"       # Proper relative path computation
fs2 = "0.4"            # Cross-platform file locking (flock)
chrono = "0.4"          # Timestamp handling for last_sync_ts
```

### 6.2 Removed Dependencies (from hook)

- Hook no longer needs direct `rusqlite` — uses shared `ZedDbReader`
- (Actually, hook still links rusqlite transitively through shared lib)

---

## 7. Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Zed changes sqlite3 linking (static → bundled) | Low | Hook fails to find symbol | symbols.rs already has fallback chain |
| PR #46225 merges with different schema | Medium | Our mapping partially redundant | Graceful dual-path (Refinement #4) |
| MultiWorkspace ships to all users | High (6-12 months) | Our 1:1 model still works | Per-workspace_id design is compatible |
| Lock file left behind after crash | Low | Next write blocks forever | Use `try_lock` with timeout + stale lock detection |
| Zed Preview vs Stable DB confusion | Medium | Sync to wrong DB | `zed_channel` in mapping file |

---

## 8. Definition of Done

### Phase 1 (Shared Library):
- [ ] All 5 new modules compile and have unit tests
- [ ] `parse_workspace_paths()` exists in exactly 1 place
- [ ] `ZedDbReader::default_db_path()` detects channel from exe
- [ ] Path normalization handles `..` and `.` correctly
- [ ] Relative path computation produces `../sibling` correctly
- [ ] Diff detects reorder (not just membership change)
- [ ] File locking works under concurrent access

### Phase 2 (Hook):
- [ ] No `Mutex` on sqlite3 hot path (OnceLock or AtomicPtr)
- [ ] Per-workspace_id debounce (not global)
- [ ] Uses shared library for all DB access and file operations
- [ ] Creates mapping file on discovery (not project_name hack)
- [ ] Self-write detection prevents sync loops
- [ ] Hook discovery auto-migrates from v1 format

### Phase 3 (MCP):
- [ ] 8 tools implemented and documented
- [ ] All tools use mapping-based workspace lookup
- [ ] All tools use file locking for writes
- [ ] `zed --add` / `zed --reuse` exit codes checked
- [ ] `workspace_discover` returns complete state
- [ ] `workspace_status` shows diagnostic info
- [ ] `workspace_open` opens workspace from file

### Overall:
- [ ] `cargo test` passes (all unit + integration tests)
- [ ] `cargo clippy` clean
- [ ] E2E test script passes with running Zed Preview
- [ ] No regression in existing sync behavior
- [ ] Memory: no leaks (hook runs for hours)
- [ ] Performance: <1ms overhead per sqlite3 call on hot path
