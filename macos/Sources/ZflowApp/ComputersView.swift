import AppKit
import SwiftUI

/// The home page: sharing, the arrangement of the desk, and computers nearby.
/// Until a computer is paired, it shows how to add the first one instead.
struct ComputersView: View {
  var model: AppModel
  var snapshot: Snapshot

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 20) {
        if snapshot.peers.isEmpty {
          FirstRun(model: model, snapshot: snapshot)
        } else {
          status
          ForEach(snapshot.problems) { HealthBanner(model: model, row: $0) }
          arrangement
          if !snapshot.nearby.isEmpty { nearby }
        }
      }
      .padding(24)
      .frame(maxWidth: .infinity, alignment: .leading)
    }
    .navigationTitle("Computers")
    .toolbar {
      if !snapshot.peers.isEmpty {
        ToolbarItem(placement: .primaryAction) {
          HStack(spacing: 8) {
            Text("Sharing")
            Toggle(
              "Sharing",
              isOn: Binding(
                get: { snapshot.sharing == true },
                set: { model.send(CoreRequest(command: "set_sharing", enabled: $0)) })
            )
            .labelsHidden()
            .toggleStyle(.switch)
            .controlSize(.small)
          }
          .padding(.leading, 6)
          .disabled(snapshot.sharing == nil)
        }
        ToolbarSpacer(.fixed, placement: .primaryAction)
        ToolbarItem(placement: .primaryAction) {
          Button("Pair a Computer", systemImage: "plus") { model.pair() }
            .labelStyle(.iconOnly)
            .buttonStyle(.glassProminent)
            .help("Pair a computer")
        }
      }
    }
  }

  private var status: some View {
    VStack(alignment: .leading, spacing: 4) {
      HStack(spacing: 10) {
        Circle().fill(snapshot.status.state.color).frame(width: 10, height: 10)
        Text(snapshot.status.title).font(.title2.weight(.semibold))
      }
      Text(hint).foregroundStyle(.secondary)
    }
    .accessibilityElement(children: .combine)
  }

  private var hint: String {
    let peer = snapshot.status.peer ?? "the other computer"
    return switch snapshot.status.state {
    case .ready: "Push the pointer past an edge to move onto another computer."
    case .controlling:
      "Move back across the edge to return. ⌃⌘⌫ brings input back and pauses sharing."
    case .controlled: "\(peer) is using this Mac. Its own keyboard and trackpad still work."
    case .paused: "Turn on Sharing to move between your computers."
    case .checking: "Reaching your other computers…"
    case .setup: "Pair a computer to share this keyboard and pointer."
    case .attention: "Fix what is below to keep sharing."
    }
  }

  private var arrangement: some View {
    VStack(alignment: .leading, spacing: 8) {
      if let layout = snapshot.layout {
        ComputerLayout(tiles: LayoutTile.tiles(layout, peers: snapshot.peers)) {
          computer, x, y, tolerance in
          model.send(
            CoreRequest(command: "move_tile", id: computer.id, x: x, y: y, tolerance: tolerance))
        }
        .frame(height: 300)
        Text("Drag computers to match your desk. The pointer crosses where two edges touch.")
          .font(.callout).foregroundStyle(.secondary)
      }
    }
  }

  private var nearby: some View {
    VStack(alignment: .leading, spacing: 8) {
      HStack(alignment: .firstTextBaseline, spacing: 8) {
        Text("Nearby").font(.headline)
        Text("Computers running zflow on this network").font(.callout).foregroundStyle(.secondary)
      }
      NearbyList(model: model, nearby: snapshot.nearby, searching: false)
    }
  }
}

/// The first pairing, on the Computers page while nothing is paired.
private struct FirstRun: View {
  var model: AppModel
  var snapshot: Snapshot
  @State private var copied = false
  @State private var networkBlocked = false

  static let installCommand =
    "curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash"

  var body: some View {
    VStack(alignment: .leading, spacing: 20) {
      if !snapshot.platform.accessibility {
        Card(
          symbol: "accessibility", title: "Let zflow move the pointer here",
          detail: "macOS asks once, in Privacy & Security › Accessibility."
        ) {
          Button("Allow…") { model.openAccessibility() }
        }
        .task { await model.watchAccessibility() }
      }
      if networkBlocked {
        Card(
          symbol: "network", title: "Let zflow look for your computers",
          detail: "Turn on zflow in Privacy & Security › Local Network."
        ) {
          Button("Allow…") { model.allowLocalNetwork() }
        }
      }
      // The card above stands in for the sharing row.
      ForEach(snapshot.problems.filter { $0.id != "sharing" }) {
        HealthBanner(model: model, row: $0)
      }
      VStack(alignment: .leading, spacing: 6) {
        Text("Add your other computers").font(.title2.weight(.semibold))
        Text(
          "Computers running zflow on this network show up below by themselves. Pick one, then type the code it shows."
        )
        .foregroundStyle(.secondary)
      }
      NearbyList(model: model, nearby: snapshot.nearby, searching: true)
      VStack(alignment: .leading, spacing: 8) {
        Text("Not listed? Run this on it:").foregroundStyle(.secondary)
        HStack(alignment: .top) {
          Text(Self.installCommand).font(.callout.monospaced())
            .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
          Button(copied ? "Copied" : "Copy") {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(Self.installCommand, forType: .string)
            copied = true
          }
        }
        .padding(12)
        .background(.quinary, in: .rect(cornerRadius: 10))
        Button("Enter an address instead…") { model.pair() }.buttonStyle(.link)
      }
    }
    // Looking for computers is what makes macOS ask for Local Network access.
    .task { model.send(CoreRequest(command: "discover")) }
    .task(id: snapshot.platform.localNetwork) {
      networkBlocked = false
      guard snapshot.platform.localNetwork == .blocked else { return }
      // It reads blocked until the person answers macOS, so wait before saying so.
      try? await Task.sleep(for: .seconds(5))
      if !Task.isCancelled { networkBlocked = true }
    }
  }
}

/// A step that still needs the person, with the button that does it.
private struct Card<Action: View>: View {
  var symbol: String
  var title: String
  var detail: String
  @ViewBuilder var action: () -> Action

  var body: some View {
    HStack(spacing: 14) {
      Image(systemName: symbol).font(.title2).foregroundStyle(.tint).frame(width: 28)
      VStack(alignment: .leading, spacing: 2) {
        Text(title).fontWeight(.semibold)
        Text(detail).font(.callout).foregroundStyle(.secondary)
      }
      .frame(maxWidth: .infinity, alignment: .leading)
      action()
    }
    .padding(14)
    .background(.quinary, in: .rect(cornerRadius: 12))
  }
}

/// Computers running zflow on this network that are not paired yet.
private struct NearbyList: View {
  var model: AppModel
  var nearby: [Nearby]
  /// Ends the list with a row saying more may show up.
  var searching: Bool

  var body: some View {
    VStack(spacing: 0) {
      ForEach(nearby) { computer in
        HStack(spacing: 12) {
          Image(systemName: "desktopcomputer").font(.title3).foregroundStyle(.secondary)
          VStack(alignment: .leading, spacing: 1) {
            Text(computer.host).fontWeight(.semibold)
            Text(computer.compatible ? "Ready to pair" : "Update zflow on it to pair")
              .font(.callout).foregroundStyle(.secondary)
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          Button("Pair") { model.pair(computer.pairAddress) }
            .disabled(!computer.compatible || computer.pairAddress == nil)
        }
        .padding(.vertical, 10).padding(.horizontal, 14)
        if computer.id != nearby.last?.id || searching { Divider().padding(.leading, 14) }
      }
      if searching {
        HStack(spacing: 10) {
          ProgressView().controlSize(.small)
          Text(nearby.isEmpty ? "Looking for computers…" : "Looking for more…")
            .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 10).padding(.horizontal, 14)
      }
    }
    .background(.quinary, in: .rect(cornerRadius: 12))
  }
}
