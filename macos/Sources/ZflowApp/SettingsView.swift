import SwiftUI

struct SettingsView: View {
  @Bindable var model: AppModel
  var body: some View {
    VStack(spacing: 0) {
      HStack {
        Text("zflow").font(.title2.weight(.semibold))
        Spacer()
        Button {
          model.showingHealth.toggle()
        } label: {
          Label(
            model.title,
            systemImage: model.healthy ? "checkmark.circle.fill" : "exclamationmark.circle.fill"
          )
          .foregroundStyle(model.healthy ? .green : .secondary)
        }
        .buttonStyle(.borderless)
        .popover(isPresented: $model.showingHealth, arrowEdge: .bottom) {
          HealthView(model: model).frame(width: 400).padding(20)
        }
      }.padding(.horizontal, 24).padding(.top, 22).padding(.bottom, 18)
      if let snapshot = model.snapshot {
        VStack(alignment: .leading, spacing: 12) {
          HStack {
            Text("Your computers").font(.headline)
            Spacer()
            Button("Pair Computer…", systemImage: "plus") { model.showingPairing = true }
          }
          ComputerLayout(
            computers: snapshot.layout.monitors,
            move: { computer, x, y, tolerance in
              model.send(
                CoreRequest(command: "move", id: computer.id, x: x, y: y, tolerance: tolerance))
            },
            forget: { name in
              model.send(CoreRequest(command: "forget", name: name))
            }
          )
          .frame(height: 235)
          Text(
            snapshot.peers.isEmpty
              ? "Pair another computer to share your keyboard and trackpad."
              : "Drag to match your desk. Touching edges let your pointer cross."
          )
          .font(.callout).foregroundStyle(.secondary)
        }.padding(.horizontal, 24)
        Form {
          Section {
            Toggle(
              "Block AWDL while sharing",
              isOn: Binding(
                get: { snapshot.blockAwdl },
                set: { model.send(CoreRequest(command: "set_awdl", enabled: $0)) }))
            Text(
              "Reduces Wi-Fi interference. AirDrop and Continuity may pause while you control another computer."
            )
            .font(.caption).foregroundStyle(.secondary)
            if snapshot.blockAwdl && !model.services.helperReady {
              LabeledContent(model.services.helperTitle) {
                Button(model.services.helperAction) { model.fixHelper() }
                  .disabled(model.services.helperBusy || !model.services.hasSigningTeam)
              }
              if !model.services.hasSigningTeam {
                Text(model.services.helperDetail).font(.caption).foregroundStyle(.secondary)
              }
            }
          }
          Section {
            Toggle(
              "Open at login",
              isOn: Binding(
                get: { model.services.loginEnabled },
                set: { enabled in Task { await model.services.setLogin(enabled) } }))
          }
        }.formStyle(.grouped).scrollDisabled(true)
          .frame(height: snapshot.blockAwdl && !model.services.helperReady ? 245 : 165)
      } else {
        ContentUnavailableView(
          "Settings could not load", systemImage: "exclamationmark.triangle",
          description: Text(model.error ?? "Starting zflow…")
        )
        .frame(height: 320)
        Button("Try Again") { model.send(CoreRequest(command: "reload")) }
      }
      if let message = model.error ?? model.services.error ?? model.snapshot?.configError {
        HStack(alignment: .top) {
          Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
          Text(message).font(.callout).textSelection(.enabled)
          Spacer(minLength: 0)
        }.padding(.horizontal, 24).padding(.bottom, 12)
      }
      Divider()
      HStack {
        Button("Open Configuration…") { model.openConfig() }.buttonStyle(.link)
        Spacer()
        Text("Changes save automatically").foregroundStyle(.tertiary).font(.caption)
      }.padding(.horizontal, 24).padding(.vertical, 16)
    }
    .frame(width: 600)
    .sheet(
      isPresented: $model.showingPairing,
      onDismiss: { model.send(CoreRequest(command: "pair_cancel")) },
      content: { PairingView(model: model) }
    )
  }
}

struct HealthView: View {
  @Bindable var model: AppModel
  var body: some View {
    VStack(alignment: .leading, spacing: 18) {
      Text(model.title).font(.title3.bold())
      if let state = model.snapshot {
        check(
          "Accessibility",
          detail: state.accessibility
            ? "Keyboard and pointer access allowed."
            : "Allow zflow to send keyboard and pointer input.", ready: state.accessibility
        ) {
          if !state.accessibility { Button("Allow…") { model.openAccessibility() } }
        }
        HStack(alignment: .top) {
          Image(systemName: "network").foregroundStyle(.secondary)
          VStack(alignment: .leading, spacing: 4) {
            Text("Local network").fontWeight(.medium)
            Text("Allow zflow if nearby computers do not appear.").font(.callout).foregroundStyle(
              .secondary)
          }
          Spacer()
          Button("Settings…") {
            NSWorkspace.shared.open(
              URL(
                string:
                  "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")!)
          }
        }
        if state.blockAwdl {
          check(
            model.services.helperTitle, detail: model.services.helperDetail,
            ready: model.services.helperReady
          ) {
            if !model.services.helperReady {
              Button(model.services.helperAction) { model.fixHelper() }
                .disabled(model.services.helperBusy || !model.services.hasSigningTeam)
            }
          }
        }
        check(
          "Receiver",
          detail: state.receiverError
            ?? (state.receiverChecked
              ? "Authenticated and checked before sharing was armed."
              : state.checking
                ? "Checking the paired receiver…"
                : "Pair a computer and allow access to check its receiver."),
          ready: state.receiverChecked
        ) {
          if !state.peers.isEmpty && !state.receiverChecked {
            Button("Retry") { model.send(CoreRequest(command: "retry")) }.disabled(state.checking)
          }
        }
        if let issue = state.layoutError ?? state.desktopError {
          Text(issue).font(.callout).foregroundStyle(.secondary)
        }
        if let issue = state.configError {
          Text(issue).font(.callout).foregroundStyle(.orange)
          Button("Open Configuration…") { model.openConfig() }
        }
        if let issue = state.discoveryError {
          Text("Discovery: \(issue)").font(.callout).foregroundStyle(.secondary)
        }
        Divider()
        Text(state.notice).font(.callout).foregroundStyle(.secondary)
        Text("Emergency return: ⌃⌘⌫").font(.caption)
      }
    }
  }
  func check<Action: View>(
    _ title: String, detail: String, ready: Bool, @ViewBuilder action: () -> Action
  ) -> some View {
    HStack(alignment: .top, spacing: 10) {
      Image(systemName: ready ? "checkmark.circle.fill" : "exclamationmark.circle").foregroundStyle(
        ready ? .green : .orange)
      VStack(alignment: .leading, spacing: 4) {
        Text(title).fontWeight(.medium)
        Text(detail).font(.callout).foregroundStyle(.secondary)
      }
      Spacer(minLength: 4)
      action()
    }
  }
}
