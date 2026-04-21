//! Build script:
//!   1. Runs cbindgen to emit the C header shared with Swift.
//!   2. Compiles the sibling Swift Package via xcrun swift build.
//!   3. Wires the resulting static archive + Swift runtime into the link line.
//!
//! macOS + Apple Silicon only. On other targets the crate compiles empty.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // cbindgen should re-run whenever our Rust FFI sources change.
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_pkg = manifest_dir
        .join("..")
        .join("..")
        .join("swift")
        .join("HephaestusBridge")
        .canonicalize()
        .expect("swift package not found");

    // Regenerate the C header the Swift side imports via module.modulemap.
    let header_out = swift_pkg
        .join("Sources")
        .join("CHephaestusBridge")
        .join("include")
        .join("hephaestus_bridge.h");
    generate_header(&manifest_dir, &header_out);

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        println!("cargo:warning=hephaestus-bridge: skipping Swift build on non-macOS target");
        return;
    }

    // Re-run if any Swift source or manifest changes.
    println!("cargo:rerun-if-changed={}", swift_pkg.join("Package.swift").display());
    rerun_for_dir(&swift_pkg.join("Sources"));

    // Build the Swift package via xcrun so we pick the Xcode toolchain whose
    // Swift version matches the installed SDK (CommandLineTools ships an
    // older toolchain that can't read the macOS 26 SDK).
    //
    // We force the target triple to macos15.0 so every transitive SwiftPM
    // package is compiled with a deployment target high enough that the
    // Swift concurrency runtime is assumed to be in libswiftCore — otherwise
    // the compiler emits back-deploy references to @rpath/libswift_Concurrency.dylib
    // that fail to resolve on the host at runtime.
    let status = Command::new("xcrun")
        .args([
            "swift",
            "build",
            "-c",
            "release",
            "--triple",
            "arm64-apple-macosx15.0",
            "--package-path",
        ])
        .arg(&swift_pkg)
        .status()
        .expect("failed to invoke `xcrun swift build`");
    assert!(status.success(), "swift build failed (see output above)");

    // SwiftPM writes the static archive under .build/<triple>/release or
    // .build/release depending on its selected destination. We check both.
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
    // Export the archive path so the bin crate's build.rs can emit a
    // -force_load at the final link step. (cargo:rustc-link-arg emitted
    // from a library crate only applies when that crate itself is linking
    // something, which rlibs don't do.)
    println!(
        "cargo:rustc-env=HEPHAESTUS_BRIDGE_ARCHIVE={}",
        lib_dir.join("libHephaestusBridge.a").display()
    );
    println!(
        "cargo:archive={}",
        lib_dir.join("libHephaestusBridge.a").display()
    );

    // SwiftPM bundles all transitive Swift-package object files into the
    // library product archive (.a has ~1900 .o files), so we only need to
    // declare the *system* libs that the Swift package's linkerSettings
    // would have declared. These mirror apple/containerization's
    // CArchive target linkerSettings plus frameworks Containerization uses.
    for lib in ["archive", "z", "bz2", "lzma", "iconv", "c++"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    for framework in ["Foundation", "Virtualization", "Network", "Security"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    // System Swift runtime. macOS 26 guarantees these are present at
    // /usr/lib/swift. The rpath is required so the final binary can find
    // libswiftCore.dylib and friends at runtime.
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-lib=dylib=swiftFoundation");

    // Swift compatibility shims live in the Xcode toolchain (not in /usr/lib).
    // SwiftPM auto-links these via forced-load symbols baked into every
    // Swift object; without them linking fails on
    // `__swift_FORCE_LOAD_$_swiftCompatibility56` and friends.
    if let Some(toolchain_lib_dir) = xcode_swift_static_lib_dir() {
        println!("cargo:rustc-link-search=native={}", toolchain_lib_dir.display());
        for lib in [
            "swiftCompatibility50",
            "swiftCompatibility51",
            "swiftCompatibility56",
            "swiftCompatibilityConcurrency",
            "swiftCompatibilityDynamicReplacements",
            "swiftCompatibilityPacks",
        ] {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    } else {
        println!("cargo:warning=hephaestus-bridge: Xcode toolchain not found via xcode-select -p; Swift compatibility shims may fail to link");
    }
}

fn xcode_swift_static_lib_dir() -> Option<PathBuf> {
    let out = Command::new("xcode-select").arg("-p").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let dev_dir = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    let path = dev_dir
        .join("Toolchains")
        .join("XcodeDefault.xctoolchain")
        .join("usr")
        .join("lib")
        .join("swift")
        .join("macosx");
    path.exists().then_some(path)
}

fn generate_header(manifest_dir: &Path, header_path: &Path) {
    let cfg = cbindgen::Config::from_file(manifest_dir.join("cbindgen.toml"))
        .expect("failed to load cbindgen.toml");
    std::fs::create_dir_all(header_path.parent().unwrap()).ok();
    cbindgen::Builder::new()
        .with_crate(manifest_dir)
        .with_config(cfg)
        .generate()
        .expect("cbindgen generation failed")
        .write_to_file(header_path);
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
