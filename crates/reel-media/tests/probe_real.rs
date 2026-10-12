//! End-to-end probe: generate a small file with ffmpeg, read it back with
//! ffprobe, and check the casting plan.
//!
//! Skipped when ffmpeg/ffprobe are not installed, so it does not fail on a bare
//! machine (the same convention the rest of the workspace uses for optional
//! runtime tools).

use std::path::PathBuf;
use std::process::Command;

use reel_media::{MediaPlan, PlanOptions, Tools, plan};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-media-it-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn probes_a_generated_matroska_file() {
    let Some(tools) = Tools::try_resolve(None, None) else {
        eprintln!("skipping: ffmpeg/ffprobe are not installed");
        return;
    };

    let dir = scratch("probe");
    let file = dir.join("sample.mkv");

    // `mpeg4` + `aac` are native encoders, present in every ffmpeg build, so
    // this does not depend on libx264 being compiled in.
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
        .arg(&file)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to build the sample");

    let probe = tools
        .probe(&file.to_string_lossy())
        .await
        .expect("probe the sample");
    assert_eq!(probe.video_codec(), Some("mpeg4"));
    assert_eq!(probe.audio_codec(), Some("aac"));
    assert!(probe.is_matroska());
    assert!(probe.size.unwrap_or(0) > 0);

    // MPEG-4 video is not on the copy list, so Auto plans a transcode.
    assert!(matches!(
        plan(&probe, &PlanOptions::default()),
        MediaPlan::Transcode { .. }
    ));

    let _ = std::fs::remove_dir_all(&dir);
}
