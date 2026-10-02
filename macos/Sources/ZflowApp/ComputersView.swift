import AppKit
import SwiftUI

/// The home page: sharing, and the arrangement of the desk with the
/// computers found nearby on a shelf. Until a computer is paired, it shows
/// how to add the first one instead.
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
      }
      ToolbarItem(placement: .primaryAction) {
        Button("Add a Computer by Address", systemImage: "plus") { model.addingAddress = true }
          .labelStyle(.iconOnly)
          .help("Add a computer by address")
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
    case .setup: "Add a computer to share this keyboard and pointer."
    case .attention: "Fix what is below to keep sharing."
    }
  }

  private var arrangement: some View {
    VStack(alignment: .leading, spacing: 8) {
      if let layout = snapshot.layout {
        Arrangement(model: model, snapshot: snapshot, layout: layout, shelf: .bottom)
          .frame(height: snapshot.unplaced.isEmpty ? 300 : 400)
        Text("Drag computers to match your desk. The pointer crosses where two edges touch.")
          .font(.callout).foregroundStyle(.secondary)
      }
    }
  }
}

/// The canvas with the engine's layout and shelf, sending moves and drops.
private struct Arrangement: View {
  var model: AppModel
  var snapshot: Snapshot
  var layout: Layout
  var shelf: ShelfEdge
  var searching = false

  var body: some View {
    ComputerLayout(
      tiles: LayoutTile.tiles(layout, peers: snapshot.peers, ownMark: snapshot.ownMark),
      unplaced: snapshot.unplaced, shelf: shelf, searching: searching
    ) { computer, x, y, tolerance in
      model.send(
        CoreRequest(command: "move_tile", id: computer.id, x: x, y: y, tolerance: tolerance))
    } place: { computer, x, y, tolerance in
      model.send(
        CoreRequest(command: "place", id: computer.id, x: x, y: y, tolerance: tolerance))
    }
  }
}

/// The first computer, on the Computers page while nothing is paired.
private struct FirstRun: View {
  var model: AppModel
  var snapshot: Snapshot
  @State private var copied = false
  @State private var networkBlocked = false

  static let installCommand =
    "curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash"

  var body: some View {
    VStack(alignment: .leading, spacing: 18) {
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
      VStack(alignment: .leading, spacing: 4) {
        Text("Add your other computers").font(.title2.weight(.semibold))
        Text("Computers running zflow on this network show up here by themselves.")
          .foregroundStyle(.secondary)
      }
      if let layout = snapshot.layout {
        Arrangement(
          model: model, snapshot: snapshot, layout: layout, shelf: .trailing, searching: true
        )
        .frame(height: 230)
      }
      if let note = snapshot.finishNote {
        Note(title: note.title, detail: note.detail)
      }
      HStack(spacing: 10) {
        Text("Not listed? Run on it:").foregroundStyle(.secondary).fixedSize()
        Text(Self.installCommand).font(.callout.monospaced()).lineLimit(1)
          .truncationMode(.tail).textSelection(.enabled)
          .frame(maxWidth: .infinity, alignment: .leading)
        Button(copied ? "Copied" : "Copy") {
          NSPasteboard.general.clearContents()
          NSPasteboard.general.setString(Self.installCommand, forType: .string)
          copied = true
        }
      }
      .padding(.vertical, 6).padding(.leading, 12).padding(.trailing, 6)
      .background(.quinary, in: .rect(cornerRadius: 10))
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

extension Snapshot {
  /// What a fresh Mac's pairing window means for the person: while it is
  /// open, the other computer can finish on its own.
  var finishNote: (title: String, detail: String)? {
    switch pairingWindow.state {
    case .open:
      if let holding = pairingWindow.holding {
        return (
          "Adding \(holding.name)…",
          "It is the only new computer around, so this Mac accepts it by itself."
        )
      }
      let minutes = max(1, ((pairingWindow.secondsLeft ?? 0) + 59) / 60)
      let other = unplaced.first(where: \.placeable)?.name
      return (
        "Or finish from \(other ?? "the other computer")",
        "Drag \(AppModel.hostName) into place in zflow on \(other ?? "it"). This Mac is new, so for the next \(minutes) \(minutes == 1 ? "minute" : "minutes") it accepts that by itself."
      )
    case .closed where pairingWindow.reason == .rival:
      return (
        "Drag the one you want",
        "More than one new computer showed up, so this Mac waits for you to choose."
      )
    case .closed where pairingWindow.reason == .failed:
      return (
        "Drag the new computer into place",
        "This Mac could not add it by itself."
      )
    default:
      return nil
    }
  }
}

/// Something worth knowing that needs nothing done.
private struct Note: View {
  var title: String
  var detail: String

  var body: some View {
    HStack(alignment: .firstTextBaseline, spacing: 12) {
      Image(systemName: "info.circle").foregroundStyle(.tint)
      VStack(alignment: .leading, spacing: 2) {
        Text(title).fontWeight(.semibold)
        Text(detail).font(.callout).fixedSize(horizontal: false, vertical: true)
      }
      .frame(maxWidth: .infinity, alignment: .leading)
    }
    .padding(.vertical, 12).padding(.horizontal, 14)
    .background(Color.accentColor.opacity(0.07), in: .rect(cornerRadius: 14))
    .overlay { RoundedRectangle(cornerRadius: 14).strokeBorder(Color.accentColor.opacity(0.18)) }
    .accessibilityElement(children: .combine)
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
