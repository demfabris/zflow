import AppKit
import Foundation
import Observation
import Sparkle

/// Sparkle owns the update schedule, preferences, download, installation and relaunch.
@MainActor @Observable
final class Updates: NSObject, @preconcurrency SPUStandardUserDriverDelegate {
  private(set) var canCheck = false
  private(set) var automaticallyChecks = false
  private(set) var automaticallyDownloads = false
  private(set) var availableVersion: String?
  private(set) var unavailableReason: String?
  @ObservationIgnored private var controller: SPUStandardUpdaterController?
  @ObservationIgnored private var observations: [NSKeyValueObservation] = []

  override init() {
    super.init()
    unavailableReason = Self.configurationIssue(
      info: Bundle.main.infoDictionary ?? [:], location: .main)
    guard unavailableReason == nil else { return }
    let controller = SPUStandardUpdaterController(
      startingUpdater: false, updaterDelegate: nil, userDriverDelegate: self)
    self.controller = controller
    let updater = controller.updater
    // Sparkle's native permission and update windows can change these preferences too.
    observations = [
      updater.observe(\.canCheckForUpdates, options: [.new]) { [weak self] _, _ in
        Task { @MainActor [weak self] in self?.refresh() }
      },
      updater.observe(\.automaticallyChecksForUpdates, options: [.new]) { [weak self] _, _ in
        Task { @MainActor [weak self] in self?.refresh() }
      },
      updater.observe(\.automaticallyDownloadsUpdates, options: [.new]) { [weak self] _, _ in
        Task { @MainActor [weak self] in self?.refresh() }
      },
    ]
    do {
      try updater.start()
      refresh()
    } catch {
      unavailableReason = error.localizedDescription
      observations.removeAll()
      self.controller = nil
    }
  }

  nonisolated static func configurationIssue(info: [String: Any], location: AppLocation) -> String? {
    guard let key = info["SUPublicEDKey"] as? String,
      Data(base64Encoded: key)?.count == 32,
      let feed = info["SUFeedURL"] as? String,
      let url = URL(string: feed), url.scheme == "https", url.host != nil
    else { return "Updates are available in release builds of zflow." }
    if location.needsMove {
      return "Move zflow to Applications, then open it there to check for updates."
    }
    return nil
  }

  func check() {
    guard canCheck else { return }
    controller?.checkForUpdates(nil)
  }

  func setAutomaticChecks(_ enabled: Bool) {
    controller?.updater.automaticallyChecksForUpdates = enabled
    refresh()
  }

  func setAutomaticDownloads(_ enabled: Bool) {
    controller?.updater.automaticallyDownloadsUpdates = enabled
    refresh()
  }

  private func refresh() {
    guard let updater = controller?.updater else { return }
    canCheck = updater.canCheckForUpdates
    automaticallyChecks = updater.automaticallyChecksForUpdates
    automaticallyDownloads = updater.automaticallyDownloadsUpdates
  }

  var supportsGentleScheduledUpdateReminders: Bool { true }

  func standardUserDriverShouldHandleShowingScheduledUpdate(
    _ update: SUAppcastItem, andInImmediateFocus immediateFocus: Bool
  ) -> Bool {
    // A background check adds a menu bar reminder without interrupting shared input.
    immediateFocus
  }

  func standardUserDriverWillHandleShowingUpdate(
    _ handleShowingUpdate: Bool, forUpdate update: SUAppcastItem, state: SPUUserUpdateState
  ) {
    availableVersion = update.displayVersionString
  }

  func standardUserDriverWillFinishUpdateSession() {
    availableVersion = nil
  }
}
