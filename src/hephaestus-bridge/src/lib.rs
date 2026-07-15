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
//!
//! # Safety
//!
//! Every `unsafe` block in this crate is calling a Swift `@_cdecl` function
//! whose contract is documented on the matching `extern "C"` declaration
//! in this file. All callers here pass `CString`-derived pointers (valid
//! for reads until the `CString` drops at end of scope) and `&mut`
//! out-pointers (valid for writes for the duration of the call). The
//! Swift side copies any bytes it needs before returning. Documenting
//! each site with a copy of that invariant would be noise, so we allow
//! `clippy::undocumented_unsafe_blocks` crate-wide.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, CString, NulError};
use std::os::fd::FromRawFd;
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::net::UnixStream;
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
    /// When true, attach a NAT-backed network interface so the guest has
    /// outbound IPv4 connectivity. Uses VZ's built-in NAT which only
    /// requires the virtualization entitlement.
    pub enable_networking: bool,
    /// Last octet of the guest's IP in VZ's fixed 192.168.64.0/24 subnet.
    /// Must be in `[2, 254]` when networking is enabled. Ignored otherwise.
    pub network_ip_octet: u8,
    /// When true, wire the guest process to the host's controlling
    /// terminal as a pty. Puts the host TTY in raw mode so keystrokes
    /// (including Ctrl-C, Ctrl-D) are delivered to the guest rather than
    /// interpreted by the host shell. The bridge restores the original
    /// TTY attributes when the handle is freed, even on error paths.
    pub enable_tty: bool,
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

/// Opaque handle to a Swift-owned long-running direct-VZ VM. Obtain via
/// `hb_vz_long_new`; free via `hb_vz_long_free`. Distinct from `HbVm`
/// (containerization path) because the Swift-side backing types differ.
#[repr(C)]
#[derive(Debug)]
pub struct HbVzVm {
    _private: [u8; 0],
}

/// Per-phase wall-clock timings for a VZ restore call. Filled in by the
/// Swift side on every `hb_vz_*_restore*` entry point so callers can
/// break down where warm-start latency actually goes instead of seeing a
/// single opaque total. All fields are nanoseconds. A phase that didn't
/// run (e.g. `resume` when `resume=false` on `hb_vz_long_restore`) is 0.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct HbRestoreTimings {
    /// Config object construction (path canonicalization, attachment
    /// setup, `VZGenericMachineIdentifier` load/create).
    pub config_nanos: u64,
    /// `VZVirtualMachine(configuration:queue:)`.
    pub construct_nanos: u64,
    /// `VZVirtualMachine.restoreMachineStateFrom:` — the actual blob load.
    pub restore_nanos: u64,
    /// `VZVirtualMachine.resume()`.
    pub resume_nanos: u64,
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
    fn hb_vz_boot(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        log_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        run_seconds: u32,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_snapshot_save(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        log_path: *const c_char,
        save_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        settle_seconds: u32,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_snapshot_restore(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        log_path: *const c_char,
        save_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        run_seconds: u32,
        out_restore_nanos: *mut u64,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_sh(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        timeout_seconds: u32,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_exec(
        kernel_path: *const c_char,
        initramfs_path: *const c_char,
        rootfs_path: *const c_char,
        command_utf8: *const c_char,
        log_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        timeout_seconds: u32,
        forward_stdin: u8,
        out_exit_code: *mut i32,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_exec_snapshot_save(
        kernel_path: *const c_char,
        initramfs_path: *const c_char,
        rootfs_path: *const c_char,
        save_path: *const c_char,
        log_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_exec_snapshot_restore(
        kernel_path: *const c_char,
        initramfs_path: *const c_char,
        rootfs_path: *const c_char,
        save_path: *const c_char,
        command_utf8: *const c_char,
        log_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        out_exit_code: *mut i32,
        out_restore_nanos: *mut u64,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_new(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        initrd_path: *const c_char,
        log_path: *const c_char,
        boot_args: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        read_only: bool,
        network_mode: u32,
        mac: *const c_char,
        extra_drive_count: usize,
        extra_drive_paths: *const *const c_char,
        extra_drive_readonly: *const bool,
        out_vm: *mut *mut HbVzVm,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_start(vm: *mut HbVzVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vz_long_pause(vm: *mut HbVzVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vz_long_request_stop(vm: *mut HbVzVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vz_long_set_balloon_target(
        vm: *mut HbVzVm,
        target_bytes: u64,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_resume(vm: *mut HbVzVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vz_long_serve_mmds(
        vm: *mut HbVzVm,
        port: u32,
        json: *const u8,
        json_len: usize,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_connect(
        vm: *mut HbVzVm,
        port: u32,
        out_fd: *mut c_int,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_vmnet_read(
        vm: *mut HbVzVm,
        buffer: *mut u8,
        capacity: usize,
        out_len: *mut usize,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_vmnet_write(
        vm: *mut HbVzVm,
        buffer: *const u8,
        len: usize,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_stop(vm: *mut HbVzVm, out_err: *mut *mut c_char) -> HbStatus;
    fn hb_vz_long_free(vm: *mut HbVzVm);
    fn hb_vz_pool_restore_long(
        kernel_path: *const c_char,
        initramfs_path: *const c_char,
        rootfs_path: *const c_char,
        save_path: *const c_char,
        log_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        out_vm: *mut *mut HbVzVm,
        out_timings: *mut HbRestoreTimings,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_stock_pool_restore_long(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        save_path: *const c_char,
        log_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        out_vm: *mut *mut HbVzVm,
        out_timings: *mut HbRestoreTimings,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_save(
        vm: *mut HbVzVm,
        save_path: *const c_char,
        out_err: *mut *mut c_char,
    ) -> HbStatus;
    fn hb_vz_long_restore(
        kernel_path: *const c_char,
        rootfs_path: *const c_char,
        initrd_path: *const c_char,
        log_path: *const c_char,
        boot_args: *const c_char,
        save_path: *const c_char,
        cpu_count: u32,
        memory_mib: u64,
        read_only: bool,
        network_mode: u32,
        mac: *const c_char,
        extra_drive_count: usize,
        extra_drive_paths: *const *const c_char,
        extra_drive_readonly: *const bool,
        resume: bool,
        out_vm: *mut *mut HbVzVm,
        out_timings: *mut HbRestoreTimings,
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
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .expect("bridge returned invalid UTF-8")
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
    pub networking: bool,
    /// `None` → derive from `id` via [`allocate_ip_octet`]. `Some(n)` pins
    /// the guest's last octet (useful when a caller wants a known address).
    /// Must be in `[2, 254]`.
    pub ip_octet: Option<u8>,
    pub tty: bool,
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

    pub fn networking(mut self, enabled: bool) -> Self {
        self.networking = enabled;
        self
    }

    /// Override the automatically-allocated IP octet. Accepts `[2, 254]`.
    pub fn ip_octet(mut self, octet: u8) -> Self {
        self.ip_octet = Some(octet);
        self
    }

    pub fn tty(mut self, enabled: bool) -> Self {
        self.tty = enabled;
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
        let argv_cstrings: Vec<CString> = spec
            .argv
            .iter()
            .map(|s| CString::new(s.as_str()))
            .collect::<Result<_, _>>()?;
        let mut argv_ptrs: Vec<*const c_char> = argv_cstrings.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let argv_raw: *const *const c_char = if spec.argv.is_empty() {
            std::ptr::null()
        } else {
            argv_ptrs.as_ptr()
        };

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
            enable_networking: spec.networking,
            network_ip_octet: spec.ip_octet.unwrap_or_else(|| allocate_ip_octet(&spec.id)),
            enable_tty: spec.tty,
        };

        let mut out_vm: *mut HbVm = std::ptr::null_mut();
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vm_new(&config, &mut out_vm, &mut out_err) };
        status.into_result(out_err)?;
        debug_assert!(!out_vm.is_null());
        Ok(Vm {
            handle: out_vm,
            _stdio: stdio_state,
        })
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

/// Network attachment used by a long-running direct-VZ VM.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum VzNetworkMode {
    /// Do not attach a network device.
    #[default]
    None,
    /// Virtualization.framework's built-in NAT attachment.
    Nat,
    /// A customizable shared-mode network backed by the vmnet framework.
    Vmnet,
}

impl VzNetworkMode {
    fn code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Nat => 1,
            Self::Vmnet => 2,
        }
    }
}

/// Builder spec for a long-running direct-VZ VM.
///
/// Unlike [`Spec`]/[`Vm`] (containerization-backed, vminitd-orchestrated),
/// this drives a bare `VZVirtualMachine` the way `vz_boot` and friends do
/// but exposes start/stop as independent calls so an HTTP client can own
/// the VM's lifetime. Used by `hephaestus-firecracker`'s `InstanceStart`.
#[derive(Debug, Default)]
pub struct VzSpec {
    pub kernel_path: std::path::PathBuf,
    pub rootfs_path: std::path::PathBuf,
    /// Optional initrd/initramfs. `None` boots directly into the rootfs
    /// init (typical Firecracker setup with an ext4 rootfs).
    pub initrd_path: Option<std::path::PathBuf>,
    /// File the guest's serial console is written to for boot diagnostics.
    pub log_path: std::path::PathBuf,
    /// Kernel command line. Callers should supply Firecracker's equivalent
    /// of `DEFAULT_KERNEL_CMDLINE` or whatever the client passed via
    /// `boot_source.boot_args`.
    pub boot_args: String,
    /// `0` → framework default (2).
    pub cpus: u32,
    /// `0` → framework default (512).
    pub memory_mib: u64,
    /// Attach the rootfs block device read-only. Mirrors Firecracker's
    /// `drive.is_read_only`; when a client marks a shared/golden rootfs
    /// read-only, the guest must not be able to mutate it.
    pub read_only: bool,
    /// Network attachment for the guest NIC. `None` boots without a network
    /// device, `Nat` uses VZ's built-in NAT, and `Vmnet` creates a shared-mode
    /// vmnet network.
    pub network_mode: VzNetworkMode,
    /// Optional guest MAC address (from Firecracker's `guest_mac`). `None`
    /// lets VZ assign a random locally-administered address.
    pub mac: Option<String>,
    /// Secondary (non-root) drives, attached after the rootfs in order as
    /// `/dev/vdb`, `/dev/vdc`, … Each is `(path, read_only)`.
    pub extra_drives: Vec<(std::path::PathBuf, bool)>,
}

impl VzSpec {
    pub fn new(kernel: &Path, rootfs: &Path, log: &Path, boot_args: impl Into<String>) -> Self {
        Self {
            kernel_path: kernel.into(),
            rootfs_path: rootfs.into(),
            log_path: log.into(),
            boot_args: boot_args.into(),
            ..Self::default()
        }
    }

    pub fn initrd(mut self, path: &Path) -> Self {
        self.initrd_path = Some(path.into());
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn networking(mut self, networking: bool) -> Self {
        self.network_mode = if networking {
            VzNetworkMode::Nat
        } else {
            VzNetworkMode::None
        };
        self
    }

    pub fn network_mode(mut self, mode: VzNetworkMode) -> Self {
        self.network_mode = mode;
        self
    }

    pub fn mac(mut self, mac: Option<String>) -> Self {
        self.mac = mac;
        self
    }

    pub fn extra_drives(mut self, drives: Vec<(std::path::PathBuf, bool)>) -> Self {
        self.extra_drives = drives;
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

    pub fn build(self) -> Result<VzVm, VmError> {
        VzVm::new(self)
    }
}

/// Owned handle to a Swift-side long-running direct-VZ VM.
///
/// Drop best-effort stops the VM and releases the handle, matching
/// [`Vm`]'s shape. `Send` is safe for the same reason as `Vm`: the
/// Swift-side `VzVmHandle` serializes all `VZVirtualMachine` access on a
/// per-handle dispatch queue; moving the Rust handle pointer across
/// threads never races the VM object.
#[derive(Debug)]
pub struct VzVm {
    handle: *mut HbVzVm,
}

// SAFETY: see VzVm doc comment.
unsafe impl Send for VzVm {}
// SAFETY: Swift serializes all handle access through the per-VM dispatch queue.
unsafe impl Sync for VzVm {}

impl VzVm {
    fn new(spec: VzSpec) -> Result<Self, VmError> {
        let kernel_c = CString::new(path_to_str(&spec.kernel_path, "kernel")?)?;
        let rootfs_c = CString::new(path_to_str(&spec.rootfs_path, "rootfs")?)?;
        let log_c = CString::new(path_to_str(&spec.log_path, "log")?)?;
        let boot_args_c = CString::new(spec.boot_args)?;
        let initrd_c = opt_path_cstring(spec.initrd_path.as_deref(), "initrd")?;
        let initrd_ptr = initrd_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let mac_c = spec.mac.as_deref().map(CString::new).transpose()?;
        let mac_ptr = mac_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let drives = DriveArrays::new(&spec.extra_drives)?;

        let mut out_vm: *mut HbVzVm = std::ptr::null_mut();
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe {
            hb_vz_long_new(
                kernel_c.as_ptr(),
                rootfs_c.as_ptr(),
                initrd_ptr,
                log_c.as_ptr(),
                boot_args_c.as_ptr(),
                spec.cpus,
                spec.memory_mib,
                spec.read_only,
                spec.network_mode.code(),
                mac_ptr,
                drives.count(),
                drives.paths_ptr(),
                drives.readonly_ptr(),
                &mut out_vm,
                &mut out_err,
            )
        };
        status.into_result(out_err)?;
        debug_assert!(!out_vm.is_null());
        Ok(VzVm { handle: out_vm })
    }

    /// Boot the VM. Returns once the kernel has started; the guest then
    /// runs independently until `stop` or `drop`.
    pub fn start(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_start(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Pause a running VM. VZ freezes the vCPUs; memory stays resident.
    pub fn pause(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_pause(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Request a graceful guest shutdown (VZ `requestStop()`). The guest
    /// stops asynchronously; errors if the VM can't accept the request.
    pub fn request_stop(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_request_stop(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Set the memory balloon's target VM memory size in bytes. A target
    /// below the configured memory inflates the balloon and reclaims the
    /// difference from the guest.
    pub fn set_balloon_target(&self, target_bytes: u64) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status =
            unsafe { hb_vz_long_set_balloon_target(self.handle, target_bytes, &mut out_err) };
        status.into_result(out_err)
    }

    /// Resume a paused VM.
    pub fn resume(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_resume(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Serve a static JSON MMDS response on a guest-initiated vsock port.
    pub fn serve_mmds_vsock(&self, port: u32, json: &[u8]) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe {
            hb_vz_long_serve_mmds(self.handle, port, json.as_ptr(), json.len(), &mut out_err)
        };
        status.into_result(out_err)
    }

    /// Connect to a guest vsock port and return a UnixStream for the bridged
    /// host-side file descriptor. Borrows `&self`, so the VZ handle is
    /// guaranteed to outlive the connect — callers that need to connect from
    /// another thread should share an `Arc<VzVm>` rather than a raw address.
    pub fn connect_vsock(&self, port: u32) -> Result<UnixStream, VmError> {
        let mut out_fd: c_int = -1;
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_connect(self.handle, port, &mut out_fd, &mut out_err) };
        status.into_result(out_err)?;
        if out_fd < 0 {
            return Err(VmError::Swift(
                "vz_long_connect reported success but returned no file descriptor".into(),
            ));
        }
        // SAFETY: hb_vz_long_connect returns a freshly dup(2)'d descriptor whose
        // ownership is transferred to Rust.
        Ok(unsafe { UnixStream::from_raw_fd(out_fd) })
    }

    /// Poll one raw Ethernet frame from the process-owned vmnet interface.
    /// Returns zero when no frame is currently available.
    pub fn vmnet_read(&self, buffer: &mut [u8]) -> Result<usize, VmError> {
        let mut len = 0usize;
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe {
            hb_vz_long_vmnet_read(
                self.handle,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut len,
                &mut out_err,
            )
        };
        status.into_result(out_err)?;
        Ok(len)
    }

    /// Inject one raw Ethernet frame through the process-owned vmnet interface.
    pub fn vmnet_write(&self, buffer: &[u8]) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe {
            hb_vz_long_vmnet_write(self.handle, buffer.as_ptr(), buffer.len(), &mut out_err)
        };
        status.into_result(out_err)
    }

    /// Request graceful stop, then force-stop. Idempotent.
    pub fn stop(&self) -> Result<(), VmError> {
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_stop(self.handle, &mut out_err) };
        status.into_result(out_err)
    }

    /// Save the VM's full state to `save_path`. The VM must already be
    /// paused (VZ requires it); call [`Self::pause`] first. Stays paused
    /// after save — caller decides whether to resume or stop.
    ///
    /// Also writes the platform machine identifier alongside the save
    /// (`<save>.machineid`) so [`vz_long_restore`] in a fresh process
    /// can reuse it.
    pub fn save_state(&self, save_path: &Path) -> Result<(), VmError> {
        let save_c = CString::new(path_to_str(save_path, "save")?)?;
        let mut out_err: *mut c_char = std::ptr::null_mut();
        let status = unsafe { hb_vz_long_save(self.handle, save_c.as_ptr(), &mut out_err) };
        status.into_result(out_err)
    }
}

impl Drop for VzVm {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // hb_vz_long_free calls blockingStop internally, so no need to
            // call stop() first from here.
            unsafe { hb_vz_long_free(self.handle) };
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
        Self {
            sink: Box::into_raw(boxed),
        }
    }

    fn userdata(&self) -> *mut c_void {
        self.sink.cast::<c_void>()
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
    unsafe {
        trampoline(userdata, data, len, /* stderr = */ false)
    };
}

unsafe extern "C" fn trampoline_stderr(userdata: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: same as trampoline_stdout.
    unsafe {
        trampoline(userdata, data, len, /* stderr = */ true)
    };
}

unsafe fn trampoline(userdata: *mut c_void, data: *const u8, len: usize, stderr: bool) {
    if userdata.is_null() || (data.is_null() && len > 0) {
        return;
    }
    // SAFETY: userdata was set from StdioState::userdata and outlives the VM.
    let sink: &dyn StdioSink = unsafe { &**userdata.cast::<Box<dyn StdioSink>>() };
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
                debug_assert!(
                    err_ptr.is_null(),
                    "Ok status must not produce an error string"
                );
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
    p.to_str()
        .ok_or_else(|| VmError::InvalidArgument(format!("{label} path is not UTF-8")))
}

/// `CString` for an optional path, propagating a non-UTF-8 or interior-NUL
/// error instead of silently substituting an empty path. Used for optional
/// inputs (`log`, `initrd`) where the previous `.unwrap_or("")` turned a bad
/// path into `""` — disabling logging or yielding a confusing downstream VZ
/// error rather than a clear "not UTF-8" at the boundary.
fn opt_path_cstring(p: Option<&Path>, label: &str) -> Result<Option<CString>, VmError> {
    p.map(|p| Ok::<_, VmError>(CString::new(path_to_str(p, label)?)?))
        .transpose()
}

/// Owns the C-side storage for a secondary-drive list passed across the FFI as
/// parallel `(paths, readonly)` arrays. Keep it alive for the duration of the
/// `hb_vz_long_*` call — the returned pointers borrow its buffers.
struct DriveArrays {
    // Kept alive so `ptrs` stays valid; the heap buffers outlive Vec moves.
    _paths: Vec<CString>,
    ptrs: Vec<*const c_char>,
    readonly: Vec<bool>,
}

impl DriveArrays {
    fn new(drives: &[(std::path::PathBuf, bool)]) -> Result<Self, VmError> {
        let mut paths = Vec::with_capacity(drives.len());
        let mut readonly = Vec::with_capacity(drives.len());
        for (path, ro) in drives {
            paths.push(CString::new(path_to_str(path, "drive")?)?);
            readonly.push(*ro);
        }
        let ptrs: Vec<*const c_char> = paths.iter().map(|c| c.as_ptr()).collect();
        Ok(Self {
            _paths: paths,
            ptrs,
            readonly,
        })
    }

    fn count(&self) -> usize {
        self.ptrs.len()
    }

    fn paths_ptr(&self) -> *const *const c_char {
        if self.ptrs.is_empty() {
            std::ptr::null()
        } else {
            self.ptrs.as_ptr()
        }
    }

    fn readonly_ptr(&self) -> *const bool {
        if self.readonly.is_empty() {
            std::ptr::null()
        } else {
            self.readonly.as_ptr()
        }
    }
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

// =============================================================================
// Direct Virtualization.framework path — N0 spike.
// Bypasses apple/containerization entirely. Foundation for snapshots (N1).
// =============================================================================

/// Boot a Linux kernel + rootfs directly via VZVirtualMachine, pipe the
/// guest's serial console to `log`, and stop after `run_seconds`.
///
/// No vminitd, no gRPC, no container orchestration — this is a smoke test
/// for our direct-VZ path. Use `cpu_count = 0` and `memory_mib = 0` for
/// framework defaults (2 CPUs, 512 MiB).
pub fn vz_boot(
    kernel: &Path,
    rootfs: &Path,
    log: &Path,
    cpu_count: u32,
    memory_mib: u64,
    run_seconds: u32,
) -> Result<(), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let log_c = CString::new(path_to_str(log, "log")?)?;
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let status = unsafe {
        hb_vz_boot(
            kernel_c.as_ptr(),
            rootfs_c.as_ptr(),
            log_c.as_ptr(),
            cpu_count,
            memory_mib,
            run_seconds,
            &mut out_err,
        )
    };
    status.into_result(out_err)
}

/// Run a single command inside `rootfs` via our guest agent and return
/// its exit code. Boots a minimal VM whose initramfs is the cross-compiled
/// `hephaestus-agent` binary, which mounts `rootfs` at `/`, `chroot`s,
/// listens on vsock port 1234, executes the command we send, returns the
/// exit code, and halts.
///
/// The command is delivered *after* VM start via vsock (not via kernel
/// cmdline) so the same booted VM can later be snapshotted and restored
/// with a different command — the command isn't baked into the save.
///
/// When `forward_stdin` is true, the host pumps its stdin into the guest
/// command's stdin after sending the command frame. The command string
/// must be prefixed with `__hephaestus_stdin__` so the agent knows to
/// pipe the vsock bytes into the child's stdin instead of treating the
/// vsock as a one-shot command/response channel.
#[allow(clippy::too_many_arguments)] // mirrors the FFI shape; refactor doesn't help
pub fn vz_exec(
    kernel: &Path,
    initramfs: &Path,
    rootfs: &Path,
    command: &str,
    log: Option<&Path>,
    cpu_count: u32,
    memory_mib: u64,
    timeout_seconds: u32,
    forward_stdin: bool,
) -> Result<i32, VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let initramfs_c = CString::new(path_to_str(initramfs, "initramfs")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let cmd_c = CString::new(command)?;
    let log_c = opt_path_cstring(log, "log")?;
    let log_ptr = log_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let mut exit_code: i32 = -1;
    let status = unsafe {
        hb_vz_exec(
            kernel_c.as_ptr(),
            initramfs_c.as_ptr(),
            rootfs_c.as_ptr(),
            cmd_c.as_ptr(),
            log_ptr,
            cpu_count,
            memory_mib,
            timeout_seconds,
            if forward_stdin { 1 } else { 0 },
            &mut exit_code,
            &mut out_err,
        )
    };
    status.into_result(out_err)?;
    Ok(exit_code)
}

/// Pre-warm a VM with our agent listening on vsock and save its state.
/// The saved VM is "ready to accept a command" — pair with
/// [`vz_exec_snapshot_restore`] to dispatch different commands into
/// identical restored VMs.
pub fn vz_exec_snapshot_save(
    kernel: &Path,
    initramfs: &Path,
    rootfs: &Path,
    save: &Path,
    log: Option<&Path>,
    cpu_count: u32,
    memory_mib: u64,
) -> Result<(), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let initramfs_c = CString::new(path_to_str(initramfs, "initramfs")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let log_c = opt_path_cstring(log, "log")?;
    let log_ptr = log_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let status = unsafe {
        hb_vz_exec_snapshot_save(
            kernel_c.as_ptr(),
            initramfs_c.as_ptr(),
            rootfs_c.as_ptr(),
            save_c.as_ptr(),
            log_ptr,
            cpu_count,
            memory_mib,
            &mut out_err,
        )
    };
    status.into_result(out_err)
}

/// Restore a pre-warmed VM, send it `command` over vsock, return the
/// guest's exit code. Also returns how long the restore + resume pair
/// took, in nanoseconds — the warm-start latency metric.
#[allow(clippy::too_many_arguments)] // mirrors the FFI shape; refactor doesn't help
pub fn vz_exec_snapshot_restore(
    kernel: &Path,
    initramfs: &Path,
    rootfs: &Path,
    save: &Path,
    command: &str,
    log: Option<&Path>,
    cpu_count: u32,
    memory_mib: u64,
) -> Result<(i32, u64), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let initramfs_c = CString::new(path_to_str(initramfs, "initramfs")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let cmd_c = CString::new(command)?;
    let log_c = opt_path_cstring(log, "log")?;
    let log_ptr = log_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let mut exit_code: i32 = -1;
    let mut restore_nanos: u64 = 0;
    let status = unsafe {
        hb_vz_exec_snapshot_restore(
            kernel_c.as_ptr(),
            initramfs_c.as_ptr(),
            rootfs_c.as_ptr(),
            save_c.as_ptr(),
            cmd_c.as_ptr(),
            log_ptr,
            cpu_count,
            memory_mib,
            &mut exit_code,
            &mut restore_nanos,
            &mut out_err,
        )
    };
    status.into_result(out_err)?;
    Ok((exit_code, restore_nanos))
}

/// Restore a pool snapshot into a long-running [`VzVm`] handle. Caller
/// owns the resulting VM (start has already happened — VZ resumed from
/// the saved state during the restore call) and is responsible for
/// dropping it to release VZ resources.
///
/// Returns the (handle, restore-wall-time-nanos) pair so the caller can
/// log warm-start latency.
pub fn vz_pool_restore_long(
    kernel: &Path,
    initramfs: &Path,
    rootfs: &Path,
    save: &Path,
    log: Option<&Path>,
    cpu_count: u32,
    memory_mib: u64,
) -> Result<(VzVm, HbRestoreTimings), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let initramfs_c = CString::new(path_to_str(initramfs, "initramfs")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let log_c = opt_path_cstring(log, "log")?;
    let log_ptr = log_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    let mut out_vm: *mut HbVzVm = std::ptr::null_mut();
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let mut timings = HbRestoreTimings::default();
    let status = unsafe {
        hb_vz_pool_restore_long(
            kernel_c.as_ptr(),
            initramfs_c.as_ptr(),
            rootfs_c.as_ptr(),
            save_c.as_ptr(),
            log_ptr,
            cpu_count,
            memory_mib,
            &mut out_vm,
            &mut timings,
            &mut out_err,
        )
    };
    status.into_result(out_err)?;
    debug_assert!(!out_vm.is_null());
    Ok((VzVm { handle: out_vm }, timings))
}

/// Restore a snapshot taken via [`VzVm::save_state`] into a fresh
/// long-running [`VzVm`] handle. Caller supplies the same kernel,
/// rootfs, cmdline, vcpu, and memory the original VM was created with
/// (i.e. the values that came in via `PUT /machine-config` +
/// `PUT /boot-source` + `PUT /drives`).
///
/// `resume` controls whether the restored VM resumes immediately
/// (`true`, matching `LoadSnapshotConfig.resume_vm: true`) or stays
/// paused (`false`).
#[allow(clippy::too_many_arguments)]
pub fn vz_long_restore(
    kernel: &Path,
    rootfs: &Path,
    initrd: Option<&Path>,
    log: &Path,
    boot_args: &str,
    save: &Path,
    cpu_count: u32,
    memory_mib: u64,
    read_only: bool,
    network_mode: VzNetworkMode,
    mac: Option<&str>,
    extra_drives: &[(std::path::PathBuf, bool)],
    resume: bool,
) -> Result<(VzVm, HbRestoreTimings), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let log_c = CString::new(path_to_str(log, "log")?)?;
    let boot_args_c = CString::new(boot_args)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let initrd_c = opt_path_cstring(initrd, "initrd")?;
    let initrd_ptr = initrd_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let mac_c = mac.map(CString::new).transpose()?;
    let mac_ptr = mac_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let drives = DriveArrays::new(extra_drives)?;

    let mut out_vm: *mut HbVzVm = std::ptr::null_mut();
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let mut timings = HbRestoreTimings::default();
    let status = unsafe {
        hb_vz_long_restore(
            kernel_c.as_ptr(),
            rootfs_c.as_ptr(),
            initrd_ptr,
            log_c.as_ptr(),
            boot_args_c.as_ptr(),
            save_c.as_ptr(),
            cpu_count,
            memory_mib,
            read_only,
            network_mode.code(),
            mac_ptr,
            drives.count(),
            drives.paths_ptr(),
            drives.readonly_ptr(),
            resume,
            &mut out_vm,
            &mut timings,
            &mut out_err,
        )
    };
    status.into_result(out_err)?;
    debug_assert!(!out_vm.is_null());
    Ok((VzVm { handle: out_vm }, timings))
}

/// Stock-init counterpart of [`vz_pool_restore_long`]. Restores a snapshot
/// produced by [`vz_snapshot_save`] (no agent, no vsock, no initramfs)
/// into a long-running [`VzVm`] handle. Used by `PoolFlavor::StockInit`
/// pools so the HTTP backend can hand back a VM that's behaviorally
/// indistinguishable from a cold-boot one.
pub fn vz_stock_pool_restore_long(
    kernel: &Path,
    rootfs: &Path,
    save: &Path,
    log: Option<&Path>,
    cpu_count: u32,
    memory_mib: u64,
) -> Result<(VzVm, HbRestoreTimings), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let log_c = opt_path_cstring(log, "log")?;
    let log_ptr = log_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    let mut out_vm: *mut HbVzVm = std::ptr::null_mut();
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let mut timings = HbRestoreTimings::default();
    let status = unsafe {
        hb_vz_stock_pool_restore_long(
            kernel_c.as_ptr(),
            rootfs_c.as_ptr(),
            save_c.as_ptr(),
            log_ptr,
            cpu_count,
            memory_mib,
            &mut out_vm,
            &mut timings,
            &mut out_err,
        )
    };
    status.into_result(out_err)?;
    debug_assert!(!out_vm.is_null());
    Ok((VzVm { handle: out_vm }, timings))
}

/// Pause a booted VM and save its full state to disk. Builds a fresh
/// `VZVirtualMachineConfiguration` from the provided artifacts, starts the
/// VM, waits `settle_seconds` for the guest to reach a quiescent state,
/// then pauses and dumps state to `save`.
pub fn vz_snapshot_save(
    kernel: &Path,
    rootfs: &Path,
    log: &Path,
    save: &Path,
    cpu_count: u32,
    memory_mib: u64,
    settle_seconds: u32,
) -> Result<(), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let log_c = CString::new(path_to_str(log, "log")?)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let status = unsafe {
        hb_vz_snapshot_save(
            kernel_c.as_ptr(),
            rootfs_c.as_ptr(),
            log_c.as_ptr(),
            save_c.as_ptr(),
            cpu_count,
            memory_mib,
            settle_seconds,
            &mut out_err,
        )
    };
    status.into_result(out_err)
}

/// Restore a VM from a save file and resume it. Returns how long the
/// `restoreMachineStateFrom:` + `resume()` pair took, in nanoseconds —
/// the marquee "fast boot" number.
///
/// The VM config must structurally match what was saved; pass the same
/// kernel/rootfs/cpus/memory you saved with.
pub fn vz_snapshot_restore(
    kernel: &Path,
    rootfs: &Path,
    log: &Path,
    save: &Path,
    cpu_count: u32,
    memory_mib: u64,
    run_seconds: u32,
) -> Result<u64, VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let log_c = CString::new(path_to_str(log, "log")?)?;
    let save_c = CString::new(path_to_str(save, "save")?)?;
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let mut restore_nanos: u64 = 0;
    let status = unsafe {
        hb_vz_snapshot_restore(
            kernel_c.as_ptr(),
            rootfs_c.as_ptr(),
            log_c.as_ptr(),
            save_c.as_ptr(),
            cpu_count,
            memory_mib,
            run_seconds,
            &mut restore_nanos,
            &mut out_err,
        )
    };
    status.into_result(out_err)?;
    Ok(restore_nanos)
}

/// Interactive shell on the direct-VZ path — no vminitd, no
/// containerization orchestration. Guest serial port is wired to the
/// host's stdin/stdout, host TTY is put in raw mode for the duration.
///
/// `timeout_seconds = 0` means 1 hour; session naturally ends when the
/// guest shell exits and the kernel halts (we use `panic=0`).
pub fn vz_sh(
    kernel: &Path,
    rootfs: &Path,
    cpu_count: u32,
    memory_mib: u64,
    timeout_seconds: u32,
) -> Result<(), VmError> {
    let kernel_c = CString::new(path_to_str(kernel, "kernel")?)?;
    let rootfs_c = CString::new(path_to_str(rootfs, "rootfs")?)?;
    let mut out_err: *mut c_char = std::ptr::null_mut();
    let status = unsafe {
        hb_vz_sh(
            kernel_c.as_ptr(),
            rootfs_c.as_ptr(),
            cpu_count,
            memory_mib,
            timeout_seconds,
            &mut out_err,
        )
    };
    status.into_result(out_err)
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

// =============================================================================
// Network IP allocation for concurrent VMs.
// =============================================================================

/// Deterministically derive a last-octet in `[2, 254]` from an arbitrary VM
/// id. Used as the default static address on VZ's fixed 192.168.64.0/24 NAT
/// subnet so concurrent VMs with distinct ids land on distinct IPs.
///
/// Uses FNV-1a 32-bit because it's tiny, stdlib-free, and has good bucket
/// distribution for short strings. We reserve:
///
/// - `.0` — network address
/// - `.1` — the VZ NAT gateway
/// - `.255` — broadcast
///
/// Collisions between distinct ids are possible (253 buckets) but rare for
/// the handful of concurrent VMs any single host can realistically run.
/// When determinism-across-collisions matters, callers should pass an
/// explicit octet via `Spec::ip_octet`.
pub fn allocate_ip_octet(id: &str) -> u8 {
    const FNV_OFFSET: u32 = 0x811c9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;

    let mut h: u32 = FNV_OFFSET;
    for b in id.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    // 253 addresses in [2, 254]; bias into that range.
    (h % 253) as u8 + 2
}

/// Take ownership of a Swift-heap-allocated error string and free it.
fn take_swift_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Swift allocated this via strdup-equivalent; we copy the bytes
    // before freeing through hb_string_free so the original allocator
    // reclaims the allocation.
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { hb_string_free(ptr) };
    Some(s)
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
        Compression::auto_detect(&tmp).unwrap_err();
    }

    #[test]
    fn spec_builder_sets_fields() {
        let spec = Spec::new("id", Path::new("/k"), Path::new("/i"), Path::new("/r"))
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

    #[test]
    fn ip_octet_is_deterministic() {
        for id in ["dev", "a", "some-ci-runner-42", ""] {
            assert_eq!(allocate_ip_octet(id), allocate_ip_octet(id));
        }
    }

    #[test]
    fn ip_octet_stays_in_range() {
        // Sample a bunch of plausible ids to confirm we never fall outside
        // [2, 254], never return 0/1/255 (reserved).
        let long = "x".repeat(256);
        let ids = [
            "dev",
            "a",
            "",
            "hephaestus-vm",
            "ci-runner-001",
            "ci-runner-002",
            "ci-runner-999",
            long.as_str(),
        ];
        for id in ids {
            let octet = allocate_ip_octet(id);
            assert!(
                (2..=254).contains(&octet),
                "octet {octet} out of range for id {id:?}"
            );
        }
    }

    #[test]
    fn ip_octet_distributes_across_range() {
        // Hash 1000 distinct ids, count unique buckets hit. FNV-1a on short
        // strings should hit far more than half the 253 buckets.
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000 {
            seen.insert(allocate_ip_octet(&format!("vm-{i}")));
        }
        assert!(
            seen.len() > 150,
            "expected wide distribution, got {} distinct buckets out of 253",
            seen.len()
        );
    }

    #[test]
    fn ip_octet_stable_known_values() {
        // Pin a few values so accidental changes to the hash scheme are
        // caught early — breaking this is a compat break for callers that
        // depend on deterministic IPs.
        assert_eq!(allocate_ip_octet("dev"), determine_octet_for("dev"));
        assert_eq!(allocate_ip_octet(""), determine_octet_for(""));
    }

    // Mirror of the implementation so "stable values" tests fail loudly if
    // the algorithm changes (rather than silently keeping the new result).
    fn determine_octet_for(id: &str) -> u8 {
        let mut h: u32 = 0x811c_9dc5;
        for b in id.as_bytes() {
            h ^= u32::from(*b);
            h = h.wrapping_mul(0x0100_0193);
        }
        (h % 253) as u8 + 2
    }
}
