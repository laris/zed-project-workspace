# Design: Project Picker Pin + project_name Write Scope

Date: 2026-02-28
Status: **IMPLEMENTED** — Layer 4 picker sort hook is working. Extends `09_workspace_identity_pinning.md`.

---

## Part A: Picker Sort Hook (Layer 4)

### Problem

The project picker dropdown always sorts folders **alphabetically** by `root_name()`. Layers 1-3 pin the target root in the project panel and DB, but the picker dropdown ignores this ordering.

### Research Findings

From Zed source analysis (`crates/recent_projects/src/recent_projects.rs`):

| Aspect | How it works | Source |
|---|---|---|
| Folder names | `worktree.root_name()` — actual directory name | `recent_projects.rs:177` |
| Sort order | `entries.sort_by(\|a, b\| a.name.to_lowercase().cmp(...))` — always alphabetical | `recent_projects.rs:191` |
| Checkmark (✓) | Follows active worktree: override > active repo > first visible | `recent_projects.rs:152-167` |
| `project_name` effect | **NONE on picker** — only affects OS window title | `workspace.rs:5119` |

### Feasibility: ARM64 Binary Analysis

Key binary symbols (confirmed via `nm`, not stripped):

```
# Comparator closure (standalone 196-byte function):
0x106d3c740 t __RNCINv...sort_by...get_open_folders...OpenFolderEntry...

# get_open_folders itself (7152 bytes, inlines sort logic):
0x106dce990 t __RNv...recent_projects16get_open_folders

# Sort algorithm monomorphizations for OpenFolderEntry:
0x106cf1590 T driftsort_main<OpenFolderEntry, closure, Vec<OpenFolderEntry>>
0x106cf2840 T insertion_sort_shift_left<OpenFolderEntry, closure>
0x106cf2d34 t drift::sort<OpenFolderEntry, closure>
0x106cf34e0 T quicksort<OpenFolderEntry, closure>
```

**OpenFolderEntry struct** (from Zed source + disassembly):
```rust
struct OpenFolderEntry {
    worktree_id: WorktreeId,       // WorktreeId(usize) = 8 bytes
    name: SharedString,            // ArcCow<'static, str> = 24 bytes
    path: PathBuf,                 // 24 bytes
    branch: Option<SharedString>,  // 24 bytes (niche optimization)
    is_active: bool,               // 1 byte + 7 padding
}
// sizeof = 0x58 (88 bytes), confirmed by `mov w8, #0x58` in insertion_sort disassembly
// Compiler reorders: `name` is at offset 0 (confirmed by comparator loading from [x0])
```

SharedString (ArcCow) layout at entry offset 0:
- `[entry+0]`  = tag (u64): 0 = Borrowed (`&'static str`), 1 = Arc
- `[entry+8]`  = base_ptr: direct str pointer (Borrowed) or Arc pointer
- `[entry+16]` = len (u64): byte length of string
- Actual string data: `base_ptr + tag * 16` (Arc adds 16 bytes for strong+weak refcounts)

### Critical Discovery: Inlined Sort with Size Threshold

The compiler inlines `sort_by` → `stable::sort` → `driftsort_main` into `get_open_folders`. The inlined code has a **branch at element count 20**:

```asm
; Inside get_open_folders (inlined sort logic):
0x106dcfecc  cmp  x10, #0x14              ; compare count with 20
0x106dcfed0  b.hs 0x106dcfff4             ; if >= 20: go to driftsort_main path
; Small array path (< 20 elements):
0x106dcfed4  ldr  x0, [sp, #0x30]         ; data pointer
0x106dcfed8  mov  x1, x24                 ; element count
0x106dcfedc  bl   insertion_sort_shift_left<OpenFolderEntry, ...>
0x106dcfee8  b    0x106dcfaf4             ; continue (sort done)
; ...
; Large array path (>= 20 elements):
0x106dcfff4  sub  x2, x29, #0xe0
0x106dcfff8  ldr  x0, [sp, #0x30]
0x106dcfffc  mov  x1, x24
0x106dd0000  bl   driftsort_main<OpenFolderEntry, ...>
```

**With typical workspace sizes (< 20 folders), `driftsort_main` is NEVER called. Only `insertion_sort_shift_left` is called.**

---

### Approaches Tried (Chronological)

#### Approach 1: Hook Comparator Closure — FAILED

**Idea**: Replace the comparator closure (`fn(&OpenFolderEntry, &OpenFolderEntry) -> Ordering`) with a custom detour that pins the target entry.

**Symbol matched**: `sort_by...get_open_folders...OpenFolderEntry` (excluding sort algorithm symbols).

**What happened**:
- `Interceptor::replace()` returned Ok
- Target name was set correctly
- **One-shot DIAG log never fired** — the detour was never called

**Root cause**: The deferred install pattern (`DEFERRED_PICKER_PTR`) caused the hook to be installed in **child processes** (Zed spawns multiple processes via `DYLD_INSERT_LIBRARIES`), NOT in the main UI process (the one that renders the picker). The main UI process never got the hook.

**Secondary issue**: Even with eager install, the comparator closure — while having its own standalone symbol — was ALSO inlined into `insertion_sort_shift_left`. The `bl` to the standalone closure exists but `insertion_sort_shift_left` is called from the inlined sort logic in `get_open_folders`, not through `driftsort_main`.

**Lesson**: Deferred install via discovery callbacks is unreliable in multi-process environments. Install hooks eagerly during init.

#### Approach 2: Hook `driftsort_main` — FAILED

**Idea**: Hook the top-level sort entry point `driftsort_main<OpenFolderEntry, ...>`. Call original (sort completes), then post-process to move target to front.

**Symbol matched**: `driftsort_main...get_open_folders...OpenFolderEntry`.

**What happened**:
- Hook installed eagerly (in all processes, including main UI)
- Prologue patching **VERIFIED** — before/after bytes confirmed:
  ```
  BEFORE: [0xd10183ff, 0xa9025ff8, 0xa90357f6, 0xa9044ff4]  (sub sp; stp; stp; stp)
  AFTER:  [0x58000050, 0xd61f0200, addr_lo, addr_hi]         (ldr x16, #8; br x16; .quad detour)
  ```
- `Interceptor::replace()` IS working on ARM64 macOS (code pages successfully patched)
- **DIAG log never fired** — the detour was never called

**Root cause**: With 5 worktrees (< 20 elements), the inlined sort logic in `get_open_folders` branches directly to `insertion_sort_shift_left`, **skipping `driftsort_main` entirely** (see disassembly above).

**Lesson**: `Interceptor::replace()` works correctly on ARM64 macOS. The issue was hooking the wrong function — the compiler's inlined size threshold routes small arrays to insertion sort.

#### Approach 3: Hook `insertion_sort_shift_left` — WORKING ✅

**Idea**: Hook the function actually called for small arrays (< 20 elements). Call original (sort completes), then post-process to move target to front.

**Symbol matched**: `insertion_sort_shift_left...get_open_folders...OpenFolderEntry`.

**What happened**:
- Hook installed eagerly in all processes
- **DIAG log fires on first picker open**:
  ```
  picker_sort DIAG: len=5, target='zed-project-workspace', sorted=[
    Some("_my__zed-api-key"), Some("dylib-kit"), Some("gh-zed-industries__zed"),
    Some("zed-project-workspace"), Some("zed-yolo-hook")
  ]
  picker_sort: pinned 'zed-project-workspace' from index 3 to front (out of 5)
  ```
- `read_entry_name` correctly reads SharedString at offset 0 for all 5 entries
- Target successfully rotated from index 3 to index 0 on every picker open

---

### Final Implementation (Working)

**File: `zed-prj-workspace-hook/src/hooks/picker_sort.rs`**

| Item | Purpose |
|---|---|
| `SYMBOL_INCLUDE` | `["insertion_sort_shift_left", "get_open_folders", "OpenFolderEntry"]` |
| `SYMBOL_EXCLUDE` | `["drop_in_place", "vtable"]` |
| `ORIG_FN` | `AtomicPtr<c_void>` — saved original function pointer |
| `TARGET_NAME` | `AtomicPtr<String>` — leaked Box, process-lifetime |
| `set_target_name(name)` | Stores target folder name (called once from init or deferred discovery) |
| `install(interceptor, ptr)` | Frida `Interceptor::replace()` |
| `read_entry_name(ptr) -> Option<&str>` | Reads SharedString at offset 0; safety: tag≤1, non-null, len≤4096, valid UTF-8 |
| `sort_detour(data, len, offset, is_less)` | Calls original, then `pin_target_to_front()` |
| `pin_target_to_front(data, len)` | Scans sorted slice, rotates target entry to index 0 |

**Algorithm** (post-process after original sort completes):
```
sort_detour(data, len, offset, is_less):
    original(data, len, offset, is_less)     // alphabetical sort completes
    if TARGET_NAME is null: return           // no target → done

    for i in 0..len:
        name = read_entry_name(data + i * 0x58)
        if name == TARGET_NAME:
            if i == 0: return                // already at front
            rotate(data, i, 0)               // move to front: copy to tmp, shift, copy back
            break
```

**Key design decisions**:

| Decision | Rationale |
|---|---|
| Post-process instead of custom comparator | Simpler, more robust — original sort is unmodified |
| Hook `insertion_sort_shift_left` not `driftsort_main` | Threshold < 20 means insertion sort is always used for typical workspaces |
| Eager install (not deferred) | Multi-process: deferred install lands in wrong process |
| Detour is no-op when target is null | Safe to install before discovery completes |

**Modified files:**

| File | Change |
|---|---|
| `hooks/mod.rs` | Add `pub mod picker_sort;` |
| `lib.rs` | Symbol lookup + **eager** install in `init_inner()`; `resolve_picker_target_name()` for immediate; `try_deferred_picker_install()` for post-discovery target name set only (no re-install needed) |
| `sync.rs` | Call `try_deferred_picker_install()` after discovery succeeds (sets target name) |

### Graceful Degradation

| Failure | Behavior |
|---|---|
| Symbol not found (new Zed version) | Warning log, picker stays alphabetical |
| Target name not yet resolved | Detour calls original, post-process is no-op (no pin until discovery) |
| `read_entry_name` fails (layout changed) | Returns `None` → target not found → no rotation |
| Bad UTF-8 / corrupt memory | Returns `None` → no rotation |
| >= 20 folders (takes driftsort path) | insertion_sort hook doesn't fire; picker stays alphabetical for that call |

**No crash path.** Every failure results in the original alphabetical sort.

### Fragility Notes

| Concern | Mitigation |
|---|---|
| Symbol name hash changes per Zed build | `find_by_pattern` uses substring matching, not exact name |
| OpenFolderEntry field reorder (name moves from offset 0) | Tag check (must be 0 or 1) catches invalid layout |
| SharedString internal layout change | Length cap (4096) + UTF-8 validation catches corruption |
| Size threshold changes (currently 20) | If Zed changes threshold, may need to also hook `driftsort_main` |
| `sizeof(OpenFolderEntry)` changes (currently 0x58) | Entry scan reads wrong offsets → `read_entry_name` returns None → no rotation (safe) |
| Multiple Zed processes | Eager install in `init_inner()` ensures all processes are hooked |

### ARM64 Hooking Learnings

Key findings from debugging `Interceptor::replace()` on ARM64 macOS:

| Finding | Detail |
|---|---|
| **`Interceptor::replace()` WORKS** | Prologue patching confirmed via before/after byte reads. `ldr x16, #8; br x16; .quad addr` trampoline is correctly written. |
| **Code signing is not a blocker** | Frida uses `mach_vm_remap` to create writable alias of code pages, bypassing `r-x/r-x` max protection. |
| **Multiple processes load the dylib** | `DYLD_INSERT_LIBRARIES` injects into ALL Zed child processes. Each gets its own hook install. |
| **Deferred install is unreliable** | Discovery callbacks may fire in child processes, not the main UI process. Use eager install instead. |
| **Compiler inlining is aggressive** | `sort_by` → `stable::sort` → `driftsort_main` chain is fully inlined into `get_open_folders`. Size-based dispatch to different sort algorithms happens inside the inlined code. |

---

## Part B: project_name Write Scope Fix

### Problem

`write_project_name_to_roots()` writes `project_name` to **ALL** worktree roots. This pollutes non-primary roots with workspace identity that doesn't belong to them.

Current state (all trash except the primary):
```
zed-project-workspace/.zed/settings.json   → "117:zed-project-workspace"  ← correct
dylib-kit/.zed/settings.json               → "117:zed-project-workspace"  ← TRASH
gh-zed-industries__zed/.zed/settings.json  → "117:zed-project-workspace"  ← TRASH
_my__zed-api-key/.zed/settings.json        → "97:zed-project-workspace.code-workspace"  ← stale v1 TRASH
```

### Root Cause

`src/settings.rs:84-91`:
```rust
pub fn write_project_name_to_roots(roots: &[PathBuf], project_name: &str) -> io::Result<()> {
    for root in roots {
        write_project_name(root, project_name)?;  // writes to EVERY root
    }
    Ok(())
}
```

Called from 10 sites across `src/discovery.rs` (4) and `hook/src/discovery.rs` (6).

### Design: Triple-Match Validation

When reading `project_name` from multiple roots, identify the primary root with three checks:

```
For project_name = "{id}:{name}" found in root R:
  1. R.folder_name == name              (root IS the named project)
  2. R/{name}.code-workspace exists     (workspace file is present)
  3. id validates in Zed's DB           (workspace_id exists)

First root satisfying all three = definitive primary root.
```

| Root | project_name | folder==name? | .code-workspace? | Primary? |
|---|---|---|---|---|
| `zed-project-workspace` | `117:zed-project-workspace` | YES | YES | **YES** |
| `dylib-kit` | `117:zed-project-workspace` | NO | — | trash |
| `_my__zed-api-key` | `97:zed-project-workspace.code-workspace` | NO | — | stale v1 trash |
| `gh-zed-industries__zed` | `117:zed-project-workspace` | NO | — | trash |

The `folder_name == name` check alone is unambiguous. The `.code-workspace` check is defense-in-depth.

### Changes

**`src/settings.rs`:**
- Deprecate `write_project_name_to_roots()` → replace with writing only to primary root
- Add `find_primary_root(roots) -> Option<&Path>` — triple-match validation
- Add `cleanup_stale_project_names(roots, primary_root)` — remove `project_name` from non-primary roots

**`src/discovery.rs`** (4 call sites):
- Replace `write_project_name_to_roots(roots, &pn)` → `write_project_name(primary_root, &pn)`

**`hook/src/discovery.rs`** (6 call sites):
- `write_zed_settings_for_roots()` → `write_zed_settings_for_root()` (singular, primary only)

**Discovery read path:**
- When scanning roots for `project_name`, apply triple-match validation
- If conflicting values across roots: pick the one passing triple-match
- If none pass: fall back to `.code-workspace` scan → bootstrap

---

## Four-Layer Defense Strategy (Updated)

```
Layer 1: DB Hook (sqlite3_prepare_v2 detection + direct UPDATE)
├── Detects workspace write in Zed's DB
├── Corrects paths_order so target root is at index 0
└── DB ALWAYS has target root first — survives crash

Layer 2: Zed's Own Behavior (automatic, free)
├── Reads corrected paths_order from DB on workspace open
├── Non-alphabetical order → set worktrees_reordered=TRUE
└── Future add() calls APPEND (never alphabetical insert)

Layer 3: CLI reuse_folders (current session fix)
├── After DB correction, call MacOS/cli --reuse with pinned order
├── NOTE: must use MacOS/cli (CLI shim), NOT MacOS/zed (main binary)
└── Zed rebuilds in-memory worktrees Vec immediately

Layer 4: Picker sort hook (project picker dropdown)     ← IMPLEMENTED ✅
├── Hooks insertion_sort_shift_left<OpenFolderEntry> via Frida
├── Calls original sort (alphabetical), then post-processes
├── Scans sorted array for target, rotates to index 0
├── Eager install in all processes; target name set via discovery
└── Graceful degradation: falls through to alphabetical on any failure
```

### Updated Coverage Matrix

| Scenario | L1 (DB) | L2 (auto flag) | L3 (reuse) | L4 (picker) | Result |
|---|---|---|---|---|---|
| Fresh workspace open | Correct order | Flag=true | Not needed | Target at top | Fully pinned |
| New folder added | DB corrected | Already true | Not needed | Target at top | Appended, pinned |
| User drag-drop | DB corrected | Already true | Re-pins | Target at top | Re-pinned |
| Switch active file | — | — | — | Target stays top, ✓ moves | Visual consistency |
| Zed crash | DB correct | Next open: flag=true | Not needed | Target at top | Correct on restart |
| New Zed version (symbol gone) | Works | Works | Works | Falls back to alphabetical | L1-3 still work |
| >= 20 folders | Works | Works | Works | Falls back to alphabetical | L1-3 still work |

---

## References

- `crates/recent_projects/src/recent_projects.rs:144-193` — `get_open_folders()`, sort, checkmark logic
- `crates/recent_projects/src/recent_projects.rs:65-71` — `OpenFolderEntry` struct definition
- `crates/recent_projects/src/recent_projects.rs:1014-1087` — picker entry rendering
- `crates/title_bar/src/title_bar.rs:427-451` — `effective_active_worktree()`
- `crates/worktree/src/worktree_settings.rs:13` — `WorktreeSettings::project_name` (window title only)
- `crates/workspace/src/workspace.rs:5119` — `update_window_title()` reads `project_name`
- `crates/project/src/worktree_store.rs:187-193` — `visible_worktrees()` iterator order
- `crates/gpui/src/shared_string.rs:14` — `SharedString(ArcCow<'static, str>)`
- `crates/settings/src/settings.rs:86` — `WorktreeId(usize)` definition
