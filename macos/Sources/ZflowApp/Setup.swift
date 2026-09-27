import Foundation

/// Where zflow runs from. Gatekeeper runs a quarantined app from a temporary
/// read-only copy until Finder moves it, and a disk image goes away when it
/// is ejected, so both need a move to Applications first.
struct AppLocation: Equatable {
  var path: String
  var quarantined: Bool
  var readOnly: Bool
  var home: String

  /// Where this process runs from, which can't change while it runs.
  static let main = AppLocation(bundle: .main)

  /// Local builds under target/ carry no quarantine flag, so they stay put.
  var needsMove: Bool {
    if path.contains("/AppTranslocation/") { return true }
    if path.hasPrefix("/Volumes/") && readOnly { return true }
    let folders = ["/Applications/", home + "/Applications/"]
    return quarantined && !folders.contains { path.hasPrefix($0) }
  }

  /// The copy the user dragged to Applications, when it is this same version.
  static func movedCopy(of bundle: Bundle = .main) -> URL? {
    let home = FileManager.default.homeDirectoryForCurrentUser
    let folders = [
      URL(fileURLWithPath: "/Applications"), home.appendingPathComponent("Applications"),
    ]
    let version = bundle.infoDictionary?["CFBundleVersion"] as? String
    for folder in folders {
      let copy = folder.appendingPathComponent(bundle.bundleURL.lastPathComponent)
      // Bundle(url:) caches, and the user may replace an older copy.
      let info = NSDictionary(contentsOf: copy.appendingPathComponent("Contents/Info.plist"))
      if copy.standardizedFileURL != bundle.bundleURL.standardizedFileURL,
        info?["CFBundleIdentifier"] as? String == bundle.bundleIdentifier,
        info?["CFBundleVersion"] as? String == version
      {
        return copy
      }
    }
    return nil
  }
}

extension AppLocation {
  init(bundle: Bundle) {
    let url = bundle.bundleURL
    let values = try? url.resourceValues(forKeys: [.volumeIsReadOnlyKey])
    self.init(
      path: url.path,
      quarantined: getxattr(url.path, "com.apple.quarantine", nil, 0, 0, 0) >= 0,
      readOnly: values?.volumeIsReadOnly ?? false,
      home: FileManager.default.homeDirectoryForCurrentUser.path)
  }
}

enum SetupStep: Int, CaseIterable, Comparable {
  case move, accessibility, localNetwork, computers, tryIt

  static func < (lhs: SetupStep, rhs: SetupStep) -> Bool { lhs.rawValue < rhs.rawValue }

  var title: String {
    switch self {
    case .move: "Move"
    case .accessibility: "Accessibility"
    case .localNetwork: "Local Network"
    case .computers: "Pair"
    case .tryIt: "Try It"
    }
  }
}

/// What setup knows about each step's condition.
struct SetupFacts: Equatable {
  var needsMove = false
  var accessibility = false
  var localNetwork = false
  var peers = 0

  func done(_ step: SetupStep) -> Bool {
    switch step {
    case .move: !needsMove
    case .accessibility: accessibility
    case .localNetwork: localNetwork
    case .computers: peers > 0
    case .tryIt: false
    }
  }

  /// The first step that still needs the user.
  var start: SetupStep { SetupStep.allCases.first { !done($0) } ?? .tryIt }

  /// The next step after `step` that still needs the user.
  func next(after step: SetupStep) -> SetupStep {
    SetupStep.allCases.first { $0 > step && !done($0) } ?? .tryIt
  }

  /// Moves on when the showing step's condition turns true. Pairing counts
  /// computers, so pairing another one later also moves on.
  func advance(_ step: SetupStep, from old: SetupFacts) -> SetupStep {
    let finished = step == .computers ? peers > old.peers : done(step) && !old.done(step)
    return finished ? next(after: step) : step
  }
}

extension Snapshot {
  /// Which edge of this Mac leads to `peer`, from the saved layout.
  func edge(toward peer: String?) -> String? {
    let monitors = layout.monitors
    guard let mac = monitors.first(where: { $0.peer == nil }),
      let other = monitors.first(where: { $0.peer != nil && (peer == nil || $0.peer == peer) })
    else { return nil }
    if other.x >= mac.x + mac.width { return "right" }
    if other.x + other.width <= mac.x { return "left" }
    if other.y + other.height <= mac.y { return "top" }
    if other.y >= mac.y + mac.height { return "bottom" }
    return nil
  }
}
