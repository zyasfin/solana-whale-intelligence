//! REV-109: static contract for the Indonesian Signal Forge operator dashboard.
//!
//! These are the STRUCTURAL half of the proof. The behavioural half (navigation,
//! per-view reads, failure states, inert XSS payload, explicit mutation +
//! readback, secret not retained) is a real-browser probe run against a
//! deterministic mock HTTP server; see the REV-109 ledger entry.

use std::fs;
use std::path::PathBuf;

fn dashboard() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static/index.html");
    fs::read_to_string(path).expect("read dashboard")
}

#[test]
fn dashboard_exposes_the_full_operator_workspace() {
    let html = dashboard();
    for needle in [
        "lang=\"id\"",
        "data-view=\"overview\"",
        "data-view=\"token\"",
        "data-view=\"wallets\"",
        "data-view=\"funding\"",
        "data-view=\"signals\"",
        "data-view=\"clusters\"",
        "data-view=\"telegram\"",
        "data-view=\"queues\"",
        "data-view=\"settings\"",
        "/api/metrics/overview",
        "/api/wallets",
        "/api/funding/radar/cases",
        "/api/signals",
        "/api/clusters",
        "/api/telegram/channels",
        "/api/queues",
        "/api/settings/env",
    ] {
        assert!(html.contains(needle), "dashboard missing {needle}");
    }
}

#[test]
fn dashboard_copy_is_operator_friendly_and_accessible() {
    let html = dashboard();
    for copy in [
        "Ringkasan",
        "Intelijen token",
        "Dompet",
        "Radar dana",
        "Sinyal",
        "Klaster",
        "Antrean",
        "Pengaturan",
        "Belum ada data",
        "aria-live=\"polite\"",
        "aria-label=\"Navigasi utama\"",
        "@media (max-width: 760px)",
    ] {
        assert!(html.contains(copy), "dashboard missing copy/accessibility contract: {copy}");
    }
    for stale in [
        "Enter a contract address.",
        "No query yet.",
        "capability unavailable",
        "DB/API failure",
        ">Load<",
    ] {
        assert!(!html.contains(stale), "stiff legacy copy remains: {stale}");
    }
}

/// Every view must be wired to the endpoints `src/admin.rs` actually serves.
/// A view that renders without ever naming its API is decoration, not a console.
#[test]
fn every_view_is_wired_to_a_real_admin_endpoint() {
    let html = dashboard();
    for path in [
        "/api/health",
        "/api/metrics/overview",
        "/api/metrics/signals/timeline",
        "/api/metrics/radar/trend",
        "/api/queues",
        "/api/tokens/",
        "/report",
        "/recent?window=",
        "/relations",
        "/scores",
        "/labels",
        "/revoke",
        "/api/funding/radar/cases",
        "/api/signals/rejections",
        "/api/clusters",
        "/api/telegram/channels",
        "/api/telegram/channels/bulk",
        "/pause",
        "/api/settings/env",
        "/api/settings/runtime",
        "/api/settings/secrets",
    ] {
        assert!(html.contains(path), "no view calls {path}");
    }
    // Endpoints that do not exist must not be invented.
    for absent in [
        "/api/clusters/",
        "/api/signals/",
        "/api/funding/radar/timeline",
        "/api/tokens/search",
    ] {
        // `/api/signals/rejections` is real; only that one may match the prefix.
        let hits = html.matches(absent).count();
        let allowed = if absent == "/api/signals/" { html.matches("/api/signals/rejections").count() } else { 0 };
        assert_eq!(hits, allowed, "dashboard calls a nonexistent endpoint: {absent}");
    }
}

/// Untrusted API/provider/DB strings must never reach markup or a handler
/// attribute raw. One proven escaper, DOM listeners only, per-segment encoding.
#[test]
fn untrusted_values_cross_one_escaping_boundary() {
    let html = dashboard();
    assert!(html.contains("function esc("), "the single HTML escaper is gone");
    for entity in ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;"] {
        assert!(html.contains(entity), "esc must map to {entity}");
    }
    assert!(
        html.contains("encodeURIComponent"),
        "path/query segments must be encoded individually"
    );
    // No inline event handler attributes at all: with none present, no dynamic
    // value can ever be interpolated into one.
    for handler in ["onclick=", "onerror=", "onload=", "onchange=", "onsubmit=", "onmouseover=", "oninput="] {
        assert!(!html.contains(handler), "inline handler attribute {handler} is an injection sink");
    }
    // Legacy REV-062-F02 fields stay escaped.
    for bare in [
        "${e.event_type}",
        "${e.occurred_at}",
        "${e.observed_at}",
        "${e.confidence_level}",
        "${e.coverage}",
        "${e.capability_status}",
        "${c.to_identity.kind}",
        "${c.to_identity.value}",
        "${c.confidence}",
        "${missing.join(', ')}",
        "${(e.evidence_refs || []).join(', ')}",
        "${(c.evidence_refs || []).join(', ')}",
        "${missingUnion.join(', ')}",
    ] {
        assert!(!html.contains(bare), "`{bare}` reaches markup unescaped (REV-062-F02)");
    }
}

/// Mutations are operator-initiated only, confirmed, deduplicated while pending,
/// and followed by an authoritative readback. Secrets are write-only.
#[test]
fn mutations_are_explicit_confirmed_and_read_back() {
    let html = dashboard();
    for needle in [
        "function confirmAction(",
        "data-confirm",
        "aria-busy",
        "type=\"password\"",
        "Konfirmasi",
        "Muat ulang otoritatif",
    ] {
        assert!(html.contains(needle), "mutation-safety contract missing: {needle}");
    }
    // A pending guard must exist so a double click cannot double-mutate.
    assert!(
        html.contains("pending") && html.contains("disabled = true"),
        "no in-flight guard against duplicate submission"
    );
    // The disposition vocabulary submitted to the backend must be exactly the
    // `Disposition::parse` values; the display strings are separate.
    for value in ["value=\"score\"", "value=\"watch\"", "value=\"flow_only\"", "value=\"skip\""] {
        assert!(html.contains(value), "backend disposition value missing: {value}");
    }
    // Only allowlisted secret names may be offered.
    for name in ["DATABASE_URL", "TG_API_ID", "TG_API_HASH", "TG_SESSION_PATH", "TELEGRAM_BOT_TOKEN", "TELEGRAM_CHAT_ID"] {
        assert!(html.contains(name), "secret allowlist entry missing: {name}");
    }
    assert!(!html.contains("HELIUS_KEY_1\""), "env-only secret offered as editable");
}

/// Each view must be able to say which of the distinct states it is in; a UI
/// that collapses "kosong", "gagal" and "belum ditanya" is what REV-023 forbids.
#[test]
fn state_vocabulary_distinguishes_every_outcome() {
    let html = dashboard();
    for phrase in [
        "Belum ada kueri",
        "Memuat",
        "Belum ada data",
        "Cakupan sebagian",
        "Data mungkin usang",
        "Koneksi terputus",
        "Sesi berakhir",
        "Gangguan server",
        "Respons tidak dikenali",
        "Belum tersedia",
        "Perubahan tersimpan",
    ] {
        assert!(html.contains(phrase), "state vocabulary missing: {phrase}");
    }
    assert!(html.contains("prefers-reduced-motion"), "reduced motion is not respected");
    assert!(html.contains(":focus-visible"), "keyboard focus is not visible");
}

/// The Token Recent semantics REV-023/REV-027/REV-058/REV-060/REV-062 settled
/// must survive the redesign.
#[test]
fn legacy_token_recent_semantics_survive() {
    let html = dashboard();
    for needle in [
        "data-filter=\"cross_chain\"",
        "data-filter=\"social\"",
        "data-filter=\"deployer\"",
        "data-filter=\"official\"",
        "data-filter=\"copycat\"",
        "official_ca_announcement",
        "suspected_copycat",
        "reused_social_link",
        "cross_chain_deployment",
        "is_current_coverage",
        "coverage_disclosure",
        "missing_inputs",
        "evidence_refs",
    ] {
        assert!(html.contains(needle), "legacy relation semantics lost: {needle}");
    }
    // Absence of a relation under partial coverage is not a finding of no reuse.
    assert!(
        html.contains("bukan temuan"),
        "the partial-coverage disclaimer was dropped"
    );
    // Same-symbol alone stays unrelated.
    assert!(html.contains("simbol"), "the same-symbol caveat was dropped");
}

/// REV-110-F01: the shipped page must contain no harness placeholder call.
/// `__omp_shell(...)` is undefined in a browser, so the expression it stands in
/// throws at render time and a perfectly valid `/api/health` is reported as
/// "Respons tidak dikenali". A placeholder is not a defect of one line: any
/// `__omp_` token in the served asset is an unexecutable stub.
#[test]
fn no_harness_placeholder_survives_in_the_served_page() {
    let html = dashboard();
    assert!(
        !html.contains("__omp_"),
        "harness placeholder token `__omp_` is present in the served dashboard"
    );
    // The health verdict must be computed from the parsed latency itself.
    assert!(
        html.contains("!Number.isFinite(lat) || lat < 0"),
        "database health must be decided by a finite, non-negative latency test"
    );
}

/// REV-110-F02: repeated failed refreshes must not stack staleness banners.
/// The banner has to be identifiable and the previous one replaced, while the
/// last-known data underneath stays intact.
#[test]
fn the_staleness_banner_is_replaced_not_stacked() {
    let html = dashboard();
    assert!(
        html.contains("banner.dataset.stale = '1'"),
        "the staleness banner carries no marker, so it cannot be de-duplicated"
    );
    assert!(
        html.contains("[data-stale=\"1\"]"),
        "no lookup of the previous staleness banner before inserting a new one"
    );
    assert!(
        html.contains("removeChild(prev)"),
        "the previous staleness banner is never removed"
    );
    // The degrade path must still keep the stale content rather than clearing.
    let load_fn = html.split("async function load(node, path, render, opts)").nth(1).expect("load()");
    let body = &load_fn[..load_fn.find("function table(").unwrap_or(load_fn.len())];
    assert!(
        !body.contains("clear(node);\n        note"),
        "the failure branch must not erase last-known data"
    );
}

/// REV-110-F03: the secret field is cleared before the request is initiated,
/// not after it resolves. A 20 s hung request must not leave the plaintext
/// secret sitting in the DOM.
#[test]
fn the_secret_field_is_cleared_before_the_request_is_sent() {
    let html = dashboard();
    let form = html
        .split("$('st-secret').addEventListener('submit'")
        .nth(1)
        .expect("secret submit handler");
    let form = &form[..form.find("$('sec-del')").expect("secret delete handler")];
    let cleared = form.find("$('sec-value').value = '';").expect("secret field is never cleared");
    let sent = form.find("api('/api/settings/secrets'").expect("secret POST");
    assert!(
        cleared < sent,
        "the secret is still in the DOM while the request is in flight"
    );
    // The captured value must be a local, never re-read from the DOM later.
    assert!(
        form.contains("body: { name, value }"),
        "the request must send the captured local value, not a re-read of the input"
    );
}
