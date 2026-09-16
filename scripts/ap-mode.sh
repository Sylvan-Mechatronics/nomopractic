#!/usr/bin/env bash
# ap-mode.sh — manage the nomon Wi-Fi Soft AP via NetworkManager.
#
# Usage:
#   ap-mode.sh up      — activate the nomon-<last4> hotspot
#   ap-mode.sh down    — deactivate the hotspot
#   ap-mode.sh status  — print current AP state ("up" or "down")
#
# The SSID is derived from the last 4 hex digits of the wlan0 MAC address.
# The WPA2 passphrase is read from NOMON_AP_PASSPHRASE_PATH (default:
# /var/lib/nomon/ap_passphrase) — a long random secret nomothetic generates,
# deliberately distinct from the 8-digit pairing code (an 8-digit WPA2 PSK is
# crackable offline from a captured handshake). If that file is absent (an
# older nomothetic), the script falls back to NOMON_PAIRING_SECRET_PATH with a
# warning. The AP is served at 192.168.4.1 with NetworkManager shared-mode DHCP
# (`ipv4.method shared`) on 192.168.4.0/24.

set -euo pipefail

AP_PASSPHRASE_PATH="${NOMON_AP_PASSPHRASE_PATH:-/var/lib/nomon/ap_passphrase}"
PAIRING_SECRET_PATH="${NOMON_PAIRING_SECRET_PATH:-/var/lib/nomon/pairing_secret}"
IFACE="wlan0"
CON_NAME="nomon-softap"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_get_mac_last4() {
    local mac
    mac=$(cat "/sys/class/net/${IFACE}/address" 2>/dev/null || true)
    if [[ -z "${mac}" ]]; then
        echo "0000"
        return
    fi
    # Strip colons, take last 4 hex chars
    echo "${mac//:/}" | tr '[:upper:]' '[:lower:]' | grep -o '.\{4\}$'
}

_get_ssid() {
    echo "nomon-$(_get_mac_last4)"
}

_get_passphrase() {
    local source
    if [[ -f "${AP_PASSPHRASE_PATH}" ]]; then
        source="${AP_PASSPHRASE_PATH}"
    elif [[ -f "${PAIRING_SECRET_PATH}" ]]; then
        echo "WARNING: ${AP_PASSPHRASE_PATH} not found; falling back to the pairing" \
             "secret as the WPA2 passphrase (weak — upgrade nomothetic)" >&2
        source="${PAIRING_SECRET_PATH}"
    else
        echo "ERROR: no AP passphrase file: ${AP_PASSPHRASE_PATH} (nor ${PAIRING_SECRET_PATH})" >&2
        exit 1
    fi
    local secret
    secret=$(tr -d '[:space:]' < "${source}")
    # WPA2 requires 8-63 characters; enforce the standard bounds.
    if [[ ${#secret} -lt 8 ]]; then
        echo "ERROR: AP passphrase must be at least 8 characters (WPA2 requirement)" >&2
        exit 1
    fi
    if [[ ${#secret} -gt 63 ]]; then
        echo "ERROR: AP passphrase must be at most 63 characters (WPA2 requirement)" >&2
        exit 1
    fi
    echo "${secret}"
}

_connection_exists() {
    nmcli -t -f NAME connection show 2>/dev/null | grep -q "^${CON_NAME}$"
}

_connection_active() {
    nmcli -t -f NAME connection show --active 2>/dev/null | grep -q "^${CON_NAME}$"
}

# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------

cmd_up() {
    if _connection_active; then
        # Keep the stored PSK current even while up; it takes effect on the
        # next activation (a live change would drop connected clients).
        # Best-effort only: the AP is already serving clients, so a missing or
        # unreadable passphrase file must not turn this into a hard failure —
        # it would also skip the service start below.
        local current
        if current=$(_get_passphrase 2>/dev/null); then
            nmcli connection modify "${CON_NAME}" wifi-sec.psk "${current}" 2>/dev/null || true
        else
            echo "WARNING: could not read the AP passphrase; leaving the stored PSK unchanged" >&2
        fi
        echo "already up"
        # Ensure the AP service is running even if the AP was already active.
        sudo systemctl start nomothetic-ap.service 2>/dev/null || true
        exit 0
    fi

    # Disconnect any active station-mode connection on wlan0 before activating
    # the AP. Without this, NetworkManager rejects the AP activation with
    # "base network connection was interrupted".
    if nmcli -t -f DEVICE,STATE device 2>/dev/null | grep -q "^${IFACE}:connected"; then
        nmcli device disconnect "${IFACE}" 2>/dev/null || true
        sleep 2
    fi

    local ssid passphrase
    ssid=$(_get_ssid)
    passphrase=$(_get_passphrase)

    if _connection_exists; then
        # Connection profile exists but is not active. Re-apply the passphrase
        # first: profiles created by older builds carry the 8-digit pairing
        # code as their PSK, and the passphrase may have been rotated since.
        nmcli connection modify "${CON_NAME}" wifi-sec.psk "${passphrase}"
        nmcli connection up "${CON_NAME}"
    else
        # Create a new hotspot connection profile.
        nmcli connection add \
            type wifi \
            ifname "${IFACE}" \
            con-name "${CON_NAME}" \
            autoconnect no \
            ssid "${ssid}" \
            -- \
            wifi.mode ap \
            wifi-sec.key-mgmt wpa-psk \
            wifi-sec.psk "${passphrase}" \
            ipv4.method shared \
            ipv4.addresses "192.168.4.1/24" \
            ipv6.method disabled

        nmcli connection up "${CON_NAME}"
    fi

    # Start the AP service now that the AP interface is active.
    # nomothetic-ap.service runs the device API over plain HTTP on
    # 192.168.4.1:8080.  No [Install] section — only live while the
    # Soft AP is active (ADR-016).
    sudo systemctl start nomothetic-ap.service 2>/dev/null || true

    echo "up: SSID=${ssid}"
}

cmd_down() {
    # Stop the AP service before AP teardown so that port 8080 on
    # 192.168.4.1 is no longer reachable.
    sudo systemctl stop nomothetic-ap.service 2>/dev/null || true

    if _connection_active; then
        nmcli connection down "${CON_NAME}"
        echo "down"
    else
        echo "already down"
    fi
}

cmd_status() {
    if _connection_active; then
        echo "up"
    else
        echo "down"
    fi
}

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

case "${1:-}" in
    up)     cmd_up ;;
    down)   cmd_down ;;
    status) cmd_status ;;
    *)
        echo "Usage: $(basename "$0") up|down|status" >&2
        exit 1
        ;;
esac
