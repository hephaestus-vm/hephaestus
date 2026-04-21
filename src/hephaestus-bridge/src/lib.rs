//! Rust side of the Swift FFI bridge.
//!
//! Swift symbols are defined in `swift/HephaestusBridge/Sources/HephaestusBridge/Bridge.swift`
//! and linked in as a static archive by this crate's `build.rs`.

use std::ffi::CStr;

unsafe extern "C" {
    /// Returns a pointer to a NUL-terminated static C string from Swift.
    fn hb_ping() -> *const std::os::raw::c_char;
}

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
