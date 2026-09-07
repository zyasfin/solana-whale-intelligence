-- Write-only encrypted secret store (Opsi A). Secrets set from the admin UI are
-- AES-256-GCM encrypted server-side and never returned to the browser.
-- `nonce` holds the 96-bit random nonce (hex); `ciphertext` holds base64(ciphertext||tag).

CREATE TABLE IF NOT EXISTS secret_store (
    name        text PRIMARY KEY,
    nonce       text NOT NULL,
    ciphertext  text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
