//! Headless frame dumper: proves the whole native playback chain works
//! (torrent -> HTTP range stream -> libmpv decode -> RGBA pixels) without
//! needing a display server.
//!
//! Usage:
//!   dump_frame <url> [--out frame.ppm] [--seek SECONDS] [--frames N]
//!              [--width W] [--height H] [--timeout SECONDS] [-v]
//!
//! Prints machine-readable `key=value` lines so scripts can assert on them.
//!
//! Every exit path goes through `run`, so the `Player` is dropped and mpv is
//! torn down on its own thread before the process leaves. Calling
//! `process::exit` with a live mpv would tear down its callback threads
//! mid-flight and can segfault.

use std::io::Write;
use std::time::{Duration, Instant};

use reel_player::{Backend, Player, PlayerConfig};

struct Args {
    url: String,
    out: Option<String>,
    seek: Option<f64>,
    frames: usize,
    width: u32,
    height: u32,
    timeout: Duration,
    verbose: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        url: String::new(),
        out: None,
        seek: None,
        frames: 1,
        width: 640,
        height: 360,
        timeout: Duration::from_secs(60),
        verbose: false,
    };

    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => parsed.out = Some(value()?),
            "--seek" => parsed.seek = Some(value()?.parse().map_err(|e| format!("--seek: {e}"))?),
            "--frames" => parsed.frames = value()?.parse().map_err(|e| format!("--frames: {e}"))?,
            "--width" => parsed.width = value()?.parse().map_err(|e| format!("--width: {e}"))?,
            "--height" => parsed.height = value()?.parse().map_err(|e| format!("--height: {e}"))?,
            "--timeout" => {
                parsed.timeout =
                    Duration::from_secs_f64(value()?.parse().map_err(|e| format!("--timeout: {e}"))?)
            }
            "--verbose" | "-v" => parsed.verbose = true,
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => parsed.url = other.to_string(),
        }
    }

    if parsed.url.is_empty() {
        return Err("missing <url>".to_string());
    }
    Ok(parsed)
}

fn init_tracing(verbose: bool) {
    let filter = if verbose {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("reel_player=debug"))
    } else {
        tracing_subscriber::EnvFilter::new("reel_player=warn")
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("dump_frame: {e}");
            eprintln!("usage: dump_frame <url> [--out frame.ppm] [--seek SECONDS] [--frames N]");
            std::process::exit(2);
        }
    };

    init_tracing(args.verbose);
    let code = run(&args);
    std::process::exit(code);
}

fn run(args: &Args) -> i32 {
    // Embedded only: the external backend cannot hand back pixels.
    let config = PlayerConfig {
        prefer_embedded: true,
        no_audio: true,
        mute: true,
        start_position: args.seek,
        ..Default::default()
    };

    let player = match Player::new(config) {
        Ok(player) => player,
        Err(e) => {
            println!("backend=unavailable");
            println!("error={e}");
            return 1;
        }
    };

    println!("backend={}", backend_name(player.backend()));
    if let Some(reason) = player.unavailable_reason() {
        println!("fallback_reason={reason}");
    }
    if let Some((major, minor)) = reel_player::mpv_api_version() {
        println!("mpv_api={major}.{minor}");
    }
    if player.backend() != Backend::Embedded {
        println!("error=embedded backend unavailable, cannot dump frames");
        return 1;
    }

    player.set_target_size(Some((args.width, args.height)));

    let started = Instant::now();
    if let Err(e) = player.load(&args.url) {
        println!("error={e}");
        return 1;
    }

    let deadline = Instant::now() + args.timeout;
    let mut sequence = 0u64;
    let mut rendered = 0usize;
    let mut last_snapshot = None;

    while rendered < args.frames {
        if Instant::now() > deadline {
            report_stall(&player, rendered, sequence);
            return 1;
        }

        match player.frame_after(sequence) {
            Some(snapshot) => {
                sequence = snapshot.sequence;
                rendered += 1;
                if rendered == 1 {
                    println!("first_frame_ms={}", started.elapsed().as_millis());
                }
                if args.verbose && rendered % 10 == 0 {
                    let state = player.state();
                    eprintln!(
                        "[{rendered}] seq={} pos={:.3} paused={} idle={} buf={:?}",
                        snapshot.sequence, state.position, state.paused, state.idle, state.buffering_percent
                    );
                }
                if rendered == args.frames {
                    last_snapshot = Some(snapshot);
                }
            }
            None => {
                if args.verbose && rendered == 0 {
                    let state = player.state();
                    eprintln!(
                        "[waiting] pos={:.3} paused={} idle={} eof={} cache={}",
                        state.position, state.paused, state.idle, state.eof, state.paused_for_cache
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    let state = player.state();
    let snapshot = last_snapshot.expect("a snapshot when frames == rendered");
    let frame = &snapshot.frame;
    let stats = FrameStats::of(&frame.rgba);

    println!("frame_sequence={}", snapshot.sequence);
    println!("frame_size={}x{}", frame.width, frame.height);
    println!("frames_rendered={rendered}");
    println!("elapsed_ms={}", started.elapsed().as_millis());
    println!(
        "sustained_fps={:.1}",
        rendered as f64 / started.elapsed().as_secs_f64()
    );
    println!("mean_luma={:.2}", stats.mean_luma);
    println!("peak_luma={}", stats.peak_luma);
    println!("nonblack_percent={:.2}", stats.nonblack_percent);
    println!("opaque={}", stats.fully_opaque);
    println!("state_loaded={}", state.loaded);
    println!("state_error={}", state.error.clone().unwrap_or_default());
    println!(
        "video_size={}x{}",
        state.video_width.unwrap_or(0),
        state.video_height.unwrap_or(0)
    );
    if let Some(duration) = state.duration {
        println!("duration={duration:.3}");
    }
    println!("position={:.3}", state.position);

    if let Some(path) = args.out.as_deref() {
        if let Err(e) = write_ppm(path, frame.width, frame.height, &frame.rgba) {
            println!("error=writing {path}: {e}");
            return 1;
        }
        println!("wrote={path}");
    }

    // A blank frame means we decoded nothing useful.
    if stats.nonblack_percent < 1.0 {
        println!("error=frame is essentially blank");
        return 1;
    }

    0
}

/// Explain why no frames arrived; far more useful than a bare timeout.
fn report_stall(player: &Player, rendered: usize, sequence: u64) {
    let state = player.state();
    println!("error=timed out waiting for frames");
    println!("frames_rendered={rendered}");
    println!("frame_sequence={sequence}");
    println!("last_position={:.3}", state.position);
    println!("last_paused={}", state.paused);
    println!("last_idle={}", state.idle);
    println!("last_eof={}", state.eof);
    println!("last_paused_for_cache={}", state.paused_for_cache);
    println!(
        "last_buffering={}",
        state
            .buffering_percent
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
    println!("last_loaded={}", state.loaded);
    println!("last_error={}", state.error.clone().unwrap_or_default());
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Embedded => "embedded",
        Backend::External => "external",
        Backend::Unavailable => "unavailable",
    }
}

struct FrameStats {
    mean_luma: f64,
    peak_luma: u8,
    nonblack_percent: f64,
    fully_opaque: bool,
}

impl FrameStats {
    fn of(rgba: &[u8]) -> Self {
        let pixels = rgba.len() / 4;
        if pixels == 0 {
            return Self {
                mean_luma: 0.0,
                peak_luma: 0,
                nonblack_percent: 0.0,
                fully_opaque: false,
            };
        }

        let mut sum: u64 = 0;
        let mut peak: u8 = 0;
        let mut nonblack: u64 = 0;
        let mut opaque = true;

        for pixel in rgba.chunks_exact(4) {
            let luma = (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3;
            sum += luma as u64;
            peak = peak.max(luma as u8);
            if luma > 8 {
                nonblack += 1;
            }
            if pixel[3] != 255 {
                opaque = false;
            }
        }

        Self {
            mean_luma: sum as f64 / pixels as f64,
            peak_luma: peak,
            nonblack_percent: nonblack as f64 * 100.0 / pixels as f64,
            fully_opaque: opaque,
        }
    }
}

/// Write a binary PPM (P6). No image-encoding dependency, and `ffmpeg` can
/// convert it if a human wants to look.
fn write_ppm(path: &str, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(file, "P6\n{width} {height}\n255\n")?;
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    file.write_all(&rgb)?;
    file.flush()
}
