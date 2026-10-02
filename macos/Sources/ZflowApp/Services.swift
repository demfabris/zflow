import AppKit
import Foundation
import Observation
import Security
import ServiceManagement
import XPC

@MainActor @Observable
final class Services {
  var helperReady = false
  /// What the Reduce Wi-Fi lag row says while the helper is not ready.
  var helperNote = "Checking the Wi-Fi helper…"
  /// The row's fix, when there is one.
  var helperAction: String?
  var helperBusy = false
  var loginEnabled = SMAppService.mainApp.status == .enabled
  var error: String?
  /// Only a build signed by a team can install the helper, and that can't
  /// change while zflow runs.
  let hasSigningTeam = Services.signingTeam()
  private var refreshing = false
  private let helper = SMAppService.daemon(plistName: "io.zflow.awdl.plist")

  private nonisolated static func signingTeam() -> Bool {
    var code: SecCode?
    var information: CFDictionary?
    var staticCode: SecStaticCode?
    guard SecCodeCopySelf([], &code) == errSecSuccess, let code,
      SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode,
      SecCodeCopySigningInformation(
        staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &information) == errSecSuccess,
      let information = information as? [String: Any]
    else { return false }
    return information[kSecCodeInfoTeamIdentifier as String] as? String != nil
  }

  func refreshLogin() { loginEnabled = SMAppService.mainApp.status == .enabled }

  func refreshHelper() async {
    guard !refreshing else { return }
    refreshing = true
    defer { refreshing = false }
    guard hasSigningTeam else {
      helperReady = false
      helperNote = "Needs a signed build of zflow. Sharing works without it."
      helperAction = nil
      return
    }
    switch helper.status {
    case .notRegistered, .notFound:
      helperReady = false
      helperNote = "Needs a helper. macOS asks you to allow it."
      helperAction = "Install…"
    case .requiresApproval:
      helperReady = false
      helperNote = "Allow zflow in System Settings › General › Login Items & Extensions."
      helperAction = "Allow…"
    case .enabled:
      helperReady = await Self.checkHelper()
      helperNote = helperReady ? "" : "The helper is installed but did not answer."
      helperAction = helperReady ? nil : "Repair…"
    @unknown default:
      helperReady = false
      helperNote = "macOS returned an unknown status for the helper."
      helperAction = nil
    }
  }

  func installOrRepair() async {
    guard !helperBusy else { return }
    helperBusy = true
    defer { helperBusy = false }
    error = nil
    guard hasSigningTeam else {
      error = helperNote
      return
    }
    do {
      let status = helper.status
      if status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
      } else if status == .enabled, await Self.checkHelper() {
        // The row refreshes every 10 s, so this button can outlive the approval
        // that started the helper. Unregistering disables the item, and macOS
        // refuses an immediate register, so a working helper is left alone.
      } else {
        if helper.status == .enabled { try await helper.unregister() }
        try helper.register()
        if helper.status == .requiresApproval { SMAppService.openSystemSettingsLoginItems() }
      }
      await refreshHelper()
    } catch {
      if helper.status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
        await refreshHelper()
      } else {
        self.error = error.localizedDescription
      }
    }
  }

  func setLogin(_ enabled: Bool) async {
    error = nil
    do {
      if enabled && SMAppService.mainApp.status != .requiresApproval {
        try SMAppService.mainApp.register()
      } else if !enabled {
        try await SMAppService.mainApp.unregister()
      }
      loginEnabled = SMAppService.mainApp.status == .enabled
      if enabled && SMAppService.mainApp.status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
      }
    } catch {
      if enabled && SMAppService.mainApp.status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
      } else {
        self.error = error.localizedDescription
      }
      loginEnabled = SMAppService.mainApp.status == .enabled
    }
  }

  /// Asks the helper whether it can reach awdl0. The helper answers only this
  /// signed app, and this side accepts only the helper from the same team.
  private nonisolated static func checkHelper() async -> Bool {
    guard
      let session = try? XPCSession(
        machService: "io.zflow.awdl", options: .privileged,
        requirement: .isFromSameTeam(andMatchesSigningIdentifier: "io.zflow.awdl-daemon"))
    else { return false }
    var request = XPCDictionary()
    request["command"] = "check"
    // Cancelling answers a pending reply with an error, so a stuck helper
    // cannot hold the check open.
    DispatchQueue.global().asyncAfter(deadline: .now() + 3) {
      session.cancel(reason: "Helper check timed out")
    }
    return await withCheckedContinuation { continuation in
      session.send(message: request) { result in
        let reply = try? result.get()
        let ready: Bool? = reply?["ready"]
        session.cancel(reason: "Helper checked")
        continuation.resume(returning: ready == true)
      }
    }
  }
}
