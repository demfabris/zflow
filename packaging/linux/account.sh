#!/usr/bin/env bash
# Shared account checks for archive and Debian installation.
ensure_service_account() {
    local group_record members passwd_record uid gid shell uid_min nologin

    if group_record="$(getent group "$SERVICE_GROUP")"; then
        IFS=: read -r _ _ _ members <<<"$group_record"
        [[ -z "$members" ]] || die "group $SERVICE_GROUP has members; remove them before installing"
    else
        groupadd --system "$SERVICE_GROUP"
    fi

    if passwd_record="$(getent passwd "$SERVICE_USER")"; then
        IFS=: read -r _ _ uid gid _ _ shell <<<"$passwd_record"
        uid_min="$(awk '$1 == "UID_MIN" { print $2; exit }' /etc/login.defs)"
        uid_min="${uid_min:-1000}"
        [[ "$uid" =~ ^[0-9]+$ && "$uid" -lt "$uid_min" ]] || die "$SERVICE_USER exists but is not a system account"
        [[ "$gid" == "$(getent group "$SERVICE_GROUP" | cut -d: -f3)" ]] || die "$SERVICE_USER does not use group $SERVICE_GROUP"
        case "$shell" in
            */nologin|*/false) ;;
            *) die "$SERVICE_USER has a login shell: $shell" ;;
        esac
        return
    fi

    nologin="$(command -v nologin || true)"
    [[ -n "$nologin" ]] || nologin="/usr/sbin/nologin"
    [[ -x "$nologin" ]] || die "could not find a nologin executable"
    useradd \
        --system \
        --gid "$SERVICE_GROUP" \
        --home-dir "$STATE_DIR" \
        --no-create-home \
        --shell "$nologin" \
        --comment "zflow input daemon" \
        "$SERVICE_USER"
}
