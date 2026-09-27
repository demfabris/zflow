#!/usr/bin/env bash
# Open zflow's UDP ports in an active ufw or a running firewalld, or close them.
# It never turns a firewall on, and closing removes only the rules it added.
# A firewall that refuses a change gets a warning; installation continues.
set -Eeuo pipefail
IFS=$'\n\t'

readonly UFW_PROFILE="/etc/ufw/applications.d/zflow"
readonly FIREWALLD_SERVICE="/etc/firewalld/services/zflow.xml"
readonly PORTS="UDP ports 43119 (input) and 43120 (pairing)"

die() {
    printf 'zflow firewall: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'zflow firewall: %s\n' "$*" >&2
}

ufw_active() {
    command -v ufw >/dev/null 2>&1 && LC_ALL=C ufw status 2>/dev/null | grep -qx 'Status: active'
}

firewalld_running() {
    command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1
}

open_ports() {
    if ufw_active; then
        [[ ! -L "$UFW_PROFILE" ]] || die "refusing symlink: $UFW_PROFILE"
        install -d -m 0755 "${UFW_PROFILE%/*}"
        printf '[zflow]\ntitle=zflow\ndescription=Keyboard and pointer sharing between paired computers\nports=43119/udp|43120/udp\n' \
            > "$UFW_PROFILE"
        chmod 0644 "$UFW_PROFILE"
        if ufw allow zflow >/dev/null; then
            printf 'Opened %s in ufw (application profile zflow).\n' "$PORTS"
        else
            warn "ufw refused the zflow profile. Allow $PORTS yourself."
        fi
    fi
    if firewalld_running; then
        [[ ! -L "$FIREWALLD_SERVICE" ]] || die "refusing symlink: $FIREWALLD_SERVICE"
        install -d -m 0755 "${FIREWALLD_SERVICE%/*}"
        cat > "$FIREWALLD_SERVICE" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<service>
  <short>zflow</short>
  <description>Keyboard and pointer sharing between paired computers.</description>
  <port protocol="udp" port="43119"/>
  <port protocol="udp" port="43120"/>
</service>
XML
        chmod 0644 "$FIREWALLD_SERVICE"
        # firewalld reads new service files only on reload.
        if firewall-cmd --reload >/dev/null && firewall-cmd --permanent --add-service=zflow >/dev/null \
            && firewall-cmd --add-service=zflow >/dev/null; then
            printf 'Opened %s in firewalld (service zflow, zone %s).\n' "$PORTS" "$(firewall-cmd --get-default-zone)"
        else
            warn "firewalld refused the zflow service. Allow $PORTS yourself."
        fi
    fi
}

close_ports() {
    local zone
    local -a tool zones
    if [[ -f "$UFW_PROFILE" ]]; then
        # ufw keeps rules while it is off, and needs the profile to find this one.
        if ! command -v ufw >/dev/null 2>&1; then
            rm -f -- "$UFW_PROFILE"
        elif ufw delete allow zflow >/dev/null; then
            rm -f -- "$UFW_PROFILE"
            printf 'Removed the zflow rule from ufw.\n'
        else
            warn "ufw kept the zflow rule. Remove it with: ufw delete allow zflow"
        fi
    fi
    [[ -f "$FIREWALLD_SERVICE" ]] || return 0
    if firewalld_running; then
        tool=(firewall-cmd --permanent)
    elif command -v firewall-offline-cmd >/dev/null 2>&1; then
        tool=(firewall-offline-cmd)
    else
        rm -f -- "$FIREWALLD_SERVICE"
        return 0
    fi
    # A zone that still names a deleted service fails to load, so keep the file
    # unless every zone let go of it.
    IFS=' ' read -r -a zones <<< "$("${tool[@]}" --get-zones)"
    for zone in "${zones[@]}"; do
        if "${tool[@]}" --zone="$zone" --query-service=zflow >/dev/null 2>&1 \
            && ! "${tool[@]}" --zone="$zone" --remove-service=zflow >/dev/null; then
            warn "firewalld kept the zflow service in zone $zone."
            return 0
        fi
    done
    rm -f -- "$FIREWALLD_SERVICE"
    if firewalld_running; then firewall-cmd --reload >/dev/null || warn 'firewalld did not reload.'; fi
    printf 'Removed the zflow service from firewalld.\n'
}

[[ "$EUID" -eq 0 ]] || die "run as root"
case "${1:-}" in
    open) open_ports ;;
    close) close_ports ;;
    *) die "usage: firewall.sh open|close" ;;
esac
