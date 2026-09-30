//! The real TMDB client, driven against a stub server.
//!
//! This exercises the whole request/response path — query building, auth style,
//! error handling, candidate scoring, detail fetching and artwork caching —
//! without an API key and without touching the real API. Everything the unit
//! tests cannot reach lives here.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use reel_catalog::cache::CatalogCache;
use reel_catalog::matching::LookupQuery;
use reel_catalog::provider::{MetadataProvider, TmdbProvider};
use reel_catalog::tmdb::TmdbClient;

/// A byte string that stands in for a JPEG; nothing decodes it in this test.
const POSTER_BYTES: &[u8] = b"\xff\xd8\xff\xe0 fake-but-plausible-jpeg \xff\xd9";
const BACKDROP_BYTES: &[u8] = b"\xff\xd8\xff\xe0 fake-backdrop \xff\xd9";

#[derive(Clone, Default)]
struct Stub {
    /// Every request we received, as `path?query` plus the auth header.
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

#[derive(Debug, Clone)]
struct SeenRequest {
    path: String,
    query: String,
    authorization: Option<String>,
}

async fn record(stub: &Stub, path: &str, query: &str, headers: &HeaderMap) {
    stub.seen.lock().unwrap().push(SeenRequest {
        path: path.to_string(),
        query: query.to_string(),
        authorization: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    });
}

async fn search_handler(
    State(stub): State<Stub>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    record(&stub, uri.path(), uri.query().unwrap_or(""), &headers).await;

    // Respond differently to an unauthorised request, like the real API.
    let query = uri.query().unwrap_or("");
    let has_auth = headers.contains_key("authorization") || query.contains("api_key=");
    if !has_auth {
        return (
            StatusCode::UNAUTHORIZED,
            r#"{"status_code":7,"status_message":"Invalid API key: You must be granted a valid key.","success":false}"#,
        )
            .into_response();
    }

    let body = r#"{
      "page": 1,
      "results": [
        {"id": 604, "title": "The Matrix Reloaded", "release_date": "2003-05-15",
         "overview": "Six months later.", "popularity": 30.0, "vote_average": 7.0,
         "vote_count": 11000, "poster_path": "/reloaded.jpg", "backdrop_path": "/reloaded-bd.jpg"},
        {"id": 603, "title": "The Matrix", "original_title": "The Matrix", "release_date": "1999-03-30",
         "overview": "A hacker learns the truth.", "popularity": 42.5, "vote_average": 8.2,
         "vote_count": 25417, "poster_path": "/matrix.jpg", "backdrop_path": "/matrix-bd.jpg"}
      ],
      "total_pages": 1, "total_results": 2
    }"#;
    (StatusCode::OK, body).into_response()
}

async fn movie_handler(
    State(stub): State<Stub>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    record(&stub, uri.path(), uri.query().unwrap_or(""), &headers).await;

    if id != 603 {
        return (
            StatusCode::NOT_FOUND,
            r#"{"status_code":34,"status_message":"The resource you requested could not be found.","success":false}"#,
        )
            .into_response();
    }

    let body = r#"{
      "id": 603,
      "title": "The Matrix",
      "original_title": "The Matrix",
      "overview": "A hacker learns the truth.",
      "tagline": "Welcome to the Real World.",
      "release_date": "1999-03-30",
      "runtime": 136,
      "genres": [{"id": 28, "name": "Action"}, {"id": 878, "name": "Science Fiction"}],
      "vote_average": 8.219,
      "vote_count": 25417,
      "popularity": 42.5678,
      "poster_path": "/matrix.jpg",
      "backdrop_path": "/matrix-bd.jpg"
    }"#;
    (StatusCode::OK, body).into_response()
}

async fn image_handler(
    State(stub): State<Stub>,
    // The route has two parameters; extracting one would fail the handler.
    Path((_size, file)): Path<(String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    record(&stub, uri.path(), uri.query().unwrap_or(""), &headers).await;

    let bytes: &[u8] = if file.contains("matrix.jpg") {
        POSTER_BYTES
    } else {
        BACKDROP_BYTES
    };
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "image/jpeg")],
        bytes.to_vec(),
    )
        .into_response()
}

/// Start the stub server and return its base URL plus the recorder.
async fn spawn_stub() -> (String, Stub) {
    let stub = Stub::default();
    let app = Router::new()
        .route("/3/search/movie", get(search_handler))
        .route("/3/movie/{id}", get(movie_handler))
        .route("/img/{size}/{file}", get(image_handler))
        .with_state(stub.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), stub)
}

/// Initialise logging once, so `RUST_LOG=reel_catalog=debug cargo test` explains
/// a failure instead of just asserting.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("reel_catalog=debug")),
        )
        .with_target(false)
        .try_init();
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-tmdb-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn client(base: &str, key: &str) -> TmdbClient {
    TmdbClient::new(key)
        .unwrap()
        .with_base_url(format!("{base}/3"))
        .with_image_base(format!("{base}/img"))
}

#[tokio::test]
async fn resolves_a_title_and_caches_its_artwork() {
    init_tracing();
    let (base, stub) = spawn_stub().await;
    let cache = Arc::new(CatalogCache::new(scratch("resolve")));
    let provider = TmdbProvider::new(client(&base, "v3-key"), cache.clone());

    // A real torrent name, so the cleaning step is exercised too.
    let metadata = provider
        .lookup(LookupQuery::from_release_name(
            "The.Matrix.1999.1080p.BluRay.x264-GROUP",
        ))
        .await
        .expect("lookup should succeed")
        .expect("the original should match");

    assert_eq!(metadata.source_id, "603", "the sequel must not win");
    assert_eq!(metadata.title, "The Matrix");
    assert_eq!(metadata.year, Some(1999));
    assert_eq!(metadata.runtime_label().as_deref(), Some("2h 16m"));
    assert_eq!(metadata.genres, vec!["Action", "Science Fiction"]);
    assert_eq!(metadata.tagline.as_deref(), Some("Welcome to the Real World."));

    // Artwork was downloaded and wired up for the UI.
    let poster = metadata.artwork.poster.as_ref().expect("poster");
    assert!(poster.is_cached(), "poster should be on disk");
    assert!(poster.local_uri().unwrap().starts_with("file://"));
    assert_eq!(
        std::fs::read(poster.local_path.as_ref().unwrap()).unwrap(),
        POSTER_BYTES
    );

    let backdrop = metadata.artwork.backdrop.as_ref().expect("backdrop");
    assert!(backdrop.is_cached());
    assert_eq!(
        std::fs::read(backdrop.local_path.as_ref().unwrap()).unwrap(),
        BACKDROP_BYTES
    );

    // The cache remembers which title resolved to which id.
    let second = provider
        .lookup(LookupQuery::new("The Matrix", Some(1999)))
        .await
        .unwrap()
        .expect("cached");
    assert_eq!(second.source_id, "603");

    let requests = stub.seen.lock().unwrap().clone();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/3/search/movie"), "{paths:?}");
    assert!(paths.contains(&"/3/movie/603"), "{paths:?}");
    assert!(paths.iter().any(|p| p.starts_with("/img/")), "{paths:?}");

    // A v3 key travels as a query parameter.
    let search = requests
        .iter()
        .find(|r| r.path == "/3/search/movie")
        .expect("search request");
    assert!(search.query.contains("query=The+Matrix") || search.query.contains("query=The%20Matrix"),
        "query was {:?}", search.query);
    assert!(search.query.contains("year=1999"), "query was {:?}", search.query);
    assert!(search.query.contains("api_key=v3-key"), "query was {:?}", search.query);
    assert!(search.authorization.is_none());

    let _ = std::fs::remove_dir_all(cache.root());
}

#[tokio::test]
async fn a_v4_token_is_sent_as_a_bearer_header() {
    let (base, stub) = spawn_stub().await;
    let cache = Arc::new(CatalogCache::new(scratch("bearer")));
    let provider = TmdbProvider::new(client(&base, "eyJhbGciOiJIUzI1NiJ9.payload.signature"), cache.clone());

    let metadata = provider
        .lookup(LookupQuery::new("The Matrix", Some(1999)))
        .await
        .unwrap()
        .expect("lookup should succeed");
    assert_eq!(metadata.source_id, "603");

    let requests = stub.seen.lock().unwrap().clone();
    let search = requests
        .iter()
        .find(|r| r.path == "/3/search/movie")
        .expect("search request");

    assert_eq!(
        search.authorization.as_deref(),
        Some("Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature")
    );
    assert!(
        !search.query.contains("api_key"),
        "a v4 token must not be sent as a query parameter: {:?}",
        search.query
    );

    let _ = std::fs::remove_dir_all(cache.root());
}

#[tokio::test]
async fn a_rejected_key_surfaces_as_an_api_error() {
    let (_base, _stub) = spawn_stub().await;
    let cache = Arc::new(CatalogCache::new(scratch("noauth")));

    // An empty key is refused outright...
    assert!(matches!(
        TmdbClient::new(""),
        Err(reel_catalog::CatalogError::NotConfigured)
    ));

    // ...and an unreachable host becomes an HTTP error rather than a panic.
    let unreachable = TmdbProvider::new(
        TmdbClient::new("v3-key")
            .unwrap()
            // Point at a port nothing is listening on.
            .with_base_url("http://127.0.0.1:1/3"),
        cache.clone(),
    );
    let error = unreachable
        .lookup(LookupQuery::new("The Matrix", Some(1999)))
        .await
        .expect_err("should fail to connect");
    assert!(
        matches!(error, reel_catalog::CatalogError::Http(_)),
        "expected an HTTP error, got {error:?}"
    );

    let _ = std::fs::remove_dir_all(cache.root());
}

#[tokio::test]
async fn a_title_with_no_good_candidate_is_a_clean_miss() {
    let (base, _stub) = spawn_stub().await;
    let cache = Arc::new(CatalogCache::new(scratch("miss")));
    let provider = TmdbProvider::new(client(&base, "v3-key"), cache.clone());

    // The stub only knows about The Matrix.
    let miss = provider
        .lookup(LookupQuery::new("Big Buck Bunny", Some(2008)))
        .await
        .expect("lookup should succeed");
    assert!(miss.is_none(), "should not have matched anything");

    let _ = std::fs::remove_dir_all(cache.root());
}

#[tokio::test]
async fn artwork_caching_survives_a_restart_without_the_network() {
    let (base, _stub) = spawn_stub().await;
    let root = scratch("offline");
    let cache = Arc::new(CatalogCache::new(&root));

    // First run: downloads everything.
    let online = TmdbProvider::new(client(&base, "v3-key"), cache.clone());
    let first = online
        .lookup(LookupQuery::new("The Matrix", Some(1999)))
        .await
        .unwrap()
        .expect("matched");
    assert!(first.artwork.poster.as_ref().unwrap().is_cached());

    // Second run: the provider cannot reach the network at all, but the cached
    // lookup must still answer and still point at the images.
    let offline = TmdbProvider::new(
        TmdbClient::new("v3-key")
            .unwrap()
            .with_base_url("http://127.0.0.1:1/3")
            .with_image_base("http://127.0.0.1:1/img"),
        cache.clone(),
    );
    let seconds = offline
        .lookup(LookupQuery::new("The Matrix", Some(1999)))
        .await
        .expect("a cached lookup must not need the network")
        .expect("matched from cache");

    assert_eq!(seconds.source_id, "603");
    let poster = seconds.artwork.poster.as_ref().expect("poster");
    assert!(poster.is_cached(), "artwork should still be on disk");
    assert!(poster.local_uri().is_some());

    let _ = std::fs::remove_dir_all(&root);
}
