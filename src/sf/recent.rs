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

/// Relationship taxonomy (REV-020, 13 frozen variants). Relations stay
/// independent: shared social/funder evidence does not automatically prove
/// common ownership or official status.
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
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IdentityKind {
    Token,
    Wallet,
    Social,
    Website,
    Telegram,
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
    pub retraction: Option<Retraction>,
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

/// Token-triggered evidence adapter boundary. The concrete self-hosted
/// authorized-session scraper (X/TikTok/web) is a transport concern wired
/// behind this trait; a test adapter is provided in `recent_runtime`.
pub trait TokenEvidenceAdapter {
    fn fetch(&self, query: &str) -> anyhow::Result<Vec<SocialEvidenceObservation>>;
}
