import AppKit
import Foundation
import Observation
import Security
import ServiceManagement
import XPC

@MainActor @Observable
final class Services {
  var helperReady = false
  var helperTitle = "Checking helper…"
  var helperDetail = ""
  var helperAction = "Install…"
  var helperBusy = false
  var loginEnabled = SMAppService.mainApp.status == .enabled
  var error: String?
  private var refreshing = false
  private let helper = SMAppService.daemon(plistName: "io.zflow.awdl.plist")

  var hasSigningTeam: Bool {
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

  func refresh() async {
    guard !refreshing else { return }
    refreshing = true
    defer { refreshing = false }
    loginEnabled = SMAppService.mainApp.status == .enabled
    guard hasSigningTeam else {
      helperReady = false
      helperTitle = "Signed build required"
      helperDetail =
        "Installing the AWDL helper requires an Apple-signed build. Sharing works with AWDL blocking off."
      helperAction = "Install…"
      return
    }
    switch helper.status {
    case .notRegistered, .notFound:
      helperReady = false
      helperTitle = "Helper not installed"
      helperDetail = "macOS will ask you to authorize the background helper."
      helperAction = "Install…"
    case .requiresApproval:
      helperReady = false
      helperTitle = "Allow the background helper"
      helperDetail = "Allow zflow in System Settings → General → Login Items & Extensions."
      helperAction = "Allow…"
    case .enabled:
      let result = await Self.checkHelper()
      helperReady = result
      helperTitle = result ? "AWDL helper ready" : "AWDL helper unavailable"
      helperDetail =
        result
        ? "Only active while controlling another computer."
        : "The helper is registered but did not respond."
      helperAction = "Repair…"
    @unknown default:
      helperReady = false
      helperTitle = "Helper unavailable"
      helperDetail = "macOS returned an unknown background service status."
    }
  }

  func installOrRepair() async {
    guard !helperBusy else { return }
    helperBusy = true
    defer { helperBusy = false }
    error = nil
    guard hasSigningTeam else {
      error = helperDetail
      return
    }
    do {
      let status = helper.status
      if status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
      } else if status == .enabled, await Self.checkHelper() {
        // The panel refreshes every 10 s, so this button can outlive the approval
        // that started the helper. Unregistering disables the item, and macOS
        // refuses an immediate register, so a working helper is left alone.
      } else {
        if helper.status == .enabled { try await helper.unregister() }
        try helper.register()
        if helper.status == .requiresApproval { SMAppService.openSystemSettingsLoginItems() }
      }
      await refresh()
    } catch {
      if helper.status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
        await refresh()
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
