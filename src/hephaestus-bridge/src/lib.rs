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
use std::os::raw::{c_char, c_void};
use std::path::Path;

// =============================================================================
// C-ABI types shared with Swift (emitted to hephaestus_bridge.h via cbindgen).
// =============================================================================

/// Callback invoked by Swift whenever the guest writes a chunk to
/// stdout or stderr. Called from arbitrary threads; the Rust side is
/// responsible for synchronizing. `userdata` is the opaque pointer passed
/// through `HbVmConfig::stdio_userdata`.
pub type HbWriteCallback =
    Option<unsafe extern "C" fn(userdata: *mut c_void, data: *const u8, len: usize)>;

/// Input configuration for `hb_vm_new`.
///
/// All pointers must be NUL-terminated UTF-8 strings valid for the duration
/// of the `hb_vm_new` call. Swift copies the bytes it needs.
#[repr(C)]
#[derive(Debug)]
pub struct HbVmConfig {
    pub id: *const c_char,
    pub kernel_path: *const c_char,
    /// Path to an ext4 block device containing Apple's `vminitd` init system.
    /// Required for the VM to reach a usable state; without it `hb_vm_create`
    /// will hang or error.
    pub initfs_path: *const c_char,
    /// Path to an ext4 block device that vminitd will mount as the container
    /// rootfs (`/` inside the guest).
    pub rootfs_path: *const c_char,
    /// 0 → use framework default.
    pub cpus: u32,
    /// 0 → use framework default.
    pub memory_mib: u64,
    /// NULL-terminated array of NUL-terminated UTF-8 strings. The first
    /// element is the program; rest are arguments. A NULL sentinel marks
    /// end-of-list. May be NULL to use `/bin/sh` as the default.
    pub argv: *const *const c_char,
    /// Working directory inside the guest; NULL → "/".
    pub cwd: *const c_char,
    pub on_stdout: HbWriteCallback,
    pub on_stderr: HbWriteCallback,
    pub stdio_userdata: *mut c_void,
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
    fn hb_vm_create(vm: *mut HbVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vm_start(vm: *mut HbVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vm_wait(vm: *mut HbVm, out_exit: *mut i32, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vm_stop(vm: *mut HbVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vm_free(vm: *mut HbVm);
    fn hb_string_free(s: *mut c_char);
    fn hb_rootfs_from_tar(
        tar_path: *const c_char,
        out_path: *const c_char,
        block_size_mib: u64,
        compression: u32,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
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

/// Builder-style spec for a VM. Consume with [`Spec::build`].
#[derive(Debug, Default)]
pub struct Spec {
    pub id: String,
    pub kernel_path: std::path::PathBuf,
    pub initfs_path: std::path::PathBuf,
    pub rootfs_path: std::path::PathBuf,
    pub cpus: u32,
    pub memory_mib: u64,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
}

impl Spec {
    pub fn new(id: impl Into<String>, kernel: &Path, initfs: &Path, rootfs: &Path) -> Self {
        Self {
            id: id.into(),
            kernel_path: kernel.into(),
            initfs_path: initfs.into(),
            rootfs_path: rootfs.into(),
            ..Self::default()
        }
    }

    pub fn argv<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv = argv.into_iter().map(Into::into).collect();
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn cpus(mut self, cpus: u32) -> Self {
        self.cpus = cpus;
        self
    }

    pub fn memory_mib(mut self, memory_mib: u64) -> Self {
        self.memory_mib = memory_mib;
        self
    }

    /// Construct a VM handle from this spec.
    ///
    /// `stdio` receives chunks written by the guest; its `write` is invoked
    /// from arbitrary Swift threads, so implement thread-safety at that layer.
    pub fn build<W: StdioSink + 'static>(self, stdio: W) -> Result<Vm, VmError> {
        Vm::new(self, stdio)
    }
}

/// Sink for guest stdout/stderr chunks streamed by the bridge.
///
/// Implementations must be safe to call from Swift background threads.
pub trait StdioSink: Send + Sync {
    fn on_stdout(&self, bytes: &[u8]);
    fn on_stderr(&self, bytes: &[u8]);
}

/// An owned handle to a Swift-side `LinuxContainer`.
///
/// Drop releases the underlying Swift object; no explicit close method is
/// exposed because there is no meaningful "close without drop" path for M1.
#[derive(Debug)]
pub struct Vm {
    handle: *mut HbVm,
    // Owned C-side storage for the stdio trampoline. Freed on drop.
    _stdio: StdioState,
}

// SAFETY: The Swift LinuxContainer type is Sendable; the handle is just a
// retained reference we own, so moving it across threads is fine. We do not
// implement Sync because concurrent drops would be UB.
unsafe impl Send for Vm {}

impl Vm {
    fn new<W: StdioSink + 'static>(spec: Spec, stdio: W) -> Result<Self, VmError> {
        // Keep C strings alive for the duration of the hb_vm_new call.
        let id_c = CString::new(spec.id.clone())?;
        let kernel_c = CString::new(path_to_str(&spec.kernel_path, "kernel")?)?;
        let initfs_c = CString::new(path_to_str(&spec.initfs_path, "initfs")?)?;
        let rootfs_c = CString::new(path_to_str(&spec.rootfs_path, "rootfs")?)?;
        let cwd_c = spec.cwd.as_deref().map(CString::new).transpose()?;
        // argv → CStrings → pointer array with NULL sentinel.
        let argv_cstrings: Vec<CString> =
            spec.argv.iter().map(|s| CString::new(s.as_str())).collect::<Result<_, _>>()?;
        let mut argv_ptrs: Vec<*const c_char> = argv_cstrings.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let argv_raw: *const *const c_char =
            if spec.argv.is_empty() { std::ptr::null() } else { argv_ptrs.as_ptr() };

        // Build the C-side stdio trampoline: Box the sink so it has a stable
        // pointer, then hand the raw pointer to Swift as `stdio_userdata`.
        let stdio_state = StdioState::new(Box::new(stdio));

        let config = HbVmConfig {
            id: id_c.as_ptr(),
            kernel_path: kernel_c.as_ptr(),
            initfs_path: initfs_c.as_ptr(),
            rootfs_path: rootfs_c.as_ptr(),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            argv: argv_raw,
            cwd: cwd_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            on_stdout: Some(trampoline_stdout),
            on_stderr: Some(trampoline_stderr),
            stdio_userdata: stdio_state.userdata(),
        };

        let mut out_vm: *mut HbVm = std::ptr::null_mut();
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vm_new(&config, &mut out_vm, &mut out_err) };
        status.into_result(out_err)?;
        debug_assert!(!out_vm.is_null());
        Ok(Vm { handle: out_vm, _stdio: stdio_state })
    }

    /// Boot the VM and wait for the guest agent handshake.
    pub fn create(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vm_create(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Start the configured init process inside the container.
    pub fn start(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vm_start(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Block until the container's init process exits. Returns its exit code.
    pub fn wait(&self) -> Result<i32, VmError> {
        let mut out_exit: i32 = 0;
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vm_wait(self.handle, &mut out_exit, &mut out_err) };
        status.into_result(out_err)?;
        Ok(out_exit)
    }

    /// Tear the VM down. Idempotent. Must be called even after `wait()`.
    pub fn stop(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vm_stop(self.handle, &mut out_err) };
        status.into_result(out_err)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // Best-effort: the guest VM may already be stopped; swallow errors.
            let mut out_err: *mut c_char = std::ptr::null_mut();
            unsafe { hb_vm_stop(self.handle, &mut out_err) };
            if !out_err.is_null() {
                unsafe { hb_string_free(out_err) };
            }
            // SAFETY: handle was produced by a successful hb_vm_new call.
            unsafe { hb_vm_free(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// Box carrying the Rust-side `StdioSink` across the FFI. The bridge holds
/// its address as `userdata`; on drop we reclaim and free the box.
#[derive(Debug)]
struct StdioState {
    sink: *mut Box<dyn StdioSink>,
}

impl StdioState {
    fn new(sink: Box<dyn StdioSink>) -> Self {
        // Double-box so the fat-pointer DST has a thin stable address.
        let boxed = Box::new(sink);
        Self { sink: Box::into_raw(boxed) }
    }

    fn userdata(&self) -> *mut c_void {
        self.sink as *mut c_void
    }
}

impl Drop for StdioState {
    fn drop(&mut self) {
        if !self.sink.is_null() {
            // SAFETY: sink was produced by Box::into_raw in Self::new and is
            // not freed elsewhere. This happens after Vm::drop has already
            // freed the Swift-side handle, so Swift won't invoke callbacks
            // against this pointer anymore.
            unsafe { drop(Box::from_raw(self.sink)) };
        }
    }
}

unsafe extern "C" fn trampoline_stdout(userdata: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: caller (Swift) guarantees userdata came from StdioState::userdata
    // and data..data+len is readable for the duration of this call.
    unsafe { trampoline(userdata, data, len, /* stderr = */ false) };
}

unsafe extern "C" fn trampoline_stderr(userdata: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: same as trampoline_stdout.
    unsafe { trampoline(userdata, data, len, /* stderr = */ true) };
}

unsafe fn trampoline(userdata: *mut c_void, data: *const u8, len: usize, stderr: bool) {
    if userdata.is_null() || (data.is_null() && len > 0) {
        return;
    }
    // SAFETY: userdata was set from StdioState::userdata and outlives the VM.
    let sink: &Box<dyn StdioSink> = unsafe { &*(userdata as *const Box<dyn StdioSink>) };
    // SAFETY: Swift guarantees data..data+len is readable for the call duration.
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    if stderr {
        sink.on_stderr(slice);
    } else {
        sink.on_stdout(slice);
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

impl HbStatus {
    fn into_result(self, err_ptr: *mut c_char) -> Result<(), VmError> {
        match self {
            HbStatus::Ok => {
                debug_assert!(err_ptr.is_null(), "Ok status must not produce an error string");
                Ok(())
            }
            HbStatus::InvalidArgument | HbStatus::SwiftError => {
                let message = take_swift_string(err_ptr).unwrap_or_else(|| "(no message)".into());
                Err(match self {
                    HbStatus::InvalidArgument => VmError::InvalidArgument(message),
                    HbStatus::SwiftError => VmError::Swift(message),
                    HbStatus::Ok => unreachable!(),
                })
            }
        }
    }
}

fn path_to_str<'a>(p: &'a Path, label: &str) -> Result<&'a str, VmError> {
    p.to_str().ok_or_else(|| VmError::InvalidArgument(format!("{label} path is not UTF-8")))
}

// =============================================================================
// Rootfs helpers — produce an ext4 block device from a tar archive.
// =============================================================================

/// Compression of a tar archive fed to `build_rootfs_from_tar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

impl Compression {
    fn code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Gzip => 1,
            Self::Zstd => 2,
        }
    }

    /// Peek the first bytes of a file to identify its compression format.
    /// Returns `None` if the signature doesn't match a known format.
    pub fn auto_detect(path: &Path) -> std::io::Result<Option<Self>> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut head = [0u8; 4];
        let n = f.read(&mut head)?;
        if n >= 2 && head[0] == 0x1f && head[1] == 0x8b {
            return Ok(Some(Self::Gzip));
        }
        if n >= 4 && head == [0x28, 0xb5, 0x2f, 0xfd] {
            return Ok(Some(Self::Zstd));
        }
        Ok(None) // plausible uncompressed tar; caller may treat as None
    }
}

/// Build an ext4 block device at `out` from the tar archive at `tar`.
///
/// `block_size_mib` is the minimum filesystem size; 0 → framework default.
pub fn build_rootfs_from_tar(
    tar: &Path,
    out: &Path,
    block_size_mib: u64,
    compression: Compression,
) -> Result<(), VmError> {
    let tar_c = CString::new(path_to_str(tar, "tar")?)?;
    let out_c = CString::new(path_to_str(out, "output")?)?;
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let status = unsafe {
        hb_rootfs_from_tar(
            tar_c.as_ptr(),
            out_c.as_ptr(),
            block_size_mib,
            compression.code(),
            &mut out_err,
        )
    };
    status.into_result(out_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detect_gzip_magic() {
        let tmp = std::env::temp_dir().join("hephaestus-test-gzip");
        std::fs::write(&tmp, [0x1f, 0x8b, 0x08, 0x00, 0x00]).unwrap();
        let res = Compression::auto_detect(&tmp).unwrap();
        assert_eq!(res, Some(Compression::Gzip));
    }

    #[test]
    fn auto_detect_zstd_magic() {
        let tmp = std::env::temp_dir().join("hephaestus-test-zstd");
        std::fs::write(&tmp, [0x28, 0xb5, 0x2f, 0xfd, 0x00]).unwrap();
        let res = Compression::auto_detect(&tmp).unwrap();
        assert_eq!(res, Some(Compression::Zstd));
    }

    #[test]
    fn auto_detect_plain_tar_returns_none() {
        let tmp = std::env::temp_dir().join("hephaestus-test-plain");
        // Fake tar header sentinel; real tars start with filename bytes.
        std::fs::write(&tmp, b"./some/path\0\0").unwrap();
        let res = Compression::auto_detect(&tmp).unwrap();
        assert_eq!(res, None);
    }

    #[test]
    fn auto_detect_missing_path_errors() {
        let tmp = std::env::temp_dir().join("hephaestus-test-does-not-exist-xyz");
        let _ = std::fs::remove_file(&tmp);
        assert!(Compression::auto_detect(&tmp).is_err());
    }

    #[test]
    fn spec_builder_sets_fields() {
        let spec = Spec::new(
            "id",
            Path::new("/k"),
            Path::new("/i"),
            Path::new("/r"),
        )
        .argv(["a", "b"])
        .cwd("/workdir")
        .cpus(4)
        .memory_mib(1024);
        assert_eq!(spec.id, "id");
        assert_eq!(spec.argv, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(spec.cwd.as_deref(), Some("/workdir"));
        assert_eq!(spec.cpus, 4);
        assert_eq!(spec.memory_mib, 1024);
    }

    #[test]
    fn compression_codes_are_stable() {
        // Swift side reads these numeric codes; order must not shift.
        assert_eq!(Compression::None.code(), 0);
        assert_eq!(Compression::Gzip.code(), 1);
        assert_eq!(Compression::Zstd.code(), 2);
    }
}

/// Take ownership of a Swift-heap-allocated error string and free it.
fn take_swift_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Swift allocated this via strdup-equivalent; we copy the bytes
    // before freeing through hb_string_free so the original allocator
    // reclaims the allocation.
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    unsafe { hb_string_free(ptr) };
    Some(s)
}
