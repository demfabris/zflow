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
    .executable(name: "zflow-awdl-client", targets: ["AWDLClient"]),
    .executable(name: "zflow-awdl-daemon", targets: ["AWDLDaemon"]),
  ],
  targets: [
    .testTarget(name: "ZflowAppTests", dependencies: ["ZflowApp"]),
    .target(name: "ZflowCore", publicHeadersPath: "include"),
    .target(name: "AWDLGuardian", publicHeadersPath: "include"),
    .executableTarget(
      name: "ZflowApp", dependencies: ["ZflowCore"],
      linkerSettings: [
        .unsafeFlags(["-L", rustLibrary, "-lzflow"]),
        .linkedFramework("ApplicationServices"), .linkedFramework("CoreFoundation"),
        .linkedFramework("Security"),
      ]),
    .executableTarget(name: "AWDLClient"),
    .executableTarget(name: "AWDLDaemon", dependencies: ["AWDLGuardian"]),
  ]
)
