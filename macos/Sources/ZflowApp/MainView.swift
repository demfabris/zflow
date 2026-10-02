import AppKit
import SwiftUI

/// The one window: the computers, settings, and a page for each paired computer.
struct MainView: View {
  @Bindable var model: AppModel

  var body: some View {
    NavigationSplitView {
      Sidebar(model: model)
    } detail: {
      page
        .safeAreaInset(edge: .bottom) {
          if let failure = model.failure {
            Banner(level: .warning, title: "That didn’t work", detail: failure) {
              Button("Dismiss") { model.failure = nil }
            }
            .padding([.horizontal, .bottom], 16)
          }
        }
    }
    .frame(minWidth: 700, minHeight: 480)
    .sheet(isPresented: $model.addingAddress) { AddAddressView(model: model) }
    .alert("Move zflow to Applications", isPresented: $model.askToMove) {
      Button("Show Applications") {
        NSWorkspace.shared.open(URL(fileURLWithPath: "/Applications"))
      }
      Button("Not Now", role: .cancel) {}
    } message: {
      Text(
        "zflow is running from a disk image or a temporary folder. Drag it into Applications, then open it from there."
      )
    }
    // Without a window, zflow lives in the menu bar only.
    .onAppear { NSApplication.shared.setActivationPolicy(.regular) }
    .onDisappear { NSApplication.shared.setActivationPolicy(.accessory) }
  }

  @ViewBuilder private var page: some View {
    if let snapshot = model.snapshot {
      switch model.page {
      case .computers:
        ComputersView(model: model, snapshot: snapshot)
      case .settings:
        SettingsView(model: model, snapshot: snapshot)
      case .computer(let name):
        if let peer = snapshot.peers.first(where: { $0.name == name }) {
          ComputerView(model: model, peer: peer)
        }
      }
    } else if let error = model.error {
      ContentUnavailableView {
        Label("zflow could not start", systemImage: "exclamationmark.triangle")
      } description: {
        Text(error)
      } actions: {
        Button("Try Again") { model.send(CoreRequest(command: "reload")) }
      }
    } else {
      ProgressView()
    }
  }
}

private struct Sidebar: View {
  @Bindable var model: AppModel

  var body: some View {
    List(selection: $model.page) {
      Label("Computers", systemImage: "rectangle.3.group").tag(Page.computers)
      Label("Settings", systemImage: "slider.horizontal.3").tag(Page.settings)
      let peers = model.snapshot?.peers ?? []
      if !peers.isEmpty {
        Section("Paired") {
          ForEach(peers) { peer in
            Label {
              HStack {
                Text(peer.name).lineLimit(1)
                Spacer()
                StateDot(state: peer.state)
              }
            } icon: {
              Image(systemName: "desktopcomputer")
            }
            .tag(Page.computer(peer.name))
            .accessibilityValue(Text(peer.state.label))
          }
        }
      }
    }
    .navigationSplitViewColumnWidth(min: 180, ideal: 210, max: 280)
    .safeAreaInset(edge: .bottom, alignment: .leading) {
      Text(Self.version).font(.caption).foregroundStyle(.secondary)
        .padding(.horizontal, 20).padding(.bottom, 14)
    }
  }

  private static let version =
    (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String).map { "zflow \($0)" }
    ?? "zflow"
}

/// A paired computer's state as a small dot: filled while connected, hollow
/// while it is not.
struct StateDot: View {
  var state: Peer.State

  var body: some View {
    Group {
      if let color = state.color {
        Circle().fill(color)
      } else {
        Circle().strokeBorder(.secondary, lineWidth: 1.5)
      }
    }
    .frame(width: 8, height: 8)
    .accessibilityHidden(true)
  }
}

extension Peer.State {
  /// Short enough for a tile or a menu row; the computer's page has the detail.
  var label: String {
    switch self {
    case .paired: "Not connected"
    case .connecting: "Connecting…"
    case .connected: "Connected"
    case .controllingThis: "Controlling this Mac"
    case .controlledFromHere: "Controlled from here"
    case .unreachable: "Offline"
    }
  }
  /// Nil draws a hollow dot.
  var color: Color? {
    switch self {
    case .connected: .green
    case .controllingThis, .controlledFromHere: .accentColor
    case .connecting: .yellow
    case .paired, .unreachable: nil
    }
  }
}

extension Status.State {
  var color: Color {
    switch self {
    case .ready: .green
    case .controlling, .controlled: .accentColor
    case .checking: .yellow
    case .attention: .orange
    case .paused, .setup: .secondary
    }
  }
}

/// Something worth a look, with its fix when there is one.
struct Banner<Actions: View>: View {
  var level: Health.Level
  var title: String
  var detail: String
  @ViewBuilder var actions: () -> Actions

  var body: some View {
    let tint: Color = level == .error ? .red : .orange
    HStack(spacing: 12) {
      Image(
        systemName: level == .error
          ? "exclamationmark.octagon.fill" : "exclamationmark.triangle.fill"
      )
      .foregroundStyle(tint)
      VStack(alignment: .leading, spacing: 2) {
        Text(title).fontWeight(.semibold)
        Text(detail).foregroundStyle(.secondary).textSelection(.enabled)
          .fixedSize(horizontal: false, vertical: true)
      }
      .frame(maxWidth: .infinity, alignment: .leading)
      actions()
    }
    .padding(.vertical, 10).padding(.horizontal, 14)
    .background(tint.opacity(0.1), in: .rect(cornerRadius: 12))
    .overlay { RoundedRectangle(cornerRadius: 12).strokeBorder(tint.opacity(0.3)) }
  }
}

/// A health row that is not fine, with its fix.
struct HealthBanner: View {
  var model: AppModel
  var row: Health

  var body: some View {
    Banner(level: row.level, title: row.title, detail: row.detail) {
      if let action = model.fix(for: row) {
        Button(action.label) { model.fix(action) }
      }
    }
  }
}
