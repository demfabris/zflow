"""Exercise update publication and nested signing without Apple services."""

import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PUBLIC = base64.b64encode(bytes(range(32))).decode()
PRIVATE = "private-key-fixture-never-print"
MOCK = r'''#!/usr/bin/env python3
import base64
import json
import os
from pathlib import Path
import stat
import sys
import xml.etree.ElementTree as ET

name = Path(sys.argv[0]).name
args = sys.argv[1:]
mode = os.environ.get("MODE", "ok")
entry = [name, *args]
if name == "uname":
    print("Darwin")
if name == "generate_appcast":
    if sys.stdin.read().strip() != os.environ["SPARKLE_PRIVATE_ED_KEY"]:
        sys.exit("Key was not supplied on stdin")
    if mode == "generator-failure":
        sys.exit(1)
    image = next(Path(args[-1]).glob("*.dmg"))
    ns = "http://www.andymatuschak.org/xml-namespaces/sparkle"
    feed = ET.Element("rss")
    item = ET.SubElement(ET.SubElement(feed, "channel"), "item")
    ET.SubElement(item, "{" + ns + "}version").text = "9.9.9" if mode == "wrong-version" else "1.2.3"
    prefix = args[args.index("--download-url-prefix") + 1]
    attributes = {
        "url": "https://example.com/wrong.dmg" if mode == "wrong-url" else prefix + image.name,
        "length": "0" if mode == "wrong-size" else str(image.stat().st_size),
    }
    if mode != "unsigned":
        attributes["{" + ns + "}edSignature"] = base64.b64encode(bytes(64)).decode()
    ET.SubElement(item, "enclosure", attributes)
    ET.ElementTree(feed).write(args[args.index("-o") + 1])
if name == "codesign" and mode == "bad-signature":
    sys.exit(1)
if name == "xcrun" and mode == "bad-ticket":
    sys.exit(1)
if name == "gh":
    if args[:2] == ["variable", "list"]:
        print(os.environ.get("EXISTING_PUBLIC", ""))
    if args[:2] == ["secret", "set"]:
        if sys.stdin.read() != os.environ["SPARKLE_PRIVATE_ED_KEY"]:
            sys.exit("Private key was not supplied via stdin")
        if stat.S_IMODE(os.fstat(sys.stdin.fileno()).st_mode) != 0o600:
            sys.exit("Private key file must have mode 0600")
    if (args[:2] == ["secret", "set"] and mode == "secret-failure") or (args[:2] == ["variable", "set"] and mode == "variable-failure"):
        sys.exit(1)
if name == "generate_keys":
    if "-p" in args:
        if mode == "missing-local-key":
            sys.exit(1)
        print(os.environ["SPARKLE_PUBLIC_ED_KEY"])
    if "-x" in args:
        private_file = Path(args[args.index("-x") + 1])
        private_file.write_text(os.environ["SPARKLE_PRIVATE_ED_KEY"])
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(entry) + "\n")
'''


class MacOSUpdatesTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zflow mac updates ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        for script in ("macos-update-feed.sh", "sign-macos-app.sh", "setup-macos-updates.sh", "build-macos-app.sh"):
            shutil.copyfile(ROOT / "scripts" / script, self.root / "scripts" / script)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.sparkle_bin = self.root / "macos/.build/artifacts/sparkle/Sparkle/bin"
        self.sparkle_bin.mkdir(parents=True)
        for name in ("codesign", "xcrun", "uname", "gh", "swift", "cargo"):
            self.tool(self.bin / name)
        for name in ("generate_appcast", "generate_keys"):
            self.tool(self.sparkle_bin / name)
        self.calls = self.root / "calls.jsonl"
        self.dmg = self.root / "zflow-v1.2.3-macos.dmg"
        self.dmg.write_bytes(b"final stapled disk image fixture")
        self.output = self.root / "appcast.xml"
        self.app = self.root / "zflow.app"
        (self.app / "Contents/Frameworks/Sparkle.framework").mkdir(parents=True)
        self.environment = {
            **os.environ,
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "CALLS": str(self.calls),
            "SPARKLE_PUBLIC_ED_KEY": PUBLIC,
            "SPARKLE_PRIVATE_ED_KEY": PRIVATE,
            "TMPDIR": str(self.root),
        }

    def tool(self, path):
        path.write_text(MOCK)
        path.chmod(0o755)

    def run_script(self, name, *args, mode="ok", extra=None):
        self.calls.unlink(missing_ok=True)
        result = subprocess.run(
            [os.environ.get("TEST_BASH", "bash"), str(self.root / "scripts" / name), *map(str, args)],
            env={**self.environment, "MODE": mode, **(extra or {})}, capture_output=True, text=True,
        )
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        self.assertNotIn(PRIVATE, result.stdout + result.stderr + json.dumps(calls))
        return result, calls

    def feed(self, **kwargs):
        return self.run_script("macos-update-feed.sh", self.dmg, "1.2.3", self.output, **kwargs)

    def test_feed_signs_final_image_and_uses_versioned_download(self):
        result, calls = self.feed()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(self.output.is_file())
        self.assertEqual([call[0] for call in calls], ["codesign", "xcrun", "generate_appcast"])
        self.assertIn("https://github.com/demfabris/zflow/releases/download/v1.2.3/", calls[-1])
        self.assertIn("--ed-key-file", calls[-1])
        self.assertFalse(list(self.root.glob(".appcast.*")))

    def test_invalid_feed_never_replaces_previous_output(self):
        for mode in ("unsigned", "wrong-version", "wrong-url", "wrong-size", "generator-failure"):
            with self.subTest(mode=mode):
                self.output.write_text("previous feed")
                result, _ = self.feed(mode=mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.output.read_text(), "previous feed")
                self.assertFalse(list(self.root.glob(".appcast.*")))

    def test_signing_and_notarization_failures_stop_before_generating(self):
        for mode in ("bad-signature", "bad-ticket"):
            with self.subTest(mode=mode):
                result, calls = self.feed(mode=mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("generate_appcast", [call[0] for call in calls])

    def test_unsigned_configuration_stops_before_platform_tools(self):
        result, calls = self.feed(extra={"SPARKLE_PRIVATE_ED_KEY": ""})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])

    def test_release_build_requires_valid_public_key_before_compilation(self):
        for public in ("", "invalid", base64.b64encode(bytes(31)).decode()):
            with self.subTest(public=public):
                result, calls = self.run_script("build-macos-app.sh", "--updates", extra={"SPARKLE_PUBLIC_ED_KEY": public})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("SPARKLE_PUBLIC_ED_KEY", result.stderr)
                self.assertNotIn("cargo", [call[0] for call in calls])

    def test_nested_code_is_signed_inside_out_with_downloader_entitlements(self):
        result, calls = self.run_script("sign-macos-app.sh", "Developer ID fixture", self.app)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        targets = [Path(call[-1]).name for call in calls]
        self.assertEqual(targets, ["Installer.xpc", "Downloader.xpc", "Autoupdate", "Updater.app", "Sparkle.framework", "zflow-awdl-daemon", "zflow.app", "zflow.app"])
        self.assertIn("--preserve-metadata=entitlements", calls[1])
        for call in calls[:-1]:
            self.assertIn("runtime", call)
            self.assertIn("--timestamp", call)
            self.assertNotIn("--deep", call)
        self.assertEqual(calls[-1][1:4], ["--verify", "--strict", "--deep"])

    def test_ad_hoc_local_build_does_not_enforce_library_validation(self):
        result, calls = self.run_script("sign-macos-app.sh", "-", self.app)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for call in calls[:-1]:
            self.assertNotIn("runtime", call)
            self.assertIn("--timestamp=none", call)
            self.assertIn("0", call)

    def test_setup_uses_keychain_and_private_stdin_then_removes_export(self):
        result, calls = self.run_script("setup-macos-updates.sh")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(any(call[:3] == ["gh", "secret", "set"] for call in calls))
        self.assertTrue(any(call[:3] == ["gh", "variable", "set"] for call in calls))
        self.assertFalse(list(self.root.glob("zflow-update-key.*")))

    def test_setup_cannot_replace_existing_release_identity(self):
        for mode, public in (("ok", "another-public-key"), ("missing-local-key", PUBLIC)):
            with self.subTest(mode=mode):
                result, calls = self.run_script("setup-macos-updates.sh", mode=mode, extra={"EXISTING_PUBLIC": public})
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(call[0] == "gh" and "set" in call for call in calls))

    def test_setup_removes_private_export_when_github_configuration_fails(self):
        for mode in ("secret-failure", "variable-failure"):
            with self.subTest(mode=mode):
                result, _ = self.run_script("setup-macos-updates.sh", mode=mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(list(self.root.glob("zflow-update-key.*")))


if __name__ == "__main__":
    unittest.main()
