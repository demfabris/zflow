import AppKit
import SwiftUI

/// The menu bar panel: sharing, the computers, and the way into the window.
struct MenuBarView: View {
  var model: AppModel

  var body: some View {
    let snapshot = model.snapshot
    VStack(alignment: .leading, spacing: 2) {
      HStack(spacing: 10) {
        Image(systemName: "computermouse").font(.title3).foregroundStyle(.tint).frame(width: 24)
        VStack(alignment: .leading, spacing: 0) {
          Text("zflow").fontWeight(.semibold)
          Text(model.title).font(.callout).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        Toggle(
          "Sharing",
          isOn: Binding(
            get: { snapshot?.sharing == true },
            set: { model.send(CoreRequest(command: "set_sharing", enabled: $0)) })
        )
        .labelsHidden()
        .toggleStyle(.switch)
        .disabled(snapshot?.sharing == nil)
      }
      .padding(.horizontal, 8).padding(.vertical, 6)
      let peers = snapshot?.peers ?? []
      let found = snapshot?.unplaced.filter(\.placeable) ?? []
      if !peers.isEmpty || !found.isEmpty {
        Divider().padding(.vertical, 4)
        Text("Computers").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
          .padding(.horizontal, 8).padding(.bottom, 2)
        ForEach(peers) { peer in
          MenuRow(title: peer.name, symbol: "desktopcomputer") {
            model.show(.computer(peer.name))
          } trailing: {
            HStack(spacing: 5) {
              StateDot(state: peer.state)
              Text(peer.state.label)
            }
          }
        }
        // Found nearby: placing one happens on the arrangement.
        ForEach(found) { computer in
          MenuRow(title: computer.name, symbol: computer.symbol) {
            model.show(.computers)
          } trailing: {
            Text("Place…")
          }
        }
      }
      Divider().padding(.vertical, 4)
      MenuRow(title: "Open zflow") {
        model.show()
      } trailing: {
        Text("⌘,")
      }
      .keyboardShortcut(",")
      MenuRow(
        title: model.updates.availableVersion.map { "Update to \($0)…" } ?? "Check for Updates…"
      ) {
        model.updates.check()
      } trailing: {
        if model.updates.availableVersion != nil { Image(systemName: "arrow.down.circle") }
      }
      .disabled(!model.updates.canCheck)
      MenuRow(title: "Quit zflow") {
        model.quit()
      } trailing: {
        Text("⌘Q")
      }
      .keyboardShortcut("q")
      .disabled(model.busy)
    }
    .padding(6)
    .frame(width: 290)
  }
}

/// A row that acts like a menu item: it highlights under the pointer.
private struct MenuRow<Trailing: View>: View {
  var title: String
  var symbol: String?
  var action: () -> Void
  @ViewBuilder var trailing: () -> Trailing
  @State private var hovering = false
  @Environment(\.isEnabled) private var enabled

  var body: some View {
    Button(action: action) {
      HStack(spacing: 8) {
        if let symbol {
          Image(systemName: symbol).foregroundStyle(.secondary).frame(width: 18)
        }
        Text(title).lineLimit(1)
        Spacer(minLength: 12)
        trailing().foregroundStyle(.secondary)
      }
      .padding(.horizontal, 8).padding(.vertical, 5)
      .contentShape(.rect)
      .background(
        hovering && enabled ? Color.primary.opacity(0.1) : .clear, in: .rect(cornerRadius: 6))
    }
    .buttonStyle(.plain)
    .onHover { hovering = $0 }
  }
}
