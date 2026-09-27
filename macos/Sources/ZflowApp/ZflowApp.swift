import AppKit
import SwiftUI

@main
struct ZflowApp: App {
  @State private var model = AppModel()
  @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
  var body: some Scene {
    MenuBarExtra {
      Text("zflow · \(model.title)")
      Divider()
      Button(model.snapshot?.sharing == true ? "Pause Sharing" : "Resume Sharing") {
        model.send(CoreRequest(command: "set_sharing", enabled: model.snapshot?.sharing != true))
      }.disabled(model.snapshot == nil)
      SetUpButton()
      SettingsLink { Text("Settings…") }.keyboardShortcut(",")
      Divider()
      Button("Quit zflow") { model.quit() }.keyboardShortcut("q").disabled(model.busy)
    } label: {
      MenuIcon(model: model).onAppear { delegate.model = model }
    }
    .menuBarExtraStyle(.menu)

    Settings {
      SettingsView(model: model)
    }
    .defaultSize(width: 600, height: 560)
    .windowResizability(.contentSize)

    Window("Set Up zflow", id: "setup") {
      OnboardingView(model: model)
    }
    .windowResizability(.contentSize)
    .defaultPosition(.center)
    .restorationBehavior(.disabled)
    .defaultLaunchBehavior(.suppressed)
  }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
  var model: AppModel?
  func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
    guard let model else { return .terminateNow }
    guard !model.busy else { return .terminateCancel }
    Task {
      await model.shutdown()
      sender.reply(toApplicationShouldTerminate: true)
    }
    return .terminateLater
  }
}

private struct SetUpButton: View {
  @Environment(\.openWindow) private var openWindow
  var body: some View {
    Button("Set Up…") {
      openWindow(id: "setup")
      NSApplication.shared.activate()
    }
  }
}

private struct MenuIcon: View {
  var model: AppModel
  @Environment(\.openSettings) private var openSettings
  @Environment(\.openWindow) private var openWindow
  var body: some View {
    Image(systemName: model.healthy ? "computermouse" : "computermouse.fill")
      .accessibilityLabel("zflow, \(model.title)")
      .task {
        // The engine's first answer says whether any computer is paired.
        let deadline = ContinuousClock.now + .seconds(5)
        while model.snapshot == nil && model.error == nil && ContinuousClock.now < deadline {
          try? await Task.sleep(for: .milliseconds(50))
        }
        if model.snapshot?.peers.isEmpty == true {
          openWindow(id: "setup")
          NSApplication.shared.activate()
        } else if model.showSettingsAtLaunch || model.snapshot == nil {
          NSApplication.shared.activate()
          openSettings()
        }
      }
  }
}
