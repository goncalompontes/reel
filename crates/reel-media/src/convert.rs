//! ffmpeg/ffprobe argument construction.
//!
//! Kept as pure functions returning argument vectors so the command lines can
//! be asserted on without spawning anything.
//!
//! Inputs are strings, not paths: ffmpeg and ffprobe accept URLs, and a
//! torrent's bytes are served over HTTP (its on-disk file is incomplete while
//! it downloads), so the loopback stream URL is the natural input.

use std::path::Path;

use crate::encoders::VideoEncoder;
use crate::probe::{AudioTrack, Probe};

/// The VA-API render node ffmpeg is pointed at. This is the conventional first
/// render device; a machine with several would need an override (future work).
pub const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

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

/// Arguments to extract one subtitle stream to a standalone `.srt`.
///
/// A renderer that will not read subtitles out of the container can still fetch
/// them as a sidecar file; this turns an embedded track into one.
pub fn extract_subtitle_args(input: &str, stream_index: u32, output: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input.to_string(),
        "-map".into(),
        format!("0:{stream_index}"),
        "-c:s".into(),
        "srt".into(),
        output.to_string_lossy().into_owned(),
    ]
}

/// Convert SubRip text to WebVTT.
///
/// Browser-based receivers (Google Cast's web receiver, a `<video>` element)
/// want WebVTT, while DLNA renderers read SubRip. The two differ only in the
/// `WEBVTT` header and the decimal separator of the timestamps, so this is a
/// text transform, not a re-encode. The SRT index lines are valid WebVTT cue
/// identifiers and are kept.
pub fn srt_to_vtt(srt: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for line in srt.replace("\r\n", "\n").lines() {
        if line.contains("-->") {
            // Only the timestamp line uses `,` for the fractional part.
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
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
pub fn remux_stream_args(
    input: &str,
    video_tag: Option<&str>,
    audio_index: Option<u32>,
) -> Vec<String> {
    // A remux copies every stream, so it has no encoder and no input hwaccel.
    let mut args = common_args(input, VideoEncoder::Software, audio_index);
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

/// Arguments for a full-file remux: copy every stream into a seekable,
/// faststart MP4 on disk.
///
/// Unlike [`remux_stream_args`] (a live fragmented stream), the output carries a
/// real duration and can be range-seeked, which a Google Cast receiver needs —
/// it otherwise reports the media as only as long as what has been streamed.
pub fn remux_file_args(
    input: &str,
    video_tag: Option<&str>,
    audio_index: Option<u32>,
    output: &Path,
) -> Vec<String> {
    let mut args = vec!["-y".to_string()];
    args.extend(common_args(input, VideoEncoder::Software, audio_index));
    args.extend(["-c".to_string(), "copy".to_string()]);
    if let Some(tag) = video_tag {
        args.push("-tag:v".to_string());
        args.push(tag.to_string());
    }
    args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    args.push(output.to_string_lossy().into_owned());
    args
}

/// Arguments for a full conversion into a seekable, faststart MP4 file.
///
/// Video is copied when it is already 8-bit H.264 at or below the cap;
/// otherwise it is re-encoded with `encoder`. Audio is copied when a TV will
/// decode it (`aac`/`mp3`) and re-encoded to AAC otherwise. A pure remux is just
/// this with every stream copyable.
pub fn transcode_args(
    input: &str,
    probe: &Probe,
    max_height: u32,
    output: &Path,
    encoder: VideoEncoder,
    audio: Option<&AudioTrack>,
) -> Vec<String> {
    // `-y` because a failed hardware attempt can leave a partial file behind
    // that the software retry must overwrite.
    let mut args = vec!["-y".to_string()];
    args.extend(common_args(input, encoder, audio.map(|track| track.index)));
    args.extend(video_args(probe, max_height, encoder));
    args.extend(audio_args(probe, audio));
    args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    args.push(output.to_string_lossy().into_owned());
    args
}

/// Input-side options for an encoder: the VA-API device, or hardware decoding
/// for the GPU encoders. Software keeps the exact command line it had before.
fn input_args(encoder: VideoEncoder) -> Vec<String> {
    match encoder {
        VideoEncoder::Software => Vec::new(),
        VideoEncoder::Vaapi => vec!["-vaapi_device".into(), VAAPI_DEVICE.into()],
        // Decode on the GPU and copy frames back for the filter graph/encoder.
        // `-hwaccel_output_format` is deliberately left unset so the frames are
        // system memory, which every downstream filter here expects.
        _ => vec!["-hwaccel".into(), "auto".into()],
    }
}

/// The `-map` target for the audio stream: the chosen stream when the user
/// picked one, else the file's first audio (optional, so audio-only files work).
fn audio_map(audio_index: Option<u32>) -> String {
    match audio_index {
        Some(index) => format!("0:{index}"),
        None => "0:a:0?".to_string(),
    }
}

fn common_args(input: &str, encoder: VideoEncoder, audio_index: Option<u32>) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
    ];
    args.extend(input_args(encoder));
    args.extend([
        "-i".into(),
        input.to_string(),
        // `?` makes the stream optional, so audio-only files still work.
        "-map".into(),
        "0:v:0?".into(),
        "-map".into(),
        audio_map(audio_index),
        // Subtitles are never copied while casting.
        "-sn".into(),
    ]);
    args
}

/// The codec/quality flags for an encoder, without any scale filter.
fn codec_args(encoder: VideoEncoder) -> Vec<String> {
    let parts: &[&str] = match encoder {
        VideoEncoder::Software => &[
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "21", "-pix_fmt", "yuv420p",
        ],
        // VA-API takes a constant quantiser (`-qp`); there is no `-crf`.
        VideoEncoder::Vaapi => &["-c:v", "h264_vaapi", "-qp", "21"],
        VideoEncoder::Qsv => &[
            "-c:v",
            "h264_qsv",
            "-preset",
            "veryfast",
            "-global_quality",
            "21",
        ],
        VideoEncoder::Nvenc => &[
            "-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "21",
        ],
        VideoEncoder::VideoToolbox => &["-c:v", "h264_videotoolbox", "-q:v", "55"],
        VideoEncoder::Amf => &[
            "-c:v",
            "h264_amf",
            "-quality",
            "balanced",
            "-rc",
            "cqp",
            "-qp_i",
            "21",
            "-qp_p",
            "21",
        ],
    };
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// The scale filter, never upscaling; `-2` keeps the width even.
fn scale_filter(max_height: u32) -> String {
    format!("scale=-2:min(ih\\,{})", max_height.max(1))
}

/// The pixel-format conversion an encoder needs after scaling. VA-API needs the
/// frames uploaded to the device; Quick Sync wants NV12.
fn format_suffix(encoder: VideoEncoder) -> &'static str {
    match encoder {
        VideoEncoder::Vaapi => ",format=nv12,hwupload",
        VideoEncoder::Qsv => ",format=nv12",
        _ => "",
    }
}

fn video_args(probe: &Probe, max_height: u32, encoder: VideoEncoder) -> Vec<String> {
    let Some(video) = &probe.video else {
        return Vec::new();
    };
    let copy = probe.is_h264_8bit() && video.height.is_none_or(|height| height <= max_height);
    if copy {
        return vec!["-c:v".into(), "copy".into()];
    }
    let mut args = codec_args(encoder);
    args.extend([
        "-vf".into(),
        format!("{}{}", scale_filter(max_height), format_suffix(encoder)),
    ]);
    args
}

/// Audio codec flags. The chosen track's codec decides whether the audio can be
/// copied; without a choice, the file's first track does.
fn audio_args(probe: &Probe, audio: Option<&AudioTrack>) -> Vec<String> {
    let codec = audio
        .map(|track| track.codec.as_str())
        .or_else(|| probe.audio.as_ref().map(|audio| audio.codec.as_str()));
    let Some(codec) = codec else {
        return Vec::new();
    };
    if matches!(codec, "aac" | "mp3") {
        vec!["-c:a".into(), "copy".into()]
    } else {
        vec!["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]
    }
}

/// Arguments for a full conversion that also burns an image subtitle track
/// (PGS/DVD) into the picture.
///
/// Bitmap subtitles cannot be delivered as a text sidecar, and renderers that
/// will not read them from the container leave no other way to show them. Video
/// is always re-encoded — a subtitle overlay and `-c:v copy` are incompatible —
/// and the scale filter is folded into the same `filter_complex` graph.
pub fn transcode_args_burn(
    input: &str,
    probe: &Probe,
    max_height: u32,
    output: &Path,
    subtitle_index: u32,
    encoder: VideoEncoder,
    audio: Option<&AudioTrack>,
) -> Vec<String> {
    let cap = max_height.max(1);
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        // ffmpeg writes machine-readable progress to stdout while the media
        // itself goes to `output`, so the UI can show how far along it is.
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
    ];
    args.extend(input_args(encoder));
    args.extend([
        "-i".into(),
        input.to_string(),
        "-filter_complex".into(),
        format!(
            "[0:v:0]{scale}[scaled];[scaled][0:{subtitle_index}]overlay=shortest=1{suffix}[v]",
            scale = scale_filter(cap),
            suffix = format_suffix(encoder),
        ),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        audio_map(audio.map(|track| track.index)),
    ]);
    args.extend(codec_args(encoder));
    args.extend(audio_args(probe, audio));
    args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    args.push(output.to_string_lossy().into_owned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Audio, Video};

    const INPUT: &str = "http://127.0.0.1:3030/stream/1/0";

    #[test]
    fn extracts_one_subtitle_stream_to_srt() {
        let args = extract_subtitle_args("http://127.0.0.1/stream/3/8", 4, Path::new("/tmp/x.srt"));
        let has_pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(has_pair("-map", "0:4"));
        assert!(has_pair("-c:s", "srt"));
        assert!(args.iter().any(|a| a == "http://127.0.0.1/stream/3/8"));
        assert_eq!(args.last().unwrap(), "/tmp/x.srt");
    }

    fn probe(video: Option<Video>, audio: Option<Audio>) -> Probe {
        Probe {
            format_name: Some("matroska,webm".into()),
            duration: None,
            size: None,
            video,
            audio,
            audio_tracks: Vec::new(),
            subtitles: 0,
            subtitle_tracks: Vec::new(),
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
    fn burn_in_overlays_the_subtitle_stream() {
        let p = probe(Some(h264()), None);
        let args =
            transcode_args_burn(
                INPUT,
                &p,
                720,
                Path::new("/tmp/out.mp4"),
                3,
                VideoEncoder::Software,
                None,
            );
        let joined = args.join(" ");
        assert!(joined.contains("[0:3]overlay"), "overlay the subtitle: {joined}");
        assert!(joined.contains("-map [v]"));
        assert!(!joined.contains("-c:v copy"), "burn-in must re-encode video");
        assert_eq!(args.last().unwrap(), "/tmp/out.mp4");
    }

    #[test]
    fn converts_srt_to_webvtt() {
        let srt = "1\n00:00:01,000 --> 00:00:03,500\nHello, world\n\n\
                   2\n00:00:04,000 --> 00:00:05,250\nBye\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:03.500"));
        assert!(vtt.contains("00:00:04.000 --> 00:00:05.250"));
        // A comma in the cue text is not a timestamp separator.
        assert!(vtt.contains("Hello, world"));
        assert!(!vtt.contains("00:00:01,000"));
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
        let args = remux_stream_args(INPUT, None, None);
        let joined = args.join(" ");
        assert!(joined.contains("-c copy"));
        assert!(joined.contains("frag_keyframe"));
        assert_eq!(args.last().unwrap(), "pipe:1");
        assert!(joined.contains("-sn"));
        assert!(!joined.contains("-tag:v"));
    }

    #[test]
    fn remux_tags_hevc_as_hvc1() {
        let joined = remux_stream_args(INPUT, Some("hvc1"), None).join(" ");
        assert!(joined.contains("-tag:v hvc1"));
    }

    #[test]
    fn remux_file_copies_everything_to_a_faststart_file() {
        let args = remux_file_args(INPUT, Some("hvc1"), Some(3), Path::new("/o.mp4")).join(" ");
        assert!(args.contains("-c copy"));
        assert!(args.contains("-tag:v hvc1"));
        assert!(args.contains("+faststart"));
        assert!(args.contains("-map 0:3"));
        // A finished file, not a live fragmented stream.
        assert!(!args.contains("frag_keyframe"));
        assert!(args.ends_with("/o.mp4"));
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
            VideoEncoder::Software,
            None,
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
            VideoEncoder::Software,
            None,
        );
        let joined = args.join(" ");
        assert!(joined.contains("libx264"));
        assert!(joined.contains("scale=-2:min(ih\\,720)"));
        assert!(joined.contains("-c:a aac"));
    }

    #[test]
    fn hardware_encoders_change_the_command_line() {
        let hevc = Video {
            codec: "hevc".into(),
            profile: None,
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
        };
        let source = probe(
            Some(hevc),
            Some(Audio {
                codec: "dts".into(),
                channels: Some(6),
            }),
        );

        let vaapi = transcode_args(
            INPUT,
            &source,
            1080,
            Path::new("/o.mp4"),
            VideoEncoder::Vaapi,
            None,
        )
        .join(" ");
        assert!(vaapi.contains("-vaapi_device /dev/dri/renderD128"));
        assert!(vaapi.contains("-c:v h264_vaapi"));
        assert!(vaapi.contains("format=nv12,hwupload"));
        assert!(!vaapi.contains("libx264"));

        let nvenc = transcode_args(
            INPUT,
            &source,
            1080,
            Path::new("/o.mp4"),
            VideoEncoder::Nvenc,
            None,
        )
        .join(" ");
        assert!(nvenc.contains("-c:v h264_nvenc"));
        assert!(nvenc.contains("-hwaccel auto"));
        // NVENC scales in software; there is no upload suffix.
        assert!(!nvenc.contains("hwupload"));

        // A copyable stream ignores the encoder entirely.
        let copy = transcode_args(
            INPUT,
            &probe(Some(h264()), None),
            1080,
            Path::new("/o.mp4"),
            VideoEncoder::Nvenc,
            None,
        )
        .join(" ");
        assert!(copy.contains("-c:v copy"));
        assert!(!copy.contains("h264_nvenc"));
    }

    #[test]
    fn burn_in_uploads_frames_for_vaapi() {
        let p = probe(Some(h264()), None);
        let args = transcode_args_burn(
            INPUT,
            &p,
            720,
            Path::new("/tmp/out.mp4"),
            3,
            VideoEncoder::Vaapi,
            None,
        )
        .join(" ");
        assert!(args.contains("[0:3]overlay"), "overlay the subtitle: {args}");
        assert!(args.contains("overlay=shortest=1,format=nv12,hwupload[v]"));
        assert!(args.contains("-c:v h264_vaapi"));
        assert!(!args.contains("-c:v libx264"));
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
            VideoEncoder::Software,
            None,
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-c:a aac"));
    }

    #[test]
    fn a_chosen_audio_track_is_mapped_by_index() {
        let track = AudioTrack {
            index: 3,
            codec: "ac3".into(),
            channels: Some(6),
            language: Some("eng".into()),
            title: None,
        };
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
            VideoEncoder::Software,
            Some(&track),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:3"), "map the chosen stream: {joined}");
        // AC-3 is not copied unless the renderer advertises it.
        assert!(joined.contains("-c:a aac"));
    }
}
