# Compatibility Verification: Zed Preview v0.233.0

> Date: 2026-04-17
> Installed app: `/Applications/Zed Preview.app`
> App version: `0.233.0` (build `20260415.163537`)
> Zed source commit: `b8b7aad70a8127fa6deb8e83ba80725fca04c0fd`
> Previous verified version: `v0.232.0` (commit `957fa4d9e3`, docs/13)

---

## 1. Summary

**Result: fully compatible, no code changes required.**

All three hook targets are stable between v0.232.0 and v0.233.0:

| Hook target                           | Type                 | v0.233.0 status |
|---------------------------------------|----------------------|-----------------|
| `sqlite3_prepare_v2`                  | C API                | stable          |
| `sqlite3_bind_int64`                  | C API                | stable          |
| `core::slice::sort::stable::driftsort_main<OpenFolderEntry, …>` | Rust mono | stable |

`OpenFolderEntry` source is byte-identical between the two commits, and the
binary still emits `mov w9, #0x58` as the entry size constant inside the
driftsort monomorphization — confirming `ENTRY_SIZE = 0x58` is unchanged.

Contrast with `zed-yolo-hook`, which required an offset recalibration on
v0.233.0 because `AcpThread` gained a new `cost: Option<SessionCost>` field
and Rust's `repr(Rust)` relocated the `entries: Vec<_>` by 0x20. This repo
does not touch `AcpThread`, so it is unaffected.

---

## 2. Symbol Verification (v0.233.0 binary)

From `/Applications/Zed Preview.app/Contents/MacOS/zed.original`:

```
000000010ab13008 T _sqlite3_prepare_v2
000000010ab1bff4 T _sqlite3_bind_int64
0000000106abd8c4 T __RINvNtNtNtCsbUtogaBoXXO_4core5slice4sort6stable14driftsort_main
                    NtCsd7XRdP7tZex_15recent_projects15OpenFolderEntry
                    NCINvMNtCsipk7xt2ATo7_5alloc5sliceSBZ_7sort_by
                    NCNvB11_16get_open_folders s3_0 E0
                    INtNtB1V_3vec3Vec BZ_ EE B11_
```

All three symbols are present. Pattern matches in the hook
(`["driftsort_main", "get_open_folders", "OpenFolderEntry"]`) resolve the sort
function hash-agnostically, so no pattern updates are needed.

---

## 3. Source-Level Comparison

### 3.1 `OpenFolderEntry` struct — UNCHANGED

Diff of `crates/recent_projects/src/recent_projects.rs` lines 69–95 (v0.232.0)
vs 70–96 (v0.233.0): empty. The struct layout, field types, and field order
are byte-for-byte identical.

### 3.2 `driftsort_main` ENTRY_SIZE — UNCHANGED

From `otool -tvV` on the `driftsort_main<OpenFolderEntry>` symbol in v0.233.0:

```asm
0000000106abd8e0	mov	w9, #0x631d
0000000106abd904	mov	w9, #0x58     ← ENTRY_SIZE = 88 bytes
```

Same as v0.232.0. `sizeof(OpenFolderEntry) = 0x58` confirmed.

---

## 4. Local Build

```
$ cargo build --release -p zed-prj-workspace-hook
Finished `release` profile [optimized] target(s) in 2m 49s
```

Clean build. 24 pre-existing dead-code warnings, nothing new.

---

## 5. CI Verification

See the corresponding GitHub Actions run referenced in this commit's message.
Workflow: `.github/workflows/verify-hook.yml`, runner `macos-15`, dispatches
against the latest `v0.233.0-pre` Zed release from `zed-industries/zed`.

---

## 6. Version Compatibility Matrix (Updated)

| Zed Version   | sqlite3 hooks | picker_sort            | Notes                                  |
|---------------|---------------|------------------------|----------------------------------------|
| v0.225.9      | OK            | OK (insertion_sort)    | Legacy layout                          |
| v0.228.x      | OK            | OK (insertion_sort)    |                                        |
| v0.230.0      | OK            | OK (insertion_sort)    | New niche encoding                     |
| v0.231.1      | OK            | OK (insertion_sort)    | Verified 2026-04-03                    |
| v0.232.0      | OK            | OK (driftsort_main)    | Symbol pattern updated                 |
| **v0.233.0**  | **OK**        | **OK (driftsort_main)**| **No changes needed**                  |
