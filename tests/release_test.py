"""Exercise release pushes against a disposable bare repo, with GitHub mocked."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/release.sh"
GIT = shutil.which("git")
MOCK_TOOL = r'''
import json
import os
from pathlib import Path
import subprocess
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
if name == 'git':
    if args == ['remote', 'get-url', '--push', 'origin']:
        print(os.environ.get('PUSH_URL', 'git@github.com:demfabris/zflow.git'))
        sys.exit(0)
    os.execv(os.environ['REAL_GIT'], ['git', *args])
with open(os.environ['CALLS'], 'a') as log:
    log.write(json.dumps(args) + '\n')
if args[:2] == ['repo', 'view']:
    print(os.environ.get('RELEASE_REPO', 'demfabris/zflow'))
elif args[:2] == ['release', 'view']:
    print('https://github.com/demfabris/zflow/releases/tag/v0.5.0' if args[2].startswith('v') else os.environ.get('LATEST', 'v0.4.0'))
elif args[:2] == ['variable', 'list']:
    print('public-key-fixture')
elif args[:2] == ['secret', 'list']:
    print('' if os.environ.get('MISSING_SIGNING_KEY') else 'SPARKLE_PRIVATE_ED_KEY')
elif args[:2] == ['run', 'list']:
    assert '--commit' in args and '--event' in args and '--branch' in args
    print('123' if args[args.index('--workflow') + 1] == 'ci.yml' else '456')
elif args[:2] == ['run', 'watch']:
    if args[2] == os.environ.get('FAIL_RUN'):
        sys.exit(1)
    if args[2] == '123' and os.environ.get('CHANGE_CHECKOUT'):
        Path('changed-while-waiting').touch()
    if args[2] == '123' and os.environ.get('CHANGE_TAG'):
        subprocess.run([os.environ['REAL_GIT'], 'tag', '-fa', 'v0.5.0', 'HEAD~1', '-m', 'Concurrent tag change'], check=True)
else:
    sys.exit('Unexpected GitHub command: ' + repr(args))
'''


class ReleaseTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zflow release test ")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "checkout"
        self.remote = self.base / "origin.git"
        self.root.mkdir()
        self.git("init", "--initial-branch=main")
        self.git("config", "user.name", "release-goblin")
        self.git("config", "user.email", "test@example.invalid")
        subprocess.run([GIT, "init", "--bare", str(self.remote)], check=True, capture_output=True)
        (self.root / "scripts").mkdir()
        shutil.copyfile(SCRIPT, self.root / "scripts/release.sh")
        (self.root / "Cargo.toml").write_text('[package]\nname = "zflow-kvm"\nversion = "0.4.0"\n')
        self.git("add", ".")
        self.git("commit", "-m", "Previous release")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "main")
        self.old_head = self.git("rev-parse", "HEAD").stdout.strip()
        (self.root / "Cargo.toml").write_text('[package]\nname = "zflow-kvm"\nversion = "0.5.0"\n')
        self.git("commit", "-am", "Prepare release")
        self.head = self.git("rev-parse", "HEAD").stdout.strip()
        tools = self.base / "tools"
        tools.mkdir()
        for name in ("gh", "git"):
            path = tools / name
            path.write_text(f"#!{sys.executable}\n" + MOCK_TOOL)
            path.chmod(0o755)
        self.calls = self.base / "calls.jsonl"
        self.env = {**os.environ, "PATH": f"{tools}:{os.environ['PATH']}", "CALLS": str(self.calls), "REAL_GIT": GIT}

    def git(self, *args, check=True):
        return subprocess.run([GIT, *args], cwd=self.root, capture_output=True, text=True, check=check)

    def release(self, *args, **env):
        return subprocess.run(
            [os.environ.get("TEST_BASH", "bash"), "scripts/release.sh", *args],
            cwd=self.root, env={**self.env, **env}, capture_output=True, text=True,
        )

    def remote_ref(self, ref):
        return self.git("ls-remote", "origin", ref).stdout.split()[0] if self.git("ls-remote", "origin", ref).stdout else None

    def github_calls(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []

    def test_dry_run_does_not_push_or_create_a_tag(self):
        result = self.release("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.old_head)
        self.assertEqual(self.git("tag", "--list").stdout, "")
        self.assertFalse(any(call[:1] == ["run"] for call in self.github_calls()))

    def test_release_pushes_exact_commit_and_annotated_tag_after_ci(self):
        result = self.release()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.head)
        self.assertEqual(self.remote_ref("refs/tags/v0.5.0^{}"), self.head)
        self.assertEqual(self.git("cat-file", "-t", "v0.5.0").stdout.strip(), "tag")
        self.assertEqual([call[2] for call in self.github_calls() if call[:2] == ["run", "watch"]], ["123", "456"])

    def test_dirty_tree_stops_before_contacting_github(self):
        (self.root / "untracked").touch()
        result = self.release()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("working-tree changes", result.stderr)
        self.assertEqual(self.github_calls(), [])

    def test_wrong_branch_is_rejected(self):
        self.git("switch", "-c", "feature")
        self.assertNotEqual(self.release().returncode, 0)
        self.assertIsNone(self.remote_ref("refs/tags/v0.5.0"))

    def test_failed_ci_pushes_no_tag(self):
        self.assertNotEqual(self.release(FAIL_RUN="123").returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.head)
        self.assertIsNone(self.remote_ref("refs/tags/v0.5.0"))
        self.assertEqual(self.git("tag", "--list").stdout, "")

    def test_checkout_changed_while_ci_ran_is_not_tagged(self):
        result = self.release(CHANGE_CHECKOUT="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checkout changed", result.stderr)
        self.assertIsNone(self.remote_ref("refs/tags/v0.5.0"))

    def test_no_wait_still_requires_ci(self):
        self.assertEqual(self.release("--no-wait").returncode, 0)
        self.assertEqual([call[2] for call in self.github_calls() if call[:2] == ["run", "watch"]], ["123"])

    def test_tag_changed_while_ci_ran_is_not_pushed(self):
        result = self.release(CHANGE_TAG="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release tag changed", result.stderr)
        self.assertIsNone(self.remote_ref("refs/tags/v0.5.0"))

    def test_failed_release_is_reported_without_retagging(self):
        self.assertNotEqual(self.release(FAIL_RUN="456").returncode, 0)
        self.assertEqual(self.remote_ref("refs/tags/v0.5.0^{}"), self.head)
        retry = self.release()
        self.assertNotEqual(retry.returncode, 0)
        self.assertIn("already exists on GitHub", retry.stderr)

    def test_conflicting_local_tag_is_rejected(self):
        self.git("tag", "-a", "v0.5.0", self.old_head, "-m", "Wrong commit")
        self.assertNotEqual(self.release().returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.old_head)

    def test_matching_local_tag_can_be_pushed_after_interruption(self):
        self.git("tag", "-a", "v0.5.0", "-m", "Prepared tag")
        self.assertEqual(self.release().returncode, 0)
        self.assertEqual(self.remote_ref("refs/tags/v0.5.0^{}"), self.head)

    def test_older_or_equal_version_is_rejected(self):
        for latest in ("v0.5.0", "v0.6.0", "v1.0.0"):
            with self.subTest(latest=latest):
                self.assertNotEqual(self.release(LATEST=latest).returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.old_head)

    def test_unexpected_repository_or_push_destination_is_rejected(self):
        for env in ({"RELEASE_REPO": "someone/fork"}, {"PUSH_URL": "git@github.com:someone/fork.git"}):
            with self.subTest(env=env):
                self.assertNotEqual(self.release(**env).returncode, 0)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.old_head)

    def test_missing_update_signing_key_stops_before_push(self):
        result = self.release(MISSING_SIGNING_KEY="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("just release setup", result.stderr)
        self.assertEqual(self.remote_ref("refs/heads/main"), self.old_head)


if __name__ == "__main__":
    unittest.main()
