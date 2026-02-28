//! zed-prj-workspace: shared library for workspace file parsing, Zed DB access, and sync engine.
//!
//! This crate is the common dependency for both the hook cdylib (`zed-prj-workspace-hook`)
//! and the MCP server binary (`zed-prj-workspace-mcp`).

pub mod discovery;
pub mod hook_client;
pub mod hook_registry;
pub mod lock;
pub mod mapping;
pub mod paths;
pub mod pinning;
pub mod settings;
pub mod sync_engine;
pub mod workspace_db;
pub mod workspace_file;
