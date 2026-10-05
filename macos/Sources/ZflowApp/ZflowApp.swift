import AppKit
import SwiftUI

@main
struct ZflowApp: App {
  @State private var model = AppModel()
  @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
  var body: some Scene {
    MenuBarExtra {
      MenuBarView(model: model)
    } label: {
      MenuIcon(model: model, delegate: delegate)
    }
    .menuBarExtraStyle(.window)

    Window("zflow", id: "main") {
      MainView(model: model)
    }
    .defaultSize(width: 960, height: 660)
    .restorationBehavior(.disabled)
    .defaultLaunchBehavior(.suppressed)
    .commands {
      CommandGroup(after: .appInfo) {
        Button("Check for Updates…") { model.updates.check() }
          .disabled(!model.updates.canCheck)
      }
      // Settings is a page of the window, not a window of its own.
      CommandGroup(replacing: .appSettings) {
        Button("Settings…") { model.show(.settings) }.keyboardShortcut(",")
        Button("Open Configuration…") { model.openConfig() }
      }
    }
  }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
  var model: AppModel? {
    didSet { if let model { notifier.attach(model) } }
  }
  let notifier = Notifier()
  /// zflow lives in the menu bar; the window brings the Dock icon along.
  func applicationWillFinishLaunching(_ notification: Notification) {
    NSApplication.shared.setActivationPolicy(.accessory)
    notifier.listen()
  }
  /// Opening zflow again while it runs, as from Applications, shows the window.
  func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
    model?.show()
    return false
  }
  func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
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

/// The menu bar icon, which is there from launch to quit, so it also opens
/// the window when zflow starts and something needs the person.
private struct MenuIcon: View {
  var model: AppModel
  var delegate: AppDelegate
  @Environment(\.openWindow) private var openWindow
  var body: some View {
    Image(
      systemName: model.updates.availableVersion != nil
        ? "arrow.down.circle" : (model.healthy ? "computermouse" : "computermouse.fill")
    )
      .accessibilityLabel(
        model.updates.availableVersion != nil ? "zflow, update available" : "zflow, \(model.title)")
      .task {
        delegate.model = model
        model.openWindow = openWindow
        // The engine's first answer says whether any computer is paired.
        let deadline = ContinuousClock.now + .seconds(5)
        while model.snapshot == nil && model.error == nil && ContinuousClock.now < deadline {
          try? await Task.sleep(for: .milliseconds(50))
        }
        if model.showSettingsAtLaunch {
          model.show(.settings)
        } else if model.snapshot?.peers.isEmpty != false || model.askToMove {
          model.show()
        }
      }
  }
}
