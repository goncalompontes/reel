//! End-to-end playback through the code the desktop app actually uses.
//!
//! `tests/backend.rs` proves the engine and the library state machine; the
//! snapshots prove the interface draws. Neither plays a frame. This one starts
//! a local seeder, has the desktop backend stream from it into temporary
//! storage, and drives the **real** `PlayerController` (libmpv decoding into
//! egui textures) through play → leave → resume.
//!
//! It is skipped when ffmpeg or libmpv is unavailable, so it does not fail a
//! machine that cannot run it.
//!
//! Run with output: `cargo test -p reel-desktop --test player_resume -- --nocapture`

use std::process::Command;
use std::time::{Duration, Instant};

use reel_core::{AddOptions, AddSource, Engine, EngineConfig};
use reel_desktop::backend::{Backend, CatalogOptions, EngineBackend};
use reel_desktop::{PlaybackInfo, PlayerCapability, PlayerController};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-resume-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn have(tool: &str, arg: &str) -> bool {
    Command::new(tool)
        .arg(arg)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn wait_for(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Drive the real player until it has produced `want` frames, or time out.
/// Returns how many frames it managed and why it stopped. The controller is
/// reused across plays, exactly as the app does (open → close → open).
fn play(
    controller: &mut PlayerController,
    ctx: &egui::Context,
    url: &str,
    start_at: Option<f64>,
    want: u64,
    timeout: Duration,
) -> (u64, String) {
    let info = PlaybackInfo {
        torrent_id: 0,
        file_id: 0,
        info_hash: "resume-test".to_string(),
        title: "resume test".to_string(),
        file_name: url.rsplit('/').next().unwrap_or("stream").to_string(),
        fallback_duration: None,
        start_at,
        subtitles: Vec::new(),
    };
    if let Err(e) = controller.open(ctx, &PlayerCapability::Embedded, url, info) {
        return (0, format!("open failed: {e}"));
    }

    let deadline = Instant::now() + timeout;
    while controller.frames_uploaded() < want {
        if Instant::now() > deadline {
            let state = controller.state();
            return (
                controller.frames_uploaded(),
                format!(
                    "timed out at {:.1}s, paused={}, error={:?}",
                    state.position, state.paused, state.error
                ),
            );
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(1280.0, 720.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| controller.video_ui(ui, ctx));
        output.textures_delta.clear();
        std::thread::sleep(Duration::from_millis(5));
    }
    (controller.frames_uploaded(), String::new())
}

#[test]
fn a_stream_plays_again_after_leaving() {
    if reel_player::embedded_available().is_err() {
        eprintln!("skipping: libmpv is not available");
        return;
    }
    if !have("ffmpeg", "-version") {
        eprintln!("skipping: ffmpeg is not available");
        return;
    }

    let dir = scratch("run");
    let video = dir.join("clip.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=30:size=640x360:rate=24",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
        ])
        .arg(&video)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to make the fixture");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let created = runtime
        .block_on(reel_core::create_torrent_file(
            &video,
            None,
            Vec::new(),
            Some(64 * 1024),
        ))
        .expect("create torrent");
    let torrent_path = dir.join("clip.torrent");
    std::fs::write(&torrent_path, &created.bytes).unwrap();

    // A seeder: the fixture is already on disk, so it is finished immediately.
    // Uploads are on for it, unlike the app.
    let seed_dir = dir.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::copy(&video, seed_dir.join("clip.mp4")).unwrap();
    let (seed_engine, seed_id) = runtime.block_on(async {
        let engine = Engine::new(EngineConfig {
            disable_dht: true,
            disable_trackers: true,
            persist_session: false,
            disable_upload: false,
            ..EngineConfig::new(&seed_dir)
        })
        .await
        .expect("seed engine");
        let outcome = engine
            .add(
                AddSource::File(created.bytes.clone()),
                AddOptions {
                    media_only: false,
                    allow_overwrite: true,
                    ..Default::default()
                },
            )
            .await
            .expect("seed add");
        (engine, outcome.torrent.id)
    });
    wait_for("the seeder to have the data", Duration::from_secs(60), || {
        seed_engine.view(seed_id).map(|v| v.finished).unwrap_or(false)
    });
    let seed_addr = seed_engine
        .session()
        .listen_addr()
        .expect("seeder listen address");

    // The leecher is the desktop backend, streaming into temporary storage.
    let leech = dir.join("leech");
    let scratch = dir.join("scratch");
    let backend = EngineBackend::start_with_options(
        EngineConfig {
            disable_dht: true,
            disable_trackers: true,
            persist_session: false,
            initial_peers: vec![seed_addr],
            stream_scratch_dir: Some(scratch.clone()),
            ..EngineConfig::new(&leech)
        },
        CatalogOptions {
            api_key: None,
            settings: Default::default(),
            data_dir: dir.join("catalog"),
            disable_bundled_sources: true,
        },
    )
    .expect("leech backend");

    backend.add(torrent_path.to_str().unwrap(), true);
    wait_for("the torrent in the library", Duration::from_secs(60), || {
        !backend.library().is_empty()
    });
    let id = backend.library().into_iter().next().unwrap().torrent.id;
    wait_for("the torrent to be live", Duration::from_secs(60), || {
        backend.is_live(id)
    });

    // First play: this is what fills the buffer.
    backend.start_files(id, &[0]);
    wait_for("the first stream to start", Duration::from_secs(60), || {
        backend
            .item(id)
            .is_some_and(|item| !item.torrent.stats.is_paused())
    });
    let _ = backend.library();
    let url = backend
        .item(id)
        .and_then(|item| item.torrent.files.first().and_then(|f| f.stream.url.clone()))
        .expect("a stream url");
    let ctx = egui::Context::default();
    let mut controller = PlayerController::new();
    let (frames, why) = play(&mut controller, &ctx, &url, None, 10, Duration::from_secs(90));
    assert!(frames >= 10, "first play produced no frames: {why}");

    // Leave: stop the player (as `leave_player` does), then the backend pauses
    // and keeps the buffer.
    controller.close();
    backend.stop_streaming(id);
    // It stays live (a stream-less torrent fetches nothing); the buffer and the
    // peers are kept.
    wait_for("the stream to be cached", Duration::from_secs(60), || {
        backend.is_live(id)
            && backend
                .item(id)
                .is_some_and(|item| item.torrent.stats.progress_bytes > 0)
    });

    // Resume: the same sequence the app performs.
    backend.start_files(id, &[0]);
    wait_for("the resumed stream to start", Duration::from_secs(60), || {
        backend
            .item(id)
            .is_some_and(|item| !item.torrent.stats.is_paused())
    });
    let url = backend
        .item(id)
        .and_then(|item| item.torrent.files.first().and_then(|f| f.stream.url.clone()))
        .expect("a stream url");
    let (frames, why) = play(
        &mut controller,
        &ctx,
        &url,
        Some(5.0),
        10,
        Duration::from_secs(60),
    );
    assert!(frames >= 10, "second play produced no frames: {why}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&scratch);
}
