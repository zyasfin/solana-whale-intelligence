-- ============================================================================
-- Signal Forge — Migration 1027: REV-037 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-037 required remediation #1, #2, #3, #4.
--
-- FOUR HIGH FINDINGS, THREE OF THEM IN CODE I WROTE IN 1026.
--
-- F01  I found the `_migrations` gap myself in REV-036 and closed only half of it:
--      I added `GRANT SELECT` and never revoked the INSERT/UPDATE/DELETE the role
--      inherited from 1025's blanket grant. The reviewer inserted a filename into
--      the ledger and `ensure_schema_current()` then passed for a migration that
--      was never applied. So the read-only replacement I built for auto-migrate
--      could be falsified by the very role it was meant to constrain. There was no
--      checksum either, so file CONTENTS could change unnoticed
--      (`HASH_MISMATCH_ACCEPTED=yes`).
--
-- F02  My reconciliation fix was incomplete. The policy gate runs BEFORE the
--      purpose lookup, so settlement and unwind still stop when a policy is
--      rotated or paused — ordinary operations. Same bug as REV-035, different
--      cause. I only ever tested the active/current-policy path.
--
-- F03  1026 was a DENYLIST over 1025's blanket DML. I closed the six tables the
--      reviewer named and left `decision_bundles` and `signer_checks` fully
--      mutable: reject -> approve -> delete. That is precisely the
--      "fix the instance, not the class" pattern I claimed to have swept.
--
-- F04  I rewrote both kill-switch queries in 1026 and did not add `workspace_id`,
--      although the column exists. One tenant's global HALT stopped every
--      workspace.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- F01. The migration ledger becomes migration-only and checksum-bound.
--
-- `_migrations` is created by `db::migrate()` at runtime rather than by a
-- migration file, so it is never covered by grants issued here at DDL time. The
-- posture is therefore stated twice: explicit REVOKEs for the table as it exists
-- now, and DEFAULT PRIVILEGES so a ledger created later by the migrator inherits
-- SELECT-only.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = '_migrations'
    ) THEN
        -- No runtime role may claim a migration was applied.
        EXECUTE 'REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public._migrations
                 FROM swi_legacy_runtime, swi_app, PUBLIC';
        EXECUTE 'GRANT SELECT ON public._migrations TO swi_legacy_runtime, swi_app';

        -- Checksum binding: a recorded filename is not evidence that the file
        -- applied is the file on disk. `sha256` is nullable because rows recorded
        -- by earlier versions predate it; `db::ensure_schema_current` treats a NULL
        -- as "unverifiable" and reports it rather than accepting it.
        IF NOT EXISTS (
            SELECT 1 FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = '_migrations'
               AND column_name = 'sha256'
        ) THEN
            EXECUTE 'ALTER TABLE public._migrations ADD COLUMN sha256 text';
        END IF;
    END IF;
END$$;

-- A ledger created by a LATER first-time migration must land in the same posture.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE INSERT, UPDATE, DELETE ON TABLES FROM swi_legacy_runtime;

-- ---------------------------------------------------------------------------
-- F03. Replace the denylist with an explicit allowlist.
--
-- 1025 granted DML on ALL tables and 1026 subtracted the ones the reviewer named.
-- Subtraction cannot be complete: `decision_bundles` and `signer_checks` were
-- never named, so they stayed writable, and any table added by a future migration
-- would start writable too.
--
-- So: withdraw everything, then grant back only what each role's actual queries
-- need. The lists below are derived from the INSERT/UPDATE/DELETE statements in
-- `src/*.rs` — the pre-freeze runtime's real write surface.
-- ---------------------------------------------------------------------------
REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON ALL TABLES IN SCHEMA public
    FROM swi_legacy_runtime, swi_app;

-- SELECT stays broad: reading is not the risk being addressed, and the pre-freeze
-- binary reads widely. Credential and session material is re-restricted below.
GRANT SELECT ON ALL TABLES IN SCHEMA public TO swi_legacy_runtime;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO swi_app;

-- Ingest / observation / scoring: the tables this runtime owns the workflow for.
DO $$
DECLARE
    t text;
    owned text[] := ARRAY[
        -- raw ingest and normalization
        'raw_events', 'trades', 'transfers', 'tokens', 'wallets',
        'chain_sync_state', 'market_snapshots',
        -- graph and clustering
        'funding_edges', 'funding_observations', 'wallet_clusters',
        'wallet_cluster_members', 'wallet_labels', 'wallet_scores',
        -- provider observations
        'gmgn_token_observations', 'gmgn_wallet_observations',
        -- telegram ingest
        'telegram_channels', 'telegram_messages', 'telegram_mentions',
        -- research output owned by this runtime
        'signals', 'signal_evaluations', 'alerts', 'narratives',
        'narrative_evidence', 'funding_radar_cases', 'funding_radar_events'
    ];
BEGIN
    FOREACH t IN ARRAY owned
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            EXECUTE format(
                'GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO swi_legacy_runtime', t);
        END IF;
    END LOOP;
END$$;

-- Sequences for the tables above.
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO swi_legacy_runtime;

-- ---------------------------------------------------------------------------
-- F03. Authoritative append-only truth: INSERT allowed, rewriting NEVER.
--
-- `decision_bundles` and `signer_checks` are point-in-time decision/signer truth
-- under PLAN SWI. The reviewer drove `reject -> approve -> deleted`. Evidence you
-- can rewrite is not evidence.
--
-- Note what this actually produces, because it is STRICTER than "append-only": the
-- allowlist above does not grant INSERT on these tables either, so the runtime
-- ends up with SELECT only. That is deliberate and it is the honest posture — the
-- pre-freeze binary never writes them (no INSERT statement for either table exists
-- in `src/*.rs`), and the authoritative writer PLAN SWI calls for does not exist
-- yet. When that writer is built it gets INSERT explicitly, as its own role.
-- Granting INSERT now "for later" would hand out authority nobody is using.
-- The REVOKEs below are still stated explicitly rather than left implicit, so a
-- future blanket GRANT cannot quietly restore UPDATE/DELETE.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    t text;
    append_only text[] := ARRAY[
        'decision_bundles', 'decision_components', 'signer_checks',
        'recent_events', 'social_identities', 'intent_transitions',
        'evidence_refs', 'caller_provenance'
    ];
BEGIN
    FOREACH t IN ARRAY append_only
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            -- Explicit and unconditional: no UPDATE, no DELETE, no TRUNCATE, for
            -- any runtime role or PUBLIC.
            EXECUTE format(
                'REVOKE UPDATE, DELETE, TRUNCATE ON public.%I
                   FROM swi_legacy_runtime, swi_app, PUBLIC', t);
        END IF;
    END LOOP;
END$$;

-- `social_identities` INSERT stays revoked (1026): a forged row promotes funding
-- corroboration to `Reconstructed`. Identity truth needs an authoritative writer,
-- which does not exist yet — so nobody writes it rather than everybody.

-- ---------------------------------------------------------------------------
-- F02/F05. Credentials and admin session material.
--
-- 1026 revoked ALL on `secret_store` from `swi_legacy_runtime` as well as
-- `swi_app`. That was wrong and I would have shipped a broken admin UI: the
-- pre-freeze binary's own admin routes INSERT/DELETE `secret_store` and manage
-- `admin_sessions` (`src/admin.rs`). Denying the API role is correct; denying the
-- role that implements the feature is an outage, not a boundary.
--
-- So the legacy runtime keeps its own admin tables, and `swi_app` — the canonical
-- read-only API role — gets nothing.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['secret_store', 'admin_sessions', 'admin_users',
                             'admin_settings', 'login_attempts']
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM swi_app, PUBLIC', t);
            EXECUTE format(
                'GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO swi_legacy_runtime', t);
        END IF;
    END LOOP;
END$$;

-- Legacy archive schema (bridged databases only): same allowlist posture.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'swi_legacy') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA swi_legacy TO swi_legacy_runtime';
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA swi_legacy
                 TO swi_legacy_runtime';
        EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA swi_legacy TO swi_legacy_runtime';
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- F04. Kill switches are workspace-scoped; system-global stops are explicit.
--
-- `kill_switches.workspace_id` exists and both my 1026 queries ignored it, so a
-- global row belonging to workspace 2 halted workspace 1
-- (`OTHER_WORKSPACE_KILL=REJECTED`).
--
-- A platform operator does need a stop that crosses tenants, but that must be
-- stated rather than implied by a NULL tenant id. `system_scope` marks it, and
-- only a migration or a superuser can set it — the runtime cannot write this table
-- at all.
-- ---------------------------------------------------------------------------
ALTER TABLE kill_switches
    ADD COLUMN IF NOT EXISTS system_scope boolean NOT NULL DEFAULT false;

-- One literal, not adjacent literals: PostgreSQL concatenates adjacent string
-- literals across newlines, but the SQL parser used to review these files does not,
-- and a migration nobody can parse cannot be reviewed.
COMMENT ON COLUMN kill_switches.system_scope IS 'true = platform-wide stop that deliberately crosses every workspace. false (default) = a tenant switch, affecting only its own workspace_id. REV-037-F04: a NULL workspace_id must never mean "all tenants" by accident.';

-- Pre-existing rows: a row with no workspace was previously behaving as
-- system-wide, so preserve that observable behaviour explicitly rather than
-- silently narrowing a live safety control during an upgrade.
UPDATE kill_switches
   SET system_scope = true
 WHERE workspace_id IS NULL
   AND system_scope = false;

-- ---------------------------------------------------------------------------
-- F02/F04. transition_intent(): purpose FIRST, then purpose-appropriate gates.
--
-- Order of checks now:
--   (a) intent exists and has a workspace
--   (b) caller is authorized for that workspace
--   (c) frozen edge + frozen purpose        <-- moved UP, was after policy
--   (d) policy: current+active for `entry`; existence+workspace for the rest
--   (e) kill switches: `entry` only, scoped to this workspace or system-wide
--   (f) frozen reservation semantics, audit row, status update
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION transition_intent(
    p_intent_id bigint,
    p_to_state  text,
    p_reason    text DEFAULT NULL
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    v_from_state   text;
    v_action       text;
    v_workspace_id bigint;
    v_chain        text;
    v_wallet_id    bigint;
    v_policy_ver   bigint;
    v_intent_act   text;
    v_risk_class   text;
    v_purpose      text;
    v_caller       text := session_user;   -- the CONNECTED role, not the definer
BEGIN
    SELECT status, workspace_id, chain, wallet_address_id,
           automation_policy_version_id, action
      INTO v_from_state, v_workspace_id, v_chain, v_wallet_id,
           v_policy_ver, v_intent_act
      FROM trade_intents
     WHERE id = p_intent_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'intent % not found', p_intent_id;
    END IF;

    -- (a) Workspace authority.
    IF v_workspace_id IS NULL THEN
        RAISE EXCEPTION
            'intent % has no workspace; cannot authorize transition', p_intent_id;
    END IF;

    -- (b) Caller authority for THIS workspace. Unchanged, and deliberately still
    --     first: an unauthorized caller learns nothing further.
    IF NOT EXISTS (
        SELECT 1 FROM execution_role_workspaces
         WHERE role_name = v_caller
           AND workspace_id = v_workspace_id
    ) THEN
        RAISE EXCEPTION
            'role % is not authorized for workspace % (intent %)',
            v_caller, v_workspace_id, p_intent_id
            USING HINT = 'grant authority in execution_role_workspaces';
    END IF;

    -- (c) Frozen edge and purpose, BEFORE any policy or kill-switch test
    --     (REV-037-F02). This is the ordering bug: in 1026 the policy test ran
    --     first, so a rotated or paused policy blocked settlement and unwind even
    --     though neither creates new exposure.
    IF NOT EXISTS (
        SELECT 1 FROM intent_transition_edges
         WHERE from_state = v_from_state AND to_state = p_to_state
    ) THEN
        RAISE EXCEPTION 'illegal intent transition % -> %', v_from_state, p_to_state;
    END IF;

    SELECT purpose INTO v_purpose
      FROM intent_transition_purposes
     WHERE from_state = v_from_state AND to_state = p_to_state;

    IF v_purpose IS NULL THEN
        RAISE EXCEPTION
            'transition % -> % has no frozen purpose; refusing transition',
            v_from_state, p_to_state;
    END IF;

    -- (d) Policy, at the strength the purpose warrants.
    --
    -- Every intent must carry a policy version regardless of purpose: an execution
    -- with no policy provenance is not auditable, and that check is not weakened.
    IF v_policy_ver IS NULL THEN
        RAISE EXCEPTION
            'intent % carries no automation policy version; refusing transition',
            p_intent_id;
    END IF;

    IF v_purpose = 'entry' THEN
        -- New commitment must be authorized by the policy in force RIGHT NOW.
        IF NOT EXISTS (
            SELECT 1
              FROM automation_policy_versions apv
              JOIN automation_policies ap ON ap.id = apv.policy_id
             WHERE apv.id = v_policy_ver
               AND ap.status = 'active'
               AND ap.workspace_id = v_workspace_id
               AND ap.current_version_id = apv.id
        ) THEN
            RAISE EXCEPTION
                'automation policy version % is not the active current version for workspace % (intent %)',
                v_policy_ver, v_workspace_id, p_intent_id;
        END IF;
    ELSE
        -- Settlement and unwind record or release something already committed
        -- under a policy that was current AT THE TIME. Requiring it to still be
        -- current would mean an ordinary policy rotation freezes reconciliation
        -- and strands reservations. What must still hold is that the persisted
        -- policy is real and belongs to THIS workspace — so the audit trail is
        -- intact and the evidence cannot be borrowed from another tenant.
        IF NOT EXISTS (
            SELECT 1
              FROM automation_policy_versions apv
              JOIN automation_policies ap ON ap.id = apv.policy_id
             WHERE apv.id = v_policy_ver
               AND ap.workspace_id = v_workspace_id
        ) THEN
            RAISE EXCEPTION
                'automation policy version % does not belong to workspace % (intent %)',
                v_policy_ver, v_workspace_id, p_intent_id;
        END IF;
    END IF;

    -- (e) Kill switches (§19) — ENTRY only, and workspace-scoped (REV-037-F04).
    IF v_purpose = 'entry' THEN
        IF EXISTS (
            SELECT 1
              FROM kill_switches ks
             WHERE ks.active
               AND ks.mode = 'halt'
               -- either an explicit platform-wide stop, or one belonging to THIS
               -- workspace. Another tenant's switch is not authority here.
               AND (ks.system_scope OR ks.workspace_id = v_workspace_id)
               AND (
                     ks.scope = 'global'
                  OR (ks.scope = 'chain'  AND ks.scope_key = v_chain)
                  OR (ks.scope = 'wallet' AND ks.scope_key = (
                         SELECT wa.address FROM wallet_addresses wa WHERE wa.id = v_wallet_id
                     ))
               )
        ) THEN
            RAISE EXCEPTION
                'an active kill switch (halt) blocks entry transition % -> % for intent %',
                v_from_state, p_to_state, p_intent_id;
        END IF;

        SELECT risk_class INTO v_risk_class
          FROM intent_action_classes
         WHERE action = v_intent_act;

        IF v_risk_class IS NULL THEN
            RAISE EXCEPTION
                'intent % carries unclassified action %; refusing transition',
                p_intent_id, v_intent_act;
        END IF;

        IF v_risk_class = 'adding' AND EXISTS (
            SELECT 1
              FROM kill_switches ks
             WHERE ks.active
               AND ks.mode = 'exit_only'
               AND (ks.system_scope OR ks.workspace_id = v_workspace_id)
               AND (
                     ks.scope = 'global'
                  OR (ks.scope = 'chain'  AND ks.scope_key = v_chain)
                  OR (ks.scope = 'wallet' AND ks.scope_key = (
                         SELECT wa.address FROM wallet_addresses wa WHERE wa.id = v_wallet_id
                     ))
               )
        ) THEN
            RAISE EXCEPTION
                'EXIT_ONLY is active: risk-adding action % is not permitted (intent %)',
                v_intent_act, p_intent_id;
        END IF;
    END IF;

    -- (f) Frozen reservation semantics.
    SELECT reservation_action INTO v_action
      FROM intent_reservation_actions
     WHERE to_state = p_to_state;

    IF v_action IS NULL THEN
        RAISE EXCEPTION 'no frozen reservation action for state %', p_to_state;
    END IF;

    INSERT INTO intent_transitions
        (intent_id, from_state, to_state, reservation_action, reason, actor_role)
    VALUES (p_intent_id, v_from_state, p_to_state, v_action, p_reason, v_caller);

    UPDATE trade_intents SET status = p_to_state WHERE id = p_intent_id;
END;
$$;

ALTER FUNCTION transition_intent(bigint, text, text) OWNER TO swi_transition_owner;
REVOKE ALL ON FUNCTION transition_intent(bigint, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION transition_intent(bigint, text, text) TO swi_executor;

-- The function reads these while running as its owner.
GRANT SELECT ON kill_switches, intent_action_classes, intent_transition_purposes,
                intent_transition_edges, intent_reservation_actions,
                automation_policies, automation_policy_versions,
                execution_role_workspaces, trade_intents, wallet_addresses
    TO swi_transition_owner;

-- ---------------------------------------------------------------------------
-- Verify after applying:
--   SELECT has_table_privilege('swi_legacy_runtime','_migrations','INSERT');       -- f
--   SELECT has_table_privilege('swi_legacy_runtime','decision_bundles','UPDATE');  -- f
--   SELECT has_table_privilege('swi_legacy_runtime','signer_checks','DELETE');     -- f
--   SELECT has_table_privilege('swi_legacy_runtime','decision_bundles','INSERT');  -- f
--   SELECT has_table_privilege('swi_legacy_runtime','tokens','INSERT');            -- t
--   SELECT has_table_privilege('swi_legacy_runtime','secret_store','INSERT');      -- t
--   SELECT has_table_privilege('swi_app','secret_store','SELECT');                 -- f
-- ---------------------------------------------------------------------------
