# Research Report: Zed <-> VS Code `.code-workspace` Sync

Date: 2026-02-27

Repo(s) reviewed:
- Our sync tool + hook: `$HOME/codes/zed-project-workspace`
- Zed source: `$HOME/codes-repos/gh-zed-industries__zed` (commit `1c39e192f1fa83a6d131d4f43d13ade53e8a424d`, branch `main`)

Primary goal:
- Make Zed behave "as if it had a project/workspace file" for multi-folder project organization, using VS Code/Cursor-compatible `*.code-workspace` as the shared manifest.
- Our current approach (MCP + hook) is a workaround; this report documents the actual invariants in Zed's sqlite DB and VS Code's workspace file so we can design a workflow that is correct and stable.

---

## Executive Summary (What Matters Most)

1. Zed stores workspace root folders in sqlite as a *set* + a separate *order*:
   - `workspaces.paths` is newline-separated absolute paths in **lexicographic order**.
   - `workspaces.paths_order` is a comma-separated permutation that reconstructs the user-visible order.
   - Any sync that ignores `paths_order` will reorder folders (typically alphabetically) and drift from expected UX.

   **Live example (Zed Preview, workspace 110 — current focused window, DB: `~/Library/Application Support/Zed/db/0-preview/db.sqlite`):**

   `paths` (9 folders, stored in lexicographic order):
   ```
   $HOME/codes/zed-patcher              ← index 0
   $HOME/codes/zed-project-workspace     ← index 1
   $HOME/codes/zed-yolo-hook             ← index 2
   $HOME/codes-repos/gh-Tyilo__insert_dylib          ← index 3
   $HOME/codes-repos/gh-YinMo19__insert-dylib        ← index 4
   $HOME/codes-repos/gh-cocoa-xu__insert_dylib_rs    ← index 5
   $HOME/codes-repos/gh-frida__frida-rust             ← index 6
   $HOME/codes-repos/gh-modelcontextprotocol__rust-sdk ← index 7
   $HOME/codes-repos/gh-zed-industries__zed           ← index 8
   ```

   `paths_order`: `0,1,2,3,4,5,6,7,8` (sequential — user-visible order happens to match lexicographic)

   **Non-trivial reorder example (Zed stable, workspace 38, DB: `0-stable/db.sqlite`):**

   `paths` (6 folders, lexicographic):
   ```
   $HOME/codes/_my__jdbox                                        ← index 0
   $HOME/codes/_topic-ai-chatlog-history/_my__cursor-jwt-decoder ← index 1
   $HOME/codes/_topic-ai-chatlog-history/websession-kimi         ← index 2
   $HOME/codes-repos/gh-keats__jsonwebtoken                      ← index 3
   $HOME/codes-repos/gh-libyal__dtformats                        ← index 4
   $HOME/codes-repos/gh-saying121__tidy-browser                  ← index 5
   ```

   `paths_order`: `0,4,1,5,2,3` — semantics: `order[lex_index] = user_position`.
   So: lex_0→pos0, lex_1→pos4, lex_2→pos1, lex_3→pos5, lex_4→pos2, lex_5→pos3.
   User-visible order (sorted by position):
   1. `_my__jdbox` (lex_0, pos 0)
   2. `websession-kimi` (lex_2, pos 1)
   3. `gh-libyal__dtformats` (lex_4, pos 2)
   4. `gh-saying121__tidy-browser` (lex_5, pos 3)
   5. `_my__cursor-jwt-decoder` (lex_1, pos 4)
   6. `gh-keats__jsonwebtoken` (lex_3, pos 5)

   **How the three Zed UI lists map to the DB (verified on Zed Preview, workspace 110):**

   | UI Surface | What It Shows | Data Source | Sort Order |
   |---|---|---|---|
   | **Sidebar (Project Panel)** | Workspace root folders | `workspaces.paths` + `paths_order` for the current `workspace_id` | User order (reconstructed via `paths_order`) |
   | **"Search projects..." picker** (title bar dropdown or `File > Open Recent`) | Open folders in current workspace (top section) + recent workspaces (below) | **Open folders:** runtime `project.visible_worktrees(cx)` → same roots as sidebar. **Recent:** `SELECT ... FROM workspaces WHERE paths IS NOT NULL ORDER BY timestamp DESC` | Open folders: **alphabetical by display name** (case-insensitive). Recent: **most-recently-used first** (by `timestamp`) |
   | **"File > Open Recent" menu** | Same picker as above (modal variant with path details) | Same as above — both are rendered by `RecentProjects` picker in `crates/recent_projects/src/recent_projects.rs` | Same as above |

   Source code references (Zed commit `1c39e192f`):
   - Open folders list: `crates/recent_projects/src/recent_projects.rs:144-193` — `get_open_folders()` calls `project.visible_worktrees(cx)`, sorts alphabetically (line 191).
   - Recent workspaces query: `crates/workspace/src/persistence.rs:1633-1641` — `recent_workspaces_query()` selects from `workspaces` table ordered by `timestamp DESC`.
   - Recent workspaces on-disk filter: `crates/workspace/src/persistence.rs:1803-1855` — `recent_workspaces_on_disk()` validates paths still exist on disk.

   Key insight: **All three lists ultimately derive from the single `workspaces` table.** There is no separate "recent projects" or "project picker" table. The difference is:
   - Sidebar uses `paths` + `paths_order` (user-ordered) for the **current** workspace.
   - Picker uses `visible_worktrees()` at runtime (alphabetically sorted) for the open-folders section, plus `workspaces` rows ordered by `timestamp` for recent entries.

2. VS Code `.code-workspace` files are effectively JSON-with-comments (JSONC) in the wild:
   - The official multi-root docs explicitly show comments in `.code-workspace` examples and state comments are allowed.
   - Strict JSON parsing (`serde_json`) will reject many real workspace files.
3. Zed "remove folders" on a running instance is not a clean incremental API from outside:
   - Zed CLI supports `--add` (add into existing workspace) and `--reuse` (replace workspace in an existing window).
   - If we want file-driven removals to take effect without restart, `--reuse` is the most realistic external path (but it's disruptive).
4. Our current sync implementation works for membership sync but has correctness gaps:
   - `paths_order` is not used anywhere in our DB readers/sync logic today.
   - MCP-side DB selection uses `LIKE "%{folder_path}%"` which can match the wrong workspace.
   - `.code-workspace` parsing is strict JSON (no comments), and relative path calculation is limited.
5. Our current workspace-id mapping hack uses `.zed/settings.json` `project_name`, which is likely unacceptable UX:
   - In Zed, `project_name` is a user-facing display name (it shows in the window title).
   - Writing `{workspace_id}:{filename}` into `project_name` will pollute UI and can conflict with user intent.
   - Editing `.zed/settings.json` can also strip user comments/formatting (Zed settings support JSON-with-comments).

---

## Ground Truth: How Zed Persists Workspace Roots

### Zed DB Columns and Serialization Invariants

Zed persists workspaces in sqlite via the `workspaces` table. For local workspaces, the relevant fields for root folders are:
- `workspace_id` (integer primary key)
- `paths` (TEXT)
- `paths_order` (TEXT)
- `timestamp` (CURRENT_TIMESTAMP updated on save)
- `remote_connection_id` (NULL for local)

The critical detail: Zed does not store "ordered roots" directly. Instead, it stores:
- `paths`: paths sorted lexicographically, serialized as newline-separated string.
- `paths_order`: the order indices corresponding to the original input order, serialized as comma-separated string.

This behavior is implemented in `PathList`:
- Zed: `crates/util/src/path_list.rs`
  - `PathList::new()` sorts input paths lexicographically and stores original positions in `order`.
  - `PathList::serialize()` emits:
    - `SerializedPathList.paths` as `"pathA\npathB\n..."`
    - `SerializedPathList.order` as `"1,0,2,..."`
  - `PathList::ordered_paths()` reconstructs the original order using `order`.

Implication:
- If we read only `paths` from DB and treat it as ordered, we will always get lexicographic order, not user order.

### How Zed Selects an Existing Workspace by Roots

Zed selects a workspace record by matching the serialized path set:
- Zed: `crates/workspace/src/persistence.rs`
  - `workspace_for_roots_internal(...)` constructs `root_paths = PathList::new(worktree_roots)` (sorting for DB stability).
  - It selects `FROM workspaces WHERE paths IS ? AND remote_connection_id IS ?` using the serialized `paths` string, ignoring user ordering.

This is important because it shows Zed's "workspace identity":
- Workspace identity is based on the set of roots (plus remote/local), not their order.
- The order is a separate UX layer.

### How Zed Uses `paths_order` When Opening

When opening a local workspace, Zed prefers the ordered list derived from the stored `paths_order`:
- Zed: `crates/workspace/src/workspace.rs`
  - In `Workspace::new_local(...)`, if a serialized workspace exists, Zed uses:
    - `paths.ordered_paths()` for `paths_to_open`
    - `paths.is_lexicographically_ordered()` to decide whether to mark "reordered"

Implication:
- The user-visible workspace root ordering in Zed can be stable across sessions, but only if `paths_order` is preserved.

---

## Ground Truth: Zed CLI Behaviors Relevant to Sync

There are two binaries involved:
- `crates/cli` is the external `zed` CLI launcher (what we call from our MCP tool).
- `crates/zed` is the app binary handling the IPC requests and doing the actual open work.

### CLI Flags

Zed CLI (launcher) flags that matter:
- `zed --add <paths...>`:
  - sets `open_new_workspace = Some(false)` in the IPC request (adds to existing workspace/window).
  - Zed-side behavior: open paths in the focused/appropriate existing window.
  - Source: Zed `crates/cli/src/main.rs` around `open_new_workspace` computation.
- `zed --reuse <paths...>`:
  - sets `reuse = true` and triggers Zed-side logic to replace the workspace in an existing window (first matching location window).
  - Source: Zed `crates/zed/src/zed/open_listener.rs` around `reuse` handling.

Practical interpretation for us:
- Adds can be incremental (`--add`).
- Removals are not an incremental API; "remove" is achieved by replacing the workspace roots with exactly the desired set (`--reuse`), which is disruptive but feasible.

---

## Ground Truth: VS Code `.code-workspace` Semantics (Multi-root Workspaces)

Primary reference:
- VS Code docs: "Multi-root Workspaces"
  - https://code.visualstudio.com/docs/editing/workspaces/multi-root-workspaces

Key behaviors from the docs:
- Multi-root workspaces are a collection of folders ("root folders") opened together.
- Saving a multi-root workspace creates a `.code-workspace` file.
- VS Code supports reordering workspace folders (for example via drag-and-drop in the Explorer), which implies folder order is intended to be a stable, user-controlled property.
- VS Code also maintains an *untitled* workspace file (`untitled.code-workspace`) behind-the-scenes when you add a second folder before explicitly saving a `.code-workspace` file.
- `.code-workspace` schema is:
  - `folders`: ordered array of folder entries.
    - Each entry can use an absolute or relative `path`.
    - Optional `name` attribute overrides display name in Explorer.
  - Workspace-global `settings` can be included under `settings`.
  - Extension recommendations can be included under `extensions`.
- The docs show and explicitly allow comments in `.code-workspace` examples ("As you can see ... you can add comments to your Workspace files.").

Practical interpretation for us:
- The `folders` array order is meaningful (UI ordering, disambiguation, etc.).
- Parsing must accept comments (JSONC-ish) to be robust in real-world VS Code/Cursor workspace files.
- When we rewrite workspace files, we should preserve:
  - `extra`/unknown fields (we already do)
  - `name` fields on folder entries (we already store, but we currently lose them on add/remove because we always write `name: None`)
  - formatting/comments if possible (we currently do not; rewriting will strip comments)
- If we want VS Code-like behavior, then (a) Zed UI add/remove/reorder should update the workspace file and (b) if no workspace file exists we likely need a deterministic bootstrap file (analogous to VS Code's untitled workspace behavior).

---

## Important Note: Doc Drift on Zed Native `.code-workspace` Support

`docs/01_research.md` references an experimental/PR branch (e.g. "PR #46225") that allegedly added native `.code-workspace` parsing/persistence into Zed.

On the Zed `main` commit reviewed in this report (`1c39e192f1fa83a6d131d4f43d13ade53e8a424d`), a code search for `code-workspace` only finds Windows installer file-association entries (see `crates/zed/resources/windows/zed.iss`), and does not find any Rust-side `.code-workspace` parsing or "workspace file path" persistence columns in the DB migrations.

Implication:
- We should assume Zed does not (currently) manage `.code-workspace` files natively, so our hook/MCP approach remains necessary if `.code-workspace` is the desired shared manifest.

---

## Ground Truth: Zed Workspace Identity — From DB to UI

This section answers the fundamental question: **How does Zed's opaque `workspace_id` integer become a human-recognizable project name in the UI?**

### 1. workspace_id Lifecycle

- **Creation**: `INSERT INTO workspaces DEFAULT VALUES RETURNING workspace_id` — a new auto-incremented integer.
  - Source: `crates/workspace/src/persistence.rs:1602-1606` (`next_id()`)
- **Reuse**: When opening folders, Zed queries `WHERE paths IS ? AND remote_connection_id IS ?` (exact match on sorted canonical path set). If found, reuses that workspace_id.
  - Source: `crates/workspace/src/persistence.rs:1004-1076` (`workspace_for_roots_internal()`)
- **Sticky ID**: When adding/removing folders from a running workspace, the workspace_id **stays the same** — only the `paths` column is updated via UPSERT.
  - Source: `crates/workspace/src/persistence.rs:1420-1469` (`save_workspace()`)
- **Identity = (paths, remote_connection_id)**: The unique index is on these two columns. workspace_id is just an internal handle.

**Critical implication**: Zed does NOT have a stable "workspace name" concept. The identity is the **set of paths**. If you change the paths (add/remove a folder), the old path-set identity is gone — but the workspace_id persists because it's updated in-place.

### 2. Title Bar Display Name (Window Title)

**Source**: `crates/workspace/src/workspace.rs:5119-5180` (`update_window_title()`)

The window title is constructed by iterating ALL visible worktrees and concatenating names:

```
for each visible worktree:
    name = worktree's .zed/settings.json project_name ?? folder basename
title = join(names, ", ")
title += " — " + active_file_name
```

- **Single-root workspace**: title = `"zed-patcher — main.rs"`
- **Multi-root workspace (9 folders)**: title = `"zed-patcher, zed-project-workspace, zed-yolo-hook, gh-Tyilo__insert_dylib, ... — main.rs"`
- **With project_name override**: If `.zed/settings.json` has `"project_name": "My Project"`, that string replaces the folder name in the title.

**What we see in the screenshot** (title bar shows just "zed-yolo-hook"): This is because the **project picker dropdown** in the title bar shows only the **active worktree** name, not the full concatenated title. The actual window title (in macOS title bar/Mission Control) would show all folder names.

### 3. "Search projects..." Picker

**Source**: `crates/recent_projects/src/recent_projects.rs`

The picker combines TWO sections:

**Open Folders section** (top, from current workspace):
- `get_open_folders()` (lines 144-193): calls `project.visible_worktrees(cx)`, sorts alphabetically (case-insensitive, line 191).
- Each entry is a single worktree from the current workspace, with its folder name + git branch.
- Clicking an entry **switches the active worktree** (not a new window).

**Recent Workspaces section** (below):
- `get_recent_projects()` (lines 86-138): queries `workspaces` table ordered by `timestamp DESC`.
- Each entry represents ONE workspace_id with ALL its paths.
- Display name: folder names joined by ", " (for multi-root workspaces).
- Clicking an entry opens that workspace (by passing paths to `open_workspace_for_paths()`, which re-derives workspace_id via path matching).

**The screenshot shows ONLY the "Open Folders" section** — the 9 individual worktrees of workspace 110, not "recent workspaces" entries. The recent workspaces would appear below (scrolled off-screen).

### 4. How "Open Folder" Maps to workspace_id

When user does `File > Open` and picks a folder:
1. `open_paths()` → `Workspace::new_local()` at `workspace.rs:1699`
2. `DB.workspace_for_roots(paths)` at `workspace.rs:1763` — looks up by canonical sorted path set
3. If match found: reuse workspace_id. If not: `DB.next_id()` creates a new one.

**There is NO menu option to "open a workspace by ID"**. Zed always works through paths.

### 5. project_name Field — The Only Identity Override

**Source**: `crates/worktree/src/worktree_settings.rs:11-24`, `crates/settings_content/src/project.rs:91-96`

```rust
pub struct WorktreeSettingsContent {
    /// The displayed name of this project. If not set or null, the root directory
    /// name will be displayed.
    pub project_name: Option<String>,
}
```

Key properties:
- **Per-worktree** (NOT per-workspace): Each worktree root has its own `.zed/settings.json`.
- In multi-root workspace: each worktree's `project_name` appears in the title, comma-separated.
- **Only affects display**: title bar, picker display name. Does NOT affect workspace identity in DB.
- **Unknown keys are allowed**: Zed uses `#[serde(flatten)]` without `deny_unknown_fields`, so custom keys like `"zed_project_workspace": {...}` are silently ignored.

### 6. Zed Has NO Native "Workspace File" Concept

Zed currently has:
- Per-worktree `.zed/settings.json` (project-level settings)
- Global user settings
- SQLite DB for session state

Zed does NOT have:
- A `.zed-workspace` file or equivalent
- Workspace-level settings scope (only global and worktree scopes exist)
- Native `.code-workspace` parsing (only Windows installer file-association exists in code)
- Any concept of "named workspace" beyond `project_name` display override

---

## Cross-Editor Comparison: Workspace/Project Identity

| Aspect | VS Code | JetBrains IDEs | Sublime Text | Zed |
|--------|---------|---------------|--------------|-----|
| **Identity mechanism** | Folder path (implicit) + `.code-workspace` file (explicit for multi-root) | `.idea/` directory | `.sublime-project` file | SQLite DB keyed by canonical path set |
| **Human-friendly name** | Folder basename or `.code-workspace` filename | Directory name; overridable via `.idea/.name` file | `.sublime-project` filename | Folder basename; overridable via `.zed/settings.json` `project_name` |
| **Explicit name field?** | No (only `window.title` setting) | Yes (`.idea/.name`) | No | Semi (`project_name` in `.zed/settings.json`) |
| **Multi-root file** | `*.code-workspace` (JSON, folders array) | `.idea/modules.xml` + `.iml` files | `.sublime-project` (JSON, folders array) | **None** — paths stored only in SQLite |
| **Session state** | `workspaceStorage/<hash>/state.vscdb` | `.idea/workspace.xml` | `*.sublime-workspace` | SQLite `workspaces` table |
| **Recent projects storage** | Global SQLite DB | `recentProjects.xml` | Session file | Same `workspaces` table (`ORDER BY timestamp DESC`) |
| **VCS-shareable config** | `.code-workspace`, `.vscode/settings.json` | Most of `.idea/` | `.sublime-project` | `.zed/settings.json` only |

**Key insight**: VS Code, JetBrains, and Sublime all have an **explicit, VCS-shareable project definition file** that serves as both identity anchor and shared configuration. **Zed is the only editor that relies entirely on an opaque database** for workspace identity, with no file-based project manifest.

This gap is exactly what our `zed-project-workspace` tool fills — providing the `.code-workspace` file as the missing "project manifest" layer.

---

## Our Current Implementation (What It Actually Does Today)

### Crate Layout

In `$HOME/codes/zed-project-workspace`:
- Library (shared):
  - `src/workspace_file.rs`: strict JSON parse + resolve + diff + add/remove folder entries
  - `src/workspace_db.rs`: rusqlite reader for Zed DB
  - `src/sync_engine.rs`: compute + execute sync actions (file <-> DB) and optionally invoke `zed --add`
- Hook (in-process, event-driven Zed -> file):
  - `zed-prj-workspace-hook/src/hooks/sqlite3_prepare.rs`: intercept `sqlite3_prepare_v2`
  - `zed-prj-workspace-hook/src/discovery.rs`: find the target `.code-workspace` file
  - `zed-prj-workspace-hook/src/sync.rs`: query DB, diff, update `.code-workspace`
- MCP server (on-demand file -> Zed and introspection):
  - `zed-prj-workspace-mcp/src/main.rs`: exposes tools `workspace_folders_list/add/remove/sync`
- Xtask patcher:
  - `xtask/src/main.rs`: build, inject dylib, codesign, verify, restore

### Zed -> File (Hook-driven)

Trigger:
- Hook intercepts sqlite3 prepare calls and filters SQL as a workspace write.

Flow:
1. `sqlite3_prepare_v2` detour runs inside Zed.
2. First call triggers discovery if `TARGET_PENDING`:
   - Query latest workspace: `SELECT workspace_id, paths FROM workspaces ORDER BY timestamp DESC LIMIT 1`.
   - Use folder roots to locate `*.code-workspace` in those roots, or consult `.zed/settings.json project_name`, or bootstrap.
3. After delay/cooldown:
   - Query DB by `workspace_id` for `paths`.
   - Read `.code-workspace`.
   - Diff and update the file (DB is treated as source of truth).

Key current limitations:
- DB query reads only `paths`, not `paths_order`, so file output order is lexicographic (not Zed UI order).

### File -> Zed (MCP + Library)

Trigger:
- On-demand tool call (`workspace_folders_add` or `workspace_folders_sync`) that invokes `zed --add`.

Key current limitations:
- Removals are "pending removal" only; we do not attempt `zed --reuse`.
- DB workspace selection for comparison uses `find_by_folder()` (substring match), which can select wrong records.
- Our DB/file diff is effectively set-based (it does not detect pure folder reordering as a change).
- MCP `workspace_folders_list` computes `in_sync` using full vector equality (order-sensitive), but DB folder order is not currently reconstructed from `paths_order`.

---

## Major Gaps / Risks (Workflow-Critical)

### 1) Workspace Mapping Abuses `.zed/settings.json` `project_name` (User-Facing)

Impact:
- Using `project_name` for sync metadata pollutes Zed UI (window title) and can override/compete with user intent.
- Editing `.zed/settings.json` for metadata can also strip comments/formatting (Zed supports parsing settings with comments).

Evidence:
- Zed treats `project_name` as a displayed project name (used in window title):
  - Zed: `crates/workspace/src/workspace.rs` (`update_window_title`) uses `WorktreeSettings::get(...).project_name`.
- Our code writes mapping into `project_name`:
  - Hook: `zed-prj-workspace-hook/src/discovery.rs` `write_zed_settings_for_roots()` writes `"project_name": "{workspace_id}:{filename}"`.

Fix direction:
- Do not use `project_name` for machine mapping.
- Prefer one of:
  - A dedicated metadata key inside `.zed/settings.json` that does not affect UI (if Zed tolerates unknown keys), for example `"zed_project_workspace": { "workspace_id": ..., "workspace_file": ... }`.
  - A dedicated file we own, for example `.zed/zed-project-workspace.json`, to avoid rewriting user settings at all.
- If we keep writing anything into `.zed/settings.json`, we need to ensure we preserve user formatting/comments (or avoid touching the file unless strictly required).

### 2) Folder Order Is Not Preserved (Zed `paths_order` Ignored + Set-Based Diff)

Impact:
- `.code-workspace` will oscillate into alphabetical order after any Zed event-driven update.
- VS Code/Cursor folder order and Zed folder order will diverge from user expectation.
- Reordering roots inside Zed (or VS Code) is not detected by our current diff; we only sync membership changes.

Evidence:
- Zed persists order separately (`paths_order`) in `PathList`.
- Our code:
  - `src/workspace_db.rs` reads only `paths` (and never reads `paths_order`).
  - Hook query in `zed-prj-workspace-hook/src/sync.rs` selects only `paths`.
  - Our diff uses sets (`BTreeSet`) and therefore does not detect pure reorders (`src/workspace_file.rs::diff_folders`).

Fix direction:
- Read both `paths` and `paths_order`, reconstruct ordered list using Zed's logic (equivalent to `PathList::deserialize` + `ordered_paths()`).
- Define whether order is a first-class synced property:
  - If yes: compare sequences, not sets, and update `.code-workspace` folder ordering on reorder-only changes.

### 3) Wrong Workspace Record Selection in MCP/Library (`LIKE "%...%"`)

Impact:
- MCP tool can sync against the wrong workspace (especially if a path is a substring of another path).
- `%` and `_` are SQL wildcards; a path containing them changes query meaning.

Evidence:
- `src/workspace_db.rs::find_by_folder()` uses `WHERE paths LIKE ?1` with `"%{folder_path}%"`.

Fix direction:
- Replace substring match with a deterministic identity strategy:
  - Prefer a stable `workspace_id` mapping (but not via `project_name`; see Gap #1).
  - Or match by Zed's own identity rule: serialize the full root path set into `paths` string and query `WHERE paths IS ?`.

### 4) `.code-workspace` Parsing Is Strict JSON, But VS Code Allows Comments

Impact:
- Real VS Code/Cursor workspace files that contain comments will fail to parse; our tools would break or refuse to sync.

Evidence:
- `src/workspace_file.rs` uses `serde_json::from_str`.
- VS Code docs show comments in `.code-workspace` examples and state comments are supported.

Fix direction:
- Switch parsing to JSONC-capable parser (or pre-strip comments before `serde_json`).
- Keep serialization deterministic (we can still write standard JSON, but should acknowledge comments will be lost unless we preserve formatting).

### 5) Relative Path Computation Is Not a True Relpath

Impact:
- When workspace file is in one root and another root is a sibling, VS Code typically stores `../other-root`, but our code falls back to absolute paths unless the folder is under the workspace file directory.

Evidence:
- `src/workspace_file.rs::pathdiff_relative()` uses `strip_prefix(base)` only.

Fix direction:
- Use a real relative-path algorithm (e.g. compute `..` segments) for portability of shared `.code-workspace` files.

### 6) Remove/Close Semantics Need a Defined Workflow

We need to decide what "sync" means for removals:
- Option A (conservative): never attempt to remove from running Zed; only update file and require restart/reopen.
- Option B (smart but disruptive): if `.code-workspace` removes folders, apply via `zed --reuse <desired roots...>` (replace workspace).

Given Zed CLI supports `--reuse`, Option B is feasible but should be explicit and well-communicated to avoid surprising users.

---

## Proposed "Smart Sync" Workflow (Discussion Draft)

### Design Principle

Treat `.code-workspace` as the human-facing, cross-editor manifest (VS Code/Cursor/Zed).
Treat Zed DB as the current-session ground truth of what Zed has opened, but not a stable manifest.

### Two Sync Modes

1. Incremental mode (default, low risk):
   - Zed -> file:
     - hook updates `.code-workspace` membership + order (must read `paths_order`).
   - File -> Zed:
     - apply only additions via `zed --add` (in the file's folder order).
     - removals remain pending (needs restart/reopen).

2. Reconcile mode (explicit, higher risk):
   - When removals/reordering occur in the file, replace the workspace roots via `zed --reuse <all desired roots...>`.
   - This provides "workspace file is source of truth" semantics more like VS Code.

### Conflict Handling

We should define conflict semantics up-front:
- Use `workspaces.timestamp` vs `.code-workspace` file mtime to detect which side changed last.
- Default to "do not overwrite silently"; log and require explicit reconcile when both changed.

### Stable Mapping Between DB Workspace and File

Current hook mapping strategy (as implemented) uses `.zed/settings.json` `project_name` as `{workspace_id}:{filename}`.

However, this is likely not viable long-term:
- `project_name` is intended to be user-facing (display name), not machine metadata.
- Writing IDs into `project_name` pollutes UI and blocks users from using `project_name` for its intended purpose.
- Editing `.zed/settings.json` can strip comments/formatting.

Suggested extension:
- Switch to a dedicated mapping mechanism (preferred: our own file in `.zed/`), then MCP sync should consult that mapping to get `workspace_id` instead of guessing with substring search.
- Optionally also store `workspace_id` inside `.code-workspace` (as extra metadata) if VS Code preserves unknown fields sufficiently in practice.

---

## Action Items for Next Round

Workflow decisions (need alignment):
1. Do we support file-driven removals without restart by using `zed --reuse`?
2. Do we require `.code-workspace` parsing to accept comments (JSONC), even if writing will strip them?
3. Do we consider folder ordering as a first-class synced property (recommended), or only membership?

Implementation tasks (if we proceed):
1. Read and use `paths_order` from DB in:
   - hook sync (`zed-prj-workspace-hook/src/sync.rs`)
   - library DB reader (`src/workspace_db.rs`)
   - MCP tool `in_sync` computation (compare as sets + order separately)
2. Replace `.zed/settings.json project_name` mapping with a non-user-facing metadata strategy (see Gap #1).
3. Replace `find_by_folder()` with deterministic workspace identity:
   - via mapping -> `workspace_id`
   - or `paths` exact match using Zed's own serialization strategy
4. Improve `.code-workspace` parsing:
   - JSONC support
   - preserve `name` fields where possible when rewriting folders
5. Improve relative path writing:
   - true `relpath` (supports `..`)

---

## Appendix A: Evidence Commands Used

**Important: Zed DB paths differ by channel:**
- Zed stable: `~/Library/Application Support/Zed/db/0-stable/db.sqlite`
- Zed Preview: `~/Library/Application Support/Zed/db/0-preview/db.sqlite`
- Zed global (shared): `~/Library/Application Support/Zed/db/0-global/db.sqlite`

All three share the same parent `~/Library/Application Support/Zed/db/`. The running process (`/Applications/Zed Preview.app/Contents/MacOS/zed`) determines which DB is active.

Zed `.code-workspace` presence in code (current main):
- `rg -n -- \"\\.code-workspace\" crates | head`
  - In this local clone, this only finds Windows installer association lines, not parsing logic.

Zed `PathList` serialization:
- `crates/util/src/path_list.rs`

Zed workspace upsert:
- `crates/workspace/src/persistence.rs` `save_workspace()` writes both `paths` and `paths_order`.

Zed CLI `--add/--reuse` semantics:
- `crates/cli/src/main.rs` computes `open_new_workspace` and `reuse`.
- `crates/zed/src/zed/open_listener.rs` applies `reuse` by selecting a window and replacing.

VS Code multi-root docs:
- https://code.visualstudio.com/docs/editing/workspaces/multi-root-workspaces

## Appendix B: Line-Level References (For Implementation Discussion)

Zed (commit `1c39e192f1fa83a6d131d4f43d13ade53e8a424d`):
- PathList sorting/order serialization:
  - `crates/util/src/path_list.rs:10-119`
- Zed settings support JSON-with-comments (editing `.zed/settings.json` may strip comments on rewrite):
  - `crates/settings_content/src/project.rs:7-35`
- `code-workspace` only appears in Windows installer association in this commit (no Rust parsing):
  - `crates/zed/resources/windows/zed.iss:253-259`
- Workspace identity selection uses `paths` (set identity):
  - `crates/workspace/src/persistence.rs:1004-1076` (`WHERE paths IS ? AND remote_connection_id IS ?`)
- Workspace upsert writes both `paths` and `paths_order`:
  - `crates/workspace/src/persistence.rs:1323-1470`
- `--reuse` replaces workspace in an existing window:
  - `crates/zed/src/zed/open_listener.rs:509-526`
- `project_name` is a displayed name (used in window title):
  - `crates/workspace/src/workspace.rs:5123-5140`
- Project picker ("Search projects...") open folders list:
  - `crates/recent_projects/src/recent_projects.rs:144-193` (`get_open_folders()` → `project.visible_worktrees(cx)`, sorted alphabetically at line 191)
- Recent workspaces DB query (powers "File > Open Recent" and recent section of picker):
  - `crates/workspace/src/persistence.rs:1633-1641` (`recent_workspaces_query()` → `SELECT ... FROM workspaces ... ORDER BY timestamp DESC`)
  - `crates/workspace/src/persistence.rs:1803-1855` (`recent_workspaces_on_disk()` → filters to paths that exist on disk)

Our repo (`$HOME/codes/zed-project-workspace`):
- Strict JSON parsing (no comments):
  - `src/workspace_file.rs:60-75`
- Folder entry model only supports `{ path, name }` (folder-level unknown fields are dropped on rewrite):
  - `src/workspace_file.rs:28-43`
- Diff is set-based (reorders not detected):
  - `src/workspace_file.rs:157-180`
- Relative path computation is `strip_prefix` only:
  - `src/workspace_file.rs:182-194`
- DB selection is substring match (`LIKE`):
  - `src/workspace_db.rs:106-133`
- MCP list uses order-sensitive equality to compute `in_sync`:
  - `zed-prj-workspace-mcp/src/main.rs:79-105`
- Hook bootstrap writes `{workspace_id}:{filename}` into `project_name`:
  - `zed-prj-workspace-hook/src/discovery.rs:211-307`
- Hook sync reads only `paths` (not `paths_order`):
  - `zed-prj-workspace-hook/src/sync.rs:223-246`
