//! Browser worker domain (Phase 2): X + TikTok authorized ingestion.
//!
//! Canonical source: PLAN SWI §4.3 `signal-forge-browser-worker`. Separate
//! only because browser/session dependencies differ. No CAPTCHA bypass, mass
//! account creation, or quota evasion (doc §25 non-goals).

use serde::{Deserialize, Serialize};

/// A browser worker task (authorized X/TikTok ingestion).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserTask {
    pub platform: BrowserPlatform,
    pub task_type: BrowserTaskType,
    pub target: String, // profile/list/search term, token-triggered
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserPlatform {
    X,
    Tiktok,
    Web,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTaskType {
    Search,
    Profile,
    List,
    TokenTriggeredResolve,
}

/// Captured payload: raw HTML/media with parser-version + challenge/session
/// health reporting (doc §4.3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserCapture {
    pub raw_ref: String, // content-addressed raw payload/media
    pub parser_version: String,
    pub challenge_health: ChallengeHealth,
    pub session_health: SessionHealth,
}

/// Challenge/session health reporting (doc §4.3). ASR/OCR only for shortlisted
/// candidates (cheap-first, principle #5).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChallengeHealth {
    Ok,
    ChallengeDetected,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionHealth {
    Ok,
    Expired,
    Invalid,
}

/// Media enrichment (ASR/OCR) applied only to shortlisted candidates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaEnrichment {
    pub kind: MediaKind,
    pub raw_ref: String,
    pub transcript: Option<String>, // ASR
    pub text: Option<String>,       // OCR
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Audio,
    Image,
    Video,
}
