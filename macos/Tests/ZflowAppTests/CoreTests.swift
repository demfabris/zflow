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
  #expect(initial.status.state == .paused)
  let changed = try await core.request(CoreRequest(command: "set_awdl", enabled: true))
  #expect(changed.platform.blockAwdl)
  #expect(
    try String(contentsOf: file, encoding: .utf8)
      == original.replacingOccurrences(of: "block_awdl = false", with: "block_awdl = true"))
  try "[broken".write(to: file, atomically: true, encoding: .utf8)
  let invalid = try await core.request(CoreRequest(command: "reload"))
  #expect(invalid.platform.blockAwdl)
  #expect(invalid.health.contains { $0.id == "config" && $0.level == .error })
  #expect(invalid.status.state == .paused)
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
  #expect(initial.sharing == true)
  try "[broken".write(to: file, atomically: true, encoding: .utf8)
  do {
    _ = try await core.request(CoreRequest(command: "set_sharing", enabled: false))
    Issue.record("Saving over an external edit should fail")
  } catch {}
  let paused = try await core.request(CoreRequest(command: "snapshot"))
  #expect(paused.sharing == false)
  #expect(paused.status.state == .paused)
  await core.shutdown()
}

@Test func requestsAfterShutdownDoNotRestartTheEngine() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  try "[transport]\ndiscovery = false\n".write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  _ = try await core.request(CoreRequest(command: "snapshot"))
  await core.shutdown()
  do {
    _ = try await core.request(CoreRequest(command: "snapshot"))
    Issue.record("A request after shutdown must not recreate the engine")
  } catch {}
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
  #expect(snapshot.platform.blockAwdl)
  #expect(snapshot.status.state == .paused)
  await core.shutdown()
}

@Test func theSnapshotCarriesTheSharedRowsAndRequestNames() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  try "[transport]\ndiscovery = false\n".write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  let snapshot = try await core.request(CoreRequest(command: "snapshot"))
  #expect(snapshot.status.state == .setup)
  #expect(snapshot.status.title == "Pair a computer")
  #expect(snapshot.health.first?.id == "sharing")
  #expect(snapshot.layout != nil)
  #expect(snapshot.peers.isEmpty)
  #expect(snapshot.configPath == file.path)
  // The old names are gone; the shared ones reach the engine.
  for old in ["move", "pair_start", "pair", "pair_cancel"] {
    await #expect(throws: AppError.self) { try await core.request(CoreRequest(command: old)) }
  }
  do {
    _ = try await core.request(
      CoreRequest(command: "move_tile", id: "nowhere", x: 0, y: 0, tolerance: 8))
    Issue.record("An unknown tile cannot move")
  } catch { #expect(error.localizedDescription == "Unknown computer") }
  // A computer added by address waits on the shelf until its hello comes back.
  let added = try await core.request(CoreRequest(command: "add_address", address: "100.64.0.7"))
  let tile = try #require(added.unplaced.first)
  #expect(tile.state == .identifying && tile.via == .address && !tile.placeable)
  do {
    _ = try await core.request(
      CoreRequest(command: "place", id: tile.id, x: 0, y: 0, tolerance: 8))
    Issue.record("A computer still being identified cannot be placed")
  } catch {
    #expect(error.localizedDescription == "100.64.0.7:43119 is still being identified")
  }
  #expect(added.pairingWindow.state == .never)
  #expect(added.ownMark == nil || added.ownMark?.count == 6)
  #expect(added.notices.isEmpty)
  await core.shutdown()
}

@Test func eachComputersSettingsReachTheEngine() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  try
    "[transport]\ndiscovery = false\n[macos]\nsharing = false\n[peers.desk]\nspki_der_hex = \"01\"\n"
    .write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  let initial = try #require(try await core.request(CoreRequest(command: "snapshot")).peers.first)
  #expect(!initial.allowControl && initial.keyboard == .standard && !initial.reverseScroll)
  // Two-word fields go out in snake case, one at a time.
  _ = try await core.request(CoreRequest(command: "set_peer", name: "desk", allowControl: true))
  _ = try await core.request(
    CoreRequest(command: "set_peer", name: "desk", keyboard: .pcPositions))
  let changed = try await core.request(
    CoreRequest(command: "set_peer", name: "desk", reverseScroll: true))
  let desk = try #require(changed.peers.first)
  #expect(desk.allowControl && desk.keyboard == .pcPositions && desk.reverseScroll)
  let saved = try String(contentsOf: file, encoding: .utf8)
  #expect(saved.contains("keyboard = \"pc_positions\"") && saved.contains("reverse_scroll = true"))
  await core.shutdown()
}

@Test func theClipboardAndPauseSwitchesReachTheEngine() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  try "[transport]\ndiscovery = false\n[macos]\nsharing = false\n".write(
    to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  let initial = try await core.request(CoreRequest(command: "snapshot"))
  #expect(initial.shareClipboard == false)
  let changed = try await core.request(CoreRequest(command: "set_clipboard", share: true))
  #expect(changed.shareClipboard == true)
  #expect(try String(contentsOf: file, encoding: .utf8).contains("[clipboard]\nshare = true"))
  #expect(initial.pauseAtEdges == false)
  let paused = try await core.request(CoreRequest(command: "set_switching", pauseAtEdges: true))
  #expect(paused.pauseAtEdges == true)
  #expect(
    try String(contentsOf: file, encoding: .utf8).contains("[switching]\npause_at_edges = true"))
  await core.shutdown()
}

@Test func nothingIsSentToTheNetworkBeforeTheWindowAsks() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  // Discovery stays on; only the missing computer and the window hold it back.
  try "[macos]\nsharing = false\n".write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  _ = try await core.request(CoreRequest(command: "check_accessibility"))
  // Let the worker's first maintenance tick run.
  try await Task.sleep(for: .milliseconds(200))
  let snapshot = try await core.request(CoreRequest(command: "snapshot"))
  #expect(snapshot.platform.localNetwork == .unknown)
  #expect(snapshot.unplaced.isEmpty)
  await core.shutdown()
}
