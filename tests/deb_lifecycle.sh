#!/usr/bin/env bash
# Exercise the real Debian lifecycle inside a disposable Ubuntu container.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
architecture=$(dpkg --print-architecture)
packages=("$root/target/dist/"zflow_*_"$architecture.deb")
[[ ${#packages[@]} == 1 && -f "${packages[0]}" ]] || { echo 'Build one native .deb first.' >&2; exit 1; }
docker run --rm -i --mount "type=bind,src=${packages[0]},dst=/tmp/zflow.deb,readonly" ubuntu:24.04 bash -es <<'CONTAINER'
export DEBIAN_FRONTEND=noninteractive
dpkg-deb -e /tmp/zflow.deb /tmp/control
grep -q '_dh_action=restart' /tmp/control/postinst
apt-get update -qq
apt-get install -y -qq --no-install-recommends /tmp/zflow.deb
test -x /usr/bin/zflow
/usr/bin/zflow --version
test -f /usr/share/gnome-shell/extensions/zflow@demfabris/extension.js
test -f /usr/share/applications/io.zflow.zflow.desktop
test -f /usr/lib/systemd/system/zflowd.service
grep -qx 'ExecStart=/usr/bin/zflowd --config /etc/zflow/zflow.toml' /usr/lib/systemd/system/zflowd.service
test "$(stat -c '%U:%G:%a' /etc/zflow/zflow.toml)" = zflow:zflow:600
test "$(stat -c '%U:%G:%a' /var/lib/zflow)" = zflow:zflow:700
test "$(getent group zflow | cut -d: -f4)" = ''
printf '\n# Preserve this edit\n' >> /etc/zflow/zflow.toml
printf '# Preserve selected devices\n' > /etc/udev/rules.d/71-zflow-capture.rules
printf 'identity sentinel\n' > /var/lib/zflow/identity-test
cp /etc/zflow/zflow.toml /tmp/expected.toml
cp /etc/udev/rules.d/71-zflow-capture.rules /tmp/expected.rules
dpkg -i /tmp/zflow.deb
cmp /etc/zflow/zflow.toml /tmp/expected.toml
cmp /etc/udev/rules.d/71-zflow-capture.rules /tmp/expected.rules
dpkg --remove zflow
test ! -e /usr/bin/zflow
test ! -e /etc/udev/rules.d/71-zflow-capture.rules
cmp /etc/udev/71-zflow-capture.rules.disabled /tmp/expected.rules
test "$(stat -c '%U:%G' /etc/udev/71-zflow-capture.rules.disabled)" = root:root
if runuser -u zflow -- test -w /etc/udev; then echo 'Daemon can replace saved rules' >&2; exit 1; fi
cmp /etc/zflow/zflow.toml /tmp/expected.toml
dpkg -i /tmp/zflow.deb
cmp /etc/udev/rules.d/71-zflow-capture.rules /tmp/expected.rules
cmp /etc/zflow/zflow.toml /tmp/expected.toml
dpkg --purge zflow
test ! -e /etc/zflow/zflow.toml
test ! -e /etc/udev/rules.d/71-zflow-capture.rules
grep -qx 'identity sentinel' /var/lib/zflow/identity-test
# A package must not silently shadow a previous /usr/local installation.
mkdir -p /usr/local/bin
touch /usr/local/bin/zflow
if dpkg -i /tmp/zflow.deb; then echo 'Legacy installation was not rejected' >&2; exit 1; fi
test ! -e /usr/bin/zflow
echo 'Debian install, upgrade, removal, reinstall, purge, and legacy detection passed.'
CONTAINER
