//! `VmmBackend` implementation that drives `hephaestus-vmm`'s direct-VZ path.
//!
//! Single-VM-per-process matches upstream's contract; `VzBackend` holds
//! the accumulated pre-boot config and, once booted, an owned [`VzVm`]
//! handle. Dropping the backend stops the VM via `VzVm::Drop`.

use std::path::PathBuf;

use hephaestus_fc_api::vmm_config::boot_source::{BootSourceConfig, DEFAULT_KERNEL_CMDLINE};
use hephaestus_fc_api::vmm_config::drive::{BlockDeviceConfig, BlockDeviceUpdateConfig};
use hephaestus_fc_api::vmm_config::instance_info::{InstanceInfo, VmState};
use hephaestus_fc_api::vmm_config::logger::LoggerConfig;
use hephaestus_fc_api::vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
use hephaestus_fc_api::vmm_config::metrics::MetricsConfig;
use hephaestus_fc_api::vmm_config::net::NetworkInterfaceConfig;
use hephaestus_fc_api::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotConfig, MemBackendType, SnapshotType,
};
use hephaestus_fc_api::{VmmBackend, VmmBackendError};
use hephaestus_pool::{ClaimedSlot, Pool, PoolMatchSpec};
use hephaestus_vmm::{VzSpec, VzVm, vz_long_restore};

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

#[derive(Debug)]
pub struct VzBackend {
    id: String,
    state: VmState,
    boot_source: Option<BootSourceConfig>,
    root_drive: Option<PathBuf>,
    iface: Option<NetworkInterfaceConfig>,
    machine_config: MachineConfig,
    /// Drop order matters: `vm` is dropped before `pool_slot` so the
    /// VM tears down before the slot's `Drop` deletes the rootfs the VM
    /// was reading from.
    vm: Option<VzVm>,
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
            iface: None,
            machine_config: MachineConfig::default(),
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

    fn configure_boot_source(
        &mut self,
        cfg: BootSourceConfig,
    ) -> Result<(), VmmBackendError> {
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

    fn insert_block_device(
        &mut self,
        cfg: BlockDeviceConfig,
    ) -> Result<(), VmmBackendError> {
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
        Ok(())
    }

    fn update_block_device(
        &mut self,
        cfg: BlockDeviceUpdateConfig,
    ) -> Result<(), VmmBackendError> {
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
        // Accept pre- or post-boot: Firecracker allows PUT /logger at any
        // time. Honor log_path best-effort by appending a single
        // structured line announcing the config; future work can stream
        // server events through proper `log` crate plumbing.
        if let Some(path) = cfg.log_path.as_ref() {
            let line = format!(
                "{{\"timestamp\":\"init\",\"level\":\"{}\",\"msg\":\"hephaestus-firecracker logger configured\"}}\n",
                cfg.level.as_deref().unwrap_or("Info"),
            );
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
                .map_err(|err| {
                    VmmBackendError::InvalidConfig(format!(
                        "cannot open log_path {}: {err}",
                        path.display()
                    ))
                })?;
        }
        Ok(())
    }

    fn configure_metrics(&mut self, cfg: MetricsConfig) -> Result<(), VmmBackendError> {
        // Same shape as configure_logger: open the file, write a single
        // structured init line, return. Real periodic-flush plumbing is
        // deferred (most upstream metrics fields don't map to macOS).
        let line = "{\"timestamp\":\"init\",\"event\":\"hephaestus-firecracker metrics configured\"}\n";
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.metrics_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
            .map_err(|err| {
                VmmBackendError::InvalidConfig(format!(
                    "cannot open metrics_path {}: {err}",
                    cfg.metrics_path.display()
                ))
            })
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
        if cfg.mem_size_mib == 0 {
            return Err(VmmBackendError::InvalidConfig(
                "mem_size_mib must be > 0".into(),
            ));
        }
        self.machine_config = cfg;
        Ok(())
    }

    fn patch_machine_config(
        &mut self,
        update: MachineConfigUpdate,
    ) -> Result<(), VmmBackendError> {
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
        if let Some(v) = update.cpu_template {
            cfg.cpu_template = Some(v);
        }
        self.put_machine_config(cfg)
    }

    fn start_micro_vm(&mut self) -> Result<(), VmmBackendError> {
        self.require_preboot()?;

        let boot = self.boot_source.as_ref().ok_or_else(|| {
            VmmBackendError::InvalidState("boot-source not configured".into())
        })?;
        let rootfs = self.root_drive.clone().ok_or_else(|| {
            VmmBackendError::InvalidState("root drive not configured".into())
        })?;
        let kernel = PathBuf::from(&boot.kernel_image_path);
        let boot_args = boot
            .boot_args
            .clone()
            .unwrap_or_else(|| DEFAULT_KERNEL_CMDLINE.to_string());
        let cpu = u32::from(self.machine_config.vcpu_count);
        let memory = u64::try_from(self.machine_config.mem_size_mib)
            .map_err(|err| VmmBackendError::InvalidConfig(err.to_string()))?;
        let log = PathBuf::from(format!("/tmp/hephaestus-firecracker-{}.log", self.id));

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
                        self.vm = Some(vm);
                        self.pool_slot = Some(slot);
                        self.state = VmState::Running;
                        self.origin = Some(RunOrigin::Pool);
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
                }
                Err(err) => {
                    eprintln!(
                        "hephaestus-firecracker: pool claim error ({err}); cold-booting"
                    );
                }
            }
        }

        let mut spec = VzSpec::new(&kernel, &rootfs, &log, boot_args)
            .cpus(cpu)
            .memory_mib(memory);
        if let Some(initrd) = boot.initrd_path.as_ref() {
            spec = spec.initrd(std::path::Path::new(initrd));
        }
        let vm = spec
            .build()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;
        vm.start()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;

        self.vm = Some(vm);
        self.state = VmState::Running;
        self.origin = Some(RunOrigin::ColdBoot);
        Ok(())
    }

    fn create_snapshot(
        &mut self,
        params: CreateSnapshotParams,
    ) -> Result<(), VmmBackendError> {
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
                return Err(VmmBackendError::InvalidState(
                    "no VM running".into(),
                ));
            }
        }

        let vm = self.vm.as_ref().ok_or_else(|| {
            VmmBackendError::Internal("Paused state without a VM handle".into())
        })?;
        vm.save_state(&params.snapshot_path)
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;

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

    fn load_snapshot(
        &mut self,
        params: LoadSnapshotConfig,
    ) -> Result<(), VmmBackendError> {
        self.require_preboot()?;

        if params.enable_diff_snapshots || params.track_dirty_pages {
            return Err(VmmBackendError::NotSupported(
                "diff/dirty-page tracking (VZ has no equivalent)".into(),
            ));
        }
        if let Some(backend) = params.mem_backend.as_ref() {
            if !matches!(backend.backend_type, MemBackendType::File) {
                return Err(VmmBackendError::NotSupported(
                    "mem_backend.backend_type=Uffd (Linux-only)".into(),
                ));
            }
        }

        let boot = self.boot_source.as_ref().ok_or_else(|| {
            VmmBackendError::InvalidState(
                "snapshot/load requires PUT /boot-source first".into(),
            )
        })?;
        let rootfs = self.root_drive.clone().ok_or_else(|| {
            VmmBackendError::InvalidState(
                "snapshot/load requires PUT /drives/{id} first".into(),
            )
        })?;
        let kernel = PathBuf::from(&boot.kernel_image_path);
        let boot_args = boot
            .boot_args
            .clone()
            .unwrap_or_else(|| DEFAULT_KERNEL_CMDLINE.to_string());
        let cpu = u32::from(self.machine_config.vcpu_count);
        let memory = u64::try_from(self.machine_config.mem_size_mib)
            .map_err(|err| VmmBackendError::InvalidConfig(err.to_string()))?;
        let log = PathBuf::from(format!("/tmp/hephaestus-firecracker-{}.log", self.id));

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

        self.vm = Some(vm);
        self.state = if params.resume_vm {
            VmState::Running
        } else {
            VmState::Paused
        };
        self.origin = Some(RunOrigin::SnapshotLoad);
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
        let vm = self.vm.as_ref().ok_or_else(|| {
            VmmBackendError::Internal("running state without a VM handle".into())
        })?;
        vm.pause()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;
        self.state = VmState::Paused;
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
        let vm = self.vm.as_ref().ok_or_else(|| {
            VmmBackendError::Internal("paused state without a VM handle".into())
        })?;
        vm.resume()
            .map_err(|err| VmmBackendError::Internal(err.to_string()))?;
        self.state = VmState::Running;
        Ok(())
    }
}
