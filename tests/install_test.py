"""Test release selection and verification without installing on the host."""

import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
BASH = os.environ.get("TEST_BASH", "bash")

# Only host commands are replaced; checksums, archive extraction and files are real.
HOST_COMMANDS = r'''
record() { printf '%s\n' "$*" >> "$LOG"; }
uname() { if [[ "$1" == -s ]]; then echo "$PLATFORM"; else echo "$ARCH"; fi; }
id() { echo "${MOCK_UID:-1000}"; }
command() {
    if [[ "$1" == -v ]]; then
        case "$2" in
            apt-get|dnf|pacman) [[ "$2" == "$MANAGER" ]]; return ;;
        esac
    fi
    builtin command "$@"
}
as_root() { record "root $*"; }
fetch() { record "fetch $1"; cp "$RELEASE/${1##*/}" "$2"; }
curl() { record latest; echo "${LATEST_URL:-https://github.com/demfabris/zflow/releases/tag/v0.1.0}"; }
archive_install_present() { [[ "$LEGACY" == true ]]; }
dpkg-query() { echo "${DPKG_STATUS:-unknown}"; }
getconf() { echo "${LIBC:-glibc 2.39}"; }
systemctl() { echo "${SYSTEMD_VERSION-259}"; }
udevadm() { echo verify; }
gjs() { echo "${GTK_VERSIONS:-4.12 1.5}"; }
gnome-extensions() { record "extension $*"; }
for tool in getent groupadd useradd runuser setfacl modprobe systemd-analyze; do
    eval "$tool() { :; }"
done
for tool in cargo rustup rustc swift xcrun xcode-select cc make; do
    eval "$tool() { record forbidden-toolchain; return 99; }"
done
function /usr/local/bin/zflow() {
    record "user installed-zflow $*"
    echo "${DESKTOP_OUTPUT:-Desktop installed}"
    return "${DESKTOP_STATUS:-0}"
}
function /usr/bin/zflow() { record "user deb-zflow $*"; }
sw_vers() { echo "${MACOS_VERSION:-26.0}"; }
sysctl() { echo "${ROSETTA:-0}"; }
codesign() { record "codesign $*"; return "${SIGNATURE_STATUS:-0}"; }
ditto() { cp -R "$1" "$2"; }
pgrep() { return 1; }
open() { record "open $*"; }
function /usr/libexec/PlistBuddy() { echo io.zflow.zflow; }
'''


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zflow-installer-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "payload with spaces"
        (self.source / "scripts").mkdir(parents=True)
        (self.source / "bin").mkdir()
        (self.source / "scripts/install.sh").write_text("exit 99\n")
        for name in ("zflow", "zflowd"):
            binary = self.source / "bin" / name
            binary.write_text('#!/bin/sh\nprintf "zflow 0.1.0\\n"\n')
            binary.chmod(0o755)
        (self.source / "zflow.app/Contents").mkdir(parents=True)
        (self.source / "zflow.app/Contents/version").write_text("new")
        self.release = self.root / "release"
        self.release.mkdir()
        for architecture in ("x86_64", "aarch64"):
            for platform in ("unknown-linux-gnu", "apple-darwin"):
                path = self.release / f"zflow-v0.1.0-{architecture}-{platform}.tar.gz"
                with tarfile.open(path, "w:gz") as archive:
                    archive.add(self.source, arcname="zflow-release")
        for architecture in ("amd64", "arm64"):
            (self.release / f"zflow_0.1.0_{architecture}.deb").write_bytes(b"package fixture")
        self.write_checksums()
        self.log = self.root / "calls"
        (self.root / "home").mkdir()
        (self.root / "tmp").mkdir()
        self.env = {
            **os.environ,
            "HOME": str(self.root / "home"), "TMPDIR": str(self.root / "tmp"),
            "LOG": str(self.log), "FIXTURE": str(self.source),
            "RELEASE": str(self.release), "TEST_ROOT": str(self.root),
            "PLATFORM": "Linux", "ARCH": "x86_64", "MANAGER": "apt-get", "LEGACY": "true",
            "XDG_CURRENT_DESKTOP": "ubuntu:GNOME", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/test-session",
            "DISPLAY": ":test",
        }

    def write_checksums(self):
        lines = [f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n"
                 for p in self.release.iterdir() if p.name != "SHA256SUMS"]
        (self.release / "SHA256SUMS").write_text("".join(lines))

    def run_shell(self, code, *, before="", env=None, ok=True):
        result = subprocess.run(
            [BASH, "-c", f"source {shlex.quote(str(INSTALLER))}\n{HOST_COMMANDS}\n{before}\n{code}"],
            env={**self.env, **(env or {})}, stdin=subprocess.DEVNULL,
            capture_output=True, text=True,
        )
        output = result.stdout + result.stderr
        if ok:
            self.assertEqual(result.returncode, 0, output)
        else:
            self.assertNotEqual(result.returncode, 0, output)
        self.assertNotIn("forbidden-toolchain", self.calls())
        return output

    def calls(self):
        return self.log.read_text() if self.log.exists() else ""

    def test_linux_archive_install_never_builds(self):
        self.run_shell("main --yes --no-launch")
        calls = self.calls()
        self.assertIn("root apt-get install -y acl", calls)
        self.assertIn("gir1.2-adw-1", calls)
        self.assertIn("/payload/scripts/install.sh --install-built", calls)
        self.assertIn("user installed-zflow desktop-agent --install", calls)
        self.assertEqual(list((self.root / "tmp").iterdir()), [])
        for package in ("build-essential", "gcc", "base-devel", "pkg-config"):
            self.assertNotIn(package, calls)

    def test_headless_runtime_packages(self):
        for manager in ("apt-get", "dnf", "pacman"):
            with self.subTest(manager=manager):
                self.log.write_text("")
                self.run_shell("main --yes --headless", env={"MANAGER": manager})
                self.assertIn(f"root {manager} ", self.calls())
                self.assertNotIn("gjs", self.calls())
                self.assertNotIn("installed-zflow", self.calls())
                self.assertNotIn("pacman -Sy", self.calls())

    def test_debian_uses_package_manager_and_global_desktop_assets(self):
        self.run_shell("main --yes --no-launch", env={"LEGACY": "false"})
        self.assertIn("zflow_0.1.0_amd64.deb", self.calls())
        self.assertIn("root apt-get install -y", self.calls())
        self.assertIn("extension enable zflow@demfabris", self.calls())
        self.assertNotIn("desktop-agent --install", self.calls())
        self.assertNotIn("/scripts/install.sh", self.calls())

    def test_headless_debian_skips_desktop_recommendations(self):
        self.run_shell("main --yes --headless", env={"LEGACY": "false"})
        self.assertIn("--no-install-recommends", self.calls())
        self.assertNotIn("extension enable", self.calls())

    def test_platform_and_architecture_select_release_assets(self):
        for platform, architecture, target in (
            ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ):
            with self.subTest(target=target):
                self.log.write_text("")
                self.run_shell("main --yes --no-launch --version 0.1.0",
                               before='install_macos() { test -d "$payload_dir/zflow.app"; }',
                               env={"PLATFORM": platform, "ARCH": architecture})
                self.assertIn(f"/releases/download/v0.1.0/zflow-v0.1.0-{target}.tar.gz", self.calls())
                self.assertNotIn("latest", self.calls())
        self.run_shell('platform=Darwin; version=v0.1.0; select_artifact; echo "$asset"',
                       env={"ARCH": "x86_64", "ROSETTA": "1"})

    def test_latest_resolution_is_pinned_for_all_downloads(self):
        self.run_shell("main --yes --headless")
        self.assertEqual(self.calls().splitlines().count("latest"), 1)
        downloads = [line for line in self.calls().splitlines() if line.startswith("fetch ")]
        self.assertEqual(len(downloads), 2)
        self.assertTrue(all("/releases/download/v0.1.0/" in line for line in downloads))

    def test_corrupt_or_missing_checksum_fails_before_privileges(self):
        checksum = self.release / "SHA256SUMS"
        original = checksum.read_text()
        for text in ("", original + original, "\n".join("0" * 64 + line[64:] for line in original.splitlines())):
            with self.subTest(checksum=text[:16]):
                checksum.write_text(text)
                self.run_shell("main --yes --no-launch", ok=False)
                self.assertNotIn("root ", self.calls())
                self.assertEqual(list((self.root / "tmp").iterdir()), [])

    def test_download_and_archive_errors_never_elevate(self):
        self.run_shell("main --yes", before="fetch() { return 22; }", ok=False)
        archive = self.release / "zflow-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        archive.write_bytes(b"not an archive")
        self.write_checksums()
        self.run_shell("main --yes", ok=False)
        self.assertNotIn("root ", self.calls())

    def test_unsupported_hosts_stop_before_downloads(self):
        for env in ({"ARCH": "riscv64"}, {"LIBC": "glibc 2.38"},
                    {"SYSTEMD_VERSION": ""}, {"PLATFORM": "Darwin", "MACOS_VERSION": "15.0"}):
            with self.subTest(env=env):
                self.run_shell("main --yes --version v0.1.0", env=env, ok=False)
                self.assertNotIn("fetch ", self.calls())
                self.assertNotIn("root ", self.calls())

    def test_gnome_delayed_enable_succeeds_but_write_errors_fail(self):
        args = "main --yes --no-launch"
        output = self.run_shell(args, env={"DESKTOP_STATUS": "1", "DESKTOP_OUTPUT": "Integration installed. Log out and back in"})
        self.assertIn("installation finished", output)
        output = self.run_shell(args, env={"DESKTOP_STATUS": "1", "DESKTOP_OUTPUT": "Permission denied"}, ok=False)
        self.assertIn("desktop setup failed", output)

    def test_rejects_root_and_source_build_options(self):
        cases = (("--yes", {"MOCK_UID": "0"}), ("--version", {}), ("--version --yes", {}),
                 ("--version ../main", {}), ("--source .", {}), ("--ref main", {}), ("--sign Developer", {}))
        for args, env in cases:
            with self.subTest(args=args, env=env):
                self.run_shell(f"main {args}", env=env, ok=False)
                self.assertEqual(self.calls(), "")

    def test_piped_script_and_truncated_download(self):
        script = INSTALLER.read_text()
        result = subprocess.run([BASH, "-s", "--", "--help"], input=script, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Usage:", result.stdout)
        result = subprocess.run([BASH, "-s"], input=script[:script.rindex('if [[ -z "${BASH_SOURCE')], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_confirmation_without_terminal_requires_yes(self):
        result = subprocess.run(
            [BASH, "-c", f'source {shlex.quote(str(INSTALLER))}\n{HOST_COMMANDS}\nmain --version 0.1.0'],
            env=self.env, stdin=subprocess.DEVNULL, start_new_session=True, capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pass --yes", result.stderr)
        self.assertEqual(self.calls(), "")

    def test_graphical_auth_and_terminal_auth(self):
        code = r'''
eval "$original_root"
pkexec() { record "pkexec $*"; }
sudo() { record "sudo $*"; }
platform=Linux
as_root install example
unset DISPLAY WAYLAND_DISPLAY
as_root install example
platform=Darwin
as_root install example
'''
        result = subprocess.run(
            [BASH, "-c", f"source {shlex.quote(str(INSTALLER))}\noriginal_root=$(declare -f as_root)\n{HOST_COMMANDS}\n{code}"],
            env=self.env, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls().splitlines(), [
            "pkexec --disable-internal-agent install example", "sudo -- install example", "sudo -- install example",
        ])

    def mac_install(self, *, failure="", existing=True):
        destination = self.root / "Applications/zflow.app"
        destination.parent.mkdir(exist_ok=True)
        if existing:
            (destination / "Contents").mkdir(parents=True)
            (destination / "Contents/version").write_text("old")
        code = r'''
as_root() {
    record "root $*"
    # All filesystem operations stay under this test's temporary directory.
    case "$1" in
        mktemp) [[ "$3" == "$TEST_ROOT/"* ]] ;;
        mv|rm|ditto|codesign) [[ "$*" == *"$TEST_ROOT/"* ]] ;;
        *) return 99 ;;
    esac
    if [[ "$1" == mv && "$2" == "$mac_stage/zflow.app" && "$FAILURE" == replace ]]; then return 42; fi
    if [[ "$1" == mv && "$2" == "$mac_stage/previous.app" && "$FAILURE" == restore ]]; then return 43; fi
    if [[ "$1" == mv && "$2" == "$mac_stage/zflow.app" && "$FAILURE" == restore ]]; then return 42; fi
    "$@"
}
payload_dir="$FIXTURE"
mac_destination="$TEST_ROOT/Applications/zflow.app"
mac_stage=''
work_dir=''
launch=false
trap cleanup EXIT
install_macos
'''
        self.run_shell(code, env={"FAILURE": failure}, ok=not failure)
        return destination

    def test_macos_fresh_install(self):
        destination = self.mac_install(existing=False)
        self.assertEqual((destination / "Contents/version").read_text(), "new")
        self.assertEqual(list(destination.parent.glob(".zflow-install.*")), [])
        self.run_shell('check_macos', before='')

    def test_macos_update_replaces_existing_app(self):
        destination = self.mac_install()
        self.assertEqual((destination / "Contents/version").read_text(), "new")
        self.assertEqual(list(destination.parent.glob(".zflow-install.*")), [])

    def test_macos_failed_replacement_restores_previous_app(self):
        destination = self.mac_install(failure="replace")
        self.assertEqual((destination / "Contents/version").read_text(), "old")
        self.assertEqual(list(destination.parent.glob(".zflow-install.*")), [])

    def test_macos_failed_restore_keeps_recoverable_copy(self):
        destination = self.mac_install(failure="restore")
        previous = list(destination.parent.glob(".zflow-install.*/previous.app/Contents/version"))
        self.assertEqual(len(previous), 1)
        self.assertEqual(previous[0].read_text(), "old")


if __name__ == "__main__":
    unittest.main()
