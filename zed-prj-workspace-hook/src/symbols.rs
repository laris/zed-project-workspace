//! Symbol lookup helpers for Frida-based hooking.
//!
//! Generic utility: searches a module's exports/symbols by include/exclude patterns.
//! Also provides a specific helper for finding sqlite3_prepare_v2.

use frida_gum::{Gum, Module, NativePointer};
use std::ffi::c_void;

/// Find a symbol in `module` whose name contains ALL `include` patterns
/// and NONE of the `exclude` patterns.
///
/// Searches exports first (faster), then falls back to full symbol table.
pub fn find_by_pattern(
    module: &Module,
    include: &[&str],
    exclude: &[&str],
) -> Option<(String, NativePointer)> {
    tracing::info!(
        "Searching for symbol matching {:?} (excluding {:?})",
        include, exclude
    );

    for export in module.enumerate_exports() {
        let name = &export.name;
        if include.iter().all(|pat| name.contains(pat))
            && exclude.iter().all(|pat| !name.contains(pat))
        {
            return Some((
                name.clone(),
                NativePointer(export.address as *mut c_void),
            ));
        }
    }

    tracing::info!("Not found in exports, trying symbols...");
    for sym in module.enumerate_symbols() {
        let name = &sym.name;
        if include.iter().all(|pat| name.contains(pat))
            && exclude.iter().all(|pat| !name.contains(pat))
        {
            return Some((
                name.clone(),
                NativePointer(sym.address as *mut c_void),
            ));
        }
    }

    None
}

/// Find sqlite3_prepare_v2 in the main binary or system libsqlite3.
///
/// Zed statically links sqlite3 — look in the main binary first,
/// then fall back to the system libsqlite3.dylib.
pub fn find_sqlite3_prepare_v2(gum: &Gum, main_module: &Module) -> Option<NativePointer> {
    main_module
        .find_export_by_name("sqlite3_prepare_v2")
        .or_else(|| {
            tracing::info!("sqlite3_prepare_v2 not in main binary, trying libsqlite3.dylib");
            let sys_module = Module::load(gum, "libsqlite3.dylib");
            sys_module.find_export_by_name("sqlite3_prepare_v2")
        })
}
