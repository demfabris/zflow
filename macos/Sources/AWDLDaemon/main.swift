import AWDLGuardian
import Darwin
import Dispatch
import Foundation
import XPC

// Only one guardian may own awdl0. Its dedicated worker remains alive through client exits.
final class Guardian: @unchecked Sendable {
  let workers = DispatchGroup()
  private let lock = NSLock()
  private var leased = false
  func acquire() -> Bool {
    lock.withLock {
      if leased { return false }
      leased = true
      return true
    }
  }
  func release() { lock.withLock { leased = false } }
}

final class Peer: XPCPeerHandler, @unchecked Sendable {
  typealias Input = XPCDictionary
  typealias Output = XPCDictionary
  let guardian: Guardian
  init(_ guardian: Guardian) { self.guardian = guardian }
  func handleIncomingRequest(_ request: XPCDictionary) -> XPCDictionary? {
    var reply = XPCDictionary()
    let command: String? = request["command"]
    guard zflow_awdl_check() == 0 else {
      reply["error"] = "AWDL is unavailable"
      return reply
    }
    if command == "check" {
      reply["ready"] = true
      return reply
    }
    guard command == "lease" else {
      reply["error"] = "Unknown request"
      return reply
    }
    guard guardian.acquire() else {
      reply["error"] = "AWDL is already in use"
      return reply
    }
    var input = [Int32](repeating: -1, count: 2)
    var output = [Int32](repeating: -1, count: 2)
    guard pipe(&input) == 0, pipe(&output) == 0 else {
      for fd in input + output where fd >= 0 { close(fd) }
      guardian.release()
      reply["error"] = "Could not open AWDL lease"
      return reply
    }
    reply.withUnsafeUnderlyingDictionary {
      xpc_dictionary_set_fd($0, "input", input[1])
      xpc_dictionary_set_fd($0, "output", output[0])
    }
    close(input[1])
    close(output[0])
    let readFD = input[0]
    let writeFD = output[1]
    let guardian = guardian
    guardian.workers.enter()
    DispatchQueue.global(qos: .userInitiated).async {
      _ = zflow_awdl_run(readFD, writeFD)
      close(readFD)
      close(writeFD)
      guardian.release()
      guardian.workers.leave()
    }
    return reply
  }
  func handleCancellation(error: XPCRichError) {}
}

signal(SIGPIPE, SIG_IGN)
signal(SIGTERM, SIG_IGN)
signal(SIGINT, SIG_IGN)
let guardian = Guardian()
let termination = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .global())
termination.setEventHandler {
  zflow_awdl_stop()
  guardian.workers.wait()
  exit(0)
}
termination.resume()
let requirement = XPCPeerRequirement.isFromSameTeam(
  andMatchesSigningIdentifier: "io.zflow.awdl-client")
let listener = try XPCListener(service: "io.zflow.awdl", requirement: requirement) { request in
  request.accept { (session: XPCSession) -> Peer in
    session.setPeerRequirement(requirement)
    return Peer(guardian)
  }
}
dispatchMain()
