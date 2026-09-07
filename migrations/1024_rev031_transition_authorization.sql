-- ============================================================================
-- Signal Forge — Migration 1024: REV-031 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-031 (REV-025-F05 PARTIAL).
--
-- WHAT WAS STILL WRONG
-- 1023 made the mutation path authoritative: only the SECURITY DEFINER owner can
-- change `trade_intents.status`, and the reviewer confirmed every direct-table
-- bypass is now rejected. Then 1023 ended with:
--
--     GRANT EXECUTE ON FUNCTION transition_intent(bigint, text, text) TO PUBLIC;
--
-- So the door was armour-plated and then propped open. The reviewer used an
-- ordinary role holding nothing but schema usage and SELECT to invoke the
-- function and move an arbitrary intent `proposed -> approved`. The function
-- checked the transition EDGE but never asked *who* is calling or *whether the
-- caller owns that intent*: authoritative, yet globally callable.
--
-- Three siblings of the same mistake are now on record (1019 GUC, 1022 audit row,
-- 1023 public grant). The pattern: each time I secured the mechanism and left the
-- ENTRY unguarded. This migration closes the entry.
--
-- WHAT THIS MIGRATION DOES
--   1. Revokes EXECUTE from PUBLIC; grants it only to `swi_executor`, the role
--      the execution worker authenticates as.
--   2. Adds in-transaction authorization INSIDE the function, so holding EXECUTE
--      is necessary but not sufficient:
--        * the intent must belong to a workspace the caller is a member of,
--        * the intent's automation policy must be active and not halted,
--        * a kill switch covering the intent's chain/wallet blocks the transition.
--   3. Records the authenticated caller on the audit row, so every transition
--      answers "who" and not only "what".
--
-- Authorization is evaluated against `session_user` (the role that connected),
-- NOT `current_user`: inside SECURITY DEFINER `current_user` is the function
-- owner, which is exactly why 1023's guard works — and exactly why it must not be
-- used to identify the caller.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 1. Application role for the execution worker. NOLOGIN here: the deployment
--    grants LOGIN/credentials out of band. Created idempotently so this file is
--    replayable.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'swi_executor') THEN
        CREATE ROLE swi_executor NOLOGIN;
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- 2. Map a database role to the workspaces it may act for.
--
--    Without this, "authorized" can only mean "can execute", which is the hole
--    REV-031 found. Membership is explicit data, auditable and revocable, and
--    an empty table means NOBODY is authorized (fail-closed).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS execution_role_workspaces (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    role_name    text NOT NULL,
    workspace_id bigint NOT NULL REFERENCES workspaces (id),
    granted_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT execution_role_workspaces_key UNIQUE (role_name, workspace_id)
);

COMMENT ON TABLE execution_role_workspaces IS 'Which database role may transition intents for which workspace. Empty = nobody (fail-closed).';

-- Only the definer needs to read it; callers must not be able to widen their own
-- authorization by writing to this table.
REVOKE INSERT, UPDATE, DELETE ON execution_role_workspaces FROM PUBLIC;
GRANT SELECT ON execution_role_workspaces TO swi_transition_owner;

-- The definer also needs to read the tables the new checks consult.
GRANT SELECT ON automation_policies TO swi_transition_owner;
GRANT SELECT ON automation_policy_versions TO swi_transition_owner;
GRANT SELECT ON kill_switches TO swi_transition_owner;
GRANT SELECT ON wallet_addresses TO swi_transition_owner;

-- ---------------------------------------------------------------------------
-- 3. Record the authenticated caller on every transition (audit answers "who").
-- ---------------------------------------------------------------------------
ALTER TABLE intent_transitions
    ADD COLUMN IF NOT EXISTS actor_role text;

-- ---------------------------------------------------------------------------
-- 4. The authorized transition path.
--
--    Order matters: identify the caller, prove workspace authority, prove policy
--    is live, prove no kill switch applies, verify the frozen edge, THEN audit and
--    update. Every failure raises — no branch returns quietly.
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
    v_caller       text := session_user;   -- the CONNECTED role, not the definer
BEGIN
    -- Lock the intent and read the context the authorization checks need.
    SELECT status, workspace_id, chain, wallet_address_id, automation_policy_version_id
      INTO v_from_state, v_workspace_id, v_chain, v_wallet_id, v_policy_ver
      FROM trade_intents
     WHERE id = p_intent_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'intent % not found', p_intent_id;
    END IF;

    -- (a) Workspace authority. An intent with no workspace cannot be authorized
    --     by anyone: there is nothing to check membership against (fail-closed).
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

    -- (b) The governing automation policy must be active. A retired/paused policy
    --     must not be able to move capital.
    IF v_policy_ver IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM automation_policy_versions apv
              JOIN automation_policies ap ON ap.id = apv.policy_id
             WHERE apv.id = v_policy_ver
               AND ap.status = 'active'
        ) THEN
            RAISE EXCEPTION
                'automation policy for intent % is not active', p_intent_id;
        END IF;
    END IF;

    -- (c) Kill switches (§19). A halt covering global/chain/wallet scope blocks
    --     the transition. Risk-reducing EXIT_ONLY handling stays in the
    --     application policy engine; at this boundary an active halt stops it.
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
            'an active kill switch blocks transitions for intent %', p_intent_id;
    END IF;

    -- (d) Frozen transition edge (unchanged from 1022/1023).
    IF NOT EXISTS (
        SELECT 1 FROM intent_transition_edges
         WHERE from_state = v_from_state AND to_state = p_to_state
    ) THEN
        RAISE EXCEPTION 'illegal intent transition % -> %', v_from_state, p_to_state;
    END IF;

    -- (e) Frozen reservation semantics; fail closed on an unmapped destination.
    SELECT reservation_action INTO v_action
      FROM intent_reservation_actions
     WHERE to_state = p_to_state;

    IF v_action IS NULL THEN
        RAISE EXCEPTION 'no frozen reservation action for state %', p_to_state;
    END IF;

    -- Audit (now naming the caller), then the guarded update.
    INSERT INTO intent_transitions
        (intent_id, from_state, to_state, reservation_action, reason, actor_role)
    VALUES (p_intent_id, v_from_state, p_to_state, v_action, p_reason, v_caller);

    UPDATE trade_intents SET status = p_to_state WHERE id = p_intent_id;
END;
$$;

ALTER FUNCTION transition_intent(bigint, text, text) OWNER TO swi_transition_owner;

-- ---------------------------------------------------------------------------
-- 5. Close the entry: EXECUTE is no longer public.
--
--    REVOKE must name every grantee that 1023 granted, including PUBLIC.
-- ---------------------------------------------------------------------------
REVOKE ALL ON FUNCTION transition_intent(bigint, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION transition_intent(bigint, text, text) TO swi_executor;

-- ---------------------------------------------------------------------------
-- Operator note
--
-- After this migration the execution worker must connect as `swi_executor` (or a
-- role granted membership in it) AND have a row in `execution_role_workspaces`
-- for each workspace it acts on. Both are deliberate: an unprovisioned deployment
-- fails closed with an explicit error instead of transitioning intents for
-- workspaces nobody authorized.
-- ---------------------------------------------------------------------------
