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
  var error: String?
}
struct Nearby: Decodable, Identifiable, Sendable {
  var instance: String
  var addresses: [String]
  var compatible: Bool
  var id: String { instance }
}
struct Snapshot: Decodable, Sendable {
  var configPath: String
  var layoutPath: String
  var status: String
  var sharing: Bool
  var blockAwdl: Bool
  var accessibility: Bool
  var notice: String
  var peers: [String]
  var layout: Layout
  var pairing: Pairing
  var nearby: [Nearby]
  var configError: String?
  var layoutError: String?
  var receiverError: String?
  var receiverChecked: Bool
  var checking: Bool
  var discoveryError: String?
  var desktopError: String?

  var title: String {
    switch status {
    case "ready": "Ready"
    case "sharing": "Sharing"
    case "paused": "Paused"
    case "checking": "Checking…"
    case "setup": "Pair a computer"
    default: "Needs attention"
    }
  }
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
  init(path: String) { self.path = path }
  func request(_ request: CoreRequest) throws -> Snapshot {
    if handle == nil { handle = try CoreHandle(path: path) }
    let data = try JSONEncoder().encode(request)
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
  func shutdown() { handle = nil }
}
