# Compatibility Verification: Zed Preview v0.232.0

> Date: 2026-04-12
> Installed app: `/Applications/Zed Preview.app`
> App version: `0.232.0`
> App build: `0.232.0+preview.219.957fa4d9e3530ba0c8773a92943f42263b25ca1f`
> CI verification: [Run 24297100125](https://github.com/laris/zed-project-workspace/actions/runs/24297100125) (PASS, first try)

---

## 1. Scope

This note records the 2026-04-12 verification that `zed-project-workspace`
(both the hook and MCP server) is compatible with Zed Preview 0.232.0.

**Result: compatible after one code fix (picker_sort symbol change).**

The sqlite3 hooks are fully stable. The picker pinning hook required updating
the symbol pattern due to a Rust standard library sort algorithm change.

---

## 2. Hook Symbol Availability

### 2.1 sqlite3_prepare_v2 (primary — workspace write detection)

```text
000000010aa0ab48 T _sqlite3_prepare_v2
```

Statically linked SQLite. Stable across all tested versions (v0.225.9 → v0.232.0).

### 2.2 sqlite3_bind_int64 (secondary — workspace_id capture)

```text
000000010aa13b34 T _sqlite3_bind_int64
```

Standard SQLite C API. Stable.

### 2.3 OpenFolderEntry sort function — BREAKING CHANGE FIXED

**v0.231.1 and earlier:**
```text
_RINvNtNtNtNtCsbUtogaBoXXO_4core5slice4sort6stable21insertion_sort_shift_leftNtCs...15OpenFolderEntry...
```

**v0.232.0:**
```text
(symbol does not exist — insertion_sort_shift_left removed)
```

**New symbols in v0.232.0:**
```text
_RINv...core..slice..sort..stable..driftsort_main..OpenFolderEntry..get_open_folders...
_RINv...core..slice..sort..stable..quicksort..quicksort..OpenFolderEntry..get_open_folders...
```

**Root cause:** Rust's standard library updated its stable sort implementation.
The old `insertion_sort_shift_left` (used for arrays < 20 elements) was replaced
by `driftsort_main` as the primary entry point. The sort algorithm change affects
all Rust programs using `[T]::sort_by()`.

**Demangled new symbol:**
```
core::slice::sort::stable::driftsort_main::<
    recent_projects::OpenFolderEntry,
    <[OpenFolderEntry]>::sort_by<get_open_folders::{closure#2}>::{closure#0},
    alloc::vec::Vec<OpenFolderEntry>
>
```

---

## 3. Fix Applied: picker_sort.rs

### 3.1 Symbol pattern update

```rust
// Before (v0.228–v0.231):
pub const SYMBOL_INCLUDE: &[&str] = &[
    "insertion_sort_shift_left",
    "get_open_folders",
    "OpenFolderEntry",
];

// After (v0.232+, with legacy fallback):
pub const SYMBOL_INCLUDE: &[&str] = &[
    "driftsort_main",
    "get_open_folders",
    "OpenFolderEntry",
];

pub const SYMBOL_INCLUDE_LEGACY: &[&str] = &[
    "insertion_sort_shift_left",
    "get_open_folders",
    "OpenFolderEntry",
];
```

### 3.2 Fallback logic in lib.rs

```rust
let picker_sort_ptr = symbols::find_by_pattern(
    &main_module,
    hooks::picker_sort::SYMBOL_INCLUDE,       // driftsort_main (v0.232+)
    hooks::picker_sort::SYMBOL_EXCLUDE,
).or_else(|| {
    tracing::info!("driftsort_main not found, trying legacy...");
    symbols::find_by_pattern(
        &main_module,
        hooks::picker_sort::SYMBOL_INCLUDE_LEGACY, // insertion_sort_shift_left
        hooks::picker_sort::SYMBOL_EXCLUDE,
    )
});
```

### 3.3 Function signature compatibility

| | insertion_sort_shift_left | driftsort_main |
|-|--------------------------|----------------|
| x0 | data: *mut u8 | data: *mut u8 |
| x1 | len: usize | len: usize |
| x2 | offset: usize | is_less: &mut F |
| x3 | is_less: *mut c_void | scratch: &mut Vec<T> |

Our detour only uses `x0` (data) and `x1` (len) for the pin-to-front logic.
The other arguments are forwarded to the original function unchanged. The
generic 4-arg `extern "C"` function type works for both:

```rust
type SortFn = unsafe extern "C" fn(*mut u8, usize, usize, *mut c_void);
```

### 3.4 OpenFolderEntry layout — CONFIRMED UNCHANGED

Verified via disassembly of v0.232.0 binary:

```asm
106a34c10: 52800b09    mov  w9, #0x58    ; ENTRY_SIZE = 88 bytes (unchanged)
```

`sizeof(OpenFolderEntry)` = 0x58 = 88 bytes. Same as v0.230.0 and v0.231.1.
`name: SharedString` remains at offset 0 (compiler field reordering unchanged).

---

## 4. CI Verification

### 4.1 First CI run — PASS

**Run:** https://github.com/laris/zed-project-workspace/actions/runs/24297100125
**Result:** PASS (2m5s, first try)

Log output confirms all hooks installed:

```
Found sqlite3_prepare_v2 at NativePointer(0x10d3dfb48)
Found sqlite3_bind_int64 at NativePointer(0x10d3e8b34)
Searching for symbol matching ["driftsort_main", "get_open_folders", "OpenFolderEntry"]
Found picker sort function: _RINvNtNtNt...driftsort_main...OpenFolderEntry... at NativePointer(0x109614bd0)
Hook installed: sqlite3_prepare_v2
Hook installed: sqlite3_bind_int64
Hook installed: picker_sort (sort detour for OpenFolderEntry)
Event-driven workspace sync ready
YOLO mode ACTIVE (pid=6845)
```

### 4.2 Post-locked_register fix — PASS

**Run:** https://github.com/laris/zed-project-workspace/actions/runs/24307266929
**Result:** PASS (1m0s, cached build)

### 4.3 Workflow setup

Created `.github/workflows/verify-hook.yml` modeled on zed-yolo-hook's workflow:
- Runner: `macos-15` (ARM64 M1)
- Triggers: `workflow_dispatch`, `schedule` (weekly), `push` to main
- Steps: download Zed → build hook → inject → launch → check log markers
- Includes `Check symbols` step to verify sqlite3 + OpenFolderEntry symbols
- Picker sort status reported as informational (not fatal if missing)
- Artifacts: hook log uploaded, 14-day retention

---

## 5. Dependency Changes

### 5.1 Path to git dependencies

All three `dylib-kit` path dependencies converted to git for CI compatibility:

```toml
# Cargo.toml (root):
dylib-hook-registry = { git = "https://github.com/laris/dylib-kit" }

# zed-prj-workspace-hook/Cargo.toml:
dylib-hook-registry = { git = "https://github.com/laris/dylib-kit" }

# xtask/Cargo.toml:
dylib-patcher = { git = "https://github.com/laris/dylib-kit" }
dylib-hook-registry = { git = "https://github.com/laris/dylib-kit" }
```

Cargo resolves workspace member crates by name from git repos automatically.

### 5.2 Registry race condition fix

Switched `register_in_registry` from manual load-register-save to
`HookRegistry::locked_register()` (added in `dylib-kit` commit `11c764a`).
Uses `fs2` file locking to prevent concurrent `#[ctor]` race conditions when
multiple Zed processes load injected dylibs simultaneously.

---

## 6. Version Compatibility Matrix (Updated)

| Zed Version | sqlite3 hooks | picker_sort | Notes |
|-------------|--------------|-------------|-------|
| v0.225.9 | OK | OK (insertion_sort) | Legacy layout |
| v0.228.x | OK | OK (insertion_sort) | |
| v0.230.0 | OK | OK (insertion_sort) | New niche encoding |
| v0.231.1 | OK | OK (insertion_sort) | Verified 2026-04-03 |
| **v0.232.0** | **OK** | **OK (driftsort_main)** | **Symbol pattern updated** |

---

## 7. Commits

| Commit | Description |
|--------|-------------|
| `99e2ca4` | Fix picker_sort for driftsort_main + add CI workflow + git deps |
| `5fae6a2` | Use locked_register to prevent registry race conditions |
