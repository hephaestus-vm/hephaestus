//! Rust side of the Swift FFI bridge.
//!
//! The Swift symbols referenced here are implemented in
//! `swift/HephaestusBridge/Sources/HephaestusBridge/Bridge.swift` and linked
//! in as a static archive by this crate's `build.rs`.
//!
//! Struct layouts (`HbVmConfig`, `HbStatus`) are shared with Swift via a
//! cbindgen-generated C header that SwiftPM imports through the
//! `CHephaestusBridge` module map target. Both sides must therefore see
//! identical memory layouts for these types.

use std::ffi::{CStr, CString, NulError};
use std::os::raw::c_char;
use std::path::Path;

// =============================================================================
// C-ABI types shared with Swift (emitted to hephaestus_bridge.h via cbindgen).
// =============================================================================

/// Input configuration for `hb_vm_new`.
///
/// All pointers must be NUL-terminated UTF-8 strings valid for the duration
/// of the `hb_vm_new` call. Swift copies the bytes it needs.
#[repr(C)]
#[derive(Debug)]
pub struct HbVmConfig {
    pub id: *const c_char,
    pub kernel_path: *const c_char,
    pub rootfs_path: *const c_char,
}

/// Return status for fallible FFI calls.
#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
pub enum HbStatus {
    Ok = 0,
    InvalidArgument = 1,
    SwiftError = 2,
}

/// Opaque handle to a Swift-owned VM. Obtain via `hb_vm_new`; free via
/// `hb_vm_free`. The Rust side never dereferences the pointee.
#[repr(C)]
#[derive(Debug)]
pub struct HbVm {
    _private: [u8; 0],
}

// =============================================================================
// Rust-side declarations of Swift-implemented symbols.
// These do NOT appear in the generated header — they're only for Rust to call.
// =============================================================================

unsafe extern "C" {
    fn hb_ping() -> *const c_char;
    fn hb_vm_new(
        config: *const HbVmConfig,
        out_vm: *mut *mut HbVm,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vm_free(vm: *mut HbVm);
    fn hb_string_free(s: *mut c_char);
}

// =============================================================================
// Safe Rust wrappers.
// =============================================================================

/// Round-trip the Swift bridge with a ping/pong handshake.
///
/// # Panics
/// If the bridge returns a null or non-UTF-8 pointer (indicates a broken build).
pub fn ping() -> &'static str {
    // SAFETY: hb_ping returns a pointer to a Swift-side static string with 'static lifetime.
    let ptr = unsafe { hb_ping() };
    assert!(!ptr.is_null(), "hb_ping returned null");
    // SAFETY: Swift guarantees NUL termination for the returned static string.
    unsafe { CStr::from_ptr(ptr) }.to_str().expect("bridge returned invalid UTF-8")
}

/// An owned handle to a Swift-side `LinuxContainer`.
///
/// Drop releases the underlying Swift object; no explicit close method is
/// exposed because there is no meaningful "close without drop" path for M1.
#[derive(Debug)]
pub struct Vm {
    handle: *mut HbVm,
}

// SAFETY: the Swift LinuxContainer type is Sendable; the handle is just a
// retained reference we own, so moving it across threads is fine. We do not
// implement Sync because concurrent drops would be UB.
unsafe impl Send for Vm {}

impl Vm {
    pub fn new(id: &str, kernel: &Path, rootfs: &Path) -> Result<Self, VmError> {
        let id_c = CString::new(id)?;
        let kernel_c = CString::new(
            kernel
                .to_str()
                .ok_or_else(|| VmError::InvalidArgument("kernel path is not UTF-8".into()))?,
        )?;
        let rootfs_c = CString::new(
            rootfs
                .to_str()
                .ok_or_else(|| VmError::InvalidArgument("rootfs path is not UTF-8".into()))?,
        )?;

        let config = HbVmConfig {
            id: id_c.as_ptr(),
            kernel_path: kernel_c.as_ptr(),
            rootfs_path: rootfs_c.as_ptr(),
        };

        let mut out_vm: *mut HbVm = std::ptr::null_mut();
        let mut out_err: *mut c_char = std::ptr::null_mut();

        // SAFETY: we pass valid pointers to out-params; the string pointers
        // inside `config` stay valid until after this call returns.
        let status = unsafe { hb_vm_new(&config, &mut out_vm, &mut out_err) };

        match status {
            HbStatus::Ok => {
                debug_assert!(!out_vm.is_null(), "Ok status must produce a non-null handle");
                Ok(Vm { handle: out_vm })
            }
            HbStatus::InvalidArgument | HbStatus::SwiftError => {
                let message = take_swift_string(out_err).unwrap_or_else(|| "(no message)".into());
                Err(match status {
                    HbStatus::InvalidArgument => VmError::InvalidArgument(message),
                    HbStatus::SwiftError => VmError::Swift(message),
                    HbStatus::Ok => unreachable!(),
                })
            }
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: handle was produced by a successful hb_vm_new call and
            // has not been freed before.
            unsafe { hb_vm_free(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

#[derive(Debug)]
pub enum VmError {
    InvalidArgument(String),
    Swift(String),
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            VmError::Swift(m) => write!(f, "swift error: {m}"),
        }
    }
}

impl std::error::Error for VmError {}

impl From<NulError> for VmError {
    fn from(e: NulError) -> Self {
        VmError::InvalidArgument(format!("string contained interior NUL: {e}"))
    }
}

/// Take ownership of a Swift-heap-allocated error string and free it.
///
/// Returns `None` if the pointer is null.
fn take_swift_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Swift allocated this via strdup-equivalent; we copy the bytes
    // into a Rust-owned String before freeing through hb_string_free so the
    // allocator that allocated it is the one that frees it.
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    unsafe { hb_string_free(ptr) };
    Some(s)
}
