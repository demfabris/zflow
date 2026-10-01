import AppKit
import SwiftUI

/// First-run setup. Each step checks itself off and moves on; Back returns
/// to a finished step.
struct OnboardingView: View {
  @Bindable var model: AppModel
  @Environment(\.dismiss) private var dismiss
  @Environment(\.openWindow) private var openWindow
  @State private var step: SetupStep?
  @State private var movedOn = false
  @State private var movedCopy: URL?
  @State private var blocked = false
  @State private var searched = false
  @State private var pairing: PairingTarget?
  @State private var newestPeer: String?
  @State private var shared = false
  @State private var copied = false
  @State private var openAtLogin = true
  @State private var reduceLag = false

  static let installCommand =
    "curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/demfabris/zflow/main/install.sh | bash"

  private var steps: [SetupStep] {
    SetupStep.allCases.filter { $0 != .move || AppLocation.main.needsMove }
  }
  private var facts: SetupFacts {
    SetupFacts(
      needsMove: AppLocation.main.needsMove && !movedOn,
      accessibility: model.snapshot?.platform.accessibility ?? false,
      localNetwork: model.snapshot?.platform.localNetwork == "allowed",
      peers: model.snapshot?.peers.count ?? 0)
  }

  var body: some View {
    let current = step ?? facts.start
    VStack(alignment: .leading, spacing: 0) {
      progress(current).padding(.horizontal, 24).padding(.top, 18)
      VStack(alignment: .leading, spacing: 16) {
        header(current)
        content(current)
      }
      .padding(24)
      .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
      if let error = model.error {
        Label(error, systemImage: "exclamationmark.triangle.fill").font(.callout)
          .padding(.horizontal, 24).padding(.bottom, 10)
      }
      Divider()
      buttons(current).padding(.horizontal, 24).padding(.vertical, 14)
    }
    .frame(width: 520, height: 420)
    .onChange(of: facts) { old, new in
      let showing = step ?? old.start
      let next = new.advance(showing, from: old)
      step = next
      if next != showing {
        pairing = nil
        // The user answered in System Settings, which is in front now.
        if showing == .accessibility || showing == .localNetwork { bringToFront() }
      }
    }
    .onChange(of: model.snapshot?.peerNames ?? []) { old, new in
      if let added = new.first(where: { !old.contains($0) }) { newestPeer = added }
    }
    .onChange(of: model.snapshot?.status.state) { _, state in
      if state == "controlling" && current == .tryIt { shared = true }
    }
    .onChange(of: model.snapshot?.pairing.state) { _, state in
      // Pairing saves the computer to the file; read it now instead of at
      // the next maintenance reload.
      if state == "paired" && pairing != nil { model.send(CoreRequest(command: "reload")) }
    }
    .task(id: current) { await watch(current) }
    .sheet(
      item: $pairing,
      onDismiss: { model.send(CoreRequest(command: "pair_cancel")) },
      content: { PairingView(model: model, initialAddress: $0.address) }
    )
  }

  private func progress(_ current: SetupStep) -> some View {
    HStack(spacing: 14) {
      ForEach(steps, id: \.self) { item in
        let done = facts.done(item) || (item == .tryIt && shared)
        HStack(spacing: 4) {
          if done {
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
          } else {
            Image(systemName: item == current ? "circle.inset.filled" : "circle")
              .foregroundStyle(item == current ? AnyShapeStyle(.tint) : AnyShapeStyle(.tertiary))
          }
          Text(item.title).foregroundStyle(item == current ? .primary : .secondary)
        }
      }
    }
    .font(.subheadline)
  }

  private func header(_ step: SetupStep) -> some View {
    let (symbol, title, detail): (String, String, String) =
      switch step {
      case .move:
        (
          "arrow.down.app", "Move zflow to Applications",
          "zflow is running from a disk image or a temporary folder. Drag it into Applications, then open it from there."
        )
      case .accessibility:
        (
          "accessibility", "Allow Accessibility",
          "zflow needs Accessibility access to share your keyboard and trackpad with your other computers, and to let them control this Mac."
        )
      case .localNetwork:
        (
          "network", "Allow Local Network",
          "zflow looks for your computers on this network. When macOS asks, choose Allow."
        )
      case .computers:
        (
          "desktopcomputer", "Pair Your Other Computer",
          "Computers running zflow on this network show up here. Pair one and type the code it shows."
        )
      case .tryIt:
        (
          "cursorarrow.motionlines", "Try It",
          "Move the pointer off the \(model.snapshot?.edge(toward: newestPeer) ?? "right") edge of your screen."
        )
      }
    return HStack(alignment: .top, spacing: 14) {
      Image(systemName: symbol).font(.system(size: 28)).foregroundStyle(.tint).frame(width: 36)
      VStack(alignment: .leading, spacing: 4) {
        Text(title).font(.title2.bold())
        Text(detail).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
      }
    }
  }

  @ViewBuilder private func content(_ step: SetupStep) -> some View {
    switch step {
    case .move:
      if movedCopy != nil {
        status("zflow is in Applications now.", .done)
      } else {
        status("Waiting for zflow in Applications…", .waiting)
      }
    case .accessibility:
      if facts.accessibility {
        status("Accessibility is on.", .done)
      } else {
        status("Click Allow, then turn on zflow in the Accessibility list.", .waiting)
      }
    case .localNetwork:
      if facts.localNetwork {
        status("Local Network is on.", .done)
      } else if blocked {
        VStack(alignment: .leading, spacing: 10) {
          status("Turn on zflow in Privacy & Security → Local Network.", .problem)
          Button("Open Privacy & Security…") { model.openLocalNetwork() }
        }
      } else {
        status("Waiting for Local Network access…", .waiting)
      }
    case .computers:
      computers
    case .tryIt:
      tryIt
    }
  }

  @ViewBuilder private var computers: some View {
    let nearby = model.snapshot?.nearby ?? []
    let peers = model.snapshot?.peerNames ?? []
    VStack(alignment: .leading, spacing: 12) {
      if !peers.isEmpty {
        status("Paired with \(peers.formatted(.list(type: .and))).", .done)
      }
      if !nearby.isEmpty {
        ScrollView {
          VStack(spacing: 0) {
            ForEach(nearby) { computer in
              let address = computer.pairAddress
              HStack {
                Label(
                  "Computer · \(address.map(PairingView.host) ?? "unknown address")",
                  systemImage: "desktopcomputer")
                Spacer()
                if let address, computer.compatible {
                  Button("Pair…") { pairing = PairingTarget(address: address) }
                } else {
                  Text("Update zflow on it to pair").font(.callout).foregroundStyle(.secondary)
                }
              }
              .padding(.vertical, 6)
              if computer.id != nearby.last?.id { Divider() }
            }
          }
          .padding(.horizontal, 12)
        }
        .frame(maxHeight: 130)
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 8))
      } else if !searched {
        status("Looking for computers on this network…", .waiting)
      } else {
        Text(
          "Nothing found yet. Install zflow on your other computer; it shows up here once it runs."
        )
        .foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        HStack(alignment: .top) {
          Text(Self.installCommand).font(.system(.callout, design: .monospaced))
            .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
          Spacer(minLength: 8)
          Button(copied ? "Copied" : "Copy") {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(Self.installCommand, forType: .string)
            copied = true
          }
        }
        .padding(10)
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 8))
      }
      if searched || !nearby.isEmpty {
        Button("Enter an Address…") { pairing = PairingTarget(address: nil) }.buttonStyle(.link)
      }
    }
  }

  @ViewBuilder private var tryIt: some View {
    let (text, state) = sharingStatus
    VStack(alignment: .leading, spacing: 10) {
      status(text, state)
      if model.snapshot?.status.state == "paused" {
        Button("Resume Sharing") {
          model.send(CoreRequest(command: "set_sharing", enabled: true))
        }
      }
      Text("⌃⌘⌫ always brings input back to this Mac.").font(.callout).foregroundStyle(.secondary)
      Divider().padding(.vertical, 4)
      // A login item would point at the disk image or a temporary copy.
      Toggle("Open zflow at login", isOn: $openAtLogin).disabled(AppLocation.main.needsMove)
      VStack(alignment: .leading, spacing: 2) {
        Toggle("Reduce Wi-Fi lag", isOn: $reduceLag).disabled(!model.services.hasSigningTeam)
        Text(
          model.services.hasSigningTeam
            ? "Pauses AirDrop and Continuity while you control another computer."
            : "Needs a signed build of zflow."
        )
        .font(.callout).foregroundStyle(.secondary).padding(.leading, 20)
      }
    }
    .toggleStyle(.checkbox)
  }

  private var sharingStatus: (String, Status) {
    guard let snapshot = model.snapshot else { return ("Starting zflow…", .waiting) }
    let peer = newestPeer ?? snapshot.peerNames.first ?? "the other computer"
    if snapshot.status.state == "controlling" {
      return ("You're controlling \(snapshot.status.peer ?? peer). Move back to return.", .done)
    }
    if shared { return ("It works. Your pointer is back on this Mac.", .done) }
    switch snapshot.status.state {
    case "ready": return ("Ready when you are.", .waiting)
    case "checking": return ("Connecting to \(peer)…", .waiting)
    case "paused": return ("Sharing is paused.", .problem)
    default:
      if !snapshot.platform.accessibility { return ("Allow Accessibility first.", .problem) }
      let problem = snapshot.health.first { $0.level != "ok" }
      return (problem?.detail ?? snapshot.status.title, .problem)
    }
  }

  @ViewBuilder private func buttons(_ step: SetupStep) -> some View {
    HStack {
      if let previous = steps.last(where: { $0 < step }) {
        Button("Back") { self.step = previous }
      }
      Spacer()
      switch step {
      case .move:
        Button("Continue Anyway") {
          movedOn = true
          self.step = facts.next(after: .move)
        }
        if let movedCopy {
          Button("Open from Applications") { open(movedCopy) }.keyboardShortcut(.defaultAction)
        } else {
          Button("Show Applications") {
            NSWorkspace.shared.open(URL(fileURLWithPath: "/Applications"))
          }
          .keyboardShortcut(.defaultAction)
        }
      case .accessibility:
        if facts.accessibility {
          continueButton(after: step)
        } else {
          Button("Skip") { self.step = facts.next(after: step) }
          Button("Allow…") { model.openAccessibility() }.keyboardShortcut(.defaultAction)
        }
      case .localNetwork:
        if facts.localNetwork {
          continueButton(after: step)
        } else {
          Button("Skip") { self.step = facts.next(after: step) }
        }
      case .computers:
        if facts.done(.computers) { continueButton(after: step) }
      case .tryIt:
        Button("Done") { finish() }.keyboardShortcut(.defaultAction)
      }
    }
  }

  private func continueButton(after step: SetupStep) -> some View {
    Button("Continue") { self.step = facts.next(after: step) }.keyboardShortcut(.defaultAction)
  }

  private enum Status { case waiting, done, problem }

  private func status(_ text: String, _ state: Status) -> some View {
    HStack(spacing: 8) {
      switch state {
      case .waiting: ProgressView().controlSize(.small)
      case .done: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
      case .problem: Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
      }
      Text(text).fixedSize(horizontal: false, vertical: true)
    }
  }

  /// Polls what the showing step waits for, faster than the engine's own checks.
  private func watch(_ step: SetupStep) async {
    switch step {
    case .move:
      while !Task.isCancelled {
        movedCopy = AppLocation.movedCopy()
        try? await Task.sleep(for: .seconds(1))
      }
    case .accessibility:
      while !Task.isCancelled && !facts.accessibility {
        await model.perform(CoreRequest(command: "check_accessibility"), quiet: true)
        try? await Task.sleep(for: .milliseconds(500))
      }
    case .localNetwork:
      // The first check starts browsing, which makes macOS ask. The answer
      // looks blocked until the user responds, so wait before saying so.
      var checks = 0
      while !Task.isCancelled && !facts.localNetwork {
        await model.perform(CoreRequest(command: "discover"), quiet: true)
        checks = model.snapshot?.platform.localNetwork == "blocked" ? checks + 1 : 0
        blocked = checks > 4
        try? await Task.sleep(for: .seconds(1))
      }
    case .computers:
      // Also covers a skipped Local Network step.
      await model.perform(CoreRequest(command: "discover"), quiet: true)
      searched = false
      try? await Task.sleep(for: .seconds(4))
      if !Task.isCancelled { searched = true }
    case .tryIt:
      break
    }
  }

  private func bringToFront() {
    openWindow(id: "setup")
    NSApplication.shared.activate()
  }

  /// Opens the copy in Applications and quits this one. The copy has the
  /// same identifier, so macOS would otherwise just bring this one forward.
  private func open(_ copy: URL) {
    let configuration = NSWorkspace.OpenConfiguration()
    configuration.createsNewApplicationInstance = true
    Task {
      if (try? await NSWorkspace.shared.openApplication(at: copy, configuration: configuration))
        != nil
      {
        model.quit()
      }
    }
  }

  private func finish() {
    let login = openAtLogin
    let lag = reduceLag
    Task {
      if !AppLocation.main.needsMove && login != model.services.loginEnabled {
        await model.services.setLogin(login)
      }
      if lag {
        await model.perform(CoreRequest(command: "set_awdl", enabled: true))
        model.fixHelper()
      }
    }
    dismiss()
  }
}

/// The computer chosen for the pairing sheet; nil asks for an address.
struct PairingTarget: Identifiable {
  let id = UUID()
  var address: String?
}
