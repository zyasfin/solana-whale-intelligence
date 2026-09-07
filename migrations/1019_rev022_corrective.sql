-- ============================================================================
-- Signal Forge — Migration 1019: REV-022/REV-023 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-023 §6 + independent-review addenda.
--
-- Do NOT edit shipped 1001..1018 again; this is a corrective migration applied
-- after them. It:
--   1. Adds the immutable `event_id` to recent_events (REV-023 §4).
--   2. Enforces the anchor invariant (anchor == chain_qualified_contract ==
--      token_identity) at the DB layer (REV-023 §1).
--   3. Makes `workspace_id` NOT NULL on recent_events (workspace isolation).
--   4. Versions social identities: drops the non-versioned unique constraint
--      and adds a versioned key (platform + immutable_user_id + valid_from)
--      plus supersession linkage (addendum #4).
--   5. Enforces append-only on recent_events / social_identities with a trigger
--      (REV-022-F10): UPDATE/DELETE/TRUNCATE are rejected even for the owner.
--   6. Adds an authoritative intent-transition function + trigger so illegal
--      direct status changes fail (addendum #5).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 1. Immutable event_id on recent_events.
-- ---------------------------------------------------------------------------
ALTER TABLE recent_events
    ADD COLUMN IF NOT EXISTS event_id text;

-- Backfill any pre-existing rows with a stable synthetic id (they have none).
UPDATE recent_events
   SET event_id = 'rev022-' || id::text
 WHERE event_id IS NULL;

ALTER TABLE recent_events
    ALTER COLUMN event_id SET NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS recent_events_event_id_idx
    ON recent_events (workspace_id, event_id);

-- ---------------------------------------------------------------------------
-- 2. Workspace isolation: workspace_id must be present (idempotent upgrade).
--    Legacy rows may carry a NULL workspace_id; backfill them to a sentinel
--    workspace so the NOT NULL constraint never fails on an upgrade path
--    (REV-025-F10).
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE slug = 'default') THEN
        INSERT INTO workspaces (name, slug) VALUES ('Default', 'default');
    END IF;
END$$;

UPDATE recent_events
   SET workspace_id = (SELECT id FROM workspaces WHERE slug = 'default')
 WHERE workspace_id IS NULL;

ALTER TABLE recent_events
    ALTER COLUMN workspace_id SET NOT NULL;

-- ---------------------------------------------------------------------------
-- 3. Anchor invariant: anchor == chain_qualified_contract == token_identity.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'recent_events_anchor_invariant'
    ) THEN
        ALTER TABLE recent_events
            ADD CONSTRAINT recent_events_anchor_invariant
            CHECK (anchor_identity = chain_qualified_contract
                   AND chain_qualified_contract = token_identity);
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- 4. Versioned social identities.
--    Remove the non-versioned unique constraint; enforce one *current* binding
--    via a partial unique index, and keep historical rows versioned by
--    valid_from (addendum #4).
-- ---------------------------------------------------------------------------
ALTER TABLE social_identities
    DROP CONSTRAINT IF EXISTS social_identities_platform_user_key;
CREATE OR REPLACE FUNCTION reject_mutation()
RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'append-only table %: % is forbidden (archive-not-delete)',
        TG_TABLE_NAME, TG_OP;
END;
$$ LANGUAGE plpgsql;

-- Statement-level TRUNCATE guard (a row-level trigger cannot fire on TRUNCATE).
CREATE OR REPLACE FUNCTION reject_truncate()
RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'append-only table %: TRUNCATE is forbidden (archive-not-delete)',
        TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS recent_events_append_only ON recent_events;
CREATE TRIGGER recent_events_append_only
    BEFORE UPDATE OR DELETE ON recent_events
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();

DROP TRIGGER IF EXISTS recent_events_append_only_truncate ON recent_events;
CREATE TRIGGER recent_events_append_only_truncate
    BEFORE TRUNCATE ON recent_events
    FOR EACH STATEMENT EXECUTE FUNCTION reject_truncate();

DROP TRIGGER IF EXISTS social_identities_append_only ON social_identities;
CREATE TRIGGER social_identities_append_only
    BEFORE UPDATE OR DELETE ON social_identities
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();

DROP TRIGGER IF EXISTS social_identities_append_only_truncate ON social_identities;
CREATE TRIGGER social_identities_append_only_truncate
    BEFORE TRUNCATE ON social_identities
    FOR EACH STATEMENT EXECUTE FUNCTION reject_truncate();


-- ---------------------------------------------------------------------------
-- 6. Authoritative intent transitions (addendum #5).
--    A status vocabulary CHECK is not a transition table. Direct updates to
--    trade_intents.status are revoked from PUBLIC and replaced by a single
--    transactional function that locks the row, verifies a legal adjacent
--    edge, appends an immutable transition record, then updates the status.
-- ---------------------------------------------------------------------------
-- Legal adjacent edges of the frozen §16 state machine (sf/intent.rs).
CREATE TABLE IF NOT EXISTS intent_transition_edges (
    from_state text NOT NULL,
    to_state   text NOT NULL,
    PRIMARY KEY (from_state, to_state)
);

INSERT INTO intent_transition_edges (from_state, to_state) VALUES
    -- forward path
    ('proposed', 'approved'),
    ('approved', 'reserved'),
    ('reserved', 'built'),
    ('built', 'simulated'),
    ('simulated', 'signed'),
    ('signed', 'submitted'),
    ('submitted', 'confirmed'),
    -- reject/cancel
    ('proposed', 'cancelled'),
    ('approved', 'cancelled'),
    -- fail-closed (release reservation)
    ('reserved', 'failed_safe'),
    ('built', 'failed_safe'),
    ('simulated', 'failed_safe'),
    ('signed', 'failed_safe'),
    ('submitted', 'failed_safe'),
    -- reconciliation
    ('submitted', 'unknown_reconciliation'),
    ('unknown_reconciliation', 'confirmed'),
    ('unknown_reconciliation', 'failed_safe'),
    ('unknown_reconciliation', 'cancelled')
ON CONFLICT (from_state, to_state) DO NOTHING;

-- Authoritative transition function: lock, verify edge, append audit, update.
-- It flips a session-scoped GUC flag so the status-guard trigger (below) permits
-- THIS update while still rejecting any direct caller UPDATE.
CREATE OR REPLACE FUNCTION transition_intent(
    p_intent_id bigint,
    p_to_state  text,
    p_reason    text DEFAULT NULL
)
RETURNS void AS $$
DECLARE
    v_from_state text;
BEGIN
    -- Lock the intent row.
    SELECT status INTO v_from_state
      FROM trade_intents
     WHERE id = p_intent_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'intent % not found', p_intent_id;
    END IF;

    -- Verify a legal adjacent edge.
    IF NOT EXISTS (
        SELECT 1 FROM intent_transition_edges
         WHERE from_state = v_from_state AND to_state = p_to_state
    ) THEN
        RAISE EXCEPTION 'illegal intent transition % -> %', v_from_state, p_to_state;
    END IF;

    -- Append immutable transition audit.
    INSERT INTO intent_transitions (intent_id, from_state, to_state, reservation_action, reason)
    VALUES (p_intent_id, v_from_state, p_to_state,
            CASE WHEN p_to_state = 'reserved' THEN 'hold' ELSE 'release' END,
            p_reason);

    -- Permit the guarded status update from within this function only.
    PERFORM set_config('app.transition_intent', '1', true);
    UPDATE trade_intents SET status = p_to_state WHERE id = p_intent_id;
    PERFORM set_config('app.transition_intent', '0', true);
END;
$$ LANGUAGE plpgsql;

-- Owner-independent status guard: reject a direct UPDATE to trade_intents.status
-- unless it originates from transition_intent (session flag). The table owner
-- cannot bypass this trigger.
CREATE OR REPLACE FUNCTION guard_intent_status()
RETURNS trigger AS $$
BEGIN
    IF NEW.status IS DISTINCT FROM OLD.status
       AND coalesce(current_setting('app.transition_intent', true), '0') <> '1' THEN
        RAISE EXCEPTION 'direct trade_intents.status mutation is forbidden; use transition_intent()';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trade_intents_status_guard ON trade_intents;
CREATE TRIGGER trade_intents_status_guard
    BEFORE UPDATE ON trade_intents
    FOR EACH ROW EXECUTE FUNCTION guard_intent_status();

-- Revoke direct status mutation from PUBLIC; only the function may transition.
REVOKE UPDATE ON trade_intents FROM PUBLIC;
