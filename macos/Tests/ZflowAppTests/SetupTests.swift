import Foundation
import Testing

@testable import ZflowApp

private func location(_ path: String, quarantined: Bool = false, readOnly: Bool = false)
  -> AppLocation
{
  AppLocation(path: path, quarantined: quarantined, readOnly: readOnly, home: "/Users/me")
}

@Test func localBuildsAndInstalledCopiesStayPut() {
  for path in [
    "/Users/me/dev/zflow/target/debug/zflow.app",
    "/Users/me/dev/zflow/target/release/zflow.app",
    "/Volumes/Work/zflow/target/debug/zflow.app",
    "/Users/me/Downloads/zflow.app",
  ] {
    #expect(!location(path).needsMove, "\(path)")
  }
  for path in ["/Applications/zflow.app", "/Users/me/Applications/zflow.app"] {
    #expect(!location(path, quarantined: true).needsMove, "\(path)")
  }
}

@Test func diskImagesTranslocationAndQuarantinedCopiesMove() {
  #expect(location("/Volumes/zflow/zflow.app", readOnly: true).needsMove)
  #expect(
    location("/private/var/folders/xy/abc/T/AppTranslocation/1234-ABCD/d/zflow.app", readOnly: true)
      .needsMove)
  #expect(location("/Users/me/Downloads/zflow.app", quarantined: true).needsMove)
  #expect(location("/Applications Old/zflow.app", quarantined: true).needsMove)
}

@Test func setupStartsAtTheFirstUnfinishedStep() {
  #expect(SetupFacts(needsMove: true).start == .move)
  #expect(SetupFacts().start == .accessibility)
  #expect(SetupFacts(accessibility: true).start == .localNetwork)
  #expect(SetupFacts(accessibility: true, localNetwork: true).start == .computers)
  #expect(SetupFacts(accessibility: true, localNetwork: true, peers: 1).start == .tryIt)
  // Already paired, but Accessibility was removed since.
  #expect(SetupFacts(localNetwork: true, peers: 1).start == .accessibility)
}

@Test func stepsMoveOnOnlyWhenTheirConditionTurnsTrue() {
  let waiting = SetupFacts()
  let granted = SetupFacts(accessibility: true)
  #expect(granted.advance(.accessibility, from: waiting) == .localNetwork)
  // Going back to a finished step stays there until the user continues.
  #expect(granted.advance(.accessibility, from: granted) == .accessibility)
  // Finished later steps are skipped.
  let allowed = SetupFacts(accessibility: true, localNetwork: true)
  #expect(allowed.advance(.accessibility, from: SetupFacts(localNetwork: true)) == .computers)
  #expect(allowed.advance(.localNetwork, from: granted) == .computers)
  // Another step's change leaves the showing step alone.
  #expect(allowed.advance(.computers, from: granted) == .computers)
}

@Test func pairingMovesOnWhenAnotherComputerIsAdded() {
  let one = SetupFacts(accessibility: true, localNetwork: true, peers: 1)
  let two = SetupFacts(accessibility: true, localNetwork: true, peers: 2)
  #expect(
    one.advance(.computers, from: SetupFacts(accessibility: true, localNetwork: true)) == .tryIt)
  #expect(two.advance(.computers, from: one) == .tryIt)
  #expect(one.advance(.computers, from: one) == .computers)
  #expect(one.advance(.computers, from: two) == .computers)
}

@Test func tryItNamesTheEdgeThatLeadsToTheComputer() throws {
  var snapshot = try JSONDecoder.snake.decode(Snapshot.self, from: Data(minimalSnapshot.utf8))
  let mac = Computer(id: "local", label: "This Mac", x: 0, y: 0, width: 1512, height: 982)
  func place(_ x: Int, _ y: Int) -> String? {
    snapshot.layout.monitors = [
      mac,
      Computer(
        id: "peer:ubuntu", label: "ubuntu", peer: "ubuntu", x: x, y: y, width: 1920, height: 1080),
    ]
    return snapshot.edge(toward: "ubuntu")
  }
  #expect(place(1512, 0) == "right")
  #expect(place(-1920, -40) == "left")
  #expect(place(0, -1080) == "top")
  #expect(place(-200, 982) == "bottom")
}

@Test func nothingIsSentToTheNetworkBeforeSetupAsks() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  let file = directory.appendingPathComponent("zflow.toml")
  // Discovery stays on; only the missing computer and setup hold it back.
  try "[macos]\nsharing = false\n".write(to: file, atomically: true, encoding: .utf8)
  let core = CoreBridge(path: file.path)
  _ = try await core.request(CoreRequest(command: "check_accessibility"))
  // Let the worker's first maintenance tick run.
  try await Task.sleep(for: .milliseconds(200))
  let snapshot = try await core.request(CoreRequest(command: "snapshot"))
  #expect(snapshot.localNetwork == "unknown")
  #expect(snapshot.nearby.isEmpty)
  await core.shutdown()
}

extension JSONDecoder {
  fileprivate static var snake: JSONDecoder {
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    return decoder
  }
}

private let minimalSnapshot = """
  {"config_path":"","layout_path":"","status":"ready","sharing":true,"block_awdl":false,
  "accessibility":true,"notice":"","peers":["ubuntu"],"layout":{"monitors":[]},
  "pairing":{"state":"idle"},"nearby":[],"receiver_checked":true,"checking":false,
  "local_network":"allowed"}
  """
