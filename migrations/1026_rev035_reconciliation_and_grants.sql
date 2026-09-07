-- ============================================================================
-- Signal Forge — Migration 1026: REV-035 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-035 required remediation #2, #3, #5.
--
-- THIS MIGRATION FIXES A REGRESSION I INTRODUCED IN 1025.
--
-- #3 (the serious one). 1025 classified risk from the intent's ORIGINAL action and
--    applied the kill-switch gate to EVERY transition. PLAN SWI §19 freezes:
--
--        When halted:
--        - No new entries/signatures by default.
--        - Reconciliation continues.
--        - Risk-reducing claim/withdraw/close may operate only under EXIT_ONLY.
--
--    So my gate blocked reconciliation exactly when it is most needed — while a
--    kill switch is active and the operator needs to know the true position of
--    already-submitted funds. The reviewer measured:
--
--        EXIT_ONLY + submitted BUY  -> confirmed              REJECTED
--        HALT     + submitted SELL  -> unknown_reconciliation REJECTED
--
--    Both intents stayed `submitted` with their reservation held, invisible. A
--    permissive hole (EXIT_ONLY letting a BUY be approved) is bad; blinding the
--    system during an emergency is worse. The kill switch exists to stop NEW
--    commitment of funds, not to stop LEARNING what happened to funds already
--    committed.
--
--    Fix: the gate keys on the TRANSITION, not on the intent's action. A
--    transition is classified as `entry` (advances toward new on-chain
--    commitment), `settlement` (records what already happened), or `unwind`
--    (releases/reduces). Kill switches gate `entry` only.
--
-- #2  1025 granted blanket `ALL TABLES IN SCHEMA public` DML to the runtime role.
--     Four REVOKEs protected the status/audit tables, but the runtime could still
--     mutate the INPUTS that transition_intent() trusts — it could insert its own
--     workspace mapping, pause a policy, delete a kill switch, or forge a social
--     identity. Tightening the door while leaving the guest list writable is not a
--     boundary.
--
-- #5  `swi_app` could read `secret_store` and `admin_sessions`. A read-only API
--     role has no business reading credentials or session material.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- #3. Transition purpose, derived from the frozen §16 state machine.
--
-- Keyed on (from_state, to_state) — the same key as `intent_transition_edges` —
-- so every legal edge has exactly one purpose and a new edge cannot be added
-- without classifying it.
--
--   entry      — moves an intent toward NEW on-chain commitment. Gated by kill
--                switches: HALT blocks all of it, EXIT_ONLY blocks it when the
--                intent's action is risk-adding.
--   settlement — records an outcome that already happened on chain, including
--                reconciliation. NEVER gated: refusing to record reality does
--                not undo it, it only hides it.
--   unwind     — cancels or fails safe, releasing the reservation. NEVER gated:
--                blocking an unwind would strand funds under a kill switch.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS intent_transition_purposes (
    from_state text NOT NULL,
    to_state   text NOT NULL,
    purpose    text NOT NULL CHECK (purpose IN ('entry', 'settlement', 'unwind')),
    PRIMARY KEY (from_state, to_state),
    FOREIGN KEY (from_state, to_state)
        REFERENCES intent_transition_edges (from_state, to_state)
);

INSERT INTO intent_transition_purposes (from_state, to_state, purpose) VALUES
    -- Forward path toward new commitment: this is what a kill switch must stop.
    ('proposed',  'approved',  'entry'),
    ('approved',  'reserved',  'entry'),
    ('reserved',  'built',     'entry'),
    ('built',     'simulated', 'entry'),
    ('simulated', 'signed',    'entry'),
    ('signed',    'submitted', 'entry'),
    -- Settlement / reconciliation: already submitted, only the OUTCOME is being
    -- recorded. PLAN SWI §19 "Reconciliation continues".
    ('submitted',              'confirmed',              'settlement'),
    ('submitted',              'unknown_reconciliation', 'settlement'),
    ('unknown_reconciliation', 'confirmed',              'settlement'),
    -- Unwind: releases the reservation or fails safe. Blocking these under a kill
    -- switch would hold funds hostage to the very switch meant to protect them.
    ('proposed',               'cancelled',   'unwind'),
    ('approved',               'cancelled',   'unwind'),
    ('reserved',               'failed_safe', 'unwind'),
    ('built',                  'failed_safe', 'unwind'),
    ('simulated',              'failed_safe', 'unwind'),
    ('signed',                 'failed_safe', 'unwind'),
    ('submitted',              'failed_safe', 'unwind'),
    ('unknown_reconciliation', 'failed_safe', 'unwind'),
    ('unknown_reconciliation', 'cancelled',   'unwind')
ON CONFLICT (from_state, to_state) DO UPDATE SET purpose = EXCLUDED.purpose;

GRANT SELECT ON intent_transition_purposes TO swi_transition_owner;

-- ---------------------------------------------------------------------------
-- #3. transition_intent(), with the kill-switch gate applied to `entry` only.
--
-- Everything 1025 got right is preserved verbatim: mandatory/workspace-matched/
-- current policy, workspace authority from execution_role_workspaces, the frozen
-- edge check, frozen reservation semantics, session_user in the audit row.
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

    -- (b) Policy is MANDATORY, workspace-matched, and CURRENT (from 1025).
    IF v_policy_ver IS NULL THEN
        RAISE EXCEPTION
            'intent % carries no automation policy version; refusing transition',
            p_intent_id;
    END IF;

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

    -- (c) Frozen transition edge. Checked BEFORE the kill-switch gate now,
    --     because the gate needs the edge's purpose.
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
        -- A legal edge nobody classified cannot be proven safe under a halt.
        RAISE EXCEPTION
            'transition % -> % has no frozen purpose; refusing transition',
            v_from_state, p_to_state;
    END IF;

    -- (d) Kill switches (§19) — ENTRY transitions only.
    --
    -- REV-035-#3: 1025 applied this to every transition, which blocked
    -- reconciliation and unwind under a halt. Settlement records what already
    -- happened; unwind releases funds. Neither creates new exposure, so neither is
    -- gated. This is the whole point of "Reconciliation continues".
    IF v_purpose = 'entry' THEN
        IF EXISTS (
            SELECT 1
              FROM kill_switches ks
             WHERE ks.active
               AND ks.mode = 'halt'
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
            -- An action nobody classified cannot be proven risk-reducing.
            RAISE EXCEPTION
                'intent % carries unclassified action %; refusing transition',
                p_intent_id, v_intent_act;
        END IF;

        IF v_risk_class = 'adding' AND EXISTS (
            SELECT 1
              FROM kill_switches ks
             WHERE ks.active
               AND ks.mode = 'exit_only'
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

    -- (e) Frozen reservation semantics.
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

-- ---------------------------------------------------------------------------
-- #2/#5. Withdraw the blanket grants 1025 handed out.
--
-- The runtime role must not be able to write the tables that AUTHORIZE it, nor
-- the tables that constitute safety state or identity truth. The reviewer proved
-- every one of these live, so each REVOKE below corresponds to a measured
-- capability, not a hypothetical one.
-- ---------------------------------------------------------------------------

-- Authorization inputs: the runtime could grant ITSELF a workspace.
REVOKE INSERT, UPDATE, DELETE ON execution_role_workspaces
    FROM swi_legacy_runtime, swi_app;

-- Policy inputs: the runtime could pause a policy or forge a policy version,
-- which is exactly what transition_intent() consults for authority.
REVOKE INSERT, UPDATE, DELETE ON automation_policies
    FROM swi_legacy_runtime, swi_app;
REVOKE INSERT, UPDATE, DELETE ON automation_policy_versions
    FROM swi_legacy_runtime, swi_app;

-- Safety state: the runtime could delete the kill switch restraining it.
REVOKE INSERT, UPDATE, DELETE ON kill_switches
    FROM swi_legacy_runtime, swi_app;

-- Risk classification and transition semantics are frozen vocabularies; only a
-- migration may change them.
REVOKE INSERT, UPDATE, DELETE ON intent_action_classes
    FROM swi_legacy_runtime, swi_app;
REVOKE INSERT, UPDATE, DELETE ON intent_transition_purposes
    FROM swi_legacy_runtime, swi_app;
REVOKE INSERT, UPDATE, DELETE ON intent_transition_edges
    FROM swi_legacy_runtime, swi_app;
REVOKE INSERT, UPDATE, DELETE ON intent_reservation_actions
    FROM swi_legacy_runtime, swi_app;

-- Identity truth: a forged social identity promotes funding corroboration to
-- `Reconstructed`. INSERT is the whole attack; 1025 only revoked UPDATE/DELETE.
REVOKE INSERT ON social_identities FROM swi_legacy_runtime, swi_app;

-- Intent creation is the execution worker's job, not the legacy runtime's.
REVOKE INSERT ON trade_intents FROM swi_legacy_runtime, swi_app;

-- #5. Credentials and session material: a read-only API role has no business
-- reading either, and neither does the legacy runtime.
--
-- `secret_store` and `admin_sessions` come from the LEGACY baseline (0004/1019a),
-- so they are absent on a canonical-fresh install. Revoking conditionally keeps
-- both lanes replayable — the same pattern 1025 uses for the `swi_legacy` schema.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['secret_store', 'admin_sessions', 'admin_users']
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = t
        ) THEN
            EXECUTE format('REVOKE ALL ON public.%I FROM swi_app', t);
            -- The legacy runtime needs its own admin/session tables to serve the
            -- admin UI, but never the secret store.
            IF t = 'secret_store' THEN
                EXECUTE format('REVOKE ALL ON public.%I FROM swi_legacy_runtime', t);
            END IF;
        END IF;
    END LOOP;
END$$;

REVOKE SELECT ON execution_role_workspaces FROM swi_app;

-- The migration ledger must be READABLE by the runtime roles.
--
-- Found while probing as the real role rather than as a superuser: `_migrations`
-- is created by `db::migrate()` at runtime, NOT by any migration file, so 1025's
-- `GRANT ... ON ALL TABLES IN SCHEMA public` never covered it — that grant only
-- affects tables existing when it runs. Without this, `db::ensure_schema_current`
-- (the read-only replacement for auto-migrate) fails as the runtime role with
-- `relation "public._migrations" does not exist`, and the service still cannot
-- start. SELECT only: the runtime verifies the schema, it never records a
-- migration.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = '_migrations'
    ) THEN
        EXECUTE 'GRANT SELECT ON public._migrations TO swi_legacy_runtime, swi_app';
    END IF;
END$$;

-- And for a database migrated for the first time AFTER this migration, where the
-- ledger is created by the migrator later in the same run.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT ON TABLES TO swi_legacy_runtime, swi_app;

-- Default privileges: a table created by a LATER migration must not silently
-- re-open blanket DML. 1025's `ALL TABLES` grants only covered tables existing at
-- the time; this makes the narrow posture the default going forward.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    REVOKE INSERT, UPDATE, DELETE ON TABLES FROM swi_app;

-- ---------------------------------------------------------------------------
-- Operator note
--
-- `swi_legacy_runtime` keeps SELECT everywhere plus DML on the operational tables
-- it actually owns the workflow for (raw_events, trades, tokens, wallets, graph,
-- telegram_*, funding_radar_*). What it lost is the ability to write the tables
-- that decide whether it is allowed to act at all.
--
-- Verify after applying:
--   SELECT has_table_privilege('swi_legacy_runtime','kill_switches','DELETE');  -- f
--   SELECT has_table_privilege('swi_legacy_runtime','social_identities','INSERT'); -- f
--   SELECT has_table_privilege('swi_app','secret_store','SELECT');              -- f
-- ---------------------------------------------------------------------------
