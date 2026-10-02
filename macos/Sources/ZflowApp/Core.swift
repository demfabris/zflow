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
struct Layout: Decodable, Equatable, Sendable { var monitors: [Computer] }
struct Pairing: Decodable, Equatable, Sendable {
  var state: State
  var code: String?
  var name: String?
  var address: String?
  var error: String?

  enum State: String, Decodable, Sendable {
    case idle, listening, confirm, connecting, approving, paired, failed
  }
}
struct Nearby: Decodable, Identifiable, Equatable, Sendable {
  var instance: String
  var addresses: [String]
  var compatible: Bool
  /// Where its pairing listener waits; computers advertise their input port.
  var pairAddress: String?
  var id: String { instance }
  /// Discovery names nobody, so people know a computer by its address.
  @MainActor var host: String {
    (pairAddress ?? addresses.first).map { PairingView.host($0) } ?? "Unknown address"
  }
}
/// src/app/api.rs Snapshot, which the GNOME settings window shows too. The
/// shared rows come first, in window order, then the ones for this Mac.
struct Snapshot: Decodable, Equatable, Sendable {
  var status: Status
  /// Nil while the part that shares input cannot be reached.
  var sharing: Bool?
  var health: [Health]
  var layout: Layout?
  var peers: [Peer]
  var pairing: Pairing
  var nearby: [Nearby]
  /// Whether the pointer rests against an edge for a moment before it crosses.
  var pauseAtEdges: Bool?
  /// Whether the clipboard goes along with the pointer.
  var shareClipboard: Bool?
  var configPath: String
  var platform: MacPlatform

  /// The rows the window shows as banners.
  var problems: [Health] { health.filter { $0.level != .ok } }
}
struct Status: Decodable, Equatable, Sendable {
  var state: State
  var peer: String?
  var title: String

  enum State: String, Decodable, Sendable {
    case ready, controlling, controlled, paused, checking, setup, attention
  }
}
struct Peer: Decodable, Identifiable, Equatable, Sendable {
  var name: String
  var state: State
  /// What its row says; an unreachable computer's error.
  var detail: String
  /// Whether it may control this Mac.
  var allowControl: Bool
  /// How its keys act on this Mac.
  var keyboard: KeyboardMode
  /// Whether its scrolling is turned around on this Mac.
  var reverseScroll: Bool
  var id: String { name }

  enum State: String, Decodable, Sendable {
    case paired, connecting, connected, unreachable
    case controllingThis = "controlling_this"
    case controlledFromHere = "controlled_from_here"
  }
}
/// src/core/keymap.rs KeyboardMode, in menu order.
enum KeyboardMode: String, Codable, CaseIterable, Sendable {
  case standard
  case pcPositions = "pc_positions"
  case mac
}
struct Health: Decodable, Identifiable, Equatable, Sendable {
  var id: String
  var level: Level
  var title: String
  var detail: String
  var action: HealthAction?

  enum Level: String, Decodable, Sendable { case ok, warning, error }
}
struct HealthAction: Decodable, Equatable, Sendable {
  var label: String
  var command: String
}
struct MacPlatform: Decodable, Equatable, Sendable {
  var accessibility: Bool
  var localNetwork: LocalNetwork
  var blockAwdl: Bool

  /// Unknown until zflow first looks for computers, which is when macOS asks.
  enum LocalNetwork: String, Decodable, Sendable { case allowed, blocked, unknown }
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
  var keyboard: KeyboardMode?
  var reverseScroll: Bool?
  var pauseAtEdges: Bool?
  var share: Bool?
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
