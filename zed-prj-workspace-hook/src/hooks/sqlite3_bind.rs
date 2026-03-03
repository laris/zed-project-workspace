//! Hook for sqlite3_bind_int64 — captures workspace_id from bound parameters.
//!
//! Works in tandem with the sqlite3_prepare hook: when prepare detects workspace
//! write SQL, it records the statement pointer in `PENDING_WORKSPACE_STMT`. When
//! bind_int64 is called with index==1 on that statement, we capture the workspace_id
//! and enqueue it for sync.
//!
//! Hot path: one atomic load + null check per bind call. Zero overhead when no
//! workspace write is pending.

use frida_gum::{NativePointer, interceptor::Interceptor};
use libc::c_int;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

type BindInt64Fn = unsafe extern "C" fn(
    *mut c_void, // sqlite3_stmt*
    c_int,       // index (1-based)
    i64,         // value
) -> c_int;

/// Original function pointer (set once at install).
static ORIG_BIND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// The stmt pointer from the most recent workspace write prepare.
/// Set by sqlite3_prepare hook, consumed by this hook.
/// Null means "no workspace write pending".
pub static PENDING_WORKSPACE_STMT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Check if the bind hook is installed (original function pointer is set).
pub fn is_installed() -> bool {
    !ORIG_BIND.load(Ordering::Relaxed).is_null()
}

/// Install the sqlite3_bind_int64 hook using Frida's interceptor.replace().
pub fn install(interceptor: &mut Interceptor, bind_ptr: NativePointer) {
    let original = interceptor
        .replace(
            bind_ptr,
            NativePointer(bind_int64_detour as *mut c_void),
            NativePointer(std::ptr::null_mut()),
        )
        .expect("Failed to install sqlite3_bind_int64 hook");

    ORIG_BIND.store(original.0, Ordering::Release);

    tracing::info!("Hook installed: sqlite3_bind_int64");
}

/// Detour for sqlite3_bind_int64.
///
/// Hot path: one atomic load. When PENDING_WORKSPACE_STMT is null (the common case),
/// returns immediately after calling the original function.
unsafe extern "C" fn bind_int64_detour(
    stmt: *mut c_void,
    index: c_int,
    value: i64,
) -> c_int {
    // Always call original first — never block Zed's DB operations
    let orig: BindInt64Fn = unsafe { std::mem::transmute(ORIG_BIND.load(Ordering::Acquire)) };
    let result = unsafe { orig(stmt, index, value) };

    // Fast path: check if this stmt is the one we're watching
    let pending = PENDING_WORKSPACE_STMT.load(Ordering::Acquire);
    if pending.is_null() || stmt != pending {
        return result;
    }

    // Only care about parameter index 1 (workspace_id is always ?1)
    if index == 1 {
        // Clear the pending flag immediately (consume once)
        PENDING_WORKSPACE_STMT.store(std::ptr::null_mut(), Ordering::Release);

        tracing::info!("Captured workspace_id={} from bind_int64", value);
        crate::sync::enqueue_workspace_sync(value);
    }

    result
}
