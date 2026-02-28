//! macOS libdispatch FFI bindings.
//!
//! Provides `dispatch_async_f` for scheduling work on the main queue.
//! Used to run commands on Zed's main thread from the socket server thread.
//! (Will be used in Phase 2 for direct OpenListener injection.)

#![allow(dead_code)]

use std::ffi::c_void;

unsafe extern "C" {
    #[link_name = "_dispatch_main_q"]
    static _dispatch_main_q: c_void;
    pub fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// Returns a pointer to the main dispatch queue.
///
/// # Safety
/// Must be called from a process that links libdispatch (all macOS apps).
pub unsafe fn get_main_queue() -> *const c_void {
    &raw const _dispatch_main_q
}
