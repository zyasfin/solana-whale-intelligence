-- ============================================================================
-- Signal Forge — Migration 1025: REV-033 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-033 required remediation #2, #3, #4, #5.
--
-- FOUR RESIDUALS THIS CLOSES
--
-- #2  swi_legacy is unreachable for a least-privilege role. The bridge (1000)
--     archives the legacy tables, and 1024 fixed the connection, but no role was
--     ever granted USAGE on the archive schema:
--         has_schema_privilege(role,'swi_legacy','USAGE') = false
--         current_schemas(false) = {public}
--     so the legacy runtime still could not see its own tables.
--
-- #3  `transition_intent()` accepted intents with NO policy at all, a policy
--     belonging to ANOTHER workspace, and a STALE policy version. My 1024 guard
--     read `IF v_policy_ver IS NOT NULL THEN ... END IF`, so a NULL policy skipped
--     the check entirely — a hole I wrote into the check itself.
--
-- #4  EXIT_ONLY was not enforced at this boundary: an active EXIT_ONLY kill switch
--     plus a BUY intent was accepted, because 1024 only blocked `mode='halt'` and
--     deferred EXIT_ONLY to an "application policy engine" that has no caller.
--
-- #5  No least-privilege runtime role existed at all; the migrator role and the
--     runtime role were the same, and `.env.example` still pointed at `postgres`.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- #2/#5. Least-privilege runtime roles.
--
--   swi_legacy_runtime — the pre-freeze binary. Reads/writes legacy tables in the
--                        archive schema plus the non-colliding legacy tables that
--                        stayed in `public`.
--   swi_app            — the canonical read/API runtime (public schema only).
--
-- Both are NOLOGIN here; the deployment attaches credentials out of band. Neither
-- owns any table, so neither can bypass the append-only or status guards.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'swi_legacy_runtime') THEN
        CREATE ROLE swi_legacy_runtime NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'swi_app') THEN
        CREATE ROLE swi_app NOLOGIN;
    END IF;
END$$;

-- The archive schema only exists on a bridged database; grant conditionally so a
-- canonical-fresh install is unaffected.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'swi_legacy') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA swi_legacy TO swi_legacy_runtime';
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA swi_legacy TO swi_legacy_runtime';
        EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA swi_legacy TO swi_legacy_runtime';
        -- Tables archived by a LATER bridge run must be reachable too.
        EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA swi_legacy GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO swi_legacy_runtime';
    END IF;
END$$;

-- Both runtimes need `public` (the legacy runtime for its non-colliding tables).
GRANT USAGE ON SCHEMA public TO swi_legacy_runtime, swi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO swi_legacy_runtime;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO swi_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO swi_legacy_runtime;

-- The status/append-only guards must hold for these roles as well: they are not
-- owners, and the explicit revokes from 1019/1023/1024 target PUBLIC, which they
-- inherit. Re-assert the two that matter most so a later blanket GRANT above
-- cannot quietly re-open them.
REVOKE UPDATE ON trade_intents FROM swi_legacy_runtime, swi_app;
REVOKE INSERT, UPDATE, DELETE ON intent_transitions FROM swi_legacy_runtime, swi_app;
REVOKE UPDATE, DELETE ON recent_events FROM swi_legacy_runtime, swi_app;
REVOKE UPDATE, DELETE ON social_identities FROM swi_legacy_runtime, swi_app;

-- ---------------------------------------------------------------------------
-- #4. Action classes for EXIT_ONLY enforcement at the DB boundary.
--
-- PLAN SWI §19: when halted, EXIT_ONLY permits only risk-REDUCING actions.
-- 1024 deferred this to the application layer, which has no caller, so an active
-- EXIT_ONLY switch plus a BUY was accepted. The classification mirrors
-- `sf::autonomy_runtime::classify_action` so the two cannot drift silently.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS intent_action_classes (
    action       text PRIMARY KEY,
    risk_class   text NOT NULL CHECK (risk_class IN ('reducing', 'adding'))
);

INSERT INTO intent_action_classes (action, risk_class) VALUES
    -- Risk-reducing: permitted under EXIT_ONLY.
    ('sell',              'reducing'),
    ('partial_sell',      'reducing'),
    ('close',             'reducing'),
    ('emergency_exit',    'reducing'),
    ('claim_fees',        'reducing'),
    ('partial_withdraw',  'reducing'),
    ('close_position',    'reducing'),
    ('swap_residuals',    'reducing'),
    ('lp_emergency_exit', 'reducing'),
    -- Risk-adding: blocked under EXIT_ONLY.
    ('buy',               'adding'),
    ('open_position',     'adding'),
    ('add_liquidity',     'adding'),
    ('compound_fees',     'adding'),
    ('reseed_position',   'adding')
ON CONFLICT (action) DO UPDATE SET risk_class = EXCLUDED.risk_class;

GRANT SELECT ON intent_action_classes TO swi_transition_owner;

-- ---------------------------------------------------------------------------
-- #3/#4. The transition function, with the policy holes closed and EXIT_ONLY
-- enforced at the same authoritative boundary as everything else.
--
-- Changes from 1024:
--   * policy version is now MANDATORY — no `IF ... IS NOT NULL` escape hatch;
--   * the policy must belong to the intent's workspace;
--   * the policy version must be the policy's CURRENT version, not a stale one;
--   * an EXIT_ONLY switch blocks risk-adding actions;
--   * an unclassified action is rejected rather than assumed safe (fail-closed).
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

    -- (b) Policy is MANDATORY, workspace-matched, and CURRENT.
    --     PLAN SWI: every execution carries a policy version. 1024 let a NULL
    --     policy skip the whole check.
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
           -- the policy must govern THIS intent's workspace
           AND ap.workspace_id = v_workspace_id
           -- and this must be the policy's current version, not a stale one
           AND ap.current_version_id = apv.id
    ) THEN
        RAISE EXCEPTION
            'automation policy version % is not the active current version for workspace % (intent %)',
            v_policy_ver, v_workspace_id, p_intent_id;
    END IF;

    -- (c) Kill switches (§19). A HALT blocks everything; an EXIT_ONLY blocks
    --     risk-ADDING actions only.
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
            'an active kill switch (halt) blocks transitions for intent %', p_intent_id;
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

    -- (d) Frozen transition edge.
    IF NOT EXISTS (
        SELECT 1 FROM intent_transition_edges
         WHERE from_state = v_from_state AND to_state = p_to_state
    ) THEN
        RAISE EXCEPTION 'illegal intent transition % -> %', v_from_state, p_to_state;
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
-- Operator note (REV-033 #5)
--
-- Three distinct roles are now expected, and none of them is the migrator:
--   migrator            — superuser/CREATEROLE; applies migrations only.
--   swi_legacy_runtime  — the pre-freeze binary (DATABASE_URL).
--   swi_executor        — the execution worker; needs a row per workspace in
--                         execution_role_workspaces.
--
-- Migration 1023/1025 create roles, so the migrator must hold CREATEROLE (or be a
-- superuser). REV-033 correctly notes that replay as a plain table owner fails at
-- 1023 with `must be able to SET ROLE "swi_transition_owner"`; that is why the
-- migrator is a separate identity from the runtime roles.
-- ---------------------------------------------------------------------------
