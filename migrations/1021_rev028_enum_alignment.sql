-- ============================================================================
-- Signal Forge — Migration 1021: REV-028 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-028 findings F02..F05.
--
-- Do NOT edit shipped 1001..1020; this is a corrective migration applied after
-- them. It closes four SQL-vs-Rust enum/column drifts plus the dangling social
-- identity versioning left by 1019 §4:
--
--   F02  decision_components.component_class carried only 2 of the 4 frozen
--        ComponentClass variants, and named the mandatory one 'mandatory'
--        instead of the canonical wire form 'mandatory_pass'.
--   F03  browser_captures.platform rejected 'web' although BrowserPlatform
--        gained a Web variant (REV-021).
--   F04  autonomous_lp_cycles.action carried 5 of the 9 frozen LpAction values.
--   F05  signer_checks predates the 16 §17 fields appended to SignerPolicy in
--        REV-012, so a full checklist could not be persisted for audit.
--   F06  1019 §4 dropped social_identities' non-versioned UNIQUE constraint but
--        never created the promised replacement, leaving no "one current
--        binding" guarantee and no append-only supersession path.
--
-- Constraint drops are performed by looking the constraint up in pg_constraint
-- rather than guessing its auto-generated name: REV-021 shipped a wrong-name
-- `DROP CONSTRAINT IF EXISTS` that silently no-op'd and left the old CHECK
-- enforcing, which this migration must not repeat.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Helper: drop every CHECK constraint attached to one column, whatever its
-- generated name. Used below so a renamed/auto-named constraint cannot survive.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION drop_column_checks(p_table regclass, p_column text)
RETURNS void AS $$
DECLARE
    v_conname text;
BEGIN
    FOR v_conname IN
        SELECT c.conname
          FROM pg_constraint c
          JOIN pg_attribute a
            ON a.attrelid = c.conrelid
           AND a.attnum = ANY (c.conkey)
         WHERE c.conrelid = p_table
           AND c.contype = 'c'
           AND a.attname = p_column
    LOOP
        EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', p_table::text, v_conname);
    END LOOP;
END;
$$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------------------
-- F02. decision_components: four frozen component classes (PLAN SWI §12.2),
-- named exactly as `sf::portfolio::ComponentClass` serializes them.
-- Existing 'mandatory' rows are migrated to the canonical 'mandatory_pass'.
-- ---------------------------------------------------------------------------
SELECT drop_column_checks('decision_components', 'component_class');

UPDATE decision_components
   SET component_class = 'mandatory_pass'
 WHERE component_class = 'mandatory';

ALTER TABLE decision_components
    ADD CONSTRAINT decision_components_component_class_check
    CHECK (component_class IN ('mandatory_pass', 'sizing_input',
                               'strategy_input', 'halt_input'));

-- ---------------------------------------------------------------------------
-- F03. browser_captures.platform: X / TikTok / Web (sf::browser::BrowserPlatform).
-- ---------------------------------------------------------------------------
SELECT drop_column_checks('browser_captures', 'platform');

ALTER TABLE browser_captures
    ADD CONSTRAINT browser_captures_platform_check
    CHECK (platform IN ('x', 'tiktok', 'web'));

-- ---------------------------------------------------------------------------
-- F04. autonomous_lp_cycles.action: all nine frozen LpAction values (§15).
-- CREATE_POOL / CREATE_TOKEN remain deliberately absent (§1 LP semantics:
-- separate, disabled, later capabilities).
-- ---------------------------------------------------------------------------
SELECT drop_column_checks('autonomous_lp_cycles', 'action');

ALTER TABLE autonomous_lp_cycles
    ADD CONSTRAINT autonomous_lp_cycles_action_check
    CHECK (action IN ('open_position', 'add_liquidity', 'claim_fees',
                      'compound_fees', 'partial_withdraw', 'close_position',
                      'reseed_position', 'swap_residuals', 'emergency_exit'));

-- ---------------------------------------------------------------------------
-- F05. signer_checks: the 16 §17 fields appended to SignerPolicy in REV-012.
-- Every check is NOT NULL DEFAULT false so an unwritten field is recorded as a
-- FAILED check, never as a silent pass (fail-closed, principle #7).
-- ---------------------------------------------------------------------------
ALTER TABLE signer_checks
    ADD COLUMN IF NOT EXISTS workspace_binding_valid   boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS wallet_binding_valid      boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS policy_binding_valid      boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS idempotency_binding_valid boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS factory_allowed           boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS manager_allowed           boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS pool_verified             boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS authority_verified        boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS gas_ok                    boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS priority_fee_ok           boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS tip_ok                    boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS rent_ok                   boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS writable_accounts_allowed boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS approvals_bounded         boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS instructions_decoded      boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS no_unrelated_operations   boolean NOT NULL DEFAULT false;

-- ---------------------------------------------------------------------------
-- F06. Social identity versioning (completes 1019 §4).
--
-- 1019 §4 dropped `social_identities_platform_user_key` and its comment promised
-- a partial unique index enforcing one current binding — but no index was ever
-- created, so the table ended up with NO uniqueness at all.
--
-- The replacement cannot be `UNIQUE (...) WHERE valid_until IS NULL`: closing a
-- superseded row would require an UPDATE, which the 1019 append-only trigger
-- rejects by design. So versioning is expressed append-only instead:
--
--   * uniqueness is per VERSION — one row per (workspace, platform, user, valid_from);
--   * the CURRENT binding is the greatest valid_from for that identity, resolved
--     by the `social_identities_current` view — no UPDATE, no row closing;
--   * `valid_until` / `superseded_by` stay available for evidence supplied at
--     INSERT time, but are never required to make supersession work.
-- ---------------------------------------------------------------------------
CREATE UNIQUE INDEX IF NOT EXISTS social_identities_version_key
    ON social_identities (workspace_id, platform, immutable_user_id, valid_from);

-- Current binding per identity: latest version wins (append-only supersession).
CREATE OR REPLACE VIEW social_identities_current AS
SELECT DISTINCT ON (workspace_id, platform, immutable_user_id)
       id, workspace_id, platform, immutable_user_id, current_handle,
       historical_handles, domain, telegram_chat_id, valid_from, valid_until,
       superseded_by, payload, created_at
  FROM social_identities
 ORDER BY workspace_id, platform, immutable_user_id, valid_from DESC, id DESC;

-- ---------------------------------------------------------------------------
-- Cleanup: the helper is migration-local; drop it so it is not part of the
-- runtime API surface.
-- ---------------------------------------------------------------------------
DROP FUNCTION IF EXISTS drop_column_checks(regclass, text);
