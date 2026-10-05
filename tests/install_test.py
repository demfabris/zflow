"""Test release selection and verification without installing on the host."""

import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
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
            gjs|gnome-extensions) [[ -z "${GNOME_TOOLS_MISSING:-}" ]]; return ;;
            # Debian keeps these in /usr/sbin, outside a normal user's PATH.
            getent|groupadd|useradd|runuser|modprobe|systemd-analyze) return 1 ;;
        esac
    fi
    builtin command "$@"
}
# The single elevated step really runs as bash -c CODE NAME ARGS, without root.
as_root() {
    if [[ "$1 $2" == "bash -c" ]]; then
        record "root bash -c ${*:4}"
        # Keep the transaction lock and version probe inside the fixture too.
        local code=${3//\/run\/zflow-update.lock/$TEST_ROOT\/root\/update.lock}
        code="$(declare -f /usr/local/bin/zflow /usr/bin/zflow)"$'\n'"$code"
        bash -c "$code" "${@:4}"
    else record "root $*"; fi
}
fetch() { record "fetch $1"; cp "$RELEASE/${1##*/}" "$2"; }
curl() { record latest; echo "${LATEST_URL:-https://github.com/demfabris/zflow/releases/tag/v0.1.0}"; }
archive_install_present() { [[ "$LEGACY" == true ]]; }
dpkg-query() { echo "${DPKG_STATUS:-unknown}"; }
dpkg() { echo "${DPKG_ARCH:-amd64}"; }
getconf() { echo "${LIBC:-glibc 2.39}"; }
systemctl() { echo "${SYSTEMD_VERSION-259}"; }
udevadm() { echo verify; }
gjs() { [[ -z "${GTK_MISSING:-}" ]] || return 1; echo "${GTK_VERSIONS:-4.12 1.5}"; }
gnome-extensions() { record "extension $*"; }
pkexec() { record "unexpected pkexec"; return 99; }
for tool in cargo rustup rustc swift xcrun xcode-select cc make; do
    eval "$tool() { record forbidden-toolchain; return 99; }"
done
function /usr/local/bin/zflow() {
    if [[ "$1" == --version ]]; then echo "zflow ${INSTALLED_VERSION:-0.0.9}"; return; fi
    record "user installed-zflow $*"
    echo "${DESKTOP_OUTPUT:-Desktop installed}"
    return "${DESKTOP_STATUS:-0}"
}
function /usr/bin/zflow() {
    if [[ "$1" == --version ]]; then echo "zflow ${INSTALLED_VERSION:-0.0.9}"; return; fi
    record "user deb-zflow $*"
}
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
        (self.source / "scripts/install.sh").write_text('printf \'setup %s %s\\n\' "$0" "$*" >> "$LOG"\n')
        for name in ("zflow", "zflowd"):
            binary = self.source / "bin" / name
            # TMPDIR can be noexec, so the installer must never run downloaded files.
            binary.write_text('#!/bin/sh\necho executed-download >> "$LOG"\n')
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
        (self.root / "root").mkdir()
        # Commands the elevated shell runs; it cannot see this shell's functions.
        tools = self.root / "bin"
        tools.mkdir()
        for name in ("apt-get", "dnf", "pacman", "flock"):
            (tools / name).write_text(f'#!/bin/sh\necho "{name} $*" >> "$LOG"\n')
        # The root step stages under /tmp; keep that inside the test directory.
        (tools / "mktemp").write_text(
            '#!/bin/sh\ncase "$2" in /tmp/zflow-install.*) set -- "$1" "$TEST_ROOT/root/${2#/tmp/}";; esac\n'
            'exec /usr/bin/mktemp "$@"\n')
        for tool in tools.iterdir():
            tool.chmod(0o755)
        self.env = {
            **os.environ,
            "HOME": str(self.root / "home"), "TMPDIR": str(self.root / "tmp"),
            "PATH": f"{self.root / 'bin'}:{os.environ['PATH']}",
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
            capture_output=True, text=True, errors="backslashreplace",
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

    def elevations(self):
        return [line for line in self.calls().splitlines() if line.startswith("root ")]

    def assert_temporary_files_removed(self):
        self.assertEqual(list((self.root / "tmp").iterdir()), [])
        self.assertEqual([path for path in (self.root / "root").iterdir() if path.name != "update.lock"], [])

    def test_linux_archive_install_never_builds(self):
        self.run_shell("main --yes --no-launch")
        calls = self.calls()
        self.assertEqual(len(self.elevations()), 1)
        self.assertIn("\napt-get install -y kmod", calls)
        self.assertIn("gir1.2-adw-1", calls)
        # Root runs the installer from its own extracted copy, not the user's download.
        self.assertIn(f"setup {self.root}/root/zflow-install.", calls)
        self.assertIn("/payload/scripts/install.sh --install-built", calls)
        self.assertIn("user installed-zflow desktop-agent --install", calls)
        self.assertNotIn("executed-download", calls)
        self.assert_temporary_files_removed()
        for package in ("build-essential", "gcc", "base-devel", "pkg-config"):
            self.assertNotIn(package, calls)

    def test_headless_runtime_packages(self):
        for manager in ("apt-get", "dnf", "pacman"):
            with self.subTest(manager=manager):
                self.log.write_text("")
                self.run_shell("main --yes --headless", env={"MANAGER": manager})
                self.assertEqual(len(self.elevations()), 1)
                self.assertIn(f"\n{manager} ", self.calls())
                self.assertIn("--install-built", self.calls())
                self.assertNotIn("gjs", self.calls())
                self.assertNotIn("installed-zflow", self.calls())
                self.assertNotIn("pacman -Sy", self.calls())

    def test_debian_uses_package_manager_then_sets_up_the_desktop_user(self):
        self.run_shell("main --no-launch", env={"LEGACY": "false"})
        self.assertEqual(len(self.elevations()), 1)
        self.assertIn(f"apt-get install -y --install-recommends {self.root}/root/zflow-install.", self.calls())
        self.assertIn("/zflow_0.1.0_amd64.deb", self.calls())
        # The package ships the launcher, autostart and D-Bus files; the user step
        # adds the extension and starts the agent in this session.
        self.assertTrue(self.calls().endswith("user deb-zflow desktop-agent --install\n"), self.calls())
        self.assertNotIn("extension ", self.calls())
        self.assertNotIn("setup ", self.calls())
        self.assert_temporary_files_removed()

    def test_gui_update_preserves_install_type_and_skips_desktop_setup(self):
        for legacy, asset in (("true", "unknown-linux-gnu.tar.gz"), ("false", "amd64.deb")):
            with self.subTest(legacy=legacy):
                self.log.write_text("")
                self.run_shell("main --update 0.0.9 --gui --no-launch --version v0.1.0", env={"LEGACY": legacy})
                self.assertIn(asset, self.calls())
                self.assertEqual(len(self.elevations()), 1)
                self.assertNotIn("desktop-agent", self.calls())
                self.assertNotIn("settings", self.calls())
                self.assertNotIn("latest", self.calls())
                self.assert_temporary_files_removed()

    def test_gui_authorization_cancellation_keeps_the_installation(self):
        self.run_shell("main --update 0.0.9 --gui --no-launch --version v0.1.0",
                       before='as_root() { record "cancelled approval"; return 126; }', ok=False)
        self.assertIn("cancelled approval", self.calls())
        self.assertNotIn("apt-get", self.calls())
        self.assertNotIn("setup ", self.calls())
        self.assertNotIn("desktop-agent", self.calls())
        self.assert_temporary_files_removed()

    def test_gui_update_requires_graphical_authorization_without_sudo_fallback(self):
        code = r'''
command() {
    if [[ "$1 $2" == '-v pkexec' ]]; then return 1; fi
    builtin command "$@"
}
'''
        output = self.run_shell("main --update 0.0.9 --gui --version v0.1.0", before=code, ok=False)
        self.assertIn("pkexec", output)
        self.assertEqual(self.calls(), "")
        self.run_shell("main --gui --version v0.1.0", ok=False)
        self.run_shell("main --update 0.0.9 --gui --version v0.1.0", env={"DBUS_SESSION_BUS_ADDRESS": ""}, ok=False)
        self.assertEqual(self.calls(), "")

    def test_update_cannot_replace_a_version_installed_during_the_download(self):
        for legacy in ("true", "false"):
            with self.subTest(legacy=legacy):
                self.log.write_text("")
                output = self.run_shell("main --update 0.0.9 --gui --no-launch --version v0.1.0",
                                        env={"LEGACY": legacy, "INSTALLED_VERSION": "0.2.0"}, ok=False)
                self.assertIn("changed while this update", output)
                self.assertNotIn("\napt-get", self.calls())
                self.assertNotIn("setup ", self.calls())

    def test_root_step_rejects_a_download_swapped_after_verification(self):
        # Simulate a process running as the user replacing the file during the password prompt.
        swap = 'eval "original_$(declare -f as_root)"\nas_root() { printf swapped > "$5"; original_as_root "$@"; }'
        for legacy in ("false", "true"):
            with self.subTest(legacy=legacy):
                self.log.write_text("")
                output = self.run_shell("main --yes --no-launch", before=swap, env={"LEGACY": legacy}, ok=False)
                self.assertIn("Checksum mismatch", output)
                self.assertEqual(len(self.elevations()), 1)
                self.assertFalse([line for line in self.calls().splitlines() if line.startswith(("apt-get", "setup "))])
                self.assert_temporary_files_removed()

    def test_debian_package_follows_dpkg_architecture(self):
        self.run_shell("main --yes --headless", env={"LEGACY": "false", "ARCH": "x86_64", "DPKG_ARCH": "arm64"})
        self.assertIn("zflow_0.1.0_arm64.deb", self.calls())
        self.log.write_text("")
        output = self.run_shell("main --yes --headless", env={"LEGACY": "false", "ARCH": "aarch64", "DPKG_ARCH": "armhf"}, ok=False)
        self.assertIn("armhf", output)
        self.assertNotIn("fetch ", self.calls())

    def test_old_gtk_stops_before_changes_but_missing_gtk_is_installed(self):
        output = self.run_shell("main --yes --no-launch", env={"LEGACY": "false", "GTK_VERSIONS": "4.10 1.4"}, ok=False)
        self.assertIn("GTK 4.12+", output)
        self.assertNotIn("fetch ", self.calls())
        self.assertNotIn("root ", self.calls())
        self.run_shell("main --yes --no-launch", env={"LEGACY": "false", "GTK_MISSING": "1"})
        self.assertIn("deb-zflow desktop-agent --install", self.calls())

    def test_missing_gnome_tools_are_installed_before_desktop_setup(self):
        for legacy in ("false", "true"):
            with self.subTest(legacy=legacy):
                self.log.write_text("")
                self.run_shell("main --no-launch", env={"LEGACY": legacy, "GNOME_TOOLS_MISSING": "1"})
                calls = self.calls()
                self.assertLess(calls.index("apt-get install"), calls.index("desktop-agent --install"))
                if legacy == "false":
                    self.assertIn("--install-recommends", calls)
                else:
                    self.assertIn("gjs gir1.2-gtk-4.0 gir1.2-adw-1 pkexec", calls)

    def test_headless_debian_skips_desktop_recommendations(self):
        self.run_shell("main --yes --headless", env={"LEGACY": "false"})
        self.assertIn("--no-install-recommends", self.calls())
        self.assertNotIn("desktop-agent", self.calls())

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
        # An arm64 Mac running this script under Rosetta still gets the native app.
        output = self.run_shell('platform=Darwin; version=v0.1.0; select_artifact; echo "$legacy_asset"',
                                env={"ARCH": "x86_64", "ROSETTA": "1"})
        self.assertIn("zflow-v0.1.0-aarch64-apple-darwin.tar.gz", output)

    def test_macos_prefers_universal_archive(self):
        universal = self.release / "zflow-v0.1.0-universal-apple-darwin.tar.gz"
        with tarfile.open(universal, "w:gz") as archive:
            archive.add(self.source, arcname="zflow-release")
        self.write_checksums()
        for architecture in ("arm64", "x86_64"):
            with self.subTest(architecture=architecture):
                self.log.write_text("")
                self.run_shell("main --yes --no-launch --version 0.1.0",
                               before='install_macos() { test -d "$payload_dir/zflow.app"; }',
                               env={"PLATFORM": "Darwin", "ARCH": architecture})
                downloads = [line for line in self.calls().splitlines() if line.startswith("fetch ")]
                self.assertEqual(len(downloads), 2)
                self.assertTrue(downloads[1].endswith("/v0.1.0/zflow-v0.1.0-universal-apple-darwin.tar.gz"))

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

    def test_desktop_step_reports_the_next_step_and_fails_on_errors(self):
        args = "main --no-launch"
        output = self.run_shell(args, env={"DESKTOP_OUTPUT": "Log out and back in to finish setting up zflow in GNOME."})
        self.assertIn("Log out and back in", output)
        self.assertIn("installation finished", output)
        output = self.run_shell(args, env={"DESKTOP_STATUS": "1", "DESKTOP_OUTPUT": "Permission denied"}, ok=False)
        self.assertIn("Permission denied", output)
        self.assertIn("desktop setup failed", output)
        self.assertNotIn("installation finished", output)

    def test_rejects_root_and_invalid_options(self):
        cases = (("--yes", {"MOCK_UID": "0"}), ("--version", {}), ("--version --yes", {}),
                 ("--version ../main", {}), ("--unknown", {}))
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

    def test_no_question_before_the_password_prompt(self):
        # No terminal at all: the password prompt is the only confirmation.
        result = subprocess.run(
            [BASH, "-c", f'source {shlex.quote(str(INSTALLER))}\n{HOST_COMMANDS}\nmain --version 0.1.0 --no-launch'],
            env=self.env, stdin=subprocess.DEVNULL, start_new_session=True, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("[y/N]", result.stdout)
        # The elevated step starts right after "Installing the service".
        summary = result.stdout[:result.stdout.index("Installing the service")]
        for line in ("This installs:", "UDP port 43119 when", "for every GNOME user", "GNOME extension"):
            self.assertIn(line, summary)
        # --yes stays accepted so older command lines keep working.
        self.log.write_text("")
        self.run_shell("main --yes --version 0.1.0 --no-launch")
        self.assertEqual(len(self.elevations()), 1)

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


class PackagingTest(unittest.TestCase):
    ROOT = INSTALLER.parent

    def read(self, path):
        return (self.ROOT / path).read_text()

    @unittest.skipIf(os.geteuid() == 0, "tests the normal user's archive entry point")
    def test_archive_installer_defaults_to_prebuilt_binaries(self):
        with tempfile.TemporaryDirectory(prefix="zflow-archive-test-") as temp:
            root = Path(temp)
            for path in ("scripts", "packaging", "assets/linux"):
                shutil.copytree(self.ROOT / path, root / path)
            (root / "bin").mkdir()
            for name in ("zflow", "zflowd"):
                binary = root / "bin" / name
                binary.write_text("#!/bin/sh\nexit 0\n")
                binary.chmod(0o755)
            tools = root / "tools"
            tools.mkdir()
            log = root / "calls"
            for name, body in (
                ("uname", "echo Linux"),
                ("sudo", 'printf "sudo %s\\n" "$*" >> "$LOG"'),
                ("cargo", 'echo forbidden-cargo >> "$LOG"; exit 99'),
            ):
                tool = tools / name
                tool.write_text(f"#!/bin/sh\n{body}\n")
                tool.chmod(0o755)
            result = subprocess.run([BASH, str(root / "scripts/install.sh")],
                                    env={**os.environ, "PATH": f"{tools}:{os.environ['PATH']}", "LOG": str(log)},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(log.read_text(), f"sudo -- {root}/scripts/install.sh --install-built\n")
            self.assertFalse((root / "Cargo.toml").exists(), "release users do not need a source checkout")

    @unittest.skipUnless(os.uname().sysname == "Linux", "the Debian packaging script runs on Linux")
    def test_debian_build_stamps_only_the_staged_extension_metadata(self):
        with tempfile.TemporaryDirectory(prefix="zflow-deb-test-") as temp:
            root = Path(temp)
            for path in ("debian", "packaging", "assets/linux"):
                shutil.copytree(self.ROOT / path, root / path)
            (root / "scripts").mkdir()
            shutil.copy(self.ROOT / "scripts/build-deb.sh", root / "scripts")
            (root / "Cargo.toml").write_text('[package]\nversion = "0.7.3"\n')
            (root / "bin").mkdir()
            tools = root / "tools"
            tools.mkdir()
            metadata = root / "packaged-metadata.json"
            for name, body in (
                ("dpkg", "echo amd64"),
                ("dpkg-buildpackage", 'cp packaging/gnome-extension/metadata.json "$TEST_METADATA"; touch ../zflow_0.7.3_amd64.deb'),
            ):
                tool = tools / name
                tool.write_text(f"#!/bin/sh\n{body}\n")
                tool.chmod(0o755)
            original = (root / "packaging/gnome-extension/metadata.json").read_text()
            result = subprocess.run([BASH, str(root / "scripts/build-deb.sh"), str(root / "bin")],
                                    env={**os.environ, "PATH": f"{tools}:{os.environ['PATH']}", "TEST_METADATA": str(metadata)},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(json.loads(metadata.read_text())["version-name"], "0.7.3")
            self.assertEqual((root / "packaging/gnome-extension/metadata.json").read_text(), original)
            self.assertTrue((root / "target/dist/zflow_0.7.3_amd64.deb").exists())

    def test_every_install_path_ships_the_extension_files_desktop_rs_writes(self):
        rust = self.read("src/app/desktop.rs")
        block = rust[rust.index("const EXTENSION_FILES"):]
        written = set(re.findall(r'"([\w.]+)",\s*include_str!', block[:block.index("];")]))
        packed = set(re.search(r"files=\(([^)]*)\)", self.read("scripts/pack-extension.sh")).group(1).split())
        packaged = set(re.search(r"addprefix packaging/gnome-extension/,([^)]*)\)", self.read("debian/rules")).group(1).split())
        self.assertEqual(written, packed)
        self.assertEqual(written, packaged)
        # The setup banner installs extensions, which extensions.gnome.org reviewers reject.
        self.assertNotIn("setup.js", written)
        self.assertNotIn("app.js", written)
        self.assertNotIn("updates.js", written)

    def test_archive_install_and_uninstall_cover_the_same_session_files(self):
        install, uninstall = self.read("scripts/install.sh"), self.read("scripts/uninstall.sh")
        for path in ("/usr/local/share/applications/io.zflow.zflow.desktop",
                     "/usr/local/share/dbus-1/services/io.zflow.Desktop.service",
                     "/etc/xdg/autostart/io.zflow.desktop-agent.desktop",
                     "/usr/local/share/icons/hicolor/scalable/apps/io.zflow.zflow.svg",
                     "/usr/local/share/icons/hicolor/symbolic/apps/io.zflow.zflow-symbolic.svg",
                     "firewall.sh"):
            with self.subTest(path=path):
                self.assertIn(path.rsplit("/", 1)[-1], install)
                self.assertIn(path.rsplit("/", 1)[-1], uninstall)
        self.assertIn("Icon=io.zflow.zflow\n", self.read("packaging/linux/io.zflow.zflow.desktop"))
        entry = self.read("packaging/linux/io.zflow.desktop-agent.desktop")
        self.assertIn("OnlyShowIn=GNOME;", entry)
        # A removed-but-not-purged package leaves this conffile behind.
        self.assertIn("TryExec=/usr/bin/zflow", entry)


class UninstallTest(unittest.TestCase):
    def test_refuses_debian_package_host(self):
        with tempfile.TemporaryDirectory(prefix="zflow-uninstall-test-") as temp:
            tools = Path(temp)
            log = tools / "calls"
            for name, body in (("uname", "echo Linux"), ("dpkg-query", "echo 'install ok installed'"),
                               ("systemctl", f"echo systemctl >> {shlex.quote(str(log))}")):
                (tools / name).write_text(f"#!/bin/sh\n{body}\n")
                (tools / name).chmod(0o755)
            result = subprocess.run(
                [BASH, str(INSTALLER.parent / "scripts/uninstall.sh")],
                env={**os.environ, "PATH": f"{tools}:{os.environ['PATH']}"},
                capture_output=True, text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("managed by dpkg", result.stderr)
            self.assertFalse(log.exists())


if __name__ == "__main__":
    unittest.main()
