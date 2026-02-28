# Design: Workspace Identity via project_name + Root Pinning

Date: 2026-02-28
Status: **DESIGN** — supersedes mapping file approach from `06_proposal_design.md`.

---

## Problem Statement

In a multi-root Zed workspace, the **first folder** in the project panel determines:
1. Which `.zed/settings.json` Zed reads for `project_name` (title bar display)
2. The "primary" root shown at the top of the project panel
3. What appears in "Recent Projects" / "Open Recent" search lists

There is currently no mechanism to:
- **Pin** a specific root at index 0 (top of the list)
- **Prevent** Zed from automatically reordering folders alphabetically
- **Stably identify** which root is the "project home"

The previous approach (doc `06_proposal_design.md`) introduced a separate `.zed/zed-project-workspace.json` mapping file. This design **replaces** that with a simpler approach using `project_name` directly.

---

## Why the Mapping File Is Redundant

The `.zed/zed-project-workspace.json` mapping file stores:

```json
{
  "workspace_id": 117,
  "workspace_file": "zed-project-workspace.code-workspace",
  "zed_channel": "preview",
  "last_sync_ts": "2026-02-28T03:41:58.061953+00:00"
}
```

Every field is either derivable or unnecessary:

| Field | Alternative | Why mapping file is unnecessary |
|---|---|---|
| `workspace_id` | Embed in `project_name` as `"117:name"` | `project_name` in `.zed/settings.json` already exists and Zed reads it natively |
| `workspace_file` | Deterministic: `{folder_name}.code-workspace` | VS Code convention — folder name = workspace file name |
| `zed_channel` | MCP detects dynamically from running process | No need to persist; changes when user switches channels |
| `last_sync_ts` | File mtime or drop entirely | Not critical for sync correctness |

**Decision**: Eliminate the mapping file. Use `project_name` in `.zed/settings.json` as the single identity anchor.

---

## Why `project_name` Is the Best Anchor

`project_name` is the **only** field that simultaneously:

1. **Zed reads natively** — displays in title bar, window list, "Recent Projects"
2. **Lives in `.zed/settings.json`** — per-root, human-editable, VCS-trackable
3. **Can embed workspace_id** — `"117:zed-project-workspace"` maps to DB integer
4. **Follows VS Code convention** — root folder name = project name, matching what VS Code shows in its own "Recent" lists
5. **No side effects** — `project_name` is display-only in Zed; changing it doesn't affect functionality

### Format

```
project_name = "{workspace_id}:{root_folder_name}"
                      ↑                    ↑
               DB integer           human-readable name
               (machine-local)      (= folder name of target root)
```

Example: `"117:zed-project-workspace"`

- The `workspace_id` prefix enables MCP to map directly to Zed's SQLite DB
- The folder name matches VS Code's convention (display root folder name)
- The `.code-workspace` extension is NOT included (it's deterministic: `{name}.code-workspace`)

### Why NOT a "clean" name without ID?

Earlier discussion considered a clean name like `"zed-project-workspace"` (no ID). Rejected because:
- MCP needs the ID to efficiently query the DB — without it, must scan all workspaces
- The ID:name format in Zed's title bar is acceptable (no functional side effects)
- Keeping the ID avoids the need for a separate mapping file to store it

---

## Why `.code-workspace` Is NOT the Primary Identity

The `.code-workspace` file is an **optional VS Code/Cursor interop bridge**, not the primary identity:

1. **Zed projects may not have one** — Zed creates workspaces from folder paths, not from `.code-workspace` files
2. **It's a sync target** — written FROM Zed's DB state, not the source of truth
3. **VS Code compatibility** — must not contain custom properties (only `folders`, `settings`, `extensions`, `launch`, `tasks` are standard)
4. **Deterministic filename** — always `{root_folder_name}.code-workspace`, derivable from `project_name`

The MCP handles `.code-workspace` creation and sync as a downstream concern. Identity flows:

```
project_name ("117:zed-project-workspace")    ← IDENTITY ANCHOR
       ↓ parse
workspace_id = 117                             ← DB lookup key
       ↓ query
Zed SQLite DB (paths, paths_order)             ← source of truth
       ↓ sync (optional, MCP-managed)
zed-project-workspace.code-workspace           ← VS Code interop bridge
```

---

## Root Pinning: Problem Analysis

### How Zed Manages Folder Order

Zed stores folder order in two places:

**SQLite DB** (persisted across restarts):
- `paths` column: newline-separated, **lexicographically sorted** (identity key)
- `paths_order` column: comma-separated permutation where `order[lex_index] = user_position`
  - e.g., `"1,2,0"` means: lex_0→pos1, lex_1→pos2, lex_2→pos0 (lex_2 is displayed first)
  - Zed's `PathList::ordered_paths()` zips order with paths, sorts by order value
  - **IMPORTANT**: NOT `order[user_position] = lex_index` (this was a previous bug in our code)

**In-memory** (`WorktreeStore.worktrees` Vec):
- The `worktrees` Vec order = UI display order
- A `worktrees_reordered` boolean flag controls insertion behavior

### The `worktrees_reordered` Flag (Critical)

This flag is the **master switch** for ordering behavior:

| Flag state | `add()` behavior | How it gets set |
|---|---|---|
| `false` (default) | `binary_search_by_key` → insert **alphabetically** | Default on new workspace |
| `true` | `.push()` → **append to end** | Set by drag-drop, or when DB has non-alpha order |

**Source**: `crates/project/src/worktree_store.rs:703-714`

### All Triggers That Change Order

From analysis of Zed source code (PR #11504, #19232):

| # | Trigger | Code location | Behavior | Risk to pinning |
|---|---|---|---|---|
| 1 | **New folder added** (flag=false) | `worktree_store.rs:706-713` | `binary_search` → alphabetical insert | **HIGH** — can push target away from index 0 |
| 2 | **New folder added** (flag=true) | `worktree_store.rs:704` | `.push()` → append to end | **NONE** — target stays at index 0 |
| 3 | **User drag-drop** | `worktree_store.rs:877` | `remove` + `insert`, sets flag=true | **MEDIUM** — user intent, but could move target |
| 4 | **Workspace open with non-alpha DB order** | `workspace.rs:1738-1742` | Sets flag=true | **NONE** — this is what we want |
| 5 | **`--reuse` CLI** | `open_listener.rs:510-522` | Replaces workspace | **NONE** — we control the order |

**Key insight**: Trigger #1 is the **only automatic** danger. Once `worktrees_reordered=true`, it can never happen. And Zed sets it to `true` automatically when DB has non-alphabetical order (trigger #4).

### Why In-Memory Hooking Is Not Needed

Considered and rejected: hooking `WorktreeStore::set_worktrees_reordered()` via Frida-Gum.

**Reasons for rejection:**
1. **The function is a 1-line setter** — almost certainly inlined by the compiler in release builds. No symbol to hook.
2. **Fragile across Zed versions** — struct field offsets change with any field addition/reorder
3. **Unnecessary** — Zed already sets `worktrees_reordered=true` when DB has non-alphabetical `paths_order`. We just need to ensure the DB is correct.

### Four-Layer Defense Strategy

```
Layer 1: DB Hook (sqlite3_prepare_v2 detection + direct UPDATE)
├── Detects workspace write in Zed's DB
├── After 300ms settle, reads paths_order
├── If target root NOT at index 0:
│   └── UPDATE workspaces SET paths_order = ? (corrected)
└── DB ALWAYS has target root first — even after Zed crash

Layer 2: Zed's Own Behavior (automatic, free)
├── On workspace open, reads corrected paths_order from DB
├── paths_order is non-alphabetical (target root pinned first)
├── is_lexicographically_ordered() returns false
├── → set_worktrees_reordered(TRUE) automatically
└── All future add() calls APPEND to end (never alphabetical insert)

Layer 3: CLI reuse_folders — INEFFECTIVE for panel reorder ⚠️
├── `cli --reuse` with already-open folders is a NO-OP in Zed
├── It cannot reorder existing worktrees in the project panel
├── Still useful for ADDING new folders to an existing window
├── Panel order fix relies on Layer 1 (DB) taking effect on next restart
└── Startup pin was removed — it was calling reuse_folders uselessly

Layer 4: Picker sort hook (project picker dropdown)     ← IMPLEMENTED ✅
├── Hooks insertion_sort_shift_left<OpenFolderEntry> via Frida
├── Calls original sort (alphabetical), then post-processes
├── Scans sorted array for target, rotates to index 0
├── Eager install in all processes; target name set via discovery
└── Graceful degradation: falls through to alphabetical on any failure
```

See `10_picker_pin_design.md` for full Layer 4 design, including three approaches tried and ARM64 hooking learnings.

### Coverage Matrix

| Scenario | L1 (DB) | L2 (auto flag) | L3 (reuse) | L4 (picker) | Result |
|---|---|---|---|---|---|
| Fresh workspace open | DB correct | Flag=true | N/A | Target at top | Fully pinned |
| New folder added (flag=true) | DB corrected | Already true | N/A | Target at top | Appended, pinned |
| New folder added (flag=false) | DB corrected ~300ms | Flag→true next restart | N/A | Target at top | Self-healing (next restart) |
| User drag-drop moves target | DB corrected ~300ms | Already true | N/A | Target at top | Re-pinned (next restart) |
| Switch active file | — | — | — | Target stays, ✓ moves | Visual consistency |
| Zed crash mid-session | DB correct | Next open: flag=true | N/A | Target at top | Correct on restart |
| New Zed version (symbol gone) | Works | Works | N/A | Falls back to alpha | L1-2 still work |

**Note**: Panel order changes from L1 (DB correction) take effect on the **next Zed restart**.
Zed reads `paths_order` during workspace restore; mid-session reordering via CLI is not possible.

### Visual Result (Verified 2026-02-28)

**Project Panel** (Layer 1+2: DB pinning → panel order on restart):

```
┌─ Project Panel ─────────────────┐
│ ▸ zed-project-workspace    ← pinned at top (target root)
│ ▸ zed-yolo-hook
│ ▸ _my__zed-api-key
│ ▸ gh-zed-industries__zed
│ ▸ dylib-kit
└─────────────────────────────────┘
```

**Search Projects Picker** (Layer 4: Frida hook → picker order):

```
┌─ Search projects... ────────────┐
│ zed-project-workspace main ✓  ✕ │  ← pinned at top + active marker
│ _my__zed-api-key main           │
│ dylib-kit main                  │  ← rest sorted alphabetically
│ gh-zed-industries__zed main     │
│ zed-yolo-hook main              │
│─────────────────────────────────│
│ Recent Projects                 │
│ corust-test                     │
│ networking                      │
│ ...                             │
└─────────────────────────────────┘
```

**Title Bar**: `zed-project-workspace ∨ main` — reads `project_name` from pinned root's `.zed/settings.json`.

### Zed's Workspace Restore Flow (from source analysis)

When Zed starts and restores a workspace:

1. **DB read**: `persistence.rs` reads `paths` + `paths_order` columns
2. **PathList::deserialize**: Reconstructs ordered path list from `SerializedPathList`
3. **worktrees_reordered check** (`workspace.rs:1738-1742`):
   - If `!paths.is_lexicographically_ordered()` → `set_worktrees_reordered(true)`
4. **Sequential worktree creation** (`workspace.rs:1749-1761`):
   - `for path in paths_to_open` → `find_or_create_worktree(path).await`
   - Each worktree is awaited before the next is created
5. **WorktreeStore::add()** (`worktree_store.rs:703-714`):
   - When `worktrees_reordered=true`: `.push(handle)` — preserves paths_to_open order
   - When `false`: `binary_search_by_key` → alphabetical insert

**Result**: If paths_order correctly puts target first, and Zed sets worktrees_reordered=true
(because the order is non-alphabetical), the panel will display target first on next restart.

### `paths_order` Semantics Bug (Fixed 2026-02-28)

Our code originally interpreted `paths_order` as `order[user_position] = lex_index`.
This was **inverted** from Zed's actual semantics: `order[lex_index] = user_position`.

**Discovery**: Reading `PathList::ordered_paths()` in `crates/util/src/path_list.rs`:
```rust
pub fn ordered_paths(&self) -> impl Iterator<Item = &PathBuf> {
    self.order
        .iter()
        .zip(self.paths.iter())
        .sorted_by_key(|(i, _)| **i)  // sort by order VALUE (= user position)
        .map(|(_, path)| path)
}
```

The zip pairs `(order[lex_idx], paths[lex_idx])`, then sorts by the order value.
So `order[lex_idx]` IS the user position for that lex-indexed path.

**Impact**: Our `correct_paths_order()` and `reconstruct_ordered_paths()` were producing
inverted permutations — e.g., writing `"2,4,1,0,3"` when we meant `"1,2,0,3,4"`.
This made `zed-yolo-hook` appear first instead of `zed-project-workspace`.

**Fix**: Updated `reconstruct_ordered_paths()`, `compute_paths_order()`, and
`correct_paths_order()` to use the correct `order[lex_index] = user_position` semantics.

### Layer 3 Ineffectiveness (Discovered 2026-02-28)

`cli --reuse` with paths that are already open in the current workspace is a **no-op**.
Zed's `--reuse` flag finds an existing workspace window and opens paths in it, but does
not reorder worktrees that are already present. The `replace_window` logic only triggers
when the workspace paths differ.

**Attempted approaches that failed**:
- `cli --reuse path1 path2 ...` — no-op for already-open paths
- Startup pin (calling reuse_folders after first sync) — same no-op
- Timed delay before reuse_folders — still a no-op

**What actually works**: Layer 1 (DB correction) is the real fix. Once `paths_order` is
correct in the DB, Zed reads it on next restart and restores worktrees in the right order.

### Loop Prevention

The correction cycle: `reuse_folders → Zed writes DB → hook detects → reads order → already correct → done`.

Mechanisms:
1. **Per-workspace cooldown** (`DEBOUNCE_MAP`, 1 second) — suppresses re-entry
2. **Self-write detection** — hook recognizes its own corrections
3. **Idempotency** — `reuse_folders` with same paths+order is a no-op from Zed's perspective
4. **`NEEDS_REPIN` flag** — defers pinning to next sync cycle, prevents recursion inside callback

---

## project_name Lives Only in Primary Root

### Problem

The original design wrote `project_name` to ALL worktree roots so that "any root can identify the workspace." This creates trash — non-primary roots like `dylib-kit` get a `project_name` that doesn't belong to them.

### Rule: Write Only to Primary Root

`project_name` should exist in exactly ONE root's `.zed/settings.json` — the root whose folder name matches the name portion of the value.

**Triple-match validation** to identify the primary root:

```
For project_name = "{id}:{name}" found in root R:
  1. R.folder_name == name              (root IS the named project)
  2. R/{name}.code-workspace exists     (workspace file is present)
  3. id validates in Zed's DB           (workspace_id exists)
```

The `folder_name == name` check alone is sufficient and unambiguous.

### Discovery with Conflicting Values

When multiple roots have `project_name` with different values:
1. Scan all roots, collect `(root, project_name)` pairs
2. For each, apply triple-match validation
3. First root passing all three checks = primary root → use its value
4. Clean up: remove `project_name` from all other roots' settings.json

See `10_picker_pin_design.md` Part B for full implementation details.

---

## Implementation Summary

### What Changes

| Component | Change |
|---|---|
| `project_name` format | `"117:zed-project-workspace"` (id:folder_name, no extension) |
| `.zed/zed-project-workspace.json` | **Eliminated** — no longer needed |
| `.code-workspace` | Unchanged — optional VS Code interop, managed by MCP sync |
| Hook sync | After DB write detection, correct `paths_order` + `reuse_folders` |
| MCP startup | Read `project_name` → parse id → validate DB → pin → sync |
| Discovery | Simplified: `project_name` → DB → `.code-workspace` (optional) |

### New Modules

- `src/settings.rs` — read/write/parse `project_name` from `.zed/settings.json`
- `src/pinning.rs` — target root determination, order correction, `reuse_folders` invocation

### Files Modified

- `src/discovery.rs` — simplified 3-step chain using `project_name`
- `src/mapping.rs` — deprecated, cleanup helper for legacy files
- `src/sync_engine.rs` — pinning after execute_sync
- `hook/src/discovery.rs` — clean format, remove mapping writes
- `hook/src/sync.rs` — DB writer correction + `NEEDS_REPIN` + `reuse_folders`
- `mcp/src/main.rs` — lazy identity, pinning after tools

---

## References

- Zed PR #11504: "Enable manual worktree organization" (May 2024)
- Zed PR #19232: Fix order persistence across restarts (Oct 2024)
- `crates/project/src/worktree_store.rs` — `worktrees_reordered` flag, `add()`, `move_worktree()`
- `crates/workspace/src/workspace.rs:1736-1761` — automatic flag setting + sequential worktree creation
- `crates/util/src/path_list.rs` — `PathList` serialization, `ordered_paths()`, `is_lexicographically_ordered()`
- `crates/workspace/src/persistence.rs:1458-1469` — `paths_order` DB column write
- `crates/zed/src/zed/open_listener.rs:510-522` — `--reuse` flag handling (no-op for same paths)
