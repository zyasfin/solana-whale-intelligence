-- ============================================================================
-- Signal Forge — Phase 7 Migration 1017: Frozen rollout thresholds
-- Canonical source: PLAN SWI §14/§15 + signal-forge-security-auto-trade-design.md
--   (rollout ladder) + signal-forge-lp-autopilot-design.md §14 Rollout.
-- RESOLVES blocker #2 (rollout thresholds).
--
-- Frozen decisions (see CONVENTIONS.md):
--   * Min forward sample: 30 trades/cycles per strategy/pool.
--   * Forward horizon: 14 calendar days.
--   * Max drawdown: -10% of dedicated hot wallet (hard stop).
--   * Approval: every limit raise requires human approval (WebAuthn step-up).
--   * CI: 95% CI lower bound > 0 before raise.
--   * Ordering: claim/close matures before open/reseed; CREATE_POOL stays out.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Replace the autonomy_guards TBD columns with frozen defaults.
-- ---------------------------------------------------------------------------
ALTER TABLE autonomy_guards
    ADD COLUMN forward_horizon_days integer NOT NULL DEFAULT 14,
    ADD COLUMN max_drawdown_pct double precision NOT NULL DEFAULT -10.0,
    ADD COLUMN ci_lower_bound double precision NOT NULL DEFAULT 0.0;

-- min_forward_sample already exists as integer; enforce the frozen default.
ALTER TABLE autonomy_guards
    ALTER COLUMN min_forward_sample SET DEFAULT 30;

-- requires_approval is already boolean NOT NULL DEFAULT true (matches frozen rule).

-- ---------------------------------------------------------------------------
-- Rollout ladder phase (security design + LP design §14). Autonomous claim/close
-- matures before autonomous open/reseed. CREATE_POOL remains outside.
-- ---------------------------------------------------------------------------
ALTER TABLE autonomous_cycles
    ADD COLUMN phase text NOT NULL DEFAULT 'auto_bounded_claim_close'
        CHECK (phase IN ('shadow', 'paper', 'confirm_each',
                         'auto_bounded_claim_close', 'auto_bounded_open_reseed', 'advanced'));

-- ---------------------------------------------------------------------------
-- Human approval audit for limit raises (gate #9 WebAuthn step-up). Every raise
-- requires an approval record; no raise without one.
-- ---------------------------------------------------------------------------
CREATE TABLE limit_raise_approvals (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    cycle_id        bigint NOT NULL REFERENCES autonomous_cycles (id),
    identity_id     bigint NOT NULL REFERENCES identities (id), -- approving human
    webauthn_step_up boolean NOT NULL DEFAULT true,             -- gate #9
    approved_at     timestamptz NOT NULL DEFAULT now(),
    reason          text
);
CREATE INDEX limit_raise_approvals_cycle_idx ON limit_raise_approvals (cycle_id);
