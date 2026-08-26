#!/usr/bin/env bash
#
# install.sh - install solana-whale-intelligence on a Debian/Ubuntu host
# (Proxmox VM or LXC). Idempotent: safe to run multiple times.
#
# Usage:
#   sudo bash deploy/install.sh [path-to-linux-binary]
#   (default binary path: ./solana-whale-intelligence)
#
set -euo pipefail

SERVICE_NAME="solana-whale-intelligence"
APP_USER="solana-intel"
APP_GROUP="solana-intel"
INSTALL_DIR="/opt/solana-whale-intelligence"
ENV_FILE="/etc/${SERVICE_NAME}.env"
UNIT_DEST="/etc/systemd/system/${SERVICE_NAME}.service"
BINARY_SRC="${1:-./solana-whale-intelligence}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '[install] %s\n' "$*"; }

if [[ "${EUID}" -ne 0 ]]; then
    echo "ERROR: must run as root (use sudo)." >&2
    exit 1
fi

# --- locate payloads -------------------------------------------------------
# config.toml / migrations are resolved from the repo layout first (deploy/
# sits one level below the project root), then from the current directory.
CONFIG_SRC=""
for cand in "${SCRIPT_DIR}/../config.toml" "./config.toml"; do
    if [[ -f "${cand}" ]]; then CONFIG_SRC="${cand}"; break; fi
done
MIGRATIONS_SRC=""
for cand in "${SCRIPT_DIR}/../migrations" "./migrations"; do
    if [[ -d "${cand}" ]]; then MIGRATIONS_SRC="${cand}"; break; fi
done

if [[ ! -f "${BINARY_SRC}" ]]; then
    echo "ERROR: Linux binary not found at '${BINARY_SRC}'." >&2
    echo "       Build it for x86_64-unknown-linux-gnu first (see deploy/README.md)." >&2
    exit 1
fi

# --- install directory -----------------------------------------------------
mkdir -p "${INSTALL_DIR}"

# --- system user / group (idempotent) --------------------------------------
if ! getent group "${APP_GROUP}" >/dev/null; then
    log "creating group ${APP_GROUP}"
    groupadd --system "${APP_GROUP}"
fi
if ! id -u "${APP_USER}" >/dev/null 2>&1; then
    log "creating user ${APP_USER}"
    useradd --system --gid "${APP_GROUP}" \
        --home-dir "${INSTALL_DIR}" --shell /usr/sbin/nologin \
        --comment "solana-whale-intelligence service" "${APP_USER}"
fi

# --- env file placeholder (created once, never overwritten) ----------------
if [[ ! -f "${ENV_FILE}" ]]; then
    log "writing env placeholder ${ENV_FILE}"
    cat > "${ENV_FILE}" <<'ENVEOF'
# solana-whale-intelligence environment
# Loaded by systemd via EnvironmentFile=-/etc/solana-whale-intelligence.env
# Uncomment and fill in real values on the target host. Keep secrets out of git.

# PostgreSQL connection for the intelligence service
#DATABASE_URL=postgres://postgres:postgres@localhost:5432/whale_intel

# Helius API keys (add HELIUS_KEY_2..N as needed)
#HELIUS_KEY_1=

# GMGN Agent API key (query-only enrichment; never a trading key)
#GMGN_API_KEY=

# Telegram MTProto app credentials (from https://my.telegram.org)
#TG_API_ID=
#TG_API_HASH=
# Encrypted MTProto session file (must be writable by the solana-intel user)
#TG_SESSION_PATH=/opt/solana-whale-intelligence/tg-session.bin

# Telegram Bot API (outbound research alerts only)
#TELEGRAM_BOT_TOKEN=
#TELEGRAM_CHAT_ID=

# Robinhood Chain (Arbitrum Orbit L2) shares the Helius account; its EVM RPC
# endpoint derives from [helius].base_url + [chains.robinhood].helius_slug in
# config.toml and authenticates with HELIUS_KEY_1. No separate RPC env var.

# Admin panel password hash (base64-encoded bcrypt).
# Generate with: /opt/solana-whale-intelligence/solana-whale-intelligence db hash-password
#ADMIN_PASSWORD_HASH_B64=
ENVEOF
    chown root:"${APP_GROUP}" "${ENV_FILE}"
    chmod 0640 "${ENV_FILE}"
else
    log "${ENV_FILE} already exists - leaving untouched"
fi

# --- binary ----------------------------------------------------------------
log "installing binary from ${BINARY_SRC}"
install -m 0755 "${BINARY_SRC}" "${INSTALL_DIR}/solana-whale-intelligence"

# --- config.toml (copied once; local edits are preserved) ------------------
if [[ -n "${CONFIG_SRC}" ]]; then
    if [[ ! -f "${INSTALL_DIR}/config.toml" ]]; then
        log "installing config.toml from ${CONFIG_SRC}"
        install -m 0644 "${CONFIG_SRC}" "${INSTALL_DIR}/config.toml"
    else
        log "config.toml already present in ${INSTALL_DIR} - leaving untouched"
    fi
else
    log "WARNING: config.toml not found (looked in repo root and cwd) - copy it to ${INSTALL_DIR} manually"
fi

# --- migrations (refreshed on every run) -----------------------------------
if [[ -n "${MIGRATIONS_SRC}" ]]; then
    log "installing migrations from ${MIGRATIONS_SRC}"
    rm -rf "${INSTALL_DIR}/migrations"
    cp -a "${MIGRATIONS_SRC}" "${INSTALL_DIR}/migrations"
else
    log "WARNING: migrations/ not found (looked in repo root and cwd) - copy it to ${INSTALL_DIR} manually"
fi

# --- ownership -------------------------------------------------------------
chown -R "${APP_USER}:${APP_GROUP}" "${INSTALL_DIR}"

# --- systemd unit ----------------------------------------------------------
log "installing systemd unit ${UNIT_DEST}"
install -m 0644 "${SCRIPT_DIR}/solana-whale-intelligence.service" "${UNIT_DEST}"
systemctl daemon-reload

# --- next steps ------------------------------------------------------------
cat <<EOF

Install complete. The service was NOT enabled or started.

Next steps:

  1. Edit the environment file and fill in real values:
       sudoedit ${ENV_FILE}

  2. Run database migrations:
       sudo -u ${APP_USER} bash -c 'set -a; . ${ENV_FILE}; set +a; cd ${INSTALL_DIR} && ./solana-whale-intelligence db migrate'

  3. (Admin panel only) Generate a password hash and paste the printed
     ADMIN_PASSWORD_HASH_B64=... line into ${ENV_FILE}:
       ${INSTALL_DIR}/solana-whale-intelligence db hash-password

  4. Enable and start the daemon:
       systemctl enable --now ${SERVICE_NAME}
       systemctl status ${SERVICE_NAME}

  5. Watch logs:
       journalctl -u ${SERVICE_NAME} -f

  To run the admin panel instead of the daemon: edit ${UNIT_DEST},
  comment the 'run' ExecStart, uncomment the 'watch serve-admin' ExecStart,
  then: systemctl daemon-reload && systemctl restart ${SERVICE_NAME}
EOF
