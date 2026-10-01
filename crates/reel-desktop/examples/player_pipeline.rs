//! Drives the desktop app's video surface headlessly against a real URL.
//!
//! This closes the last gap in the playback chain. `dump_frame` proves libmpv
//! can decode the stream, and the UI snapshots prove the interface renders —
//! but neither proves that a decoded frame reaches egui as a texture through
//! the code the desktop app actually uses.
//!
//! It runs real egui passes with `Context::run_ui`, so no window or display
//! server is needed, and reports machine-readable `key=value` lines.
//!
//! Usage: player_pipeline <url> [--frames N] [--timeout SECONDS] [--size WxH]

use std::time::{Duration, Instant};

use reel_desktop::{PlaybackInfo, PlayerCapability, PlayerController};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut url = String::new();
    let mut want_frames: u64 = 10;
    let mut timeout = Duration::from_secs(60);
    let mut size = (1280.0_f32, 720.0_f32);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frames" => {
                want_frames = args.next().and_then(|v| v.parse().ok()).unwrap_or(10);
            }
            "--timeout" => {
                let secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(60.0);
                timeout = Duration::from_secs_f64(secs);
            }
            "--size" => {
                if let Some(value) = args.next() {
                    if let Some((w, h)) = value.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                            size = (w, h);
                        }
                    }
                }
            }
            other => url = other.to_string(),
        }
    }

    if url.is_empty() {
        println!("error=missing url");
        std::process::exit(2);
    }

    let capability = match reel_player::embedded_available() {
        Ok(()) => PlayerCapability::Embedded,
        Err(e) => {
            println!("backend=unavailable");
            println!("error={e}");
            std::process::exit(1);
        }
    };

    // A real egui context, driven by hand instead of by a window.
    let ctx = egui::Context::default();
    let mut controller = PlayerController::new();

    let info = PlaybackInfo {
        torrent_id: 0,
        file_id: 0,
        info_hash: "pipeline-test".to_string(),
        title: "pipeline test".to_string(),
        file_name: url.rsplit('/').next().unwrap_or("stream").to_string(),
        fallback_duration: None,
        start_at: None,
    };

    if let Err(e) = controller.open(&ctx, &capability, &url, info) {
        println!("error={e}");
        std::process::exit(1);
    }

    let started = Instant::now();
    let deadline = started + timeout;
    let mut textures_uploaded = 0usize;
    let mut last_texture_size = (0usize, 0usize);

    while controller.frames_uploaded() < want_frames {
        if Instant::now() > deadline {
            println!("backend=embedded");
            println!("error=timed out");
            println!("frames_uploaded={}", controller.frames_uploaded());
            println!("textures_uploaded={textures_uploaded}");
            let state = controller.state();
            println!("last_position={:.3}", state.position);
            println!("last_error={}", state.error.clone().unwrap_or_default());
            std::process::exit(1);
        }

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(size.0, size.1),
            )),
            ..Default::default()
        };

        let mut output = ctx.run_ui(input, |ui| {
            controller.video_ui(ui, &ctx);
        });

        // Every uploaded image is a frame that made it all the way to egui.
        for deltas in output.textures_delta.set.values() {
            for delta in deltas {
                textures_uploaded += 1;
                last_texture_size = (delta.image.width(), delta.image.height());
            }
        }

        // epaint panics if a TexturesDelta is dropped with unapplied deltas.
        // There is no renderer here, so the deltas are consumed and discarded.
        output.textures_delta.clear();

        std::thread::sleep(Duration::from_millis(5));
    }

    let state = controller.state();
    println!("backend=embedded");
    println!("frames_uploaded={}", controller.frames_uploaded());
    println!("textures_uploaded={textures_uploaded}");
    println!("texture_size={}x{}", last_texture_size.0, last_texture_size.1);
    println!("surface_size={}x{}", size.0 as u32, size.1 as u32);
    println!(
        "video_size={}x{}",
        state.video_width.unwrap_or(0),
        state.video_height.unwrap_or(0)
    );
    println!("opaque_and_sized={}", last_texture_size.1 > 0);
    println!("elapsed_ms={}", started.elapsed().as_millis());

    if textures_uploaded == 0 || last_texture_size.1 == 0 {
        println!("error=no frame reached egui as a texture");
        std::process::exit(1);
    }

    std::process::exit(0);
}
