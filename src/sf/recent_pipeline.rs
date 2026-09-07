//! Production pipeline for Recent intelligence (REV-046-A4).
//!
//! Every piece of this existed already and nothing called it:
//! `resolve_candidates_from_store()`, `fetch_social_identities()`,
//! `insert_recent_event()` — the last one had test callers only. That is the shape
//! the work order calls PARTIAL: helpers plus unit tests, no production caller, so
//! resolved relations were never persisted and never reachable from the API.
//!
//! Pipeline, per the work order:
//!
//! ```text
//! activation trigger
//!   -> authoritative actor/graph input
//!   -> resolve_candidates_from_store(workspace)
//!   -> append recent_events
//!   -> current projection / API / dashboard
//! ```
//!
//! Rules enforced here, not merely documented:
//!   * the workspace comes from the JOB CONTEXT, never from caller-supplied data;
//!   * social evidence comes only from the persisted store (the resolver entry point
//!     performs that lookup internally — see REV-035-#4);
//!   * an append failure means the candidate is NOT published;
//!   * retries are idempotent through a stable, content-derived event key;
//!   * nothing here touches trading, signing, or intents.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::recent::{
    ActorExtraction, CapabilityStatus, Coverage, IdentityKind, RecentConfidence, RecentEvent,
};
use super::token::TokenLifecycle;

/// Why a token is being resolved. Only an allowed trigger runs the pipeline, so a
/// dormant or tombstoned token is not polled individually (REV-020 acceptance #10).
pub use super::recent::ActivationTrigger;

/// Workspace identity for a pipeline run.
///
/// A newtype rather than a bare `i64` so a workspace cannot be passed positionally
/// by accident, and so the ONLY way to obtain one is from an authenticated or job
/// context. `from_job_context` is the single constructor; there is deliberately no
/// `From<i64>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceScope(i64);

impl WorkspaceScope {
    /// Bind the scope from the worker's own configuration/job context.
    ///
    /// The work order requires the workspace to come from an authenticated or job
    /// context and NOT from a request body. Keeping the constructor named after its
    /// provenance makes a request-derived value visibly wrong at the call site.
    pub fn from_job_context(workspace_id: i64) -> Result<Self> {
        if workspace_id <= 0 {
            anyhow::bail!("workspace id must be positive; got {workspace_id}");
        }
        Ok(Self(workspace_id))
    }

    pub fn id(self) -> i64 {
        self.0
    }
}

/// Outcome of one pipeline run, for logs and health.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PipelineOutcome {
    pub resolved: usize,
    pub appended: usize,
    /// Already present from an earlier run; counted separately so a retry storm is
    /// visible instead of looking like fresh work.
    pub duplicates: usize,
    /// Whether a coverage-disclosure event was appended by this run
    /// (REV-050-F04 / REV-051). Counted separately from `appended`, because it is
    /// not a resolved relation and must not inflate the relation count.
    pub disclosed_partial_coverage: bool,
    /// Whether this run published an IMPROVEMENT to full coverage (REV-058-F05).
    ///
    /// Separate from `disclosed_partial_coverage` because they are opposite facts: one
    /// says "we still cannot see X", the other says "we now can". Collapsing them would
    /// make a recovery indistinguishable from a limitation in logs and health.
    pub disclosed_full_coverage: bool,
    pub skipped_not_triggered: bool,
}

/// Stable event key for a resolved relation.
///
/// Idempotency requirement (REV-046-A4): a retry must not append a second row. The
/// key is derived from the facts, not from a timestamp or a random id, so the same
/// relation resolved twice yields the same key and the second insert conflicts.
///
/// Workspace is part of the key: the same relation in two workspaces is two facts.
///
/// REV-060-F04: the key used to be `(workspace, anchor, relation, target)`, which
/// made a REVISION a duplicate. `Estimated + A` then `Exact + A+B` (the same
/// relation, same target, but a stronger confidence and a second target) shared one
/// key, so the newer, more authoritative version was silently dropped. The
/// discriminator now carries the target KIND, the confidence, and the canonical
/// evidence set — a revision is a distinct fact, while a byte-identical retry still
/// collides and stays a no-op.
pub fn relation_event_key(
    workspace: WorkspaceScope,
    anchor: &str,
    relation: &str,
    target: &str,
    target_kind: IdentityKind,
    confidence: RecentConfidence,
    evidence_refs: &[String],
) -> String {
    use sha2::{Digest, Sha256};
    // REV-064-F05: EVERY field is length-framed, not just the evidence set. The
    // old encoding joined the tuple with a bare `|`, so a value CONTAINING the
    // delimiter shifted the field boundary: `anchor="A"` + `target="B|same_deployer|C"`
    // and `anchor="A|same_deployer|B"` + `target="C"` produced byte-identical input
    // and therefore ONE key for two different facts. Framing makes the encoding
    // injective: no field's bytes can be re-read as a different field split.
    //
    // REV-066-F05: the encoding is VERSIONED. Changing how a key is derived rewrites
    // the identity of every stored fact, so a retry of an assertion published under
    // the previous encoding no longer collides and appends a second row — an
    // idempotency break for data that was never ambiguous. The version is absorbed
    // first, and `RELATION_KEY_V1` is frozen so an upgrade can recognise the row a
    // prior release wrote (see `recent_store::relation_row_already_published`).
    let mut h = Sha256::new();
    framed(&mut h, RELATION_KEY_VERSION.to_le_bytes().as_slice());
    framed(&mut h, &workspace.id().to_le_bytes());
    framed(&mut h, anchor.as_bytes());
    framed(&mut h, relation.as_bytes());
    framed(&mut h, target.as_bytes());
    // The kind belongs in the key: a Token and a Wallet sharing a value are distinct
    // targets (REV-027). The confidence and evidence make a revision distinct (REV-060-F04).
    framed(&mut h, target_kind.as_str().as_bytes());
    framed(&mut h, confidence.as_str().as_bytes());
    // REV-062-F05: the evidence refs are a SET — order never matters and duplicate
    // refs are the same fact — so they hash through `canonical_set_bytes`
    // (length-prefixed, sorted, deduped) rather than a `join(",")` that collided on
    // interior commas. The whole set encoding is framed as one field in turn.
    framed(&mut h, &canonical_set_bytes(evidence_refs.iter().map(String::as_str)));
    format!("rel:{:x}", h.finalize())
}

/// Current relation-key encoding version.
///
/// Bump this — and add the previous encoder to [`legacy_relation_event_keys`] — when
/// the derivation changes. Without a version an encoding change is indistinguishable
/// from a fact change, and the store cannot tell "this is the same assertion under a
/// new algorithm" from "this is a new assertion" (REV-066-F05).
pub const RELATION_KEY_VERSION: u32 = 2;

/// Every key the SAME semantic tuple would have had under a superseded encoding,
/// NEWEST superseded first.
///
/// This is the upgrade path, not a fallback: a caller uses it to look for a row a
/// PREVIOUS release published for THIS EXACT tuple. It never widens what counts as a
/// duplicate on its own — the caller must still verify the stored row's semantic
/// fields, because a legacy key could be shared by two different tuples (that
/// ambiguity is the defect the current encoding fixes, and trusting a legacy key
/// alone would inherit it).
///
/// REV-069-F05: this returned ONLY the delimiter encoding, skipping the IMMEDIATE
/// predecessor. REV-065 shipped length-framed keys with no version field, and that
/// is the format sitting in every database upgrading from REV-065 — the generation
/// most likely to be present, and the one that was missed. Three generations are now
/// enumerated explicitly:
///
/// 1. current  — versioned + framed (`relation_event_key`);
/// 2. REV-065  — unversioned + framed;
/// 3. REV-063  — delimiter-joined.
pub fn legacy_relation_event_keys(
    workspace: WorkspaceScope,
    anchor: &str,
    relation: &str,
    target: &str,
    target_kind: IdentityKind,
    confidence: RecentConfidence,
    evidence_refs: &[String],
) -> Vec<String> {
    vec![
        relation_event_key_rev065(
            workspace, anchor, relation, target, target_kind, confidence, evidence_refs,
        ),
        relation_event_key_rev063(
            workspace, anchor, relation, target, target_kind, confidence, evidence_refs,
        ),
    ]
}

/// FROZEN REV-065 relation-key encoding: length-framed, NO version field.
///
/// Byte-for-byte what REV-065 shipped. Never used to MINT a key — only to find the
/// row that release wrote. Editing it breaks the upgrade lookup for every database
/// deployed at REV-065, so it must not change.
fn relation_event_key_rev065(
    workspace: WorkspaceScope,
    anchor: &str,
    relation: &str,
    target: &str,
    target_kind: IdentityKind,
    confidence: RecentConfidence,
    evidence_refs: &[String],
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    framed(&mut h, &workspace.id().to_le_bytes());
    framed(&mut h, anchor.as_bytes());
    framed(&mut h, relation.as_bytes());
    framed(&mut h, target.as_bytes());
    framed(&mut h, target_kind.as_str().as_bytes());
    framed(&mut h, confidence.as_str().as_bytes());
    framed(&mut h, &canonical_set_bytes(evidence_refs.iter().map(String::as_str)));
    format!("rel:{:x}", h.finalize())
}

/// FROZEN REV-063 relation-key encoding (REV-062-F05 era).
///
/// Delimiter-joined and therefore ambiguous — that is exactly why later generations
/// exist. Kept unchanged for the same reason as [`relation_event_key_rev065`].
fn relation_event_key_rev063(
    workspace: WorkspaceScope,
    anchor: &str,
    relation: &str,
    target: &str,
    target_kind: IdentityKind,
    confidence: RecentConfidence,
    evidence_refs: &[String],
) -> String {
    use sha2::{Digest, Sha256};
    let canonical_evidence = canonical_set_bytes(evidence_refs.iter().map(String::as_str));
    let mut h = Sha256::new();
    h.update(workspace.id().to_le_bytes());
    h.update(b"|");
    h.update(anchor.as_bytes());
    h.update(b"|");
    h.update(relation.as_bytes());
    h.update(b"|");
    h.update(target.as_bytes());
    h.update(b"|");
    h.update(target_kind.as_str().as_bytes());
    h.update(b"|");
    h.update(confidence.as_str().as_bytes());
    h.update(b"|");
    h.update(&canonical_evidence);
    format!("rel:{:x}", h.finalize())
}

/// Absorb one field into the hash as `len(u64 LE) || bytes`.
///
/// A separator byte is not an encoding: any field whose value can contain the
/// separator makes the concatenation ambiguous, and two distinct tuples then share
/// a digest (REV-064-F05). Length framing is injective for every byte string,
/// including empty, NUL-bearing, and multi-byte UTF-8 values.
fn framed(h: &mut impl sha2::Digest, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// How often an UNCHANGED coverage gap is re-disclosed (REV-056-F02).
///
/// Six hours: comfortably inside the default `24h` timeline window, so the current
/// view always contains a live disclosure, while a token resolved repeatedly does not
/// accumulate a row per pass. History keeps every refresh.
pub const COVERAGE_REFRESH_SECONDS: i64 = 6 * 60 * 60;

/// Canonical byte encoding of a SET of string items, immune to delimiter
/// collisions. `["a,b","c"]` and `["a","b,c"]` must not share bytes, so each
/// item is length-prefixed (u64 LE) before its bytes and items are sorted so
/// their order never matters (REV-062-F05). `sorted_...join(",")` was an
/// ambiguous encoding: a witness whose ref contains a comma collided with two
/// distinct refs, making an assertion's key depend on the WRONG fact set.
fn canonical_set_bytes<'a>(items: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut sorted: Vec<&str> = items.collect();
    sorted.sort_unstable();
    sorted.dedup(); // a SET has no duplicates; `["a","a"]` == `["a"]`
    let mut out = Vec::new();
    for item in sorted {
        // Length-prefix each item so delimiters inside an item cannot collide
        // with the separator between items (REV-062-F05).
        out.extend_from_slice(&(item.len() as u64).to_le_bytes());
        out.extend_from_slice(item.as_bytes());
    }
    out
}
/// Stable event key for a coverage disclosure.
///
/// Keyed on the workspace, the anchor, the SORTED missing-input set, the refresh
/// BUCKET, and the identity of the state being superseded (`predecessor`).
///
/// The first three make a changed gap a distinct fact; the bucket is what
/// REV-056-F02 forced: without it the key was stable forever, the existence check
/// refused to append, and the only disclosure aged out of the default 24h view — so
/// the API went empty again and "no relation" was once more readable as "no reuse".
///
/// REV-060-F01: the bucket alone was not monotonic. Within one bucket, `A -> full ->
/// A` produced the same key for the first and last row (same set, same bucket), so
/// the third append was silently a duplicate and the current projection pinned the
/// SECOND state instead of the third. Folding the PREDECESSOR state identity into
/// the key makes every transition a distinct immutable row, while a retry of the
/// SAME state (same set, same bucket, same predecessor) still collides and stays a
/// no-op — the idempotency guarantee is unchanged, only the discriminator is richer.
///
/// A raw timestamp would append a row on every pass. A bucket collapses everything
/// inside one refresh interval to a single event while guaranteeing a fresh row after
/// it; the predecessor makes a transition distinct even when the bucket does not.
pub fn coverage_event_key(
    workspace: WorkspaceScope,
    anchor: &str,
    missing_inputs: &[String],
    refresh_bucket: i64,
    predecessor: &str,
) -> String {
    use sha2::{Digest, Sha256};
    // REV-064-F05: same framing as `relation_event_key` — every field is
    // length-prefixed, so no anchor or predecessor value containing the old `|`
    // separator can be re-read as a different field split.
    //
    // REV-066-F05: versioned for the same reason the relation key is. The coverage
    // append is additionally gated on `changed || stale`, so a version bump cannot
    // re-publish history; the exposure is one refresh bucket during a mixed-version
    // deploy, and `legacy_coverage_event_keys` closes even that.
    let mut h = Sha256::new();
    framed(&mut h, COVERAGE_KEY_VERSION.to_le_bytes().as_slice());
    framed(&mut h, &workspace.id().to_le_bytes());
    framed(&mut h, anchor.as_bytes());
    framed(&mut h, &canonical_set_bytes(missing_inputs.iter().map(String::as_str)));
    framed(&mut h, &refresh_bucket.to_le_bytes());
    framed(&mut h, predecessor.as_bytes());
    format!("cov:{:x}", h.finalize())
}

/// Current coverage-key encoding version. See [`RELATION_KEY_VERSION`].
pub const COVERAGE_KEY_VERSION: u32 = 2;

/// Every key the SAME coverage state would have had under a superseded encoding,
/// NEWEST superseded first.
///
/// Unlike a relation assertion, a coverage row identifies a STATE TRANSITION whose
/// predecessor is read from the store, so an unchanged state does not even reach the
/// append. This exists for the cases that can: an upgraded deployment whose stored
/// predecessor chain was written by an older encoder, and two processes at different
/// versions writing inside the same refresh bucket during a deploy.
///
/// REV-069-F05: the IMMEDIATE predecessor (REV-065, framed without a version field)
/// was missing here exactly as it was for relations.
pub fn legacy_coverage_event_keys(
    workspace: WorkspaceScope,
    anchor: &str,
    missing_inputs: &[String],
    refresh_bucket: i64,
    predecessor: &str,
) -> Vec<String> {
    vec![
        coverage_event_key_rev065(workspace, anchor, missing_inputs, refresh_bucket, predecessor),
        coverage_event_key_rev063(workspace, anchor, missing_inputs, refresh_bucket, predecessor),
    ]
}

/// FROZEN REV-065 coverage-key encoding: length-framed, NO version field.
fn coverage_event_key_rev065(
    workspace: WorkspaceScope,
    anchor: &str,
    missing_inputs: &[String],
    refresh_bucket: i64,
    predecessor: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    framed(&mut h, &workspace.id().to_le_bytes());
    framed(&mut h, anchor.as_bytes());
    framed(&mut h, &canonical_set_bytes(missing_inputs.iter().map(String::as_str)));
    framed(&mut h, &refresh_bucket.to_le_bytes());
    framed(&mut h, predecessor.as_bytes());
    format!("cov:{:x}", h.finalize())
}

/// FROZEN REV-063 coverage-key encoding: delimiter-joined.
fn coverage_event_key_rev063(
    workspace: WorkspaceScope,
    anchor: &str,
    missing_inputs: &[String],
    refresh_bucket: i64,
    predecessor: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(workspace.id().to_le_bytes());
    h.update(b"|");
    h.update(anchor.as_bytes());
    h.update(b"|");
    h.update(&canonical_set_bytes(missing_inputs.iter().map(String::as_str)));
    h.update(b"|");
    h.update(refresh_bucket.to_le_bytes());
    h.update(b"|");
    h.update(predecessor.as_bytes());
    format!("cov:{:x}", h.finalize())
}

/// Authoritative actor inputs that were absent when the anchor was built
/// (REV-050-F04 / REV-051 "Coverage disclosure").
///
/// The resolver already fails closed on `None`, so an absent deployer produces no
/// `SameDeployer`. What was missing is saying so: a caller that receives an empty
/// relation list cannot tell "we checked and there is no reuse" from "we never had
/// the deployer". Those are different facts and only one of them is evidence.
///
/// Only the three inputs REV-048 names are reported, because those are the ones the
/// production worker currently cannot extract. `fee_payer` and `factory` are not in
/// the list: `factory` is an EXCLUSION (its absence widens nothing) and `fee_payer`
/// carries no relation the worker claims coverage over.
fn missing_actor_inputs(anchor: &ActorExtraction) -> Vec<String> {
    let mut missing = Vec::new();
    if anchor.deployer.is_none() {
        missing.push("deployer".to_string());
    }
    if anchor.authority.is_none() {
        missing.push("authority".to_string());
    }
    if anchor.initial_funder.is_none() {
        missing.push("initial_funder".to_string());
    }
    missing
}

/// Resolve candidate relations for one anchor and append them as recent events.
///
/// Returns what happened; the caller decides whether to log or degrade. Errors are
/// propagated, because a resolved-but-unpersisted candidate must never be treated as
/// published.
pub async fn run_for_anchor(
    pool: &PgPool,
    workspace: WorkspaceScope,
    lifecycle: TokenLifecycle,
    trigger: Option<ActivationTrigger>,
    anchor: &ActorExtraction,
    nodes: &[super::graph::EntityNode],
    edges: &[super::graph::EntityEdge],
    now: DateTime<Utc>,
) -> Result<PipelineOutcome> {
    // Activation gate first: a dormant/tombstoned token is not resolved at all.
    if !super::recent_runtime::should_trigger_lookup(lifecycle, trigger) {
        return Ok(PipelineOutcome {
            skipped_not_triggered: true,
            ..Default::default()
        });
    }

    // The resolver performs the social-identity lookup INTERNALLY against the
    // workspace-scoped store, so no caller can supply the evidence that decides
    // confidence (REV-035-#4).
    let candidates = super::recent_store::resolve_candidates_from_store(
        pool,
        workspace.id(),
        anchor,
        nodes,
        edges,
        now.timestamp(),
    )
    .await
    .context("failed to resolve recent-intelligence candidates")?;

    let mut outcome = PipelineOutcome {
        resolved: candidates.len(),
        ..Default::default()
    };

    // REV-050-F04: coverage is DERIVED from what was actually available, never
    // asserted. REV-047 hardcoded `Coverage::Full` while the production worker passed
    // `deployer: None, authority: None, initial_funder: None`, so every published
    // event claimed complete coverage over relations it had no input for.
    let missing_inputs = missing_actor_inputs(anchor);
    let coverage = if missing_inputs.is_empty() {
        Coverage::Full
    } else {
        // `Degraded` is this vocabulary's name for partial; the WHICH lives in
        // `missing_inputs`, because a level with no subject discloses nothing.
        Coverage::Degraded
    };
    // The evidence that DID back the emitted relation is still complete — the
    // resolver only emits from authoritative edges — so capability is downgraded
    // only when nothing could be checked at all.
    let capability_status = if missing_inputs.len() == 3 {
        CapabilityStatus::Insufficient
    } else {
        CapabilityStatus::Available
    };

    for c in &candidates {
        let event_id = relation_event_key(
            workspace,
            &anchor.token,
            c.relation.as_str(),
            &c.to_identity.value,
            c.to_identity.kind,
            c.confidence,
            &c.evidence_refs,
        );
        let event = RecentEvent {
            event_id,
            event_type: "relation_resolved".to_string(),
            // `insert_recent_event` rejects a row whose anchor and contract disagree,
            // so the anchor is used for both deliberately.
            anchor_identity: anchor.token.clone(),
            // The typed identity, not a bare string: the store round-trip keeps the
            // kind, so a Token and a Wallet sharing a value stay distinguishable.
            related_identities: vec![c.to_identity.clone()],
            chain_qualified_contract: anchor.token.clone(),
            occurred_at: now,
            observed_at: now,
            relation: Some(c.relation),
            truth_status: super::core::TruthStatus::Confirmed,
            confidence: None,
            confidence_level: c.confidence,
            evidence_refs: c.evidence_refs.clone(),
            dependency_group: None,
            freshness: None,
            coverage,
            capability_status,
            missing_inputs: missing_inputs.clone(),
            retraction: None,
            // A WRITER never asserts currency: "which state is in force" is a
            // read-time projection over the canonical order (REV-062-F03).
            is_current_coverage: false,
        };

        // REV-066-F05: an assertion published by a PREVIOUS release carries the
        // previous key encoding, so under the new one it no longer collides and
        // would append a duplicate of a fact that was never ambiguous. Look for
        // that row first, and only accept it when its SEMANTIC tuple matches this
        // event — a v1 key can be shared by two different tuples, and treating a
        // v1 collision as "already published" would swallow a genuinely new fact.
        let legacy_keys = legacy_relation_event_keys(
            workspace,
            &anchor.token,
            c.relation.as_str(),
            &c.to_identity.value,
            c.to_identity.kind,
            c.confidence,
            &c.evidence_refs,
        );
        if super::recent_store::legacy_row_publishes_the_same_fact(
            pool,
            workspace.id(),
            &legacy_keys,
            &event,
        )
        .await
        .context("failed to check for a previously published relation")?
        {
            outcome.duplicates += 1;
            continue;
        }

        // REV-058-F06: ONE atomic statement decides append-vs-duplicate.
        //
        // The previous `SELECT EXISTS` then `INSERT` was not atomic: two workers on the
        // same token could both see "absent", and the loser's pass died on the unique
        // index while this comment claimed duplicates were a no-op. The claim is now
        // true because `ON CONFLICT DO NOTHING RETURNING` makes a conflict a normal
        // result rather than an error.
        //
        // The earlier lesson still holds: correctness must not depend on classifying an
        // error string or SQLSTATE. It does not — rows returned is the answer.
        //
        // The legacy check above is NOT a check-then-insert race of the old kind: it
        // reads HISTORY written by a prior release, which no concurrent pass at this
        // version can create. Same-version concurrency is still decided atomically here.
        let appended =
            super::recent_store::append_recent_event_if_absent(pool, workspace.id(), &event)
                .await
                .context("failed to append a resolved relation")?;
        if appended {
            outcome.appended += 1;
        } else {
            outcome.duplicates += 1;
        }
    }

    // REV-062-F01: the predecessor is the latest disclosure's EVENT ID — a
    // monotonic discriminator, not a re-derived state identity. A state identity
    // (*"A"*) repeats when the state cycles (`A -> B -> A -> B`), so the second
    // *and* fourth rows shared a key and the fourth was silently a duplicate.
    // An event id is unique per row, so every transition keys differently and a
    // byte-identical retry still collides (same set, same bucket, same
    // predecessor) and stays a no-op.
    let latest: Option<(String, DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
        "SELECT event_id, occurred_at, missing_inputs FROM recent_events \
          WHERE workspace_id = $1 \
            AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure' \
          ORDER BY occurred_at DESC, id DESC \
          LIMIT 1",
    )
    .bind(workspace.id())
    .bind(&anchor.token)
    .fetch_optional(pool)
    .await
    .context("failed to read the current coverage state")?;

    let current_missing: Option<Vec<String>> = latest
        .as_ref()
        .and_then(|(_, _, m)| serde_json::from_value(m.clone()).ok());
    // A CHANGED set must be published immediately; an unchanged one is refreshed on the
    // 6-hour cadence so the default window always holds a live row.
    let changed = current_missing.as_deref() != Some(missing_inputs.as_slice());
    let stale = match latest.as_ref() {
        None => true,
        Some((_, at, _)) => (now - *at).num_seconds() >= COVERAGE_REFRESH_SECONDS,
    };

    // Nothing has ever been disclosed AND coverage is full: there is no limitation to
    // report, and inventing a row would be noise rather than disclosure.
    let first_and_full = latest.is_none() && missing_inputs.is_empty();

    if (changed || stale) && !first_and_full {
        let bucket = now.timestamp() / COVERAGE_REFRESH_SECONDS;
        // REV-062-F01: the predecessor is the event id of the state being superseded,
        // so a cycle (`A -> B -> A -> B`) writes a DISTINCT row for every transition
        // instead of colliding. Empty only on the very first disclosure.
        let predecessor = latest
            .as_ref()
            .map(|(event_id, _, _)| event_id.clone())
            .unwrap_or_default();
        let event_id = coverage_event_key(
            workspace,
            &anchor.token,
            &missing_inputs,
            bucket,
            &predecessor,
        );
        let event = RecentEvent {
            event_id,
            event_type: "coverage_disclosure".to_string(),
            anchor_identity: anchor.token.clone(),
            related_identities: vec![],
            chain_qualified_contract: anchor.token.clone(),
            occurred_at: now,
            observed_at: now,
            // No relation: this row asserts the LIMITS of knowledge, never a link.
            relation: None,
            truth_status: super::core::TruthStatus::Confirmed,
            confidence: None,
            // An absent input is not an estimate of anything. Full coverage, by
            // contrast, is an exact statement about capability.
            confidence_level: if missing_inputs.is_empty() {
                super::recent::RecentConfidence::Exact
            } else {
                super::recent::RecentConfidence::Insufficient
            },
            evidence_refs: vec![],
            dependency_group: None,
            freshness: None,
            coverage,
            capability_status,
            missing_inputs: missing_inputs.clone(),
            retraction: None,
            // Set by the read projection, not by the writer (REV-062-F03).
            is_current_coverage: false,
        };

        // REV-066-F05: the coverage key is versioned too, so a process still running
        // the previous release may have written THIS transition inside THIS refresh
        // bucket. The predecessor makes that a narrow window — an unchanged state
        // does not reach this branch at all — but a mixed-version deploy can hit it,
        // and the semantic check keeps a genuinely different state appendable.
        let legacy_keys = legacy_coverage_event_keys(
            workspace,
            &anchor.token,
            &missing_inputs,
            bucket,
            &predecessor,
        );
        if super::recent_store::legacy_row_publishes_the_same_fact(
            pool,
            workspace.id(),
            &legacy_keys,
            &event,
        )
        .await
        .context("failed to check for a previously published coverage state")?
        {
            return Ok(outcome);
        }
        // Atomic (REV-058-F06): concurrent workers inside one bucket must not fail.
        let appended =
            super::recent_store::append_recent_event_if_absent(pool, workspace.id(), &event)
                .await
                .context("failed to append the coverage state")?;
        if appended {
            // Only a LIMITED state counts as "disclosed partial coverage"; publishing
            // full coverage is the opposite fact and must not be reported as a gap.
            outcome.disclosed_partial_coverage = !missing_inputs.is_empty();
            outcome.disclosed_full_coverage = missing_inputs.is_empty();
        }
    }

    Ok(outcome)
}

// NOTE: an `is_duplicate_key()` error classifier was removed deliberately.
//
// Inferring "already published" from an error makes the idempotency guarantee depend
// on error taxonomy: misclassify once and a genuine append failure is silently
// counted as a no-op, which is exactly the fail-open shape REV-045-F02 flagged
// elsewhere. The existence check above answers the question directly.

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> WorkspaceScope {
        WorkspaceScope::from_job_context(7).expect("valid")
    }

    #[test]
    fn workspace_scope_rejects_non_positive_ids() {
        assert!(WorkspaceScope::from_job_context(0).is_err());
        assert!(WorkspaceScope::from_job_context(-1).is_err());
        assert_eq!(ws().id(), 7);
    }

    #[allow(dead_code)] // helper used by the key tests below
    fn key(
        s: &str,
        relation: &str,
        target: &str,
        kind: IdentityKind,
        confidence: RecentConfidence,
        evidence: &[&str],
    ) -> String {
        let ev: Vec<String> = evidence.iter().map(|e| e.to_string()).collect();
        relation_event_key(ws(), &format!("solana:{s}"), relation, target, kind, confidence, &ev)
    }

    #[test]
    fn event_key_is_stable_for_the_same_facts() {
        let ev = ["ev1"];
        let a = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                    RecentConfidence::Exact, &ev);
        let b = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                    RecentConfidence::Exact, &ev);
        assert_eq!(a, b, "a retry must produce the same key or it double-publishes");
    }

    #[test]
    fn event_key_separates_facts_that_differ() {
        let ev = ["ev1"];
        let base = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                       RecentConfidence::Exact, &ev);
        // Different target.
        assert_ne!(base, key("AAA", "same_deployer", "solana:CCC", IdentityKind::Token,
                             RecentConfidence::Exact, &ev));
        // Different relation.
        assert_ne!(base, key("AAA", "same_funder", "solana:BBB", IdentityKind::Token,
                             RecentConfidence::Exact, &ev));
        // Different anchor.
        assert_ne!(base, key("ZZZ", "same_deployer", "solana:BBB", IdentityKind::Token,
                             RecentConfidence::Exact, &ev));
        // Different WORKSPACE: the same relation in two tenants is two facts, and
        // collapsing them would leak one workspace's conclusion into another.
        let other = WorkspaceScope::from_job_context(8).unwrap();
        let other_ev: Vec<String> = ev.iter().map(|e| e.to_string()).collect();
        assert_ne!(
            base,
            relation_event_key(other, "solana:AAA", "same_deployer", "solana:BBB",
                               IdentityKind::Token, RecentConfidence::Exact, &other_ev)
        );
        // REV-060-F04: a REVISION is a distinct fact. Stronger confidence, extra
        // target, or different evidence must not collide with the base row.
        assert_ne!(base, key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                             RecentConfidence::Estimated, &ev));
        assert_ne!(base, key("AAA", "same_deployer", "solana:BBB", IdentityKind::Wallet,
                             RecentConfidence::Exact, &ev));
        assert_ne!(base, key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                             RecentConfidence::Exact, &["ev1", "ev2"]));
    }

    // REV-064-F05: the tuple fields used to be joined with a bare `|`. Any field
    // whose VALUE contains that byte shifts the field boundary, so two different
    // tuples serialize to identical bytes and share one event key — the second
    // fact is then silently swallowed by `ON CONFLICT DO NOTHING`. This is the
    // reviewer's exact witness plus one case per field.
    #[test]
    fn event_key_encoding_is_injective_across_field_boundaries() {
        let ev: Vec<String> = vec!["ev1".to_string()];
        let k = |anchor: &str, relation: &str, target: &str| {
            relation_event_key(ws(), anchor, relation, target, IdentityKind::Token,
                               RecentConfidence::Exact, &ev)
        };

        // The witness from REV-064: both tuples flattened to
        // `A|same_deployer|B|same_deployer|C` under the old `|` join.
        assert_ne!(
            k("A", "same_deployer", "B|same_deployer|C"),
            k("A|same_deployer|B", "same_deployer", "C"),
            "a delimiter inside a field must not be readable as a field boundary"
        );

        // One shifted boundary per adjacent field pair.
        assert_ne!(k("A|B", "r", "T"), k("A", "B|r", "T"));
        assert_ne!(k("A", "r|s", "T"), k("A", "r", "s|T"));

        // Empty, NUL, and multi-byte UTF-8 values stay distinguishable.
        assert_ne!(k("", "r", "AB"), k("A", "r", "B"));
        assert_ne!(k("A\0B", "r", "T"), k("A", "\0B|r", "T"));
        assert_ne!(k("é", "r", "T"), k("e", "\u{301}r", "T"));

        // The evidence set is framed as one field, so a ref cannot leak into the
        // confidence field either.
        let spill: Vec<String> = vec!["Exact|ev1".to_string()];
        assert_ne!(
            relation_event_key(ws(), "A", "r", "T", IdentityKind::Token,
                               RecentConfidence::Exact, &ev),
            relation_event_key(ws(), "A", "r", "T", IdentityKind::Token,
                               RecentConfidence::Estimated, &spill)
        );
    }

    // Same class, coverage key: anchor and predecessor are both free-form strings.
    #[test]
    fn coverage_key_encoding_is_injective_across_field_boundaries() {
        let w = ws();
        let miss = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };

        // Anchor bleeding into the missing-input set.
        assert_ne!(
            coverage_event_key(w, "A|deployer", &miss(&[]), 0, ""),
            coverage_event_key(w, "A", &miss(&["deployer"]), 0, ""),
        );
        // Predecessor bleeding backwards past the bucket.
        assert_ne!(
            coverage_event_key(w, "A", &miss(&[]), 0, "p"),
            coverage_event_key(w, "A", &miss(&[]), 0, "|p"),
        );
        // Empty vs absent anchor.
        assert_ne!(
            coverage_event_key(w, "", &miss(&["x"]), 0, ""),
            coverage_event_key(w, "x", &miss(&[]), 0, ""),
        );
        // NUL and Unicode remain distinct.
        assert_ne!(
            coverage_event_key(w, "A\0B", &miss(&[]), 0, ""),
            coverage_event_key(w, "A", &miss(&[]), 0, "\0B"),
        );
        assert_ne!(
            coverage_event_key(w, "é", &miss(&[]), 0, ""),
            coverage_event_key(w, "e\u{301}", &miss(&[]), 0, ""),
        );
    }

    #[test]
    fn idempotency_never_depends_on_classifying_database_errors() {
        // Guard on the source, so reintroducing an error classifier is caught:
        // deciding "already published" from error taxonomy means one
        // misclassification silently counts a failed append as a no-op.
        //
        // REV-058-F06 changed the MECHANISM and this test had to follow. It previously
        // required `SELECT EXISTS (...)` before the insert, which was correct about the
        // principle and wrong about the implementation: a check-then-insert is not
        // atomic, so two concurrent workers could both see "absent" and one would die on
        // the unique index. `ON CONFLICT DO NOTHING RETURNING` answers the same question
        // in one statement, and rows-returned — not an error string — is still the
        // source of truth. The invariant is unchanged; only the pinned mechanism moved.
        //
        // Only the region ABOVE this test module is inspected. My first version read
        // the whole file with `include_str!` and matched its own assertion text — a
        // guard that fails on its own description is a guard that gets deleted.
        let src = include_str!("recent_pipeline.rs");
        let code = src
            .split("mod tests {")
            .next()
            .expect("source has a body before the test module");
        let executable: String = code
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !executable.contains("fn is_duplicate_key"),
            "idempotency must never come from classifying database errors"
        );
        assert!(
            executable.contains("append_recent_event_if_absent"),
            "the pipeline must decide append-vs-duplicate with ONE atomic statement, \
             so a concurrent duplicate is a normal result rather than a failed pass"
        );
        // The non-atomic predecessor must not creep back for either append path.
        assert!(
            !executable.contains("SELECT EXISTS (SELECT 1 FROM recent_events"),
            "a check-then-insert is not atomic: two workers can both observe `absent` \
             and one then fails on the unique index (REV-058-F06)"
        );
    }

    // REV-062-F05: the evidence set was hashed as `sorted.join(",")`, an AMBIGUOUS
    // encoding — a ref containing a comma collided with two distinct refs, so two
    // different fact sets produced ONE key and the second revision was silently a
    // duplicate. Length-prefixing makes the encoding injective.
    #[test]
    fn evidence_encoding_cannot_collide_across_delimiters() {
        let a = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                    RecentConfidence::Exact, &["a,b", "c"]);
        let b = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                    RecentConfidence::Exact, &["a", "b,c"]);
        assert_ne!(
            a, b,
            "['a,b','c'] and ['a','b,c'] are different evidence sets and must not \
             share a key (REV-062-F05)"
        );

        // A SET has no duplicates and no order: these are the SAME fact.
        let once = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                       RecentConfidence::Exact, &["ev1", "ev2"]);
        let twice = key("AAA", "same_deployer", "solana:BBB", IdentityKind::Token,
                        RecentConfidence::Exact, &["ev2", "ev1", "ev1"]);
        assert_eq!(
            once, twice,
            "duplicate and reordered refs describe one set, so the key must be stable \
             (REV-062-F05)"
        );
    }

    // REV-062-F01: the coverage key's discriminator must be MONOTONIC, not a
    // re-derived state identity. With a state identity, a cycling state repeats its
    // own key: `A -> B -> A -> B` produced only 3 distinct keys for 4 transitions and
    // the 4th append was silently a duplicate. Keying on the predecessor's EVENT ID
    // makes every transition distinct while an identical retry still collides.
    #[test]
    fn coverage_key_distinguishes_every_transition_in_a_cycle() {
        let w = ws();
        let anchor = "solana:AAA";
        let bucket = 42;
        let set_a: Vec<String> = vec!["deployer".into(), "authority".into()];
        let set_b: Vec<String> = vec!["authority".into()];

        // Simulate A -> B -> A -> B inside ONE bucket, threading each row's key in as
        // the next row's predecessor exactly as the pipeline does.
        let k1 = coverage_event_key(w, anchor, &set_a, bucket, "");
        let k2 = coverage_event_key(w, anchor, &set_b, bucket, &k1);
        let k3 = coverage_event_key(w, anchor, &set_a, bucket, &k2);
        let k4 = coverage_event_key(w, anchor, &set_b, bucket, &k3);

        let keys = [&k1, &k2, &k3, &k4];
        let unique: std::collections::HashSet<&&String> = keys.iter().collect();
        assert_eq!(
            unique.len(),
            4,
            "A -> B -> A -> B is four transitions and needs four distinct keys; got \
             {} (REV-062-F01)",
            unique.len()
        );

        // Idempotency is unchanged: the SAME state with the SAME predecessor in the
        // SAME bucket must still collide, so a retry stays a no-op.
        assert_eq!(
            k3,
            coverage_event_key(w, anchor, &set_a, bucket, &k2),
            "an identical retry must still produce one key (idempotent no-op)"
        );

        // The missing-input set is a SET: order must not change the key.
        let reordered: Vec<String> = vec!["authority".into(), "deployer".into()];
        assert_eq!(
            k1,
            coverage_event_key(w, anchor, &reordered, bucket, ""),
            "the missing-input set is unordered; a reorder is the same state"
        );
    }
}
