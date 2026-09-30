//! Integration test for the seam the whole app depends on: the real engine
//! plus the in-process streaming HTTP server, on a real localhost socket.
//!
//! Deliberately uses a hand-written HTTP request over a `TcpStream` so that the
//! test has no HTTP client dependency and provokes the server exactly as the
//! video player would.

use std::io::{Read, Write};
use std::net::TcpStream;

use reel_core::EngineConfig;
use reel_desktop::backend::{Backend, CatalogOptions, EngineBackend};

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn get(host: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(host).expect("connect to the API");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

#[test]
fn engine_backend_serves_its_api_in_process() {
    let dir = scratch_dir("api");
    let config = EngineConfig {
        // Keep the test hermetic: no DHT, no tracker traffic, no state on disk
        // beyond the download directory.
        disable_dht: true,
        disable_trackers: true,
        persist_session: false,
        ..EngineConfig::new(&dir)
    };

    // Point the catalog at a scratch directory and configure no provider, so
    // the test never reaches the network or the real cache.
    let catalog = CatalogOptions {
        api_key: None,
        data_dir: dir.join("catalog"),
    };
    let backend =
        EngineBackend::start_with_options(config, catalog).expect("start the engine backend");

    let base = backend.base_url().to_string();
    assert!(
        base.starts_with("http://127.0.0.1:"),
        "the API must bind to an ephemeral localhost port, got {base}"
    );
    let host = base.trim_start_matches("http://");

    // An empty library is the expected starting state.
    assert!(backend.library().is_empty());

    let health = get(host, "/api/health");
    assert!(
        health.starts_with("HTTP/1.1 200"),
        "health check failed:\n{health}"
    );
    assert!(
        health.contains("\"status\":\"ok\""),
        "health body unexpected:\n{health}"
    );
    assert!(
        health.contains(&base),
        "health should report the API's own base URL"
    );

    let torrents = get(host, "/api/torrents");
    assert!(torrents.starts_with("HTTP/1.1 200"));
    assert!(
        torrents.trim_end().ends_with("[]"),
        "an empty library should serialise as an empty array:\n{torrents}"
    );

    // Unknown ids must 404 rather than error out.
    let missing = get(host, "/api/torrents/4242");
    assert!(
        missing.starts_with("HTTP/1.1 404"),
        "unknown torrent should 404:\n{missing}"
    );

    // The capabilities tell the UI how video will be played.
    let capabilities = backend.capabilities();
    assert!(!capabilities.download_dir.is_empty());
    assert!(!capabilities.client_name.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fake_backend_drives_the_ui_without_an_engine() {
    use reel_desktop::backend::FakeBackend;
    use reel_desktop::testing::sample_library;

    let backend = FakeBackend::new(sample_library());
    assert_eq!(backend.library().len(), 3);
    assert!(backend.item(1).is_some());
    assert!(backend.item(999).is_none());

    // The fake reports a catalog that is configured, so settings render.
    assert!(backend.catalog_status().configured);

    // The fake must never advertise embedded playback, or a test could try to
    // open a real video device.
    assert!(matches!(
        backend.capabilities().player,
        reel_desktop::PlayerCapability::Unavailable { .. }
    ));
}
