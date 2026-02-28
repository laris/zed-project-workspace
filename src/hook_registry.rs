//! Re-export of `dylib-hook-registry` crate.
//!
//! This module re-exports the standalone `dylib-hook-registry` crate so that
//! both the hook and MCP crates can use it through the shared library.

pub use dylib_hook_registry::*;
