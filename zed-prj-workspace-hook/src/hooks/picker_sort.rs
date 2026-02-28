//! Hook for the project picker sort in `get_open_folders()`.
//!
//! Hooks `insertion_sort_shift_left<OpenFolderEntry, ...>` — the sort function
//! used for arrays with < 20 elements (which covers typical workspace sizes).
//!
//! The inlined sort logic in `get_open_folders` has a threshold at 20 elements:
//! - < 20 elements: calls `insertion_sort_shift_left` directly
//! - >= 20 elements: calls `driftsort_main`
//!
//! This hook intercepts the insertion sort, lets it complete normally
//! (alphabetical by name), then scans the sorted slice to move the target
//! folder to the front.
//!
//! Layout (confirmed by disassembly):
//! - `sizeof(OpenFolderEntry)` = 0x58 (88 bytes)
//! - `name: SharedString` is at offset 0 (compiler reordered to first field)

use frida_gum::{NativePointer, interceptor::Interceptor};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// Symbol search patterns for `insertion_sort_shift_left<OpenFolderEntry, ...>`.
pub const SYMBOL_INCLUDE: &[&str] = &[
    "insertion_sort_shift_left",
    "get_open_folders",
    "OpenFolderEntry",
];

/// Exclude patterns to avoid matching unrelated symbols.
pub const SYMBOL_EXCLUDE: &[&str] = &["drop_in_place", "vtable"];

/// Element size of OpenFolderEntry in bytes (from disassembly: `mov w8, #0x58`).
const ENTRY_SIZE: usize = 0x58;

/// Original function pointer type.
///
/// ARM64 Rust ABI for `insertion_sort_shift_left(v: &mut [T], offset: usize, is_less: &mut F)`:
/// x0=data, x1=len, x2=offset, x3=is_less
type SortFn = unsafe extern "C" fn(*mut u8, usize, usize, *mut c_void);

/// Saved original function pointer — set once at install time.
static ORIG_FN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Target folder name to pin at top (e.g., "zed-project-workspace").
/// Stored as a leaked `Box<String>` — lives for process lifetime.
static TARGET_NAME: AtomicPtr<String> = AtomicPtr::new(std::ptr::null_mut());

/// One-shot diagnostic flag.
static DIAG_LOGGED: AtomicBool = AtomicBool::new(false);

/// Set the target folder name for pinning.
pub fn set_target_name(name: String) {
    let ptr = Box::into_raw(Box::new(name));
    TARGET_NAME.store(ptr, Ordering::Release);
    tracing::info!("picker_sort: target name set to '{}'", unsafe { &*ptr });
}

/// Install the hook via Frida `Interceptor::replace()`.
pub fn install(interceptor: &mut Interceptor, fn_ptr: NativePointer) {
    unsafe {
        let original = interceptor
            .replace(
                fn_ptr,
                NativePointer(sort_detour as *mut c_void),
                NativePointer(std::ptr::null_mut()),
            )
            .expect("Failed to install picker_sort hook");

        ORIG_FN.store(original.0, Ordering::Release);
    }

    tracing::info!("Hook installed: picker_sort (insertion_sort_shift_left for OpenFolderEntry)");
}

/// Read the `name: SharedString` field from an OpenFolderEntry pointer.
///
/// SharedString is `ArcCow<'static, str>` with layout (confirmed by ARM64 disassembly):
///
/// ```text
/// offset 0:  tag (u64)      — 0 = Borrowed (&'static str), 1 = Arc
/// offset 8:  base_ptr (u64) — pointer to str data (Borrowed) or Arc allocation
/// offset 16: len (u64)      — byte length of the string
///
/// Actual string data: base_ptr + tag * 16
///   Borrowed: tag=0, data is at base_ptr directly
///   Arc:      tag=1, Arc has 16 bytes of refcounts before data
/// ```
///
/// # Safety
///
/// Caller must ensure `entry` points to a valid OpenFolderEntry whose
/// `name` field (SharedString) is at offset 0 (confirmed by compiler field reordering).
unsafe fn read_entry_name<'a>(entry: *const u8) -> Option<&'a str> {
    if entry.is_null() {
        return None;
    }

    let words = entry as *const u64;
    let tag = *words;
    let base_ptr = *words.add(1);
    let len = *words.add(2);

    // Tag must be 0 (Borrowed) or 1 (Arc)
    if tag > 1 {
        return None;
    }

    // Sanity checks on pointer and length
    if base_ptr == 0 || len == 0 || len > 4096 {
        return None;
    }

    // str_ptr = base_ptr + tag * 16
    let str_ptr = (base_ptr as usize).wrapping_add((tag as usize) * 16) as *const u8;

    // Must be a reasonable user-space pointer
    if (str_ptr as usize) < 0x1000 {
        return None;
    }

    let slice = std::slice::from_raw_parts(str_ptr, len as usize);
    std::str::from_utf8(slice).ok()
}

/// Shared post-processing: scan sorted array for target and rotate to front.
unsafe fn pin_target_to_front(data: *mut u8, len: usize) {
    let target_ptr = TARGET_NAME.load(Ordering::Acquire);
    if target_ptr.is_null() {
        return;
    }
    let target = &*target_ptr;

    // One-shot diagnostic
    if !DIAG_LOGGED.swap(true, Ordering::Relaxed) {
        let mut names = Vec::new();
        for i in 0..len.min(10) {
            let entry = data.add(i * ENTRY_SIZE);
            names.push(format!("{:?}", read_entry_name(entry)));
        }
        tracing::info!(
            "picker_sort DIAG: len={}, target='{}', sorted=[{}]",
            len,
            target,
            names.join(", ")
        );
    }

    // Scan for target entry
    let mut target_idx = None;
    for i in 0..len {
        let entry = data.add(i * ENTRY_SIZE);
        if let Some(name) = read_entry_name(entry) {
            if name == target.as_str() {
                target_idx = Some(i);
                break;
            }
        }
    }

    // If target is already at front or not found → done
    let idx = match target_idx {
        Some(0) | None => return,
        Some(i) => i,
    };

    // Rotate entry[idx] to entry[0]:
    // 1. Copy entry[idx] to tmp
    // 2. Shift entry[0..idx] right by one position
    // 3. Copy tmp to entry[0]
    let mut tmp = [0u8; 256]; // ENTRY_SIZE (0x58=88) fits comfortably
    let entry_ptr = data.add(idx * ENTRY_SIZE);
    std::ptr::copy_nonoverlapping(entry_ptr, tmp.as_mut_ptr(), ENTRY_SIZE);
    std::ptr::copy(data, data.add(ENTRY_SIZE), idx * ENTRY_SIZE);
    std::ptr::copy_nonoverlapping(tmp.as_ptr(), data, ENTRY_SIZE);

    tracing::info!(
        "picker_sort: pinned '{}' from index {} to front (out of {})",
        target, idx, len
    );
}

/// Detour for `insertion_sort_shift_left<OpenFolderEntry, ...>`.
///
/// Calls the original sort, then post-processes to pin the target folder at front.
unsafe extern "C" fn sort_detour(
    data: *mut u8,
    len: usize,
    offset: usize,
    is_less: *mut c_void,
) {
    // Call original sort
    let orig: SortFn = std::mem::transmute(ORIG_FN.load(Ordering::Acquire));
    orig(data, len, offset, is_less);

    // Post-process: pin target to front
    pin_target_to_front(data, len);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_entry_name_null_returns_none() {
        assert_eq!(unsafe { read_entry_name(std::ptr::null()) }, None);
    }

    #[test]
    fn read_entry_name_borrowed_variant() {
        let name = "my-project";
        let name_bytes = name.as_bytes();

        let mut entry = [0u64; 16];
        entry[0] = 0; // tag = Borrowed
        entry[1] = name_bytes.as_ptr() as u64;
        entry[2] = name_bytes.len() as u64;

        let result = unsafe { read_entry_name(entry.as_ptr() as *const u8) };
        assert_eq!(result, Some("my-project"));
    }

    #[test]
    fn read_entry_name_arc_variant() {
        let name = "arc-project";
        let name_bytes = name.as_bytes();

        let mut arc_buf = vec![0u8; 16 + name_bytes.len()];
        arc_buf[0..8].copy_from_slice(&1u64.to_ne_bytes());
        arc_buf[8..16].copy_from_slice(&1u64.to_ne_bytes());
        arc_buf[16..].copy_from_slice(name_bytes);

        let mut entry = [0u64; 16];
        entry[0] = 1; // tag = Arc
        entry[1] = arc_buf.as_ptr() as u64;
        entry[2] = name_bytes.len() as u64;

        let result = unsafe { read_entry_name(entry.as_ptr() as *const u8) };
        assert_eq!(result, Some("arc-project"));
    }

    #[test]
    fn read_entry_name_bad_tag_returns_none() {
        let mut entry = [0u64; 16];
        entry[0] = 99;
        entry[1] = 0x1000;
        entry[2] = 5;
        assert_eq!(unsafe { read_entry_name(entry.as_ptr() as *const u8) }, None);
    }

    #[test]
    fn read_entry_name_zero_len_returns_none() {
        let mut entry = [0u64; 16];
        entry[0] = 0;
        entry[1] = 0x1000;
        entry[2] = 0;
        assert_eq!(unsafe { read_entry_name(entry.as_ptr() as *const u8) }, None);
    }

    #[test]
    fn set_target_name_stores_correctly() {
        set_target_name("test-project".to_string());
        let ptr = TARGET_NAME.load(Ordering::Acquire);
        assert!(!ptr.is_null());
        let name = unsafe { &*ptr };
        assert_eq!(name.as_str(), "test-project");
    }
}
