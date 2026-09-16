import Foundation
import Testing

@testable import ZflowApp

@Test func bridgePersistsSettingsAndRetainsValidConfig() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  let original =
    "# My Mac\n[transport]\ndiscovery = false\n[macos]\nsharing = false\nblock_awdl = false # keep this\n"
  try original.write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  let initial = try await core.request(CoreRequest(command: "snapshot"))
  #expect(initial.status == "paused")
  let changed = try await core.request(CoreRequest(command: "set_awdl", enabled: true))
  #expect(changed.blockAwdl)
  #expect(
    try String(contentsOf: file, encoding: .utf8)
      == original.replacingOccurrences(of: "block_awdl = false", with: "block_awdl = true"))
  try "[broken".write(to: file, atomically: true, encoding: .utf8)
  let invalid = try await core.request(CoreRequest(command: "reload"))
  #expect(invalid.blockAwdl)
  #expect(invalid.configError != nil)
  await core.shutdown()
}

@Test func pauseStillWorksWhenConfigCannotBeSaved() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  try "[transport]\ndiscovery = false\n".write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  let initial = try await core.request(CoreRequest(command: "snapshot"))
  #expect(initial.sharing)
  try "[broken".write(to: file, atomically: true, encoding: .utf8)
  do {
    _ = try await core.request(CoreRequest(command: "set_sharing", enabled: false))
    Issue.record("Saving over an external edit should fail")
  } catch {}
  let paused = try await core.request(CoreRequest(command: "snapshot"))
  #expect(!paused.sharing)
  #expect(paused.status == "paused")
  await core.shutdown()
}

@Test func workerReloadsSettingsWithoutWindowPolling() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  let original = "[transport]\ndiscovery = false\n[macos]\nsharing = false\nblock_awdl = false\n"
  try original.write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  _ = try await core.request(CoreRequest(command: "snapshot"))
  try original.replacingOccurrences(of: "block_awdl = false", with: "block_awdl = true").write(
    to: file, atomically: true, encoding: .utf8)
  // No UI requests occur while the worker observes the edit.
  try await Task.sleep(for: .milliseconds(2200))
  let snapshot = try await core.request(CoreRequest(command: "snapshot"))
  #expect(snapshot.blockAwdl)
  #expect(snapshot.status == "paused")
  await core.shutdown()
}
