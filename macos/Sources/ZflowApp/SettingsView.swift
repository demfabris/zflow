import AppKit
import SwiftUI

/// The top sections match the GNOME settings window, in the order of
/// src/app/api.rs Snapshot. The last section is for this Mac only.
struct SettingsView: View {
  @Bindable var model: AppModel
  @State private var forgetting: String?

  var body: some View {
    Group {
      if let snapshot = model.snapshot {
        form(snapshot)
      } else {
        VStack(spacing: 12) {
          ContentUnavailableView(
            "Settings could not load", systemImage: "exclamationmark.triangle",
            description: Text(model.error ?? "Starting zflow…"))
          Button("Try Again") { model.send(CoreRequest(command: "reload")) }
        }
        .frame(height: 320).padding(.bottom, 24)
      }
    }
    .frame(width: 600)
    .sheet(
      isPresented: $model.showingPairing,
      onDismiss: { model.send(CoreRequest(command: "pair_cancel")) },
      content: { PairingView(model: model) }
    )
    .confirmationDialog(
      "Forget \(forgetting ?? "computer")?",
      isPresented: Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } }),
      titleVisibility: .visible
    ) {
      Button("Forget Computer", role: .destructive) {
        if let name = forgetting { model.send(CoreRequest(command: "forget", name: name)) }
        forgetting = nil
      }
      Button("Cancel", role: .cancel) { forgetting = nil }
    } message: {
      Text(
        "This stops input between the two computers and removes its trusted identity. Pair it again to reconnect."
      )
    }
  }

  private func form(_ snapshot: Snapshot) -> some View {
    Form {
      Section {
        HStack(spacing: 10) {
          let badge = Self.badge(snapshot.status.state)
          Image(systemName: badge.0).font(.title).foregroundStyle(badge.1)
          Text(snapshot.status.title).font(.title2.weight(.semibold))
        }
        .accessibilityElement(children: .combine)
        Toggle(
          isOn: Binding(
            get: { snapshot.sharing == true },
            set: { model.send(CoreRequest(command: "set_sharing", enabled: $0)) })
        ) {
          Text("Input Sharing")
          Text("Allow input to move between paired computers.")
        }
        .disabled(snapshot.sharing == nil)
      }
      if !snapshot.health.isEmpty {
        Section("Checks") {
          ForEach(snapshot.health) { row in
            CheckRow(title: row.title, detail: row.detail, level: row.level) {
              if let action = row.action {
                Button(action.label) { model.fix(action) }
              }
            }
          }
        }
      }
      // Requests that failed; checks that fail are in the section above.
      if let message = model.error ?? model.services.error {
        Section {
          CheckRow(title: "Needs attention", detail: message, level: "warning") {}
        }
      }
      Section("Computers") {
        if let layout = snapshot.layout {
          ComputerLayout(
            computers: layout.monitors,
            move: { computer, x, y, tolerance in
              model.send(
                CoreRequest(
                  command: "move_tile", id: computer.id, x: x, y: y, tolerance: tolerance))
            }
          )
          .frame(height: 235)
          Text(
            snapshot.peers.isEmpty
              ? "Pair another computer to share your keyboard and trackpad."
              : "Drag to match your desk. Touching edges let your pointer cross."
          )
          .font(.callout).foregroundStyle(.secondary)
        }
        // Control switch and keyboard picker wait until this Mac takes input (Phase 2).
        ForEach(snapshot.peers) { peer in
          LabeledContent {
            Button("Forget \(peer.name)…", systemImage: "trash") { forgetting = peer.name }
              .labelStyle(.iconOnly).buttonStyle(.borderless)
              .help("Forget \(peer.name)")
          } label: {
            Label {
              Text(peer.name)
              Text(peer.detail).textSelection(.enabled)
            } icon: {
              Image(systemName: "display")
            }
          }
        }
        HStack {
          Spacer()
          Button("Pair Computer…", systemImage: "plus") {
            // Browsing waits for a reason to ask macOS for Local Network access.
            model.send(CoreRequest(command: "discover"))
            model.showingPairing = true
          }
          .disabled(snapshot.sharing == nil)
        }
      }
      if !snapshot.shortcuts.isEmpty {
        Section("Shortcuts") {
          ForEach(snapshot.shortcuts, id: \.title) { shortcut in
            LabeledContent(shortcut.title) {
              Text(shortcut.keys).monospaced().textSelection(.enabled)
            }
          }
        }
      }
      Section {
        Toggle(
          "Start at Login",
          isOn: Binding(
            get: { model.services.loginEnabled },
            set: { enabled in Task { await model.services.setLogin(enabled) } })
        )
        // A login item would point at the disk image or a temporary copy.
        .disabled(AppLocation.main.needsMove)
        LabeledContent("Advanced configuration") {
          Button("Open Configuration…") { model.openConfig() }
        }
      } footer: {
        Text("Changes save automatically.").font(.caption).foregroundStyle(.tertiary)
      }
      Section("This Mac") {
        MacRows(model: model, platform: snapshot.platform)
      }
    }
    .formStyle(.grouped)
    .frame(height: 640)
  }

  static func badge(_ state: String) -> (String, Color) {
    switch state {
    case "ready": ("checkmark.circle.fill", .green)
    case "controlling", "controlled": ("arrow.left.arrow.right.circle.fill", .accentColor)
    case "paused": ("pause.circle.fill", .secondary)
    case "checking": ("clock.fill", .secondary)
    case "setup": ("plus.circle.fill", .secondary)
    default: ("exclamationmark.circle.fill", .orange)
    }
  }
}

/// Accessibility, Local Network, Reduce Wi-Fi lag with its helper, and Move
/// to Applications when zflow runs from somewhere it cannot stay.
private struct MacRows: View {
  var model: AppModel
  var platform: MacPlatform

  var body: some View {
    CheckRow(
      title: "Accessibility",
      detail: platform.accessibility
        ? "Keyboard and pointer access allowed."
        : "Allow zflow to share keyboard and pointer input.",
      level: platform.accessibility ? "ok" : "error"
    ) {
      if !platform.accessibility { Button("Allow…") { model.openAccessibility() } }
    }
    let (network, networkLevel) = localNetwork
    CheckRow(title: "Local Network", detail: network, level: networkLevel) {
      if platform.localNetwork != "allowed" {
        Button("Settings…") { model.openLocalNetwork() }
      }
    }
    Toggle(
      isOn: Binding(
        get: { platform.blockAwdl },
        set: { model.send(CoreRequest(command: "set_awdl", enabled: $0)) })
    ) {
      Text("Reduce Wi-Fi lag")
      Text("Pauses AirDrop and Continuity while you control another computer.")
    }
    if platform.blockAwdl {
      CheckRow(
        title: model.services.helperTitle, detail: model.services.helperDetail,
        level: model.services.helperReady ? "ok" : "warning"
      ) {
        if !model.services.helperReady {
          Button(model.services.helperAction) { model.fixHelper() }
            .disabled(model.services.helperBusy || !model.services.hasSigningTeam)
        }
      }
    }
    if AppLocation.main.needsMove {
      CheckRow(
        title: "Move to Applications",
        detail:
          "zflow is running from a disk image or a temporary folder. Drag it into Applications, then open it from there.",
        level: "warning"
      ) {
        Button("Show Applications") {
          NSWorkspace.shared.open(URL(fileURLWithPath: "/Applications"))
        }
      }
    }
  }

  private var localNetwork: (String, String) {
    switch platform.localNetwork {
    case "allowed": ("zflow can reach computers on this network.", "ok")
    case "blocked": ("Turn on zflow in Privacy & Security → Local Network.", "error")
    default: ("Allow zflow if nearby computers do not appear.", "unknown")
    }
  }
}

/// One checked item: its state, what it means, and a fix when there is one.
struct CheckRow<Action: View>: View {
  var title: String
  var detail: String
  /// ok, warning or error; anything else has not been checked yet.
  var level: String
  @ViewBuilder var action: () -> Action

  var body: some View {
    HStack(alignment: .top, spacing: 10) {
      Image(systemName: mark.0).foregroundStyle(mark.1)
      VStack(alignment: .leading, spacing: 2) {
        Text(title)
        Text(detail).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
          .fixedSize(horizontal: false, vertical: true)
      }
      Spacer(minLength: 4)
      action()
    }
  }

  private var mark: (String, Color) {
    switch level {
    case "ok": ("checkmark.circle.fill", .green)
    case "warning": ("exclamationmark.triangle.fill", .orange)
    case "error": ("exclamationmark.circle.fill", .red)
    default: ("circle.dashed", .secondary)
    }
  }
}
