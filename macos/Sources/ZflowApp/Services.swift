import AppKit
import Foundation
import Observation
import Security
import ServiceManagement

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
      if helper.status == .requiresApproval {
        SMAppService.openSystemSettingsLoginItems()
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

  private nonisolated static func checkHelper() async -> Bool {
    await Task.detached {
      guard
        let executable = Bundle.main.executableURL?.deletingLastPathComponent()
          .appendingPathComponent("zflow-awdl-client")
      else { return false }
      let process = Process()
      process.executableURL = executable
      process.arguments = ["--check"]
      process.standardOutput = FileHandle.nullDevice
      process.standardError = FileHandle.nullDevice
      do { try process.run() } catch { return false }
      // The client bounds XPC startup with a three-second deadline.
      process.waitUntilExit()
      return process.terminationStatus == 0
    }.value
  }
}
