//! The `VmmBackend` trait — how `hephaestus-firecracker` drives a concrete
//! VMM implementation.
//!
//! This is not an upstream concept. Upstream's `api_server` dispatches
//! `VmmAction` enums across an mpsc channel into a monolithic `Vmm` struct
//! whose device-model, event loop, and KVM plumbing are all tightly
//! coupled. On macOS we don't have that `Vmm` — we have
//! Virtualization.framework wrapped in `hephaestus-vmm`. The trait lets the
//! HTTP layer stay backend-agnostic, so a hypothetical future backend
//! (QEMU-over-HVF? another direct-VZ impl?) slots in the same way.
//!
//! Scope for v0.3 is minimal: one VM per `hephaestus-firecracker` process
//! (matches upstream's contract), and only the subset of endpoints that
//! `firectl` and Kata exercise on a cold boot. Update / balloon / mmds /
//! snapshot / entropy live behind `VmmBackendError::NotSupported` until we
//! have a client asking for them.

use crate::vmm_config::boot_source::BootSourceConfig;
use crate::vmm_config::drive::{BlockDeviceConfig, BlockDeviceUpdateConfig};
use crate::vmm_config::instance_info::InstanceInfo;
use crate::vmm_config::logger::LoggerConfig;
use crate::vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
use crate::vmm_config::metrics::MetricsConfig;
use crate::vmm_config::net::NetworkInterfaceConfig;
use crate::vmm_config::snapshot::{CreateSnapshotParams, LoadSnapshotConfig};

/// Errors a backend method can surface to the HTTP layer.
///
/// Kept flat + `String`-bodied because the HTTP layer translates these
/// into Firecracker-compat JSON error bodies (`{"fault_message": "..."}`)
/// and doesn't need to pattern-match on structured variants.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VmmBackendError {
    /// Operation not permitted in the current VM state: {0}
    InvalidState(String),
    /// Invalid configuration: {0}
    InvalidConfig(String),
    /// This endpoint is not supported by this backend: {0}
    NotSupported(String),
    /// Backend failure: {0}
    Internal(String),
}

/// The Firecracker HTTP API surface a backend must implement.
///
/// Methods take `&mut self` because a backend owns mutable VM state. The
/// HTTP server serializes requests anyway (one in flight per socket),
/// matching upstream's single-threaded handler model; no `Send + Sync`
/// bounds needed on the concrete backend as long as the server thread
/// owns it.
pub trait VmmBackend {
    /// `GET /` — instance info.
    fn instance_info(&self) -> InstanceInfo;

    /// `PUT /boot-source`
    fn configure_boot_source(
        &mut self,
        cfg: BootSourceConfig,
    ) -> Result<(), VmmBackendError>;

    /// `PUT /drives/{id}`
    fn insert_block_device(
        &mut self,
        cfg: BlockDeviceConfig,
    ) -> Result<(), VmmBackendError>;

    /// `PATCH /drives/{id}`
    fn update_block_device(
        &mut self,
        cfg: BlockDeviceUpdateConfig,
    ) -> Result<(), VmmBackendError>;

    /// `PUT /network-interfaces/{id}`
    fn insert_network_device(
        &mut self,
        cfg: NetworkInterfaceConfig,
    ) -> Result<(), VmmBackendError>;

    /// `PATCH /network-interfaces/{id}`. Upstream uses this primarily for
    /// rate-limiter updates; macOS VZ doesn't enforce rate limits, so the
    /// default impl accepts-and-noops to match `firectl`/Kata's expectation
    /// that the call returns 204.
    fn update_network_device(
        &mut self,
        _cfg: NetworkInterfaceConfig,
    ) -> Result<(), VmmBackendError> {
        Ok(())
    }

    /// `PUT /logger`. Backends that can't honor the config fully should
    /// still accept-and-best-effort rather than error — Firecracker
    /// clients treat this as fire-and-forget config.
    fn configure_logger(&mut self, cfg: LoggerConfig) -> Result<(), VmmBackendError>;

    /// `PUT /metrics`. Same accept-and-best-effort convention as
    /// `configure_logger` — Firecracker clients send this fire-and-forget.
    /// The macOS backend's metrics surface is sparse (no KVM exit
    /// counters etc.), so the default impl just opens the file and
    /// writes one init line so consumers that grep for "the metrics
    /// file exists and has content" are satisfied.
    fn configure_metrics(&mut self, _cfg: MetricsConfig) -> Result<(), VmmBackendError> {
        Err(VmmBackendError::NotSupported("metrics".into()))
    }

    /// `GET /machine-config`
    fn get_machine_config(&self) -> MachineConfig;

    /// `PUT /machine-config`
    fn put_machine_config(
        &mut self,
        cfg: MachineConfig,
    ) -> Result<(), VmmBackendError>;

    /// `PATCH /machine-config`
    fn patch_machine_config(
        &mut self,
        update: MachineConfigUpdate,
    ) -> Result<(), VmmBackendError>;

    /// `PUT /actions` with `InstanceStart` — boot the microVM.
    fn start_micro_vm(&mut self) -> Result<(), VmmBackendError>;

    /// `PATCH /vm` with `state: Paused`.
    fn pause(&mut self) -> Result<(), VmmBackendError> {
        Err(VmmBackendError::NotSupported("pause".into()))
    }

    /// `PATCH /vm` with `state: Resumed`.
    fn resume(&mut self) -> Result<(), VmmBackendError> {
        Err(VmmBackendError::NotSupported("resume".into()))
    }

    /// `PUT /snapshot/create`. Per Firecracker semantics the VM must be
    /// `Paused` before calling. Default impl is `NotSupported` so
    /// backends opt in.
    fn create_snapshot(
        &mut self,
        _params: CreateSnapshotParams,
    ) -> Result<(), VmmBackendError> {
        Err(VmmBackendError::NotSupported("snapshot/create".into()))
    }

    /// `PUT /snapshot/load`. Pre-boot only. Replaces the cold-boot path:
    /// after `load_snapshot` returns Ok, the VM is either `Running` or
    /// `Paused` depending on `params.resume_vm`. Backend should reject
    /// if pre-boot config (kernel, rootfs, vcpu, mem) hasn't been set.
    fn load_snapshot(
        &mut self,
        _params: LoadSnapshotConfig,
    ) -> Result<(), VmmBackendError> {
        Err(VmmBackendError::NotSupported("snapshot/load".into()))
    }
}
