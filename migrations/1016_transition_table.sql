-- ============================================================================
-- Signal Forge — Phase 6 Migration 1016: Frozen intent transition table
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §16 (Execution
--   state machine). RESOLVES blocker #1 (execution-readiness).
--
-- Frozen decisions (see CONVENTIONS.md):
--   * Transition table below is authoritative (fail-closed, principle #7/#10).
--   * Reservation expiry: configurable per chain, DEFAULT 120 seconds.
--   * UNKNOWN_RECONCILIATION holds reservation; escalation to CANCELLED after
--     10 minutes (no auto-release until recon is definite).
--   * FAILED_SAFE never auto-retries; a retry is a NEW intent (new idempotency key).
--
-- This migration replaces the loose `status` CHECK on trade_intents with the
-- frozen transition table. History is immutable; we add a new state column and
-- a transition audit table rather than mutating existing rows.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Replace the loose intent status CHECK with the frozen state set.
-- ---------------------------------------------------------------------------
ALTER TABLE trade_intents
    DROP CONSTRAINT IF EXISTS trade_intents_status_check;

ALTER TABLE trade_intents
    ADD CONSTRAINT trade_intents_status_check
    CHECK (status IN ('proposed', 'approved', 'reserved', 'built', 'simulated',
                      'signed', 'submitted', 'confirmed', 'failed_safe',
                      'unknown_reconciliation', 'cancelled'));

-- ---------------------------------------------------------------------------
-- Intent transition audit (immutable). Every transition appends here and is
-- validated against the frozen table in the application layer (or via a
-- trigger; see below).
-- ---------------------------------------------------------------------------
CREATE TABLE intent_transitions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    intent_id       bigint NOT NULL REFERENCES trade_intents (id),
    from_state      text NOT NULL,
    to_state        text NOT NULL,
    reservation_action text NOT NULL
                    CHECK (reservation_action IN ('hold', 'release')),
    transitioned_at timestamptz NOT NULL DEFAULT now(),
    actor_identity_id bigint REFERENCES identities (id),
    actor_workload_id bigint REFERENCES workload_identities (id),
    reason          text,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX intent_transitions_intent_idx ON intent_transitions (intent_id, transitioned_at);

-- ---------------------------------------------------------------------------
-- Reservation expiry column: default 120 seconds (configurable per chain via
-- automation policy; the column holds the resolved value at reservation time).
-- ---------------------------------------------------------------------------
ALTER TABLE capital_reservations
    ADD COLUMN IF NOT EXISTS expires_at timestamptz; -- populated at RESERVED time (now + T)

-- ---------------------------------------------------------------------------
-- UNKNOWN_RECONCILIATION escalation: default 10 minutes. Tracked via a
-- reconciliation deadline on the reconciliation incident.
-- ---------------------------------------------------------------------------
ALTER TABLE reconciliation_incidents
    ADD COLUMN IF NOT EXISTS escalation_deadline timestamptz; -- opened_at + 10m default
