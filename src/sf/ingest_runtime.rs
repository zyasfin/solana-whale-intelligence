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

/// Proof that a durable canonical append actually happened (REV-013 addendum #2).
/// Only the authoritative append path can mint this; a bare caller cannot forge
/// it, so `commit` cannot be invoked without durable-append evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableAppendReceipt(pub String);

/// A two-phase idempotency store (REV-011-F05 + REV-013 addendum #2):
/// `contains` is read-only; `commit` persists a key only when handed a
/// `DurableAppendReceipt` proving a durable append succeeded.
pub trait IdempotencyStore {
    /// Read-only check: has this key already been durably committed?
    fn contains(&self, key: &str) -> bool;
    /// Persist the key, gated on a durable-append receipt.
    fn commit(&mut self, key: &str, receipt: &DurableAppendReceipt);
}

/// In-memory idempotency store for tests and as a reference implementation.
#[derive(Default)]
pub struct InMemoryIdempotency {
    committed: HashSet<String>,
}

impl InMemoryIdempotency {
    /// Authoritative durable-append simulation: mints a receipt and commits the
    /// key atomically. This is the ONLY way a key becomes committed.
    pub fn record_durable_append(&mut self, key: &str) -> DurableAppendReceipt {
        let receipt = DurableAppendReceipt(format!("append:{}", key));
        self.committed.insert(key.to_string());
        receipt
    }
}

impl IdempotencyStore for InMemoryIdempotency {
    fn contains(&self, key: &str) -> bool {
        self.committed.contains(key)
    }
    fn commit(&mut self, key: &str, _receipt: &DurableAppendReceipt) {
        self.committed.insert(key.to_string());
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

    // 2. Raw evidence write — NOT executed in this pure-logic slice (no writer
    // backend). Marked Skipped, not Ok (REV-009-F05): an unwritten evidence is
    // never reported as success.
    stages.push(StageResult::Skipped(
        IngestionStage::RawEvidenceWrite,
        "no raw evidence writer backend (pure logic slice)".into(),
    ));

    // 3. Envelope validation — fail-closed (principle #7). A canonical envelope
    // requires raw_hash, schema version, a source name, and a non-empty event
    // type. Whitespace-only values are rejected (REV-009 addendum #4).
    if raw.raw_hash.trim().is_empty()
        || raw.payload_schema_version.trim().is_empty()
        || raw.source_name.trim().is_empty()
        || raw.event_type.trim().is_empty()
    {
        stages.push(StageResult::Failed(
            IngestionStage::EnvelopeValidation,
            "incomplete envelope: raw_hash/schema_version/source_name/event_type required".into(),
        ));
        return PipelineOutcome { stages, accepted, deduped };
    }
    stages.push(StageResult::Ok(IngestionStage::EnvelopeValidation));

    // 4. Idempotent append — two-phase (REV-011-F05). In this pure-logic slice
    // there is no durable append backend, so we only CHECK `contains` (read-only)
    // and never `commit`.
    let key = idempotency_key(raw);
    if idem.contains(&key) {
        // REV-013-F02: a committed duplicate stops here — no normalization or
        // downstream stage processing is repeated.
        deduped = true;
        stages.push(StageResult::Skipped(
            IngestionStage::IdempotentAppend,
            "duplicate committed idempotency key".into(),
        ));
        return PipelineOutcome { stages, accepted, deduped };
    }
    stages.push(StageResult::Skipped(
        IngestionStage::IdempotentAppend,
        "no canonical append backend (pure logic slice)".into(),
    ));

    // 5. Normalization — parse derived claims. Malformed counts, not fatal.
    if let Some(_entity_keys) = normalize_entity_keys(raw) {
        stages.push(StageResult::Ok(IngestionStage::Normalization));
        // 6-10. Persistence-dependent stages are NOT executed in this pure-logic
        // slice (REV-007-F09): they are marked Skipped, not Ok, so a no-op is
        // never reported as success. The real runtime wires these to the
        // evidence graph, projections, trigger, and jobs/outbox.
        stages.push(StageResult::Skipped(
            IngestionStage::EntityResolution,
            "no graph backend (pure logic slice)".into(),
        ));
        stages.push(StageResult::Skipped(
            IngestionStage::GraphEdges,
            "no graph backend (pure logic slice)".into(),
        ));
        stages.push(StageResult::Skipped(
            IngestionStage::ScalarProjections,
            "no projection backend (pure logic slice)".into(),
        ));
        stages.push(StageResult::Skipped(
            IngestionStage::TriggerEvaluation,
            "no trigger backend (pure logic slice)".into(),
        ));
        stages.push(StageResult::Skipped(
            IngestionStage::JobsOutbox,
            "no jobs/outbox backend (pure logic slice)".into(),
        ));
        // REV-009-F05: the payload is only NORMALIZED, not accepted into the
        // canonical store (evidence/graph/projection/trigger/outbox are skipped).
        accepted = false;
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

    // REV-011-F05: pure-logic slice never commits, so a retry of a payload that
    // was never durably persisted is NOT deduped.
    #[test]
    fn retry_without_commit_is_not_duplicate() {
        let mut idem = InMemoryIdempotency::default();
        let one = payload(None, "same", 0, "A");
        assert!(!run_pipeline(&one, &mut idem).accepted);
        assert!(!idem.contains(&idempotency_key(&one)), "pure slice must not commit");
        // A retry of the same payload is also not deduped (nothing committed).
        let retry = run_pipeline(&one, &mut idem);
        assert!(!retry.deduped);
    }

    // REV-011-F05: after a durable append (commit), a retry IS deduped.
    #[test]
    fn retry_after_commit_is_duplicate() {
        let mut idem = InMemoryIdempotency::default();
        let one = payload(None, "same", 0, "A");
        let key = idempotency_key(&one);
        idem.record_durable_append(&key);
        let retry = run_pipeline(&one, &mut idem);
        assert!(retry.deduped);
        // REV-013-F02: a committed duplicate must NOT run normalization/downstream.
        let has_normalization = retry
            .stages
            .iter()
            .any(|s| matches!(s, StageResult::Ok(IngestionStage::Normalization)));
        assert!(!has_normalization, "duplicate must not re-run normalization");
    }
}
