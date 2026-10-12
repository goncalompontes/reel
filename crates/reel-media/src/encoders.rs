//! Which H.264 encoder to use, and what the ffmpeg build actually offers.
//!
//! Re-encoding is the only expensive part of casting, and on the CPU it is often
//! slower than realtime for 4K sources. This module asks ffmpeg which encoders
//! it has (a hardware encoder can be compiled in but still unusable on a machine
//! with no matching GPU, so the encoder list is a *hint* — the caller still has
//! to tolerate a runtime failure), and picks the best one for the platform.
//!
//! Pure except for [`AvailableEncoders::detect`], which spawns ffmpeg once.

use std::path::Path;
use std::process::Command;

/// An H.264 encoder ffmpeg can be asked to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    /// `libx264`, the portable CPU encoder. Always available.
    Software,
    /// Linux VA-API (`h264_vaapi`), Intel/AMD.
    Vaapi,
    /// Intel Quick Sync Video (`h264_qsv`).
    Qsv,
    /// NVIDIA NVENC (`h264_nvenc`).
    Nvenc,
    /// macOS VideoToolbox (`h264_videotoolbox`).
    VideoToolbox,
    /// Windows AMD AMF (`h264_amf`).
    Amf,
}

impl VideoEncoder {
    /// Whether this encoder runs on a GPU.
    pub fn is_hardware(self) -> bool {
        !matches!(self, VideoEncoder::Software)
    }

    /// The ffmpeg encoder name, e.g. `h264_nvenc`.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            VideoEncoder::Software => "libx264",
            VideoEncoder::Vaapi => "h264_vaapi",
            VideoEncoder::Qsv => "h264_qsv",
            VideoEncoder::Nvenc => "h264_nvenc",
            VideoEncoder::VideoToolbox => "h264_videotoolbox",
            VideoEncoder::Amf => "h264_amf",
        }
    }

    /// A short human label for logs and the Settings page.
    pub fn label(self) -> &'static str {
        match self {
            VideoEncoder::Software => "CPU (libx264)",
            VideoEncoder::Vaapi => "VA-API (GPU)",
            VideoEncoder::Qsv => "Quick Sync (GPU)",
            VideoEncoder::Nvenc => "NVENC (GPU)",
            VideoEncoder::VideoToolbox => "VideoToolbox (GPU)",
            VideoEncoder::Amf => "AMF (GPU)",
        }
    }
}

/// Which hardware H.264 encoders this ffmpeg build lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AvailableEncoders {
    /// VA-API (`h264_vaapi`).
    pub vaapi: bool,
    /// Quick Sync (`h264_qsv`).
    pub qsv: bool,
    /// NVENC (`h264_nvenc`).
    pub nvenc: bool,
    /// VideoToolbox (`h264_videotoolbox`).
    pub videotoolbox: bool,
    /// AMF (`h264_amf`).
    pub amf: bool,
}

impl AvailableEncoders {
    /// Ask the ffmpeg binary what it offers. Never fails: an unreadable answer
    /// is treated as "software only".
    pub fn detect(ffmpeg: &Path) -> AvailableEncoders {
        match list_encoders(ffmpeg) {
            Some(listing) => AvailableEncoders::from_encoder_listing(&listing),
            None => AvailableEncoders::default(),
        }
    }

    /// Parse the output of `ffmpeg -encoders`.
    ///
    /// Encoder lines look like ` V....D h264_nvenc ...`, so the name is the
    /// second whitespace-separated token.
    pub fn from_encoder_listing(listing: &str) -> AvailableEncoders {
        let has = |name: &str| {
            listing
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(name))
        };
        AvailableEncoders {
            vaapi: has("h264_vaapi"),
            qsv: has("h264_qsv"),
            nvenc: has("h264_nvenc"),
            videotoolbox: has("h264_videotoolbox"),
            amf: has("h264_amf"),
        }
    }

    /// Whether any hardware encoder is listed.
    pub fn any_hardware(&self) -> bool {
        self.vaapi || self.qsv || self.nvenc || self.videotoolbox || self.amf
    }

    /// Whether a specific encoder is usable, per this listing.
    pub fn supports(&self, encoder: VideoEncoder) -> bool {
        match encoder {
            VideoEncoder::Software => true,
            VideoEncoder::Vaapi => self.vaapi,
            VideoEncoder::Qsv => self.qsv,
            VideoEncoder::Nvenc => self.nvenc,
            VideoEncoder::VideoToolbox => self.videotoolbox,
            VideoEncoder::Amf => self.amf,
        }
    }

    /// The best encoder to use on this platform, or [`VideoEncoder::Software`].
    ///
    /// Hardware encoders are listed even when a machine has no matching GPU, so
    /// this is a preference, not a guarantee; the caller retries on software if
    /// the chosen encoder fails.
    pub fn best(&self) -> VideoEncoder {
        #[cfg(target_os = "macos")]
        {
            if self.videotoolbox {
                return VideoEncoder::VideoToolbox;
            }
        }
        #[cfg(windows)]
        {
            if self.nvenc {
                return VideoEncoder::Nvenc;
            }
            if self.qsv {
                return VideoEncoder::Qsv;
            }
            if self.amf {
                return VideoEncoder::Amf;
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // VA-API is the broadest on Linux; NVENC next (usually fastest).
            if self.vaapi {
                return VideoEncoder::Vaapi;
            }
            if self.nvenc {
                return VideoEncoder::Nvenc;
            }
            if self.qsv {
                return VideoEncoder::Qsv;
            }
        }
        VideoEncoder::Software
    }

    /// Resolve a preference to an encoder that is actually usable.
    ///
    /// `None` means "auto" (best for the platform); `Some(Software)` forces the
    /// CPU; any other pinned encoder falls back to software when it is not
    /// listed.
    pub fn choose(&self, prefer: Option<VideoEncoder>) -> VideoEncoder {
        match prefer {
            None => self.best(),
            Some(encoder) if self.supports(encoder) => encoder,
            Some(_) => VideoEncoder::Software,
        }
    }
}

/// Run `ffmpeg -encoders` and return its output (stdout, or stderr when a build
/// prints there). `None` when the binary cannot be run.
fn list_encoders(ffmpeg: &Path) -> Option<String> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !text.contains("Encoders:") {
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed `ffmpeg -encoders` listing.
    const LISTING: &str = "\
Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V..... h264_qsv             H.264 / AVC (Intel Quick Sync Video) (codec h264)
 V....D h264_vaapi           H.264/AVC (VAAPI) (codec h264)
 A..... aac                  AAC (Advanced Audio Coding)
";

    #[test]
    fn parses_the_encoder_listing() {
        let available = AvailableEncoders::from_encoder_listing(LISTING);
        assert!(available.vaapi);
        assert!(available.qsv);
        assert!(available.nvenc);
        assert!(!available.videotoolbox);
        assert!(!available.amf);
        assert!(available.any_hardware());
    }

    #[test]
    fn an_empty_listing_is_software_only() {
        let available = AvailableEncoders::default();
        assert!(!available.any_hardware());
        assert_eq!(available.best(), VideoEncoder::Software);
        assert_eq!(available.choose(None), VideoEncoder::Software);
    }

    #[test]
    fn a_pinned_encoder_falls_back_when_absent() {
        let available = AvailableEncoders::from_encoder_listing(LISTING);
        assert_eq!(available.choose(Some(VideoEncoder::Amf)), VideoEncoder::Software);
        assert_eq!(
            available.choose(Some(VideoEncoder::Software)),
            VideoEncoder::Software
        );
        assert_eq!(available.choose(Some(VideoEncoder::Vaapi)), VideoEncoder::Vaapi);
        // Auto picks something the listing supports, never an absent encoder.
        assert!(available.supports(available.choose(None)));
    }

    #[test]
    fn encoder_names_and_labels_are_stable() {
        assert_eq!(VideoEncoder::Nvenc.ffmpeg_name(), "h264_nvenc");
        assert_eq!(VideoEncoder::Software.ffmpeg_name(), "libx264");
        assert!(VideoEncoder::Vaapi.is_hardware());
        assert!(!VideoEncoder::Software.is_hardware());
    }
}
