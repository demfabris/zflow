import SwiftUI

struct PairingView: View {
  @Bindable var model: AppModel
  /// Skips the chooser when the caller already knows which computer to pair.
  var initialAddress: String? = nil
  @Environment(\.dismiss) private var dismiss
  /// The pairing address of the chosen computer; nil while choosing.
  @State private var target: String?
  @State private var address = ""
  @State private var code = ""
  /// The code last sent, so the sixth digit and Return don't both start a pairing.
  @State private var submitted: String?
  var pairing: Pairing? { model.snapshot?.pairing }
  var busy: Bool {
    ["listening", "confirm", "connecting", "approving"].contains(pairing?.state ?? "")
  }
  var body: some View {
    VStack(alignment: .leading, spacing: 20) {
      Text("Pair a computer").font(.title2.bold())
      if pairing?.state == "confirm" {
        Text("Allow \(pairing?.name ?? "this computer") to pair with this Mac?").font(.headline)
        Text(
          "It entered this Mac's code from \(pairing?.address ?? "your network"). Allow it only if it is the computer you are setting up."
        ).foregroundStyle(.secondary)
      } else if pairing?.state == "listening" {
        Text("Type this code on the other computer:")
        Text(pairing?.code ?? "…").font(.system(size: 34, weight: .medium, design: .monospaced))
          .textSelection(.enabled)
          .frame(maxWidth: .infinity)
        HStack {
          ProgressView().controlSize(.small)
          Text("Waiting for the other computer…")
        }
      } else if let target {
        Text("Type the code shown on \(Self.host(target)):")
        TextField("000 000", text: $code)
          .font(.system(size: 28, weight: .medium, design: .monospaced))
          .textFieldStyle(.roundedBorder)
          .disabled(busy)
          .onChange(of: code) { _, value in
            let digits = String(value.filter(\.isNumber).prefix(6))
            if digits != value { code = digits }
            if digits.count == 6 { submit(digits) }
          }
          .onSubmit(connect)
        if pairing?.state == "connecting" {
          HStack {
            ProgressView().controlSize(.small)
            Text("Pairing…")
          }
        } else if pairing?.state == "approving" {
          HStack {
            ProgressView().controlSize(.small)
            Text("On \(Self.host(target)), choose Allow.")
          }
        } else {
          Text("On Linux, the code is in zflow under Pair Computer.").font(.callout)
            .foregroundStyle(.secondary)
        }
      } else {
        Text("Choose the computer this Mac should control.").foregroundStyle(.secondary)
        if let nearby = model.snapshot?.nearby, !nearby.isEmpty {
          VStack(alignment: .leading, spacing: 8) {
            ForEach(nearby) { candidate in
              let address = candidate.addresses.first.map(Self.pairingAddress)
              Button {
                if let address { choose(address) }
              } label: {
                Label(
                  address.map { "Linux computer · \(Self.host($0))" } ?? "Nearby computer",
                  systemImage: "display")
              }.disabled(!candidate.compatible || address == nil)
            }
          }
        }
        HStack {
          TextField("IP address", text: $address).textFieldStyle(.roundedBorder)
            .onSubmit { choose(address.trimmingCharacters(in: .whitespaces)) }
          Button("Next") { choose(address.trimmingCharacters(in: .whitespaces)) }.disabled(
            address.trimmingCharacters(in: .whitespaces).isEmpty)
        }
        Button("Show a code on this Mac instead") {
          model.send(CoreRequest(command: "pair_start"))
        }.buttonStyle(.link)
      }
      if let error = pairing?.error ?? model.error {
        Text(error).foregroundStyle(.red).font(.callout).textSelection(.enabled)
      }
      HStack {
        if target != nil, !busy {
          Button("Back") { choose(nil) }
        }
        Spacer()
        Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
        if pairing?.state == "confirm" {
          Button("Decline") { model.send(CoreRequest(command: "pair_respond", allow: false)) }
          Button("Allow") { model.send(CoreRequest(command: "pair_respond", allow: true)) }
            .keyboardShortcut(.defaultAction)
        }
        if target != nil, !busy {
          Button("Pair", action: connect).keyboardShortcut(.defaultAction).disabled(code.count != 6)
        }
      }
    }.padding(28).frame(width: 440)
      .onAppear { if target == nil, let initialAddress { choose(initialAddress) } }
  }
  func choose(_ address: String?) {
    target = address?.isEmpty == false ? address : nil
    code = ""
    submitted = nil
    // Clears a failed attempt, which also stops a code this Mac was showing.
    model.send(CoreRequest(command: "pair_cancel"))
  }
  func connect() { submit(code) }
  func submit(_ digits: String) {
    guard let target, digits.count == 6, !busy, submitted != digits else { return }
    submitted = digits
    model.send(CoreRequest(command: "pair_start", address: target, code: digits))
  }
  /// Receivers advertise their input port; pairing listens on its own port
  /// (DEFAULT_PAIRING_PORT in src/pairing.rs).
  static func pairingAddress(_ advertised: String) -> String {
    guard let separator = advertised.lastIndex(of: ":") else { return advertised }
    return "\(advertised[..<separator]):43120"
  }
  /// The address people recognize, without a port.
  static func host(_ address: String) -> String {
    if address.hasPrefix("["), let end = address.firstIndex(of: "]") {
      return String(address[address.index(after: address.startIndex)..<end])
    }
    if address.filter({ $0 == ":" }).count == 1, let colon = address.firstIndex(of: ":") {
      return String(address[..<colon])
    }
    return address
  }
}
