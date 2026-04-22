// upstream: vendor/firecracker/vmm/src/vmm_config/machine_config.rs
//
// Kept: `MachineConfig` and `MachineConfigUpdate` wire structs. Dropped:
// `HugePageConfig::mmap_flags` (uses `libc::MAP_HUGETLB`, Linux-only) and
// the `cpu_template` field's strongly-typed `CpuTemplateType` — that enum
// lives in the Linux VMM's `cpu_config::templates` module. We accept the
// field on the wire as an opaque `serde_json::Value` so clients still pass
// `deny_unknown_fields` parsing; the backend ignores it and logs a warning.

use std::fmt::Debug;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The default memory size of the VM, in MiB.
pub const DEFAULT_MEM_SIZE_MIB: usize = 128;
/// Firecracker aims to support small scale workloads only, so limit the maximum
/// vCPUs supported.
pub const MAX_SUPPORTED_VCPUS: u8 = 32;

/// Errors associated with configuring the microVM.
#[rustfmt::skip]
#[derive(Debug, thiserror::Error, displaydoc::Display, PartialEq, Eq)]
pub enum MachineConfigError {
    /// The memory size (MiB) is smaller than the previously set balloon device target size.
    IncompatibleBalloonSize,
    /// The memory size (MiB) is either 0, or not a multiple of the configured page size.
    InvalidMemorySize,
    /// The number of vCPUs must be greater than 0, less than {MAX_SUPPORTED_VCPUS:} and must be 1 or an even number if SMT is enabled.
    InvalidVcpuCount,
    /// Could not get the configuration of the previously installed balloon device to validate the memory size.
    InvalidVmState,
    /// Enabling simultaneous multithreading is not supported on aarch64.
    #[cfg(target_arch = "aarch64")]
    SmtNotSupported,
    /// Could not determine host kernel version when checking hugetlbfs compatibility
    KernelVersion,
}

/// Describes the possible (huge)page configurations for a microVM's memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HugePageConfig {
    /// Do not use hugepages, e.g. back guest memory by 4K
    #[default]
    None,
    /// Back guest memory by 2MB hugetlbfs pages
    #[serde(rename = "2M")]
    Hugetlbfs2M,
}

/// Struct used in PUT `/machine-config` API call.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    /// Number of vcpu to start.
    pub vcpu_count: u8,
    /// The memory size in MiB.
    pub mem_size_mib: usize,
    /// Enables or disabled SMT.
    #[serde(default)]
    pub smt: bool,
    /// A CPU template that it is used to filter the CPU features exposed to the guest.
    /// Opaque on macOS — accepted for wire-compat, not honored by the backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_template: Option<Value>,
    /// Enables or disables dirty page tracking. Enabling allows incremental snapshots.
    #[serde(default)]
    pub track_dirty_pages: bool,
    /// Configures what page size Firecracker should use to back guest memory.
    #[serde(default)]
    pub huge_pages: HugePageConfig,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            vcpu_count: 1,
            mem_size_mib: DEFAULT_MEM_SIZE_MIB,
            smt: false,
            cpu_template: None,
            track_dirty_pages: false,
            huge_pages: HugePageConfig::None,
        }
    }
}

/// Struct used in PATCH `/machine-config` API call.
/// Mirrors all the fields in `MachineConfig` but each is optional; `None`
/// means "leave as-is".
#[derive(Clone, Default, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfigUpdate {
    /// Number of vcpu to start.
    #[serde(default)]
    pub vcpu_count: Option<u8>,
    /// The memory size in MiB.
    #[serde(default)]
    pub mem_size_mib: Option<usize>,
    /// Enables or disabled SMT.
    #[serde(default)]
    pub smt: Option<bool>,
    /// A CPU template that it is used to filter the CPU features exposed to the guest.
    #[serde(default)]
    pub cpu_template: Option<Value>,
    /// Enables or disables dirty page tracking. Enabling allows incremental snapshots.
    #[serde(default)]
    pub track_dirty_pages: Option<bool>,
    /// Configures what page size Firecracker should use to back guest memory.
    #[serde(default)]
    pub huge_pages: Option<HugePageConfig>,
}

impl MachineConfigUpdate {
    /// Returns `true` iff all fields are `None`.
    pub fn is_empty(&self) -> bool {
        self == &Default::default()
    }
}
