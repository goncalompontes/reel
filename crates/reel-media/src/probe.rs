//! What `ffprobe` tells us about a file.
//!
//! Only the fields that drive a casting decision are kept: the container, the
//! first real video and audio streams (cover art is ignored), bit depth, and
//! whether there are subtitles.

use std::time::Duration;

use serde::Deserialize;

use crate::error::Result;

/// The probe result for one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// `format_name`, e.g. `matroska,webm` or `mov,mp4,m4a,3gp,3g2,mj2`.
    pub format_name: Option<String>,
    /// Total duration.
    pub duration: Option<Duration>,
    /// Size in bytes.
    pub size: Option<u64>,
    /// The primary video stream, if any.
    pub video: Option<Video>,
    /// The primary audio stream, if any.
    pub audio: Option<Audio>,
    /// Every audio stream, in file order. The first is also [`Probe::audio`].
    pub audio_tracks: Vec<AudioTrack>,
    /// Number of subtitle streams.
    pub subtitles: usize,
    /// The subtitle streams, in file order.
    pub subtitle_tracks: Vec<SubtitleTrack>,
}

/// The primary video stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Video {
    /// ffmpeg codec name, e.g. `h264`, `hevc`, `mpeg4`.
    pub codec: String,
    /// Codec profile, e.g. `High`, `Main 10`.
    pub profile: Option<String>,
    /// Width in pixels.
    pub width: Option<u32>,
    /// Height in pixels.
    pub height: Option<u32>,
    /// Bits per pixel component, when it can be inferred.
    pub bit_depth: Option<u32>,
}

/// The primary audio stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    /// ffmpeg codec name, e.g. `aac`, `ac3`, `dts`.
    pub codec: String,
    /// Channel count.
    pub channels: Option<u32>,
}

/// One audio stream, so a preferred-language track can be chosen when a file
/// carries several (a dual-audio release, for example).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    /// The stream index ffmpeg uses, e.g. `0:2` in `-map`.
    pub index: u32,
    /// ffmpeg codec name, e.g. `aac`, `ac3`, `dts`.
    pub codec: String,
    /// Channel count.
    pub channels: Option<u32>,
    /// ISO 639 language tag, when the file names one.
    pub language: Option<String>,
    /// Human-readable track title, when present.
    pub title: Option<String>,
}

/// One subtitle stream, so a text track can be extracted and delivered to a
/// renderer that does not read the container's embedded subtitles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
    /// The stream index ffmpeg uses, e.g. `0:3` in `-map`.
    pub index: u32,
    /// ffmpeg codec name, e.g. `subrip`, `ass`, `hdmv_pgs_subtitle`.
    pub codec: String,
    /// ISO 639 language tag, when the file names one.
    pub language: Option<String>,
    /// Human-readable track title, when present.
    pub title: Option<String>,
}

impl SubtitleTrack {
    /// Whether this is a text track, which can be delivered as `.srt`. Image
    /// formats (PGS/VobSub) cannot be, so they are skipped.
    pub fn is_text(&self) -> bool {
        matches!(
            self.codec.as_str(),
            "subrip"
                | "srt"
                | "ass"
                | "ssa"
                | "mov_text"
                | "webvtt"
                | "text"
                | "subviewer"
                | "subviewer1"
                | "microdvd"
                | "eia_608"
        )
    }
}

impl Probe {
    /// Parse the JSON emitted by `ffprobe -print_format json -show_format
    /// -show_streams`.
    pub fn from_json(json: &str) -> Result<Probe> {
        let output: Output = serde_json::from_str(json)?;

        let mut video = None;
        let mut audio = None;
        let mut audio_tracks: Vec<AudioTrack> = Vec::new();
        let mut subtitle_tracks: Vec<SubtitleTrack> = Vec::new();
        for stream in output.streams {
            match stream.codec_type.as_deref() {
                // Cover art is a video stream too; it must not become "the
                // video" a casting decision is based on.
                Some("video") if stream.disposition.attached_pic.unwrap_or(0) == 0 => {
                    video.get_or_insert_with(|| Video {
                        codec: stream.codec_name.clone().unwrap_or_default(),
                        profile: stream.profile.clone(),
                        width: stream.width,
                        height: stream.height,
                        bit_depth: bit_depth(&stream),
                    });
                }
                Some("audio") => {
                    let track = AudioTrack {
                        index: stream.index.unwrap_or(u32::MAX),
                        codec: stream.codec_name.clone().unwrap_or_default(),
                        channels: stream.channels,
                        language: clean_tag(stream.tags.language.as_deref()),
                        title: clean_tag(stream.tags.title.as_deref()),
                    };
                    if audio.is_none() {
                        audio = Some(Audio {
                            codec: track.codec.clone(),
                            channels: track.channels,
                        });
                    }
                    audio_tracks.push(track);
                }
                Some("subtitle") => subtitle_tracks.push(SubtitleTrack {
                    index: stream.index.unwrap_or(u32::MAX),
                    codec: stream.codec_name.clone().unwrap_or_default(),
                    language: clean_tag(stream.tags.language.as_deref()),
                    title: clean_tag(stream.tags.title.as_deref()),
                }),
                _ => {}
            }
        }
        let subtitles = subtitle_tracks.len();

        let format = output.format.unwrap_or_default();
        Ok(Probe {
            format_name: format.format_name,
            duration: format
                .duration
                .as_deref()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|seconds| *seconds > 0.0)
                .map(Duration::from_secs_f64),
            size: format.size.as_deref().and_then(|value| value.parse().ok()),
            video,
            audio,
            audio_tracks,
            subtitles,
            subtitle_tracks,
        })
    }

    /// Whether the container is in the MP4 family.
    pub fn is_mp4(&self) -> bool {
        self.container_token()
            .is_some_and(|token| matches!(token, "mp4" | "mov" | "m4a" | "3gp" | "3g2" | "mj2"))
    }

    /// Whether the container is Matroska (`.mkv`/`.webm`).
    pub fn is_matroska(&self) -> bool {
        self.container_token() == Some("matroska")
    }

    /// The primary video codec name.
    pub fn video_codec(&self) -> Option<&str> {
        self.video.as_ref().map(|video| video.codec.as_str())
    }

    /// The primary audio codec name.
    pub fn audio_codec(&self) -> Option<&str> {
        self.audio.as_ref().map(|audio| audio.codec.as_str())
    }

    /// The first meaningful container token from `format_name`.
    pub fn container_token(&self) -> Option<&str> {
        self.format_name.as_deref().and_then(|name| {
            name.split(',')
                .map(str::trim)
                .find(|token| !token.is_empty())
        })
    }

    /// Whether the primary video is 8-bit H.264 (the DLNA sweet spot).
    pub fn is_h264_8bit(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(|video| video.codec == "h264" && video.bit_depth.unwrap_or(8) <= 8)
    }
}

/// Infer bit depth from `bits_per_raw_sample`, or from the pixel format.
fn bit_depth(stream: &Stream) -> Option<u32> {    if let Some(bits) = stream.bits_per_raw_sample.as_deref() {
        if let Ok(bits) = bits.trim().parse::<u32>() {
            return Some(bits);
        }
    }
    let pix_fmt = stream.pix_fmt.as_deref()?;
    // High bit-depth formats end in `le`/`be` and name the depth, e.g.
    // `yuv420p10le`. `yuv420p` (and `yuv410p`) do not, and are 8-bit.
    if let Some(stem) = pix_fmt
        .strip_suffix("le")
        .or_else(|| pix_fmt.strip_suffix("be"))
    {
        if let Some(digits) = stem.rsplit(|ch: char| !ch.is_ascii_digit()).next() {
            if let Ok(depth) = digits.parse::<u32>() {
                return Some(depth);
            }
        }
    }
    Some(8)
}

#[derive(Debug, Deserialize)]
struct Output {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Option<Format>,
}

/// Normalise an ffprobe tag: trim it and drop the placeholders ffprobe emits
/// for "no language" (`und`, empty).
fn clean_tag(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("und"))
        .map(str::to_string)
}

#[derive(Debug, Deserialize)]
struct Stream {
    #[serde(default)]
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    pix_fmt: Option<String>,
    bits_per_raw_sample: Option<String>,
    #[serde(default)]
    disposition: Disposition,
    #[serde(default)]
    tags: Tags,
}

#[derive(Debug, Default, Deserialize)]
struct Tags {
    language: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Disposition {
    attached_pic: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct Format {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const H264_MKV: &str = r#"{
        "streams": [
            {"index":0,"codec_type":"video","codec_name":"h264","profile":"High",
             "width":1920,"height":1080,"pix_fmt":"yuv420p","bits_per_raw_sample":"8",
             "disposition":{"attached_pic":0}},
            {"index":1,"codec_type":"audio","codec_name":"aac","channels":2},
            {"index":2,"codec_type":"subtitle","codec_name":"subrip",
             "tags":{"language":"eng","title":"English"}}
        ],
        "format": {"format_name":"matroska,webm","duration":"42.500000","size":"104857600"}
    }"#;

    #[test]
    fn parses_streams_and_format() {
        let probe = Probe::from_json(H264_MKV).unwrap();
        assert_eq!(probe.container_token(), Some("matroska"));
        assert!(probe.is_matroska());
        assert!(!probe.is_mp4());
        assert_eq!(probe.video_codec(), Some("h264"));
        assert_eq!(probe.audio_codec(), Some("aac"));
        assert_eq!(probe.subtitles, 1);
        assert_eq!(probe.subtitle_tracks.len(), 1);
        let track = &probe.subtitle_tracks[0];
        assert_eq!(track.index, 2);
        assert_eq!(track.language.as_deref(), Some("eng"));
        assert!(track.is_text());
        assert_eq!(probe.duration, Some(Duration::from_millis(42_500)));
        assert_eq!(probe.size, Some(104_857_600));
        assert!(probe.is_h264_8bit());
    }

    #[test]
    fn collects_every_audio_track_with_language() {
        let json = r#"{
            "streams": [
                {"index":0,"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p"},
                {"index":1,"codec_type":"audio","codec_name":"aac","channels":2,
                 "tags":{"language":"eng","title":"English"}},
                {"index":2,"codec_type":"audio","codec_name":"ac3","channels":6,
                 "tags":{"language":"fra"}},
                {"index":3,"codec_type":"audio","codec_name":"ac3","channels":6}
            ]
        }"#;
        let probe = Probe::from_json(json).unwrap();
        assert_eq!(probe.audio_tracks.len(), 3);
        // The primary stream is still the first one.
        assert_eq!(probe.audio_codec(), Some("aac"));
        assert_eq!(probe.audio_tracks[0].index, 1);
        assert_eq!(probe.audio_tracks[0].language.as_deref(), Some("eng"));
        assert_eq!(probe.audio_tracks[1].index, 2);
        assert_eq!(probe.audio_tracks[1].language.as_deref(), Some("fra"));
        assert_eq!(probe.audio_tracks[2].language, None);
    }

    #[test]
    fn image_subtitles_are_not_text_tracks() {
        let json = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
            {"index":1,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle",
             "tags":{"language":"und"}}]}"#;
        let probe = Probe::from_json(json).unwrap();
        assert_eq!(probe.subtitle_tracks.len(), 1);
        assert!(!probe.subtitle_tracks[0].is_text());
        // `und` is not a language.
        assert_eq!(probe.subtitle_tracks[0].language, None);
    }

    #[test]
    fn ignores_cover_art_and_infers_ten_bit_depth() {
        let json = r#"{
            "streams": [
                {"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
                {"codec_type":"video","codec_name":"hevc","profile":"Main 10","pix_fmt":"yuv420p10le","height":2160},
                {"codec_type":"audio","codec_name":"eac3","channels":6}
            ],
            "format": {"format_name":"matroska,webm"}
        }"#;
        let probe = Probe::from_json(json).unwrap();
        assert_eq!(probe.video_codec(), Some("hevc"));
        assert_eq!(probe.video.as_ref().unwrap().bit_depth, Some(10));
        assert!(!probe.is_h264_8bit());
        assert_eq!(probe.audio_codec(), Some("eac3"));
    }

    #[test]
    fn recognizes_the_mp4_family() {
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p"}],
            "format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#;
        let probe = Probe::from_json(json).unwrap();
        assert!(probe.is_mp4());
        assert_eq!(probe.video.as_ref().unwrap().bit_depth, Some(8));
    }

    #[test]
    fn tolerates_missing_format_and_streams() {
        let probe = Probe::from_json(r#"{"streams":[]}"#).unwrap();
        assert_eq!(probe.container_token(), None);
        assert!(probe.video.is_none());
        assert_eq!(probe.subtitles, 0);
    }
}
