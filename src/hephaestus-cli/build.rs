//! Emits `-force_load <path>` on the final binary's link step so Swift's
//! module-init sections from the statically-linked HephaestusBridge archive
//! actually run. Without this, NIO + swift-atomics module registration is
//! skipped and the process crashes at runtime allocating
//! `ManagedAtomic<Bool>` because its type metadata is never registered.
//!
//! We read the archive path via the `DEP_HEPHAESTUS_BRIDGE_ARCHIVE` env var
//! that cargo sets from `cargo:archive=...` in hephaestus-bridge's build.rs.

use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        return;
    }
    let Ok(archive) = env::var("DEP_HEPHAESTUS_BRIDGE_NATIVE_ARCHIVE") else {
        println!(
            "cargo:warning=DEP_HEPHAESTUS_BRIDGE_NATIVE_ARCHIVE not set; hephaestus-bridge may not have run its build.rs"
        );
        return;
    };
    println!("cargo:rerun-if-env-changed=DEP_HEPHAESTUS_BRIDGE_NATIVE_ARCHIVE");
    println!("cargo:rustc-link-arg=-Wl,-force_load,{archive}");
}
