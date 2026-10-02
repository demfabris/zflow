import SwiftUI

/// One paired computer: what it may do on this Mac, and forgetting it.
struct ComputerView: View {
  var model: AppModel
  var peer: Peer
  @State private var forgetting = false

  var body: some View {
    let name = peer.name
    Form {
      Section {
        Toggle(
          "\(name) can control this Mac",
          isOn: binding(peer.allowControl) {
            CoreRequest(command: "set_peer", name: name, allowControl: $0)
          })
      } header: {
        identity
      } footer: {
        Text("Push the pointer past \(name)’s edge to move onto this Mac. Take it back with ⌃⌘⌫.")
      }
      Section {
        Picker(
          "Keys from \(name)",
          selection: binding(peer.keyboard) {
            CoreRequest(command: "set_peer", name: name, keyboard: $0)
          }
        ) {
          ForEach(KeyboardMode.allCases, id: \.self) { Text($0.title).tag($0) }
        }
        Toggle(
          "Reverse scrolling",
          isOn: binding(peer.reverseScroll) {
            CoreRequest(command: "set_peer", name: name, reverseScroll: $0)
          })
      } footer: {
        Text("\(peer.keyboard.detail) Changes apply the next time \(name) takes control.")
      }
      Section {
        Button("Forget \(name)…", role: .destructive) { forgetting = true }
      } footer: {
        Text("To use \(name) again, drag it back into place from Found on your network.")
      }
    }
    .formStyle(.grouped)
    .navigationTitle(name)
    .confirmationDialog("Forget \(name)?", isPresented: $forgetting) {
      Button("Forget", role: .destructive) {
        model.send(CoreRequest(command: "forget", name: name))
      }
    } message: {
      Text(
        "This stops input between the two computers and removes its trusted identity. It goes back to Found on your network."
      )
    }
  }

  private var identity: some View {
    HStack(spacing: 12) {
      Image(systemName: "desktopcomputer").font(.system(size: 30)).foregroundStyle(.secondary)
      VStack(alignment: .leading, spacing: 2) {
        Text(peer.name).font(.title2.weight(.semibold)).foregroundStyle(.primary)
        HStack(spacing: 6) {
          StateDot(state: peer.state)
          Text(peer.detail).textSelection(.enabled)
        }
        .font(.callout).foregroundStyle(.secondary)
      }
    }
    .textCase(nil)
    .padding(.bottom, 8)
    .accessibilityElement(children: .combine)
  }

  /// Shows the engine's value, and sends each change to it.
  private func binding<Value>(
    _ value: Value, _ request: @escaping (Value) -> CoreRequest
  ) -> Binding<Value> {
    Binding(get: { value }, set: { model.send(request($0)) })
  }
}

extension KeyboardMode {
  var title: String {
    switch self {
    case .standard: "Standard keys"
    case .pcPositions: "PC key positions"
    case .mac: "Mac shortcuts"
    }
  }
  /// What the mode does on this Mac, as the README's Keyboard modes has it.
  var detail: String {
    switch self {
    case .standard: "Keys arrive as sent: Super is Command and Alt is Option."
    case .pcPositions: "Alt and Super trade places, so each key acts like the Mac key in its spot."
    case .mac:
      "Picked for a PC keyboard: Ctrl and Command trade places, so Ctrl+C copies, except in terminals."
    }
  }
}
