import Foundation
import Testing

@testable import ZflowApp

private var installed: AppLocation {
  AppLocation(
    path: "/Applications/zflow.app", quarantined: false, readOnly: false, home: "/Users/me")
}
private let updateInfo: [String: String] = [
  "SUPublicEDKey": Data(repeating: 7, count: 32).base64EncodedString(),
  "SUFeedURL": "https://github.com/demfabris/zflow/releases/latest/download/appcast.xml",
]

@Test func updateChecksRequireReleaseConfiguration() {
  #expect(Updates.configurationIssue(info: [:], location: installed) != nil)
  #expect(Updates.configurationIssue(info: updateInfo, location: installed) == nil)
  for key in ["", "invalid", Data(repeating: 7, count: 31).base64EncodedString()] {
    var info = updateInfo
    info["SUPublicEDKey"] = key
    #expect(Updates.configurationIssue(info: info, location: installed) != nil)
  }
  for feed in ["", "http://example.com/appcast.xml", "file:///tmp/appcast.xml"] {
    var info = updateInfo
    info["SUFeedURL"] = feed
    #expect(Updates.configurationIssue(info: info, location: installed) != nil)
  }
}

@Test func updatesWaitUntilTheAppMovesOffItsDiskImage() {
  let image = AppLocation(
    path: "/Volumes/zflow/zflow.app", quarantined: true, readOnly: true, home: "/Users/me")
  #expect(Updates.configurationIssue(info: updateInfo, location: image)?.contains("Applications") == true)
}
