# zed-project-workspace

Workspace sync hook + MCP tooling for Zed on macOS.

This project tracks workspace folder changes and syncs them with `.code-workspace` files. It includes:

- `zed-prj-workspace-hook`: injected hook crate
- `zed-prj-workspace-mcp`: MCP server binary
- shared workspace logic in `src/`
- `xtask` for patch/deploy/verification workflows

## Related Repositories

- `dylib-kit`: https://github.com/laris/dylib-kit
- `zed-yolo-hook`: https://github.com/laris/zed-yolo-hook

## How This Repo Uses dylib-kit

`xtask` depends on `dylib-kit` crates:

- `dylib-patcher`: shared patch/restore/codesign/verify workflow
- `dylib-hook-registry`: hook registry metadata and health checks

Command UX is unified through `cargo patch` aliases.

## Quickstart

```bash
# Build everything
cargo build --release

# Patch this hook into Zed Preview
cargo patch

# Verify registry + artifact hashes + hook status
cargo patch status

# Runtime smoke checks (MCP + mapping + workspace + DB)
cargo patch doctor

# Restore original app binary
cargo patch restore
```

## Standalone vs Stacked Use

- Standalone: use only this repo's `cargo patch` commands.
- Stacked with `zed-yolo-hook`: patch yolo first from its repo, then patch this repo.

Each repo remains independently patchable; no hard dependency on patching the other first.

## Docs

See `docs/` for architecture, deployment, and design notes.
