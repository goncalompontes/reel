//! The real ext.to backend, driven against a stub site and a stub solver.
//!
//! This covers what the unit tests cannot: that a challenged search is handed to
//! a FlareSolverr-compatible solver, that the clearance it returns is replayed on
//! the next request, and that a solver failure is reported rather than swallowed.
//! No network, no real Cloudflare, no real browser.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reel_catalog::backends::ext_to::{ExtToBackend, SOURCE_NAME};
use reel_catalog::search::{SearchBackend, SearchQuery};

/// The Cloudflare interstitial the stub returns to anyone without clearance.
const CHALLENGE: &str = "<html><head><title>Just a moment...</title></head>\
                         <body>cf_chl_opt</body></html>";

/// A results page in the site's current shape: each title links to its page and
/// the listing carries no magnet link.
const RESULTS: &str = r#"<html><head><title>Download 100 s02e01 Torrent (2 results) - EXT Torrents</title></head>
<body><table class="search-table"><tbody>
<tr>
  <td class="text-left"><a href="/the-100-s06e01-1/" class="torrent-title-link"><b>The 100 S06E01 1080p</b></a>
    <div class="related-posted">Posted by <a class="external-user" href="?source%5B%5D=3"><strong>uploader</strong></a> in <a href="/tv/"><strong>TV</strong></a></div></td>
  <td><div class="add-block-wrapper"><span class="add-block">Size</span><span>676.9 MB</span></div></td>
  <td><div class="add-block-wrapper"><span class="add-block">Seeds</span><span>547</span></div></td>
</tr>
<tr>
  <td class="text-left"><a href="/the-100-s06e01-2/" class="torrent-title-link"><b>The 100 S06E01 720p</b></a>
    <div class="related-posted">Posted in <a href="/tv/"><strong>TV</strong></a></div></td>
  <td><div class="add-block-wrapper"><span class="add-block">Size</span><span>400 MB</span></div></td>
  <td><div class="add-block-wrapper"><span class="add-block">Seeds</span><span>12</span></div></td>
</tr>
</tbody></table></body></html>"#;

/// A token that the stub's torrent pages carry and the magnet endpoint checks.
const PAGE_TOKEN: &str = "tok123";
const CSRF_TOKEN: &str = "csrf456";

/// A torrent detail page: the two tokens the magnet call needs, and the magnet
/// button. No magnet link in the markup, matching the real site.
const DETAIL: &str = r#"<html><head><title>Sintel torrent</title></head><body>
<a href="javascript:void(0);" class="btn btn-primary detail-magnet" data-id="15037012" title="Get magnet">Magnet</a>
<script>window.pageToken = 'tok123';</script>
<script>window.csrfToken = 'csrf456';</script>
</body></html>"#;

/// The magnet the stub endpoint hands back for a correctly signed request.
const MAGNET: &str = "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd&dn=Sintel";

#[derive(Clone)]
struct Stub {
    direct_challenges: Arc<AtomicUsize>,
    direct_hits: Arc<AtomicUsize>,
    solves: Arc<AtomicUsize>,
    magnets: Arc<AtomicUsize>,
    solver_ok: bool,
}

impl Stub {
    fn new(solver_ok: bool) -> Self {
        Self {
            direct_challenges: Arc::new(AtomicUsize::new(0)),
            direct_hits: Arc::new(AtomicUsize::new(0)),
            solves: Arc::new(AtomicUsize::new(0)),
            magnets: Arc::new(AtomicUsize::new(0)),
            solver_ok,
        }
    }
}

/// The search page: only answers with results to a request carrying clearance.
async fn search_handler(State(stub): State<Stub>, headers: HeaderMap) -> Response {
    let cookie = headers
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if cookie.contains("cf_clearance=good") {
        stub.direct_hits.fetch_add(1, SeqCst);
        (StatusCode::OK, RESULTS).into_response()
    } else {
        stub.direct_challenges.fetch_add(1, SeqCst);
        (StatusCode::FORBIDDEN, CHALLENGE).into_response()
    }
}

/// A torrent detail page, also behind clearance.
async fn detail_handler(headers: HeaderMap) -> Response {
    let cookie = headers
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if cookie.contains("cf_clearance=good") {
        (StatusCode::OK, DETAIL).into_response()
    } else {
        (StatusCode::FORBIDDEN, CHALLENGE).into_response()
    }
}

/// `/ajax/getTorrentMagnet.php`: verifies the SHA-256 token and the clearance,
/// then answers with a magnet.
async fn magnet_handler(State(stub): State<Stub>, headers: HeaderMap, body: String) -> Response {
    stub.magnets.fetch_add(1, SeqCst);
    let cookie = headers
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !cookie.contains("cf_clearance=good") {
        return (StatusCode::FORBIDDEN, CHALLENGE).into_response();
    }

    let form: std::collections::HashMap<&str, &str> = body
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();
    let id = form.get("torrent_id").copied().unwrap_or("");
    let timestamp = form.get("timestamp").copied().unwrap_or("");
    let hmac = form.get("hmac").copied().unwrap_or("");
    let expected = sha256_hex(&format!("{id}|{timestamp}|{PAGE_TOKEN}"));
    if hmac != expected || form.get("sessid").copied() != Some(CSRF_TOKEN) {
        return (StatusCode::BAD_REQUEST, "bad hmac").into_response();
    }

    (
        StatusCode::OK,
        format!(r#"{{"success":true,"url":"{MAGNET}","type":"magnet"}}"#),
    )
        .into_response()
}

/// The same digest the backend computes, for the stub to verify against.
fn sha256_hex(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A FlareSolverr-compatible `/v1` endpoint.
async fn solver_handler(State(stub): State<Stub>, Json(body): Json<serde_json::Value>) -> Response {
    stub.solves.fetch_add(1, SeqCst);
    assert_eq!(body["cmd"], "request.get", "the solver must be asked to fetch");

    if !stub.solver_ok {
        return (
            StatusCode::OK,
            r#"{"status":"error","message":"Challenge not detected"}"#,
        )
            .into_response();
    }

    let solution = serde_json::json!({
        "status": "ok",
        "solution": {
            "response": RESULTS,
            "cookies": [
                {"name": "cf_clearance", "value": "good"},
                {"name": "__cf_bm", "value": "bm"}
            ],
            "userAgent": "Mozilla/5.0 (Solver)"
        }
    });
    (StatusCode::OK, Json(solution)).into_response()
}

async fn spawn_stub(solver_ok: bool) -> (String, Stub) {
    let stub = Stub::new(solver_ok);
    let app = Router::new()
        .route("/browse/", get(search_handler))
        .route("/the-100-s06e01-1/", get(detail_handler))
        .route("/the-100-s06e01-2/", get(detail_handler))
        .route("/ajax/getTorrentMagnet.php", post(magnet_handler))
        .route("/v1", post(solver_handler))
        .with_state(stub.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), stub)
}

#[tokio::test]
async fn the_solver_clears_the_challenge_and_the_clearance_is_reused() {
    let (base, stub) = spawn_stub(true).await;
    let backend = ExtToBackend::new().with_base_url(&base).without_bundled_solver().with_solver_url(&base);
    assert_eq!(backend.name(), SOURCE_NAME);

    // First search: challenged directly, then solved.
    let hits = backend
        .search(&SearchQuery::new("100 s02e01"))
        .await
        .expect("the solver should clear the challenge");
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.detail_url.is_some()), "each hit links to its page");
    assert_eq!(hits[0].seeders, Some(547));
    assert_eq!(stub.direct_challenges.load(SeqCst), 1, "the first try was challenged");
    assert_eq!(stub.solves.load(SeqCst), 1, "the solver was asked once");

    // Second search: the cached clearance passes directly, so no second solve.
    let direct_before = stub.direct_hits.load(SeqCst);
    let hits = backend
        .search(&SearchQuery::new("100 s02e01"))
        .await
        .expect("the cached clearance should be accepted");
    assert_eq!(hits.len(), 2);
    assert_eq!(stub.solves.load(SeqCst), 1, "the solver must not be called again");
    assert!(
        stub.direct_hits.load(SeqCst) > direct_before,
        "the cached clearance was replayed"
    );
}

#[tokio::test]
async fn the_magnet_for_a_listing_row_is_resolved_and_signed() {
    let (base, stub) = spawn_stub(true).await;
    let backend = ExtToBackend::new().with_base_url(&base).without_bundled_solver().with_solver_url(&base);

    let hits = backend
        .search(&SearchQuery::new("100 s02e01"))
        .await
        .expect("the solver should clear the challenge");
    assert!(hits[0].magnet.is_none(), "the listing carries no magnet");

    // One page load plus one signed request yields the magnet.
    let magnet = backend
        .magnet_for(&hits[0])
        .await
        .expect("resolving must not fail")
        .expect("a magnet is expected");
    assert_eq!(magnet, MAGNET);
    assert_eq!(stub.magnets.load(SeqCst), 1, "the magnet endpoint was asked once");
}

#[tokio::test]
async fn a_challenge_without_a_solver_is_an_actionable_error() {
    let (base, _stub) = spawn_stub(true).await;
    // A stale cookie but no solver: the challenge cannot be cleared.
    let backend = ExtToBackend::new().with_base_url(&base).without_bundled_solver().with_cookie("cf_clearance=stale");

    let error = backend
        .search(&SearchQuery::new("x"))
        .await
        .expect_err("a challenge is not results");
    let message = error.to_string();
    assert!(message.contains("Cloudflare"), "{message}");
    assert!(message.contains("stale"), "{message}");
}

#[tokio::test]
async fn a_solver_that_fails_is_reported() {
    let (base, stub) = spawn_stub(false).await;
    let backend = ExtToBackend::new().with_base_url(&base).without_bundled_solver().with_solver_url(&base);

    let error = backend
        .search(&SearchQuery::new("x"))
        .await
        .expect_err("a failing solver is not results");
    assert!(error.to_string().contains("Challenge not detected"), "{error}");
    assert_eq!(stub.solves.load(SeqCst), 1);
}
