// swift-tools-version: 6.1
// HephaestusBridge: Swift FFI shim between the Rust hephaestus-bridge crate
// and Apple's containerization framework. M0 only exposes a ping function to
// prove the Rust<->Swift link; real LinuxContainer plumbing lands in M1/M2.

import PackageDescription

let package = Package(
    name: "HephaestusBridge",
    platforms: [
        .macOS(.v15) // macOS 26 is the real floor (apple/containerization requires it),
                     // but SwiftPM's enum lags behind; v15 is the highest enum value
                     // that SwiftPM recognizes today. The real check happens at link time.
    ],
    products: [
        .library(
            name: "HephaestusBridge",
            type: .static,
            targets: ["HephaestusBridge"]
        )
    ],
    targets: [
        .target(
            name: "HephaestusBridge",
            path: "Sources/HephaestusBridge"
        )
    ]
)
