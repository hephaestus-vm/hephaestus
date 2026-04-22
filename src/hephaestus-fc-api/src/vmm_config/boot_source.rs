// upstream: vendor/firecracker/vmm/src/vmm_config/boot_source.rs
//
// Kept: `DEFAULT_KERNEL_CMDLINE`, `BootSourceConfig` (wire struct),
// `BootSourceConfigError` (wire error variants relevant to the HTTP API).
// Dropped: `BootSource`/`BootConfig` builders and `BootConfig::new`, which
// pull in `linux_loader::cmdline::Cmdline` and open host files. The macOS
// backend validates kernel/initrd paths in its own boot path.

use std::io;

use serde::{Deserialize, Serialize};

/// Default guest kernel command line:
/// - `reboot=k` shut down the guest on reboot, instead of well... rebooting;
/// - `panic=1` on panic, reboot after 1 second;
/// - `nomodule` disable loadable kernel module support;
/// - `8250.nr_uarts=0` disable 8250 serial interface;
/// - `i8042.noaux` do not probe the i8042 controller for an attached mouse (save boot time);
/// - `i8042.nomux` do not probe i8042 for a multiplexing controller (save boot time);
/// - `i8042.dumbkbd` do not attempt to control kbd state via the i8042 (save boot time).
/// - `swiotlb=noforce` disable software bounce buffers (SWIOTLB)
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=1 nomodule 8250.nr_uarts=0 i8042.noaux \
                                          i8042.nomux i8042.dumbkbd swiotlb=noforce";

/// Strongly typed data structure used to configure the boot source of the
/// microvm.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootSourceConfig {
    /// Path of the kernel image.
    pub kernel_image_path: String,
    /// Path of the initrd, if there is one.
    pub initrd_path: Option<String>,
    /// The boot arguments to pass to the kernel. If this field is uninitialized,
    /// DEFAULT_KERNEL_CMDLINE is used.
    pub boot_args: Option<String>,
}

/// Errors associated with actions on `BootSourceConfig`.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BootSourceConfigError {
    /// The kernel file cannot be opened: {0}
    InvalidKernelPath(io::Error),
    /// The initrd file cannot be opened due to invalid path or invalid permissions. {0}
    InvalidInitrdPath(io::Error),
    /// The kernel command line is invalid: {0}
    InvalidKernelCommandLine(String),
}
