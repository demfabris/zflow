import AppKit
import SwiftUI

/// What applies to this Mac as a whole. Each paired computer has its own page.
struct SettingsView: View {
  var model: AppModel
  var snapshot: Snapshot

  var body: some View {
    let platform = snapshot.platform
    Form {
      Section("General") {
        Toggle(
          "Start at login",
          isOn: Binding(get: { model.services.loginEnabled }, set: { model.setLogin($0) })
        )
        // A login item would point at the disk image or a temporary copy.
        .disabled(AppLocation.main.needsMove)
        if let share = snapshot.shareClipboard {
          Toggle(
            isOn: Binding(
              get: { share },
              set: { model.send(CoreRequest(command: "set_clipboard", share: $0)) })
          ) {
            Text("Share clipboard")
            Text("Copy on one computer, paste on the next.")
          }
        }
        if let pause = snapshot.pauseAtEdges {
          Toggle(
            isOn: Binding(
              get: { pause },
              set: { model.send(CoreRequest(command: "set_switching", pauseAtEdges: $0)) })
          ) {
            Text("Pause at edges")
            Text("Rest against an edge for a moment before switching.")
          }
        }
      }
      // Only a signed build can install the helper this needs.
      if model.services.hasSigningTeam {
        Section("Wi-Fi") {
          WiFiRow(model: model, enabled: platform.blockAwdl)
        }
      }
      Section {
        PermissionRow(title: "Accessibility", allowed: platform.accessibility) {
          model.openAccessibility()
        }
        PermissionRow(
          title: "Local Network", allowed: platform.localNetwork == .allowed,
          detail: platform.localNetwork == .blocked
            ? "Needed to find your computers and reach them."
            : "macOS asks when zflow first looks for computers."
        ) {
          model.allowLocalNetwork()
        }
        PermissionRow(
          title: "Paste from Other Apps", allowed: model.pasteAccess == .alwaysAllow,
          detail: "Needed to share what you copy on this Mac."
        ) {
          model.allowPaste()
        }
      } header: {
        Text("Permissions")
      } footer: {
        Text("Take back control from any computer with ⌃⌘⌫.")
      }
      Section("Updates") {
        if let reason = model.updates.unavailableReason {
          Text(reason).foregroundStyle(.secondary)
        } else {
          Toggle(
            "Check for updates automatically",
            isOn: Binding(
              get: { model.updates.automaticallyChecks },
              set: { model.updates.setAutomaticChecks($0) })
          )
          Toggle(
            "Download and install updates automatically",
            isOn: Binding(
              get: { model.updates.automaticallyDownloads },
              set: { model.updates.setAutomaticDownloads($0) })
          )
          .disabled(!model.updates.automaticallyChecks)
          Button("Check for Updates…") { model.updates.check() }
            .disabled(!model.updates.canCheck)
          if let version = model.updates.availableVersion {
            Text("zflow \(version) is available.").foregroundStyle(.secondary)
          }
        }
      }
    }
    .formStyle(.grouped)
    .navigationTitle("Settings")
  }
}

/// Reduce Wi-Fi lag, with what its helper still needs while it is on.
private struct WiFiRow: View {
  var model: AppModel
  var enabled: Bool

  var body: some View {
    let services = model.services
    let waiting = enabled && !services.helperReady
    LabeledContent {
      HStack {
        if waiting, let action = services.helperAction {
          Button(action) { model.fixHelper() }.disabled(services.helperBusy)
        }
        Toggle(
          "Reduce Wi-Fi lag", isOn: Binding(get: { enabled }, set: { model.setAwdl($0) })
        )
        .labelsHidden()
        .toggleStyle(.switch)
      }
    } label: {
      Text("Reduce Wi-Fi lag")
      Text(
        waiting
          ? services.helperNote
          : "Pauses AirDrop’s radio while input is shared with another computer.")
    }
  }
}

/// A permission macOS keeps, and the button that asks for it.
private struct PermissionRow: View {
  var title: String
  var allowed: Bool
  /// Said only while it is not allowed.
  var detail: String?
  var allow: () -> Void

  var body: some View {
    LabeledContent {
      if allowed {
        Label {
          Text("Allowed").foregroundStyle(.secondary)
        } icon: {
          Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        }
      } else {
        Button("Allow…", action: allow)
      }
    } label: {
      Text(title)
      if !allowed, let detail { Text(detail) }
    }
  }
}
