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

private struct MenuIcon: View {
  var model: AppModel
  @Environment(\.openSettings) private var openSettings
  var body: some View {
    Image(systemName: model.healthy ? "computermouse" : "computermouse.fill")
      .accessibilityLabel("zflow, \(model.title)")
      .task {
        if model.showSettingsAtLaunch {
          NSApplication.shared.activate()
          openSettings()
        }
      }
  }
}
