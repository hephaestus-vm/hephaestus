//! Placeholder for the containerization-backed VMM.
//!
//! M1 re-exports the bridge's `Vm` wrapper so callers don't reach across the
//! FFI crate directly. Real VM lifecycle (start, wait, stop) arrives in M2.

pub use hephaestus_bridge::{Vm, VmError};

pub fn ping() -> &'static str {
    hephaestus_bridge::ping()
}
