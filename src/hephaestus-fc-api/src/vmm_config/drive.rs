// upstream: src/vmm/src/vmm_config/drive.rs (BlockDeviceConfig only)
//           src/vmm/src/devices/virtio/block/mod.rs (CacheType)
//           src/vmm/src/devices/virtio/block/virtio/device.rs (FileEngineType)
//
// Kept: `BlockDeviceConfig` (wire struct) and the two small serde enums the
// wire references. Dropped: `BlockBuilder`, `BlockDeviceUpdateConfig` impls,
// and the `DriveError` variants that reference the live `Block` device tree.
// Our `DriveError` here is narrowed to what the HTTP API actually signals.

use std::io;

use serde::{Deserialize, Serialize};

use super::RateLimiterConfig;

/// Configuration options for disk caching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum CacheType {
    /// Flushing mechanic not will be advertised to the guest driver
    #[default]
    Unsafe,
    /// Flushing mechanic will be advertised to the guest driver and
    /// flush requests coming from the guest will be performed using
    /// `fsync`.
    Writeback,
}

/// The engine file type, either Sync or Async (through io_uring).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum FileEngineType {
    /// Use an Async engine, based on io_uring.
    Async,
    /// Use a Sync engine, based on blocking system calls.
    #[default]
    Sync,
}

/// Use this structure to set up the Block Device before booting the kernel.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockDeviceConfig {
    /// Unique identifier of the drive.
    pub drive_id: String,
    /// Part-UUID. Represents the unique id of the boot partition of this device. It is
    /// optional and it will be used only if the `is_root_device` field is true.
    pub partuuid: Option<String>,
    /// If set to true, it makes the current device the root block device.
    /// Setting this flag to true will mount the block device in the
    /// guest under /dev/vda unless the partuuid is present.
    pub is_root_device: bool,
    /// If set to true, the drive will ignore flush requests coming from
    /// the guest driver.
    #[serde(default)]
    pub cache_type: CacheType,

    // VirtioBlock specific fields
    /// If set to true, the drive is opened in read-only mode. Otherwise, the
    /// drive is opened as read-write.
    pub is_read_only: Option<bool>,
    /// Path of the drive.
    pub path_on_host: Option<String>,
    /// Rate Limiter for I/O operations.
    pub rate_limiter: Option<RateLimiterConfig>,
    /// The type of IO engine used by the device.
    #[serde(rename = "io_engine")]
    pub file_engine_type: Option<FileEngineType>,

    // VhostUserBlock specific fields
    /// Path to the vhost-user socket.
    pub socket: Option<String>,
}

/// Errors associated with the operations allowed on a drive.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum DriveError {
    /// Attempt to add block as a root device while the root device defined as a pmem device
    AddingSecondRootDevice,
    /// Unable to create the virtio block device: {0}
    CreateBlockDevice(String),
    /// Cannot create RateLimiter: {0}
    CreateRateLimiter(io::Error),
    /// A root block device already exists!
    RootBlockDeviceAlreadyAdded,
}
