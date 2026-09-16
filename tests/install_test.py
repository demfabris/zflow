"""Exercise the source installer without changing the host or downloading tools."""

import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
BASH = os.environ.get("TEST_BASH", "bash")

# Intercept the host boundary, while using real Bash, archive extraction and files.
SHIMS = r'''
record() { printf '%s\n' "$*" >> "$LOG"; }
uname() { printf '%s\n' "$PLATFORM"; }
id() { printf '%s\n' "${MOCK_UID:-1000}"; }
command() {
    if [[ "$1" == -v ]]; then
        case "$2" in
            apt-get|dnf|pacman) [[ "$2" == "$MANAGER" ]]; return ;;
        esac
    fi
    builtin command "$@"
}
as_root() { record "root $*"; }
fetch() { record "fetch $1"; cp "$ARCHIVE" "$2"; }
curl() { return 99; }
systemctl() { printf '%s\n' "${SYSTEMD_VERSION-259}"; }
udevadm() { printf 'verify\n'; }
gjs() { printf '%s\n' "${GTK_VERSIONS:-4.12 1.5}"; }
gnome-extensions() { :; }
rustc() { printf 'rustc %s\n' "${RUST_VERSION:-1.88.0}"; }
cargo() { record "user cargo $*"; return "${BUILD_STATUS:-0}"; }
rustup() { record "user rustup $*"; export RUST_VERSION=1.99.0; }
for tool in cc make pkg-config getent groupadd useradd runuser setfacl modprobe systemd-analyze; do
    eval "$tool() { :; }"
done
function /usr/local/bin/zflow() {
    record "user installed-zflow $*"
    printf '%s\n' "${DESKTOP_OUTPUT:-Desktop installed}"
    return "${DESKTOP_STATUS:-0}"
}
sw_vers() { printf '%s\n' "${MACOS_VERSION:-26.0}"; }
xcode-select() {
    record "xcode-select $*"
    if [[ "$1" == -p && "${MISSING_XCODE:-false}" == true ]]; then return 1; fi
}
swift() { printf 'Apple Swift version %s\n' "${SWIFT_VERSION:-6.2}"; }
xcrun() { printf '%s\n' "${SDK_VERSION:-26.0}"; }
codesign() { record "codesign $*"; }
ditto() { cp -R "$1" "$2"; }
pgrep() { return 1; }
open() { record "open $*"; }
function /usr/libexec/PlistBuddy() { printf 'io.zflow.zflow\n'; }
'''


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zflow-installer-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "source with spaces"
        (self.source / "scripts").mkdir(parents=True)
        (self.source / "Cargo.toml").write_text('rust-version = "1.88"\n')
        (self.source / "scripts/install.sh").write_text("exit 99\n")
        (self.source / "scripts/build-macos-app.sh").write_text(
            'printf "mac build %s\\n" "$*" >> "$LOG"\n'
            'if [[ $# -gt 0 ]]; then [[ $# == 2 && "$1" == --sign ]] || exit 99; fi\n'
            'bundle="$(dirname "$0")/../target/release/zflow.app"\n'
            'mkdir -p "$bundle/Contents"\n'
            'printf new > "$bundle/Contents/version"\n'
        )
        self.archive = self.root / "source.tar.gz"
        with tarfile.open(self.archive, "w:gz") as archive:
            archive.add(self.source, arcname="zflow-main")
        self.log = self.root / "calls"
        (self.root / "home").mkdir()
        (self.root / "tmp").mkdir()
        self.env = {
            **os.environ,
            "HOME": str(self.root / "home"),
            "TMPDIR": str(self.root / "tmp"),
            "LOG": str(self.log),
            "FIXTURE": str(self.source),
            "ARCHIVE": str(self.archive),
            "TEST_ROOT": str(self.root),
            "PLATFORM": "Linux",
            "MANAGER": "apt-get",
            "XDG_CURRENT_DESKTOP": "ubuntu:GNOME",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/test-session",
            "DISPLAY": ":test",
        }

    def run_shell(self, code, *, before="", env=None, ok=True):
        result = subprocess.run(
            [BASH, "-c", f"source {shlex.quote(str(INSTALLER))}\n{SHIMS}\n{before}\n{code}"],
            env={**self.env, **(env or {})},
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
        )
        output = result.stdout + result.stderr
        if ok:
            self.assertEqual(result.returncode, 0, output)
        else:
            self.assertNotEqual(result.returncode, 0, output)
        return output

    def calls(self):
        return self.log.read_text() if self.log.exists() else ""

    def test_linux_installs_as_user_and_elevates_only_system_changes(self):
        self.run_shell('main --yes --no-launch --source "$FIXTURE"')
        calls = self.calls()
        self.assertIn("root apt-get install -y build-essential", calls)
        self.assertIn("gir1.2-adw-1", calls)
        self.assertIn("user cargo build --locked --release", calls)
        self.assertIn(f"root bash {self.source}/scripts/install.sh --install-built", calls)
        self.assertIn("user installed-zflow desktop-agent --install", calls)
        self.assertNotIn("root cargo", calls)
        self.assertEqual(list((self.root / "tmp").iterdir()), [])

    def test_headless_package_managers(self):
        for manager in ("apt-get", "dnf", "pacman"):
            with self.subTest(manager=manager):
                self.log.write_text("")
                self.run_shell('main --yes --headless --source "$FIXTURE"', env={"MANAGER": manager})
                calls = self.calls()
                self.assertIn(f"root {manager} ", calls)
                self.assertNotIn("gjs", calls)
                self.assertNotIn("installed-zflow", calls)
                self.assertNotIn("pacman -Sy", calls)

    def test_download_extracts_selected_ref_and_cleans_up(self):
        self.run_shell("main --yes --headless --skip-dependencies --ref v0.2.0")
        self.assertIn("fetch https://codeload.github.com/demfabris/zflow/tar.gz/v0.2.0", self.calls())
        self.assertIn("user cargo build", self.calls())
        self.assertEqual(list((self.root / "tmp").iterdir()), [])

    def test_failed_or_invalid_download_never_elevates(self):
        for fetch in ("return 22", "printf broken > \"$2\""):
            with self.subTest(fetch=fetch):
                self.run_shell("main --yes", before=f"fetch() {{ {fetch}; }}", ok=False)
                self.assertNotIn("root ", self.calls())
                self.assertNotIn("user cargo", self.calls())
                self.assertEqual(list((self.root / "tmp").iterdir()), [])

    def test_failed_build_never_installs(self):
        self.run_shell('main --yes --source "$FIXTURE" --skip-dependencies', env={"BUILD_STATUS": "42"}, ok=False)
        self.assertNotIn("root ", self.calls())
        self.assertNotIn("installed-zflow", self.calls())

    def test_missing_systemd_or_old_gtk_stops_before_build(self):
        for env in ({"SYSTEMD_VERSION": ""}, {"GTK_VERSIONS": "4.10 1.4"}):
            with self.subTest(env=env):
                self.run_shell('main --yes --skip-dependencies --source "$FIXTURE"', env=env, ok=False)
                self.assertNotIn("user cargo", self.calls())
                self.assertNotIn("root ", self.calls())

    def test_rust_upgrade_stays_unprivileged(self):
        self.run_shell('main --yes --source "$FIXTURE"', env={"RUST_VERSION": "1.80.0"})
        self.assertIn("user rustup toolchain install stable --profile minimal", self.calls())
        self.assertNotIn("root rustup", self.calls())

    def test_skip_dependencies_does_not_install_rust(self):
        self.run_shell('main --yes --skip-dependencies --source "$FIXTURE"', env={"RUST_VERSION": "1.80.0"}, ok=False)
        self.assertNotIn("rustup", self.calls())
        self.assertNotIn("root ", self.calls())

    def test_gnome_delayed_enable_succeeds_but_write_errors_fail(self):
        args = 'main --yes --no-launch --skip-dependencies --source "$FIXTURE"'
        output = self.run_shell(args, env={"DESKTOP_STATUS": "1", "DESKTOP_OUTPUT": "Integration installed. Log out and back in"})
        self.assertIn("installation finished", output)
        output = self.run_shell(args, env={"DESKTOP_STATUS": "1", "DESKTOP_OUTPUT": "Permission denied"}, ok=False)
        self.assertIn("desktop setup failed", output)

    def test_rejects_root_and_invalid_options_before_changes(self):
        cases = (("--yes", {"MOCK_UID": "0"}), ("--ref", {}), ("--ref --yes", {}),
                 ("--ref ../main", {}), ("--sign Developer", {}), ("--unknown", {}))
        for args, env in cases:
            with self.subTest(args=args, env=env):
                self.run_shell(f"main {args}", env=env, ok=False)
                self.assertEqual(self.calls(), "")

    def test_piped_script_and_truncated_download(self):
        script = INSTALLER.read_text()
        result = subprocess.run(
            [BASH, "-s", "--", "--help"], input=script, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Usage:", result.stdout)
        result = subprocess.run(
            [BASH, "-s"], input=script[:script.rindex('if [[ -z "${BASH_SOURCE')],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_confirmation_without_terminal_requires_yes(self):
        result = subprocess.run(
            [BASH, "-c", f'source {shlex.quote(str(INSTALLER))}\n{SHIMS}\nmain --source "$FIXTURE"'],
            env=self.env, stdin=subprocess.DEVNULL, start_new_session=True,
            capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pass --yes", result.stderr)
        self.assertEqual(self.calls(), "")

    def test_graphical_auth_and_terminal_auth(self):
        # Use the real privilege selector with harmless stand-ins for both brokers.
        original = "original_root=$(declare -f as_root)"
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
            [BASH, "-c", f"source {shlex.quote(str(INSTALLER))}\n{original}\n{SHIMS}\n{code}"],
            env=self.env, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls().splitlines(), [
            "pkexec --disable-internal-agent install example", "sudo -- install example", "sudo -- install example",
        ])

    def test_macos_preflight(self):
        for env in ({"MACOS_VERSION": "15.0"}, {"SWIFT_VERSION": "6.1"}, {"SDK_VERSION": "15.0"}):
            with self.subTest(env=env):
                self.run_shell('main --yes --source "$FIXTURE"', env={"PLATFORM": "Darwin", **env}, ok=False)
                self.assertNotIn("mac build", self.calls())
                self.assertNotIn("root ", self.calls())
        self.run_shell('main --yes --source "$FIXTURE"', env={"PLATFORM": "Darwin", "MISSING_XCODE": "true"}, ok=False)
        self.assertIn("xcode-select --install", self.calls())

    def mac_install(self, *, failure="", sign="", existing=True):
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
source_dir="$FIXTURE"
mac_destination="$TEST_ROOT/Applications/zflow.app"
mac_stage=''
work_dir=''
sign_identity="$SIGN"
launch=false
trap cleanup EXIT
install_macos
'''
        self.run_shell(code, env={"FAILURE": failure, "SIGN": sign}, ok=not failure)
        return destination

    def test_macos_fresh_install(self):
        destination = self.mac_install(existing=False)
        self.assertEqual((destination / "Contents/version").read_text(), "new")
        self.assertEqual(list(destination.parent.glob(".zflow-install.*")), [])
        self.run_shell('check_macos', before='skip_dependencies=true')

    def test_macos_signed_update_replaces_existing_app(self):
        destination = self.mac_install(sign="Apple Development: Test (TEAM)")
        self.assertEqual((destination / "Contents/version").read_text(), "new")
        self.assertEqual(list(destination.parent.glob(".zflow-install.*")), [])

    def test_macos_failed_replacement_restores_previous_app(self):
        destination = self.mac_install(failure="replace", sign="Apple Development: Test (TEAM)")
        self.assertEqual((destination / "Contents/version").read_text(), "old")
        self.assertEqual(list(destination.parent.glob(".zflow-install.*")), [])
        self.assertIn("mac build --sign Apple Development: Test (TEAM)", self.calls())

    def test_macos_failed_restore_keeps_recoverable_copy(self):
        destination = self.mac_install(failure="restore")
        previous = list(destination.parent.glob(".zflow-install.*/previous.app/Contents/version"))
        self.assertEqual(len(previous), 1)
        self.assertEqual(previous[0].read_text(), "old")


if __name__ == "__main__":
    unittest.main()
