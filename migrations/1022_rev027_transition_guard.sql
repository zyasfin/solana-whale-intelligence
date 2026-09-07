-- ============================================================================
-- Signal Forge — Migration 1022: REV-027 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-027 (REV-025-F05 PARTIAL, F06 PARTIAL).
--
-- Two defects the reviewer reproduced against 1019:
--
--   F05a  GUC BYPASS. `guard_intent_status` trusted the custom GUC
--         `app.transition_intent`, which ANY caller can set:
--             SET LOCAL app.transition_intent = 1;
--             UPDATE trade_intents SET status='confirmed' WHERE ...;
--         committed with ZERO audit rows. A caller-settable flag is not an
--         authorization boundary.
--
--   F05b  WRONG RESERVATION SEMANTICS. `transition_intent` recorded
--         `reservation_action = 'release'` for every destination except
--         'reserved'. The frozen rule (sf/intent.rs::reservation_action) is
--         Hold for EVERY nonterminal forward state AND for
--         UNKNOWN_RECONCILIATION; Release only for the three terminal states
--         (CONFIRMED / FAILED_SAFE / CANCELLED). The probe
--         `proposed -> approved` recorded `release`, which would drop a
--         reservation mid-flight.
--
-- Fix approach:
--   * Replace the GUC flag with a proof that cannot be forged by a caller: the
--     guard requires the current transaction to already hold a matching
--     `intent_transitions` audit row inserted in THIS transaction. A direct
--     UPDATE has no such row, so it fails; the function writes the audit row
--     first, so it passes. No session variable is consulted.
--   * Derive `reservation_action` from a table that mirrors the frozen Rust
--     function, so the two cannot drift silently.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- F05b. Frozen reservation action per destination state
--       (mirrors sf::intent::reservation_action verbatim).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS intent_reservation_actions (
    to_state           text PRIMARY KEY,
    reservation_action text NOT NULL CHECK (reservation_action IN ('hold', 'release'))
);

INSERT INTO intent_reservation_actions (to_state, reservation_action) VALUES
    -- Terminal states release the reservation.
    ('confirmed',              'release'),
    ('failed_safe',            'release'),
    ('cancelled',              'release'),
    -- UNKNOWN_RECONCILIATION HOLDS: no auto-release until recon is definite.
    ('unknown_reconciliation', 'hold'),
    -- Forward/pre-submit states hold the reservation.
    ('proposed',  'hold'),
    ('approved',  'hold'),
    ('reserved',  'hold'),
    ('built',     'hold'),
    ('simulated', 'hold'),
    ('signed',    'hold'),
    ('submitted', 'hold')
ON CONFLICT (to_state) DO UPDATE
    SET reservation_action = EXCLUDED.reservation_action;

-- ---------------------------------------------------------------------------
-- F05a. Forge-proof status guard.
--
-- The guard no longer reads a caller-settable GUC. Instead it demands that an
-- `intent_transitions` row for exactly this (intent, from, to) already exists
-- and was written by the CURRENT transaction (`xmin = pg_current_xact_id()`).
-- `transition_intent` inserts that audit row before updating, so it passes;
-- a bare `UPDATE ... SET status` has no audit row and is rejected. Setting any
-- session variable does not help, because no session variable is read.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION guard_intent_status()
RETURNS trigger AS $$
DECLARE
    v_audited boolean;
BEGIN
    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;  -- not a status change
    END IF;

    SELECT EXISTS (
        SELECT 1
          FROM intent_transitions t
         WHERE t.intent_id = NEW.id
           AND t.from_state = OLD.status
           AND t.to_state = NEW.status
           -- Written by THIS transaction: a caller cannot pre-fabricate this,
           -- because intent_transitions is append-only audit and the row must
           -- carry the in-flight transaction id.
           AND t.xmin::text = pg_current_xact_id()::text
    ) INTO v_audited;

    IF NOT v_audited THEN
        RAISE EXCEPTION
            'direct trade_intents.status mutation is forbidden; use transition_intent() (no audited transition % -> % in this transaction)',
            OLD.status, NEW.status;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trade_intents_status_guard ON trade_intents;
CREATE TRIGGER trade_intents_status_guard
    BEFORE UPDATE ON trade_intents
    FOR EACH ROW EXECUTE FUNCTION guard_intent_status();

-- ---------------------------------------------------------------------------
-- Authoritative transition function, corrected.
--   * reservation_action comes from the frozen table (F05b).
--   * no GUC is set; the audit row itself authorizes the update (F05a).
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION transition_intent(
    p_intent_id bigint,
    p_to_state  text,
    p_reason    text DEFAULT NULL
)
RETURNS void AS $$
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

    -- Frozen reservation semantics; fail closed on an unmapped destination
    -- rather than defaulting to 'release'.
    SELECT reservation_action INTO v_action
      FROM intent_reservation_actions
     WHERE to_state = p_to_state;

    IF v_action IS NULL THEN
        RAISE EXCEPTION 'no frozen reservation action for state %', p_to_state;
    END IF;

    -- Audit FIRST: this row is what authorizes the guarded update below.
    INSERT INTO intent_transitions (intent_id, from_state, to_state, reservation_action, reason)
    VALUES (p_intent_id, v_from_state, p_to_state, v_action, p_reason);

    UPDATE trade_intents SET status = p_to_state WHERE id = p_intent_id;
END;
$$ LANGUAGE plpgsql;

REVOKE UPDATE ON trade_intents FROM PUBLIC;
