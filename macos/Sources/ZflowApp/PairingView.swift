import SwiftUI

struct PairingView: View {
  @Bindable var model: AppModel
  @Environment(\.dismiss) private var dismiss
  @State private var address = ""
  @State private var name = ""
  @State private var code = ""
  var pairing: Pairing? { model.snapshot?.pairing }
  var body: some View {
    VStack(alignment: .leading, spacing: 20) {
      Text("Pair a computer").font(.title2.bold())
      if pairing?.state == "confirm" || pairing?.state == "saving" {
        Text("Enter this Mac’s code on the other computer:")
        Text(pairing?.code ?? "").font(.system(size: 34, weight: .medium, design: .monospaced))
          .textSelection(.enabled)
          .frame(maxWidth: .infinity)
        Form {
          TextField("Computer name", text: $name)
          TextField("Their six-digit code", text: $code)
            .onChange(of: code) { _, value in code = String(value.filter(\.isNumber).prefix(6)) }
        }
        Text("Compare the codes on both computers before confirming.").font(.callout)
          .foregroundStyle(.secondary)
      } else if pairing?.state == "waiting" {
        HStack {
          ProgressView().controlSize(.small)
          Text("Waiting for the other computer…")
        }
        Text("Start pairing on the other computer. Both sides will show a code.").foregroundStyle(
          .secondary)
      } else {
        Text("Choose a nearby computer or enter its pairing address.").foregroundStyle(.secondary)
        if let nearby = model.snapshot?.nearby, !nearby.isEmpty {
          VStack(alignment: .leading, spacing: 8) {
            ForEach(nearby) { candidate in
              Button {
                start(candidate.addresses.first)
              } label: {
                Label(candidate.addresses.first ?? "Nearby computer", systemImage: "display")
              }.disabled(!candidate.compatible)
            }
          }
        }
        HStack {
          TextField("IP address:port", text: $address).textFieldStyle(.roundedBorder)
          Button("Connect") { start(address.trimmingCharacters(in: .whitespaces)) }.disabled(
            address.isEmpty)
        }
        Button("Wait for a connection instead") { start(nil) }.buttonStyle(.link)
      }
      if let error = pairing?.error ?? model.error {
        Text(error).foregroundStyle(.red).font(.callout).textSelection(.enabled)
      }
      HStack {
        Spacer()
        Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
        if pairing?.state == "confirm" || pairing?.state == "saving" {
          Button("Pair") {
            model.send(CoreRequest(command: "pair_confirm", name: name, code: code))
          }
          .keyboardShortcut(.defaultAction)
          .disabled(
            name.trimmingCharacters(in: .whitespaces).isEmpty || code.count != 6
              || pairing?.state == "saving")
        }
      }
    }.padding(28).frame(width: 440)
      .onChange(of: pairing?.name) { _, proposed in if name.isEmpty { name = proposed ?? "" } }
  }
  func start(_ address: String?) {
    model.send(CoreRequest(command: "pair_start", address: address))
  }
}
