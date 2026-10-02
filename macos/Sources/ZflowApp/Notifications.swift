import UserNotifications

/// Tells people when a computer joined, with a Forget button for one that
/// is not theirs. macOS asks for permission the first time one joins, not
/// at launch.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
  nonisolated static let category = "joined"
  nonisolated static let forget = "forget"
  private weak var model: AppModel?

  /// Takes the clicks on notifications. Called before launch finishes, so
  /// one that relaunched zflow still arrives.
  func listen() {
    let center = UNUserNotificationCenter.current()
    center.delegate = self
    let forget = UNNotificationAction(
      identifier: Self.forget, title: "Forget", options: [.destructive])
    center.setNotificationCategories([
      UNNotificationCategory(identifier: Self.category, actions: [forget], intentIdentifiers: [])
    ])
  }

  func attach(_ model: AppModel) {
    self.model = model
    model.joined = { [weak self] notice in self?.post(notice) }
  }

  private func post(_ notice: Notice) {
    let content = UNMutableNotificationContent()
    content.title = "\(notice.name) joined"
    content.body = "It can share this Mac’s keyboard and mouse. Not yours?"
    content.categoryIdentifier = Self.category
    content.userInfo = ["name": notice.name]
    let request = UNNotificationRequest(
      identifier: "joined-\(notice.id)", content: content, trigger: nil)
    Task {
      let center = UNUserNotificationCenter.current()
      guard (try? await center.requestAuthorization(options: [.alert, .sound])) == true else {
        return
      }
      try? await center.add(request)
    }
  }

  nonisolated func userNotificationCenter(
    _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse
  ) async {
    guard let name = response.notification.request.content.userInfo["name"] as? String else {
      return
    }
    let forget = response.actionIdentifier == Self.forget
    await MainActor.run {
      guard let model else { return }
      if forget {
        model.send(CoreRequest(command: "forget", name: name))
      } else {
        model.show(.computer(name))
      }
    }
  }

  /// Shown even while zflow is in front, since the person may be looking
  /// at another computer.
  nonisolated func userNotificationCenter(
    _ center: UNUserNotificationCenter, willPresent notification: UNNotification
  ) async -> UNNotificationPresentationOptions {
    [.banner, .list]
  }
}
