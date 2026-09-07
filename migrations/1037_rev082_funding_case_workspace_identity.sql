-- REV-082-F01: enforce (workspace_id, chain, recipient) as the authoritative
-- funding case identity. Before this, two workspaces could hold duplicate
-- (chain, recipient) cases and fetch_case silently picked the newest.
--
-- REV-084-F03: semantic merge instead of destructive dedup. When duplicates
-- exist, the survivor (lowest id) absorbs the best field values from ALL
-- duplicates before they are deleted, and every FK reference is repointed.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases'
    ) THEN
        RETURN;
    END IF;

    -- Semantic merge: keep the OLDEST case per (workspace_id, chain, recipient)
    -- as survivor, absorb best fields from all duplicates, repoint FKs, delete rest.
    -- (In practice 1036 backfilled everything to 'default', so duplicates
    -- should only exist if the application already created cross-tenant rows.)
    WITH ranked AS (
        SELECT id, workspace_id, chain, recipient,
               first_funded_at, stage, confidence, evidence, fanout_count, updated_at,
               ROW_NUMBER() OVER (
                   PARTITION BY workspace_id, chain, recipient
                   ORDER BY id ASC
               ) AS rn
          FROM funding_radar_cases
    ),
    keep AS (
        SELECT id AS keep_id, workspace_id, chain, recipient
          FROM ranked WHERE rn = 1
    ),
    dup AS (
        SELECT r.id AS dup_id, k.keep_id
          FROM ranked r
          JOIN keep k ON k.workspace_id = r.workspace_id
                     AND k.chain = r.chain
                     AND k.recipient = r.recipient
         WHERE r.rn > 1
    ),
    -- Merge evidence from all duplicates (oldest first, later keys overwrite).
    merged_evidence AS (
        SELECT k.keep_id,
               COALESCE(
                   (SELECT jsonb_object_agg(key, value)
                      FROM (
                          SELECT key, value,
                                 ROW_NUMBER() OVER (
                                     PARTITION BY key
                                     ORDER BY r.id ASC
                                 ) AS key_rn
                            FROM ranked r
                            JOIN keep k2 ON k2.workspace_id = r.workspace_id
                                        AND k2.chain = r.chain
                                        AND k2.recipient = r.recipient
                           CROSS JOIN LATERAL jsonb_each(r.evidence) AS e(key, value)
                          WHERE k2.keep_id = k.keep_id
                      ) sub
                      WHERE key_rn = (
                          SELECT MAX(key_rn) FROM (
                              SELECT key, ROW_NUMBER() OVER (
                                  PARTITION BY key ORDER BY r2.id ASC
                              ) AS key_rn
                                FROM ranked r2
                                JOIN keep k3 ON k3.workspace_id = r2.workspace_id
                                            AND k3.chain = r2.chain
                                            AND k3.recipient = r2.recipient
                               CROSS JOIN LATERAL jsonb_each(r2.evidence) AS e2(key, value)
                              WHERE k3.keep_id = k.keep_id
                          ) inner_sub
                          WHERE inner_sub.key = sub.key
                      )
                   ),
                   'null'::jsonb
               ) AS evidence
          FROM keep k
    ),
    -- Compute merged field values for each survivor.
    merged_fields AS (
        SELECT k.keep_id,
               MIN(r.first_funded_at) AS first_funded_at,
               MAX(r.confidence) AS confidence,
               MAX(r.fanout_count) AS fanout_count,
               MAX(r.updated_at) AS updated_at,
               -- Most advanced stage: funded < preparation < deployed < dismissed
               (SELECT stage
                  FROM ranked r2
                  JOIN keep k2 ON k2.workspace_id = r2.workspace_id
                              AND k2.chain = r2.chain
                              AND k2.recipient = r2.recipient
                 WHERE k2.keep_id = k.keep_id
                 ORDER BY CASE r2.stage
                              WHEN 'funded' THEN 1
                              WHEN 'preparation' THEN 2
                              WHEN 'deployed' THEN 3
                              WHEN 'dismissed' THEN 4
                          END DESC
                 LIMIT 1
               ) AS stage,
               me.evidence
          FROM keep k
          JOIN ranked r ON r.workspace_id = k.workspace_id
                       AND r.chain = k.chain
                       AND r.recipient = k.recipient
          JOIN merged_evidence me ON me.keep_id = k.keep_id
         GROUP BY k.keep_id, me.evidence
        HAVING COUNT(*) > 1  -- only groups with actual duplicates
    )
    UPDATE funding_radar_cases c
       SET first_funded_at = mf.first_funded_at,
           stage = mf.stage,
           confidence = mf.confidence,
           evidence = mf.evidence,
           fanout_count = mf.fanout_count,
           updated_at = mf.updated_at
      FROM merged_fields mf
     WHERE c.id = mf.keep_id;

    -- Repoint funding_radar_events FK references.
    WITH ranked AS (
        SELECT id, workspace_id, chain, recipient,
               ROW_NUMBER() OVER (
                   PARTITION BY workspace_id, chain, recipient
                   ORDER BY id ASC
               ) AS rn
          FROM funding_radar_cases
    ),
    keep AS (
        SELECT id AS keep_id, workspace_id, chain, recipient
          FROM ranked WHERE rn = 1
    ),
    dup AS (
        SELECT r.id AS dup_id, k.keep_id
          FROM ranked r
          JOIN keep k ON k.workspace_id = r.workspace_id
                     AND k.chain = r.chain
                     AND k.recipient = r.recipient
         WHERE r.rn > 1
    )
    UPDATE funding_radar_events e
       SET case_id = d.keep_id
      FROM dup d
     WHERE e.case_id = d.dup_id;

    -- Repoint alerts.funding_case_id FK references.
    WITH ranked AS (
        SELECT id, workspace_id, chain, recipient,
               ROW_NUMBER() OVER (
                   PARTITION BY workspace_id, chain, recipient
                   ORDER BY id ASC
               ) AS rn
          FROM funding_radar_cases
    ),
    keep AS (
        SELECT id AS keep_id, workspace_id, chain, recipient
          FROM ranked WHERE rn = 1
    ),
    dup AS (
        SELECT r.id AS dup_id, k.keep_id
          FROM ranked r
          JOIN keep k ON k.workspace_id = r.workspace_id
                     AND k.chain = r.chain
                     AND k.recipient = r.recipient
         WHERE r.rn > 1
    )
    UPDATE alerts a
       SET funding_case_id = d.keep_id
      FROM dup d
     WHERE a.funding_case_id = d.dup_id;

    -- Delete non-survivor duplicates.
    WITH ranked AS (
        SELECT id, workspace_id, chain, recipient,
               ROW_NUMBER() OVER (
                   PARTITION BY workspace_id, chain, recipient
                   ORDER BY id ASC
               ) AS rn
          FROM funding_radar_cases
    )
    DELETE FROM funding_radar_cases
     WHERE id IN (SELECT id FROM ranked WHERE rn > 1);

    -- Now the unique constraint is safe.
    CREATE UNIQUE INDEX IF NOT EXISTS funding_radar_cases_tenant_uidx
        ON funding_radar_cases (workspace_id, chain, recipient);
END$$;
