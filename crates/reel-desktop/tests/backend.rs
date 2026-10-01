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
        settings: Default::default(),
        data_dir: dir.join("catalog"),
        // Keep the test offline: the bundled source searches over the network.
        disable_bundled_sources: true,
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

/// Poll until `check` is true, or fail with `what` after a deadline.
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// A multi-file pack: playing one episode fetches one episode, and downloading
/// one episode keeps one episode — not the whole season.
#[test]
fn a_pack_streams_and_downloads_only_the_chosen_episode() {
    let dir = scratch_dir("pack");
    let pack = dir.join("pack");
    std::fs::create_dir_all(&pack).unwrap();
    for n in 1..=3u8 {
        let bytes: Vec<u8> = (0..96 * 1024).map(|i| ((i as u8).wrapping_add(n * 7)) as u8).collect();
        std::fs::write(pack.join(format!("Some.Show.S01E{n:02}.mkv")), bytes).unwrap();
    }
    let torrent_path = dir.join("pack.torrent");
    {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let created = runtime
            .block_on(reel_core::create_torrent_file(
                &pack,
                None,
                Vec::new(),
                Some(16 * 1024),
            ))
            .expect("create torrent");
        std::fs::write(&torrent_path, &created.bytes).unwrap();
    }

    let scratch = scratch_dir("pack-state");
    let config = EngineConfig {
        disable_dht: true,
        disable_trackers: true,
        persist_session: false,
        stream_scratch_dir: Some(scratch.clone()),
        ..EngineConfig::new(&scratch)
    };
    let backend = EngineBackend::start_with_options(
        config,
        CatalogOptions {
            api_key: None,
            settings: Default::default(),
            data_dir: dir.join("catalog"),
            disable_bundled_sources: true,
        },
    )
    .expect("start the engine backend");

    backend.add(torrent_path.to_str().unwrap(), true);
    wait_until("the pack to appear", || !backend.library().is_empty());
    let id = backend.library().into_iter().next().unwrap().torrent.id;
    wait_until("the pack to be live", || backend.is_live(id));

    let included = |backend: &EngineBackend| -> Vec<usize> {
        backend
            .item(id)
            .expect("the pack")
            .torrent
            .files
            .iter()
            .filter(|file| file.included)
            .map(|file| file.id)
            .collect()
    };

    // Added as a stream: paused, nothing is being fetched yet.
    assert!(
        backend.item(id).expect("the pack").torrent.stats.is_paused(),
        "a stream pack waits to be told what to play"
    );
    assert!(!backend.item(id).expect("the pack").downloading);

    // Play episode 2: it resumes, and fetches only episode 2.
    backend.start_files(id, &[1]);
    wait_until("episode 2 to start", || {
        !backend.item(id).expect("the pack").torrent.stats.is_paused()
    });
    assert_eq!(included(&backend), vec![1], "only the played episode is fetched");

    // Download episode 2: only it is kept, not the whole pack.
    backend.download_files(id, &[1]);
    wait_until("the download to be marked", || {
        backend.item(id).expect("the pack").downloading
    });
    wait_until("the download to run", || {
        backend.is_live(id) && !backend.item(id).expect("the pack").torrent.stats.is_paused()
    });
    assert_eq!(
        included(&backend),
        vec![1],
        "downloading one episode must not fetch the whole pack"
    );

    // Stop it again in one action.
    backend.stop_download(id);
    wait_until("the download to stop", || {
        !backend.item(id).expect("the pack").downloading
    });

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The streaming lifecycle against the real engine, with no swarm: a title is
/// added, is temporary by default, can be kept as a download, and is released
/// again so its temporary storage goes away.
#[test]
fn streaming_is_temporary_and_downloading_is_opt_in() {
    use std::time::{Duration, Instant};

    let dir = scratch_dir("lifecycle");
    std::fs::create_dir_all(dir.join("payload")).unwrap();
    // A small non-video file: enough to make a torrent without a codec.
    let payload = dir.join("payload/data.bin");
    let bytes: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(&payload, &bytes).unwrap();

    let torrent_path = dir.join("data.torrent");
    {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let created = runtime
            .block_on(reel_core::create_torrent_file(
                &dir.join("payload"),
                None,
                Vec::new(),
                Some(16 * 1024),
            ))
            .expect("create torrent");
        std::fs::write(&torrent_path, &created.bytes).unwrap();
    }

    let scratch = scratch_dir("lifecycle-state");
    let config = EngineConfig {
        disable_dht: true,
        disable_trackers: true,
        persist_session: false,
        stream_scratch_dir: Some(scratch.clone()),
        ..EngineConfig::new(&scratch)
    };
    let backend = EngineBackend::start_with_options(
        config,
        CatalogOptions {
            api_key: None,
            settings: Default::default(),
            data_dir: dir.join("catalog"),
            disable_bundled_sources: true,
        },
    )
    .expect("start the engine backend");

    let wait = |what: &str, mut check: Box<dyn FnMut() -> bool + '_>| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    };

    // Add it: by default it is a temporary stream, not a download.
    backend.add(torrent_path.to_str().unwrap(), true);
    wait("the torrent to appear in the library", Box::new(|| !backend.library().is_empty()));

    let item = backend.library().into_iter().next().unwrap();
    let id = item.torrent.id;
    assert!(!item.downloading, "a new title streams by default");
    assert!(!item.torrent.files.is_empty(), "the file list was captured");
    wait("the torrent to be live", Box::new(|| backend.is_live(id)));

    // Keep it: this switches to filesystem storage and marks it a download.
    backend.download_files(id, &[0]);
    wait("the download to be marked", Box::new(|| {
        backend.item(id).is_some_and(|item| item.downloading)
    }));

    // Stop keeping it: files are deleted and it goes back to temporary.
    backend.stop_download(id);
    wait("the download to be cleared", Box::new(|| {
        backend.item(id).is_some_and(|item| !item.downloading)
    }));

    // The title is still in the library even though the torrent was recycled.
    assert!(backend.item(id).is_some(), "the library outlives the torrent");

    // Stream, release, and stream again. The second play is what used to get
    // stuck: the torrent has to be brought back from the saved .torrent first.
    backend.start_files(id, &[0]);
    wait("the first stream to start", Box::new(|| backend.is_live(id)));

    backend.stop_streaming(id);
    wait("the stream to be cached", Box::new(|| {
        backend.is_live(id)
            && backend
                .item(id)
                .is_some_and(|item| item.torrent.stats.is_paused())
    }));

    assert!(
        backend.item(id).is_some(),
        "releasing a stream must not remove the title from the library"
    );

    backend.start_files(id, &[0]);
    wait("the second stream to start", Box::new(|| backend.is_live(id)));
    let selected: Vec<usize> = backend
        .item(id)
        .expect("the title")
        .torrent
        .files
        .iter()
        .filter(|file| file.included)
        .map(|file| file.id)
        .collect();
    assert_eq!(selected, vec![0], "the played file is selected again");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&scratch);
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

/// The real wiring: the app's backend, the real bundled source, a real network.
///
/// Marked `#[ignore]` because it needs the network. It covers what the UI test
/// with the fake cannot: that the search is spawned onto the runtime correctly
/// and that results come back as an event.
#[test]
#[ignore = "requires network access"]
fn the_bundled_source_searches_through_the_app_backend() {
    use reel_desktop::backend::BackendEvent;

    let dir = scratch_dir("search");
    let config = EngineConfig {
        disable_dht: true,
        disable_trackers: true,
        persist_session: false,
        ..EngineConfig::new(&dir)
    };
    let backend = EngineBackend::start_with_options(
        config,
        CatalogOptions {
            api_key: None,
            settings: Default::default(),
            data_dir: dir.join("catalog"),
            disable_bundled_sources: false,
        },
    )
    .expect("start the engine backend");

    assert_eq!(
        backend.search_sources().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["archive.org"],
        "the bundled source should be registered"
    );

    backend.search("nosferatu");

    // The search runs on the runtime and reports back through events.
    let mut hits = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    while std::time::Instant::now() < deadline {
        for event in backend.take_events() {
            match event {
                BackendEvent::SearchResults { results, .. } => {
                    assert!(results.failures.is_empty(), "{:?}", results.failures);
                    hits = Some(results.hits);
                }
                BackendEvent::SearchFailed { message, .. } => panic!("search failed: {message}"),
                _ => {}
            }
        }
        if hits.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let hits = hits.expect("the search should answer within 45s");
    assert!(!hits.is_empty(), "expected results for a well-known film");
    assert!(hits.iter().all(|hit| hit.is_usable()));

    println!("top hit: {} ({:?})", hits[0].title, hits[0].year);
    println!("torrent: {}", hits[0].torrent_url.as_deref().unwrap_or(""));

    let _ = std::fs::remove_dir_all(&dir);
}
