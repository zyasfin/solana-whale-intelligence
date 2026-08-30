//! Runtime logic: ingestion pipeline (Phase 1).
//!
//! Canonical source: PLAN SWI §7.2 "Ingestion flow" (fetch -> raw evidence write
//! -> envelope validation -> idempotent append -> normalization -> entity
//! resolution -> graph edges -> scalar projections -> trigger evaluation ->
//! jobs/outbox). Raw evidence is written BEFORE parser-derived claims.
//!
//! Concept references (reused, not copied — principle #13):
//! - `ingest.rs` (old): raw-first persistence; malformed counted, not fatal.
//! - `replay.rs` (old): temporal guard (no look-ahead) — encoded as `observed_at
//!   <= evaluation` checks where applicable.
//! - `helius.rs` (old): provider dedupe by signature/hash.

use std::collections::HashSet;

use super::ingest::{Chain, IngestionStage, StageResult};

/// A raw payload arriving from a provider, plus its identity for idempotency.
#[derive(Clone, Debug, PartialEq)]
pub struct RawPayload {
    pub chain: Chain,
    pub source_name: String,
    pub source_event_id: Option<String>, // stable upstream id (tx hash/signature)
    pub event_type: String,             // normalized event type (fallback idempotency)
    pub payload_schema_version: String,
    pub raw_hash: String,               // content hash of the raw bytes
    pub observed_at: i64,               // unix seconds
    pub payload: serde_json::Value,
}

/// The output of a full pipeline run — a reproducible stage-by-stage trace
/// (gate #2), plus whether the payload was accepted into the canonical store.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelineOutcome {
    pub stages: Vec<StageResult>,
    pub accepted: bool,
    pub deduped: bool, // already seen (idempotent skip)
}

/// A minimal idempotency store (in-memory for tests; the real runtime backs this
/// with the `events` table unique indexes from migration 1003).
pub trait IdempotencyStore {
    /// Returns true if this idempotency key was already appended.
    fn seen(&mut self, key: &str) -> bool;
}

/// In-memory idempotency store for tests and as a reference implementation.
#[derive(Default)]
pub struct InMemoryIdempotency {
    seen: HashSet<String>,
}

impl IdempotencyStore for InMemoryIdempotency {
    fn seen(&mut self, key: &str) -> bool {
        !self.seen.insert(key.to_string())
    }
}

/// Run the §7.2 pipeline over one raw payload. Fail-closed on envelope
/// validation: if the envelope is invalid, later stages are not run.
pub fn run_pipeline(
    raw: &RawPayload,
    idem: &mut dyn IdempotencyStore,
) -> PipelineOutcome {
    let mut stages = Vec::new();
    let mut accepted = false;
    let mut deduped = false;

    // 1. Fetch/stream — assumed complete (payload is in hand).
    stages.push(StageResult::Ok(IngestionStage::Fetch));

    // 2. Raw evidence write — raw-first (doc §7.2 + ingest.rs old).
    stages.push(StageResult::Ok(IngestionStage::RawEvidenceWrite));

    // 3. Envelope validation — fail-closed (principle #7).
    if raw.raw_hash.is_empty() || raw.payload_schema_version.is_empty() {
        stages.push(StageResult::Failed(
            IngestionStage::EnvelopeValidation,
            "missing raw_hash or payload_schema_version".into(),
        ));
        return PipelineOutcome { stages, accepted, deduped };
    }
    stages.push(StageResult::Ok(IngestionStage::EnvelopeValidation));

    // 4. Idempotent append — two modes (doc §7.1).
    let key = idempotency_key(raw);
    if idem.seen(&key) {
        deduped = true;
        stages.push(StageResult::Skipped(
            IngestionStage::IdempotentAppend,
            "duplicate idempotency key".into(),
        ));
        return PipelineOutcome { stages, accepted, deduped };
    }
    stages.push(StageResult::Ok(IngestionStage::IdempotentAppend));

    // 5. Normalization — parse derived claims. Malformed counts, not fatal
    // (ingest.rs old): we mark Skipped but do not abort.
    if let Some(entity_keys) = normalize_entity_keys(raw) {
        stages.push(StageResult::Ok(IngestionStage::Normalization));
        // 6. Entity resolution + 7. Graph edges — no-op in this pure slice; the
        //    real runtime resolves against the entity graph and writes edges.
        stages.push(StageResult::Ok(IngestionStage::EntityResolution));
        stages.push(StageResult::Ok(IngestionStage::GraphEdges));
        // 8. Scalar projections — no-op here.
        stages.push(StageResult::Ok(IngestionStage::ScalarProjections));
        // 9. Trigger evaluation — only if normalization produced entity keys.
        stages.push(StageResult::Ok(IngestionStage::TriggerEvaluation));
        // 10. Jobs/outbox — enqueue downstream work.
        stages.push(StageResult::Ok(IngestionStage::JobsOutbox));
        let _ = entity_keys; // consumed by later stages in the full runtime
        accepted = true;
    } else {
        stages.push(StageResult::Skipped(
            IngestionStage::Normalization,
            "no entity keys derivable (malformed or empty payload)".into(),
        ));
    }

    PipelineOutcome { stages, accepted, deduped }
}
/// Compute the idempotency key per frozen §7.1 defaults.
/// Mode (a): source_id + source_event_id + payload_schema_version.
/// Mode (b): source_id + normalized entity + event type + time bucket + raw hash.
fn idempotency_key(raw: &RawPayload) -> String {
    match &raw.source_event_id {
        Some(id) => format!(
            "{}|{}|{}",
            raw.source_name, id, raw.payload_schema_version
        ),
        None => {
            let entity = normalize_entity_keys(raw).unwrap_or_default().join(",");
            let bucket = time_bucket(raw.observed_at);
            format!(
                "{}|{}|{}|{}|{}",
                raw.source_name, entity, raw.event_type, bucket, raw.raw_hash
            )
        }
    }
}

/// Coarse hourly time bucket for the fallback idempotency key (doc §7.1).
fn time_bucket(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(3600)
}

/// Derive entity keys from a raw payload (normalization). Returns `None` when
/// nothing derivable (malformed). A real runtime extracts chain-qualified
/// token/wallet/caller keys per §8 normalization rules.
fn normalize_entity_keys(raw: &RawPayload) -> Option<Vec<String>> {
    let obj = raw.payload.as_object()?;
    let mut keys = Vec::new();
    if let Some(addr) = obj.get("token").or_else(|| obj.get("mint")).and_then(|v| v.as_str()) {
        keys.push(format!("token:{}:{}", raw.chain.as_str(), addr));
    }
    if let Some(w) = obj.get("wallet").or_else(|| obj.get("from")).and_then(|v| v.as_str()) {
        keys.push(format!("wallet:{}:{}", raw.chain.as_str(), w));
    }
    if keys.is_empty() {
        None
    } else {
        Some(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(event_id: Option<&str>, hash: &str, observed: i64, token: &str) -> RawPayload {
        RawPayload {
            chain: Chain::Solana,
            source_name: "source".into(),
            source_event_id: event_id.map(|s| s.to_string()),
            event_type: "transfer".into(),
            payload_schema_version: "1".into(),
            raw_hash: hash.into(),
            observed_at: observed,
            payload: serde_json::json!({"token": token}),
        }
    }

    // Regression #4: fallback idempotency key must preserve entity + event type
    // + time bucket + raw hash, so two payloads with the same raw hash but
    // different entity/time do NOT dedupe.
    #[test]
    fn fallback_key_preserves_entity_and_time_context() {
        let mut idem = InMemoryIdempotency::default();
        let one = payload(None, "same", 0, "A");
        let two = payload(None, "same", 3600, "B");
        assert!(run_pipeline(&one, &mut idem).accepted);
        let outcome = run_pipeline(&two, &mut idem);
        assert!(!outcome.deduped);
    }

    // Regression #4b: same entity + same time bucket + same hash DOES dedupe.
    #[test]
    fn fallback_key_dedupes_true_duplicate() {
        let mut idem = InMemoryIdempotency::default();
        let one = payload(None, "same", 0, "A");
        let two = payload(None, "same", 100, "A"); // same hourly bucket, same entity
        assert!(run_pipeline(&one, &mut idem).accepted);
        let outcome = run_pipeline(&two, &mut idem);
        assert!(outcome.deduped);
    }
}
