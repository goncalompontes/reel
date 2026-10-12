//! End-to-end transcode: build a small file, re-encode it, and check the
//! hardware→software fallback actually lands on a playable H.264 output.
//!
//! Skipped when ffmpeg/ffprobe (or libx264) are not installed, matching the
//! convention in `probe_real.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use reel_media::{Tools, VideoEncoder};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-media-tx-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Whether the ffmpeg build has libx264 (the software encoder).
fn has_libx264(tools: &Tools) -> bool {
    let Ok(output) = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return false;
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.lines()
        .any(|line| line.split_whitespace().nth(1) == Some("libx264"))
}

/// Build a tiny MPEG-4/AAC Matroska sample (native encoders, no libx264 needed).
fn build_sample(tools: &Tools, file: &Path) {
    let status = Command::new(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-y",
        ])
        .arg(file)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to build the sample");
}

#[tokio::test]
async fn software_transcode_produces_h264() {
    let Some(tools) = Tools::try_resolve(None, None) else {
        eprintln!("skipping: ffmpeg/ffprobe are not installed");
        return;
    };
    if !has_libx264(&tools) {
        eprintln!("skipping: this ffmpeg has no libx264");
        return;
    }

    let dir = scratch("software");
    let source = dir.join("sample.mkv");
    let output = dir.join("out.mp4");
    build_sample(&tools, &source);

    let probe = tools
        .probe(&source.to_string_lossy())
        .await
        .expect("probe the sample");
    tools
        .transcode(
            &source.to_string_lossy(),
            &probe,
            480,
            &output,
            VideoEncoder::Software,
            None,
        )
        .await
        .expect("software transcode");

    assert!(output.is_file(), "the output should exist");
    let out = tools
        .probe(&output.to_string_lossy())
        .await
        .expect("probe the output");
    assert_eq!(out.video_codec(), Some("h264"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A full-file remux (used for Google Cast, which needs a real duration) copies
/// the streams into a playable MP4 without re-encoding.
#[tokio::test]
async fn remux_to_file_copies_the_streams() {
    let Some(tools) = Tools::try_resolve(None, None) else {
        eprintln!("skipping: ffmpeg/ffprobe are not installed");
        return;
    };

    let dir = scratch("remuxfile");
    let source = dir.join("sample.mkv");
    let output = dir.join("out.mp4");
    build_sample(&tools, &source);

    tools
        .remux_to_file(&source.to_string_lossy(), None, None, &output)
        .await
        .expect("remux to file");

    let out = tools
        .probe(&output.to_string_lossy())
        .await
        .expect("probe the output");
    assert_eq!(out.video_codec(), Some("mpeg4"));
    assert_eq!(out.audio_codec(), Some("aac"));
    assert!(out.is_mp4());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Requesting a hardware encoder must never leave the cast broken: when the
/// encoder cannot run (no GPU/driver, or not even compiled in), the transcode
/// falls back to software and still succeeds.
#[tokio::test]
async fn a_hardware_encoder_falls_back_to_software() {
    let Some(tools) = Tools::try_resolve(None, None) else {
        eprintln!("skipping: ffmpeg/ffprobe are not installed");
        return;
    };
    if !has_libx264(&tools) {
        eprintln!("skipping: this ffmpeg has no libx264 (no fallback available)");
        return;
    }

    let dir = scratch("fallback");
    let source = dir.join("sample.mkv");
    let output = dir.join("out.mp4");
    build_sample(&tools, &source);

    let probe = tools
        .probe(&source.to_string_lossy())
        .await
        .expect("probe the sample");

    // VA-API is the first choice on Linux; on a machine without it (or on
    // Windows/macOS) this exercises the software retry.
    tools
        .transcode(
            &source.to_string_lossy(),
            &probe,
            480,
            &output,
            VideoEncoder::Vaapi,
            None,
        )
        .await
        .expect("a hardware request should still produce output via the fallback");

    let out = tools
        .probe(&output.to_string_lossy())
        .await
        .expect("probe the output");
    assert_eq!(out.video_codec(), Some("h264"));
    let _ = std::fs::remove_dir_all(&dir);
}
