import Foundation
import ZflowCore

struct Computer: Codable, Identifiable, Equatable, Sendable {
  var id: String
  var label: String
  var peer: String?
  var x: Int
  var y: Int
  var width: Int
  var height: Int
}
struct Layout: Decodable, Sendable { var monitors: [Computer] }
struct Pairing: Decodable, Sendable {
  var state: String
  var code: String?
  var name: String?
  var address: String?
  var error: String?
}
struct Nearby: Decodable, Identifiable, Sendable {
  var instance: String
  var addresses: [String]
  var compatible: Bool
  /// Where its pairing listener waits; computers advertise their input port.
  var pairAddress: String?
  var id: String { instance }
}
/// src/app/api.rs Snapshot, which the GNOME settings window shows too. The
/// shared rows come first, in window order, then the ones for this Mac.
struct Snapshot: Decodable, Sendable {
  var status: Status
  /// Nil while the part that shares input cannot be reached.
  var sharing: Bool?
  var health: [Health]
  var layout: Layout?
  var peers: [Peer]
  var pairing: Pairing
  var nearby: [Nearby]
  var shortcuts: [Shortcut]
  /// Nil here: the app keeps the login item itself.
  var autostart: Bool?
  var configPath: String
  var platform: MacPlatform

  var peerNames: [String] { peers.map(\.name) }
}
struct Status: Decodable, Sendable {
  /// ready, controlling, controlled, paused, checking, setup or attention.
  var state: String
  var peer: String?
  var title: String
}
struct Peer: Decodable, Identifiable, Sendable {
  var name: String
  /// paired, connecting, connected, controlling_this, controlled_from_here or unreachable.
  var state: String
  var detail: String
  /// Whether it may control this Mac.
  var allowControl: Bool
  /// standard, pc_positions or mac: how its keys act on this Mac.
  var keyboard: String
  /// Whether its scrolling is turned around on this Mac.
  var reverseScroll: Bool
  var id: String { name }
}
struct Health: Decodable, Identifiable, Sendable {
  var id: String
  /// ok, warning or error.
  var level: String
  var title: String
  var detail: String
  var action: HealthAction?
}
struct HealthAction: Decodable, Sendable {
  var label: String
  var command: String
}
struct Shortcut: Decodable, Sendable {
  var title: String
  var keys: String
}
struct MacPlatform: Decodable, Sendable {
  var accessibility: Bool
  /// "allowed", "blocked", or "unknown" until setup asks to find computers.
  var localNetwork: String
  var blockAwdl: Bool
}
struct CoreRequest: Encodable, Sendable {
  var command: String
  var enabled: Bool?
  var ready: Bool?
  var id: String?
  var x: Int?
  var y: Int?
  var tolerance: Int?
  var address: String?
  var name: String?
  var code: String?
  var allow: Bool?
  var allowControl: Bool?
  var keyboard: String?
  var reverseScroll: Bool?
}
struct CoreResponse: Decodable {
  var snapshot: Snapshot?
  var error: String?
}
struct AppError: LocalizedError {
  let message: String
  var errorDescription: String? { message }
}

// The actor serializes every FFI call. The handle owns its Rust worker and joins it on release.
private final class CoreHandle: @unchecked Sendable {
  let pointer: OpaquePointer
  init(path: String) throws {
    var error: UnsafeMutablePointer<CChar>?
    let pointer = path.withCString { zflow_app_create($0, &error) }
    defer { zflow_string_free(error) }
    guard let pointer else {
      throw AppError(message: error.map { String(cString: $0) } ?? "Could not start sharing")
    }
    self.pointer = pointer
  }
  deinit { zflow_app_destroy(pointer) }
}
actor CoreBridge {
  let path: String
  private var handle: CoreHandle?
  // A request queued behind shutdown must not start a new engine during quit.
  private var closed = false
  init(path: String) { self.path = path }
  func request(_ request: CoreRequest) throws -> Snapshot {
    guard !closed else { throw AppError(message: "The sharing engine has stopped") }
    if handle == nil { handle = try CoreHandle(path: path) }
    let encoder = JSONEncoder()
    encoder.keyEncodingStrategy = .convertToSnakeCase
    let data = try encoder.encode(request)
    let text = String(decoding: data, as: UTF8.self)
    let response = text.withCString { zflow_app_request(handle!.pointer, $0) }
    guard let response else { throw AppError(message: "The sharing engine returned no response") }
    defer { zflow_string_free(response) }
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    let result = try decoder.decode(CoreResponse.self, from: Data(String(cString: response).utf8))
    if let error = result.error { throw AppError(message: error) }
    guard let snapshot = result.snapshot else {
      throw AppError(message: "The sharing engine returned no status")
    }
    return snapshot
  }
  func shutdown() {
    closed = true
    handle = nil
  }
}
