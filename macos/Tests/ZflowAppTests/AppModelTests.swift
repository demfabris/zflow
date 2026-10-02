import Foundation
import Testing

@testable import ZflowApp

/// A snapshot as the engine sends it, with `peers` as name to state.
private func snapshot(
  peers: [(String, String)] = [], pairing: String = "idle", blockAwdl: Bool = false,
  health: String = "", window: String = #"{"state":"never"}"#, unplaced: String = "",
  notices: [UInt64] = []
) throws -> Snapshot {
  let rows = peers.map { name, state in
    """
    {"name":"\(name)","state":"\(state)","detail":"","allow_control":true,
    "keyboard":"standard","reverse_scroll":false,"mark":"0123ab"}
    """
  }
  let posts = notices.map { #"{"id":\#($0),"kind":"joined","name":"desk"}"# }
  let monitors = peers.map { name, _ in
    """
    {"id":"peer:\(name)","label":"\(name)","peer":"\(name)","x":1512,"y":0,"width":1920,"height":1080}
    """
  }
  let json = """
    {"status":{"state":"ready","peer":null,"title":"Ready"},"sharing":false,"health":[\(health)],
    "pairing_window":\(window),
    "layout":{"monitors":[{"id":"local","label":"This Mac","x":0,"y":0,"width":1512,"height":982}
    \(monitors.map { "," + $0 }.joined())]},"own_mark":"fedcba","unplaced":[\(unplaced)],
    "peers":[\(rows.joined(separator: ","))],"pairing":{"state":"\(pairing)"},"nearby":[],
    "pause_at_edges":false,"shortcuts":[],"share_clipboard":false,"autostart":null,"config_path":"",
    "notices":[\(posts.joined(separator: ","))],
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
  // src/neighbors.rs, src/pairing_window.rs and src/hello.rs.
  _ = try decode(
    [Unplaced.State].self, #"["identifying","ready","different_version","duplicate_name"]"#)
  _ = try decode([Unplaced.Via].self, #"["mdns","address"]"#)
  _ = try decode([Os].self, #"["linux","macos"]"#)
  _ = try decode([PairingWindow.State].self, #"["never","eligible","open","closed"]"#)
  _ = try decode(
    [PairingWindow.Reason].self, #"["had_peers","accepted","expired","rival","restarted"]"#)
  _ = try decode([Notice.Kind].self, #"["joined"]"#)
  #expect(throws: DecodingError.self) { try decode([Peer.State].self, #"["asleep"]"#) }
}

@Test func theOtherComputerCanFinishWhileTheWindowIsOpen() throws {
  let ubuntu = """
    {"id":"key:ab","name":"ubuntu","os":"linux","mark":"a1b2c3","version":"0.3.0",
    "state":"ready","trusts_you":false,"via":"mdns"}
    """
  let open = #"{"state":"open","seconds_left":521,"holding":null,"reason":null}"#
  let note = try #require(try snapshot(window: open, unplaced: ubuntu).finishNote)
  #expect(note.title == "Or finish from ubuntu")
  #expect(note.detail.contains("in zflow on ubuntu") && note.detail.contains("next 9 minutes"))
  let holding =
    #"{"state":"open","seconds_left":500,"holding":{"name":"ubuntu","mark":"a1b2c3","ms_left":2000}}"#
  #expect(try snapshot(window: holding).finishNote?.title == "Adding ubuntu…")
  let rival = #"{"state":"closed","reason":"rival"}"#
  #expect(try snapshot(window: rival).finishNote?.title == "Drag the one you want")
  for done in [#"{"state":"closed","reason":"expired"}"#, #"{"state":"never"}"#] {
    #expect(try snapshot(window: done).finishNote == nil)
  }
  let shelf = try snapshot(unplaced: ubuntu).unplaced
  #expect(shelf.map(\.placeable) == [true])
  #expect(shelf.first?.detail == "Linux")
}

@Test func marksPickFourOfEightColorsFromTheirFirstDigits() {
  #expect(KeyMark.indices("a1b2c3") == [2, 1, 3, 2])
  #expect(KeyMark.indices("0123ff") == [0, 1, 2, 3])
  #expect(KeyMark.indices("ffff00") == [7, 7, 7, 7])
  #expect(KeyMark.palette.count == 8)
}

@Test func aDroppedComputerIsCenteredOnThePointer() {
  // At a tenth of the layout's size, with the layout's origin at (100, 50).
  let origin = CGPoint(x: 100, y: 50)
  let center = CGPoint(x: 100 + 960 * 0.1, y: 50 + 540 * 0.1)
  #expect(ComputerLayout.corner(at: center, scale: 0.1, origin: origin) == (0, 0))
  let right = CGPoint(x: center.x + 192, y: center.y)
  #expect(ComputerLayout.corner(at: right, scale: 0.1, origin: origin) == (1920, 0))
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

@Test func tilesCarryEachPairedComputersStateAndMark() throws {
  let snapshot = try snapshot(peers: [("desk", "connected"), ("xps", "unreachable")])
  let tiles = LayoutTile.tiles(
    try #require(snapshot.layout), peers: snapshot.peers, ownMark: snapshot.ownMark)
  #expect(tiles.map(\.id) == ["local", "peer:desk", "peer:xps"])
  #expect(tiles.map(\.state) == [nil, .connected, .unreachable])
  #expect(tiles.map(\.mark) == ["fedcba", "0123ab", "0123ab"])
  #expect(tiles.allSatisfy { $0.placed })
}
