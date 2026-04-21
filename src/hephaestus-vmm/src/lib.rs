//! Containerization-backed VMM.
//!
//! Thin re-export layer over `hephaestus-bridge` so callers don't reach across
//! the FFI crate directly.

pub use hephaestus_bridge::{
    Compression, Spec, StdioSink, Vm, VmError, build_rootfs_from_tar, vz_boot, vz_sh,
    vz_snapshot_restore, vz_snapshot_save,
};

pub fn ping() -> &'static str {
    hephaestus_bridge::ping()
}
