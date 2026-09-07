-- ============================================================================
-- Signal Forge — Migration 1028: REV-039 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-039 F02, F03 (+ F05 ledger reassert).
--
-- TWO PATTERNS I HAVE NOW REPEATED ENOUGH TIMES TO NAME.
--
-- (1) "Fix the instance, not the class." REV-037 I fixed wallet joins and not token
--     joins. REV-038 I protected five admin tables and not the credential tables,
--     and two decision tables and not the append-only OPERATION class.
-- (2) "Stop at the first green evidence." I tested a fresh install and not an
--     upgrade; SELECT and not INSERT; the flagged branch and not its siblings.
--
-- So this migration is NOT written from REV-039's list of findings. It is written
-- from a MECHANICAL ENUMERATION of the source tree, and the enumeration found more
-- than the reviewer reported:
--
--   credential columns   -> reviewer named gmgn_keys, helius_keys.
--                           Enumerating `api_key|private_key_pem|rpc_url|
--                           ciphertext|token_hash|password_hash|secret` across
--                           every CREATE TABLE also found:
--                             gmgn_pubkeys.private_key_pem  (a SIGNING KEY)
--                             invites.token_hash
--                             sessions.token_hash
--
--   append-only tables   -> reviewer named 5. Deriving per-table INSERT vs
--                           UPDATE vs DELETE from every statement in src/*.rs
--                           found 18.
--
-- The same enumeration also protects me from the 1026 `secret_store` mistake,
-- where I revoked a table the binary itself writes and would have shipped a broken
-- admin UI. Tables needing UPDATE/DELETE are listed explicitly below, including the
-- ones whose need is invisible at a glance because it comes from
-- `ON CONFLICT ... DO UPDATE` rather than from an `UPDATE` statement.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- F02. Provider credentials are not readable by the canonical API role.
--
-- 1027 granted SELECT on ALL public tables to both roles and then subtracted five
-- admin tables. Subtraction by name is the same mistake as the denylist in 1026:
-- `gmgn_keys.api_key`, `helius_keys.api_key/rpc_url` and — worse, and unreported —
-- `gmgn_pubkeys.private_key_pem`, a signing key, stayed readable.
--
-- The legacy runtime keeps them because `src/gmgn.rs` and `src/helius.rs` read
-- their own pool keys. `swi_app`, the canonical read/API role, gets nothing.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    t text;
    -- Every table with a credential/secret/session column, from enumeration.
    secret_tables text[] := ARRAY[
        'gmgn_keys',        -- api_key
        'gmgn_pubkeys',     -- private_key_pem  (SIGNING KEY; not reported by review)
        'helius_keys',      -- api_key, rpc_url
        'secret_store',     -- nonce, ciphertext
        'admin_sessions',   -- token_hash
        'admin_users',
        'admin_settings',
        'login_attempts',
        'invites',          -- token_hash      (not reported by review)
        'sessions'          -- token_hash      (not reported by review)
    ];
BEGIN
    FOREACH t IN ARRAY secret_tables
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            -- The canonical API/read role never sees credential material.
            EXECUTE format('REVOKE ALL ON public.%I FROM swi_app, PUBLIC', t);
        END IF;
    END LOOP;
END$$;

-- The signing key is not readable by ANY runtime role: a private key belongs in
-- the signer, which is an isolated component under PLAN SWI principle #12.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'gmgn_pubkeys'
    ) THEN
        EXECUTE 'REVOKE ALL ON public.gmgn_pubkeys
                 FROM swi_app, swi_legacy_runtime, PUBLIC';
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- F03. Privileges are per OPERATION, derived from the source write surface.
--
-- 1027 granted SELECT+INSERT+UPDATE+DELETE per table. That let the runtime rewrite
-- observation payloads and delete evaluation rows — the reviewer demonstrated both
-- — because the grant was table-shaped while the risk is operation-shaped.
--
-- Two lists, both mechanically derived:
--   append_only  : source contains ONLY plain INSERT  -> SELECT + INSERT
--   mutable      : source contains UPDATE, DELETE, or ON CONFLICT DO UPDATE
--                  -> SELECT + INSERT + UPDATE + DELETE, with the reason recorded
-- ---------------------------------------------------------------------------

-- Start from nothing so a table added by a later migration is not born writable.
REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON ALL TABLES IN SCHEMA public
    FROM swi_legacy_runtime, swi_app, PUBLIC;

-- Append-only: evidence, observations, events, and derived research output. An
-- observation that can be rewritten is not an observation; a correction is a NEW
-- row that supersedes, which is the frozen archive-not-delete rule.
DO $$
DECLARE
    t text;
    append_only text[] := ARRAY[
        -- ingest / normalization (raw_events is NOT here: it has a retention job)
        'trades', 'transfers', 'tokens',
        -- graph and clustering
        'funding_edges', 'funding_observations', 'wallet_clusters', 'wallet_scores',
        -- provider observations (reviewer demonstrated payload rewrite)
        'gmgn_token_observations', 'gmgn_wallet_observations',
        -- telegram ingest
        'telegram_messages', 'telegram_mentions',
        -- research output
        'signals', 'signal_evaluations', 'alerts', 'narratives',
        'narrative_evidence', 'funding_radar_events',
        -- canonical recent-intelligence events
        'recent_events'
    ];
BEGIN
    FOREACH t IN ARRAY append_only
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            EXECUTE format('GRANT SELECT, INSERT ON public.%I TO swi_legacy_runtime', t);
            -- Stated explicitly so a later blanket GRANT cannot silently restore it.
            EXECUTE format(
                'REVOKE UPDATE, DELETE, TRUNCATE ON public.%I
                   FROM swi_legacy_runtime, swi_app, PUBLIC', t);
        END IF;
    END LOOP;
END$$;

-- Mutable, each with the reason it must stay mutable. Denying any of these would
-- be an outage, which is what I shipped in 1026 by revoking `secret_store` from
-- the role whose own admin routes write it.
DO $$
DECLARE
    r record;
    mutable text[][] := ARRAY[
        -- table                    reason (from src/*.rs)
        ARRAY['wallets',            'ON CONFLICT DO UPDATE (db.rs upsert_wallet)'],
        ARRAY['wallet_labels',      'UPDATE ... SET revoked_at (db.rs, admin.rs)'],
        ARRAY['wallet_cluster_members', 'ON CONFLICT DO UPDATE'],
        ARRAY['chain_sync_state',   'ON CONFLICT DO UPDATE (cursor advance)'],
        ARRAY['telegram_channels',  'ON CONFLICT DO UPDATE + status disable'],
        ARRAY['funding_radar_cases', 'UPDATE ... SET fanout_count/stage'],
        ARRAY['raw_events',         'DELETE retention job (db.rs:680)'],
        ARRAY['market_snapshots',   'DELETE retention job (db.rs:691)'],
        ARRAY['secret_store',       'admin routes upsert/delete (admin.rs)'],
        ARRAY['admin_sessions',     'UPDATE last_seen_at + DELETE logout/expiry (auth.rs)'],
        ARRAY['admin_settings',     'ON CONFLICT DO UPDATE (admin.rs:981)'],
        ARRAY['admin_users',        'admin management'],
        ARRAY['login_attempts',     'DELETE on success + expiry sweep (auth.rs)']
    ];
BEGIN
    FOR r IN SELECT mutable[i][1] AS t, mutable[i][2] AS why
               FROM generate_subscripts(mutable, 1) AS i
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = r.t
        ) THEN
            EXECUTE format(
                'GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO swi_legacy_runtime',
                r.t);
        END IF;
    END LOOP;
END$$;

-- Broad SELECT for the legacy runtime (it reads widely), MINUS the credential
-- tables revoked above — order matters, so this runs first and the credential
-- revoke is re-asserted after it.
GRANT SELECT ON ALL TABLES IN SCHEMA public TO swi_legacy_runtime;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO swi_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO swi_legacy_runtime;

-- Re-assert the credential and append-only postures AFTER the broad SELECT grant,
-- because `GRANT ... ON ALL TABLES` would otherwise have just undone them. This
-- ordering trap is exactly how 1025's blanket grant defeated my 1026 revokes.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['gmgn_keys', 'gmgn_pubkeys', 'helius_keys',
                             'secret_store', 'admin_sessions', 'admin_users',
                             'admin_settings', 'login_attempts', 'invites', 'sessions']
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM swi_app, PUBLIC', t);
        END IF;
    END LOOP;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = 'public' AND table_name = 'gmgn_pubkeys') THEN
        EXECUTE 'REVOKE ALL ON public.gmgn_pubkeys FROM swi_legacy_runtime';
    END IF;
END$$;

-- Authoritative decision/signer/audit truth: SELECT only for every runtime role.
-- The authoritative writer PLAN SWI calls for does not exist yet, so nobody writes
-- these rather than everybody.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['decision_bundles', 'decision_components', 'signer_checks',
                             'social_identities', 'intent_transitions', 'trade_intents',
                             'evidence_refs', 'caller_provenance',
                             'execution_role_workspaces', 'automation_policies',
                             'automation_policy_versions', 'kill_switches',
                             'intent_action_classes', 'intent_transition_purposes',
                             'intent_transition_edges', 'intent_reservation_actions']
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            EXECUTE format(
                'REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public.%I
                   FROM swi_legacy_runtime, swi_app, PUBLIC', t);
        END IF;
    END LOOP;
END$$;

-- ---------------------------------------------------------------------------
-- F05 (residual). Ledger protection re-asserted, including the NAMED roles.
--
-- 1027 revoked the named roles but runs once; `db::migrate()` re-asserted only
-- PUBLIC, so a restored or hand-granted explicit privilege survived a remigrate.
-- The Rust side now revokes the named roles too; this block keeps the SQL lane in
-- agreement.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = '_migrations'
    ) THEN
        EXECUTE 'REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public._migrations
                 FROM swi_legacy_runtime, swi_app, PUBLIC';
        EXECUTE 'GRANT SELECT ON public._migrations TO swi_legacy_runtime, swi_app';
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Legacy archive schema (bridged databases only) — SAME per-operation posture.
--
-- Found by probing as the real role on the LEGACY lane rather than the canonical
-- one, which is the "test the path you hope does not matter" rule applied to
-- myself. The first version of this migration closed `public.tokens` and left
-- `swi_legacy.tokens` wide open:
--
--     public.tokens      UPDATE = false
--     swi_legacy.tokens  UPDATE = true    <-- append-only silently bypassed
--
-- On a bridged database the runtime's `search_path` resolves `swi_legacy` FIRST, so
-- the archive copy is the one it actually writes. Protecting only `public` would
-- have been protection in the schema nobody uses. This is the same "fixed it in one
-- place" mistake as the wallet-vs-token joins, so the archive gets the identical
-- treatment rather than a blanket grant.
--
-- Every archived table (funding_edges, narratives, token_lifecycle_events, tokens,
-- wallet_clusters) is append-only in the source, so all five get SELECT+INSERT.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    t text;
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'swi_legacy') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA swi_legacy TO swi_legacy_runtime';
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA swi_legacy
                 FROM swi_legacy_runtime, swi_app, PUBLIC';
        EXECUTE 'GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA swi_legacy
                 TO swi_legacy_runtime';
        EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA swi_legacy
                 TO swi_legacy_runtime';

        -- Mutable archive tables, if any are added later by a further bridge run:
        -- none of the five currently archived tables is mutated by the source, so
        -- the list is deliberately empty rather than speculative.
        FOR t IN SELECT table_name FROM information_schema.tables
                  WHERE table_schema = 'swi_legacy'
                    AND table_name = ANY (ARRAY[]::text[])
        LOOP
            EXECUTE format(
                'GRANT UPDATE, DELETE ON swi_legacy.%I TO swi_legacy_runtime', t);
        END LOOP;

        EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA swi_legacy
                 GRANT SELECT, INSERT ON TABLES TO swi_legacy_runtime';
    END IF;
END$$;

-- Future tables default to read-only for both roles.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE INSERT, UPDATE, DELETE ON TABLES FROM swi_legacy_runtime, swi_app;

-- ---------------------------------------------------------------------------
-- Verify after applying:
--   SELECT has_table_privilege('swi_app','gmgn_keys','SELECT');                 -- f
--   SELECT has_table_privilege('swi_app','helius_keys','SELECT');               -- f
--   SELECT has_table_privilege('swi_legacy_runtime','gmgn_pubkeys','SELECT');   -- f
--   SELECT has_table_privilege('swi_legacy_runtime','gmgn_keys','SELECT');      -- t
--   SELECT has_table_privilege('swi_legacy_runtime','gmgn_token_observations','UPDATE'); -- f
--   SELECT has_table_privilege('swi_legacy_runtime','gmgn_token_observations','INSERT'); -- t
--   SELECT has_table_privilege('swi_legacy_runtime','signal_evaluations','DELETE');      -- f
--   SELECT has_table_privilege('swi_legacy_runtime','wallets','UPDATE');                -- t
--   SELECT has_table_privilege('swi_legacy_runtime','secret_store','UPDATE');           -- t
--   SELECT has_table_privilege('swi_legacy_runtime','raw_events','DELETE');             -- t
-- ---------------------------------------------------------------------------
