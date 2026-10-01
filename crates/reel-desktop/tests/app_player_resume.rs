//! The real desktop app, over the real engine and the real player, driven
//! headlessly: click Play, watch, go back, play again.
//!
//! This is the layer the smaller tests cannot reach: `App::play_file` defers to
//! the backend's `Ready` event, and leaving goes through `leave_player`. The
//! player fills the session buffer, so the second play must come from it.
//!
//! Skipped when ffmpeg or libmpv is unavailable.

use std::process::Command;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use reel_core::{AddOptions, AddSource, Engine, EngineConfig};
use reel_desktop::backend::{Backend, CatalogOptions, EngineBackend};
use reel_desktop::{App, Screen};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-app-resume-{}-{name}", std::process::id()));
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

/// Run frames until the player has decoded at least `want` of them.
fn drive_until_frames(harness: &mut Harness<'static, App>, want: u64, timeout: Duration) -> (u64, String) {
    let deadline = Instant::now() + timeout;
    while harness.state().frames_uploaded() < want {
        if Instant::now() > deadline {
            return (
                harness.state().frames_uploaded(),
                format!(
                    "screen={:?} player_error={:?}",
                    harness.state().screen(),
                    harness.state().player_error()
                ),
            );
        }
        harness.run_steps(1);
        std::thread::sleep(Duration::from_millis(5));
    }
    (harness.state().frames_uploaded(), String::new())
}

#[test]
fn play_watch_go_back_and_play_again() {
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
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", "testsrc=duration=30:size=640x360:rate=24",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-movflags", "+faststart",
        ])
        .arg(&video)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed");

    // A two-episode pack, the shape the app actually sees for a season.
    let seed_dir = dir.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::copy(&video, seed_dir.join("Show.S01E01.mp4")).unwrap();
    std::fs::copy(&video, seed_dir.join("Show.S01E02.mp4")).unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let created = runtime
        .block_on(reel_core::create_torrent_file(&seed_dir, None, Vec::new(), Some(64 * 1024)))
        .expect("create torrent");
    let torrent_path = dir.join("clip.torrent");
    std::fs::write(&torrent_path, &created.bytes).unwrap();
    let (seed_engine, seed_id) = runtime.block_on(async {
        let engine = Engine::new(EngineConfig {
            disable_dht: true,
            disable_trackers: true,
            persist_session: false,
            disable_upload: false,
            // The torrent root is the `seed` directory, so the session writes
            // `seed/Show.S01E01.mp4` under the parent.
            ..EngineConfig::new(&dir)
        })
        .await
        .expect("seed engine");
        let outcome = engine
            .add(
                AddSource::File(created.bytes.clone()),
                AddOptions { media_only: false, allow_overwrite: true, ..Default::default() },
            )
            .await
            .expect("seed add");
        (engine, outcome.torrent.id)
    });
    wait_for("the seeder to have the data", Duration::from_secs(60), || {
        seed_engine.view(seed_id).map(|v| v.finished).unwrap_or(false)
    });
    let seed_addr = seed_engine.session().listen_addr().expect("seed addr");

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

    let mut harness = Harness::builder()
        .with_size((1280.0, 800.0))
        .build_ui_state(
            |ui, app: &mut App| {
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(backend)),
        );

    // Wait for the title to appear, then open it.
    wait_for("the title in the library", Duration::from_secs(60), || {
        harness.state().library_len() > 0
    });
    let id = harness
        .state()
        .works()
        .first()
        .map(|work| work.lead_torrent_id())
        .expect("a work");
    harness.state_mut().navigate(Screen::Detail(id));
    harness.run_steps(3);

    // First play.
    {
        harness
            .query_all_by_label_contains("Play")
            .next()
            .expect("a Play button")
            .click();
    }
    // Long enough that a watch position is recorded, so the second play really
    // resumes rather than starting over.
    let (frames, why) = drive_until_frames(&mut harness, 90, Duration::from_secs(90));
    assert!(frames >= 90, "first play produced no frames: {why}");
    assert_eq!(harness.state().screen(), &Screen::Player);

    // Leave, as the Back button does.
    {
        harness
            .query_all_by_label_contains("Back")
            .next()
            .expect("a Back button")
            .click();
    }
    harness.run_steps(3);
    assert_ne!(harness.state().screen(), &Screen::Player);

    // Give the backend a moment to pause and cache the stream.
    std::thread::sleep(Duration::from_millis(500));
    harness.run_steps(3);

    // Second play: after leaving there is a watch position, so the button is
    // "Resume"; fall back to "Play" if it is not.
    let clicked = {
        if let Some(button) = harness.query_all_by_label_contains("Resume").next() {
            button.click();
            true
        } else if let Some(button) = harness.query_all_by_label_contains("Play").next() {
            button.click();
            true
        } else {
            false
        }
    };
    assert!(clicked, "no Resume/Play button on the detail page");
    let (frames, why) = drive_until_frames(&mut harness, 10, Duration::from_secs(60));
    assert!(frames >= 10, "second play produced no frames: {why}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&scratch);
}
