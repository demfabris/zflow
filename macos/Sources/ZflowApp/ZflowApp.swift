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
    (model.updates.availableVersion != nil
      ? Image(systemName: "arrow.down.circle")
      : Image(nsImage: model.healthy ? Mark.full : Mark.dimmed))
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

/// The zflow mark from assets/logo.svg, as a template image the menu bar tints
/// for light, dark and highlighted bars.
@MainActor
enum Mark {
  static let full = image(opacity: 1)
  /// While zflow needs the person.
  static let dimmed = image(opacity: 0.45)

  private static func image(opacity: Double) -> NSImage {
    let svg = """
      <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="166 149 640 640" fill-opacity="\(opacity)">
      <path d="M766 212C780 208 792 220 788 234L680 640C676 655 655 657 648 643L590 530L414 706C402 718 382 718 370 706L318 654C306 642 306 622 318 610L494 434L381 376C367 369 369 348 384 344Z"/>
      <path d="M188 474L274 388C283 379 297 379 306 388L324 406C333 415 333 429 324 438L238 524C229 533 215 533 206 524L188 506C179 497 179 483 188 474ZM204 670L234 640C243 631 257 631 266 640L284 658C293 667 293 681 284 690L254 720C245 729 231 729 222 720L204 702C195 693 195 679 204 670Z"/>
      </svg>
      """
    let image = NSImage(data: Data(svg.utf8)) ?? NSImage()
    image.isTemplate = true
    return image
  }
}
