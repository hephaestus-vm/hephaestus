//! Mirror hephaestus-cli's build.rs: force-load the Swift archive so Swift's
//! module-init sections run, and emit the Swift runtime rpath so binaries
//! find libswift*.dylib at runtime without relying on the dyld cache.

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
    println!("cargo:rustc-link-arg-bin=hephaestus-firecracker=-Wl,-force_load,{archive}");
    // Xcode 27 beta's Swift emits a strong reference to
    // libswiftCompatibilitySpan.dylib; the dyld cache only resolves weak
    // refs, so we need an LC_RPATH pointing at /usr/lib/swift where the
    // dylib lives on macOS 26+.
    println!("cargo:rustc-link-arg-bin=hephaestus-firecracker=-Wl,-rpath,/usr/lib/swift");
}
