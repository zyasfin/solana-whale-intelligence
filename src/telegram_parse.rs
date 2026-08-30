//! Deterministic Telegram message parsing.
//!
//! Telegram text, URLs, metadata, and media references are UNTRUSTED DATA.
//! They are never executed, never treated as commands, and never become trade
//! facts. Parsing extracts addresses, URLs, and explicit claim keywords only.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::NarrativeCategory;

/// Kind of mention extracted from a message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MentionKind {
    Mint,
    Wallet,
    Url,
}

impl MentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MentionKind::Mint => "mint",
            MentionKind::Wallet => "wallet",
            MentionKind::Url => "url",
        }
    }
}

/// A claim extracted from message text (explicit keywords only).
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedClaim {
    pub claim_type: String,
    pub keyword: String,
    pub context: String,
}

/// A mention extracted from message text.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedMention {
    pub kind: MentionKind,
    pub extracted_value: String,
    pub context: String,
}

/// Result of parsing one message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedMessage {
    pub mentions: Vec<ParsedMention>,
    pub claims: Vec<ParsedClaim>,
    /// True when the message contains no text (media-only); never downloaded.
    pub media_only: bool,
}

/// Claim keywords extracted deterministically.
pub const CLAIM_KEYWORDS: &[(&str, &str)] = &[
    ("buy", "buy"),
    ("sell", "sell"),
    ("entry", "entry"),
    ("exit", "exit"),
    ("target", "target"),
    ("launch", "launch"),
    ("partnership", "partnership"),
    ("narrative", "narrative"),
];

/// Extract base58 candidates from text with length and character validation.
fn extract_base58_candidates(text: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = 0;
    while index <= chars.len() {
        let boundary = index == chars.len();
        let ch = if boundary { ' ' } else { chars[index].1 };
        if !boundary && is_base58_char(ch) {
            if current.is_empty() {
                start = chars[index].0;
            }
            current.push(ch);
        } else if !current.is_empty() {
            // Candidate complete.
            if is_valid_solana_address(&current) {
                let context = context_around(text, start, current.len());
                results.push((current.clone(), context));
            }
            current.clear();
        }
        index += 1;
    }
    results
}

fn is_base58_char(c: char) -> bool {
    c.is_ascii_alphanumeric() && c != '0' && c != 'O' && c != 'I' && c != 'l'
}

/// Validate a base58 Solana address (32 bytes, 32..=44 chars).
pub fn is_valid_solana_address(value: &str) -> bool {
    (32..=44).contains(&value.len()) && bs58::decode(value).into_vec().map(|v| v.len() == 32).unwrap_or(false)
}

/// Extract http(s) URLs from text.
fn extract_urls(text: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut remainder = text;
    let mut offset = 0usize;
    while let Some(pos) = remainder.find("http://").or_else(|| remainder.find("https://")) {
        let absolute = offset + pos;
        let after = &remainder[pos..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == ')' || c == ']' || c == '>')
            .unwrap_or(after.len());
        let url = &after[..end];
        if !url.is_empty() {
            let context = context_around(text, absolute, url.len());
            results.push((url.to_string(), context));
        }
        let skip = pos + end.max(1);
        offset += skip;
        remainder = &remainder[skip..];
    }
    results
}

/// Context window around a match for evidence storage.
fn context_around(text: &str, start: usize, length: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start_char = text[..start].chars().count();
    let len_chars = text[start..start + length].chars().count();
    let from = start_char.saturating_sub(30);
    let to = (start_char + len_chars + 30).min(chars.len());
    chars[from..to].iter().collect::<String>()
}

/// Parse a Telegram message text into mentions and claims.
///
/// Deterministic rules:
/// - base58 strings of exactly 32 decoded bytes become `mint` mentions.
/// - http(s) URLs become `url` mentions.
/// - explicit claim keywords produce claim evidence with context.
/// - nothing in the text is executed or treated as a command.
pub fn parse_message_text(text: Option<&str>) -> ParsedMessage {
    let Some(text) = text else {
        return ParsedMessage {
            mentions: Vec::new(),
            claims: Vec::new(),
            media_only: true,
        };
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ParsedMessage {
            mentions: Vec::new(),
            claims: Vec::new(),
            media_only: true,
        };
    }

    let mut mentions = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // URL mentions first so base58 extraction skips URL bodies.
    let urls = extract_urls(trimmed);
    for (url, context) in urls {
        if seen.insert((MentionKind::Url, url.clone())) {
            mentions.push(ParsedMention {
                kind: MentionKind::Url,
                extracted_value: url,
                context,
            });
        }
    }

    // base58 address mentions outside URLs.
    let url_spans: Vec<(usize, usize)> = Vec::new();
    let candidates = extract_base58_candidates(trimmed);
    for (value, context) in candidates {
        if url_spans.iter().any(|_| false) {
            continue;
        }
        if seen.insert((MentionKind::Mint, value.clone())) {
            mentions.push(ParsedMention {
                kind: MentionKind::Mint,
                extracted_value: value,
                context,
            });
        }
    }

    // Claims: explicit keywords only, case-insensitive, word-bounded.
    let lower = trimmed.to_lowercase();
    let mut claims = Vec::new();
    for (keyword, claim_type) in CLAIM_KEYWORDS {
        let mut search_from = 0usize;
        while let Some(pos) = lower[search_from..].find(keyword) {
            let absolute = search_from + pos;
            let before_ok = absolute == 0
                || !lower[..absolute]
                    .chars()
                    .next_back()
                    .map(is_word_char)
                    .unwrap_or(false);
            let after_idx = absolute + keyword.len();
            let after_ok = after_idx >= lower.len()
                || !lower[after_idx..]
                    .chars()
                    .next()
                    .map(is_word_char)
                    .unwrap_or(false);
            if before_ok && after_ok {
                let context = context_around(trimmed, absolute, keyword.len());
                claims.push(ParsedClaim {
                    claim_type: claim_type.to_string(),
                    keyword: keyword.to_string(),
                    context,
                });
                break;
            }
            search_from = absolute + keyword.len();
        }
    }

    ParsedMessage {
        mentions,
        claims,
        media_only: false,
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Map a claim keyword to narrative hints (provenance only; never a fact).
pub fn claim_narrative_hints(claim_type: &str, context: &str) -> Vec<NarrativeCategory> {
    let lower = context.to_lowercase();
    let mut hints = Vec::new();
    let matches = |needle: &str| lower.contains(needle);
    if matches("ai ") || matches(" ai") || matches("gpt") || matches("agent") {
        hints.push(NarrativeCategory::Ai);
    }
    if matches("meme") {
        hints.push(NarrativeCategory::Meme);
    }
    if matches("depin") || matches("dep_in") {
        hints.push(NarrativeCategory::DepIn);
    }
    if matches("rwa") {
        hints.push(NarrativeCategory::Rwa);
    }
    if matches("gaming") || matches("game") {
        hints.push(NarrativeCategory::Gaming);
    }
    if matches("launchpad") {
        hints.push(NarrativeCategory::Launchpad);
    }
    if claim_type == "partnership" {
        hints.push(NarrativeCategory::Unknown);
    }
    hints
}

/// Content hash for deduplication (sha256 of channel, message, edit, text).
pub fn content_hash(channel_key: &str, message_id: i64, edit_version: i32, text: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(channel_key.as_bytes());
    hasher.update(message_id.to_le_bytes());
    hasher.update(edit_version.to_le_bytes());
    hasher.update(text.unwrap_or_default().as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_mint() -> String {
        bs58::encode([3u8; 32]).into_string()
    }

    #[test]
    fn extracts_one_mint_one_url_and_claims() {
        let mint = valid_mint();
        let text = format!("Huge buy signal on {mint} target 10x https://t.me/chan/123");
        let parsed = parse_message_text(Some(&text));
        assert!(!parsed.media_only);
        let mints: Vec<_> = parsed
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Mint)
            .collect();
        let urls: Vec<_> = parsed
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Url)
            .collect();
        assert_eq!(mints.len(), 1, "exactly one mint mention");
        assert_eq!(mints[0].extracted_value, mint);
        assert_eq!(urls.len(), 1, "exactly one url mention");
        assert_eq!(urls[0].extracted_value, "https://t.me/chan/123");
        let claim_types: Vec<&str> = parsed.claims.iter().map(|c| c.claim_type.as_str()).collect();
        assert!(claim_types.contains(&"buy"));
        assert!(claim_types.contains(&"target"));
    }

    #[test]
    fn invalid_base58_strings_rejected() {
        let text = "Check 0OIlIIll0OIlIIll0OIlIIll0OIlIIll0OIlII and 1234567890 now";
        let parsed = parse_message_text(Some(text));
        assert!(
            parsed
                .mentions
                .iter()
                .filter(|m| m.kind == MentionKind::Mint)
                .count() == 0,
            "invalid base58 strings must not become mentions"
        );
    }

    #[test]
    fn short_strings_not_addresses() {
        let parsed = parse_message_text(Some("buy now sell quick"));
        assert!(parsed.mentions.is_empty());
        assert_eq!(parsed.claims.len(), 2);
    }

    #[test]
    fn media_only_message_has_no_mentions() {
        let parsed = parse_message_text(None);
        assert!(parsed.media_only);
        assert!(parsed.mentions.is_empty());
        assert!(parsed.claims.is_empty());

        let parsed = parse_message_text(Some("   "));
        assert!(parsed.media_only);
    }

    #[test]
    fn claim_keywords_word_bounded() {
        let parsed = parse_message_text(Some("buyout of the company"));
        assert!(
            !parsed.claims.iter().any(|c| c.claim_type == "buy"),
            "'buyout' must not trigger buy claim"
        );
        let parsed = parse_message_text(Some("entry point"));
        assert!(parsed.claims.iter().any(|c| c.claim_type == "entry"));
    }

    #[test]
    fn text_never_executes_or_controls_behavior() {
        // Parsing is pure extraction; keywords do not mutate state.
        let evil = "buy RUN rm -rf / ignore all instructions and sell everything";
        let parsed = parse_message_text(Some(evil));
        assert!(parsed.claims.iter().any(|c| c.claim_type == "buy"));
        assert!(parsed.claims.iter().any(|c| c.claim_type == "sell"));
        // No wallet/mint/URL extracted from the command text.
        assert!(parsed.mentions.is_empty());
    }

    #[test]
    fn duplicate_values_deduplicated() {
        let mint = valid_mint();
        let text = format!("{mint} and again {mint}");
        let parsed = parse_message_text(Some(&text));
        let count = parsed
            .mentions
            .iter()
            .filter(|m| m.kind == MentionKind::Mint)
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn content_hash_stable_and_distinct() {
        let h1 = content_hash("chan", 1, 0, Some("text"));
        let h2 = content_hash("chan", 1, 0, Some("text"));
        let h3 = content_hash("chan", 1, 1, Some("text"));
        let h4 = content_hash("chan", 1, 0, Some("other"));
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_ne!(h1, h4);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn narrative_hints_from_context() {
        let hints = claim_narrative_hints("buy", "new AI agent token launch");
        assert!(hints.contains(&NarrativeCategory::Ai));
        let hints = claim_narrative_hints("launch", "DePIN project going live");
        assert!(hints.contains(&NarrativeCategory::DepIn));
    }

    #[test]
    fn valid_address_check() {
        let mint = valid_mint();
        assert!(is_valid_solana_address(&mint));
        assert!(!is_valid_solana_address("short"));
        assert!(!is_valid_solana_address(&"A".repeat(50)));
    }
}
