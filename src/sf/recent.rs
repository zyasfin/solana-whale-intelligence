//! Token Recent + Deployer/Social Reuse Intelligence domain (REV-020).
//!
//! Canonical source: REVIEW_RESULT.md REV-020 (2026-09-01), which amends
//! PLAN SWI §8.3.1. This is a temporal projection over evidence + graph data,
//! not a symbol-based merge and not an all-X firehose.
//!
//! Frozen identity rules (REV-020):
//!   token    = chain_id + contract_address
//!   wallet   = chain_id + wallet_address
//!   X        = platform + immutable account/user ID
//!   Telegram = platform + chat/channel ID
//!   website  = normalized registrable domain + time-bounded ownership evidence
//!
//! Name, ticker, image, handle, and URL remain discovery clues only.
//! Factory/launchpad/program addresses remain separate from project deployers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Relationship taxonomy (REV-020, 12 frozen variants). Relations stay
/// independent: shared social/funder evidence does not automatically prove
/// common ownership or official status.
///
/// The count is **12**, matching the `recent_relation` enum in migration 1018
/// one-for-one. REV-021's prose said "13"; that was a miscount, not a missing
/// variant (REV-028-F10).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(type_name = "recent_relation", rename_all = "snake_case")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RecentRelation {
    SameDeployer,
    SameAuthority,
    SameFeePayer,
    SameFunder,
    FundedByKnownDeployer,
    SameSocialAccount,
    ReusedSocialLink,
    OfficialCaAnnouncement,
    CrossChainDeployment,
    DerivativeOf,
    SuspectedCopycat,
    LiquidityAttentionRotatedTo,
}

impl RecentRelation {
    /// A stable lowercase wire form for persistence/DISPLAY parity.
    pub fn as_str(self) -> &'static str {
        match self {
            RecentRelation::SameDeployer => "same_deployer",
            RecentRelation::SameAuthority => "same_authority",
            RecentRelation::SameFeePayer => "same_fee_payer",
            RecentRelation::SameFunder => "same_funder",
            RecentRelation::FundedByKnownDeployer => "funded_by_known_deployer",
            RecentRelation::SameSocialAccount => "same_social_account",
            RecentRelation::ReusedSocialLink => "reused_social_link",
            RecentRelation::OfficialCaAnnouncement => "official_ca_announcement",
            RecentRelation::CrossChainDeployment => "cross_chain_deployment",
            RecentRelation::DerivativeOf => "derivative_of",
            RecentRelation::SuspectedCopycat => "suspected_copycat",
            RecentRelation::LiquidityAttentionRotatedTo => "liquidity_attention_rotated_to",
        }
    }
}

/// Confidence levels (REV-020). `Insufficient` is distinct from missing/zero
/// (principle #3); never coerce to a score.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RecentConfidence {
    /// Same on-chain signer/authority, immutable social ID, or first-party
    /// account announcing the exact chain-qualified CA.
    Exact,
    /// New wallet funded by a known deployer + reused social/domain + coherent
    /// time.
    Reconstructed,
    /// Several weaker corroborating signals.
    Estimated,
    /// Name/symbol/image/handle similarity alone, or contradictory evidence.
    Insufficient,
}

impl RecentConfidence {
    /// Stable lowercase wire form for persistence.
    pub fn as_str(self) -> &'static str {
        match self {
            RecentConfidence::Exact => "exact",
            RecentConfidence::Reconstructed => "reconstructed",
            RecentConfidence::Estimated => "estimated",
            RecentConfidence::Insufficient => "insufficient",
        }
    }

    /// Higher = more authoritative. Used for deterministic candidate ordering.
    pub fn rank(self) -> u8 {
        match self {
            RecentConfidence::Exact => 3,
            RecentConfidence::Reconstructed => 2,
            RecentConfidence::Estimated => 1,
            RecentConfidence::Insufficient => 0,
        }
    }
}

/// Identity kind for a chain-qualified entity (REV-020 frozen identity rules).
/// `Hash` is derived so the kind can participate in a relation-dedupe key: a
/// Token and a Wallet sharing the same `value` are distinct targets (REV-027).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum IdentityKind {
    Token,
    Wallet,
    Social,
    Website,
    Telegram,
}

impl IdentityKind {
    /// Stable lowercase wire form for persistence/display parity.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityKind::Token => "token",
            IdentityKind::Wallet => "wallet",
            IdentityKind::Social => "social",
            IdentityKind::Website => "website",
            IdentityKind::Telegram => "telegram",
        }
    }
}

/// A chain-qualified identity key. `value` encodes the frozen rule:
/// `chain:contract`, `chain:address`, `platform:immutable_user_id`,
/// normalized registrable domain, or `platform:chat_id`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityKey {
    pub kind: IdentityKind,
    pub value: String,
}

/// Extracted on-chain actors and social identities for an anchor token.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActorExtraction {
    /// Chain-qualified token identity `chain:contract`.
    pub token: String,
    pub deployer: Option<String>,
    pub authority: Option<String>,
    pub fee_payer: Option<String>,
    pub factory: Option<String>,
    pub initial_funder: Option<String>,
    pub authority_changes: Vec<String>,
    pub social_identities: Vec<String>,
}

/// Activation gates that trigger a recent-intelligence refresh (REV-020).
/// Dormant/tombstoned tokens remain event-wake only.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivationTrigger {
    FirstLiquidity,
    Migration,
    CredibleCaller,
    SmartWalletEntry,
    FreshWalletBurst,
    RevivalWake,
    VolumeActivation,
    SocialProfileChange,
    OperatorRequest,
}

/// Data coverage (REV-020 §23 data capability modes). `Unavailable` and
/// `Insufficient` are never converted to zero.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Full,
    Degraded,
    OnDemand,
    Unavailable,
}

/// Capability status of the evidence backing an event.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityStatus {
    Available,
    Insufficient,
    Unavailable,
}

/// Source freshness of an observation (checked-at + age).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Freshness {
    pub checked_at: DateTime<Utc>,
    pub age_seconds: u64,
}

/// A retraction/supersession record (archive-not-delete, principle #6). A
/// retraction is a NEW append-only row bound to a real target event ID; it
/// never mutates the superseded row. Only `Superseded` and `Erroneous` are
/// legal retraction statuses (REV-023 §4).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Retraction {
    pub target_event_id: String,
    pub retracted_at: DateTime<Utc>,
    /// Always `Superseded` or `Erroneous`.
    pub truth_status: super::core::TruthStatus,
}

/// An official social binding: an immutable account ID bound to a
/// chain-qualified contract with a validity window. First-party exact-CA
/// announcements require a binding valid at the observation time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OfficialSocialBinding {
    pub immutable_account_id: String,
    pub chain_qualified_contract: String,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
}

/// A normalized evidence-backed recent event (REV-020 "Recent projection
/// contract"). Every event carries event type, anchor/related keys,
/// chain-qualified contract, times, relation, truth status, confidence
/// components, evidence refs, dependency group, freshness, coverage,
/// capability status, and retraction/supersession status.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentEvent {
    /// Immutable event ID (REV-023 §4). Retraction rows reference this ID.
    pub event_id: String,
    pub event_type: String,
    pub anchor_identity: String,
    pub related_identities: Vec<IdentityKey>,
    /// Chain-qualified contract this event is anchored on.
    pub chain_qualified_contract: String,
    pub occurred_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub relation: Option<RecentRelation>,
    pub truth_status: super::core::TruthStatus,
    /// Numeric confidence component (0..1, optional).
    pub confidence: Option<f64>,
    pub confidence_level: RecentConfidence,
    pub evidence_refs: Vec<String>,
    /// Source-dependency group id (collapses correlated copies).
    pub dependency_group: Option<String>,
    pub freshness: Option<Freshness>,
    pub coverage: Coverage,
    pub capability_status: CapabilityStatus,
    /// Authoritative inputs that were NOT available when this event was resolved
    /// (REV-050-F04 / REV-051 "Coverage disclosure").
    ///
    /// The resolver fails closed on a missing actor, which is correct — but an
    /// event that says only "no relation" reads as "no reuse", and those are
    /// different facts. Naming the absent inputs is what makes the difference
    /// visible to the API, the dashboard, and an alert consumer. Empty means
    /// nothing is claimed to be missing; non-empty forces `coverage != Full`
    /// (constraint `recent_events_missing_inputs_not_full`, migration 1030).
    pub missing_inputs: Vec<String>,
    pub retraction: Option<Retraction>,
    /// True when this row is the CURRENT coverage state for its anchor, as decided
    /// by the store's canonical order (`occurred_at DESC, id DESC`).
    ///
    /// REV-062-F03: the dashboard used to derive "current" itself by sorting the
    /// returned events on `occurred_at` alone. Two disclosures sharing a timestamp
    /// then resolved in whatever order the engine returned them, which can differ
    /// from the order the STORE considers canonical — so the UI could display a
    /// different current state than the one every server-side policy read uses. An
    /// invalid timestamp made the comparator `NaN` on top of that.
    ///
    /// "Which state is in force" is a server-side fact with one answer, so the
    /// server states it. The UI reads this flag instead of re-deriving a ranking it
    /// cannot see the tie-breaker for. Non-coverage rows are always `false`.
    #[serde(default)]
    pub is_current_coverage: bool,
}

/// A per-token timeline of recent events, sorted by `occurred_at`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentTimeline {
    pub token: String,
    pub events: Vec<RecentEvent>,
}

/// A corroborated candidate relation resolved from reverse indexes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateRelation {
    pub to_identity: IdentityKey,
    pub relation: RecentRelation,
    pub confidence: RecentConfidence,
    pub evidence_refs: Vec<String>,
}

/// Social evidence relation kind (REV-020 "X/social evidence contract").
/// Official CA announcements remain separate from mentions.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SocialEvidenceKind {
    OfficialCaAnnouncement,
    Mention,
    ProfileCaChange,
    ProfileLinkChange,
}

/// A single social observation (X/web/TikTok). Retains post/profile ID,
/// immutable account ID, text/media hash, published/observed times, the
/// announced chain-qualified contract (when the post asserts one), relation
/// kind, raw evidence ref, parser version, coverage, and session health.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SocialEvidenceObservation {
    pub platform: super::browser::BrowserPlatform,
    pub post_or_profile_id: String,
    pub immutable_account_id: String,
    pub text_media_hash: String,
    pub published_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    /// The chain-qualified contract the observation announces (extracted CA),
    /// when present. `None` means the post did not assert an exact CA.
    pub announced_contract: Option<String>,
    pub relation_kind: SocialEvidenceKind,
    pub raw_ref: String,
    pub parser_version: String,
    pub coverage: Coverage,
    pub session_health: super::browser::SessionHealth,
}

/// A workspace-scoped projection of one `social_identities` row, as read from the
/// authoritative store (REV-033/REV-034).
///
/// FIELDS ARE PRIVATE and there is no public constructor (REV-035-#4).
///
/// REV-034 made `SocialIdentityRecord`'s fields private but left THIS type a fully
/// public wire struct, and left `records_from_store` public. So the forgery just
/// moved one layer back: the reviewer built `StoredSocialIdentity` values by hand,
/// passed them to the public converter, and obtained `Reconstructed` again
/// (`forged_store_row_count=1 reconstructed=true`). Their verdict is the right one
/// — "a public wrapper around public wire rows is not authority".
///
/// The only way to obtain one of these is [`Self::from_store_row`], which is
/// `pub(crate)` and called solely by `recent_store` after a real query against the
/// workspace-scoped current view. Outside this crate the type is opaque: it can be
/// read, never minted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSocialIdentity {
    platform: String,
    immutable_user_id: String,
    /// Current handle plus every historical handle observed for this account.
    handles: Vec<String>,
}

impl StoredSocialIdentity {
    /// Mint from a row the store actually read. `pub(crate)` on purpose: only
    /// `recent_store` may call it, and only after querying the authoritative view.
    pub(crate) fn from_store_row(
        platform: String,
        immutable_user_id: String,
        handles: Vec<String>,
    ) -> Self {
        Self {
            platform,
            immutable_user_id,
            handles,
        }
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn immutable_user_id(&self) -> &str {
        &self.immutable_user_id
    }

    pub fn handles(&self) -> &[String] {
        &self.handles
    }

    /// Fixture minting for in-crate unit tests only (REV-037-F06).
    ///
    /// `#[cfg(test)]`, NOT a Cargo feature. REV-036 gated this behind a
    /// `test_fixtures` feature so integration tests could reach it; the reviewer
    /// then enabled that feature from a downstream crate and minted authority
    /// records at will. Features are additive and dependency-selectable, so they
    /// cannot express "tests only". `#[cfg(test)]` can: it exists only while
    /// compiling THIS crate's own test harness, and no dependent can turn it on.
    ///
    /// Tests that need minted authority therefore live inside the crate; see
    /// `sf::recent_authority_tests`.
    #[cfg(test)]
    pub fn mint_for_tests(
        platform: &str,
        immutable_user_id: &str,
        handles: &[&str],
    ) -> Self {
        Self {
            platform: platform.to_string(),
            immutable_user_id: immutable_user_id.to_string(),
            handles: handles.iter().map(|h| h.to_string()).collect(),
        }
    }
}

/// Token-triggered evidence adapter boundary. The concrete self-hosted
/// authorized-session scraper (X/TikTok/web) is a transport concern wired
/// behind this trait; a test adapter is provided in `recent_runtime`.
pub trait TokenEvidenceAdapter {
    fn fetch(&self, query: &str) -> anyhow::Result<Vec<SocialEvidenceObservation>>;
}
