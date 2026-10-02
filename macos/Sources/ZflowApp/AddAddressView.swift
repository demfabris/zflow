import SwiftUI

/// Says hello to a computer this Mac cannot find on the network, such as
/// one across Tailscale. It then shows up on the shelf like any other.
struct AddAddressView: View {
  var model: AppModel
  @Environment(\.dismiss) private var dismiss
  @State private var address = ""
  @State private var error: String?
  @State private var looking = false

  private var typed: String { address.trimmingCharacters(in: .whitespaces) }

  var body: some View {
    VStack(alignment: .leading, spacing: 14) {
      VStack(alignment: .leading, spacing: 4) {
        Text("Add a computer by address").font(.title3.weight(.semibold))
        Text("For a computer zflow can’t find on this network, like one on Tailscale.")
          .foregroundStyle(.secondary)
          .fixedSize(horizontal: false, vertical: true)
      }
      VStack(alignment: .leading, spacing: 6) {
        Text("Address").font(.callout.weight(.semibold)).foregroundStyle(.secondary)
        TextField("Address", text: $address, prompt: Text("100.64.0.7"))
          .labelsHidden()
          .textFieldStyle(.roundedBorder)
          .onSubmit(lookUp)
      }
      if let error {
        Text(error).font(.callout).foregroundStyle(.red).textSelection(.enabled)
      } else {
        Text("It shows up in Found on your network. Drag it into place to add it.")
          .font(.callout).foregroundStyle(.secondary)
      }
      HStack {
        // Code pairing stays until every computer can place the others.
        Button("Pair with a code instead…") {
          model.pairWhenClosed = true
          dismiss()
        }
        .buttonStyle(.link)
        Spacer()
        Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
        Button("Look Up", action: lookUp)
          .keyboardShortcut(.defaultAction)
          .disabled(typed.isEmpty || looking)
      }
      .padding(.top, 4)
    }
    .padding(24)
    .frame(width: 440)
  }

  private func lookUp() {
    guard !typed.isEmpty, !looking else { return }
    looking = true
    Task {
      error = await model.attempt(CoreRequest(command: "add_address", address: typed))
      looking = false
      if error == nil { dismiss() }
    }
  }
}
