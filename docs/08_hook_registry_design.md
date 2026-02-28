# Hook Registry & dylib-kit SDK Integration

Updated: 2026-02-28

> This design was extracted into the standalone `dylib-kit` SDK at `~/codes/dylib-kit/`.
> See `~/codes/dylib-kit/docs/sdk_design.md` for the full SDK documentation.

## Status: Implemented

The hook registry and patcher SDK are fully implemented and integrated:

| Component | Location | Status |
|-----------|----------|--------|
| `dylib-hook-registry` crate | `~/codes/dylib-kit/crates/dylib-hook-registry/` | 9 tests passing |
| `dylib-patcher` crate | `~/codes/dylib-kit/crates/dylib-patcher/` | Builds clean |
| yolo-hook xtask integration | `~/codes/zed-yolo-hook/xtask/` | 55 lines (was 416) |
| workspace-hook xtask integration | `~/codes/zed-project-workspace/xtask/` | 51 lines (was 411) |
| Hook runtime registration | Both hooks' `lib.rs` | Registers on init |

## Key Design Decisions Made

1. **Generic naming**: `dylib-kit` not `zed-hook-registry` — works for any macOS app, not Zed-specific
2. **App-scoped registries**: `~/.config/dylib-hooks/{app_id}/registry.json` — isolated per host app
3. **No shell scripts**: Everything through `cargo patch` xtask commands
4. **Artifact tracking**: SHA-256 hash + git commit stored at patch time, stale detection on `status`
5. **Health check verification**: Log-based — each hook declares success/failure markers and log pattern
6. **Load order**: Deterministic injection order via `load_order` field, Frida chains naturally

## How Both Hooks Coexist

| Hook | Method | Symbol | Conflict? |
|------|--------|--------|-----------|
| `zed-yolo-hook` (order=1) | `attach` (listener) | Rust permission functions | No — listeners stack |
| `zed-prj-workspace-hook` (order=2) | `replace` (detour) | `sqlite3_prepare_v2` | No — different symbol |

If a future hook also `replace`'d `sqlite3_prepare_v2`, Frida would chain: second hook's "original" = first hook's detour.

## Workflow

```
# From either hook project:
cargo patch                   # build + inject all hooks + sign
cargo patch --verify          # build + inject + launch + verify health
cargo patch verify            # just verify (already patched)
cargo patch status            # show registry + artifact hashes + stale check
cargo patch remove            # remove this hook only, keep others
cargo patch restore           # restore original binary, remove all
```
