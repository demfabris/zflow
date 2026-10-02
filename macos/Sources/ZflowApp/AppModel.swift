import AppKit
import Foundation
import Observation
import SwiftUI

/// What the main window shows.
enum Page: Hashable {
  case computers
  case settings
  /// A paired computer, by name.
  case computer(String)
}

@MainActor @Observable
final class AppModel {
  var snapshot: Snapshot?
  /// Why the engine gave no snapshot. The window shows it in place of a page.
  var error: String?
  /// The last request that failed, shown for a few seconds.
  var failure: String?
  var page = Page.computers
  /// Shows the Add by address sheet.
  var addingAddress = false
  /// Posts a computer that joined. The app sets it; each notice comes once.
  @ObservationIgnored var joined: ((Notice) -> Void)?
  @ObservationIgnored private var lastNotice: UInt64 = 0
  /// Asked once at launch, when zflow runs from somewhere it cannot stay.
  var askToMove = AppLocation.main.needsMove
  /// How macOS treats zflow reading what other apps copied.
  var pasteAccess = NSPasteboard.general.accessBehavior
  var busy = false
  let services = Services()
  let configPath: String
  let showSettingsAtLaunch: Bool
  /// Set by the menu bar icon, which lives as long as the app.
  @ObservationIgnored var openWindow: OpenWindowAction?
  private let core: CoreBridge
  private var poller: Task<Void, Never>?
  @ObservationIgnored private var activity: (any NSObjectProtocol)?
  @ObservationIgnored private var wake: (any NSObjectProtocol)?
  @ObservationIgnored private var failureTimer: Task<Void, Never>?

  /// Tests pass `polling: false`, so nothing reaches the engine until they ask.
  init(arguments: [String] = ProcessInfo.processInfo.arguments, polling: Bool = true) {
    if let index = arguments.firstIndex(of: "--config"), arguments.indices.contains(index + 1) {
      configPath = URL(fileURLWithPath: arguments[index + 1]).standardizedFileURL.path
    } else {
      configPath =
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(
          "Library/Application Support/zflow/zflow.toml"
        ).path
    }
    showSettingsAtLaunch = arguments.contains("--show-settings")
    core = CoreBridge(path: configPath)
    guard polling else { return }
    poller = Task { [weak self] in
      var ticks = 0
      while !Task.isCancelled {
        guard let self else { return }
        await self.perform(CoreRequest(command: "snapshot"), quiet: true)
        let access = NSPasteboard.general.accessBehavior
        if access != self.pasteAccess { self.pasteAccess = access }
        if ticks % 20 == 0 { await self.checkServices() }
        ticks += 1
        try? await Task.sleep(for: .milliseconds(500))
      }
    }
    // A receiver can drop the session while the Mac sleeps, and the link would
    // still look ready until its next keep-alive. Recheck it on wake.
    wake = NSWorkspace.shared.notificationCenter.addObserver(
      forName: NSWorkspace.didWakeNotification, object: nil, queue: .main
    ) { [weak self] _ in
      MainActor.assumeIsolated { self?.send(CoreRequest(command: "retry")) }
    }
  }

  var title: String { snapshot?.status.title ?? "Needs attention" }
  var healthy: Bool {
    snapshot.map { [.ready, .controlling, .controlled].contains($0.status.state) } ?? false
  }

  func send(_ request: CoreRequest) {
    Task { await perform(request) }
  }
  func perform(_ request: CoreRequest, quiet: Bool = false) async {
    guard let message = await attempt(request) else { return }
    if snapshot == nil {
      error = message
    } else if !quiet {
      report(message)
    }
  }
  /// Sends `request` and returns why it failed, for a caller that shows it.
  func attempt(_ request: CoreRequest) async -> String? {
    guard !busy else { return nil }
    do {
      apply(try await core.request(request))
      error = nil
      return nil
    } catch {
      return error.localizedDescription
    }
  }

  /// Takes the engine's latest snapshot. The page follows it, and each new
  /// notice is posted.
  func apply(_ next: Snapshot) {
    if next != snapshot { snapshot = next }
    for notice in next.notices where notice.id > lastNotice {
      lastNotice = notice.id
      joined?(notice)
    }
    holdActivity(next.sharing == true)
    if case .computer(let name) = page, !next.peers.contains(where: { $0.name == name }) {
      page = .computers
    }
  }

  /// Shows a failed request for a few seconds.
  func report(_ message: String?) {
    guard let message else { return }
    failure = message
    failureTimer?.cancel()
    failureTimer = Task { [weak self] in
      try? await Task.sleep(for: .seconds(8))
      if !Task.isCancelled { self?.failure = nil }
    }
  }

  // The engine polls input on 1 to 12 ms timers. App Nap and timer coalescing
  // would stutter the pointer and delay crossings, so opt out while sharing.
  private func holdActivity(_ sharing: Bool) {
    if sharing, activity == nil {
      activity = ProcessInfo.processInfo.beginActivity(
        options: [.userInitiatedAllowingIdleSystemSleep, .latencyCritical],
        reason: "Sharing input with another computer")
    } else if !sharing, let activity {
      ProcessInfo.processInfo.endActivity(activity)
      self.activity = nil
    }
  }

  /// Brings the window to the front, on `page` when given. The app gets its
  /// Dock icon while the window is open.
  func show(_ page: Page? = nil) {
    if let page { self.page = page }
    guard let openWindow else { return }
    NSApplication.shared.setActivationPolicy(.regular)
    openWindow(id: "main")
    NSApplication.shared.activate()
  }

  /// The name other computers know this Mac by, as its hellos give it.
  nonisolated static let hostName: String = {
    var buffer = [CChar](repeating: 0, count: 256)
    guard gethostname(&buffer, buffer.count) == 0 else { return "this Mac" }
    var name = buffer.withUnsafeBufferPointer { String(cString: $0.baseAddress!) }
    if name.lowercased().hasSuffix(".local") { name.removeLast(".local".count) }
    return name.isEmpty ? "this Mac" : name
  }()

  /// A health row's fix. The engine's rows carry theirs; the clipboard row
  /// gets one here while macOS keeps zflow from reading the pasteboard.
  func fix(for row: Health) -> HealthAction? {
    if let action = row.action { return action }
    if row.id == "clipboard", pasteAccess != .alwaysAllow {
      return HealthAction(label: "Allow…", command: "allow_paste")
    }
    return nil
  }
  /// The app handles the fixes that open something here.
  func fix(_ action: HealthAction) {
    switch action.command {
    case "allow_accessibility": openAccessibility()
    case "allow_paste": allowPaste()
    case "open_config": openConfig()
    default: send(CoreRequest(command: action.command))
    }
  }
  func openConfig() { NSWorkspace.shared.open(URL(fileURLWithPath: configPath)) }
  func openAccessibility() {
    send(CoreRequest(command: "allow_accessibility"))
    openPrivacy("Privacy_Accessibility")
  }
  /// Looking for computers is what makes macOS ask; once it has, the switch
  /// is in System Settings.
  func allowLocalNetwork() {
    if snapshot?.platform.localNetwork == .unknown {
      send(CoreRequest(command: "discover"))
    } else {
      openPrivacy("Privacy_LocalNetwork")
    }
  }
  /// The first read makes macOS ask, which also lists zflow in System
  /// Settings; after that, the switch is there.
  func allowPaste() {
    if pasteAccess == .default {
      _ = NSPasteboard.general.string(forType: .string)
      pasteAccess = NSPasteboard.general.accessBehavior
    } else {
      openPrivacy("Privacy_Pasteboard")
    }
  }
  private func openPrivacy(_ anchor: String) {
    NSWorkspace.shared.open(
      URL(string: "x-apple.systempreferences:com.apple.preference.security?\(anchor)")!)
  }

  /// While Accessibility is missing, checks it faster than the engine's own
  /// ticks, and comes back from System Settings once it is on.
  func watchAccessibility() async {
    guard snapshot?.platform.accessibility == false else { return }
    while !Task.isCancelled && snapshot?.platform.accessibility == false {
      await perform(CoreRequest(command: "check_accessibility"), quiet: true)
      try? await Task.sleep(for: .milliseconds(500))
    }
    if snapshot?.platform.accessibility == true { NSApplication.shared.activate() }
  }

  /// The login item always, and the Wi-Fi helper only while Reduce Wi-Fi lag
  /// is on, since checking it means a call to the helper.
  func checkServices() async {
    services.refreshLogin()
    guard snapshot?.platform.blockAwdl == true else { return }
    await services.refreshHelper()
    await perform(
      CoreRequest(command: "helper_ready", ready: services.helperReady), quiet: true)
  }
  func setAwdl(_ enabled: Bool) {
    Task {
      await perform(CoreRequest(command: "set_awdl", enabled: enabled))
      if enabled { await checkServices() }
    }
  }
  func fixHelper() {
    Task {
      await services.installOrRepair()
      report(services.error)
      await perform(CoreRequest(command: "helper_ready", ready: services.helperReady))
    }
  }
  func setLogin(_ enabled: Bool) {
    Task {
      await services.setLogin(enabled)
      report(services.error)
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
