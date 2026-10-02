import Foundation
import Testing

@testable import ZflowApp

private func location(_ path: String, quarantined: Bool = false, readOnly: Bool = false)
  -> AppLocation
{
  AppLocation(path: path, quarantined: quarantined, readOnly: readOnly, home: "/Users/me")
}

@Test func localBuildsAndInstalledCopiesStayPut() {
  for path in [
    "/Users/me/dev/zflow/target/debug/zflow.app",
    "/Users/me/dev/zflow/target/release/zflow.app",
    "/Volumes/Work/zflow/target/debug/zflow.app",
    "/Users/me/Downloads/zflow.app",
  ] {
    #expect(!location(path).needsMove, "\(path)")
  }
  for path in ["/Applications/zflow.app", "/Users/me/Applications/zflow.app"] {
    #expect(!location(path, quarantined: true).needsMove, "\(path)")
  }
}

@Test func diskImagesTranslocationAndQuarantinedCopiesMove() {
  #expect(location("/Volumes/zflow/zflow.app", readOnly: true).needsMove)
  #expect(
    location("/private/var/folders/xy/abc/T/AppTranslocation/1234-ABCD/d/zflow.app", readOnly: true)
      .needsMove)
  #expect(location("/Users/me/Downloads/zflow.app", quarantined: true).needsMove)
  #expect(location("/Applications Old/zflow.app", quarantined: true).needsMove)
}
