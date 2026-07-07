//! `VmmBackend` implementation that drives `hephaestus-vmm`'s direct-VZ path.
//!
//! Single-VM-per-process matches upstream's contract; `VzBackend` holds
//! the accumulated pre-boot config and, once booted, an owned [`VzVm`]
//! handle. Dropping the backend stops the VM via `VzVm::Drop`.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hephaestus_fc_api::vmm_config::boot_source::{BootSourceConfig, DEFAULT_KERNEL_CMDLINE};
use hephaestus_fc_api::vmm_config::drive::{BlockDeviceConfig, BlockDeviceUpdateConfig};
use hephaestus_fc_api::vmm_config::instance_info::{InstanceInfo, VmState};
use hephaestus_fc_api::vmm_config::logger::LoggerConfig;
use hephaestus_fc_api::vmm_config::machine_config::{
    MAX_SUPPORTED_VCPUS, MachineConfig, MachineConfigUpdate,
};
use hephaestus_fc_api::vmm_config::metrics::MetricsConfig;
use hephaestus_fc_api::vmm_config::mmds::MmdsConfig;
use hephaestus_fc_api::vmm_config::net::NetworkInterfaceConfig;
use hephaestus_fc_api::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotConfig, MemBackendType, SnapshotType,
};
use hephaestus_fc_api::vmm_config::vsock::VsockConfig;
use hephaestus_fc_api::{VmmBackend, VmmBackendError};
use hephaestus_pool::{ClaimedSlot, Pool, PoolMatchSpec};
use hephaestus_vmm::{VzSpec, VzVm, vz_long_restore};
use serde_json::Value;

/// Guest-initiated vsock port for hephaestus' practical MMDS transport.
/// Port 1234 is reserved for hephaestus-agent command injection.
pub const MMDS_VSOCK_PORT: u32 = 16_992;

/// How the currently-running VM was started. Used to gate
/// `PUT /snapshot/create`: only cold-boot VMs are saveable, since their
/// config (kernel, rootfs, cmdline, no vsock) is reproducible by the
/// loader. Pool-restored VMs were built from a different config flavor
/// and would fail VZ's "configuration mismatch" on later restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOrigin {
    ColdBoot,
    Pool,
    SnapshotLoad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Off,
}

impl LogLevel {
    fn parse(value: &str) -> Result<Self, VmmBackendError> {
        match value.to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            "off" => Ok(Self::Off),
            other => Err(VmmBackendError::InvalidConfig(format!(
                "invalid logger level {other:?}"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
            Self::Off => "OFF",
        }
    }

    fn enabled(self, record: Self) -> bool {
        !matches!(self, Self::Off) && record <= self
    }
}

#[derive(Clone, Debug)]
struct LoggerState {
    path: Option<PathBuf>,
    level: LogLevel,
    show_level: bool,
    show_log_origin: bool,
    module: Option<String>,
}

impl Default for LoggerState {
    fn default() -> Self {
        Self {
            path: None,
            level: LogLevel::Info,
            show_level: false,
            show_log_origin: false,
            module: None,
        }
    }
}

#[derive(Clone, Debug)]
struct MetricsState {
    path: Option<PathBuf>,
    started_at: Instant,
    flush_count: u64,
    api_requests: u64,
    api_request_fails: u64,
    get_requests: u64,
    put_requests: u64,
    patch_requests: u64,
    pool_hits: u64,
    pool_misses: u64,
    snapshot_loads: u64,
}

impl Default for MetricsState {
    fn default() -> Self {
        Self {
            path: None,
            started_at: Instant::now(),
            flush_count: 0,
            api_requests: 0,
            api_request_fails: 0,
            get_requests: 0,
            put_requests: 0,
            patch_requests: 0,
            pool_hits: 0,
            pool_misses: 0,
            snapshot_loads: 0,
        }
    }
}

// `Sync` is required because the vsock bridge shares the handle with its
// accept-loop thread via `Arc<dyn VzVmHandle>`; `VzVm` is soundly `Sync`
// (Swift serializes all handle access on the per-VM dispatch queue).
trait VzVmHandle: std::fmt::Debug + Send + Sync {
    fn serve_mmds_vsock(&self, port: u32, json: &[u8]) -> Result<(), VmmBackendError>;
    /// Open a host-side connection to a guest vsock port. Replaces the old
    /// raw-`usize` handle-address API: because the caller holds an
    /// `Arc<dyn VzVmHandle>`, the VZ handle provably outlives the connect.
    fn connect_vsock(&self, port: u32) -> Result<UnixStream, VmmBackendError>;
    fn save_state(&self, path: &std::path::Path) -> Result<(), VmmBackendError>;
    fn pause(&self) -> Result<(), VmmBackendError>;
    fn resume(&self) -> Result<(), VmmBackendError>;
}

impl VzVmHandle for VzVm {
    fn serve_mmds_vsock(&self, port: u32, json: &[u8]) -> Result<(), VmmBackendError> {
        self.serve_mmds_vsock(port, json)
            .map_err(|err| VmmBackendError::Internal(err.to_string()))
    }

    fn connect_vsock(&self, port: u32) -> Result<UnixStream, VmmBackendError> {
        self.connect_vsock(port)
            .map_err(|err| VmmBackendError::Internal(err.to_string()))
    }

    fn save_state(&self, path: &std::path::Path) -> Result<(), VmmBackendError> {
        self.save_state(path)
            .map_err(|err| VmmBackendError::Internal(err.to_string()))
    }

    fn pause(&self) -> Result<(), VmmBackendError> {
        self.pause()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))
    }

    fn resume(&self) -> Result<(), VmmBackendError> {
        self.resume()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))
    }
}

#[derive(Debug)]
pub struct VzBackend {
    id: String,
    state: VmState,
    boot_source: Option<BootSourceConfig>,
    root_drive: Option<PathBuf>,
    /// Whether the root drive was configured `is_read_only: true`. Honored by
    /// attaching the VZ block device read-only so the guest cannot mutate a
    /// shared/golden rootfs the client marked immutable.
    root_drive_read_only: bool,
    iface: Option<NetworkInterfaceConfig>,
    machine_config: MachineConfig,
    mmds: Value,
    mmds_config: MmdsConfig,
    vsock: Option<VsockConfig>,
    logger: LoggerState,
    metrics: MetricsState,
    /// Drop order matters and is encoded by field order below:
    /// `vsock_bridge` drops first (its `Drop` stops the accept loop and
    /// releases its `Arc` clone of the VM), then `vm` (last `Arc` → frees the
    /// Swift handle), then `pool_slot` (its `Drop` deletes the rootfs clone).
    /// Reordering these can delete a rootfs out from under a live VM or leave
    /// the bridge holding a freed handle.
    vsock_bridge: Option<VsockBridge>,
    vm: Option<Arc<dyn VzVmHandle>>,
    pool_slot: Option<ClaimedSlot>,
    pool: Option<Pool>,
    /// Set once start_micro_vm or load_snapshot succeeds. None
    /// pre-boot. Gates create_snapshot.
    origin: Option<RunOrigin>,
}

impl VzBackend {
    pub fn new(id: String) -> Self {
        Self {
            id,
            state: VmState::NotStarted,
            boot_source: None,
            root_drive: None,
            root_drive_read_only: false,
            iface: None,
            machine_config: MachineConfig::default(),
            mmds: Value::Object(Default::default()),
            mmds_config: MmdsConfig::default(),
            vsock: None,
            logger: LoggerState::default(),
            metrics: MetricsState::default(),
            vsock_bridge: None,
            vm: None,
            pool_slot: None,
            pool: None,
            origin: None,
        }
    }

    /// Attach a warm pool the backend will try to claim from at
    /// `InstanceStart`. On a config mismatch or all-slots-busy, the
    /// backend silently falls back to cold boot — the client sees
    /// `start_micro_vm` either way.
    pub fn with_pool(mut self, pool: Pool) -> Self {
        self.pool = Some(pool);
        self
    }

    fn require_preboot(&self) -> Result<(), VmmBackendError> {
        if matches!(self.state, VmState::NotStarted) {
            Ok(())
        } else {
            Err(VmmBackendError::InvalidState(
                "operation not supported post-boot".into(),
            ))
        }
    }

    /// Roll the backend back to pre-boot after a post-start failure (e.g. the
    /// vsock bridge couldn't bind its UDS). Tears down the VM, bridge, and any
    /// claimed pool slot in the correct order and clears `origin`/state, so the
    /// caller sees a clean failure rather than an orphaned `Running` VM it can
    /// neither drive nor restart. Pre-boot config (boot source, drives,
    /// machine config) is untouched, so the client can fix the offending
    /// setting and retry `InstanceStart`.
    fn abort_boot(&mut self) {
        self.vsock_bridge = None;
        self.vm = None;
        self.pool_slot = None;
        self.origin = None;
        self.state = VmState::NotStarted;
    }

    fn serial_log_path(&self) -> PathBuf {
        std::env::var_os("HEPHAESTUS_FC_WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("hephaestus-firecracker-{}.log", self.id))
    }

    pub fn observe_request(&mut self, request_id: u64, method: &str, path: &str, status: u16) {
        self.metrics.api_requests = self.metrics.api_requests.saturating_add(1);
        if status >= 400 {
            self.metrics.api_request_fails = self.metrics.api_request_fails.saturating_add(1);
        }
        match method {
            "GET" => self.metrics.get_requests = self.metrics.get_requests.saturating_add(1),
            "PUT" => self.metrics.put_requests = self.metrics.put_requests.saturating_add(1),
            "PATCH" => self.metrics.patch_requests = self.metrics.patch_requests.saturating_add(1),
            _ => {}
        }
        if self.logger.level.enabled(LogLevel::Debug) {
            self.write_log(
                LogLevel::Debug,
                "api_server::request",
                None,
                &format!("request_id={request_id} method={method} path={path} status={status}"),
            );
        }
        self.flush_metrics();
    }

    pub fn flush_metrics(&mut self) {
        let Some(path) = self.metrics.path.clone() else {
            return;
        };
        self.metrics.flush_count = self.metrics.flush_count.saturating_add(1);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let uptime_us = self.metrics.started_at.elapsed().as_micros();
        let payload = serde_json::json!({
            "utc_timestamp_ms": timestamp_ms,
            "api_server": {
                "process_startup_time_us": uptime_us,
                "sync_response_fails": self.metrics.api_request_fails,
            },
            "get_api_requests": {
                "instance_info_count": self.metrics.get_requests,
                "machine_cfg_count": 0,
                "mmds_count": 0,
                "vmm_version_count": 0,
            },
            "put_api_requests": {
                "actions_count": 0,
                "boot_source_count": 0,
                "drive_count": 0,
                "logger_count": 0,
                "machine_cfg_count": 0,
                "metrics_count": 0,
                "mmds_count": 0,
                "net_count": 0,
                "snapshot_create_count": 0,
                "snapshot_load_count": self.metrics.snapshot_loads,
            },
            "patch_api_requests": {
                "drive_count": 0,
                "machine_cfg_count": 0,
                "mmds_count": 0,
                "net_count": 0,
                "vm_count": 0,
            },
            "logger": {
                "missed_log_count": 0,
                "missed_metrics_count": 0,
                "flush_count": self.metrics.flush_count,
            },
            "vmm": {
                "panic_count": 0,
            },
            "vcpu": {
                "exit_io_in": 0,
                "exit_io_out": 0,
                "failures": 0,
            },
            "seccomp": {
                "num_faults": 0,
            },
            "hephaestus": {
                "api_requests": self.metrics.api_requests,
                "api_request_fails": self.metrics.api_request_fails,
                "pool_hits": self.metrics.pool_hits,
                "pool_misses": self.metrics.pool_misses,
                "snapshot_loads": self.metrics.snapshot_loads,
            },
        });
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{payload}");
        }
    }

    fn log_info(&self, origin: &'static str, message: &str) {
        self.write_log(LogLevel::Info, origin, None, message);
    }

    fn refresh_mmds_vsock_service(&self) -> Result<(), VmmBackendError> {
        let Some(vm) = self.vm.as_ref() else {
            return Ok(());
        };
        let json = serde_json::to_vec(&self.mmds)
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;
        vm.serve_mmds_vsock(MMDS_VSOCK_PORT, &json)
    }

    fn start_vsock_bridge(&mut self) -> Result<(), VmmBackendError> {
        let Some(cfg) = self.vsock.clone() else {
            // Stock-init pool snapshots intentionally have no virtio-vsock device.
            // MMDS-over-vsock is best-effort unless the client explicitly
            // configured PUT /vsock and therefore expects a host UDS bridge.
            if let Err(err) = self.refresh_mmds_vsock_service() {
                eprintln!("hephaestus-firecracker: MMDS vsock service unavailable ({err})");
            }
            return Ok(());
        };
        self.refresh_mmds_vsock_service()?;
        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| VmmBackendError::Internal("vsock bridge without VM".into()))?
            .clone();
        self.vsock_bridge = Some(VsockBridge::start(vm, cfg.uds_path)?);
        Ok(())
    }

    fn write_log(&self, level: LogLevel, origin: &str, line: Option<u32>, message: &str) {
        if !self.logger.level.enabled(level) {
            return;
        }
        if let Some(module) = self.logger.module.as_ref()
            && !origin.starts_with(module)
        {
            return;
        }
        let Some(path) = self.logger.path.as_ref() else {
            return;
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| format!("{}.{:06}", d.as_secs(), d.subsec_micros()))
            .unwrap_or_else(|_| "0.000000".to_string());
        let thread = std::thread::current().name().unwrap_or("-").to_string();
        let level_suffix = if self.logger.show_level {
            format!(":{}", level.as_str())
        } else {
            String::new()
        };
        let origin_suffix = if self.logger.show_log_origin {
            format!(
                ":{}:{}",
                origin,
                line.map_or_else(|| "?".to_string(), |n| n.to_string())
            )
        } else {
            String::new()
        };
        let record = format!(
            "{timestamp} [{}:{thread}{level_suffix}{origin_suffix}] {message}\n",
            self.id
        );
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(record.as_bytes());
        }
    }
}

impl VmmBackend for VzBackend {
    fn instance_info(&self) -> InstanceInfo {
        InstanceInfo {
            id: self.id.clone(),
            state: self.state.clone(),
            vmm_version: env!("CARGO_PKG_VERSION").to_string(),
            app_name: "hephaestus-firecracker".to_string(),
        }
    }

    fn configure_boot_source(&mut self, cfg: BootSourceConfig) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        if !std::path::Path::new(&cfg.kernel_image_path).exists() {
            return Err(VmmBackendError::InvalidConfig(format!(
                "kernel image not found at {}",
                cfg.kernel_image_path
            )));
        }
        self.boot_source = Some(cfg);
        Ok(())
    }

    fn insert_block_device(&mut self, cfg: BlockDeviceConfig) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        if !cfg.is_root_device {
            return Err(VmmBackendError::NotSupported(
                "only root block devices are supported on macOS".into(),
            ));
        }
        let path = cfg.path_on_host.ok_or_else(|| {
            VmmBackendError::InvalidConfig("drive.path_on_host is required".into())
        })?;
        let path = PathBuf::from(path);
        if !path.exists() {
            return Err(VmmBackendError::InvalidConfig(format!(
                "rootfs not found at {}",
                path.display()
            )));
        }
        self.root_drive = Some(path);
        // Firecracker defaults an omitted is_read_only to false (writable).
        self.root_drive_read_only = cfg.is_read_only.unwrap_or(false);
        Ok(())
    }

    fn update_block_device(&mut self, cfg: BlockDeviceUpdateConfig) -> Result<(), VmmBackendError> {
        // Pre-boot-only because VZ doesn't support hot-swapping a
        // block-device attachment the way virtio-blk + io_uring does on
        // Linux. Clients that rely on post-boot patch will need to stop
        // and restart the VM; firectl/Kata both do their drive patch
        // before InstanceStart so this covers the typical path.
        self.require_preboot()?;
        if let Some(path) = cfg.path_on_host {
            let path = PathBuf::from(path);
            if !path.exists() {
                return Err(VmmBackendError::InvalidConfig(format!(
                    "rootfs not found at {}",
                    path.display()
                )));
            }
            self.root_drive = Some(path);
        }
        // rate_limiter: accept-and-ignore, we don't enforce rate limits
        // on macOS VZ's built-in block attachment.
        Ok(())
    }

    fn insert_network_device(
        &mut self,
        cfg: NetworkInterfaceConfig,
    ) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        self.iface = Some(cfg);
        Ok(())
    }

    fn configure_logger(&mut self, cfg: LoggerConfig) -> Result<(), VmmBackendError> {
        // Accept pre- or post-boot, like Firecracker. Logger updates are
        // patch-like: omitted fields retain their prior values.
        if let Some(path) = cfg.log_path {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|err| {
                    VmmBackendError::InvalidConfig(format!(
                        "cannot open log_path {}: {err}",
                        path.display()
                    ))
                })?;
            self.logger.path = Some(path);
        }
        if let Some(level) = cfg.level.as_deref() {
            self.logger.level = LogLevel::parse(level)?;
        }
        if let Some(show_level) = cfg.show_level {
            self.logger.show_level = show_level;
        }
        if let Some(show_log_origin) = cfg.show_log_origin {
            self.logger.show_log_origin = show_log_origin;
        }
        if let Some(module) = cfg.module {
            self.logger.module = if module.is_empty() {
                None
            } else {
                Some(module)
            };
        }
        self.log_info(
            "api_server::request::logger",
            "The logger was configured successfully.",
        );
        Ok(())
    }

    fn configure_metrics(&mut self, cfg: MetricsConfig) -> Result<(), VmmBackendError> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.metrics_path)
            .map_err(|err| {
                VmmBackendError::InvalidConfig(format!(
                    "cannot open metrics_path {}: {err}",
                    cfg.metrics_path.display()
                ))
            })?;
        self.metrics.path = Some(cfg.metrics_path);
        self.flush_metrics();
        Ok(())
    }

    fn get_mmds(&self) -> Value {
        self.mmds.clone()
    }

    fn put_mmds(&mut self, data: Value) -> Result<(), VmmBackendError> {
        self.mmds = data;
        self.refresh_mmds_vsock_service()?;
        Ok(())
    }

    fn patch_mmds(&mut self, data: Value) -> Result<(), VmmBackendError> {
        merge_json(&mut self.mmds, data);
        self.refresh_mmds_vsock_service()?;
        Ok(())
    }

    fn configure_mmds(&mut self, cfg: MmdsConfig) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        if let Some(addr) = cfg.ipv4_address.as_deref() {
            validate_mmds_link_local_addr(addr)?;
        }
        self.mmds_config = cfg;
        Ok(())
    }

    fn configure_vsock(&mut self, cfg: VsockConfig) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        if cfg.guest_cid < 3 {
            return Err(VmmBackendError::InvalidConfig(
                "vsock.guest_cid must be >= 3".into(),
            ));
        }
        if cfg.uds_path.as_os_str().is_empty() {
            return Err(VmmBackendError::InvalidConfig(
                "vsock.uds_path is required".into(),
            ));
        }
        self.vsock = Some(cfg);
        Ok(())
    }

    fn get_machine_config(&self) -> MachineConfig {
        self.machine_config.clone()
    }

    fn put_machine_config(&mut self, cfg: MachineConfig) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        if cfg.vcpu_count == 0 {
            return Err(VmmBackendError::InvalidConfig(
                "vcpu_count must be >= 1".into(),
            ));
        }
        if cfg.vcpu_count > MAX_SUPPORTED_VCPUS {
            return Err(VmmBackendError::InvalidConfig(format!(
                "vcpu_count must be <= {MAX_SUPPORTED_VCPUS}"
            )));
        }
        if cfg.mem_size_mib == 0 {
            return Err(VmmBackendError::InvalidConfig(
                "mem_size_mib must be > 0".into(),
            ));
        }
        if cfg.cpu_template.is_some() {
            return Err(VmmBackendError::NotSupported(
                "cpu_template is not supported on Apple Silicon/VZ".into(),
            ));
        }
        self.machine_config = cfg;
        Ok(())
    }

    fn patch_machine_config(&mut self, update: MachineConfigUpdate) -> Result<(), VmmBackendError> {
        self.require_preboot()?;
        if update.is_empty() {
            return Err(VmmBackendError::InvalidConfig(
                "empty machine-config patch".into(),
            ));
        }
        let mut cfg = self.machine_config.clone();
        if let Some(n) = update.vcpu_count {
            cfg.vcpu_count = n;
        }
        if let Some(m) = update.mem_size_mib {
            cfg.mem_size_mib = m;
        }
        if let Some(smt) = update.smt {
            cfg.smt = smt;
        }
        if let Some(t) = update.track_dirty_pages {
            cfg.track_dirty_pages = t;
        }
        if let Some(h) = update.huge_pages {
            cfg.huge_pages = h;
        }
        if update.cpu_template.is_some() {
            return Err(VmmBackendError::NotSupported(
                "cpu_template is not supported on Apple Silicon/VZ".into(),
            ));
        }
        self.put_machine_config(cfg)
    }

    fn start_micro_vm(&mut self) -> Result<(), VmmBackendError> {
        self.require_preboot()?;

        let boot = self
            .boot_source
            .as_ref()
            .ok_or_else(|| VmmBackendError::InvalidState("boot-source not configured".into()))?;
        let rootfs = self
            .root_drive
            .clone()
            .ok_or_else(|| VmmBackendError::InvalidState("root drive not configured".into()))?;
        let kernel = PathBuf::from(&boot.kernel_image_path);
        let boot_args = boot
            .boot_args
            .clone()
            .unwrap_or_else(|| DEFAULT_KERNEL_CMDLINE.to_string());
        let cpu = u32::from(self.machine_config.vcpu_count);
        let memory = u64::try_from(self.machine_config.mem_size_mib)
            .map_err(|err| VmmBackendError::InvalidConfig(err.to_string()))?;
        let log = self.serial_log_path();

        // Pool fast-path. Match against the requested kernel+rootfs+cpu+
        // memory tuple; on a hit we restore from the snapshot, skip
        // cold-boot kernel init, and the client sees an InstanceStart
        // 204 in ~tens of ms instead of hundreds. On any miss
        // (no pool, config mismatch, all slots busy, restore failure)
        // we silently fall through to cold boot — same client-visible
        // contract.
        if let Some(pool) = self.pool.as_ref() {
            let spec = PoolMatchSpec {
                kernel: kernel.clone(),
                rootfs: rootfs.clone(),
                vcpu_count: cpu,
                memory_mib: memory,
            }
            .canonicalize();
            match pool.try_claim_matching_slot(&spec) {
                Ok(Some(slot)) => match pool.restore_into_vm(&slot, Some(&log)) {
                    Ok((vm, breakdown)) => {
                        let ms = |ns: u64| ns as f64 / 1_000_000.0;
                        eprintln!(
                            "hephaestus-firecracker: pool hit slot={} total={:.1}ms \
                             (clone={:.1} config={:.1} construct={:.1} restore={:.1} resume={:.1})",
                            slot.index,
                            ms(breakdown.total_nanos()),
                            ms(breakdown.clone_nanos),
                            ms(breakdown.vz.config_nanos),
                            ms(breakdown.vz.construct_nanos),
                            ms(breakdown.vz.restore_nanos),
                            ms(breakdown.vz.resume_nanos),
                        );
                        self.vm = Some(Arc::new(vm));
                        self.pool_slot = Some(slot);
                        self.state = VmState::Running;
                        self.origin = Some(RunOrigin::Pool);
                        self.metrics.pool_hits = self.metrics.pool_hits.saturating_add(1);
                        if let Err(err) = self.start_vsock_bridge() {
                            self.abort_boot();
                            return Err(err);
                        }
                        self.flush_metrics();
                        return Ok(());
                    }
                    Err(err) => {
                        eprintln!(
                            "hephaestus-firecracker: pool restore failed ({err}); cold-booting"
                        );
                        // Slot dropped here releases the flock; cold path
                        // takes over below.
                    }
                },
                Ok(None) => {
                    // Either config mismatch or all slots busy — cold-boot.
                    self.metrics.pool_misses = self.metrics.pool_misses.saturating_add(1);
                }
                Err(err) => {
                    eprintln!("hephaestus-firecracker: pool claim error ({err}); cold-booting");
                }
            }
        }

        let mut spec = VzSpec::new(&kernel, &rootfs, &log, boot_args)
            .cpus(cpu)
            .memory_mib(memory)
            .read_only(self.root_drive_read_only);
        if let Some(initrd) = boot.initrd_path.as_ref() {
            spec = spec.initrd(std::path::Path::new(initrd));
        }
        let vm = spec
            .build()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;
        vm.start()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;

        self.vm = Some(Arc::new(vm));
        self.state = VmState::Running;
        self.origin = Some(RunOrigin::ColdBoot);
        if let Err(err) = self.start_vsock_bridge() {
            self.abort_boot();
            return Err(err);
        }
        self.log_info("vmm", "Vmm is running.");
        Ok(())
    }

    fn create_snapshot(&mut self, params: CreateSnapshotParams) -> Result<(), VmmBackendError> {
        if !matches!(self.state, VmState::Paused) {
            return Err(VmmBackendError::InvalidState(
                "snapshot/create requires the VM to be Paused (PATCH /vm first)".into(),
            ));
        }
        if !matches!(params.snapshot_type, SnapshotType::Full) {
            return Err(VmmBackendError::NotSupported(
                "snapshot_type=Diff (VZ has no incremental save)".into(),
            ));
        }
        // Pool-restored VMs were built from a config flavor (vsock,
        // initramfs, agent cmdline) that this process's snapshot/load
        // path can't reproduce — VZ would reject the restore. Caller
        // can stop the pool VM and cold-boot a new one to enable saves.
        match self.origin {
            Some(RunOrigin::ColdBoot | RunOrigin::SnapshotLoad) => {}
            Some(RunOrigin::Pool) => {
                return Err(VmmBackendError::NotSupported(
                    "snapshot/create on a pool-restored VM (cold-boot to snapshot)".into(),
                ));
            }
            None => {
                return Err(VmmBackendError::InvalidState("no VM running".into()));
            }
        }

        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| VmmBackendError::Internal("Paused state without a VM handle".into()))?;
        vm.save_state(&params.snapshot_path)?;

        // A+stub: touch an empty file at mem_file_path so clients that
        // os.Stat(mem_file_path) post-save don't error. The real blob
        // (state + memory together) is at snapshot_path. See the
        // module-level note in fc-api/.../snapshot.rs.
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&params.mem_file_path)
            .map_err(|err| {
                VmmBackendError::InvalidConfig(format!(
                    "cannot touch mem_file_path {}: {err}",
                    params.mem_file_path.display()
                ))
            })?;
        Ok(())
    }

    fn load_snapshot(&mut self, params: LoadSnapshotConfig) -> Result<(), VmmBackendError> {
        self.require_preboot()?;

        if params.enable_diff_snapshots || params.track_dirty_pages {
            return Err(VmmBackendError::NotSupported(
                "diff/dirty-page tracking (VZ has no equivalent)".into(),
            ));
        }
        if let Some(backend) = params.mem_backend.as_ref()
            && !matches!(backend.backend_type, MemBackendType::File)
        {
            return Err(VmmBackendError::NotSupported(
                "mem_backend.backend_type=Uffd (Linux-only)".into(),
            ));
        }

        let boot = self.boot_source.as_ref().ok_or_else(|| {
            VmmBackendError::InvalidState("snapshot/load requires PUT /boot-source first".into())
        })?;
        let rootfs = self.root_drive.clone().ok_or_else(|| {
            VmmBackendError::InvalidState("snapshot/load requires PUT /drives/{id} first".into())
        })?;
        let kernel = PathBuf::from(&boot.kernel_image_path);
        let boot_args = boot
            .boot_args
            .clone()
            .unwrap_or_else(|| DEFAULT_KERNEL_CMDLINE.to_string());
        let cpu = u32::from(self.machine_config.vcpu_count);
        let memory = u64::try_from(self.machine_config.mem_size_mib)
            .map_err(|err| VmmBackendError::InvalidConfig(err.to_string()))?;
        let log = self.serial_log_path();

        let initrd = boot.initrd_path.as_ref().map(PathBuf::from);
        let (vm, timings) = vz_long_restore(
            &kernel,
            &rootfs,
            initrd.as_deref(),
            &log,
            &boot_args,
            &params.snapshot_path,
            cpu,
            memory,
            self.root_drive_read_only,
            params.resume_vm,
        )
        .map_err(|err| VmmBackendError::Internal(err.to_string()))?;

        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        let total = timings.config_nanos
            + timings.construct_nanos
            + timings.restore_nanos
            + timings.resume_nanos;
        eprintln!(
            "hephaestus-firecracker: snapshot/load total={:.1}ms \
             (config={:.1} construct={:.1} restore={:.1} resume={:.1}) resume={}",
            ms(total),
            ms(timings.config_nanos),
            ms(timings.construct_nanos),
            ms(timings.restore_nanos),
            ms(timings.resume_nanos),
            params.resume_vm,
        );

        self.vm = Some(Arc::new(vm));
        self.state = if params.resume_vm {
            VmState::Running
        } else {
            VmState::Paused
        };
        self.origin = Some(RunOrigin::SnapshotLoad);
        if let Err(err) = self.start_vsock_bridge() {
            self.abort_boot();
            return Err(err);
        }
        self.metrics.snapshot_loads = self.metrics.snapshot_loads.saturating_add(1);
        self.flush_metrics();
        self.log_info("vmm", "Snapshot loaded.");
        Ok(())
    }

    fn pause(&mut self) -> Result<(), VmmBackendError> {
        match self.state {
            VmState::Running => {}
            VmState::Paused => return Ok(()),
            VmState::NotStarted => {
                return Err(VmmBackendError::InvalidState(
                    "cannot pause before InstanceStart".into(),
                ));
            }
        }
        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| VmmBackendError::Internal("running state without a VM handle".into()))?;
        vm.pause()?;
        self.state = VmState::Paused;
        self.log_info("vmm", "Vmm is paused.");
        Ok(())
    }

    fn resume(&mut self) -> Result<(), VmmBackendError> {
        match self.state {
            VmState::Paused => {}
            VmState::Running => return Ok(()),
            VmState::NotStarted => {
                return Err(VmmBackendError::InvalidState(
                    "cannot resume before InstanceStart".into(),
                ));
            }
        }
        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| VmmBackendError::Internal("paused state without a VM handle".into()))?;
        vm.resume()?;
        self.state = VmState::Running;
        self.log_info("vmm", "Vmm is resumed.");
        Ok(())
    }
}

/// Owns the host-side vsock bridge: a UDS listener whose accept loop proxies
/// `CONNECT <port>` clients to guest vsock ports. Its `Drop` stops the accept
/// loop, joins it, and removes the socket file, so the bridge's lifetime is
/// tied to the backend rather than leaking a detached thread that could later
/// touch a freed VM handle. Declared before `vm` in [`VzBackend`] so it tears
/// down first.
#[derive(Debug)]
struct VsockBridge {
    running: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    uds_path: PathBuf,
}

impl VsockBridge {
    /// Bind `uds_path` and spawn the accept loop. The loop holds an `Arc`
    /// clone of the VM handle, so the VZ handle provably outlives every
    /// `connect_vsock` it issues.
    fn start(vm: Arc<dyn VzVmHandle>, uds_path: PathBuf) -> Result<Self, VmmBackendError> {
        let _ = std::fs::remove_file(&uds_path);
        let listener = UnixListener::bind(&uds_path).map_err(|err| {
            VmmBackendError::InvalidConfig(format!(
                "cannot bind vsock uds_path {}: {err}",
                uds_path.display()
            ))
        })?;
        // Blocking accept: zero per-connection latency (a polling loop added
        // up to its poll interval of latency, which broke handshakes with
        // clients that briefly wait for an early error reply) and no idle
        // wakeups for a long-lived VM. Shutdown wakes the blocked `accept`
        // via a self-connect in `Drop`.
        let running = Arc::new(AtomicBool::new(true));
        let accept = {
            let running = running.clone();
            std::thread::spawn(move || accept_loop(&listener, &vm, &running))
        };
        Ok(Self {
            running,
            accept: Some(accept),
            uds_path,
        })
    }
}

impl Drop for VsockBridge {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // Wake the blocked `accept` by connecting to our own socket; the loop
        // then observes `running == false` and returns. Reliable because the
        // listener is still bound (we remove the file only after the join).
        if let Ok(stream) = UnixStream::connect(&self.uds_path) {
            drop(stream);
        }
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.uds_path);
    }
}

/// Accept loop for [`VsockBridge`]. Blocks in `accept`, spawning a short-lived
/// proxy thread per accepted connection, until a self-connect on teardown wakes
/// it with `running` cleared.
fn accept_loop(listener: &UnixListener, vm: &Arc<dyn VzVmHandle>, running: &AtomicBool) {
    for stream in listener.incoming() {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let Ok(host) = stream else { continue };
        let vm = vm.clone();
        std::thread::spawn(move || bridge_connection(host, vm));
    }
}

/// Proxy one accepted host connection to a guest vsock port. Reads the
/// `CONNECT <port>` handshake under a read timeout (so a client that never
/// sends it can't park this thread forever), connects to the guest, then
/// releases the VM `Arc` before the long-lived byte-copy so a stalled proxy
/// never keeps the VM handle alive past its owner.
fn bridge_connection(mut host: UnixStream, vm: Arc<dyn VzVmHandle>) {
    let _ = host.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(line) = read_connect_line(&mut host) else {
        let _ = host.write_all(b"ERR invalid CONNECT line\n");
        return;
    };
    let Some(port) = parse_connect_line(&line) else {
        let _ = host.write_all(b"ERR invalid CONNECT line\n");
        return;
    };
    let mut guest = match vm.connect_vsock(port) {
        Ok(guest) => guest,
        Err(_) => {
            let _ = host.write_all(b"ERR connect failed\n");
            return;
        }
    };
    // The vsock fd is now independent of the VM handle; drop the Arc so a
    // long-lived copy can't pin the VM past teardown.
    drop(vm);
    // Restore blocking semantics for the streaming copy (legit vsock proxy
    // connections are long-lived).
    let _ = host.set_read_timeout(None);
    let Ok(mut host_to_guest) = host.try_clone() else {
        return;
    };
    let Ok(mut guest_to_host) = guest.try_clone() else {
        return;
    };
    let a = std::thread::spawn(move || std::io::copy(&mut host_to_guest, &mut guest));
    let b = std::thread::spawn(move || std::io::copy(&mut guest_to_host, &mut host));
    let _ = a.join();
    let _ = b.join();
}

fn read_connect_line(stream: &mut std::os::unix::net::UnixStream) -> Option<String> {
    let mut out = Vec::with_capacity(32);
    let mut byte = [0u8; 1];
    while out.len() < 64 {
        stream.read_exact(&mut byte).ok()?;
        out.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(out).ok();
        }
    }
    None
}

fn parse_connect_line(line: &str) -> Option<u32> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let rest = trimmed.strip_prefix("CONNECT ")?;
    let port = rest.parse::<u32>().ok()?;
    (port > 0).then_some(port)
}

fn validate_mmds_link_local_addr(addr: &str) -> Result<(), VmmBackendError> {
    let parsed: Ipv4Addr = addr.parse().map_err(|err| {
        VmmBackendError::InvalidConfig(format!("mmds.ipv4_address must be IPv4: {err}"))
    })?;
    let octets = parsed.octets();
    if octets[0] == 169 && octets[1] == 254 {
        Ok(())
    } else {
        Err(VmmBackendError::InvalidConfig(
            "mmds.ipv4_address must be in 169.254.0.0/16".into(),
        ))
    }
}

fn merge_json(dst: &mut Value, patch: Value) {
    match (dst, patch) {
        (Value::Object(dst_map), Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                if value.is_null() {
                    dst_map.remove(&key);
                } else {
                    merge_json(dst_map.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        (dst_slot, value) => *dst_slot = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hephaestus_fc_api::vmm_config::snapshot::MemBackendConfig;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    #[derive(Debug, Default)]
    struct FakeVm {
        pauses: Arc<AtomicU32>,
        resumes: Arc<AtomicU32>,
        mmds_refreshes: Arc<AtomicU32>,
    }

    impl FakeVm {
        fn boxed() -> (
            Arc<dyn VzVmHandle>,
            Arc<AtomicU32>,
            Arc<AtomicU32>,
            Arc<AtomicU32>,
        ) {
            let pauses = Arc::new(AtomicU32::new(0));
            let resumes = Arc::new(AtomicU32::new(0));
            let mmds = Arc::new(AtomicU32::new(0));
            (
                Arc::new(Self {
                    pauses: pauses.clone(),
                    resumes: resumes.clone(),
                    mmds_refreshes: mmds.clone(),
                }),
                pauses,
                resumes,
                mmds,
            )
        }
    }

    impl VzVmHandle for FakeVm {
        fn serve_mmds_vsock(&self, _port: u32, _json: &[u8]) -> Result<(), VmmBackendError> {
            self.mmds_refreshes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn connect_vsock(&self, _port: u32) -> Result<UnixStream, VmmBackendError> {
            Err(VmmBackendError::Internal(
                "connect_vsock unsupported in tests".into(),
            ))
        }

        fn save_state(&self, path: &Path) -> Result<(), VmmBackendError> {
            std::fs::write(path, b"fake-vz-state")
                .map_err(|err| VmmBackendError::Internal(err.to_string()))
        }

        fn pause(&self) -> Result<(), VmmBackendError> {
            self.pauses.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn resume(&self) -> Result<(), VmmBackendError> {
            self.resumes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "hephaestus-backend-test-{}-{name}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn snapshot_params() -> CreateSnapshotParams {
        CreateSnapshotParams {
            snapshot_path: temp_path("state"),
            mem_file_path: temp_path("mem"),
            snapshot_type: SnapshotType::Full,
            version: None,
        }
    }

    #[test]
    fn pause_resume_state_machine_calls_vm_once_and_is_idempotent() {
        let (vm, pauses, resumes, _) = FakeVm::boxed();
        let mut backend = VzBackend::new("test".into());
        backend.vm = Some(vm);
        backend.state = VmState::Running;
        backend.origin = Some(RunOrigin::ColdBoot);

        backend.pause().unwrap();
        backend.pause().unwrap();
        assert_eq!(backend.state, VmState::Paused);
        assert_eq!(pauses.load(Ordering::Relaxed), 1);

        backend.resume().unwrap();
        backend.resume().unwrap();
        assert_eq!(backend.state, VmState::Running);
        assert_eq!(resumes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn machine_config_matches_upstream_preboot_contract() {
        let mut backend = VzBackend::new("test".into());

        backend
            .put_machine_config(MachineConfig {
                vcpu_count: 2,
                mem_size_mib: 256,
                smt: false,
                cpu_template: None,
                track_dirty_pages: true,
                huge_pages: Default::default(),
            })
            .unwrap();
        backend
            .patch_machine_config(MachineConfigUpdate {
                vcpu_count: Some(1),
                mem_size_mib: Some(128),
                smt: Some(false),
                cpu_template: None,
                track_dirty_pages: Some(false),
                huge_pages: None,
            })
            .unwrap();
        assert_eq!(backend.get_machine_config(), MachineConfig::default());

        let err = backend
            .patch_machine_config(MachineConfigUpdate::default())
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidConfig(_)));

        let err = backend
            .put_machine_config(MachineConfig {
                vcpu_count: 0,
                ..MachineConfig::default()
            })
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidConfig(_)));

        // Above the Firecracker cap (32) is rejected like upstream, not
        // silently accepted and passed to VZ.
        let err = backend
            .put_machine_config(MachineConfig {
                vcpu_count: MAX_SUPPORTED_VCPUS + 1,
                ..MachineConfig::default()
            })
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidConfig(_)));

        let err = backend
            .patch_machine_config(MachineConfigUpdate {
                cpu_template: Some(serde_json::json!("T2")),
                ..MachineConfigUpdate::default()
            })
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::NotSupported(_)));
    }

    #[test]
    fn preboot_only_config_rejects_after_start() {
        let mut backend = VzBackend::new("test".into());
        backend.state = VmState::Running;

        let err = backend
            .put_machine_config(MachineConfig::default())
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidState(_)));

        let err = backend
            .configure_vsock(VsockConfig {
                guest_cid: 3,
                uds_path: temp_path("vsock.sock"),
                vsock_id: None,
            })
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidState(_)));
    }

    #[test]
    fn drive_patch_is_preboot_and_updates_root_path() {
        let rootfs = temp_path("rootfs.ext4");
        std::fs::write(&rootfs, b"rootfs").unwrap();
        let mut backend = VzBackend::new("test".into());

        backend
            .update_block_device(BlockDeviceUpdateConfig {
                drive_id: "root".into(),
                path_on_host: Some(rootfs.display().to_string()),
                rate_limiter: None,
            })
            .unwrap();
        assert_eq!(backend.root_drive, Some(rootfs.clone()));

        backend.state = VmState::Running;
        let err = backend
            .update_block_device(BlockDeviceUpdateConfig {
                drive_id: "root".into(),
                path_on_host: Some(rootfs.display().to_string()),
                rate_limiter: None,
            })
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidState(_)));
    }

    #[test]
    fn snapshot_create_origin_gating_matches_documented_contract() {
        let mut backend = VzBackend::new("test".into());
        backend.state = VmState::Paused;
        backend.origin = Some(RunOrigin::Pool);
        let err = backend.create_snapshot(snapshot_params()).unwrap_err();
        assert!(matches!(err, VmmBackendError::NotSupported(_)));

        let (vm, _, _, _) = FakeVm::boxed();
        let mut backend = VzBackend::new("test".into());
        backend.vm = Some(vm);
        backend.state = VmState::Paused;
        backend.origin = Some(RunOrigin::ColdBoot);
        let params = snapshot_params();
        backend.create_snapshot(params.clone()).unwrap();
        assert_eq!(
            std::fs::read(&params.snapshot_path).unwrap(),
            b"fake-vz-state"
        );
        assert_eq!(std::fs::read(&params.mem_file_path).unwrap(), b"");
    }

    #[test]
    fn snapshot_load_rejects_upstream_linux_only_modes_before_vz_restore() {
        let base = || LoadSnapshotConfig {
            snapshot_path: temp_path("snapshot"),
            mem_file_path: Some(temp_path("mem")),
            mem_backend: None,
            enable_diff_snapshots: false,
            track_dirty_pages: false,
            resume_vm: false,
        };

        let mut backend = VzBackend::new("test".into());
        let mut cfg = base();
        cfg.enable_diff_snapshots = true;
        let err = backend.load_snapshot(cfg).unwrap_err();
        assert!(matches!(err, VmmBackendError::NotSupported(_)));

        let mut cfg = base();
        cfg.track_dirty_pages = true;
        let err = backend.load_snapshot(cfg).unwrap_err();
        assert!(matches!(err, VmmBackendError::NotSupported(_)));

        let mut cfg = base();
        cfg.mem_backend = Some(MemBackendConfig {
            backend_path: temp_path("uffd"),
            backend_type: MemBackendType::Uffd,
        });
        let err = backend.load_snapshot(cfg).unwrap_err();
        assert!(matches!(err, VmmBackendError::NotSupported(_)));
    }

    #[test]
    fn mmds_config_validates_firecracker_link_local_address() {
        let mut backend = VzBackend::new("test".into());
        backend
            .configure_mmds(MmdsConfig {
                ipv4_address: Some("169.254.169.254".into()),
                network_interfaces: vec!["eth0".into()],
                version: None,
            })
            .unwrap();

        let err = backend
            .configure_mmds(MmdsConfig {
                ipv4_address: Some("10.0.0.2".into()),
                network_interfaces: vec!["eth0".into()],
                version: None,
            })
            .unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidConfig(_)));
    }

    #[test]
    fn mmds_patch_refreshes_running_vsock_service() {
        let (vm, _, _, mmds_refreshes) = FakeVm::boxed();
        let mut backend = VzBackend::new("test".into());
        backend.vm = Some(vm);
        backend.state = VmState::Running;

        backend
            .put_mmds(serde_json::json!({"meta": {"a": 1, "b": 2}}))
            .unwrap();
        backend
            .patch_mmds(serde_json::json!({"meta": {"b": null, "c": 3}}))
            .unwrap();

        assert_eq!(
            backend.get_mmds(),
            serde_json::json!({"meta": {"a": 1, "c": 3}})
        );
        assert_eq!(mmds_refreshes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn vsock_bridge_binds_reports_connect_errors_and_cleans_up_on_drop() {
        let (vm, _, _, _) = FakeVm::boxed();
        let uds = temp_path("vsock-bridge.sock");
        let bridge = VsockBridge::start(vm, uds.clone()).unwrap();
        assert!(uds.exists(), "bridge should bind the uds path");

        // FakeVm::connect_vsock errors, so a well-formed CONNECT should get an
        // ERR reply (and not hang) rather than a proxied stream.
        let mut client = UnixStream::connect(&uds).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"CONNECT 5\n").unwrap();
        let mut reply = String::new();
        let _ = client.read_to_string(&mut reply);
        assert!(
            reply.starts_with("ERR"),
            "expected ERR reply, got {reply:?}"
        );

        // Drop stops the accept loop, joins it, and removes the socket file.
        drop(bridge);
        assert!(!uds.exists(), "Drop should remove the uds file");
    }

    #[test]
    fn insert_block_device_records_is_read_only() {
        use hephaestus_fc_api::vmm_config::drive::BlockDeviceConfig;
        let rootfs = temp_path("ro-rootfs.ext4");
        std::fs::write(&rootfs, b"rootfs").unwrap();

        // Omitted is_read_only defaults to writable (matches Firecracker).
        let mut backend = VzBackend::new("test".into());
        backend
            .insert_block_device(BlockDeviceConfig {
                drive_id: "root".into(),
                is_root_device: true,
                path_on_host: Some(rootfs.display().to_string()),
                is_read_only: None,
                ..Default::default()
            })
            .unwrap();
        assert!(!backend.root_drive_read_only);

        // is_read_only: true is retained so the boot path attaches read-only.
        let mut backend = VzBackend::new("test".into());
        backend
            .insert_block_device(BlockDeviceConfig {
                drive_id: "root".into(),
                is_root_device: true,
                path_on_host: Some(rootfs.display().to_string()),
                is_read_only: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert!(backend.root_drive_read_only);
    }

    #[test]
    fn abort_boot_rolls_back_to_preboot_but_keeps_config() {
        let (vm, _, _, _) = FakeVm::boxed();
        let mut backend = VzBackend::new("test".into());
        backend.vm = Some(vm);
        backend.state = VmState::Running;
        backend.origin = Some(RunOrigin::ColdBoot);
        backend.machine_config = MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 256,
            ..MachineConfig::default()
        };

        backend.abort_boot();

        assert!(backend.vm.is_none());
        assert!(backend.vsock_bridge.is_none());
        assert!(backend.pool_slot.is_none());
        assert_eq!(backend.origin, None);
        assert_eq!(backend.state, VmState::NotStarted);
        // Pre-boot config survives so the client can retry InstanceStart.
        assert_eq!(backend.machine_config.vcpu_count, 2);
    }

    #[test]
    fn start_vsock_bridge_surfaces_unbindable_uds_path() {
        let (vm, _, _, _) = FakeVm::boxed();
        let mut backend = VzBackend::new("test".into());
        backend.vm = Some(vm);
        backend.state = VmState::Running;
        backend.vsock = Some(VsockConfig {
            guest_cid: 3,
            uds_path: PathBuf::from("/no/such/dir/hephaestus-vsock.sock"),
            vsock_id: None,
        });

        // A bind failure must be a surfaced error (which start_micro_vm turns
        // into an abort_boot rollback), not a silently-running VM.
        let err = backend.start_vsock_bridge().unwrap_err();
        assert!(matches!(err, VmmBackendError::InvalidConfig(_)));
        assert!(backend.vsock_bridge.is_none());
    }
}
