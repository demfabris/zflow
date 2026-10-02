import Foundation
import Testing

@testable import ZflowApp

/// A snapshot as the engine sends it, with `peers` as name to state.
private func snapshot(
  peers: [(String, String)] = [], pairing: String = "idle", blockAwdl: Bool = false,
  health: String = ""
) throws -> Snapshot {
  let rows = peers.map { name, state in
    """
    {"name":"\(name)","state":"\(state)","detail":"","allow_control":true,
    "keyboard":"standard","reverse_scroll":false}
    """
  }
  let monitors = peers.map { name, _ in
    """
    {"id":"peer:\(name)","label":"\(name)","peer":"\(name)","x":1512,"y":0,"width":1920,"height":1080}
    """
  }
  let json = """
    {"status":{"state":"ready","peer":null,"title":"Ready"},"sharing":false,"health":[\(health)],
    "layout":{"monitors":[{"id":"local","label":"This Mac","x":0,"y":0,"width":1512,"height":982}
    \(monitors.map { "," + $0 }.joined())]},
    "peers":[\(rows.joined(separator: ","))],"pairing":{"state":"\(pairing)"},"nearby":[],
    "pause_at_edges":false,"shortcuts":[],"share_clipboard":false,"autostart":null,"config_path":"",
    "platform":{"accessibility":true,"local_network":"allowed","block_awdl":\(blockAwdl)}}
    """
  let decoder = JSONDecoder()
  decoder.keyDecodingStrategy = .convertFromSnakeCase
  return try decoder.decode(Snapshot.self, from: Data(json.utf8))
}

/// A model on its own configuration, which reaches the engine only when a test asks.
@MainActor
private func model(_ directory: URL) throws -> AppModel {
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  let file = directory.appendingPathComponent("zflow.toml")
  try "[transport]\ndiscovery = false\n[macos]\nsharing = false\n".write(
    to: file, atomically: true, encoding: .utf8)
  return AppModel(arguments: ["zflow", "--config", file.path], polling: false)
}

private func temporary() -> URL {
  FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
}

@Test func everyStateTheEngineNamesDecodes() throws {
  func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
    try JSONDecoder().decode(type, from: Data(json.utf8))
  }
  // src/app/api.rs, src/app/pairing.rs, src/core/keymap.rs and src/macos/local_network.rs.
  _ = try decode(
    [Status.State].self,
    #"["ready","controlling","controlled","paused","checking","setup","attention"]"#)
  #expect(
    try decode(
      [Peer.State].self,
      #"["paired","connecting","connected","controlling_this","controlled_from_here","unreachable"]"#
    )
      == [.paired, .connecting, .connected, .controllingThis, .controlledFromHere, .unreachable])
  _ = try decode([Health.Level].self, #"["ok","warning","error"]"#)
  _ = try decode(
    [Pairing.State].self,
    #"["idle","listening","confirm","connecting","approving","paired","failed"]"#)
  #expect(
    try decode([KeyboardMode].self, #"["standard","pc_positions","mac"]"#) == KeyboardMode.allCases)
  _ = try decode([MacPlatform.LocalNetwork].self, #"["allowed","blocked","unknown"]"#)
  #expect(throws: DecodingError.self) { try decode([Peer.State].self, #"["asleep"]"#) }
}

@Test func onlyRowsThatAreNotFineBecomeBanners() throws {
  let rows = """
    {"id":"sharing","level":"ok","title":"Sharing","detail":"","action":null},
    {"id":"computers","level":"error","title":"Paired computers","detail":"desk: down",
    "action":{"label":"Retry","command":"retry"}}
    """
  #expect(try snapshot(health: rows).problems.map(\.id) == ["computers"])
}

@MainActor @Test func theClipboardRowAsksForPasteAccessUntilItIsAllowed() throws {
  let directory = temporary()
  defer { try? FileManager.default.removeItem(at: directory) }
  let model = try model(directory)
  let clipboard = Health(
    id: "clipboard", level: .warning, title: "Clipboard", detail: "", action: nil)
  model.pasteAccess = .alwaysDeny
  #expect(model.fix(for: clipboard)?.command == "allow_paste")
  // Then the row is about something else, such as a clip too large.
  model.pasteAccess = .alwaysAllow
  #expect(model.fix(for: clipboard) == nil)
  // A row the engine gave a fix keeps it.
  let retry = HealthAction(label: "Retry", command: "retry")
  let computers = Health(id: "computers", level: .error, title: "", detail: "", action: retry)
  #expect(model.fix(for: computers) == retry)
}

@MainActor @Test func forgettingTheShownComputerGoesBackToComputers() async throws {
  let directory = temporary()
  defer { try? FileManager.default.removeItem(at: directory) }
  let model = try model(directory)
  model.apply(try snapshot(peers: [("desk", "connected"), ("xps", "unreachable")]))
  model.page = .computer("desk")
  model.apply(try snapshot(peers: [("desk", "connected")]))
  #expect(model.page == .computer("desk"))
  model.apply(try snapshot(peers: [("xps", "unreachable")]))
  #expect(model.page == .computers)
  model.page = .settings
  model.apply(try snapshot())
  #expect(model.page == .settings)
  await model.shutdown()
}

@MainActor @Test func finishedPairingClosesTheSheet() async throws {
  let directory = temporary()
  defer { try? FileManager.default.removeItem(at: directory) }
  let model = try model(directory)
  model.pairing = PairingTarget(address: "192.0.2.7")
  model.apply(try snapshot(pairing: "approving"))
  #expect(model.pairing != nil)
  model.apply(try snapshot(peers: [("desk", "paired")], pairing: "paired"))
  #expect(model.pairing == nil)
  await model.shutdown()
}

@MainActor @Test func theHelperIsCheckedOnlyWhileReduceWiFiLagIsOn() async throws {
  let directory = temporary()
  defer { try? FileManager.default.removeItem(at: directory) }
  let model = try model(directory)
  let unchecked = model.services.helperNote
  model.apply(try snapshot(blockAwdl: false))
  await model.checkServices()
  #expect(model.services.helperNote == unchecked)
  model.apply(try snapshot(blockAwdl: true))
  await model.checkServices()
  #expect(model.services.helperNote != unchecked)
  await model.shutdown()
}

@Test func tilesCarryEachPairedComputersState() throws {
  let snapshot = try snapshot(peers: [("desk", "connected"), ("xps", "unreachable")])
  let tiles = LayoutTile.tiles(try #require(snapshot.layout), peers: snapshot.peers)
  #expect(tiles.map(\.id) == ["local", "peer:desk", "peer:xps"])
  #expect(tiles.map(\.state) == [nil, .connected, .unreachable])
  #expect(tiles.allSatisfy { $0.placed })
}
