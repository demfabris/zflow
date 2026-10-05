// swift-tools-version: 6.2

import Foundation
import PackageDescription

let rustLibrary =
  ProcessInfo.processInfo.environment["ZFLOW_RUST_LIB_DIR"]
  ?? URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
  .appendingPathComponent("target/debug").path
let package = Package(
  name: "Zflow",
  platforms: [.macOS(.v26)],
  products: [
    .executable(name: "zflow-app", targets: ["ZflowApp"]),
    .executable(name: "zflow-awdl-daemon", targets: ["AWDLDaemon"]),
  ],
  dependencies: [
    .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.0")
  ],
  targets: [
    .testTarget(name: "ZflowAppTests", dependencies: ["ZflowApp"]),
    .target(name: "ZflowCore", publicHeadersPath: "include"),
    .target(name: "AWDLGuardian", publicHeadersPath: "include"),
    .executableTarget(
      name: "ZflowApp",
      dependencies: ["ZflowCore", .product(name: "Sparkle", package: "Sparkle")],
      linkerSettings: [
        .unsafeFlags(["-L", rustLibrary, "-lzflow"]),
        .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"]),
        .linkedFramework("AppKit"), .linkedFramework("ApplicationServices"),
        .linkedFramework("Carbon"), .linkedFramework("CoreFoundation"),
        .linkedFramework("ImageIO"), .linkedFramework("IOKit"), .linkedFramework("Security"),
      ]),
    .executableTarget(name: "AWDLDaemon", dependencies: ["AWDLGuardian"]),
  ]
)
