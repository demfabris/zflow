import AppKit
import Foundation
import Observation

@MainActor @Observable
final class AppModel {
  var snapshot: Snapshot?
  var error: String?
  var showingPairing = false
  var showingHealth = false
  var busy = false
  let services = Services()
  let configPath: String
  let showSettingsAtLaunch: Bool
  private let core: CoreBridge
  private var poller: Task<Void, Never>?

  init() {
    let arguments = ProcessInfo.processInfo.arguments
    if let index = arguments.firstIndex(of: "--config"), arguments.indices.contains(index + 1) {
      configPath = URL(fileURLWithPath: arguments[index + 1]).standardizedFileURL.path
    } else {
      configPath =
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(
          "Library/Application Support/zflow/zflow.toml"
        ).path
    }
    showSettingsAtLaunch =
      arguments.contains("--show-settings") || !FileManager.default.fileExists(atPath: configPath)
    core = CoreBridge(path: configPath)
    poller = Task { [weak self] in
      var ticks = 0
      while !Task.isCancelled {
        guard let self else { return }
        await self.perform(CoreRequest(command: "snapshot"), quiet: true)
        if ticks % 20 == 0 {
          await self.services.refresh()
          await self.perform(
            CoreRequest(command: "helper_ready", ready: self.services.helperReady), quiet: true)
        }
        ticks += 1
        try? await Task.sleep(for: .milliseconds(500))
      }
    }
  }

  var title: String { snapshot?.title ?? "Needs attention" }
  var healthy: Bool { ["ready", "sharing"].contains(snapshot?.status ?? "") }

  func send(_ request: CoreRequest) {
    Task { await perform(request) }
  }
  func perform(_ request: CoreRequest, quiet: Bool = false) async {
    guard !busy else { return }
    do {
      let result = try await core.request(request)
      if !quiet || snapshot == nil { error = nil }
      snapshot = result
      if snapshot?.pairing.state == "paired", showingPairing {
        showingPairing = false
        send(CoreRequest(command: "pair_cancel"))
        send(CoreRequest(command: "reload"))
      }
    } catch { if !quiet || snapshot == nil { self.error = error.localizedDescription } }
  }
  func openConfig() { NSWorkspace.shared.open(URL(fileURLWithPath: configPath)) }
  func openAccessibility() {
    send(CoreRequest(command: "allow_accessibility"))
    NSWorkspace.shared.open(
      URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!)
  }
  func fixHelper() {
    Task {
      await services.installOrRepair()
      await perform(CoreRequest(command: "helper_ready", ready: services.helperReady))
    }
  }
  func quit() { NSApplication.shared.terminate(nil) }
  func shutdown() async {
    guard !busy else { return }
    busy = true
    poller?.cancel()
    await core.shutdown()
  }
}
