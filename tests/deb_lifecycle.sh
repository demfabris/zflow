#!/usr/bin/env bash
# Exercise the real Debian lifecycle inside a disposable Ubuntu container.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
architecture=$(dpkg --print-architecture)
packages=("$root/target/dist/"zflow_*_"$architecture.deb")
[[ ${#packages[@]} == 1 && -f "${packages[0]}" ]] || { echo 'Build one native .deb first.' >&2; exit 1; }
docker run --rm -i --mount "type=bind,src=${packages[0]},dst=/tmp/zflow.deb,readonly" ubuntu:24.04 bash -es <<'CONTAINER'
set -o pipefail
export DEBIAN_FRONTEND=noninteractive
dpkg-deb -e /tmp/zflow.deb /tmp/control
grep -q '_dh_action=restart' /tmp/control/postinst
# Stand-ins for an active ufw and a running firewalld that log what the package asks
# for. apt runs maintainer scripts without /usr/local on PATH, so they go where the
# real tools live.
cat > /usr/sbin/ufw <<'FAKE'
#!/bin/sh
echo "$*" >> /tmp/ufw.log
if [ "$1" = status ]; then echo 'Status: active'; fi
FAKE
cat > /usr/bin/firewall-cmd <<'FAKE'
#!/bin/sh
echo "$*" >> /tmp/firewalld.log
case "$*" in
    *--get-default-zone*) echo public ;;
    *--get-zones*) echo 'public home' ;;
    *--zone=public*--query-service=zflow*) grep -q -- '--permanent --add-service=zflow' /tmp/firewalld.log ;;
    *--query-service=zflow*) exit 1 ;;
esac
FAKE
chmod 0755 /usr/sbin/ufw /usr/bin/firewall-cmd
apt-get update -qq
apt-get install -y -qq --no-install-recommends /tmp/zflow.deb | tee /tmp/install.log
test -x /usr/bin/zflow
/usr/bin/zflow --version
# The extension is exactly what scripts/pack-extension.sh uploads.
test "$(ls /usr/share/gnome-shell/extensions/zflow@demfabris | tr '\n' ' ')" = 'client.js extension.js indicator.js metadata.json prefs.js settings.js '
test -f /usr/share/applications/io.zflow.zflow.desktop
grep -qx 'Icon=io.zflow.zflow' /usr/share/applications/io.zflow.zflow.desktop
test -f /usr/share/icons/hicolor/scalable/apps/io.zflow.zflow.svg
test -f /usr/share/icons/hicolor/symbolic/apps/io.zflow.zflow-symbolic.svg
# Every GNOME user starts the agent at login and can activate it over D-Bus.
grep -qx 'Exec=/usr/bin/zflow desktop-agent' /etc/xdg/autostart/io.zflow.desktop-agent.desktop
grep -qx 'Exec=/usr/bin/zflow desktop-agent' /usr/share/dbus-1/services/io.zflow.Desktop.service
test -f /usr/lib/systemd/system/zflowd.service
# Pre-login boot ordering is opt-in through zflow setup --prelogin on.
test ! -e /usr/lib/systemd/system/zflowd.service.d/prelogin.conf
grep -qx 'ExecStart=/usr/bin/zflowd --config /etc/zflow/zflow.toml' /usr/lib/systemd/system/zflowd.service
test "$(stat -c '%U:%G:%a' /etc/zflow/zflow.toml)" = zflow:zflow:600
test "$(stat -c '%U:%G:%a' /var/lib/zflow)" = zflow:zflow:700
test "$(getent group zflow | cut -d: -f4)" = ''
# Both firewalls get the one UDP port, and the installer says so.
grep -qx 'allow zflow' /tmp/ufw.log
grep -qx 'ports=43119/udp' /etc/ufw/applications.d/zflow
grep -q 'Opened UDP port 43119 in ufw' /tmp/install.log
grep -qx -- '--permanent --add-service=zflow' /tmp/firewalld.log
grep -qx -- '--add-service=zflow' /tmp/firewalld.log
grep -q 'port protocol="udp" port="43119"' /etc/firewalld/services/zflow.xml
test "$(grep -c 43120 /etc/firewalld/services/zflow.xml)" = 0
grep -q 'in firewalld (service zflow, zone public)' /tmp/install.log
printf '\n# Preserve this edit\n' >> /etc/zflow/zflow.toml
printf '# Preserve selected devices\n' > /etc/udev/rules.d/71-zflow-capture.rules
printf 'identity sentinel\n' > /var/lib/zflow/identity-test
cp /etc/zflow/zflow.toml /tmp/expected.toml
cp /etc/udev/rules.d/71-zflow-capture.rules /tmp/expected.rules
dpkg -i /tmp/zflow.deb
cmp /etc/zflow/zflow.toml /tmp/expected.toml
cmp /etc/udev/rules.d/71-zflow-capture.rules /tmp/expected.rules
mkdir -p /etc/systemd/system/zflowd.service.d
printf '[Unit]\nBefore=display-manager.service\n' > /etc/systemd/system/zflowd.service.d/prelogin.conf
dpkg --remove zflow
test -f /etc/systemd/system/zflowd.service.d/prelogin.conf
test ! -e /usr/bin/zflow
test ! -e /usr/share/icons/hicolor/scalable/apps/io.zflow.zflow.svg
test ! -e /usr/share/icons/hicolor/symbolic/apps/io.zflow.zflow-symbolic.svg
test ! -e /usr/share/dbus-1/services/io.zflow.Desktop.service
# dpkg keeps the autostart conffile until purge; TryExec keeps it from running.
grep -qx 'TryExec=/usr/bin/zflow' /etc/xdg/autostart/io.zflow.desktop-agent.desktop
# Removal closes the ports again.
grep -qx 'delete allow zflow' /tmp/ufw.log
test ! -e /etc/ufw/applications.d/zflow
grep -qx -- '--permanent --zone=public --remove-service=zflow' /tmp/firewalld.log
test ! -e /etc/firewalld/services/zflow.xml
test ! -e /etc/udev/rules.d/71-zflow-capture.rules
cmp /etc/udev/71-zflow-capture.rules.disabled /tmp/expected.rules
test "$(stat -c '%U:%G' /etc/udev/71-zflow-capture.rules.disabled)" = root:root
if runuser -u zflow -- test -w /etc/udev; then echo 'Daemon can replace saved rules' >&2; exit 1; fi
cmp /etc/zflow/zflow.toml /tmp/expected.toml
dpkg -i /tmp/zflow.deb
cmp /etc/udev/rules.d/71-zflow-capture.rules /tmp/expected.rules
cmp /etc/zflow/zflow.toml /tmp/expected.toml
test -f /etc/xdg/autostart/io.zflow.desktop-agent.desktop
dpkg --purge zflow
test ! -e /etc/zflow/zflow.toml
test ! -e /etc/systemd/system/zflowd.service.d
test ! -e /etc/udev/rules.d/71-zflow-capture.rules
test ! -e /etc/xdg/autostart/io.zflow.desktop-agent.desktop
grep -qx 'identity sentinel' /var/lib/zflow/identity-test
# A package must not silently shadow a previous /usr/local installation.
mkdir -p /usr/local/bin
touch /usr/local/bin/zflow
if dpkg -i /tmp/zflow.deb; then echo 'Legacy installation was not rejected' >&2; exit 1; fi
test ! -e /usr/bin/zflow
echo 'Debian install, firewall, session files, upgrade, removal, reinstall, purge, and legacy detection passed.'
CONTAINER
