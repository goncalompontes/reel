//! ffmpeg/ffprobe argument construction.
//!
//! Kept as pure functions returning argument vectors so the command lines can
//! be asserted on without spawning anything.
//!
//! Inputs are strings, not paths: ffmpeg and ffprobe accept URLs, and a
//! torrent's bytes are served over HTTP (its on-disk file is incomplete while
//! it downloads), so the loopback stream URL is the natural input.

use std::path::Path;

use crate::probe::Probe;

/// Arguments for `ffprobe`, which writes the JSON [`Probe::from_json`] reads.
pub fn probe_args(input: &str) -> Vec<String> {
    vec![
        "-v".into(),
        "error".into(),
        "-print_format".into(),
        "json".into(),
        "-show_format".into(),
        "-show_streams".into(),
        input.to_string(),
    ]
}

/// Arguments for a live remux: copy every stream into a fragmented MP4 on
/// stdout, which starts playing immediately without seeking to the end.
///
/// `video_tag` sets the MP4 sample-entry tag for the video stream. HEVC must be
/// `hvc1`, not ffmpeg's default `hev1`, or televisions reject it as an unknown
/// format. It is `None` for other codecs (an incompatible tag errors).
///
/// Seek accuracy is limited (only bytes produced so far are available), which
/// is why the cache tees this to a file for later range requests.
pub fn remux_stream_args(input: &str, video_tag: Option<&str>) -> Vec<String> {
    let mut args = common_args(input);
    args.extend(["-c", "copy"].map(String::from));
    if let Some(tag) = video_tag {
        args.push("-tag:v".to_string());
        args.push(tag.to_string());
    }
    args.extend(
        [
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-f",
            "mp4",
            "pipe:1",
        ]
        .map(String::from),
    );
    args
}

/// Arguments for a full conversion into a seekable, faststart MP4 file.
///
/// Video is copied when it is already 8-bit H.264 at or below the cap;
/// otherwise it is re-encoded. Audio is copied when a TV will decode it
/// (`aac`/`mp3`) and re-encoded to AAC otherwise. A pure remux is just this
/// with every stream copyable.
pub fn transcode_args(input: &str, probe: &Probe, max_height: u32, output: &Path) -> Vec<String> {
    let mut args = common_args(input);
    args.extend(video_args(probe, max_height));
    args.extend(audio_args(probe));
    args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    args.push(output.to_string_lossy().into_owned());
    args
}

fn common_args(input: &str) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.to_string(),
        // `?` makes the stream optional, so audio-only files still work.
        "-map".into(),
        "0:v:0?".into(),
        "-map".into(),
        "0:a:0?".into(),
        // Subtitles are never copied while casting.
        "-sn".into(),
    ]
}

fn video_args(probe: &Probe, max_height: u32) -> Vec<String> {
    let Some(video) = &probe.video else {
        return Vec::new();
    };
    let copy = probe.is_h264_8bit() && video.height.is_none_or(|height| height <= max_height);
    if copy {
        return vec!["-c:v".into(), "copy".into()];
    }
    vec![
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "21".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        // Never upscale; `-2` keeps the width even.
        "-vf".into(),
        format!("scale=-2:min(ih\\,{})", max_height.max(1)),
    ]
}

fn audio_args(probe: &Probe) -> Vec<String> {
    let Some(audio) = &probe.audio else {
        return Vec::new();
    };
    if matches!(audio.codec.as_str(), "aac" | "mp3") {
        vec!["-c:a".into(), "copy".into()]
    } else {
        vec!["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Audio, Video};

    const INPUT: &str = "http://127.0.0.1:3030/stream/1/0";

    fn probe(video: Option<Video>, audio: Option<Audio>) -> Probe {
        Probe {
            format_name: Some("matroska,webm".into()),
            duration: None,
            size: None,
            video,
            audio,
            subtitles: 0,
        }
    }

    fn h264() -> Video {
        Video {
            codec: "h264".into(),
            profile: None,
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
        }
    }

    #[test]
    fn probe_asks_for_json() {
        let args = probe_args(INPUT);
        assert_eq!(&args[..2], ["-v", "error"]);
        assert!(args.contains(&"json".to_string()));
        assert_eq!(args.last().unwrap(), INPUT);
    }

    #[test]
    fn remux_copies_everything_to_a_fragmented_stream() {
        let args = remux_stream_args(INPUT, None);
        let joined = args.join(" ");
        assert!(joined.contains("-c copy"));
        assert!(joined.contains("frag_keyframe"));
        assert_eq!(args.last().unwrap(), "pipe:1");
        assert!(joined.contains("-sn"));
        assert!(!joined.contains("-tag:v"));
    }

    #[test]
    fn remux_tags_hevc_as_hvc1() {
        let joined = remux_stream_args(INPUT, Some("hvc1")).join(" ");
        assert!(joined.contains("-tag:v hvc1"));
    }

    #[test]
    fn transcode_copies_compatible_h264_and_aac() {
        let args = transcode_args(
            INPUT,
            &probe(
                Some(h264()),
                Some(Audio {
                    codec: "aac".into(),
                    channels: Some(2),
                }),
            ),
            1080,
            Path::new("/out.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"));
        assert!(!joined.contains("libx264"));
        assert!(joined.contains("-c:a copy"));
        assert!(joined.contains("+faststart"));
        assert_eq!(args.last().unwrap(), "/out.mp4");
    }

    #[test]
    fn transcode_reencodes_hevc_and_scales_to_the_cap() {
        let hevc = Video {
            codec: "hevc".into(),
            profile: None,
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
        };
        let args = transcode_args(
            INPUT,
            &probe(
                Some(hevc),
                Some(Audio {
                    codec: "dts".into(),
                    channels: Some(6),
                }),
            ),
            720,
            Path::new("/out.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("libx264"));
        assert!(joined.contains("scale=-2:min(ih\\,720)"));
        assert!(joined.contains("-c:a aac"));
    }

    #[test]
    fn transcode_only_reencodes_the_audio_when_video_is_fine() {
        let args = transcode_args(
            INPUT,
            &probe(
                Some(h264()),
                Some(Audio {
                    codec: "dts".into(),
                    channels: Some(6),
                }),
            ),
            1080,
            Path::new("/out.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-c:a aac"));
    }
}
