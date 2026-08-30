//! Runtime logic: browser worker task gating + challenge/session health (Phase 2).
//!
//! Canonical source: PLAN SWI §4.3 "Browser worker" (X + TikTok authorized
//! ingestion). Separate because browser/session dependencies differ. No CAPTCHA
//! bypass, mass account creation, or quota evasion (doc §25 non-goals). ASR/OCR
//! only for shortlisted candidates (cheap-first, principle #5).
//!
//! This module decides whether a browser task may run and whether a captured
//! payload is usable, given challenge/session health. It consumes the frozen
//! `browser.rs` types (`BrowserTask`, `BrowserTaskType`, `BrowserCapture`,
//! `ChallengeHealth`, `SessionHealth`) and introduces no new frozen state.

use super::browser::{BrowserCapture, ChallengeHealth, SessionHealth};

/// Whether a capture is usable (session OK and no challenge). A challenge
/// detected or a failed/expired session means the capture must be discarded
/// or retried — never silently trusted (fail-closed).
pub fn capture_usable(capture: &BrowserCapture) -> bool {
    capture.session_health == SessionHealth::Ok
        && capture.challenge_health == ChallengeHealth::Ok
}

/// Whether a capture indicates a hard failure (session invalid or challenge
/// failed) that should stop retries (vs a transient challenge that may retry).
pub fn capture_failed(capture: &BrowserCapture) -> bool {
    capture.session_health == SessionHealth::Invalid
        || capture.challenge_health == ChallengeHealth::Failed
}

/// Whether a challenge-detected capture may be retried (transient). A challenge
/// is retryable; a hard failure (invalid session / failed challenge) is not.
pub fn capture_retryable(capture: &BrowserCapture) -> bool {
    capture.challenge_health == ChallengeHealth::ChallengeDetected
        && capture.session_health == SessionHealth::Ok
}

/// Whether ASR/OCR enrichment is warranted for a task. Cheap-first: only
/// token-triggered resolve tasks (which are already shortlisted) get media
/// enrichment; broad search/profile/list tasks do not (principle #5).
pub fn enrichment_warranted(task_type: super::browser::BrowserTaskType) -> bool {
    matches!(task_type, super::browser::BrowserTaskType::TokenTriggeredResolve)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(ch: ChallengeHealth, sh: SessionHealth) -> BrowserCapture {
        BrowserCapture {
            raw_ref: "r".into(),
            parser_version: "1".into(),
            challenge_health: ch,
            session_health: sh,
        }
    }

    #[test]
    fn usable_requires_ok_session_and_no_challenge() {
        assert!(capture_usable(&cap(ChallengeHealth::Ok, SessionHealth::Ok)));
        assert!(!capture_usable(&cap(ChallengeHealth::ChallengeDetected, SessionHealth::Ok)));
        assert!(!capture_usable(&cap(ChallengeHealth::Ok, SessionHealth::Expired)));
    }

    #[test]
    fn hard_failure_detected() {
        assert!(capture_failed(&cap(ChallengeHealth::Failed, SessionHealth::Ok)));
        assert!(capture_failed(&cap(ChallengeHealth::Ok, SessionHealth::Invalid)));
        assert!(!capture_failed(&cap(ChallengeHealth::ChallengeDetected, SessionHealth::Ok)));
    }

    #[test]
    fn retryable_only_transient_challenge() {
        assert!(capture_retryable(&cap(ChallengeHealth::ChallengeDetected, SessionHealth::Ok)));
        assert!(!capture_retryable(&cap(ChallengeHealth::Failed, SessionHealth::Ok)));
        assert!(!capture_retryable(&cap(ChallengeHealth::Ok, SessionHealth::Expired)));
    }

    #[test]
    fn enrichment_only_for_shortlisted() {
        assert!(enrichment_warranted(super::super::browser::BrowserTaskType::TokenTriggeredResolve));
        assert!(!enrichment_warranted(super::super::browser::BrowserTaskType::Search));
        assert!(!enrichment_warranted(super::super::browser::BrowserTaskType::Profile));
    }
}
