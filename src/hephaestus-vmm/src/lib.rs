//! Placeholder for the containerization-backed VMM.
//!
//! M0 only exposes a ping passthrough to prove the FFI pipeline. Real VM
//! lifecycle (boot, wait, stop) arrives in M1/M2.

pub fn ping() -> &'static str {
    hephaestus_bridge::ping()
}
