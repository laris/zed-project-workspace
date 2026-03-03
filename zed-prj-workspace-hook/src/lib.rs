//! zed-prj-workspace-hook: A cdylib injected into Zed via insert_dylib.
//!
//! **Event-driven architecture**: Hooks `sqlite3_prepare_v2` to detect workspace
//! write events. When Zed serializes workspace state, we intercept at the C boundary
//! — the only stable, version-independent hook point.
//!
//! On detecting a workspace write:
//! 1. Let the original sqlite3 call complete
//! 2. Wait 300ms for the full transaction to settle
//! 3. Auto-discover the workspace file (if not already known)
//! 4. Query the DB for current workspace paths
//! 5. Diff against the `.code-workspace` file
//! 6. Update the file if needed

mod config;
mod discovery;
mod ffi;
mod hooks;
mod logging;
mod socket_server;
mod symbols;
mod sync;

use ctor::ctor;
use frida_gum::{Gum, Process, interceptor::Interceptor};
use std::sync::OnceLock;

static GUM: OnceLock<Gum> = OnceLock::new();
static INIT_ONCE: std::sync::Once = std::sync::Once::new();

#[ctor]
fn init() {
    INIT_ONCE.call_once(init_inner);
}

fn init_inner() {
    let cfg = config::SyncConfig::from_env();

    logging::init();

    tracing::info!("=== zed-prj-workspace-hook v{} ===", env!("CARGO_PKG_VERSION"));
    tracing::info!("PID: {}", std::process::id());
    tracing::info!("Executable: {:?}", std::env::current_exe().unwrap_or_default());
    tracing::info!("Config: {:?}", cfg);

    if !cfg.enabled {
        tracing::info!("Sync disabled via ZED_PRJ_WORKSPACE_SYNC=0");
        return;
    }

    // Find Zed's DB path once at startup
    discovery::init_db_path();

    // Initialize Frida-Gum and find sqlite3_prepare_v2
    let gum = GUM.get_or_init(Gum::obtain);
    let process = Process::obtain(gum);
    let main_module = process.main_module();

    let prepare_ptr = match symbols::find_sqlite3_prepare_v2(gum, &main_module) {
        Some(ptr) => ptr,
        None => {
            tracing::error!("Cannot find sqlite3_prepare_v2 — hook NOT installed");
            return;
        }
    };

    tracing::info!("Found sqlite3_prepare_v2 at {:?}", prepare_ptr);

    // Find sqlite3_bind_int64 for per-window workspace_id capture
    let bind_ptr = symbols::find_sqlite3_bind_int64(gum, &main_module);
    if let Some(ref ptr) = bind_ptr {
        tracing::info!("Found sqlite3_bind_int64 at {:?}", ptr);
    } else {
        tracing::warn!(
            "Cannot find sqlite3_bind_int64 — falling back to LIMIT 1 discovery"
        );
    }

    // Determine app_id for registry
    let app_id = zed_prj_workspace::mapping::detect_zed_channel()
        .map(|ch| format!("zed-{ch}"))
        .unwrap_or_else(|| "zed".to_string());

    // Check hook registry for conflicts before installing
    let registry = dylib_hook_registry::HookRegistry::load(&app_id);
    if let Some(ref reg) = registry {
        if let Some(conflict) = reg.find_replace_conflict("sqlite3_prepare_v2", "zed-prj-workspace-hook") {
            tracing::warn!(
                "Hook conflict: '{}' already replaces sqlite3_prepare_v2. Chaining our detour after theirs.",
                conflict
            );
        }
        for hook in reg.hooks_by_load_order() {
            tracing::info!("  Registered hook: {} v{} (order={})",
                hook.name,
                hook.version.as_deref().unwrap_or("?"),
                hook.load_order.unwrap_or(0)
            );
        }
    }

    // Find the project picker sort comparator (optional — Layer 4)
    let picker_sort_ptr = symbols::find_by_pattern(
        &main_module,
        hooks::picker_sort::SYMBOL_INCLUDE,
        hooks::picker_sort::SYMBOL_EXCLUDE,
    );
    if let Some((ref name, ref ptr)) = picker_sort_ptr {
        tracing::info!("Found picker sort driftsort_main: {} at {:?}", name, ptr);
    } else {
        tracing::warn!(
            "Cannot find driftsort_main<OpenFolderEntry> — project picker pinning will NOT work \
             (picker will remain alphabetical)"
        );
    }

    let mut interceptor = Interceptor::obtain(gum);
    hooks::sqlite3_prepare::install(&mut interceptor, prepare_ptr);

    // Install bind_int64 hook for per-window workspace_id capture
    if let Some(ptr) = bind_ptr {
        hooks::sqlite3_bind::install(&mut interceptor, ptr);
    }

    // Install picker sort hook eagerly (if symbol found).
    // The detour is a no-op when target name is null, so it's safe to install
    // before discovery completes. This ensures the hook is active in the main
    // UI process — deferred install was landing in child processes instead.
    if let Some((_, ptr)) = picker_sort_ptr {
        if let Some(target_name) = resolve_picker_target_name() {
            hooks::picker_sort::set_target_name(target_name);
        }
        hooks::picker_sort::install(&mut interceptor, ptr);
    }

    // Register ourselves in the hook registry
    register_in_registry(&app_id);

    match discovery::db_path() {
        Some(path) => tracing::info!("Zed DB: {}", path.display()),
        None => tracing::warn!("Zed DB not found — sync will not work"),
    }

    // Start socket server for MCP communication (bypasses CLI)
    let channel = zed_prj_workspace::mapping::detect_zed_channel()
        .unwrap_or_else(|| "unknown".to_string());
    let sock_path = socket_server::start(channel, std::process::id());
    tracing::info!("Hook socket: {}", sock_path.display());

    tracing::info!("Event-driven workspace sync ready");
}

/// Try to resolve the target folder name from project_name in discoverable roots.
///
/// Best-effort at init time. Returns None if no project_name found yet.
fn resolve_picker_target_name() -> Option<String> {
    // Check if discovery has already set the workspace target
    if let Ok(guard) = discovery::WORKSPACE_TARGET.read() {
        if let Some((ws_file, _wid)) = &*guard {
            if let Some(name) = ws_file.file_stem().and_then(|n| n.to_str()) {
                return Some(name.to_string());
            }
        }
    }

    // Try reading project_name from the cwd
    let cwd = std::env::current_dir().ok()?;
    let raw = zed_prj_workspace::settings::read_project_name(&cwd)?;
    let (_id, name) = zed_prj_workspace::settings::parse_project_name(&raw);
    if !name.is_empty() {
        Some(name)
    } else {
        None
    }
}

/// Install the deferred picker sort hook after discovery resolves the target name.
///
/// Called from `sync.rs` when discovery completes.
/// The hook is already installed eagerly during init — this just sets the target name.
pub fn try_deferred_picker_install(target_name: &str) {
    hooks::picker_sort::set_target_name(target_name.to_string());
    tracing::info!("Deferred picker target set to '{}'", target_name);
}

/// Register this hook in the shared hook registry.
fn register_in_registry(app_id: &str) {
    use dylib_hook_registry::{HookEntry, HookRegistry};

    let mut registry = HookRegistry::load(app_id).unwrap_or_default();
    registry.app_id = Some(app_id.to_string());

    let dylib_path = format!(
        "{}/target/release/libzed_prj_workspace_hook.dylib",
        env!("CARGO_MANIFEST_DIR")
    );

    let entry = HookEntry::new("zed-prj-workspace-hook", &dylib_path)
        .with_version(env!("CARGO_PKG_VERSION"))
        .with_features(&["workspace-sync", "code-workspace-file", "mapping-file", "picker-pin"])
        .with_symbol("sqlite3_prepare_v2", "replace", "Detect workspace write SQL → sync to .code-workspace")
        .with_symbol("sqlite3_bind_int64", "replace", "Capture workspace_id from bound parameters")
        .with_symbol("OpenFolderEntry_sort_comparator", "replace", "Pin target folder to top of project picker")
        .with_load_order(2);

    registry.register(entry);

    if let Err(e) = registry.save(app_id) {
        tracing::debug!("Could not save hook registry: {} (non-fatal)", e);
    } else {
        tracing::info!("Registered in hook registry (app_id={})", app_id);
    }
}
