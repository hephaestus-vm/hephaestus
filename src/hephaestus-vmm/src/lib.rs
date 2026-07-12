//! Containerization-backed VMM.
//!
//! Thin re-export layer over `hephaestus-bridge` so callers don't reach across
//! the FFI crate directly.

pub use hephaestus_bridge::{
    Compression, HbRestoreTimings, Spec, StdioSink, Vm, VmError, VzSpec, VzVm, allocate_ip_octet,
    build_rootfs_from_tar, vz_boot, vz_exec, vz_exec_snapshot_restore, vz_exec_snapshot_save,
    vz_long_restore, vz_pool_restore_long, vz_sh, vz_snapshot_restore, vz_snapshot_save,
    vz_stock_pool_restore_long,
};

pub fn ping() -> &'static str {
    hephaestus_bridge::ping()
}
