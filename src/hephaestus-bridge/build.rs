//! Build script that compiles the sibling Swift Package and wires it into
//! the Rust link line. macOS + Apple Silicon only.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Only build the Swift bridge on macOS. On other platforms the crate
    // compiles to an empty lib (useful for `cargo check` in CI containers).
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        println!("cargo:warning=hephaestus-bridge: skipping Swift build on non-macOS target");
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_pkg = manifest_dir
        .join("..")
        .join("..")
        .join("swift")
        .join("HephaestusBridge");
    let swift_pkg = swift_pkg.canonicalize().expect("swift package not found");

    // Re-run if any Swift source or the manifest changes.
    println!("cargo:rerun-if-changed={}", swift_pkg.join("Package.swift").display());
    rerun_for_dir(&swift_pkg.join("Sources"));

    // Build the Swift package via xcrun so we pick the Xcode toolchain whose
    // Swift version matches the installed SDK (CommandLineTools ships an
    // older toolchain that can't read the macOS 26 SDK).
    let status = Command::new("xcrun")
        .args(["swift", "build", "-c", "release", "--package-path"])
        .arg(&swift_pkg)
        .status()
        .expect("failed to invoke `xcrun swift build`");
    assert!(status.success(), "swift build failed (see output above)");

    // SwiftPM puts the static archive under .build/<triple>/release by default
    // when a specific destination is selected; otherwise it's .build/release.
    // We look for both.
    let build_root = swift_pkg.join(".build");
    let candidates = [
        build_root.join("release"),
        build_root.join("arm64-apple-macosx").join("release"),
        build_root.join("apple").join("Products").join("Release"),
    ];
    let lib_dir = candidates
        .iter()
        .find(|p| p.join("libHephaestusBridge.a").exists())
        .unwrap_or_else(|| {
            panic!(
                "libHephaestusBridge.a not found under {}",
                build_root.display()
            )
        });

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=HephaestusBridge");

    // Link against the system Swift runtime shipped under /usr/lib/swift.
    // macOS 26+ guarantees these are present.
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-lib=dylib=swiftFoundation");
}

fn rerun_for_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rerun_for_dir(&path);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}
