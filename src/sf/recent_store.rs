//! Persistence for Token Recent + Deployer/Social Reuse (REV-020 / REV-023).
//!
//! sqlx-backed store over the `recent_events` / `social_identities` tables
//! (migrations 1018 + 1019). Append-only: inserts only; retraction is a NEW row.
//!
//! REV-023 corrections:
//! - typed `DateTime<Utc>` bound directly to PostgreSQL `timestamptz` (never a
//!   Rust `String`);
//! - typed `RecentRelation` bound to the PostgreSQL `recent_relation` enum
//!   (never a Rust `String`);
//! - every read/write carries the authenticated `workspace_id` (workspace
//!   isolation);
//! - relation projection returns one row per relation + target, preserving the
//!   stored confidence/truth/evidence (no `DISTINCT ON (relation)` truncation,
//!   no hard-coded `Estimated`).

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::recent::{CandidateRelation, RecentEvent, RecentRelation};

/// The canonical columns selected for a `recent_events` read. `occurred_at` and
/// `observed_at` are typed `timestamptz` (returned as `DateTime<Utc>`), and
/// `relation` is the typed `recent_relation` enum (returned as
/// `Option<RecentRelation>`).
const RECENT_EVENT_COLUMNS: &str = r#"
    event_id, event_type, anchor_identity, related_identities,
    chain_qualified_contract, occurred_at, observed_at,
    relation, truth_status, confidence, confidence_level,
    evidence_refs, dependency_group, freshness, coverage,
    capability_status, missing_inputs, retraction
"#;

// REV-067-F08: `insert_recent_event()` was REMOVED.
//
// It was a public, non-idempotent append: a plain `INSERT ... RETURNING id` with no
// `ON CONFLICT`, so any caller reaching for it double-published on retry and died on
// the unique index under concurrency — the exact failure mode REV-058-F06 fixed for
// the production path. It had no production caller (only test seeds), and keeping a
// second, weaker way to append is how the fixed path gets bypassed later.
//
// `append_recent_event_if_absent` is the only append. It enforces the same
// invariants and answers "was it already there?" from rows returned, not from an
// error classifier.

/// Append one recent event unless its `event_id` already exists, ATOMICALLY.
///
/// Returns `true` when this call appended the row, `false` when it was already there.
///
/// REV-058-F06: the pipeline used to `SELECT EXISTS` and then `INSERT`, which is not
/// atomic. Two workers resolving the same token could both observe "absent"; the loser
/// then hit the unique index on `(workspace_id, event_id)` and its whole pass failed —
/// while the code comment claimed concurrent duplicates were a harmless no-op.
///
/// `ON CONFLICT DO NOTHING RETURNING id` decides it in ONE statement, and the answer
/// comes from rows returned rather than from classifying an error. That distinction is
/// the same one REV-027 settled: correctness must not depend on error taxonomy. Here it
/// does not, because a conflict is no longer an error at all.
pub async fn append_recent_event_if_absent(
    pool: &PgPool,
    workspace_id: i64,
    e: &RecentEvent,
) -> Result<bool> {
    let mut conn = pool.acquire().await?;
    append_recent_event_on(&mut conn, workspace_id, e).await
}

/// The append, on ONE explicit connection.
///
/// REV-069-F08: the contention regression kept its own copy of this INSERT, so a
/// regression in the production statement could not fail that test — the copy would
/// stay green while production drifted. The statement now exists exactly once and
/// both the pool wrapper and the contention test drive THIS function, which is what
/// makes the test's evidence about production rather than about itself.
///
/// `pub` rather than `pub(crate)` because the live tests live in the BINARY target,
/// a different crate from this library; crate-private would put the copy back.
pub async fn append_recent_event_on(
    conn: &mut sqlx::PgConnection,
    workspace_id: i64,
    e: &RecentEvent,
) -> Result<bool> {
    if e.anchor_identity != e.chain_qualified_contract {
        anyhow::bail!("anchor_identity != chain_qualified_contract");
    }
    if !e.missing_inputs.is_empty() && e.coverage == super::recent::Coverage::Full {
        anyhow::bail!(
            "coverage=full contradicts missing_inputs={:?}: an absent input means \
             coverage is not full, and \"no relation\" must not read as \"no reuse\"",
            e.missing_inputs
        );
    }
    let id: Option<i64> = sqlx::query_scalar(
        r#"
        INSERT INTO recent_events (
            workspace_id, event_id, token_identity, event_type, anchor_identity,
            related_identities, chain_qualified_contract, occurred_at, observed_at,
            relation, truth_status, confidence, confidence_level, evidence_refs,
            dependency_group, freshness, coverage, capability_status, missing_inputs,
            retraction
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
        ON CONFLICT (workspace_id, event_id) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&e.event_id)
    .bind(&e.anchor_identity)
    .bind(&e.event_type)
    .bind(&e.anchor_identity)
    .bind(serde_json::to_value(&e.related_identities).unwrap_or_default())
    .bind(&e.chain_qualified_contract)
    .bind(e.occurred_at)
    .bind(e.observed_at)
    .bind(e.relation)
    .bind(truth_status_str(&e.truth_status))
    .bind(e.confidence)
    .bind(e.confidence_level.as_str())
    .bind(serde_json::to_value(&e.evidence_refs).unwrap_or_default())
    .bind(&e.dependency_group)
    .bind(e.freshness.as_ref().map(|f| serde_json::to_value(f).unwrap_or_default()))
    .bind(coverage_str(e.coverage))
    .bind(capability_str(e.capability_status))
    .bind(serde_json::to_value(&e.missing_inputs).unwrap_or_default())
    .bind(e.retraction.as_ref().map(|r| serde_json::to_value(r).unwrap_or_default()))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id.is_some())
}

/// Was this EXACT fact already published under a superseded key encoding?
///
/// REV-066-F05: versioning the key derivation rewrites the identity of every stored
/// fact, so a retry of an assertion a previous release published no longer collides
/// and appends a second row — an idempotency break for data that was never
/// ambiguous. The upgrade path is to look for the legacy row BEFORE appending.
///
/// The legacy key alone is NOT sufficient evidence, and that is the whole point: a
/// v1 key could be shared by two different tuples (the ambiguity v2 exists to fix),
/// so trusting it would inherit the defect and swallow a genuinely new fact. The
/// stored row's SEMANTIC fields are therefore compared against the event we are
/// about to write:
///
/// * `event_type`, `anchor_identity`, `relation`, `confidence_level`;
/// * the target identity SET (`related_identities`, order-independent);
/// * the evidence SET (`evidence_refs`, order-independent).
///
/// A legacy row whose tuple differs is not this fact, so the caller appends the new
/// key — a v1 collision does NOT suppress a distinct assertion.
///
/// Workspace-scoped: a legacy row in another tenant is another tenant's fact.
pub async fn legacy_row_publishes_the_same_fact(
    pool: &PgPool,
    workspace_id: i64,
    legacy_keys: &[String],
    e: &RecentEvent,
) -> Result<bool> {
    if legacy_keys.is_empty() {
        return Ok(false);
    }
    let rows: Vec<RecentEventRow> = sqlx::query_as(&format!(
        "SELECT {RECENT_EVENT_COLUMNS}, false AS is_current_coverage \
           FROM recent_events \
          WHERE workspace_id = $1 AND event_id = ANY($2)"
    ))
    .bind(workspace_id)
    .bind(legacy_keys)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(RecentEventRow::into_event)
        .any(|stored| same_published_fact(&stored, e)))
}

/// Do two events assert the same fact, ignoring row identity and timestamps?
///
/// Timestamps are deliberately excluded: `occurred_at`/`observed_at` are when the
/// pass ran, not what it concluded, and comparing them would make every retry a new
/// fact — exactly the bug this guard exists to prevent.
///
/// `missing_inputs` is compared for a `coverage_disclosure` and NOT for a relation,
/// because it means different things on the two rows. On a disclosure the missing
/// set IS the asserted fact ("these inputs were unavailable"), so two disclosures
/// with different sets are different facts. On a relation it merely describes the
/// pass's input completeness, and a relation resolved from the same evidence is the
/// same relation whether or not an unrelated actor input happened to be available.
/// Comparing it there would make an ordinary retry look like a new fact — the very
/// break this guard exists to prevent.
fn same_published_fact(stored: &RecentEvent, candidate: &RecentEvent) -> bool {
    fn identity_set(e: &RecentEvent) -> Vec<(&str, &str)> {
        let mut v: Vec<(&str, &str)> = e
            .related_identities
            .iter()
            .map(|i| (i.kind.as_str(), i.value.as_str()))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    fn string_set(items: &[String]) -> Vec<&str> {
        let mut v: Vec<&str> = items.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    let core = stored.event_type == candidate.event_type
        && stored.anchor_identity == candidate.anchor_identity
        && stored.chain_qualified_contract == candidate.chain_qualified_contract
        && stored.relation == candidate.relation
        && stored.confidence_level == candidate.confidence_level
        && identity_set(stored) == identity_set(candidate)
        && string_set(&stored.evidence_refs) == string_set(&candidate.evidence_refs);
    if !core {
        return false;
    }
    if candidate.event_type == "coverage_disclosure" {
        return stored.coverage == candidate.coverage
            && string_set(&stored.missing_inputs) == string_set(&candidate.missing_inputs);
    }
    true
}

/// Row projection for a recent event (avoids sqlx's 16-tuple `FromRow` limit).
#[derive(sqlx::FromRow)]
struct RecentEventRow {
    event_id: String,
    event_type: String,
    anchor_identity: String,
    related_identities: serde_json::Value,
    chain_qualified_contract: String,
    occurred_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    relation: Option<RecentRelation>,
    truth_status: String,
    confidence: Option<f64>,
    confidence_level: String,
    evidence_refs: serde_json::Value,
    dependency_group: Option<String>,
    freshness: Option<serde_json::Value>,
    coverage: String,
    capability_status: String,
    missing_inputs: serde_json::Value,
    retraction: Option<serde_json::Value>,
    /// Decided by SQL (`occurred_at DESC, id DESC`), never re-derived in Rust —
    /// that is the point of REV-062-F03.
    is_current_coverage: bool,
}

impl RecentEventRow {
    fn into_event(self) -> Option<RecentEvent> {
        let is_current_coverage = self.is_current_coverage;
        Some(RecentEvent {
            event_id: self.event_id,
            event_type: self.event_type,
            anchor_identity: self.anchor_identity,
            related_identities: serde_json::from_value(self.related_identities).ok()?,
            chain_qualified_contract: self.chain_qualified_contract,
            occurred_at: self.occurred_at,
            observed_at: self.observed_at,
            relation: self.relation,
            truth_status: truth_status_from_str(&self.truth_status),
            confidence: self.confidence,
            confidence_level: confidence_from_str(&self.confidence_level),
            evidence_refs: serde_json::from_value(self.evidence_refs).ok()?,
            dependency_group: self.dependency_group,
            freshness: self.freshness.and_then(|f| serde_json::from_value(f).ok()),
            coverage: coverage_from_str(&self.coverage),
            capability_status: capability_from_str(&self.capability_status),
            missing_inputs: serde_json::from_value(self.missing_inputs).ok()?,
            retraction: self.retraction.and_then(|x| serde_json::from_value(x).ok()),
            is_current_coverage,
        })
    }
}

/// Fetch a token's recent timeline within a window (`1h|24h|7d|30d|all`),
/// scoped to a workspace. Sorted by `occurred_at` ascending.
///
/// REV-056-F02: the CURRENT coverage disclosure is always included, even when it falls
/// outside the requested window.
///
/// Coverage is a statement of *present capability*, not a dated observation. A window
/// filter that drops it turns "we never had the deployer for this token" into an empty
/// list, and an empty list reads as "no reuse" — the exact ambiguity REV-048 forbids.
/// The reviewer reproduced it by aging a disclosure to 25h: the `24h` view returned 0
/// rows and the API looked like a clean negative result.
///
/// Refreshing on a 6h cadence (`COVERAGE_REFRESH_SECONDS`) keeps a live row inside the
/// window while the worker runs; this pin is what holds when it does NOT, because a
/// stalled worker must degrade to "coverage unknown", never to "no reuse".
pub async fn fetch_recent_timeline(
    pool: &PgPool,
    workspace_id: i64,
    token_identity: &str,
    window: &str,
) -> Result<Vec<RecentEvent>> {
    let interval = match window {
        "1h" => Some("interval '1 hour'"),
        "24h" => Some("interval '24 hours'"),
        "7d" => Some("interval '7 days'"),
        "30d" => Some("interval '30 days'"),
        _ => None, // "all" or unknown -> no time bound
    };

    // REV-062-F03: the CURRENT coverage row is marked HERE, by the same
    // `occurred_at DESC, id DESC` order every server-side read uses, and shipped to
    // the client as `is_current_coverage`. The dashboard previously re-derived it by
    // sorting on `occurred_at` alone, which is nondeterministic on a timestamp tie
    // and NaN-prone on a malformed value, so the UI could name a different current
    // state than the store. One fact, one answer, decided server-side.
    let rows: Vec<RecentEventRow> = if let Some(interval) = interval {
        sqlx::query_as(&format!(
            "WITH current_coverage AS ( \
               SELECT c.id FROM recent_events c \
                WHERE c.workspace_id = $1 \
                  AND c.token_identity = $2 \
                  AND c.event_type = 'coverage_disclosure' \
                ORDER BY c.occurred_at DESC, c.id DESC \
                LIMIT 1 \
             ) \
             SELECT {RECENT_EVENT_COLUMNS}, \
                    coalesce(id = (SELECT id FROM current_coverage), false) AS is_current_coverage \
               FROM recent_events \
              WHERE workspace_id = $1 AND token_identity = $2 \
                AND ( \
                  id = (SELECT id FROM current_coverage) \
                  OR ( \
                    occurred_at >= now() - {interval} \
                    AND event_type <> 'coverage_disclosure' \
                  ) \
                ) \
              ORDER BY occurred_at ASC"
        ))
        .bind(workspace_id)
        .bind(token_identity)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(&format!(
            "WITH current_coverage AS ( \
               SELECT c.id FROM recent_events c \
                WHERE c.workspace_id = $1 \
                  AND c.token_identity = $2 \
                  AND c.event_type = 'coverage_disclosure' \
                ORDER BY c.occurred_at DESC, c.id DESC \
                LIMIT 1 \
             ) \
             SELECT {RECENT_EVENT_COLUMNS}, \
                    coalesce(id = (SELECT id FROM current_coverage), false) AS is_current_coverage \
               FROM recent_events \
              WHERE workspace_id = $1 AND token_identity = $2 \
              ORDER BY occurred_at ASC"
        ))
        .bind(workspace_id)
        .bind(token_identity)
        .fetch_all(pool)
        .await?
    };

    Ok(rows.into_iter().filter_map(|r| r.into_event()).collect())
}

/// Fetch corroborated relations for a token, scoped to a workspace. Projects one
/// row per relation + target, preserving stored confidence, truth status, and
/// evidence (REV-023 §1). Superseded/erroneous rows are excluded from the
/// current view.
pub async fn fetch_relations(
    pool: &PgPool,
    workspace_id: i64,
    token_identity: &str,
) -> Result<Vec<CandidateRelation>> {
    #[derive(sqlx::FromRow)]
    struct RelationRow {
        relation: RecentRelation,
        related_identities: serde_json::Value,
        evidence_refs: serde_json::Value,
        confidence_level: String,
    }

    // Current view (REV-027 "additional current-view defect", tightened in
    // REV-029/REV-030):
    //   * a row is excluded when its OWN truth status is not `confirmed`: a
    //     contradicted or retracted relation is not a corroborated one
    //     (fail-closed). The archive keeps every row; `fetch_recent_timeline`
    //     still returns them.
    //   * a row is ALSO excluded when a VALID append-only retraction targets its
    //     `event_id`.
    //
    // REV-028 accepted ANY row carrying `retraction.target_event_id`, which was a
    // denial-of-visibility hole I introduced: an `unknown` retraction anchored to a
    // DIFFERENT token could hide a valid relation. The SQL path now mirrors the
    // `apply_retraction()` validator exactly — a retraction only counts when it
    //   - declares a legal status (`superseded` / `erroneous`; §REV-023 §4),
    //   - is itself `confirmed` (an unknown/disputed retraction retracts nothing),
    //   - is anchored to the SAME token as its target, and
    //   - carries a non-empty target id.
    // REV-062-F05: `ORDER BY occurred_at DESC` alone leaves timestamp ties
    // unbroken, so the first-seen revision per `(relation, kind, target)` was
    // nondeterministic when two disclosures shared an `occurred_at`. The tie is
    // now broken by `id DESC`, making the current projection deterministic even
    // when the worker writes two rows in one pass.
    let rows: Vec<RelationRow> = sqlx::query_as(
        r#"
        SELECT e.relation, e.related_identities, e.evidence_refs, e.confidence_level
          FROM recent_events e
         WHERE e.workspace_id = $1
           AND e.token_identity = $2
           AND e.relation IS NOT NULL
           AND e.truth_status = 'confirmed'
           AND NOT EXISTS (
                 SELECT 1
                   FROM recent_events r
                  WHERE r.workspace_id = e.workspace_id
                    AND r.retraction IS NOT NULL
                    -- same token: a retraction cannot reach across anchors
                    AND r.token_identity = e.token_identity
                    AND coalesce(r.retraction ->> 'target_event_id', '') <> ''
                    AND r.retraction ->> 'target_event_id' = e.event_id
                    -- legal retraction status only
                    AND r.retraction ->> 'truth_status' IN ('superseded', 'erroneous')
                    -- and the retraction row itself must be corroborated
                    AND r.truth_status = 'confirmed'
               )
         ORDER BY e.occurred_at DESC, e.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(token_identity)
    .fetch_all(pool)
    .await?;

    // One candidate per (relation, target KIND, target value), preserving
    // confidence/truth. The kind is part of the key (REV-027): omitting it
    // collapsed a Token and a Wallet that happen to share the same value.
    let mut out: Vec<CandidateRelation> = Vec::new();
    let mut seen: std::collections::HashSet<(RecentRelation, super::recent::IdentityKind, String)> =
        std::collections::HashSet::new();
    for r in rows {
        let evidence_refs: Vec<String> =
            serde_json::from_value(r.evidence_refs).unwrap_or_default();
        let related_ids: Vec<super::recent::IdentityKey> =
            serde_json::from_value(r.related_identities).unwrap_or_default();
        for to_identity in related_ids {
            let key = (r.relation, to_identity.kind, to_identity.value.clone());
            if !seen.insert(key) {
                continue;
            }
            out.push(CandidateRelation {
                to_identity,
                relation: r.relation,
                confidence: confidence_from_str(&r.confidence_level),
                evidence_refs: evidence_refs.clone(),
            });
        }
    }
    Ok(out)
}

/// Read the workspace's CURRENT social identities from the authoritative store
/// (REV-033/REV-034).
///
/// This is the authority behind `is_immutable_social_identity`. REV-032 let the
/// caller hand the resolver a slice it built itself, so "authoritative" was a label
/// rather than a property; the reviewer forged a record and obtained
/// `Reconstructed`. The resolver now only accepts records produced from these rows.
///
/// Reads `social_identities_current` (migration 1021), which resolves the latest
/// appended version per identity — append-only supersession, no UPDATE. Historical
/// handles are unioned with the current handle so a renamed account is still
/// recognised as a handle rather than mistaken for an account ID.
pub async fn fetch_social_identities(
    pool: &PgPool,
    workspace_id: i64,
) -> Result<Vec<super::recent::StoredSocialIdentity>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        platform: String,
        immutable_user_id: String,
        current_handle: Option<String>,
        historical_handles: serde_json::Value,
    }

    let rows: Vec<Row> = sqlx::query_as(
        r#"
        SELECT platform, immutable_user_id, current_handle, historical_handles
          FROM social_identities_current
         WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let mut handles: Vec<String> =
                serde_json::from_value(r.historical_handles).unwrap_or_default();
            if let Some(current) = r.current_handle {
                if !current.trim().is_empty() {
                    handles.push(current);
                }
            }
            // `from_store_row` is pub(crate) and this is the only call site: a row
            // exists only because this query returned it (REV-035-#4).
            super::recent::StoredSocialIdentity::from_store_row(
                r.platform,
                r.immutable_user_id,
                handles,
            )
        })
        .collect())
}

/// Resolve candidate relations with the social authority looked up INSIDE the
/// operation (REV-035-#4).
///
/// This is the production entry point. REV-034 left the shape
/// `resolve_candidates(.., social_bindings, ..)`, which put the burden of supplying
/// authority on the caller — and a caller that supplies its own authority is not
/// constrained by it. The reviewer's remediation is explicit: "Make social proof a
/// store lookup performed inside the authoritative operation."
///
/// So the workspace-scoped lookup happens here, against
/// `social_identities_current`, and the resolver receives records the caller never
/// touched. The caller chooses the workspace, not the evidence.
pub async fn resolve_candidates_from_store(
    pool: &PgPool,
    workspace_id: i64,
    anchor: &super::recent::ActorExtraction,
    nodes: &[super::graph::EntityNode],
    edges: &[super::graph::EntityEdge],
    now_secs: i64,
) -> Result<Vec<super::recent::CandidateRelation>> {
    let rows = fetch_social_identities(pool, workspace_id).await?;
    let bindings = super::recent_runtime::records_from_store(rows);
    Ok(super::recent_runtime::resolve_candidates(
        anchor, nodes, edges, &bindings, now_secs,
    ))
}

fn truth_status_str(s: &super::core::TruthStatus) -> &'static str {
    use super::core::TruthStatus::*;
    match s {
        Unknown => "unknown",
        Confirmed => "confirmed",
        Disputed => "disputed",
        Superseded => "superseded",
        Erroneous => "erroneous",
    }
}

fn truth_status_from_str(s: &str) -> super::core::TruthStatus {
    use super::core::TruthStatus::*;
    match s {
        "confirmed" => Confirmed,
        "disputed" => Disputed,
        "superseded" => Superseded,
        "erroneous" => Erroneous,
        _ => Unknown,
    }
}

fn confidence_from_str(s: &str) -> super::recent::RecentConfidence {
    use super::recent::RecentConfidence::*;
    match s {
        "exact" => Exact,
        "reconstructed" => Reconstructed,
        "estimated" => Estimated,
        _ => Insufficient,
    }
}

fn coverage_str(c: super::recent::Coverage) -> &'static str {
    use super::recent::Coverage::*;
    match c {
        Full => "full",
        Degraded => "degraded",
        OnDemand => "on_demand",
        Unavailable => "unavailable",
    }
}

fn coverage_from_str(s: &str) -> super::recent::Coverage {
    use super::recent::Coverage::*;
    match s {
        "full" => Full,
        "degraded" => Degraded,
        "on_demand" => OnDemand,
        _ => Unavailable,
    }
}

fn capability_str(c: super::recent::CapabilityStatus) -> &'static str {
    use super::recent::CapabilityStatus::*;
    match c {
        Available => "available",
        Insufficient => "insufficient",
        Unavailable => "unavailable",
    }
}

fn capability_from_str(s: &str) -> super::recent::CapabilityStatus {
    use super::recent::CapabilityStatus::*;
    match s {
        "available" => Available,
        "insufficient" => Insufficient,
        _ => Unavailable,
    }
}
