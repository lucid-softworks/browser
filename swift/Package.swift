// swift-tools-version:5.9
import PackageDescription
import Foundation

// Resolve the Rust engine's static-lib directory relative to THIS manifest, so the build works on
// any machine/checkout (no hardcoded home path). Package.swift lives in `swift/`, so the engine's
// build output is at `../target/debug`.
let rustLibDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../target/debug")
    .standardizedFileURL
    .path

let package = Package(
    name: "Browser",
    platforms: [
        .macOS(.v13)
    ],
    targets: [
        .systemLibrary(
            name: "CBrowser",
            path: "Sources/CBrowser"
        ),
        .executableTarget(
            name: "Browser",
            dependencies: ["CBrowser"],
            linkerSettings: [
                .unsafeFlags([
                    "-L", rustLibDir,
                    "-lbrowser_ffi",
                ]),
                .linkedFramework("AppKit"),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
    ]
)
