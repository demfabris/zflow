import Darwin
import Foundation
import XPC

func fail(_ message: String) -> Never {
  FileHandle.standardError.write(Data((message + "\n").utf8))
  exit(1)
}

func relay(input: Int32, output: Int32) -> Never {
  var input = input
  _ = fcntl(input, F_SETFL, O_NONBLOCK)
  _ = fcntl(STDOUT_FILENO, F_SETFL, O_NONBLOCK)
  var buffer = [UInt8](repeating: 0, count: 256)
  while true {
    var fds = [
      pollfd(fd: input >= 0 ? STDIN_FILENO : -1, events: Int16(POLLIN), revents: 0),
      pollfd(fd: output, events: Int16(POLLIN), revents: 0),
    ]
    let count = poll(&fds, 2, 3000)
    if count < 0 && errno == EINTR { continue }
    guard count > 0 else { exit(1) }
    if fds[0].revents != 0 {
      let received = read(STDIN_FILENO, &buffer, buffer.count)
      if received <= 0 {
        close(input)
        input = -1
      } else if write(input, buffer, received) != received {
        exit(1)
      }
    }
    if fds[1].revents != 0 {
      let received = read(output, &buffer, buffer.count)
      if received == 0 { exit(0) }
      guard received > 0, write(STDOUT_FILENO, buffer, received) == received else { exit(1) }
    }
  }
}

signal(SIGPIPE, SIG_IGN)
// Bound an unavailable service even if synchronous XPC negotiation never returns.
alarm(3)
do {
  let session = try XPCSession(
    machService: "io.zflow.awdl", options: .privileged,
    requirement: .isFromSameTeam(andMatchesSigningIdentifier: "io.zflow.awdl-daemon"))
  var request = XPCDictionary()
  let checking = CommandLine.arguments.dropFirst().first == "--check"
  request["command"] = checking ? "check" : "lease"
  let reply: XPCDictionary = try session.sendSync(message: request)
  if let error: String = reply["error"] { fail(error) }
  if checking {
    guard let ready: Bool = reply["ready"], ready else { fail("AWDL helper is not ready") }
    exit(0)
  }
  let input = reply.withUnsafeUnderlyingDictionary { xpc_dictionary_dup_fd($0, "input") }
  let output = reply.withUnsafeUnderlyingDictionary { xpc_dictionary_dup_fd($0, "output") }
  guard input >= 0, output >= 0 else { fail("AWDL helper returned no lease") }
  // The reply also owns the pipe rights. Release those before the long-lived relay.
  reply.withUnsafeUnderlyingDictionary {
    xpc_dictionary_set_value($0, "input", nil)
    xpc_dictionary_set_value($0, "output", nil)
  }
  alarm(0)
  session.cancel(reason: "Lease transferred to pipes")
  relay(input: input, output: output)
} catch { fail("AWDL helper: \(error.localizedDescription)") }
