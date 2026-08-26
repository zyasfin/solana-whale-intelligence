# Deploy Notes

Catatan operasional ringkas untuk deploy `solana-whale-intelligence`.
Dokumentasi resmi tetap di `deploy/README.md` — file ini adalah ringkasan prosedur agar mudah diingat.

## 1. Repo

- GitHub private: `https://github.com/zyasfin/solana-whale-intelligence`
- Branch: `master`
- Remote: `origin`

## 2. Server target

- Tailscale IP: `100.122.34.87`
- Admin panel berjalan di port `8790` (redirect ke `/login`)
- SSH ditangani oleh **Tailscale SSH** (bukan sshd biasa).
  Sebelum bisa SSH, mesin harus di-authorize via device-auth link yang muncul saat SSH dicoba:
  `https://login.tailscale.com/a/<session>`

## 3. UI dashboard

- `static/index.html` di-embed ke binary via `include_str!("../static/index.html")`
  di `src/api.rs` (konstanta `DASHBOARD_HTML`).
- Konsekuensinya: perubahan pada `static/index.html` **TIDAK** akan muncul di server
  sampai binary di-rebuild (Linux) dan di-redeploy.
  Mengganti file HTML di server saja tidak cukup.

## 4. Build binary Linux

Development di Windows, deploy ke Linux. Binary harus untuk target
`x86_64-unknown-linux-gnu`. Cara: build di WSL2 (Ubuntu).

Langkah:

1. Pastikan Rust di WSL (`rustup`).
2. `rustup target add x86_64-unknown-linux-gnu`
3. Pasang dependency: `build-essential pkg-config libssl-dev`
4. Build:
   ```
   cargo build --release --target x86_64-unknown-linux-gnu
   ```

Binary output:

```
target/x86_64-unknown-linux-gnu/release/solana-whale-intelligence
```

## 5. Deploy

Sesuai `deploy/README.md`:

1. Copy binary + `config.toml` + `migrations/` + `deploy/` ke server via
   scp/rsync ke `/root/swi-deploy/`.
2. Jalankan `sudo bash deploy/install.sh <binary>` (idempotent).
3. Isi `/etc/solana-whale-intelligence.env` — minimal:
   - `DATABASE_URL`
   - `HELIUS_KEY_1`
   - `ADMIN_PASSWORD_HASH_B64`
4. Jalankan `db migrate`.
5. Jalankan `systemctl enable --now solana-whale-intelligence`.
6. Admin panel: edit service unit untuk
   `watch serve-admin --bind ...:8790` (sesuai port yang dipakai),
   lalu `daemon-reload` + `restart`.
7. Log: `journalctl -u solana-whale-intelligence -f`.

## 6. Update kedepannya

1. Commit ke git.
2. Build binary Linux di WSL.
3. scp binary.
4. Re-run `install.sh`.
5. `systemctl restart solana-whale-intelligence`.
