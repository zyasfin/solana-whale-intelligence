# Deploying solana-whale-intelligence (Proxmox / Debian / Ubuntu)

Target: a Debian/Ubuntu Proxmox VM or LXC with network access to PostgreSQL.
The app installs under `/opt/solana-whale-intelligence`, runs as the
unprivileged `solana-intel` system user, and is managed by systemd.

## 1. Build a Linux binary (development is on Windows)

You need an `x86_64-unknown-linux-gnu` release build — `cargo build` on
Windows produces a `.exe`, which will NOT work. A Linux build host (or WSL2)
or a cross tool is required. Pick ONE:

```powershell
# Option A - cargo-zigbuild (no Docker needed)
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-gnu

# Option B - cross (needs Docker Desktop running)
cargo install cross
cross build --release --target x86_64-unknown-linux-gnu
```

```bash
# Option C - WSL2: install Rust inside WSL and build natively
cargo build --release --target x86_64-unknown-linux-gnu
```

The binary lands at `target/x86_64-unknown-linux-gnu/release/solana-whale-intelligence`.

## 2. Copy files to the guest

The installer expects the repo layout — `deploy/`, `config.toml`, and
`migrations/` side by side — plus the Linux binary.

```bash
# from the project root (Git Bash / WSL on the Windows host)
GUEST=root@192.0.2.10   # your Proxmox guest IP

ssh $GUEST mkdir -p /root/swi-deploy
scp -r deploy config.toml migrations $GUEST:/root/swi-deploy/
scp target/x86_64-unknown-linux-gnu/release/solana-whale-intelligence $GUEST:/root/swi-deploy/

# or in one shot with rsync:
rsync -avz deploy config.toml migrations \
  target/x86_64-unknown-linux-gnu/release/solana-whale-intelligence \
  $GUEST:/root/swi-deploy/
```

## 3. Run the installer (on the guest)

```bash
cd /root/swi-deploy
sudo bash deploy/install.sh ./solana-whale-intelligence
```

(The binary path argument defaults to `./solana-whale-intelligence`. The
script is idempotent — safe to re-run.)

It creates the `solana-intel` system user/group, writes the placeholder
`/etc/solana-whale-intelligence.env` (only if missing — never overwrites),
installs the binary + `config.toml` + `migrations/` into
`/opt/solana-whale-intelligence`, installs the systemd unit, and runs
`daemon-reload`. It does NOT enable or start the service.

## 4. Configure

```bash
sudoedit /etc/solana-whale-intelligence.env
```

Uncomment and fill at minimum `DATABASE_URL` and `HELIUS_KEY_1`; add
`GMGN_API_KEY`, `TG_API_ID` / `TG_API_HASH` / `TG_SESSION_PATH`, and
`TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` as needed. The Robinhood EVM
endpoint is derived from the Helius base URL in `config.toml` — no separate
RPC env var.

Run migrations, and (only if you will use the admin panel) generate a
password hash:

```bash
sudo -u solana-intel bash -c 'set -a; . /etc/solana-whale-intelligence.env; set +a; \
  cd /opt/solana-whale-intelligence && ./solana-whale-intelligence db migrate'

/opt/solana-whale-intelligence/solana-whale-intelligence db hash-password
# paste the printed ADMIN_PASSWORD_HASH_B64=... into the env file
```

## 5. Enable and start

```bash
systemctl enable --now solana-whale-intelligence
systemctl status solana-whale-intelligence
```

## 6. Logs

```bash
journalctl -u solana-whale-intelligence -f          # follow live
journalctl -u solana-whale-intelligence --since "1 hour ago"
```

## 7. Admin panel (optional, port 8789)

Edit `/etc/systemd/system/solana-whale-intelligence.service`: comment the
`... run` ExecStart line and uncomment the
`... watch serve-admin --bind 0.0.0.0:8789` line, then:

```bash
systemctl daemon-reload
systemctl restart solana-whale-intelligence
```

Requires `ADMIN_PASSWORD_HASH_B64` to be set in the env file (step 4).

## 8. Tailscale (recommended for admin-panel access)

Do not expose `0.0.0.0:8789` on a public interface. Instead, put the guest on
your tailnet and reach the panel over the tailnet IP:

```bash
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up
tailscale ip -4        # e.g. 100.x.y.z
```

Then set the admin ExecStart to `watch serve-admin --bind 100.x.y.z:8789`
(daemon-reload + restart) and open `http://100.x.y.z:8789` from any tailnet
device. If you keep `--bind 0.0.0.0:8789`, restrict non-tailnet access with
Tailscale ACLs and/or the guest firewall.

## Updating later

Copy the new binary to the guest and re-run
`sudo bash deploy/install.sh <path-to-binary>`, then
`systemctl restart solana-whale-intelligence`. The env file and any local
`config.toml` edits are preserved; `migrations/` is refreshed.
