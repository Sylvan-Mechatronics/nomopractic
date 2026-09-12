#!/usr/bin/env bash
# deploy.sh — Build locally or download from GitHub, verify, atomically install, and restart nomopractic.
#
# Usage:
#   ./scripts/deploy.sh [--local] [<version>] [<pi-host>]
#
# Arguments:
#   --local   Build and deploy the current local code (aarch64 cross-compile).
#             Bypasses GitHub release downloads.
#   version   Git tag to deploy, e.g. "v0.2.0". If omitted, the script fetches
#             and deploys the latest release from GitHub. Ignored if --local.
#   pi-host   SSH host (user@host or plain hostname). Defaults to
#             NOMON_PI_HOST env var. If empty, runs locally on the Pi.
#
# Environment:
#   NOMON_PI_HOST       SSH target (overridden by pi-host arg)
#   NOMON_SSH_KEY       Path to SSH private key (optional)
#   NOMON_GITHUB_REPO   GitHub "owner/repo" slug. Default: Sylvan-Mechatronics/nomopractic
#
# Examples:
#   # Deploy local code from a dev machine to a Pi over SSH:
#   ./scripts/deploy.sh --local pi@raspberrypi.local
#
#   # Deploy latest release from a dev machine to a Pi over SSH:
#   ./scripts/deploy.sh pi@raspberrypi.local
#
#   # Deploy a specific version from a dev machine to a Pi over SSH:
#   ./scripts/deploy.sh v0.2.0 pi@raspberrypi.local
#
#   # Deploy local code directly on the Pi (scp the script first):
#   ./scripts/deploy.sh --local
#
# The script (release mode):
#   1. Downloads the binary + SHA-256 checksum from GitHub Releases.
#   2. Verifies the checksum.
#   3. Atomically replaces /usr/local/bin/nomopractic (keeps previous as .bak).
#   4. Restarts nomopractic.service.
#   5. Verifies the service is running (waits up to 10 s).
#
# The script (--local mode):
#   1. Cross-compiles the current local code for aarch64 (if not already built).
#   2. Copies the binary to the Pi.
#   3. Atomically replaces /usr/local/bin/nomopractic (keeps previous as .bak).
#   4. Restarts nomopractic.service.
#   5. Verifies the service is running (waits up to 10 s).
#
# Rollback is available by running:
#   sudo cp /usr/local/bin/nomopractic.bak /usr/local/bin/nomopractic
#   sudo systemctl restart nomopractic
#
# Exit codes:
#   0  success
#   1  usage error
#   2  download / checksum / build failure
#   3  installation / service failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "${SCRIPT_DIR}")"

# ── Constants ────────────────────────────────────────────────────────────────

REPO="${NOMON_GITHUB_REPO:-Sylvan-Mechatronics/nomopractic}"
INSTALL_PATH="/usr/local/bin/nomopractic"
AP_MODE_INSTALL_PATH="/usr/local/bin/ap-mode.sh"
SERVICE="nomopractic"
DOWNLOAD_DIR="$(mktemp -d)"

cleanup() {
    rm -rf "$DOWNLOAD_DIR"
}
trap cleanup EXIT

# Deploy-only variables that must NOT be written to the on-device env file.
_DEPLOY_EXCLUDE='^\s*(NOMON_PI_HOST|NOMON_SSH_KEY|NOMON_GITHUB_REPO|NOMON_SUDO_PASS)\s*='

# ── Load .env.device ──────────────────────────────────────────────────────────

ENV_FILE="${REPO_DIR}/.env.device"
if [[ -f "${ENV_FILE}" ]]; then
    while IFS= read -r line || [[ -n "${line}" ]]; do
        line="${line#"${line%%[![:space:]]*}"}"
        [[ "${line}" =~ ^# || -z "${line}" ]] && continue
        key="${line%%=*}"
        val="${line#*=}"
        val="${val%%#*}"
        val="${val#"${val%%[![:space:]]*}"}"
        val="${val%"${val##*[![:space:]]}"}"
        val="${val#\"}" ; val="${val%\"}"
        val="${val#\'}"; val="${val%\'}"
        case "${key}" in
            NOMON_PI_HOST|NOMON_SSH_KEY|NOMON_GITHUB_REPO|NOMON_SERVICE_USER|NOMON_SERVICE_GROUP|NOMON_SUDO_PASS)
                export "${key}=${val}" ;;
        esac
    done < "${ENV_FILE}"
fi

# Sanitize env-derived host/key values to remove any stray CR/LF or surrounding
# whitespace that may be present when editing .env on Windows or other editors.
NOMON_PI_HOST="${NOMON_PI_HOST:-}"
NOMON_SSH_KEY="${NOMON_SSH_KEY:-}"
NOMON_SUDO_PASS="${NOMON_SUDO_PASS:-}"
# Remove CR and LF, trim leading/trailing whitespace (do NOT trim password whitespace)
NOMON_PI_HOST="$(printf '%s' "${NOMON_PI_HOST}" | tr -d '\r\n' | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
NOMON_SSH_KEY="$(printf '%s' "${NOMON_SSH_KEY}" | tr -d '\r\n' | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
NOMON_SUDO_PASS="$(printf '%s' "${NOMON_SUDO_PASS}" | tr -d '\r\n')"

# Export cleaned values so later logic picks them up via ${NOMON_PI_HOST} etc.
export NOMON_PI_HOST NOMON_SSH_KEY NOMON_SUDO_PASS


# ── Argument parsing ─────────────────────────────────────────────────────────

BUILD_LOCAL=false
VERSION="${1:-}"
PI_HOST="${2:-${NOMON_PI_HOST:-}}"

# Check if first argument is --local flag
if [[ "${VERSION}" == "--local" ]]; then
    BUILD_LOCAL=true
    VERSION=""
    PI_HOST="${2:-${NOMON_PI_HOST:-}}"
fi

# Sanitize PI_HOST (remove any stray CR/LF and surrounding whitespace)
PI_HOST="$(printf '%s' "${PI_HOST}" | tr -d '\r\n' | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"

if [[ -n "${VERSION}" && ! "${VERSION}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: version must start with 'v' followed by semver (e.g. v0.2.0)" >&2
    exit 1
fi

# ── Resolve version / Build ──────────────────────────────────────────────────

if [[ "${BUILD_LOCAL}" == true ]]; then
    # Get version from Cargo.toml
    VERSION="$(grep -m1 'version' Cargo.toml | sed -E 's/.*version\s*=\s*"([^"]+)".*/\1/')"
    if [[ -z "${VERSION}" ]]; then
        echo "Error: could not determine version from Cargo.toml" >&2
        exit 2
    fi
    echo "==> Building nomopractic ${VERSION} locally for aarch64..."
    
    # Cross-compile for aarch64
    if ! command -v cross &> /dev/null; then
        echo "Error: 'cross' command not found. Install it with: cargo install cross" >&2
        exit 2
    fi

    # If CROSS_CONTAINER is set, export it so `cross` will use that container image.
    if [[ -n "${CROSS_CONTAINER:-}" ]]; then
        export CROSS_CONTAINER="${CROSS_CONTAINER}"
        echo "==> Using cross container: ${CROSS_CONTAINER}"
    fi

    # Allow passing additional cargo flags (e.g. --features ble)
    if ! cross build --target aarch64-unknown-linux-gnu --release ${CARGO_BUILD_FLAGS:-}; then
        echo "Error: cross-compilation failed" >&2
        exit 2
    fi
    
    BINARY_FILE="target/aarch64-unknown-linux-gnu/release/nomopractic"
    if [[ ! -f "${BINARY_FILE}" ]]; then
        echo "Error: compiled binary not found at ${BINARY_FILE}" >&2
        exit 2
    fi
    
    DISPLAY_VERSION="${VERSION}"
else
    # ── Resolve version ───────────────────────────────────────────────────────

    if [[ -z "${VERSION}" ]]; then
        echo "==> Resolving latest release from GitHub..."
        VERSION="$(curl -sf "https://api.github.com/repos/${REPO}/releases/latest" \
            | python3 -c 'import sys,json; print(json.load(sys.stdin)["tag_name"])')"
        if [[ -z "${VERSION}" ]]; then
            echo "Error: could not determine latest release from GitHub." >&2
            exit 1
        fi
        echo "  Latest release: ${VERSION}"
    fi

    # ── Download ─────────────────────────────────────────────────────────────

    BINARY_NAME="nomopractic-${VERSION}-aarch64-linux"
    BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
    BINARY_URL="${BASE_URL}/${BINARY_NAME}"
    SHA256_URL="${BASE_URL}/${BINARY_NAME}.sha256"

    BINARY_FILE="${DOWNLOAD_DIR}/${BINARY_NAME}"
    SHA256_FILE="${DOWNLOAD_DIR}/${BINARY_NAME}.sha256"

    echo "==> Downloading nomopractic ${VERSION} from GitHub Releases..."
    curl --fail --location --progress-bar --output "$BINARY_FILE" "$BINARY_URL"
    curl --fail --location --silent     --output "$SHA256_FILE" "$SHA256_URL"

    # ── Checksum verification ─────────────────────────────────────────────────

    echo "==> Verifying SHA-256 checksum..."
    (
        cd "$DOWNLOAD_DIR"
        # The .sha256 file may contain the full path; normalise to just the filename.
        EXPECTED="$(awk '{print $1}' "$SHA256_FILE")  ${BINARY_NAME}"
        echo "$EXPECTED" | sha256sum --check --status || {
            echo "Error: checksum mismatch for ${BINARY_FILE}" >&2
            exit 2
        }
    )
    echo "    OK: checksum verified"

    chmod +x "$BINARY_FILE"
    DISPLAY_VERSION="${VERSION}"
fi

# ── SSH helper ───────────────────────────────────────────────────────────────
# If PI_HOST is set we run everything remotely; otherwise we run locally.

if [[ -n "$PI_HOST" ]]; then
    SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)
    if [[ -n "${NOMON_SSH_KEY:-}" ]]; then
        SSH_OPTS+=(-i "$NOMON_SSH_KEY")
    fi
    _rsh() { ssh "${SSH_OPTS[@]}" "$PI_HOST" "$@"; }
    _rscp() { scp "${SSH_OPTS[@]}" "$@"; }
    ON_REMOTE=true
else
    _rsh() { bash -c "$@"; }
    _rscp() { cp "$1" "$2"; }  # local "scp" = cp
    ON_REMOTE=false
fi
# Shell-quoted form of the sudo password for safe injection into remote bash sessions.
_NOMON_SUDO_PASS_QUOTED="$(printf '%q' "${NOMON_SUDO_PASS:-}")"
copy_nomopractic_env() {
    if [[ ! -f "${ENV_FILE}" ]]; then
        echo "==> Warning: .env.device not found; skipping /etc/nomopractic/nomopractic.env creation." >&2
        return
    fi

    local filtered
    filtered="$(grep -vE "${_DEPLOY_EXCLUDE}" "${ENV_FILE}" \
        | grep -vE '^\s*#' \
        | grep -vE '^\s*$' || true)"

    if [[ -z "${filtered}" ]]; then
        echo "==> No deploy-safe env vars found; skipping /etc/nomopractic/nomopractic.env creation." >&2
        return
    fi

    if [[ -n "${PI_HOST}" ]]; then
        echo "==> Writing /etc/nomopractic/nomopractic.env on remote host..."
        # Write to a local temp file, copy it to the remote host, then move into place with sudo.
        local tmp_env_file
        tmp_env_file="$(mktemp)"
        printf '%s\n' "${filtered}" > "${tmp_env_file}"
        _rscp "${tmp_env_file}" "${PI_HOST}:/tmp/nomopractic_env.$$"
        ssh "${SSH_OPTS[@]}" "$PI_HOST" bash -s <<EOCOPYENV
NOMON_SUDO_PASS=${_NOMON_SUDO_PASS_QUOTED}
export NOMON_SUDO_PASS
if [[ -n "\${NOMON_SUDO_PASS:-}" ]]; then
    _askpass_script="\$(mktemp)"
    chmod 700 "\${_askpass_script}"
    cat > "\${_askpass_script}" <<'EOSUDOPASS'
#!/usr/bin/env sh
printf '%s\n' "\${NOMON_SUDO_PASS}"
EOSUDOPASS
    export SUDO_ASKPASS="\${_askpass_script}"
    trap 'rm -f "\${_askpass_script}"' EXIT
    sudo() { command sudo -A "\$@"; }
else
    sudo() { command sudo "\$@"; }
fi
sudo mkdir -p /etc/nomopractic
sudo mv -f /tmp/nomopractic_env.$$ /etc/nomopractic/nomopractic.env
# systemd reads EnvironmentFile as root; keep it out of world-readable space.
sudo chmod 640 /etc/nomopractic/nomopractic.env
sudo chown root:root /etc/nomopractic/nomopractic.env
EOCOPYENV
        rm -f "${tmp_env_file}"
    else
        echo "==> Writing /etc/nomopractic/nomopractic.env locally..."
        sudo mkdir -p /etc/nomopractic
        printf '%s\n' "${filtered}" | sudo tee /etc/nomopractic/nomopractic.env > /dev/null
        # systemd reads EnvironmentFile as root; keep it out of world-readable space.
        sudo chmod 640 /etc/nomopractic/nomopractic.env
        sudo chown root:root /etc/nomopractic/nomopractic.env
    fi
}
# ── Remote or local installation ─────────────────────────────────────────────

install_binary() {
    local src="$1"

    # Keep the current binary as a rollback target.
    if [[ -f "${INSTALL_PATH}" ]]; then
        sudo cp "${INSTALL_PATH}" "${INSTALL_PATH}.bak"
        echo "==> Previous binary saved to ${INSTALL_PATH}.bak"
    fi

    # Atomic swap: write to a temp file in the same directory, then rename.
    local tmp
    tmp="$(dirname "${INSTALL_PATH}")/.nomopractic.tmp.$$"
    sudo cp "$src" "$tmp"
    sudo chmod 755 "$tmp"
    sudo mv -f "$tmp" "${INSTALL_PATH}"
    echo "==> Installed to ${INSTALL_PATH}"
}

install_ap_mode_script() {
    local src="$1"

    sudo mkdir -p "$(dirname "${AP_MODE_INSTALL_PATH}")"
    sudo cp "$src" "${AP_MODE_INSTALL_PATH}"
    sudo chmod 755 "${AP_MODE_INSTALL_PATH}"
    echo "==> Installed to ${AP_MODE_INSTALL_PATH}"
}

copy_nomopractic_env

REMOTE_SVC_TMP="/tmp/nomopractic_service.$$"
REMOTE_TMPFILES="/tmp/nomon_tmpfiles.conf.$$"
REMOTE_AP_MODE_TMP="/tmp/ap-mode.sh.$$"
if [[ "$ON_REMOTE" == true ]]; then
    _rscp "${REPO_DIR}/systemd/nomopractic.service" "${PI_HOST}:${REMOTE_SVC_TMP}"
    # copy tmpfiles entry so remote deploy installs it
    _rscp "${REPO_DIR}/systemd/tmpfiles.d/nomon.conf" "${PI_HOST}:${REMOTE_TMPFILES}"
    _rscp "${REPO_DIR}/scripts/ap-mode.sh" "${PI_HOST}:${REMOTE_AP_MODE_TMP}"
    _rscp "${REPO_DIR}/systemd/nomon-softap.service"           "${PI_HOST}:/tmp/nomon-softap.service.$$"
    _rscp "${REPO_DIR}/systemd/nomon-softap-watchdog.service"  "${PI_HOST}:/tmp/nomon-softap-watchdog.service.$$"
    _rscp "${REPO_DIR}/systemd/nomon-softap-watchdog.timer"    "${PI_HOST}:/tmp/nomon-softap-watchdog.timer.$$"
    _rscp "${REPO_DIR}/systemd/polkit-1/rules.d/10-nomon-network.rules" "${PI_HOST}:/tmp/10-nomon-network.rules.$$"
fi

# ── I2C interface pre-flight (remote) ────────────────────────────────────────
# On a fresh reimage the Pi's I2C interface may not yet be enabled in the
# firmware.  We detect and enable it here so nomopractic can open /dev/i2c-1.
# If I2C was just enabled a full reboot is needed; deploy.sh handles that after
# the install so the service comes up automatically on the first boot.
_I2C_JUST_ENABLED=false
if [[ "$ON_REMOTE" == true ]]; then
    echo "==> Checking I2C interface on remote Pi..."
    _i2c_status="$(_rsh bash <<'I2C_CHECK'
raspi-config nonint get_i2c 2>/dev/null || echo 1
I2C_CHECK
    )"
    _i2c_status="$(printf '%s' "${_i2c_status}" | tr -d '[:space:]')"
    if [[ "${_i2c_status}" != "0" ]]; then
        echo "==> I2C not enabled; enabling via raspi-config (reboot will follow install)..."
        _rsh bash <<I2C_ENABLE
set -euo pipefail
NOMON_SUDO_PASS=${_NOMON_SUDO_PASS_QUOTED}
export NOMON_SUDO_PASS
if [[ -n "\${NOMON_SUDO_PASS:-}" ]]; then
    _asp="\$(mktemp)"
    chmod 700 "\${_asp}"
    cat > "\${_asp}" <<'EOSUDOPASS'
#!/usr/bin/env sh
printf '%s\n' "\${NOMON_SUDO_PASS}"
EOSUDOPASS
    export SUDO_ASKPASS="\${_asp}"
    trap 'rm -f "\${_asp}"' EXIT
    sudo() { command sudo -A "\$@"; }
else
    sudo() { command sudo "\$@"; }
fi
if command -v raspi-config >/dev/null 2>&1; then
    sudo raspi-config nonint do_i2c 0
else
    _cfg=/boot/firmware/config.txt
    [[ -f "\${_cfg}" ]] || _cfg=/boot/config.txt
    grep -qE '^dtparam=i2c_arm=on' "\${_cfg}" 2>/dev/null || \
        printf 'dtparam=i2c_arm=on\n' | sudo tee -a "\${_cfg}" >/dev/null
    printf 'i2c-dev\n' | sudo tee /etc/modules-load.d/i2c-dev.conf >/dev/null
fi
I2C_ENABLE
        _I2C_JUST_ENABLED=true
        echo "==> I2C enabled ✓ (Pi will reboot after install)"
    else
        echo "==> I2C already enabled ✓"
    fi
fi

echo "==> Installing binary..."

if [[ "$ON_REMOTE" == true ]]; then
    # Copy the verified binary to the Pi and run the install+restart there.
    REMOTE_TMP="/tmp/nomopractic.$$"
    _rscp "$BINARY_FILE" "${PI_HOST}:${REMOTE_TMP}"
    # If a local config.toml exists, upload it to the Pi for installation
    REMOTE_CONFIG_TMP=""
    if [[ -f "config.toml" ]]; then
        REMOTE_CONFIG_TMP="/tmp/nomopractic_config.$$"
        _rscp "config.toml" "${PI_HOST}:${REMOTE_CONFIG_TMP}"
    fi
    _rsh bash <<REMOTE
set -euo pipefail
NOMON_SUDO_PASS=${_NOMON_SUDO_PASS_QUOTED}
export NOMON_SUDO_PASS
if [[ -n "\${NOMON_SUDO_PASS:-}" ]]; then
    _askpass_script="\$(mktemp)"
    chmod 700 "\${_askpass_script}"
    cat > "\${_askpass_script}" <<'EOSUDOPASS'
#!/usr/bin/env sh
printf '%s\n' "\${NOMON_SUDO_PASS}"
EOSUDOPASS
    export SUDO_ASKPASS="\${_askpass_script}"
    trap 'rm -f "\${_askpass_script}"' EXIT
    sudo() { command sudo -A "\$@"; }
else
    sudo() { command sudo "\$@"; }
fi
INSTALL_PATH="${INSTALL_PATH}"
AP_MODE_INSTALL_PATH="${AP_MODE_INSTALL_PATH}"
SERVICE="${SERVICE}"
REMOTE_TMP="${REMOTE_TMP}"
REMOTE_CONFIG_TMP="${REMOTE_CONFIG_TMP}"
REMOTE_SVC_TMP="${REMOTE_SVC_TMP}"
REMOTE_TMPFILES="${REMOTE_TMPFILES}"
REMOTE_AP_MODE_TMP="${REMOTE_AP_MODE_TMP}"
REMOTE_POLKIT_RULES="/tmp/10-nomon-network.rules.$$"
_DEF_SVC_USER="${NOMON_SERVICE_USER:-root}"
_DEF_SVC_GROUP="${NOMON_SERVICE_GROUP:-nomon}"

if [[ -f "\${INSTALL_PATH}" ]]; then
    sudo cp "\${INSTALL_PATH}" "\${INSTALL_PATH}.bak"
    echo "==> Previous binary saved to \${INSTALL_PATH}.bak"
fi

tmp="\$(dirname "\${INSTALL_PATH}")/.nomopractic.tmp.\$\$"
sudo cp "\${REMOTE_TMP}" "\${tmp}"
sudo chmod 755 "\${tmp}"
sudo mv -f "\${tmp}" "\${INSTALL_PATH}"
rm -f "\${REMOTE_TMP}"
echo "==> Installed to \${INSTALL_PATH}"

echo "==> Installing ap-mode.sh..."
sudo mkdir -p "$(dirname "${AP_MODE_INSTALL_PATH}")"
sudo cp "${REMOTE_AP_MODE_TMP}" "${AP_MODE_INSTALL_PATH}"
sudo chmod 755 "${AP_MODE_INSTALL_PATH}"
rm -f "${REMOTE_AP_MODE_TMP}"
echo "==> Installed to ${AP_MODE_INSTALL_PATH}"

# ── Install systemd service file ─────────────────────────────────────────────
echo "==> Installing nomopractic.service..."
if ! command -v envsubst >/dev/null 2>&1; then
    echo "Error: envsubst not found. Install gettext-base: sudo apt-get install -y gettext-base" >&2
    exit 3
fi

# Source on-device env for variable substitution; fall back to script defaults.
# Read via sudo — the file is root-owned mode 640.
if [[ -f /etc/nomopractic/nomopractic.env ]]; then
    set -o allexport
    # shellcheck disable=SC1090
    source <(sudo cat /etc/nomopractic/nomopractic.env)
    set +o allexport
fi
export NOMON_SERVICE_USER="\${NOMON_SERVICE_USER:-\${_DEF_SVC_USER}}"
export NOMON_SERVICE_GROUP="\${NOMON_SERVICE_GROUP:-\${_DEF_SVC_GROUP}}"

_expanded="\$(envsubst '\$NOMON_SERVICE_USER \$NOMON_SERVICE_GROUP' < "\${REMOTE_SVC_TMP}")"
if [[ "\${_expanded}" != \[Unit\]* ]]; then
    echo "Error: rendered nomopractic.service is malformed (missing [Unit] header)." >&2
    echo "Rendered preview:" >&2
    printf '%s\n' "\${_expanded}" | head -n 10 >&2
    exit 3
fi
if ! printf '%s\n' "\${_expanded}" | grep -q '^ExecStart='; then
    echo "Error: rendered nomopractic.service is malformed (missing ExecStart)." >&2
    echo "Rendered preview:" >&2
    printf '%s\n' "\${_expanded}" | head -n 20 >&2
    exit 3
fi
printf '%s\n' "\${_expanded}" | sudo tee /etc/systemd/system/nomopractic.service > /dev/null
sudo chmod 644 /etc/systemd/system/nomopractic.service
rm -f "\${REMOTE_SVC_TMP}"
echo "==> nomopractic.service installed ✓"

# ── Install Soft AP systemd units ────────────────────────────────────────────
echo "==> Installing Soft AP systemd units..."
for _unit in nomon-softap.service nomon-softap-watchdog.service nomon-softap-watchdog.timer; do
    sudo cp "/tmp/\${_unit}.$$" "/etc/systemd/system/\${_unit}"
    sudo chmod 644 "/etc/systemd/system/\${_unit}"
    rm -f "/tmp/\${_unit}.$$"
    echo "==> \${_unit} installed ✓"
done
sudo systemctl enable nomon-softap-watchdog.timer
# Remove any legacy WantedBy symlink that would auto-start AP on nomopractic restart.
sudo rm -f /etc/systemd/system/nomopractic.service.wants/nomon-softap.service

# ── Install polkit rule for nmcli in non-interactive sessions ────────────────
echo "==> Installing polkit NetworkManager rule..."
sudo mkdir -p /etc/polkit-1/rules.d
sudo cp "\${REMOTE_POLKIT_RULES}" /etc/polkit-1/rules.d/10-nomon-network.rules
sudo chmod 644 /etc/polkit-1/rules.d/10-nomon-network.rules
rm -f "\${REMOTE_POLKIT_RULES}"
echo "==> 10-nomon-network.rules installed ✓"

# Add the service user to the 'netdev' group so the polkit rule above
# authorises nmcli calls from ap-mode.sh invoked by nomothetic-api.service.
_svc_user="\${NOMON_SERVICE_USER:-root}"
if [[ "\${_svc_user}" != "root" ]]; then
    echo "==> Adding \${_svc_user} to netdev group for NetworkManager access..."
    sudo usermod -aG netdev "\${_svc_user}" || true
    echo "==> \${_svc_user} added to netdev ✓"
fi

# Install a sudoers.d rule so the service user can start/stop the
# nomothetic-ap HTTP pairing service from ap-mode.sh without a password.
# This allows the AP pairing HTTP listener to be gated strictly on whether
# the Soft AP is active (see nomothetic ADR-015).
if [[ "\${_svc_user}" != "root" ]]; then
    _sudoers_tmp="\$(mktemp)"
    printf '%s ALL=(root) NOPASSWD: /usr/bin/systemctl start nomothetic-ap.service, /usr/bin/systemctl stop nomothetic-ap.service\n' \
        "\${_svc_user}" > "\${_sudoers_tmp}"
    if visudo -c -f "\${_sudoers_tmp}" >/dev/null 2>&1; then
        sudo install -m 440 -o root -g root "\${_sudoers_tmp}" /etc/sudoers.d/nomon-ap-service
        echo "==> Sudoers rule for nomothetic-ap.service installed ✓"
    else
        echo "WARNING: Generated sudoers rule failed visudo validation — not installed." >&2
    fi
    rm -f "\${_sudoers_tmp}"
fi

# Install tmpfiles.d entry for /var/lib/nomon (if provided)
if [[ -f "${REMOTE_TMPFILES}" ]]; then
    echo "==> Installing tmpfiles.d/nomon.conf..."
    sudo mkdir -p /etc/tmpfiles.d
    sudo mv -f "${REMOTE_TMPFILES}" /etc/tmpfiles.d/nomon.conf
    sudo chmod 644 /etc/tmpfiles.d/nomon.conf
    sudo systemd-tmpfiles --create /etc/tmpfiles.d/nomon.conf || true
fi

# Ensure /var/lib/nomon is owned by nomon so that the nomothetic service can
# write the pairing secret and JWT signing secret to this directory.
sudo chown -R "nomon:\${NOMON_SERVICE_GROUP:-nomon}" /var/lib/nomon || true

# If a config was uploaded, atomically install it to /etc/nomopractic
if [[ -n "\${REMOTE_CONFIG_TMP}" ]]; then
    echo "==> Installing config to /etc/nomopractic/config.toml..."
    sudo mkdir -p /etc/nomopractic
    if [[ -f /etc/nomopractic/config.toml ]]; then
        sudo cp /etc/nomopractic/config.toml /etc/nomopractic/config.toml.bak
        echo "==> Previous config saved to /etc/nomopractic/config.toml.bak"
    fi
    sudo mv -f "\${REMOTE_CONFIG_TMP}" /etc/nomopractic/config.toml
    sudo chmod 644 /etc/nomopractic/config.toml
    echo "==> Installed config to /etc/nomopractic/config.toml"
fi

sudo systemctl daemon-reload
sudo systemctl enable "\${SERVICE}"
_I2C_JUST_ENABLED="${_I2C_JUST_ENABLED}"
if [[ "\${_I2C_JUST_ENABLED}" == "true" ]]; then
    # I2C hardware was just enabled; a firmware reboot is needed to activate
    # /dev/i2c-1.  Enable the service so it auto-starts once the Pi is back.
    echo "==> I2C was just enabled; rebooting Pi to activate hardware interface..."
    sudo systemctl reboot &
    exit 0
fi

echo "==> Restarting \${SERVICE}.service..."
sudo systemctl restart "\${SERVICE}"

echo "==> Waiting for service to become active..."
for _ in \$(seq 1 10); do
    if systemctl is-active --quiet "\${SERVICE}"; then
        echo "==> \${SERVICE} is running (version ${DISPLAY_VERSION})"
        exit 0
    fi
    sleep 1
done

echo "Error: \${SERVICE} did not start within 10 seconds" >&2
sudo journalctl -u "\${SERVICE}" -n 30 --no-pager >&2
exit 3
REMOTE

# ── Post-install: wait for Pi after I2C activation reboot ────────────────────
if [[ "${_I2C_JUST_ENABLED}" == "true" ]]; then
    echo "==> Waiting for Pi to come back online after I2C activation reboot (up to 2 min)..."
    sleep 15
    _reconnect_ok=false
    for _attempt in $(seq 1 18); do
        if ssh "${SSH_OPTS[@]}" "$PI_HOST" true 2>/dev/null; then
            _reconnect_ok=true
            echo "==> Pi is back online ✓"
            break
        fi
        sleep 5
    done
    if [[ "${_reconnect_ok}" != "true" ]]; then
        echo "Error: Pi did not come back online within 2 minutes after reboot." >&2
        exit 3
    fi
    echo "==> Verifying ${SERVICE} started automatically after reboot..."
    for _ in $(seq 1 15); do
        if _rsh "systemctl is-active --quiet ${SERVICE}" 2>/dev/null; then
            echo "==> ${SERVICE} is running (version ${DISPLAY_VERSION})"
            exit 0
        fi
        sleep 2
    done
    echo "Error: ${SERVICE} did not start within 30 seconds after reboot." >&2
    _rsh "sudo journalctl -u ${SERVICE} -n 30 --no-pager" 2>&1 || true
    exit 3
fi

else
    # Local install (running on the Pi directly).

    # ── I2C interface pre-flight (local) ─────────────────────────────────────
    echo "==> Checking I2C interface..."
    if command -v raspi-config >/dev/null 2>&1; then
        _i2c_status="$(raspi-config nonint get_i2c 2>/dev/null || echo 1)"
        _i2c_status="$(printf '%s' "${_i2c_status}" | tr -d '[:space:]')"
        if [[ "${_i2c_status}" != "0" ]]; then
            echo "==> I2C not enabled; enabling via raspi-config..."
            sudo raspi-config nonint do_i2c 0
            sudo modprobe i2c-dev 2>/dev/null || true
            _I2C_JUST_ENABLED=true
            echo "==> I2C enabled ✓"
        else
            echo "==> I2C already enabled ✓"
        fi
    fi

    install_binary "$BINARY_FILE"
    install_ap_mode_script "${REPO_DIR}/scripts/ap-mode.sh"

    # If a local config.toml exists, install it to /etc/nomopractic
    if [[ -f "config.toml" ]]; then
        echo "==> Installing local config to /etc/nomopractic/config.toml..."
        sudo mkdir -p /etc/nomopractic
        if [[ -f /etc/nomopractic/config.toml ]]; then
            sudo cp /etc/nomopractic/config.toml /etc/nomopractic/config.toml.bak
            echo "==> Previous config saved to /etc/nomopractic/config.toml.bak"
        fi
        sudo cp config.toml /etc/nomopractic/config.toml
        sudo chmod 644 /etc/nomopractic/config.toml
        echo "==> Installed config to /etc/nomopractic/config.toml"
    fi

    # ── Install systemd service file ─────────────────────────────────────────────
    echo "==> Installing nomopractic.service..."
    if ! command -v envsubst >/dev/null 2>&1; then
        echo "Error: envsubst not found. Install gettext-base: sudo apt-get install -y gettext-base" >&2
        exit 3
    fi

    # Source on-device env; fall back to script defaults.
    # Read via sudo — the file is root-owned mode 640.
    if [[ -f /etc/nomopractic/nomopractic.env ]]; then
        set -o allexport
        # shellcheck disable=SC1090
        source <(sudo cat /etc/nomopractic/nomopractic.env)
        set +o allexport
    fi
    export NOMON_SERVICE_USER="${NOMON_SERVICE_USER:-root}"
    export NOMON_SERVICE_GROUP="${NOMON_SERVICE_GROUP:-nomon}"

    _expanded="$(envsubst '$NOMON_SERVICE_USER $NOMON_SERVICE_GROUP' < "${REPO_DIR}/systemd/nomopractic.service")"
    if [[ "${_expanded}" != \[Unit\]* ]]; then
        echo "Error: rendered nomopractic.service is malformed (missing [Unit] header)." >&2
        echo "Rendered preview:" >&2
        printf '%s\n' "${_expanded}" | head -n 10 >&2
        exit 3
    fi
    if ! printf '%s\n' "${_expanded}" | grep -q '^ExecStart='; then
        echo "Error: rendered nomopractic.service is malformed (missing ExecStart)." >&2
        echo "Rendered preview:" >&2
        printf '%s\n' "${_expanded}" | head -n 20 >&2
        exit 3
    fi
    printf '%s\n' "${_expanded}" | sudo tee /etc/systemd/system/nomopractic.service > /dev/null
    sudo chmod 644 /etc/systemd/system/nomopractic.service
    echo "==> nomopractic.service installed ✓"

    # ── Install Soft AP systemd units ────────────────────────────────────────────
    echo "==> Installing Soft AP systemd units..."
    for _unit in nomon-softap.service nomon-softap-watchdog.service nomon-softap-watchdog.timer; do
        sudo cp "${REPO_DIR}/systemd/${_unit}" "/etc/systemd/system/${_unit}"
        sudo chmod 644 "/etc/systemd/system/${_unit}"
        echo "==> ${_unit} installed ✓"
    done
    sudo systemctl enable nomon-softap-watchdog.timer
    # Remove any legacy WantedBy symlink that would auto-start AP on nomopractic restart.
    sudo rm -f /etc/systemd/system/nomopractic.service.wants/nomon-softap.service

    # ── Install polkit rule for nmcli in non-interactive sessions ────────────
    echo "==> Installing polkit NetworkManager rule..."
    sudo mkdir -p /etc/polkit-1/rules.d
    sudo cp "${REPO_DIR}/systemd/polkit-1/rules.d/10-nomon-network.rules" \
        /etc/polkit-1/rules.d/10-nomon-network.rules
    sudo chmod 644 /etc/polkit-1/rules.d/10-nomon-network.rules
    echo "==> 10-nomon-network.rules installed ✓"

    # Install tmpfiles.d entry for /var/lib/nomon so the pairing secret
    # directory exists with correct owner/permissions on boot.
    echo "==> Installing tmpfiles.d/nomon.conf..."
    sudo mkdir -p /etc/tmpfiles.d
    sudo cp "${REPO_DIR}/systemd/tmpfiles.d/nomon.conf" /etc/tmpfiles.d/nomon.conf
    sudo chmod 644 /etc/tmpfiles.d/nomon.conf
    sudo systemd-tmpfiles --create /etc/tmpfiles.d/nomon.conf || true

    # Ensure /var/lib/nomon is owned by nomon so that the nomothetic service
    # can write the pairing secret and JWT signing secret to this directory.
    sudo chown -R "nomon:${NOMON_SERVICE_GROUP:-nomon}" /var/lib/nomon || true

    sudo systemctl daemon-reload
    sudo systemctl enable  "${SERVICE}"

    if [[ "${_I2C_JUST_ENABLED:-false}" == "true" ]]; then
        echo "==> I2C was just enabled. A reboot is required to activate the hardware interface."
        echo "    nomopractic is installed and enabled; it will start automatically after reboot."
        echo "    Run: sudo reboot"
        exit 0
    fi

    echo "==> Restarting ${SERVICE}.service..."
    sudo systemctl restart "${SERVICE}"

    echo "==> Waiting for service to become active..."
    for _ in $(seq 1 10); do
        if systemctl is-active --quiet "${SERVICE}"; then
            echo "==> ${SERVICE} is running (version ${DISPLAY_VERSION})"
            exit 0
        fi
        sleep 1
    done

    echo "Error: ${SERVICE} did not start within 10 seconds" >&2
    sudo journalctl -u "${SERVICE}" -n 30 --no-pager >&2
    exit 3
fi
