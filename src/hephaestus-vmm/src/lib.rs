//! Containerization-backed VMM.
//!
//! Thin re-export layer over `hephaestus-bridge` so callers don't reach across
//! the FFI crate directly.

pub use hephaestus_bridge::{Spec, StdioSink, Vm, VmError};

pub fn ping() -> &'static str {
    hephaestus_bridge::ping()
}
