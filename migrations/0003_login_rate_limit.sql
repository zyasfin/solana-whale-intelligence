-- Login rate limiting: failed-attempt counter store (IP-keyed, time-windowed).

CREATE TABLE IF NOT EXISTS login_attempts (
    id           bigserial PRIMARY KEY,
    key          text NOT NULL,           -- "ip:<ip>" (or "global" when IP unknown)
    attempted_at timestamptz NOT NULL DEFAULT now(),
    success      boolean NOT NULL DEFAULT false,
    ip           text,
    user_agent   text
);

CREATE INDEX IF NOT EXISTS login_attempts_key_idx ON login_attempts (key, attempted_at);
CREATE INDEX IF NOT EXISTS login_attempts_prune_idx ON login_attempts (attempted_at);
