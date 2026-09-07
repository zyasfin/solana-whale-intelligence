//! REV-053-F03 regression: an accepted chain alias must not query a storage key that
//! is never written.
//!
//! `ChainKind::parse()` accepts `sol`, `SOL`, `Solana`, and `solana`, but the storage
//! key is always canonical (`chain.as_str()` = `solana`). The handlers validated the
//! raw path segment and then interpolated it, so `/api/tokens/sol/<mint>/recent`
//! returned `200 []` for a token that has rows under `solana:<mint>` — a false-empty
//! answer, which a caller reads as "no events" rather than as an error.
//!
//! This is a static guard rather than a live HTTP test because the failure is a
//! CONSTRUCTION mistake in source, and the class must stay closed for every future
//! handler: no handler may build a `<chain>:<mint>` key from a raw path string. The
//! live end-to-end proof runs separately against a real server.

use std::path::PathBuf;

fn source(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Lines that build a chain-qualified identity by interpolating a bare `chain`
/// binding, which is the raw path segment in every HTTP handler.
fn raw_key_constructions(src: &str) -> Vec<(usize, String)> {
    src.lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim();
            !t.starts_with("//") && t.contains("{chain}:")
        })
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

// No API handler may interpolate the raw path segment into a storage key.
#[test]
fn http_handlers_never_build_identity_keys_from_a_raw_path_segment() {
    for file in ["api.rs", "admin.rs"] {
        let offenders = raw_key_constructions(&source(file));
        assert!(
            offenders.is_empty(),
            "{file} builds a chain-qualified key from a raw path segment, so an \
             accepted alias such as `sol` queries a key that is never written and \
             returns a false-empty 200 (REV-053-F03). Use `chain.as_str()` after \
             parsing. Offenders: {offenders:#?}"
        );
    }
}

// Every recent/relations handler must bind the PARSED chain, not merely validate it.
// `parse(...).is_none()` discards the canonical value, which is how the alias bug
// survived: the check passed and the raw string was used anyway.
#[test]
fn recent_handlers_bind_the_parsed_chain_rather_than_validating_and_discarding_it() {
    for file in ["api.rs", "admin.rs"] {
        let src = source(file);
        let discarding: Vec<(usize, String)> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim();
                !t.starts_with("//") && t.contains("ChainKind::parse") && t.contains("is_none()")
            })
            .map(|(i, l)| (i + 1, l.trim().to_string()))
            .collect();
        assert!(
            discarding.is_empty(),
            "{file} validates a chain with `parse(..).is_none()` and throws the \
             canonical value away. Bind it (`let Some(chain) = ChainKind::parse(..)`) \
             so the key cannot be built from the alias (REV-053-F03). Sites: \
             {discarding:#?}"
        );
    }
}

// The bug only exists because more than one spelling is accepted per chain, and the
// canonical spelling is a DIFFERENT string from some accepted alias. This pins that
// premise from the source of truth (`ChainKind::parse` / `as_str`), so if the alias
// set is ever reduced to exact-match-only the guards above are still meaningful, and
// if it grows, the reason they exist is documented right here.
//
// `models` is a binary-only module (not re-exported by the library), so this reads the
// declaration rather than calling it. The behavioural round-trip is proven live over
// real HTTP against every accepted alias.
#[test]
fn more_than_one_spelling_is_accepted_per_chain() {
    let src = source("models.rs");

    assert!(
        src.contains(r#""solana" | "sol" => Some(ChainKind::Solana)"#),
        "expected `solana`/`sol` to both parse to Solana; if the alias set changed, \
         re-check that no handler builds a storage key from the raw path segment"
    );
    assert!(
        src.contains(r#"ChainKind::Solana => "solana""#),
        "expected `solana` to be the CANONICAL stored spelling"
    );
    assert!(
        src.contains(r#""robinhood" | "rh" => Some(ChainKind::Robinhood)"#),
        "expected `robinhood`/`rh` to both parse to Robinhood"
    );
    // Lower-casing means `SOL` and `Solana` are accepted too, so the alias-to-key gap
    // is wider than the two literal spellings above.
    assert!(
        src.contains("to_ascii_lowercase()"),
        "parse() case-folds, so mixed-case aliases are accepted as well"
    );
}
