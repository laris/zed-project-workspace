# Deploy & Test Guide

## Prerequisites

| Requirement | For MCP Server | For zed-workspace-sync-hook |
|---|---|---|
| Rust nightly toolchain | Yes | Yes |
| macOS arm64 (Apple Silicon) | Yes | Yes |
| SIP disabled | No | **Yes** |
| insert_dylib installed | No | **Yes** |
| Zed Preview installed | Recommended | **Yes** |

## Part 1: MCP Server (No SIP Required)

### 1.1 Build & Install

```fish
cd $HOME/codes/zed-project-workspace
cargo build -p zed-prj-workspace-mcp --release

# Install to ~/.mcp/bin (add to PATH if not already)
mkdir -p ~/.mcp/bin
cp target/release/zed-prj-workspace-mcp ~/.mcp/bin/
```

Add `~/.mcp/bin` to your PATH (in `~/.config/fish/config.fish` or equivalent):

```fish
fish_add_path ~/.mcp/bin
```

### 1.2 Unit Tests

```fish
cargo test
```

Expected: 25 passed, 0 failed.

### 1.3 Create a Test Workspace File

```fish
mkdir -p /tmp/test-ws/frontend /tmp/test-ws/backend

printf '{
  "folders": [
    { "path": "frontend" },
    { "path": "backend" }
  ]
}' > /tmp/test-ws/my.code-workspace
```

### 1.4 Test MCP Server Manually (JSON-RPC via stdin)

Start the server:

```fish
zed-prj-workspace-mcp 2>/dev/null
```

In another terminal, send an initialize request:

```fish
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}\n' | zed-prj-workspace-mcp 2>/dev/null
```

Expected: JSON response with `serverInfo` containing `"name":"zed-workspace-sync"`.

### 1.5 Register in Zed

Add to Zed's `settings.json` (Cmd+, in Zed, then edit JSON):

```json
{
  "context_servers": {
    "zed-workspace-sync": {
      "command": {
        "path": "zed-prj-workspace-mcp",
        "args": []
      }
    }
  }
}
```

Since `~/.mcp/bin` is in your PATH, Zed will find the binary by name — no absolute path needed.

After restart, the AI agent can call:
- `workspace_folders_list` — list folders in a `.code-workspace` file
- `workspace_folders_add` — add a folder to the file + invoke `zed --add`
- `workspace_folders_remove` — remove a folder from the file
- `workspace_folders_sync` — bidirectional sync with Zed DB

---

## Part 2: zed-workspace-sync-hook (Requires SIP Disabled)

### 2.1 Check SIP Status

```fish
csrutil status
```

If output says `enabled`, disable SIP:

1. **Shut down** Mac completely
2. **Hold power button** until "Loading startup options" appears
3. Select **Options** then continue
4. Open **Terminal** from Utilities menu
5. Run: `csrutil disable`
6. Restart

Verify after reboot:

```fish
csrutil status
# Expected: "System Integrity Protection status: disabled."
```

### 2.2 Build the Hook Dylib

```fish
cd $HOME/codes/zed-project-workspace/crates/zed-workspace-sync-hook
cargo build --release
```

Output: `crates/zed-workspace-sync-hook/target/release/libzed_workspace_sync_hook.dylib`

First build downloads the frida-gum devkit automatically (~30s).

### 2.3 Install insert_dylib

```fish
cd $HOME/codes-repos/gh-cocoa-xu__insert_dylib_rs
cargo install --path .
```

Verify:

```fish
insert_dylib_rs --help
```

### 2.4 Patch Zed Preview

```fish
cd $HOME/codes/zed-project-workspace
cargo run -p zed-workspace-sync-patcher -- \
  --zed-app "/Applications/Zed Preview.app" \
  --dylib (pwd)/crates/zed-workspace-sync-hook/target/release/libzed_workspace_sync_hook.dylib
```

Expected output:

```
[1/4] Backing up original binary...
[2/4] Injecting dylib load command...
  Trying insert_dylib_rs...
  Dylib injected via insert_dylib_rs
  Verified: LC_LOAD_WEAK_DYLIB present in binary
[3/4] Re-signing app with ad-hoc signature...
  App re-signed
[4/4] Verifying...
  Executable=/Applications/Zed Preview.app/Contents/MacOS/zed
Patched successfully!
Launch Zed normally — the hook will auto-discover the workspace file.
```

Verify the injection:

```fish
otool -L "/Applications/Zed Preview.app/Contents/MacOS/zed" | grep sqlite_hook
# Expected: .../libzed_workspace_sync_hook.dylib (compatibility version 0.0.0, current version 0.0.0, weak)
```

**Note:** `insert_dylib_rs --overwrite` does NOT overwrite in-place — it creates a `{binary}_patched` file. The patcher handles this by using `--output` and renaming the result.

### 2.5 Launch Zed

Just open Zed normally — no environment variables needed:

```fish
open -a "Zed Preview"
```

Or click Zed Preview in the Dock / Spotlight / Launchpad.

The hook will auto-discover the workspace file on the first folder change event.

### 2.6 Verify Hook is Running

```fish
# Log files use daily rotation with descriptive naming
tail -f ~/Library/Logs/Zed/zed-project-workspace-sync-mcp.$(date +%Y-%m-%d).log
```

Expected log output (Zed Preview):

```
=== zed-workspace-sync-hook v0.1.0 ===
PID: 12345
Executable: "/Applications/Zed Preview.app/Contents/MacOS/zed"
Log directory: $HOME/Library/Logs/Zed
Auto-discovery will run on first workspace write
Zed variant: Preview
Using DB: $HOME/Library/Application Support/Zed/db/0-preview/db.sqlite
Found sqlite3_prepare_v2 at NativePointer(0x...)
Hook installed: sqlite3_prepare_v2
Zed DB: $HOME/Library/Application Support/Zed/db/0-preview/db.sqlite
Event-driven workspace sync ready
```

For Zed Stable, the DB path would be `0-stable/db.sqlite` and variant would show `Stable`.

### 2.7 Test Auto-Discovery (Bootstrap — No Existing Workspace File)

1. Open a folder in Zed that has **no** `.code-workspace` file (e.g., `/tmp/test-ws/frontend`)
2. Use **File > Add Folder to Project** and add another directory (e.g., `/tmp/test-ws/backend`)
3. Watch the log:

```
Workspace write detected (state=2)
Discovery: workspace_id=84, roots=["/tmp/test-ws/frontend", "/tmp/test-ws/backend"]
Priority 3: Bootstrapping workspace file
Created /tmp/test-ws/frontend/frontend.code-workspace
Created /tmp/test-ws/frontend/.zed/settings.json
Created /tmp/test-ws/backend/.zed/settings.json
Auto-discovered sync target: /tmp/test-ws/frontend/frontend.code-workspace (workspace_id=84)
```

4. Verify the files were created:

```fish
cat /tmp/test-ws/frontend/frontend.code-workspace
# Expected: {"folders":[{"path":"."},{"path":"../backend"}]}

cat /tmp/test-ws/frontend/.zed/settings.json
# Expected: {"project_name":"84:frontend.code-workspace"}
```

### 2.8 Test Auto-Discovery (Existing Workspace File)

1. Place a `.code-workspace` file in a folder root:

```fish
printf '{"folders":[{"path":"."}]}' > /tmp/test-ws/frontend/frontend.code-workspace
```

2. Open that folder in Zed and add another folder
3. Watch the log:

```
Workspace write detected (state=2)
Discovery: workspace_id=84, roots=["/tmp/test-ws/frontend"]
Priority 1: Found single .code-workspace: /tmp/test-ws/frontend/frontend.code-workspace
Auto-discovered sync target: /tmp/test-ws/frontend/frontend.code-workspace (workspace_id=84)
Workspace write detected (state=1)
Zed event sync: +1 folders, -0 folders
  + /tmp/test-ws/backend
Workspace file updated: /tmp/test-ws/frontend/frontend.code-workspace
```

### 2.9 Test Event-Driven Sync (Ongoing)

After auto-discovery, subsequent folder adds/removes sync automatically:

1. In Zed, add another folder (e.g., `/tmp/test-ws/shared`)
2. Watch the log:

```
Workspace write detected (state=1)
Zed event sync: +1 folders, -0 folders
  + /tmp/test-ws/shared
Workspace file updated: /tmp/test-ws/frontend/frontend.code-workspace
```

3. Verify:

```fish
cat /tmp/test-ws/frontend/frontend.code-workspace
```

Expected: `shared` folder now appears in the `folders` array.

### 2.10 Test Folder Removal Sync

1. In Zed, right-click a folder root in the project panel and remove it
2. Watch the log — should show the removal detected and synced to file

### 2.11 Restore Original Zed Binary

```fish
cargo run -p zed-workspace-sync-patcher -- --restore "/Applications/Zed Preview.app"
```

Or manually:

```fish
cp "/Applications/Zed Preview.app/Contents/MacOS/zed.original" \
   "/Applications/Zed Preview.app/Contents/MacOS/zed"
```

---

## Part 3: After Zed Update

Each Zed Preview update replaces the binary. Re-patch with:

```fish
cd $HOME/codes/zed-project-workspace
./scripts/01_patch_zed_preview.sh
```

Or manually:

```fish
cargo run -p zed-workspace-sync-patcher -- \
  --zed-app "/Applications/Zed Preview.app" \
  --dylib (pwd)/crates/zed-workspace-sync-hook/target/release/libzed_workspace_sync_hook.dylib
```

---

## Troubleshooting

| Issue | Cause | Fix |
|---|---|---|
| `codesign` fails | SIP enabled | Disable SIP (see 2.1) |
| `insert_dylib` not found | Not installed | `cargo install --path /path/to/insert_dylib_rs` |
| Patcher says success but `otool -L` shows no dylib | `insert_dylib_rs --overwrite` bug | Fixed in patcher — uses `--output` + rename. Re-run patcher |
| Hook log empty (`zed-project-workspace-sync-mcp.*.log`) | Dylib not loaded | Verify patch: `otool -L "/Applications/Zed Preview.app/Contents/MacOS/zed"` should show `libzed_workspace_sync_hook.dylib` |
| Hook logs "ready" but no SQL detected | Hooking wrong sqlite3 | Fixed — hook now uses `Process::main_module()` for Zed's statically-linked sqlite3 |
| "Zed DB not found" in log | DB path mismatch | Zed Preview uses `0-preview/`, Zed Stable uses `0-stable/`. Hook auto-detects via executable path |
| Hook finds wrong workspace | Reading wrong DB (stable vs preview) | Fixed — hook detects "Zed Preview" in `current_exe()` path |
| Discovery returns empty paths | Parsing paths as JSON | Fixed — Zed stores paths as newline-separated text, not JSON arrays |
| "no such column: workspace_file_path" | Old sync query | Fixed — now queries by `workspace_id` instead |
| No auto-discovery | No workspace write events yet | Open a folder or switch tabs in Zed to trigger a write |
| Discovery keeps retrying | 30s cooldown between attempts | Wait for cooldown, check DB has workspace entries |
| Zed crashes on launch | Bad dylib | Restore: `cargo run -p zed-workspace-sync-patcher -- --restore "/Applications/Zed Preview.app"` |
| MCP server not responding | Not registered | Check Zed `settings.json` for `context_servers` config |
| Frida devkit download fails | Network issue | Manually download from `github.com/frida/frida/releases` and place in frida-gum-sys dir |
| Bootstrap creates wrong file | Multiple roots, unexpected primary | Check `.zed/settings.json` — first root alphabetically becomes primary |
| `init()` called multiple times | Zed spawns child processes | Fixed — `std::sync::Once` guard prevents duplicate initialization |

## Key Implementation Details (from debugging)

These are important findings discovered during real-world testing:

1. **Zed statically links sqlite3** — the hook must use `Process::main_module()` to find `sqlite3_prepare_v2` in Zed's binary, not `Module::load("libsqlite3.dylib")` (system library is unused by Zed)

2. **Zed Preview vs Stable use separate DBs** — `0-preview/db.sqlite` vs `0-stable/db.sqlite` under `~/Library/Application Support/Zed/db/`. The hook detects the variant from `current_exe()` path

3. **Paths are newline-separated text, not JSON** — the `paths` column in `workspaces` stores e.g. `/path/a\n/path/b`, not `["/path/a", "/path/b"]`

4. **Zed uses multi-line SQL** — e.g. `INSERT INTO\n  workspaces (...)` — the SQL pattern matcher handles this correctly

5. **`insert_dylib_rs --overwrite` is misleading** — it creates `{binary}_patched` instead of overwriting. The patcher uses `--output` + `fs::rename()` as a workaround

6. **`#[ctor]` runs multiple times** — Zed spawns child processes that inherit the dylib. `std::sync::Once` guard prevents duplicate initialization

7. **Query by workspace_id, not workspace_file_path** — the `workspaces` table has no `workspace_file_path` column. Sync queries use `WHERE workspace_id = ?`

## Verification Checklist

```fish
# 1. Unit tests (workspace crates)
cd $HOME/codes/zed-project-workspace
cargo test
# Expected: 25 passed

# 2. Unit tests (zed-workspace-sync-hook)
cd crates/zed-workspace-sync-hook && cargo test && cd ../..
# Expected: 22 passed

# 3. MCP server builds and installs
cargo build -p zed-prj-workspace-mcp --release
cp target/release/zed-prj-workspace-mcp ~/.mcp/bin/
# Expected: success

# 4. Hook builds
cd crates/zed-workspace-sync-hook && cargo build --release && cd ../..
# Expected: libzed_workspace_sync_hook.dylib

# 5. Patcher builds
cargo build -p zed-workspace-sync-patcher
# Expected: success

# 6. Hook logs appear after patching + launch
tail ~/Library/Logs/Zed/zed-project-workspace-sync-mcp.$(date +%Y-%m-%d).log
# Expected: startup banner with PID, Zed variant, DB path, "Event-driven workspace sync ready"

# 7. Auto-discovery works
# Open a folder in Zed → add another folder → check log for "Auto-discovered sync target"

# 8. .zed/settings.json created
# Check opened folder for .zed/settings.json with project_name field

# 9. Sync works on folder add
# Add folder in Zed → check .code-workspace file updates

# 10. MCP tools work from AI agent
# Ask AI: "list workspace folders for /path/to/my.code-workspace"
```
