//! Decide how a file should be delivered to a renderer.
//!
//! DLNA receivers play a narrow slice of what torrents contain. The happy path
//! is [`MediaPlan::Original`] (serve the bytes we have); otherwise the file is
//! rewrapped ([`MediaPlan::Remux`]) or re-encoded
//! ([`MediaPlan::Transcode`]), subject to what the user allows.

use crate::probe::Probe;

/// How far the user lets reel go when a file is not directly playable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranscodeMode {
    /// Serve only files a typical TV can play; never convert.
    Off,
    /// Rewrap (change the container) but never re-encode.
    RemuxOnly,
    /// Rewrap when possible, re-encode only when necessary.
    #[default]
    Auto,
}

/// How to deliver a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPlan {
    /// Serve the file as-is.
    Original,
    /// Rewrap into MP4 with every stream copied.
    Remux,
    /// Re-encode to H.264/AAC, scaled down to at most `max_height`.
    Transcode {
        /// Maximum output height in pixels.
        max_height: u32,
    },
    /// Not playable, and conversion is off or would not help.
    Unavailable {
        /// A short, user-facing explanation.
        reason: String,
    },
}

impl MediaPlan {
    /// Whether this plan requires ffmpeg.
    pub fn needs_conversion(&self) -> bool {
        matches!(self, MediaPlan::Remux | MediaPlan::Transcode { .. })
    }

    /// MIME type of the converted output, when there is one. An `Original`
    /// plan is served with the source file's own type.
    pub fn output_mime(&self) -> Option<&'static str> {
        match self {
            MediaPlan::Remux | MediaPlan::Transcode { .. } => Some("video/mp4"),
            MediaPlan::Original | MediaPlan::Unavailable { .. } => None,
        }
    }

    /// Whether the output is a complete file that can be byte-range seeked.
    ///
    /// A live remux is a fragmented stream, so only a finished transcode file
    /// can be seeked freely.
    pub fn is_seekable_file(&self) -> bool {
        matches!(self, MediaPlan::Transcode { .. })
    }
}

/// What a specific renderer accepts, from its `ConnectionManager`
/// `SinkProtocolInfo` (or a conservative guess when that cannot be read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SinkSupport {
    /// H.265/HEVC video.
    pub hevc: bool,
    /// AC-3 audio.
    pub ac3: bool,
    /// E-AC-3 (Dolby Digital Plus) audio.
    pub eac3: bool,
    /// The MP4 container (`video/mp4`).
    pub mp4: bool,
    /// The Matroska container (`video/x-matroska`).
    pub mkv: bool,
}

/// Knobs for [`plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanOptions {
    /// How much conversion is allowed.
    pub mode: TranscodeMode,
    /// Quality cap applied when re-encoding.
    pub max_height: u32,
    /// What the target renderer accepts.
    pub sink: SinkSupport,
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            mode: TranscodeMode::Auto,
            max_height: 1080,
            sink: SinkSupport::default(),
        }
    }
}

/// Audio codecs a TV will almost certainly decode inside an MP4.
fn audio_is_copyable(codec: &str, sink: SinkSupport) -> bool {
    match codec {
        "aac" | "mp3" => true,
        "ac3" => sink.ac3,
        "eac3" => sink.eac3,
        _ => false,
    }
}

/// Whether the video can be copied into MP4 unchanged.
fn video_is_copyable(probe: &Probe, sink: SinkSupport) -> bool {
    if probe.is_h264_8bit() {
        return true;
    }
    sink.hevc
        && probe
            .video
            .as_ref()
            .is_some_and(|video| matches!(video.codec.as_str(), "hevc" | "h265"))
}

/// Choose how to deliver `probe` to a renderer.
pub fn plan(probe: &Probe, options: &PlanOptions) -> MediaPlan {
    // The renderer advertises the file's own container: hand it the original
    // bytes, with no conversion at all. This is how a 4K HEVC MKV plays directly
    // on a television that lists `video/x-matroska`.
    let container_ok =
        (probe.is_mp4() && options.sink.mp4) || (probe.is_matroska() && options.sink.mkv);
    if container_ok {
        return MediaPlan::Original;
    }

    // No video stream (audio-only) counts as compatible.
    let video_ok = probe.video.is_none() || video_is_copyable(probe, options.sink);
    let audio_ok = probe
        .audio
        .as_ref()
        .is_none_or(|audio| audio_is_copyable(&audio.codec, options.sink));

    if probe.is_mp4() && video_ok && audio_ok {
        return MediaPlan::Original;
    }

    if options.mode == TranscodeMode::Off {
        return MediaPlan::Unavailable {
            reason: unsupported(probe, options.sink),
        };
    }

    // Both streams copy cleanly into MP4: a container change is enough.
    if video_ok && audio_ok {
        return MediaPlan::Remux;
    }

    if options.mode == TranscodeMode::RemuxOnly {
        return MediaPlan::Unavailable {
            reason: unsupported(probe, options.sink),
        };
    }

    MediaPlan::Transcode {
        max_height: options.max_height.max(1),
    }
}

/// Describe the parts that make a file not directly playable.
fn unsupported(probe: &Probe, sink: SinkSupport) -> String {
    let mut features = Vec::new();
    if !probe.is_mp4() {
        features.push(format!(
            "{} container",
            probe.container_token().unwrap_or("unknown")
        ));
    }
    if let Some(video) = &probe.video {
        if !video_is_copyable(probe, sink) {
            features.push(match video.bit_depth {
                Some(depth) if depth > 8 => format!("{}-bit {} video", depth, video.codec),
                _ => format!("{} video", video.codec),
            });
        }
    }
    if let Some(audio) = &probe.audio {
        if !audio_is_copyable(&audio.codec, sink) {
            features.push(format!("{} audio", audio.codec));
        }
    }

    if features.is_empty() {
        "not directly playable".to_string()
    } else {
        format!("needs conversion ({})", features.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Audio, Video};

    fn probe(container: &str, video: Option<Video>, audio: Option<Audio>) -> Probe {
        Probe {
            format_name: Some(container.to_string()),
            duration: None,
            size: None,
            video,
            audio,
            subtitles: 0,
        }
    }

    fn h264(height: u32) -> Video {
        Video {
            codec: "h264".to_string(),
            profile: Some("High".to_string()),
            width: None,
            height: Some(height),
            bit_depth: Some(8),
        }
    }

    fn audio(codec: &str) -> Audio {
        Audio {
            codec: codec.to_string(),
            channels: Some(2),
        }
    }

    fn auto() -> PlanOptions {
        PlanOptions::default()
    }

    #[test]
    fn an_mp4_tv_compatible_file_is_served_as_is() {
        let file = probe("mov,mp4,m4a", Some(h264(1080)), Some(audio("aac")));
        assert_eq!(plan(&file, &auto()), MediaPlan::Original);
        assert!(!plan(&file, &auto()).needs_conversion());
    }

    #[test]
    fn a_rewrappable_mkv_is_remuxed() {
        let file = probe("matroska,webm", Some(h264(1080)), Some(audio("aac")));
        assert_eq!(plan(&file, &auto()), MediaPlan::Remux);
        assert_eq!(
            plan(
                &file,
                &PlanOptions {
                    mode: TranscodeMode::RemuxOnly,
                    ..auto()
                }
            ),
            MediaPlan::Remux
        );
        assert!(matches!(
            plan(
                &file,
                &PlanOptions {
                    mode: TranscodeMode::Off,
                    ..auto()
                }
            ),
            MediaPlan::Unavailable { .. }
        ));
    }

    #[test]
    fn incompatible_audio_forces_a_transcode() {
        let file = probe("matroska,webm", Some(h264(1080)), Some(audio("dts")));
        assert_eq!(
            plan(&file, &auto()),
            MediaPlan::Transcode { max_height: 1080 }
        );
        assert!(matches!(
            plan(
                &file,
                &PlanOptions {
                    mode: TranscodeMode::RemuxOnly,
                    ..auto()
                }
            ),
            MediaPlan::Unavailable { .. }
        ));
    }

    #[test]
    fn ten_bit_hevc_needs_a_transcode_even_in_mp4() {
        let hevc = Video {
            codec: "hevc".to_string(),
            profile: Some("Main 10".to_string()),
            width: None,
            height: Some(2160),
            bit_depth: Some(10),
        };
        let file = probe("mov,mp4,m4a", Some(hevc), Some(audio("aac")));
        assert_eq!(
            plan(
                &file,
                &PlanOptions {
                    max_height: 720,
                    ..auto()
                }
            ),
            MediaPlan::Transcode { max_height: 720 }
        );
    }

    #[test]
    fn audio_only_is_served_or_converted() {
        assert_eq!(
            plan(&probe("mov,mp4,m4a", None, Some(audio("aac"))), &auto()),
            MediaPlan::Original
        );
        assert_eq!(
            plan(&probe("matroska,webm", None, Some(audio("flac"))), &auto()),
            MediaPlan::Transcode { max_height: 1080 }
        );
    }

    #[test]
    fn the_reason_names_what_is_wrong() {
        let file = probe("matroska,webm", Some(h264(1080)), Some(audio("dts")));
        let reason = match plan(
            &file,
            &PlanOptions {
                mode: TranscodeMode::Off,
                ..auto()
            },
        ) {
            MediaPlan::Unavailable { reason } => reason,
            other => panic!("expected unavailable, got {other:?}"),
        };
        assert!(reason.contains("matroska"));
        assert!(reason.contains("dts audio"));
    }

    #[test]
    fn passthrough_remuxes_hevc_and_ac3() {
        let hevc10 = Video {
            codec: "hevc".to_string(),
            profile: Some("Main 10".to_string()),
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
        };
        let file = probe("matroska,webm", Some(hevc10), Some(audio("eac3")));

        // The conservative default re-encodes it.
        assert!(matches!(plan(&file, &auto()), MediaPlan::Transcode { .. }));

        // A renderer that accepts HEVC and E-AC3 makes it a container remux.
        let options = PlanOptions {
            sink: SinkSupport {
                hevc: true,
                eac3: true,
                ..Default::default()
            },
            ..auto()
        };
        assert_eq!(plan(&file, &options), MediaPlan::Remux);
    }

    #[test]
    fn an_advertised_container_is_served_as_original() {
        let hevc10 = Video {
            codec: "hevc".to_string(),
            profile: Some("Main 10".to_string()),
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
        };
        let file = probe("matroska,webm", Some(hevc10), Some(audio("eac3")));
        assert!(matches!(plan(&file, &auto()), MediaPlan::Transcode { .. }));

        let options = PlanOptions {
            sink: SinkSupport {
                mkv: true,
                ..Default::default()
            },
            ..auto()
        };
        assert_eq!(plan(&file, &options), MediaPlan::Original);
    }

    #[test]
    fn plans_report_their_outputs() {
        assert_eq!(MediaPlan::Remux.output_mime(), Some("video/mp4"));
        assert_eq!(MediaPlan::Original.output_mime(), None);
        assert!(MediaPlan::Transcode { max_height: 1080 }.is_seekable_file());
        assert!(!MediaPlan::Remux.is_seekable_file());
    }
}
