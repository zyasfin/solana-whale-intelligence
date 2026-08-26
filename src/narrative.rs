//! Narrative classification and explanation with source provenance.
//!
//! GMGN-only evidence is capped at confidence 49: a narrative alone can never
//! create a signal. Telegram text is untrusted provenance, never trade fact.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::{ChainKind, NarrativeCategory};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

/// Narrative categories seeded into the `narratives` table.
pub const NARRATIVE_SLUGS: &[(&str, &str)] = &[
    ("ai", "AI / Agents"),
    ("defi", "DeFi"),
    ("dep_in", "DePIN"),
    ("meme", "Meme"),
    ("celebrity", "Celebrity"),
    ("political", "Political"),
    ("gaming", "Gaming"),
    ("rwa", "RWA"),
    ("solana_ecosystem", "Solana Ecosystem"),
    ("launchpad", "Launchpad"),
    ("community_takeover", "Community Takeover"),
    ("unknown", "Unknown"),
];

/// Seed the narrative taxonomy (idempotent).
pub async fn seed_narratives(db: &PgPool) -> Result<()> {
    for (slug, name) in NARRATIVE_SLUGS {
        sqlx::query(
            r#"
            INSERT INTO narratives (slug, name, category)
            VALUES ($1, $2, $1)
            ON CONFLICT (slug) DO NOTHING
            "#,
        )
        .bind(slug)
        .bind(name)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// One piece of narrative evidence for a token.
#[derive(Clone, Debug)]
pub struct NarrativeEvidenceInput {
    pub chain: ChainKind,
    pub mint: String,
    pub slug: String,
    /// `gmgn`, `on_chain`, or `telegram` (provenance only).
    pub source_kind: String,
    pub source_ref: String,
    pub canonical_url: Option<String>,
    pub claim_type: String,
    pub claim_text: String,
    pub polarity: String,
    pub published_at: Option<DateTime<Utc>>,
    pub confidence: i32,
    pub raw: serde_json::Value,
}

/// Store one narrative evidence row (deduplicated by content hash).
pub async fn add_narrative_evidence(db: &PgPool, evidence: &NarrativeEvidenceInput) -> Result<()> {
    let observed_at = Utc::now();
    let narrative_id: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM narratives WHERE slug = $1",
    )
    .bind(&evidence.slug)
    .fetch_one(db)
    .await?;
    let hash = content_hash_of(
        &evidence.source_kind,
        &evidence.source_ref,
        &evidence.claim_type,
        &evidence.claim_text,
    );
    sqlx::query(
        r#"
        INSERT INTO narrative_evidence
            (chain, mint, narrative_id, source_kind, source_ref, canonical_url,
             claim_type, claim_text, polarity, published_at, observed_at, confidence,
             content_hash, raw)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        "#,
    )
    .bind(evidence.chain.as_str())
    .bind(&evidence.mint)
    .bind(narrative_id)
    .bind(&evidence.source_kind)
    .bind(&evidence.source_ref)
    .bind(&evidence.canonical_url)
    .bind(&evidence.claim_type)
    .bind(&evidence.claim_text)
    .bind(&evidence.polarity)
    .bind(evidence.published_at)
    .bind(observed_at)
    .bind(evidence.confidence)
    .bind(&hash)
    .bind(&evidence.raw)
    .execute(db)
    .await?;
    Ok(())
}

fn content_hash_of(source_kind: &str, source_ref: &str, claim_type: &str, claim_text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source_kind.as_bytes());
    hasher.update(b"|");
    hasher.update(source_ref.as_bytes());
    hasher.update(b"|");
    hasher.update(claim_type.as_bytes());
    hasher.update(b"|");
    hasher.update(claim_text.as_bytes());
    hex::encode(hasher.finalize())
}

/// A classified narrative for a token.
#[derive(Clone, Debug)]
pub struct TokenNarrative {
    pub slug: String,
    pub score: Decimal,
    pub confidence: u32,
    pub status: String,
    pub evidence_count: u32,
    pub on_chain_confirmed: bool,
}

/// Classify token narratives as of an evaluation time.
///
/// Temporal rule: only evidence with `observed_at <= evaluation_time` counts.
/// GMGN-only narratives are capped at `gmgn_only_confidence_cap` (49).
pub async fn classify_token_narratives(
    db: &PgPool,
    chain: ChainKind,
    mint: &str,
    evaluation_time: DateTime<Utc>,
) -> Result<Vec<TokenNarrative>> {
    let rows = sqlx::query_as::<_, (i64, String, String, i32, Option<chrono::DateTime<Utc>>)>(
        r#"
        SELECT narrative_id, source_kind, polarity, confidence, observed_at
          FROM narrative_evidence
         WHERE chain = $1 AND mint = $2 AND observed_at <= $3
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(evaluation_time)
    .fetch_all(db)
    .await?;

    let mut by_narrative: std::collections::HashMap<i64, (Decimal, u32, bool, u32)> =
        std::collections::HashMap::new();
    for (narrative_id, source_kind, polarity, confidence, _observed) in rows {
        let entry = by_narrative.entry(narrative_id).or_insert((Decimal::ZERO, 0, false, 0));
        entry.3 += 1;
        let weight = match polarity.as_str() {
            "supporting" => Decimal::ONE,
            "contradicting" => Decimal::from(-1),
            _ => Decimal::ZERO,
        };
        entry.0 += weight * Decimal::from(confidence.max(0));
        if source_kind == "on_chain" {
            entry.2 = true;
        }
    }

    let mut results = Vec::new();
    for (narrative_id, (score, _, on_chain, count)) in by_narrative {
        let slug: String = sqlx::query_scalar::<_, String>(
            "SELECT slug FROM narratives WHERE id = $1",
        )
        .bind(narrative_id)
        .fetch_one(db)
        .await?;
        // Base confidence from accumulated score, bounded 0..100.
        let base = if count == 0 {
            0
        } else {
            let avg = score / Decimal::from(count);
            let clamped = avg.clamp(Decimal::ZERO, Decimal::from(100));
            clamped.to_u32().unwrap_or(0)
        };
        // GMGN-only (no on-chain confirmation) is capped at 49.
        let confidence = if on_chain {
            base.clamp(0, 100)
        } else {
            base.clamp(0, 49)
        };
        let status = if on_chain { "confirmed" } else { "candidate" };
        results.push(TokenNarrative {
            slug,
            score,
            confidence,
            status: status.to_string(),
            evidence_count: count,
            on_chain_confirmed: on_chain,
        });
    }
    results.sort_by(|a, b| b.confidence.cmp(&a.confidence));
    Ok(results)
}

use rust_decimal::prelude::ToPrimitive;

/// Explainable narrative report for a token.
#[derive(Clone, Debug)]
pub struct NarrativeReport {
    pub chain: ChainKind,
    pub mint: String,
    pub narrative: String,
    pub why_now: String,
    pub counter_evidence: Vec<String>,
    pub confidence: u32,
    pub sources: Vec<String>,
}

/// Produce the narrative report (`narrative`, `why_now`, `counter_evidence`)
/// as of an evaluation time.
pub async fn explain_narrative(
    db: &PgPool,
    chain: ChainKind,
    mint: &str,
    evaluation_time: DateTime<Utc>,
) -> Result<Option<NarrativeReport>> {
    let narratives = classify_token_narratives(db, chain, mint, evaluation_time).await?;
    let Some(top) = narratives.first() else {
        return Ok(None);
    };
    if top.confidence == 0 {
        return Ok(None);
    }

    let counter_rows = sqlx::query_as::<_, (String, chrono::DateTime<Utc>)>(
        r#"
        SELECT claim_text, observed_at
          FROM narrative_evidence
         WHERE chain = $1 AND mint = $2 AND polarity = 'contradicting'
           AND observed_at <= $3
         ORDER BY observed_at DESC
         LIMIT 5
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(evaluation_time)
    .fetch_all(db)
    .await?;
    let counter_evidence: Vec<String> = counter_rows
        .into_iter()
        .map(|(claim, at)| format!("{claim} (observed {at})"))
        .collect();

    let source_rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT source_kind
          FROM narrative_evidence
         WHERE chain = $1 AND mint = $2 AND observed_at <= $3
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(evaluation_time)
    .fetch_all(db)
    .await?;
    let sources: Vec<String> = source_rows.into_iter().map(|(s,)| s).collect();

    let why_now = if top.on_chain_confirmed {
        format!(
            "on-chain evidence confirms the {} narrative with {} evidence items",
            top.slug, top.evidence_count
        )
    } else {
        format!(
            "enrichment-only {} narrative with {} evidence items; awaiting on-chain confirmation",
            top.slug, top.evidence_count
        )
    };

    Ok(Some(NarrativeReport {
        chain,
        mint: mint.to_string(),
        narrative: top.slug.clone(),
        why_now,
        counter_evidence,
        confidence: top.confidence,
        sources,
    }))
}

/// Classify from GMGN token payload hints (enrichment, never canonical).
pub fn narrative_hints_from_gmgn(payload: &serde_json::Value) -> Vec<NarrativeCategory> {
    let mut hints = Vec::new();
    let text = serde_json::to_string(payload).unwrap_or_default().to_lowercase();
    if text.contains("\"ai\"") || text.contains("ai_") || text.contains("gpt") {
        hints.push(NarrativeCategory::Ai);
    }
    if text.contains("meme") {
        hints.push(NarrativeCategory::Meme);
    }
    if text.contains("depin") || text.contains("dep_in") {
        hints.push(NarrativeCategory::DepIn);
    }
    if text.contains("rwa") {
        hints.push(NarrativeCategory::Rwa);
    }
    if text.contains("gaming") {
        hints.push(NarrativeCategory::Gaming);
    }
    if text.contains("launchpad") {
        hints.push(NarrativeCategory::Launchpad);
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrative_slugs_cover_taxonomy() {
        assert_eq!(NARRATIVE_SLUGS.len(), 12);
        for (slug, _) in NARRATIVE_SLUGS {
            assert!(NarrativeCategory::parse_slug(slug).is_some(), "slug {slug} must parse");
        }
    }

    #[test]
    fn gmgn_hints_extraction() {
        let payload = serde_json::json!({ "theme": "ai", "tags": ["meme"] });
        let hints = narrative_hints_from_gmgn(&payload);
        assert!(hints.contains(&NarrativeCategory::Ai));
        assert!(hints.contains(&NarrativeCategory::Meme));
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash_of("gmgn", "ref", "theme", "text");
        let h2 = content_hash_of("gmgn", "ref", "theme", "text");
        let h3 = content_hash_of("on_chain", "ref", "theme", "text");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }
}
