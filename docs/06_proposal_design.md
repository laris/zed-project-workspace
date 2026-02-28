# Proposal: Workspace Identity & Sync Refactor

Date: 2026-02-27
Updated: 2026-02-28
Status: **IMPLEMENTED** — shared library, hook, MCP, and SDK refactor complete. See `07_implementation_plan.md` for status.

---

## Problem Statement

Zed has no native "workspace/project file" concept. Workspace identity is an opaque `workspace_id` integer in a SQLite DB, with the set of folder paths as the canonical identity key. There is no file-based, VCS-shareable, human-readable project manifest like VS Code's `.code-workspace`, JetBrains' `.idea/`, or Sublime's `.sublime-project`.

Our tool (`zed-project-workspace`) fills this gap by syncing between Zed's DB and a `.code-workspace` file. However, the current POC has several design problems:

1. **No stable workspace-to-file mapping**: We abuse `.zed/settings.json` `project_name` to store `"{workspace_id}:{filename}"`, which pollutes the Zed window title and conflicts with user intent.
2. **The title bar shows the hack**: Because `project_name` controls display name, users see `"97:zed-project-workspace.code-workspace"` instead of a clean project name.
3. **Folder order is lost**: We read only `paths` (lexicographic) and ignore `paths_order`.
4. **DB matching is fragile**: `LIKE "%folder%"` can match wrong workspaces.
5. **No human-friendly project name concept**: There's no clean way for users to name and identify their workspace/project.

---

## Design Goals

1. **Clean separation of concerns**: Workspace identity mapping should NOT pollute user-facing settings.
2. **Human-friendly project names**: Users should be able to name their workspace and see that name in Zed's UI.
3. **Stable bidirectional sync**: Changes in Zed (add/remove/reorder folders) should update `.code-workspace`, and vice versa.
4. **VS Code compatibility**: The `.code-workspace` file should be usable by VS Code/Cursor without modification.
5. **No Zed source modification required**: Work within what Zed's current extension points allow.

---

## Proposed Architecture

### Layer 1: Mapping File (`.zed/zed-project-workspace.json`)

**Purpose**: Machine-readable mapping between Zed's workspace_id and the `.code-workspace` file. Replaces the `project_name` hack.

**Location**: In each worktree root's `.zed/` directory (same as `settings.json`).

**Schema**:
```json
{
  "workspace_id": 110,
  "workspace_file": "my-project.code-workspace"
}
```

- `workspace_id`: Machine-local integer from Zed's SQLite DB.
- `workspace_file`: **Relative path** from the worktree root to the `.code-workspace` file.
  - Typical case (file in same root): `"my-project.code-workspace"`
  - File in subdirectory: `"config/my-project.code-workspace"`
  - File in parent/sibling: `"../my-project.code-workspace"`

**No absolute paths.** The mapping file sits at `{worktree_root}/.zed/zed-project-workspace.json`, so any relative path is resolved against the worktree root. If the entire workspace folder is moved, renamed, or cloned to another machine, the relative path still works. (Only `workspace_id` becomes stale, which is expected — it's re-discovered on next launch.)

**Why a separate file?**:
- Does not interfere with Zed settings parsing (Zed ignores unknown files in `.zed/`).
- Can be `.gitignore`'d (`workspace_id` is machine-specific; `workspace_file` is portable but redundant with auto-discovery).
- Does not pollute `project_name` or any user-facing setting.
- Can store additional sync metadata (last sync timestamp, sync direction preference) in the future.

**Discovery priority** (unchanged logic, cleaner storage):
1. Scan roots for `*.code-workspace` → if exactly one, auto-map.
2. Check `.zed/zed-project-workspace.json` for existing mapping.
3. Bootstrap: create `{primary_root_name}.code-workspace` + mapping file.

### Layer 2: project_name for Display (Optional, User-Controlled)

**Purpose**: Give users a clean, human-friendly name for their workspace in Zed's title bar and picker.

**Behavior**:
- On first sync/bootstrap, set `project_name` in `.zed/settings.json` of the **primary worktree** to the `.code-workspace` filename (without extension).
  - Example: `my-project.code-workspace` → `"project_name": "my-project"`
- **Only set if `project_name` is not already defined by the user**.
- **Never overwrite** a user-set `project_name`.
- This is a one-time suggestion, not a sync variable.

**Result**: The Zed title bar shows `"my-project — main.rs"` instead of `"zed-project-workspace, zed-patcher, zed-yolo-hook, ..."`.

**For multi-root workspaces**: Only set `project_name` on the **first worktree in user order** (from `paths_order`). Other worktrees keep their folder basename. This gives a clean title like `"my-project, zed-patcher, zed-yolo-hook, ..."` where the first name is the project identity.

### Layer 3: Correct Path Order Sync

**Read both `paths` AND `paths_order`** from Zed's DB to reconstruct user-visible order.

Implementation:
```rust
/// Reconstruct ordered paths from DB columns (equivalent to Zed's PathList::ordered_paths)
fn reconstruct_ordered_paths(paths: &[PathBuf], paths_order: &[usize]) -> Vec<PathBuf> {
    if paths_order.is_empty() || paths_order.len() != paths.len() {
        return paths.to_vec(); // fallback to lexicographic
    }
    let mut ordered = vec![PathBuf::new(); paths.len()];
    for (user_position, &lex_index) in paths_order.iter().enumerate() {
        if lex_index < paths.len() {
            ordered[user_position] = paths[lex_index].clone();
        }
    }
    ordered
}
```

Update all sync paths:
- Hook: `sync.rs` SELECT must include `paths_order`.
- Library: `workspace_db.rs` `WorkspaceRecord` must include `paths_order`.
- Diff: Compare ordered sequences, not just sets.
- File write: Write `.code-workspace` folders in user order.

### Layer 4: Deterministic Workspace Matching

Replace `find_by_folder()` (LIKE substring match) with Zed's own identity strategy:

```rust
/// Match workspace by exact canonical path set (same as Zed's workspace_for_roots)
fn find_by_paths(paths: &[PathBuf]) -> Option<WorkspaceRecord> {
    let mut sorted = paths.to_vec();
    sorted.sort();
    let serialized = sorted.iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    // SELECT ... FROM workspaces WHERE paths IS ?1
    query_by_exact_paths(&serialized)
}
```

Or, when we have the mapping file:
```rust
/// Direct lookup by workspace_id (from mapping file)
fn find_by_id(workspace_id: i64) -> Option<WorkspaceRecord> {
    // SELECT ... FROM workspaces WHERE workspace_id = ?1
}
```

**Preferred flow**: mapping file → workspace_id → direct DB lookup. Fall back to path matching only on first discovery.

---

## Sync Workflow (Revised)

### Event: Zed Writes to DB (Hook-Driven, Zed → File)

```
sqlite3_prepare_v2 intercepted
  ↓
Read mapping from .zed/zed-project-workspace.json
  → workspace_id known? → Query DB by workspace_id
  → workspace_id unknown? → Run discovery, create mapping
  ↓
Query DB: SELECT paths, paths_order FROM workspaces WHERE workspace_id = ?
  ↓
Reconstruct user-ordered path list
  ↓
Read .code-workspace file
  ↓
Diff (ordered sequence comparison):
  - Added folders → append to .code-workspace
  - Removed folders → remove from .code-workspace
  - Reordered → update .code-workspace folder order
  ↓
Write updated .code-workspace (preserve extra fields, settings, extensions)
```

### Event: .code-workspace File Changed (MCP/Manual, File → Zed)

```
User or tool edits .code-workspace
  ↓
Read .code-workspace: resolve all folder paths
  ↓
Read mapping → workspace_id → Query DB for current Zed state
  ↓
Diff:
  - File has folders not in Zed → zed --add <path> for each
  - File removed folders from Zed → Mark pending (or zed --reuse for full reconcile)
  - File reordered → Currently no CLI support (pending Zed restart)
  ↓
Update Zed via CLI
```

---

## Addressing the Core Questions

### Q: How does a user identify their workspace?

**Answer**: By the `.code-workspace` filename, which also becomes the `project_name` displayed in Zed's title bar.

| What | Where | Example |
|------|-------|---------|
| Machine identity | `workspace_id` in SQLite | `110` |
| File identity | `.code-workspace` file path | `~/codes/zed-project-workspace/my-project.code-workspace` |
| Human identity | `project_name` in `.zed/settings.json` | `"my-project"` |
| Mapping | `.zed/zed-project-workspace.json` | `{ "workspace_id": 110, "workspace_file": "my-project.code-workspace" }` |

### Q: How to show a static project name in Zed UI?

**Answer**: Set `project_name` in `.zed/settings.json` of the primary worktree. This is the ONLY mechanism Zed provides for custom display names. Our tool sets it once on bootstrap (from `.code-workspace` filename), then the user owns it.

### Q: What if Zed's workspace_id changes?

Zed's workspace_id changes when:
- You open a completely different set of folders (new workspace created).
- Zed DB is reset.

It does NOT change when:
- Adding/removing folders in a running workspace (UPSERT preserves ID).
- Restarting Zed with same folders (path matching reuses ID).

Our mapping file captures the workspace_id at discovery time. If it becomes stale (path set changed), re-discovery is triggered.

### Q: How to sync the workspace_id ↔ .code-workspace mapping?

**Answer**: The mapping is **machine-local** (workspace_id is per-machine). The `.code-workspace` file is **portable/shareable**. They meet in `.zed/zed-project-workspace.json` which is `.gitignore`'d.

```
Portable (VCS):     .code-workspace  ←→  .zed/settings.json (project_name)
Machine-local:      .zed/zed-project-workspace.json (workspace_id ↔ file mapping)
Zed internal:       SQLite DB (workspace_id, paths, paths_order, session state)
```

---

## Implementation Plan

### Phase 1: Mapping File Migration (Breaking Change)

1. Create `.zed/zed-project-workspace.json` schema and read/write functions.
2. Migrate discovery logic from `project_name` parsing to mapping file.
3. On first run with old `project_name` format, auto-migrate:
   - Parse `"{id}:{filename}"` from `project_name`.
   - Write mapping file.
   - Reset `project_name` to just the display name (filename without extension).
4. Update hook discovery to prefer mapping file.

### Phase 2: Order-Aware Sync

1. Add `paths_order` to `WorkspaceRecord`.
2. Update all DB queries to SELECT `paths_order`.
3. Implement `reconstruct_ordered_paths()`.
4. Update diff logic to compare ordered sequences.
5. Update `.code-workspace` writer to preserve user order.

### Phase 3: Deterministic DB Matching

1. Replace `find_by_folder()` with `find_by_id()` (using mapping file).
2. Add `find_by_paths()` as fallback (exact canonical match).
3. Remove LIKE-based matching.

### Phase 4: JSONC Support

1. Switch `.code-workspace` parsing from `serde_json` to a JSONC-capable parser.
2. Preserve comments where possible (or document that rewrite strips comments).

### Phase 5: Relative Path Improvement

1. Implement true relative path computation (with `..` segments).
2. Write relative paths to `.code-workspace` when workspace file is co-located with roots.

---

## File Layout After Refactor

```
~/codes/my-project/                          # Primary worktree root
├── .zed/
│   ├── settings.json                        # Zed settings (project_name: "my-project")
│   └── zed-project-workspace.json           # Our mapping (workspace_id ↔ file)
├── my-project.code-workspace                # VS Code compatible workspace file
├── src/
│   └── ...
│
~/codes/other-folder/                        # Secondary worktree root
├── .zed/
│   ├── settings.json                        # (no project_name needed)
│   └── zed-project-workspace.json           # Same mapping (redundant but robust)
└── ...
```

**`.gitignore` additions**:
```
.zed/zed-project-workspace.json    # Machine-local mapping (workspace_id is per-machine)
```

**VCS-committed files**:
```
my-project.code-workspace          # Portable workspace definition
.zed/settings.json                 # Shared Zed settings (including project_name)
```

---

## Open Questions for Discussion

1. **Should we write the mapping file to ALL worktree roots or just the primary?**
   - All roots: more robust (any root can be discovery entry point), but redundant.
   - Primary only: simpler, but fragile if primary root changes.
   - **Recommendation**: All roots (matches current behavior with `project_name`).

2. **Should `project_name` be set automatically or left to the user?**
   - Auto-set: better UX out of the box (title bar immediately shows project name).
   - User-only: no side effects, but title bar shows ugly comma-separated folder list.
   - **Recommendation**: Auto-set on bootstrap only, never overwrite user value.

3. **Should we store sync metadata in the `.code-workspace` file itself?**
   - VS Code preserves unknown top-level keys, so we could add `"zed_project_workspace": {...}`.
   - Pro: Single file, no need for mapping file in VCS-committed form.
   - Con: Pollutes shared file, might not survive all editors/formatters.
   - **Recommendation**: Keep mapping separate in `.zed/` for clean separation.

4. **What about Zed Preview vs Zed stable having different DBs?**
   - DB path is `~/Library/Application Support/Zed/db/0-preview/db.sqlite` vs `0-stable/db.sqlite`.
   - workspace_ids are independent between channels.
   - **Recommendation**: Detect running Zed channel (from process path) and use correct DB. Store channel in mapping file.

5. **Should we support `zed --reuse` for file-driven removals?**
   - This replaces the entire workspace (disruptive but effective).
   - **Recommendation**: Offer as explicit "reconcile" mode, not default behavior. Default = additions only via `--add`, removals pending restart.

---

## CRITICAL: Zed PR #46225 — Native `.code-workspace` Support

**Status**: Open PR by `coopbri`, closes issue #9459 (27 comments, highly requested).

PR #46225 adds **native `.code-workspace` file support** to Zed:
- New file: `crates/workspace/src/workspace_file.rs` — parses `.code-workspace` JSON
- New DB columns: `workspace_file_path TEXT`, `workspace_file_kind TEXT` added to `workspaces` table
- New enum: `SerializedWorkspaceLocation::LocalFromFile { workspace_file_path, workspace_file_kind }`
- CLI support: `zed my-project.code-workspace` opens all folders
- Recent projects: workspace file path shown in Open Recent picker
- Future: `WorkspaceFileKind::ZedWorkspace` for native `.zed-workspace` format (stubbed but not implemented)
- Deferred: settings translation, `name` field, extension recommendations, file watch/reload

**Impact on our project**: If PR #46225 merges, Zed will natively:
1. Store the `.code-workspace` file path in its DB (`workspace_file_path` column)
2. Open workspace files from CLI, File menu, and drag-and-drop
3. Show workspace file in Open Recent

**What PR #46225 does NOT do** (our value-add remains):
- No live sync (file changes do not auto-update running Zed workspace)
- No Zed→file sync (adding folders in Zed does not update `.code-workspace`)
- No settings translation
- No `name` field usage
- No file watcher/hot-reload

**Strategy (dual-path)**:
- **If PR #46225 merges**: Read the new `workspace_file_path` DB column for mapping (graceful fallback if absent). Use `zed my-project.code-workspace` for opening instead of per-folder `--add`.
- **If PR #46225 does NOT merge**: Our approach already covers and exceeds its scope. See next section.

---

## "PR-Independent" Design: Achieving the Same Goals Without Zed Source Changes

PR #46225 provides 4 capabilities. Here's how we achieve each **entirely from hook + MCP + wrapper**, with no Zed source modification:

### Capability 1: Open `.code-workspace` from CLI

**PR #46225**: `zed my-project.code-workspace` → Zed parses file, opens folders.

**Our approach**: A thin wrapper command.

```bash
# ~/.local/bin/zed-workspace (or shell function)
#!/bin/bash
# Parse .code-workspace, extract folder paths, open in Zed
WORKSPACE_FILE="$1"
FOLDERS=$(jq -r '.folders[].path' "$WORKSPACE_FILE" | while read -r rel; do
    # Resolve relative paths against workspace file location
    DIR=$(dirname "$WORKSPACE_FILE")
    echo "$(cd "$DIR" && realpath "$rel")"
done)
exec zed $FOLDERS
```

Or better — a Rust binary in our `xtask` or a standalone CLI:
```
zed-prj open my-project.code-workspace
```
This reads the `.code-workspace`, resolves all folder paths, and calls `zed <folder1> <folder2> ...`. The first invocation creates the workspace; subsequent ones reuse it (Zed's own path-matching logic).

**Advantage over PR #46225**: We can also do `--add` mode (add to existing window) and `--reuse` mode (replace existing window).

### Capability 2: macOS File Association (Open from Finder)

**PR #46225**: Registers `.code-workspace` file type in Zed's Info.plist.

**Our approach**: Register our wrapper as a file handler.

Option A: Create a minimal `.app` bundle (via `xtask`) that:
1. Receives the `.code-workspace` file path via macOS open event
2. Parses folders, invokes `zed <folders...>`

Option B: Use `duti` or `swda` to associate `.code-workspace` with our handler.

Option C: Skip — most users open from terminal or MCP anyway.

**Recommendation**: Option A if we want full Finder integration, Option C for now (lower priority).

### Capability 3: Workspace File in "Open Recent" Picker

**PR #46225**: Stores `workspace_file_path` in DB → Recent Projects picker shows it.

**Our approach**: We cannot modify Zed's Recent Projects UI. But we provide equivalent UX through:

1. **MCP `workspace_discover` tool**: AI agents can query "what workspace am I in?" and get the `.code-workspace` path.
2. **`project_name` display**: Set `project_name` = workspace filename on bootstrap → the Recent Projects picker shows `"my-project"` instead of a comma-separated folder list. Users recognize their workspace by name.
3. **MCP `workspace_status` tool**: Shows full state for debugging.

**Key insight**: The Recent Projects picker already shows workspace entries (with all folder paths). By setting `project_name` on the primary worktree, the display name becomes human-friendly WITHOUT needing a DB column.

### Capability 4: DB ↔ File Mapping

**PR #46225**: New DB columns `workspace_file_path`, `workspace_file_kind`.

**Our approach**: `.zed/zed-project-workspace.json` mapping file.

| Aspect | PR #46225 | Our Approach |
|--------|-----------|--------------|
| Storage | DB column (absolute path) | File in `.zed/` (relative path) |
| Survives folder move | No (absolute path breaks) | Yes (relative path) |
| VCS-shareable | No (DB is local) | Partially (relative path is portable, workspace_id is local) |
| Multi-machine | Not designed for it | workspace_file is portable; workspace_id re-discovered per machine |
| Requires Zed change | Yes (DB migration) | No |

**Our mapping file is actually better** because:
- Relative paths survive folder moves
- The file can be VCS-committed (minus workspace_id) for team sharing
- No DB migration needed — works with any Zed version

### Capability 5 (Beyond PR #46225): Bidirectional Live Sync

**PR #46225 does NOT provide this at all.** This is our core value proposition.

| Direction | Trigger | Mechanism |
|-----------|---------|-----------|
| **Zed → File** | User adds/removes folder in Zed UI | Hook detects workspace DB write → reads paths + paths_order → updates `.code-workspace` |
| **File → Zed** | User/tool edits `.code-workspace` | MCP sync tool reads file → computes diff → `zed --add` for additions, `zed --reuse` for full reconcile |
| **File → Zed (auto)** | `.code-workspace` file changes on disk | Hook watches file mtime (NEW) → triggers File→Zed sync |

### Capability 6 (Beyond PR #46225): File Watch / Hot-Reload

PR #46225 explicitly defers this. We can implement it in the hook:

```
Hook init:
  1. Discover .code-workspace file path
  2. Record file mtime
  3. On each workspace-write event (already triggered by sqlite3 hook):
     - Check if .code-workspace mtime changed since last sync
     - If yes: File→Zed sync (file is newer, treat as source of truth)
     - If no: Zed→File sync (DB is newer, normal flow)
```

Or more robustly, use a separate thread with `notify` crate to watch the `.code-workspace` file for changes and trigger File→Zed sync immediately.

### Summary: PR-Independent Feature Matrix

| Feature | PR #46225 | Our Approach (no Zed changes) | Gap? |
|---------|-----------|------------------------------|------|
| Open `.code-workspace` from CLI | Native `zed file.cw` | Wrapper `zed-prj open file.cw` | Minor (extra command) |
| Finder file association | Native | Custom `.app` bundle | Minor (can build) |
| Recent Projects shows workspace | DB column + UI | `project_name` display override | Partial (name only, not file path) |
| DB ↔ File mapping | DB columns | `.zed/` mapping file (relative paths) | **Better** (relative, portable) |
| Zed→File live sync | Not implemented | Hook-driven | **We provide, they don't** |
| File→Zed live sync | Not implemented | MCP + file watch | **We provide, they don't** |
| Order-aware sync | Not implemented | paths_order reconstruction | **We provide, they don't** |
| File hot-reload | Deferred | notify watcher in hook | **We provide, they don't** |

**Conclusion**: Even if PR #46225 never merges, our approach provides a **superset** of its capabilities. The only gap is native UI integration (Recent Projects showing the file path), which is a cosmetic difference bridged by `project_name`.

---

## CRITICAL: Zed MultiWorkspace (Merged, Active Development)

**This is more impactful than PR #46225 for our project.**

PR #48800 ("Re-add MultiWorkspace"), merged 2026-02-12 (+9283/-4093 lines, 100 files), introduces a **fundamental architectural change** to Zed: a single window can now contain **multiple independent Workspaces**, each with its own project, worktrees, and session state. This is actively being refined (PRs #48820, #49380, #49995, #50065, #50179 all merged in Feb 2026).

### What MultiWorkspace Is

```rust
pub struct MultiWorkspace {
    window_id: WindowId,
    workspaces: Vec<Entity<Workspace>>,     // Multiple workspaces in one window!
    active_workspace_index: usize,
    sidebar: Option<Box<dyn SidebarHandle>>,
    sidebar_open: bool,
    // ...
}
```

**Key behaviors**:
- `MultiWorkspace` is now the **root view** of each Zed window (above `Workspace`)
- Each `Workspace` in the list has its own `workspace_id`, its own set of worktree roots, its own session state
- A **sidebar** lets users switch between workspaces within one window
- Actions like `NewWorkspaceInWindow`, `NextWorkspaceInWindow`, `PreviousWorkspaceInWindow` navigate between them
- Each sub-workspace is independently serialized to DB (each gets its own `workspace_id`)
- A new `MultiWorkspaceState` is persisted: `{ active_workspace_id, sidebar_open }`
- Gated behind `AgentV2FeatureFlag` (staff-only for now)

Source: `crates/workspace/src/multi_workspace.rs`

### Impact on Our Project

This changes the fundamental model:

**Before MultiWorkspace**: One window = one workspace_id = one set of worktree roots = one `.code-workspace` file.

**After MultiWorkspace**: One window = **multiple** workspace_ids = **multiple** sets of worktree roots. Each sub-workspace could map to a different `.code-workspace` file, or they could all be grouped under one "super workspace" file.

**Concrete implications**:

1. **Our hook's `query_latest_workspace()`** picks the most recently written workspace — but now multiple workspaces in the same window write to DB. We need to handle the multi-workspace case.

2. **Our mapping file** (`.zed/zed-project-workspace.json`) maps ONE workspace_id ↔ ONE `.code-workspace` file. With MultiWorkspace, a window could have several workspace_ids. We need to decide:
   - Option A: One `.code-workspace` file per sub-workspace (each sub-workspace is independent)
   - Option B: One `.code-workspace` file per window, with sections for each sub-workspace
   - Option C: Ignore MultiWorkspace for now (it's flag-gated, staff-only)

3. **`project_name`** currently identifies one worktree. In MultiWorkspace, the sidebar shows workspace names. We should investigate if there's a workspace-level name concept.

4. **Serialization changes**: `MultiWorkspaceState` stores `active_workspace_id` and `sidebar_open`. New DB operations (`write_multi_workspace_state`, `delete_workspace_by_id`) exist. Our hook might need to watch for these too.

### Recommended Strategy

**Short-term**: Ignore MultiWorkspace (it's flag-gated to staff accounts). Our design works for the single-workspace-per-window case, which is what all non-staff users have.

**Medium-term**: When MultiWorkspace ships to all users, adapt:
- Each sub-workspace in a MultiWorkspace window maps to its own `.code-workspace` file independently
- Our hook handles multiple workspace_ids per window
- Discovery uses the active workspace (from `MultiWorkspaceState.active_workspace_id`)

**Long-term**: Consider a "meta-workspace" concept — a `.code-workspace` file that describes the MultiWorkspace layout (which sub-workspaces exist, which is active, sidebar state). This would be a Zed-specific extension beyond VS Code compatibility.

---

## Zed Community Feature Requests (GitHub Issues)

| Issue/Discussion | Title | Status | Key Insight |
|---|---|---|---|
| [#9459](https://github.com/zed-industries/zed/issues/9459) | Support opening `.code-workspace` files | Open (27 comments) | Most-requested; PR #46225 addresses it |
| [#15120](https://github.com/zed-industries/zed/issues/15120) | Multi-root workspaces | Closed | Already implemented; users want workspace **file** persistence |
| [#5583](https://github.com/zed-industries/zed/issues/5583) | Persist folders added to project (save workspace) | Closed | Core request: "save workspace" like VS Code |
| [#32850](https://github.com/zed-industries/zed/discussions/32850) | Add support for `.code-workspace` files | Discussion | Community design discussion; mentions settings, extensions |
| [#39292](https://github.com/zed-industries/zed/discussions/39292) | Add Workspace Support for Managing Multiple Project Folders | Discussion | Users find multi-window workflow unintuitive |

**Community consensus**: The Zed community strongly wants:
1. A file-based workspace definition (`.code-workspace` or `.zed-workspace`)
2. Bidirectional sync between the file and Zed's runtime state
3. Workspace-level settings (not just per-worktree)
4. Cross-editor compatibility with VS Code

---

## Hook Refactor: Better Interception Points

### Current Approach: `sqlite3_prepare_v2` Interception

**Problems**:
- Very low-level; intercepts ALL SQL queries Zed makes (hot path)
- `Mutex<Option<PrepareV2Fn>>` for the original function pointer — lock on every SQL call
- Must parse SQL text to detect workspace writes
- Cannot access the actual workspace data being written (only the SQL text)
- No way to know WHICH workspace_id is being updated (parameters not accessible at prepare time)

### Proposed: Hook `save_workspace()` Directly

**Target**: `crates/workspace/src/persistence.rs:1323` — `WorkspaceDb::save_workspace()`

This is the single function that writes ALL workspace state to DB. It receives a `SerializedWorkspace` struct containing:
- `workspace_id`
- `paths` (the full PathList with ordering)
- `docks`, `session_id`, `window_id`
- `workspace_file_path` (from PR #46225, when merged)

**Advantages over sqlite3_prepare_v2**:
- Higher abstraction level — works with typed data, not raw SQL
- Called only on workspace saves (not every SQL query)
- Direct access to workspace_id and all paths
- No SQL parsing needed
- Stable across Zed versions (core persistence function)

**Hookability**: This is an async method on `WorkspaceDb`. The actual write happens inside a closure passed to `self.write()`. We could:
- Option A: Hook the `save_workspace` symbol directly (Frida-Gum can intercept Rust functions by mangled name)
- Option B: Continue intercepting sqlite3 but use a more targeted SQL filter (the INSERT INTO workspaces pattern is stable)
- Option C: Hook `serialize_workspace_internal()` in `workspace.rs:5944` (the caller that triggers saves)

**Recommendation**: Option B (improved sqlite3 hook) is most practical — Rust mangled names change with compiler versions, making Option A fragile. But we should:
1. Replace `Mutex<Option<PrepareV2Fn>>` with `OnceLock<PrepareV2Fn>` or `AtomicPtr` (eliminate hot-path lock)
2. Extract workspace_id from the SQL parameters (bind values) when possible, not just the SQL text
3. Also match DELETE statements on workspaces table

### Alternative: Monitor Zed's Event System

**Target**: `Project::Event::WorktreeAdded/Removed/OrderChanged` in `crates/project/src/project.rs:325-358`

These events fire immediately when worktrees change (before DB write). Zed's own workspace subscribes to them at `workspace.rs:1360-1385` to trigger serialization.

**Why this matters**: If we could hook the event emission, we'd get:
- Real-time notification of worktree changes
- Access to WorktreeId
- Event type (add/remove/reorder) without having to diff

**Practical difficulty**: GPUI events use trait-based dispatch. Hooking requires intercepting the `emit()` call on the specific `Entity<Project>`, which is deeply internal to GPUI. Not practical with Frida-Gum alone.

---

## MCP Tools Refactor: New Tool Design

### Current Tools

| Tool | What It Does | Limitation |
|---|---|---|
| `workspace_folders_list` | Lists folders from file + optional DB comparison | LIKE-based DB matching; no workspace_id |
| `workspace_folders_add` | Adds folder to file + `zed --add` | Fire-and-forget zed CLI; no error checking |
| `workspace_folders_remove` | Removes folder from file | No Zed-side removal (pending) |
| `workspace_folders_sync` | Full sync via sync_engine | Uses find_by_folder (imprecise) |

### Proposed New/Refactored Tools

**1. `workspace_discover` (NEW)**
```
Input:  { workspace_file?: string, folder_path?: string }
Output: { workspace_id, workspace_file, paths, paths_order, project_name, zed_channel }
```
Runs the full discovery chain: find DB record + workspace file + mapping. Returns the complete mapping state. Essential for debugging and for other tools to use as a prerequisite.

**2. `workspace_folders_list` (REFACTORED)**
```
Input:  { workspace_file: string }
Output: { file_folders: [...], db_folders: [...], in_sync: bool, order_matches: bool, sync_direction: string }
```
Changes:
- Use mapping file for DB lookup (not LIKE match)
- Compare ordered sequences, not just sets
- Report order mismatch separately from membership mismatch
- Show which folders are added/removed/reordered

**3. `workspace_folders_add` (REFACTORED)**
```
Input:  { workspace_file: string, folder_path: string, position?: number }
Output: { success: bool, zed_add_result?: string }
```
Changes:
- Wait for `zed --add` and report exit code
- Support `position` for ordered insertion
- Update mapping file if workspace_id changes

**4. `workspace_folders_remove` (REFACTORED)**
```
Input:  { workspace_file: string, folder_path: string, reconcile?: bool }
Output: { success: bool, pending_restart: bool }
```
Changes:
- If `reconcile=true`, use `zed --reuse` with remaining folders
- If `reconcile=false` (default), remove from file only, report pending_restart

**5. `workspace_folders_reorder` (NEW)**
```
Input:  { workspace_file: string, order: [string, ...] }
Output: { success: bool }
```
Reorder folders in `.code-workspace` file. Essential since order is a first-class property.

**6. `workspace_status` (NEW)**
```
Input:  { workspace_file?: string }
Output: { workspace_id, zed_channel, db_path, mapping_file, project_name, last_sync, zed_running: bool }
```
Diagnostic tool showing the full state of the sync system. Useful for debugging.

**7. `workspace_bootstrap` (NEW)**
```
Input:  { folder_paths: [string, ...], workspace_name?: string }
Output: { workspace_file: string, mapping_files: [string, ...] }
```
Creates a new `.code-workspace` file + mapping files + optionally sets `project_name`. The "create workspace" entry point.

---

## Codebase Refactor: Cross-Cutting Issues

### 1. Deduplicate `parse_workspace_paths()`

Currently defined **3 times** (identical):
- `src/workspace_db.rs:164`
- `zed-prj-workspace-hook/src/discovery.rs:345`
- `zed-prj-workspace-hook/src/sync.rs:249`

**Fix**: Make `workspace_db::parse_workspace_paths()` public, remove the other two.

### 2. Unify DB Path Discovery

Currently **two divergent implementations**:
- `workspace_db::default_db_path()` — prefers Preview > Stable (no exe detection)
- `discovery::find_workspace_db()` — detects Preview vs Stable from executable path (correct)

**Fix**: Move exe-based detection into the shared library. Accept optional channel hint.

### 3. Hook Should Use Shared Library

Currently the hook crate (`discovery.rs`, `sync.rs`) uses raw `rusqlite` directly, while the MCP crate uses `ZedDbReader`.

**Fix**: Make the hook use `ZedDbReader` too. This eliminates duplicate DB access code and ensures consistent query patterns.

### 4. Fix Hot-Path Performance

`sqlite3_prepare_v2` detour acquires a `Mutex` on every call to get the original function pointer.

**Fix**: Replace `Mutex<Option<PrepareV2Fn>>` with `OnceLock<PrepareV2Fn>` or `AtomicPtr<()>` — set once at hook install, then lock-free reads forever.

### 5. Path Normalization

No path canonicalization anywhere. Paths like `/a/../b` and `/b` are treated as different.

**Fix**: Add `normalize_path()` utility that resolves `.` and `..` components (without following symlinks). Use in `workspace_file::resolve()`, `paths_equal()`, and `diff_folders()`.

### 6. Fix `pathdiff_relative()`

Current implementation only handles child-of-base case (`strip_prefix`). Sibling directories fall back to absolute paths.

**Fix**: Use the `pathdiff` crate or implement proper relative path computation with `..` segments.

### 7. Remove Dead Code

- `SELF_WRITE_GENERATION` in `sync.rs` — incremented but never read
- `find_by_pattern()` in `symbols.rs` — generic but unused

### 8. Fix `invoke_zed_add()`

Fire-and-forget `spawn()` with no exit code checking.

**Fix**: Wait for child process, check exit status, return error if failed.

---

## Concurrency & Lifecycle Management

### Runtime Model (Verified on Live System)

- **1 Zed process** hosts ALL windows — the cdylib hook is loaded ONCE, shared across all windows
- **N MCP processes** — Zed spawns one `zed-workspace-sync` per workspace/window (observed: 11 MCP processes for 2 active windows due to stale processes)
- **1 SQLite DB** — WAL mode, concurrent reads safe, writes serialized by SQLite

### Hook Lifecycle

The hook cdylib is:
1. Loaded at Zed startup (via `DYLD_INSERT_LIBRARIES` or `insert_dylib` patching)
2. `ctor` runs `init()` → finds sqlite3 symbol → installs detour
3. Lives for the entire Zed process lifetime (never unloaded)
4. Handles ALL workspace writes for ALL windows — must dispatch per-workspace_id

**Critical design rule**: The hook is a SINGLE instance handling MULTIPLE workspaces. It MUST NOT assume "one workspace per process". Every piece of state (discovery, debounce, sync targets) must be keyed by workspace_id.

### MCP Lifecycle

Each MCP server is:
1. Spawned by Zed when a workspace/window initializes MCP context servers
2. Runs as a separate process communicating via stdio
3. Dies when Zed closes the stdio pipe (window close or Zed quit)
4. Stale processes: Zed sometimes doesn't clean up → use parent PID liveness check

**Critical design rule**: Multiple MCP processes may operate on the SAME `.code-workspace` file (if two windows share roots). All file writes MUST use advisory file locks.

### Concurrency Solutions

| Problem | Solution |
|---------|----------|
| Hook fires for wrong workspace | Per-workspace_id dispatch + debounce map |
| Concurrent file writes (hook + MCP) | Advisory file lock (`flock`) + atomic rename |
| Self-write loop | `last_sync_ts` in mapping file |
| Stale MCP processes | Exit on stdin EOF + parent PID check |
| DB read during write | WAL mode (inherent snapshot isolation) |
| Multiple MCP tool calls | File lock serializes all writes to same `.code-workspace` |

See `07_implementation_plan.md` §1 for detailed implementation.

---

## Refinements After Review (Post-Discussion)

After reviewing Zed's PRs (#46225, #48800 MultiWorkspace, #49380, #49995), community issues (#9459, #15120, #5583, #32850, #39292), and our full codebase analysis, these refinements to the proposal:

### 1. Fix Discovery Priority Order

The proposal had:
```
1. Scan roots for *.code-workspace → if exactly one, auto-map.
2. Check .zed/zed-project-workspace.json for existing mapping.
3. Bootstrap: create new file + mapping.
```

**Problem**: Scanning first is wrong. If there are multiple `.code-workspace` files in the roots (e.g. one per-subfolder), scanning picks arbitrarily. The mapping file IS the authoritative source once established.

**Fixed order**:
```
1. Check .zed/zed-project-workspace.json → if mapping exists AND file resolves, use it.
2. Scan roots for *.code-workspace → if exactly one, auto-create mapping.
3. Bootstrap: create new file + mapping.
```

Mapping file is authoritative. Scanning is only for first-time discovery.

### 2. Enrich Mapping File Schema for Forward-Compatibility

Original:
```json
{ "workspace_id": 110, "workspace_file": "my-project.code-workspace" }
```

**Refined** (add optional fields for robustness):
```json
{
  "workspace_id": 110,
  "workspace_file": "my-project.code-workspace",
  "zed_channel": "preview",
  "last_sync_ts": "2026-02-27T11:36:47Z"
}
```

- `zed_channel`: Which Zed DB this workspace_id belongs to (`"preview"` / `"stable"`). Prevents cross-channel confusion when both are installed.
- `last_sync_ts`: Last successful sync timestamp. Used for conflict detection (compare against DB `timestamp` and file `mtime`).
- Both are optional — absent = unknown, trigger re-discovery.

This prepares for MultiWorkspace (medium-term) where we might add:
```json
{
  "workspace_id": 110,
  "workspace_file": "my-project.code-workspace",
  "zed_channel": "preview",
  "multi_workspace_index": 0
}
```

### 3. Conflict Detection Strategy (Was Missing)

The proposal mentioned conflict handling but didn't define the algorithm:

```
On sync trigger:
  db_ts  = workspaces.timestamp for this workspace_id
  file_ts = .code-workspace file mtime
  last_ts = mapping.last_sync_ts

  If db_ts > last_ts AND file_ts <= last_ts:
    → Zed changed, file didn't. Zed→File sync.
  If file_ts > last_ts AND db_ts <= last_ts:
    → File changed, Zed didn't. File→Zed sync.
  If db_ts > last_ts AND file_ts > last_ts:
    → CONFLICT. Both changed since last sync.
    → Default: log warning, prefer Zed (DB is live state).
    → MCP tool: report conflict, let user choose.
  If db_ts <= last_ts AND file_ts <= last_ts:
    → Nothing changed. Skip.
```

After each successful sync, update `mapping.last_sync_ts` to `max(db_ts, file_ts)`.

### 4. PR #46225 Coexistence (Graceful Dual-Path)

If PR #46225 eventually merges and adds `workspace_file_path` DB column:

```
Discovery step 0 (NEW, before checking mapping file):
  Try: SELECT workspace_file_path FROM workspaces WHERE workspace_id = ?
  If column exists AND value is not NULL:
    → Use it as a hint (but still prefer our mapping file if it disagrees,
       since our file uses relative paths which are more robust)
  If column doesn't exist (older Zed):
    → Skip, fall through to mapping file
```

This means our tool automatically benefits from PR #46225 when available, without depending on it.

### 5. JSONC: Acceptable to Defer (Validated by PR #46225)

PR #46225 also uses `serde_json::from_str` (no JSONC). If upstream Zed doesn't handle comments, we're in good company. **Recommendation**: Keep strict JSON for v1. Add JSONC support in Phase 4 only if real-world `.code-workspace` files break.

### 6. MultiWorkspace Readiness Without Over-Engineering

Our 1:1 design (one workspace_id ↔ one `.code-workspace`) is correct for both:
- Current single-workspace-per-window (all users)
- Future MultiWorkspace (each sub-workspace independently maps to its own file)

No schema changes needed. When MultiWorkspace ships to all users:
- Each sub-workspace has its own workspace_id → each gets its own mapping file in its own worktree roots
- Our hook fires on each sub-workspace's DB write independently
- The `active_workspace_id` from `MultiWorkspaceState` tells us which sub-workspace is focused

**No design changes needed now.** The architecture is already MultiWorkspace-compatible because it operates per-workspace_id, not per-window.

### 7. `project_name` Strategy Refined

From the title bar investigation: Zed's title bar project picker dropdown shows only the **active worktree name**, not the full concatenated title. This means:

- Setting `project_name` on the primary worktree mostly affects:
  - The title bar dropdown (shows project_name instead of folder basename)
  - macOS Mission Control / window list (full title with all worktree names)
  - Recent Projects picker (first worktree name displayed)

**Refined behavior**:
- Set `project_name` on primary worktree = `.code-workspace` filename (without extension)
- **Also** set `project_name` on ALL other worktrees to just their folder basename (no change from default, but explicitly set to prevent future confusion)
- Actually, **no** — only set on primary. Don't touch secondary worktrees at all. Less invasive.

### 8. Hook: Also Watch `sqlite3_step` (Not Just `prepare`)

Current hook intercepts `sqlite3_prepare_v2` and checks SQL text. But workspace_id is bound as a parameter AFTER prepare, during `sqlite3_bind_*` / `sqlite3_step`.

**Refinement**: After detecting a workspace-write prepare, also intercept the corresponding `sqlite3_step` call. At step time:
- The statement is fully bound (workspace_id is in parameter 1)
- We can call `sqlite3_column_int64(stmt, 0)` after step to get the workspace_id
- This gives us the EXACT workspace_id being written, solving the "which workspace?" problem

This is a targeted improvement to Option B (improved sqlite3 hook) that gives us most of Option A's benefits without needing Rust symbol hooking.

### 9. New MCP Tool: `workspace_open` (From Capability 1 Analysis)

Add to the proposed tools:

```
**8. `workspace_open` (NEW)**
Input:  { workspace_file: string, mode?: "new_window" | "add" | "reuse" }
Output: { success: bool, pid?: number }
```

Replaces the shell wrapper (`zed-prj open`). Reads `.code-workspace`, resolves paths, invokes:
- `mode=new_window` (default): `zed <folder1> <folder2> ...`
- `mode=add`: `zed --add <folder1> <folder2> ...`
- `mode=reuse`: `zed --reuse <folder1> <folder2> ...`

This is the MCP-native equivalent of PR #46225's `zed my-project.code-workspace` CLI support. AI agents can use it directly.

### Summary of Refinements

| # | Change | Impact |
|---|--------|--------|
| 1 | Fix discovery order (mapping file first) | Correctness — prevents wrong file selection |
| 2 | Add `zed_channel` + `last_sync_ts` to mapping | Forward-compat, conflict detection |
| 3 | Define conflict detection algorithm | Prevents silent data loss |
| 4 | Read PR #46225 DB column as hint | Graceful coexistence |
| 5 | Defer JSONC (validated by upstream) | Reduces scope |
| 6 | Confirm MultiWorkspace-ready (no changes needed) | De-risks future |
| 7 | Only set project_name on primary worktree | Less invasive |
| 8 | Hook sqlite3_step for workspace_id extraction | Solves "which workspace?" |
| 9 | Add `workspace_open` MCP tool | Replaces shell wrapper |
