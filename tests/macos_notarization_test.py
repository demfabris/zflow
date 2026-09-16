"""Check release gates without submitting software to Apple."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/notarize-macos-app.sh"
MOCK_TOOL = '''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps([name, *args]) + "\\n")
mode = os.environ["MODE"]
if name == "codesign" and mode == "bad-signature":
    sys.exit(1)
if name == "ditto":
    Path(args[-1]).write_bytes(b"archive fixture")
if name == "xcrun" and args[:2] == ["notarytool", "submit"]:
    print(json.dumps({"id": "test-submission"}))
if name == "xcrun" and args[:2] == ["notarytool", "wait"]:
    if mode == "timeout":
        sys.exit(1)
    print(json.dumps({"status": "Invalid" if mode == "rejected" else "Accepted"}))
if name == "xcrun" and args[:2] == ["notarytool", "log"]:
    Path(args[-1]).write_text(json.dumps({"issues": [{"message": "Rejected fixture"}] if mode == "rejected" else None}))
if name == "xcrun" and args[:2] == ["stapler", "validate"] and mode == "bad-ticket":
    sys.exit(1)
'''


class NotarizationTest(unittest.TestCase):
    def run_notarization(self, mode):
        with tempfile.TemporaryDirectory(prefix="zflow notary test ") as temp:
            root = Path(temp)
            (root / "scripts").mkdir()
            script = root / "scripts/notarize-macos-app.sh"
            shutil.copyfile(SCRIPT, script)
            (root / "target/release/zflow.app").mkdir(parents=True)
            tools = root / "bin"
            tools.mkdir()
            for name in ("codesign", "ditto", "xcrun", "spctl"):
                tool = tools / name
                tool.write_text(MOCK_TOOL)
                tool.chmod(0o755)
            calls = root / "calls.jsonl"
            result = subprocess.run(
                [os.environ.get("TEST_BASH", "bash"), str(script), "test-profile", str(root / "test.keychain-db")],
                env={**os.environ, "PATH": f"{tools}:{os.environ['PATH']}", "CALLS": str(calls), "MODE": mode},
                capture_output=True, text=True,
            )
            commands = [json.loads(line) for line in calls.read_text().splitlines()]
            submission = root / "target/notarization/submission.json"
            return result, commands, json.loads(submission.read_text()) if submission.exists() else None

    def test_accepted_app_requires_ticket_and_gatekeeper_checks(self):
        result, calls, _ = self.run_notarization("accepted")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(any(call[:3] == ["xcrun", "stapler", "staple"] for call in calls))
        self.assertTrue(any(call[:3] == ["xcrun", "stapler", "validate"] for call in calls))
        self.assertEqual(calls[-1][0], "spctl")

    def test_rejected_submission_stops_before_stapling(self):
        result, calls, _ = self.run_notarization("rejected")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Rejected fixture", result.stderr)
        self.assertFalse(any(call[:2] == ["xcrun", "stapler"] for call in calls))

    def test_timeout_retains_submission_id_for_followup(self):
        result, calls, submission = self.run_notarization("timeout")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(submission["id"], "test-submission")
        self.assertFalse(any(call[:2] == ["xcrun", "stapler"] for call in calls))

    def test_invalid_signature_stops_before_upload(self):
        result, calls, submission = self.run_notarization("bad-signature")
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(submission)
        self.assertEqual([call[0] for call in calls], ["codesign"])

    def test_invalid_ticket_fails_release(self):
        result, _, _ = self.run_notarization("bad-ticket")
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
