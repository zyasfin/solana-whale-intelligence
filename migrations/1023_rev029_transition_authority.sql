-- ============================================================================
-- Signal Forge — Migration 1023: REV-029 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-029 (REV-025-F05 PARTIAL).
--
-- WHAT WAS STILL WRONG
-- 1019 authorized status updates with a caller-settable GUC. 1022 replaced that
-- with "an `intent_transitions` audit row written in this transaction", which the
-- reviewer defeated directly:
--
--     BEGIN;
--       INSERT INTO intent_transitions (intent_id, from_state, to_state,
--                                       reservation_action)
--       VALUES (1, 'proposed', 'confirmed', 'release');
--       UPDATE trade_intents SET status = 'confirmed' WHERE id = 1;
--     COMMIT;   -- accepted; edge table and transition_intent() bypassed
--
-- Both attempts share one root cause: the authority was something the CALLER can
-- produce. A GUC is settable; an audit row is insertable. Moving the hole is not
-- closing it.
--
-- WHAT THIS MIGRATION DOES
-- The authority becomes an identity the caller cannot assume: `current_user`
-- inside a SECURITY DEFINER function owned by a dedicated NOLOGIN role.
--
--   * `swi_transition_owner` — NOLOGIN, no membership granted to anyone. Nobody
--     can `SET ROLE` to it, so nobody can impersonate it.
--   * `transition_intent()` is SECURITY DEFINER owned by that role with a fixed
--     `search_path` (no search_path hijacking).
--   * The guard trigger permits a status change ONLY when `current_user` is that
--     owner. That is true exactly while the definer function's body runs, and is
--     false for every direct caller — including the table owner, since the table
--     owner is a DIFFERENT role.
--   * `intent_transitions` INSERT and `trade_intents` UPDATE are revoked from
--     PUBLIC, so a caller cannot pre-write audit rows either.
--
-- Net effect: the ONLY path to a status change is `transition_intent()`, which
-- validates the frozen edge table and writes the audit row itself.
--
-- Deployments that cannot create roles: see the note at the end of this file.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 1. Dedicated authority role. NOLOGIN and never granted to anyone, so it can
--    only ever be "entered" via SECURITY DEFINER.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'swi_transition_owner') THEN
        CREATE ROLE swi_transition_owner NOLOGIN;
    END IF;
END$$;

-- The definer role needs table access for the function body to work, but it is
-- NOT the table owner and holds nothing beyond what the transition needs.
GRANT SELECT, UPDATE ON trade_intents TO swi_transition_owner;
GRANT SELECT, INSERT ON intent_transitions TO swi_transition_owner;
GRANT SELECT ON intent_transition_edges TO swi_transition_owner;
GRANT SELECT ON intent_reservation_actions TO swi_transition_owner;
GRANT USAGE, SELECT ON SEQUENCE intent_transitions_id_seq TO swi_transition_owner;

-- ---------------------------------------------------------------------------
-- 2. Lock the caller out of both halves of the forged path.
-- ---------------------------------------------------------------------------
REVOKE INSERT, UPDATE, DELETE ON intent_transitions FROM PUBLIC;
REVOKE UPDATE ON trade_intents FROM PUBLIC;

-- ---------------------------------------------------------------------------
-- 3. Guard trigger keyed on an unforgeable identity.
--
--    `current_user` is the effective role. Inside a SECURITY DEFINER function it
--    is the function OWNER; everywhere else it is the caller. No SET/session
--    variable influences it, and `SET ROLE swi_transition_owner` fails because no
--    membership is granted.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION guard_intent_status()
RETURNS trigger AS $$
BEGIN
    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;  -- not a status change
    END IF;

    IF current_user <> 'swi_transition_owner' THEN
        RAISE EXCEPTION
            'direct trade_intents.status mutation is forbidden (% -> %); use transition_intent()',
            OLD.status, NEW.status
            USING HINT = 'status changes are only permitted inside the SECURITY DEFINER function transition_intent()';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trade_intents_status_guard ON trade_intents;
CREATE TRIGGER trade_intents_status_guard
    BEFORE UPDATE ON trade_intents
    FOR EACH ROW EXECUTE FUNCTION guard_intent_status();

-- ---------------------------------------------------------------------------
-- 4. The single authoritative transition path.
--    SECURITY DEFINER + fixed search_path + owned by the authority role.
--    Frozen reservation semantics still come from `intent_reservation_actions`
--    (1022): Hold for every nonterminal forward state and for
--    UNKNOWN_RECONCILIATION; Release only for the three terminal states.
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
    v_from_state text;
    v_action     text;
BEGIN
    SELECT status INTO v_from_state
      FROM trade_intents
     WHERE id = p_intent_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'intent % not found', p_intent_id;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM intent_transition_edges
         WHERE from_state = v_from_state AND to_state = p_to_state
    ) THEN
        RAISE EXCEPTION 'illegal intent transition % -> %', v_from_state, p_to_state;
    END IF;

    SELECT reservation_action INTO v_action
      FROM intent_reservation_actions
     WHERE to_state = p_to_state;

    IF v_action IS NULL THEN
        RAISE EXCEPTION 'no frozen reservation action for state %', p_to_state;
    END IF;

    INSERT INTO intent_transitions (intent_id, from_state, to_state, reservation_action, reason)
    VALUES (p_intent_id, v_from_state, p_to_state, v_action, p_reason);

    UPDATE trade_intents SET status = p_to_state WHERE id = p_intent_id;
END;
$$;

-- Hand the function to the authority role: SECURITY DEFINER runs as the OWNER,
-- so ownership is what makes `current_user` become `swi_transition_owner`.
ALTER FUNCTION transition_intent(bigint, text, text) OWNER TO swi_transition_owner;

-- Callers may invoke it; they still cannot touch the underlying tables directly.
GRANT EXECUTE ON FUNCTION transition_intent(bigint, text, text) TO PUBLIC;

-- ---------------------------------------------------------------------------
-- Operator note
--
-- `CREATE ROLE` requires a superuser or CREATEROLE connection. If migrations are
-- applied by a role without that privilege, this file aborts — deliberately. A
-- silent fallback would leave the forgeable 1022 guard in place while appearing to
-- succeed, which is precisely the failure mode this migration exists to remove.
-- Provision the role once out of band, then re-run.
-- ---------------------------------------------------------------------------
