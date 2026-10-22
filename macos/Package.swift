// swift-tools-version: 6.2

import PackageDescription

// The privileged helper that keeps AirDrop's radio down while input is
// shared. The app itself is Rust (crates/zflow-app).
let package = Package(
  name: "Zflow",
  platforms: [.macOS(.v26)],
  products: [
    .executable(name: "zflow-awdl-daemon", targets: ["AWDLDaemon"])
  ],
  targets: [
    .target(name: "AWDLGuardian", publicHeadersPath: "include"),
    .executableTarget(name: "AWDLDaemon", dependencies: ["AWDLGuardian"]),
  ]
)
