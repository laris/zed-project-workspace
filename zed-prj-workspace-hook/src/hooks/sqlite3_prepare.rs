//! Hook for sqlite3_prepare_v2 — detects workspace write SQL and triggers sync.
//!
//! Refactored:
//! - `AtomicPtr` for the original function pointer (lock-free on every SQL call)
//! - Also matches DELETE on workspaces table
//! - Simplified `is_workspace_write()` using trimmed SQL only

use frida_gum::{NativePointer, interceptor::Interceptor};
use libc::{c_char, c_int, c_void};
use std::ffi::CStr;
use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

// Target state constants for the fast-path atomic.
pub const TARGET_NONE: u8 = 0;    // No target, discovery exhausted — skip all inspection
pub const TARGET_SET: u8 = 1;     // Target available — proceed to sync
pub const TARGET_PENDING: u8 = 2; // Discovery not yet attempted — inspect SQL, trigger discovery

/// Fast-path state flag. The hot path (`prepare_v2_detour`) reads only this
/// single byte to decide whether to inspect SQL. Zero overhead when TARGET_NONE.
pub static TARGET_STATE: AtomicU8 = AtomicU8::new(TARGET_PENDING);

type PrepareV2Fn = unsafe extern "C" fn(
    *mut c_void,       // sqlite3*
    *const c_char,     // zSql
    c_int,             // nByte
    *mut *mut c_void,  // ppStmt
    *mut *const c_char, // pzTail
) -> c_int;

/// Lock-free storage for the original sqlite3_prepare_v2 function pointer.
/// Set once at install time, then read on every SQL call without any lock.
static ORIG_PREPARE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Install the sqlite3_prepare_v2 hook using Frida's interceptor.replace().
pub fn install(interceptor: &mut Interceptor, prepare_ptr: NativePointer) {
    unsafe {
        let original = interceptor
            .replace(
                prepare_ptr,
                NativePointer(prepare_v2_detour as *mut c_void),
                NativePointer(std::ptr::null_mut()),
            )
            .expect("Failed to install sqlite3_prepare_v2 hook");

        ORIG_PREPARE.store(original.0, Ordering::Release);
    }

    tracing::info!("Hook installed: sqlite3_prepare_v2");
}

/// Detour for sqlite3_prepare_v2.
///
/// Fast path: reads a single AtomicU8. When TARGET_NONE, returns immediately
/// with zero overhead beyond an atomic load + function pointer load.
unsafe extern "C" fn prepare_v2_detour(
    db: *mut c_void,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut c_void,
    pz_tail: *mut *const c_char,
) -> c_int {
    // Always call original first — never block Zed's DB operations
    let orig: PrepareV2Fn = unsafe { std::mem::transmute(ORIG_PREPARE.load(Ordering::Acquire)) };
    let result = unsafe { orig(db, z_sql, n_byte, pp_stmt, pz_tail) };

    // Fast exit: single atomic byte check
    let state = TARGET_STATE.load(Ordering::Relaxed);
    if state == TARGET_NONE {
        return result;
    }

    // Inspect SQL text (best-effort, never crash Zed)
    if !z_sql.is_null()
        && let Ok(sql_str) = unsafe { CStr::from_ptr(z_sql) }.to_str()
            && is_workspace_write(sql_str) {
                tracing::debug!("SQL matched workspace write filter (len={})", sql_str.len());

                if crate::hooks::sqlite3_bind::is_installed() {
                    // Record the statement pointer so bind_int64 hook can capture workspace_id.
                    // pp_stmt points to the sqlite3_stmt* that was just prepared.
                    if !pp_stmt.is_null() {
                        let stmt_ptr = unsafe { *pp_stmt };
                        if !stmt_ptr.is_null() {
                            crate::hooks::sqlite3_bind::PENDING_WORKSPACE_STMT
                                .store(stmt_ptr, std::sync::atomic::Ordering::Release);
                        }
                    }
                } else {
                    // Legacy path: bind hook not available, use LIMIT 1 discovery
                    crate::sync::on_workspace_write_detected(sql_str, state);
                }
            }

    result
}

/// Fast check: is this SQL a workspace write?
///
/// Matches INSERT, UPDATE, or DELETE on the `workspaces` table.
/// Uses trimmed SQL only (handles leading whitespace).
pub fn is_workspace_write(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    if trimmed.len() < 20 {
        return false;
    }

    // Must be a write operation (INSERT, UPDATE, or DELETE)
    let bytes = trimmed.as_bytes();
    let is_write = bytes[..7].eq_ignore_ascii_case(b"INSERT ")
        || bytes[..7].eq_ignore_ascii_case(b"UPDATE ")
        || bytes[..7].eq_ignore_ascii_case(b"DELETE ");

    if !is_write {
        return false;
    }

    // Must reference "workspaces" table
    bytes
        .windows(10)
        .any(|w| w.eq_ignore_ascii_case(b"workspaces"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_workspace_write_insert() {
        assert!(is_workspace_write(
            "INSERT INTO workspaces (workspace_id, paths) VALUES (?, ?)"
        ));
    }

    #[test]
    fn test_is_workspace_write_update() {
        assert!(is_workspace_write(
            "UPDATE workspaces SET paths = ? WHERE workspace_id = ?"
        ));
    }

    #[test]
    fn test_is_workspace_write_delete() {
        assert!(is_workspace_write(
            "DELETE FROM workspaces WHERE workspace_id = ?"
        ));
    }

    #[test]
    fn test_is_workspace_write_with_leading_whitespace() {
        assert!(is_workspace_write(
            "  INSERT INTO workspaces (workspace_id, paths) VALUES (?, ?)"
        ));
    }

    #[test]
    fn test_is_workspace_write_select_false() {
        assert!(!is_workspace_write("SELECT * FROM workspaces"));
    }

    #[test]
    fn test_is_workspace_write_short_string() {
        assert!(!is_workspace_write("SELECT 1"));
    }

    #[test]
    fn test_is_workspace_write_unrelated() {
        assert!(!is_workspace_write("INSERT INTO items (id, name) VALUES (1, 'test')"));
    }
}
