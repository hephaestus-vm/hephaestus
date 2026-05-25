//! Minimal macOS sandbox hook for Firecracker-style jailer experiments.
//!
//! This is intentionally opt-in and profile-driven: hephaestus cannot know the
//! caller's kernel/rootfs/log/socket paths ahead of time, so heavy users can
//! generate a per-VM Sandbox.kext profile and ask the daemon to enter it before
//! serving the API socket.

use std::ffi::{CStr, CString, c_char, c_int};

#[cfg(target_os = "macos")]
#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Enter a macOS App Sandbox profile expressed in sandbox profile language.
#[cfg(target_os = "macos")]
pub fn apply_profile(profile: &str) -> Result<(), String> {
    let profile = CString::new(profile).map_err(|_| "sandbox profile contains NUL".to_string())?;
    let mut error: *mut c_char = std::ptr::null_mut();
    // SAFETY: `profile` is a valid NUL-terminated C string for the duration of
    // the call, `error` is an out-param owned by libsandbox on failure and freed
    // with `sandbox_free_error` below.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
    if rc == 0 {
        return Ok(());
    }
    let message = if error.is_null() {
        "sandbox_init failed".to_string()
    } else {
        // SAFETY: libsandbox returns a NUL-terminated error string when the
        // pointer is non-null.
        let message = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `error` came from `sandbox_init` and must be released by
        // libsandbox's matching free function.
        unsafe { sandbox_free_error(error) };
        message
    };
    Err(message)
}

/// Non-macOS builds keep the CLI shape but cannot enter a macOS sandbox.
#[cfg(not(target_os = "macos"))]
pub fn apply_profile(_profile: &str) -> Result<(), String> {
    Err("macOS sandbox profiles are only supported on macOS".to_string())
}
