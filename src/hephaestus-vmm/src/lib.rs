//! Containerization-backed VMM.
//!
//! Thin re-export layer over `hephaestus-bridge` so callers don't reach across
//! the FFI crate directly.

pub use hephaestus_bridge::{
    Compression, Spec, StdioSink, Vm, VmError, build_rootfs_from_tar,
};

pub fn ping() -> &'static str {
    hephaestus_bridge::ping()
}
