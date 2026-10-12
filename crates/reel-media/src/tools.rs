//! Finding and running ffmpeg and ffprobe.
//!
//! Resolution order: an explicit override, then a copy bundled next to the reel
//! executable, then `PATH`. When both tools are absent, casting is disabled
//! rather than failing to build — see [`Tools::try_resolve`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::convert;
use crate::encoders::VideoEncoder;
use crate::error::{MediaError, Result};
use crate::probe::{AudioTrack, Probe};

/// How long `ffprobe` may take before the cast is failed. A stalled stream (or
/// a torrent that never delivers bytes) must not hang a cast forever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolved paths to the two tools.
#[derive(Debug, Clone)]
pub struct Tools {
    /// `ffmpeg`, used for remuxing and transcoding.
    pub ffmpeg: PathBuf,
    /// `ffprobe`, used for inspection.
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Resolve both tools, or explain which is missing.
    pub fn resolve(ffmpeg: Option<&Path>, ffprobe: Option<&Path>) -> Result<Tools> {
        let ffmpeg = find("ffmpeg", ffmpeg).ok_or(MediaError::ToolMissing { tool: "ffmpeg" })?;
        let ffprobe =
            find("ffprobe", ffprobe).ok_or(MediaError::ToolMissing { tool: "ffprobe" })?;
        Ok(Tools { ffmpeg, ffprobe })
    }

    /// Resolve both tools, returning `None` if either is missing.
    pub fn try_resolve(ffmpeg: Option<&Path>, ffprobe: Option<&Path>) -> Option<Tools> {
        Tools::resolve(ffmpeg, ffprobe).ok()
    }

    /// The directory searched for bundled binaries: the reel executable's own
    /// directory (so a packaged app can ship `ffmpeg` beside the binary).
    pub fn bundled_dir() -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
    }

    /// Which hardware H.264 encoders this ffmpeg build lists.
    pub fn available_encoders(&self) -> crate::encoders::AvailableEncoders {
        crate::encoders::AvailableEncoders::detect(&self.ffmpeg)
    }

    /// Inspect a file or stream URL.
    ///
    /// Bounded: a stalled stream must fail the cast rather than hang it forever.
    pub async fn probe(&self, input: &str) -> Result<Probe> {
        let probe = tokio::process::Command::new(&self.ffprobe)
            .args(convert::probe_args(input))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(PROBE_TIMEOUT, probe)
            .await
            .map_err(|_| MediaError::Tool {
                tool: "ffprobe",
                message: format!("timed out after {}s", PROBE_TIMEOUT.as_secs()),
            })??;
        if !output.status.success() {
            return Err(MediaError::Tool {
                tool: "ffprobe",
                message: stderr(&output.stderr),
            });
        }
        Probe::from_json(&String::from_utf8_lossy(&output.stdout))
    }

    /// Start a live remux. The child's stdout is a fragmented MP4 stream.
    ///
    /// The child is killed when dropped, so ending the HTTP response stops the
    /// conversion.
    pub fn spawn_remux(
        &self,
        input: &str,
        video_tag: Option<&str>,
    ) -> Result<tokio::process::Child> {
        // No audio-stream override: the live remux the desktop runs supplies the
        // chosen index itself.
        tokio::process::Command::new(&self.ffmpeg)
            .args(convert::remux_stream_args(input, video_tag, None))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(Into::into)
    }

    /// Remux every stream into a seekable, faststart MP4 file, waiting for it to
    /// finish. Used for Google Cast, which needs a real duration and seeking.
    pub async fn remux_to_file(
        &self,
        input: &str,
        video_tag: Option<&str>,
        audio_index: Option<u32>,
        output: &Path,
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let result = tokio::process::Command::new(&self.ffmpeg)
            .args(convert::remux_file_args(
                input,
                video_tag,
                audio_index,
                output,
            ))
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        if !result.status.success() {
            return Err(MediaError::Tool {
                tool: "ffmpeg",
                message: stderr(&result.stderr),
            });
        }
        Ok(())
    }

    /// Run a full conversion to `output`, waiting for it to finish.
    ///
    /// A hardware encoder is tried first; when it fails (no GPU, missing
    /// driver, a busy device) the conversion is retried on the CPU. ffmpeg
    /// lists hardware encoders even when the machine cannot run them, so this
    /// fallback is what makes `Auto` safe.
    pub async fn transcode(
        &self,
        input: &str,
        probe: &Probe,
        max_height: u32,
        output: &Path,
        encoder: VideoEncoder,
        audio: Option<&AudioTrack>,
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match self
            .transcode_once(input, probe, max_height, output, encoder, audio)
            .await
        {
            Err(error) if encoder.is_hardware() => {
                tracing::warn!(
                    encoder = encoder.ffmpeg_name(),
                    %error,
                    "hardware transcode failed; retrying on the CPU"
                );
                self.transcode_once(
                    input,
                    probe,
                    max_height,
                    output,
                    VideoEncoder::Software,
                    audio,
                )
                .await
            }
            result => result,
        }
    }

    /// One transcode attempt with a specific encoder.
    async fn transcode_once(
        &self,
        input: &str,
        probe: &Probe,
        max_height: u32,
        output: &Path,
        encoder: VideoEncoder,
        audio: Option<&AudioTrack>,
    ) -> Result<()> {
        let result = tokio::process::Command::new(&self.ffmpeg)
            .args(convert::transcode_args(
                input, probe, max_height, output, encoder, audio,
            ))
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        if !result.status.success() {
            return Err(MediaError::Tool {
                tool: "ffmpeg",
                message: stderr(&result.stderr),
            });
        }
        Ok(())
    }

    /// Run a full conversion that burns an image subtitle track into the
    /// picture, reporting progress in seconds of input consumed.
    ///
    /// As with [`Tools::transcode`], a failed hardware attempt is retried on the
    /// CPU (the progress callback may be driven twice if the first attempt got
    /// as far as producing frames).
    #[allow(clippy::too_many_arguments)]
    pub async fn transcode_burn<F>(
        &self,
        input: &str,
        probe: &Probe,
        max_height: u32,
        output: &Path,
        subtitle_index: u32,
        encoder: VideoEncoder,
        audio: Option<&AudioTrack>,
        mut progress: F,
    ) -> Result<()>
    where
        F: FnMut(f32),
    {
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match self
            .transcode_burn_once(
                input,
                probe,
                max_height,
                output,
                subtitle_index,
                encoder,
                audio,
                &mut progress,
            )
            .await
        {
            Err(error) if encoder.is_hardware() => {
                tracing::warn!(
                    encoder = encoder.ffmpeg_name(),
                    %error,
                    "hardware subtitle burn-in failed; retrying on the CPU"
                );
                self.transcode_burn_once(
                    input,
                    probe,
                    max_height,
                    output,
                    subtitle_index,
                    VideoEncoder::Software,
                    audio,
                    &mut progress,
                )
                .await
            }
            result => result,
        }
    }

    /// One subtitle burn-in attempt with a specific encoder.
    #[allow(clippy::too_many_arguments)]
    async fn transcode_burn_once<F>(
        &self,
        input: &str,
        probe: &Probe,
        max_height: u32,
        output: &Path,
        subtitle_index: u32,
        encoder: VideoEncoder,
        audio: Option<&AudioTrack>,
        progress: &mut F,
    ) -> Result<()>
    where
        F: FnMut(f32),
    {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new(&self.ffmpeg)
            .args(convert::transcode_args_burn(
                input,
                probe,
                max_height,
                output,
                subtitle_index,
                encoder,
                audio,
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        // `-progress pipe:1` writes `key=value` lines; `out_time_us` is
        // microseconds of input consumed (the `_ms` key is microseconds too).
        if let Some(stdout) = child.stdout.take() {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(value) = line.strip_prefix("out_time_us=") {
                    if let Ok(micros) = value.trim().parse::<u64>() {
                        progress(micros as f32 / 1_000_000.0);
                    }
                }
            }
        }

        let mut child_err = child.stderr.take();
        let status = child.wait().await?;
        if !status.success() {
            let mut message = String::new();
            if let Some(child_err) = child_err.as_mut() {
                use tokio::io::AsyncReadExt;
                let _ = child_err.read_to_string(&mut message).await;
            }
            return Err(MediaError::Tool {
                tool: "ffmpeg",
                message: stderr(message.as_bytes()),
            });
        }
        Ok(())
    }

    /// Extract one subtitle stream to a standalone `.srt` file.    ///
    /// Used to hand embedded subtitles to a renderer that does not read them
    /// from the container. Image-based tracks (PGS/VobSub) cannot be converted
    /// to text and must be filtered out by the caller.
    pub async fn extract_subtitle(
        &self,
        input: &str,
        stream_index: u32,
        output: &Path,
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let result = tokio::process::Command::new(&self.ffmpeg)
            .args(convert::extract_subtitle_args(input, stream_index, output))
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        if !result.status.success() {
            return Err(MediaError::Tool {
                tool: "ffmpeg",
                message: stderr(&result.stderr),
            });
        }
        Ok(())
    }
}

/// Resolve one tool through override → bundled → `PATH`.
fn find(name: &str, override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return reel_core::process::is_executable(path).then(|| path.to_path_buf());
    }
    if let Some(dir) = Tools::bundled_dir() {
        for candidate in [dir.join(name), dir.join(format!("{name}.exe"))] {
            if reel_core::process::is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    reel_core::process::resolve_program(name)
}

fn stderr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "no error output".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MediaError;

    #[test]
    fn an_explicit_override_must_exist() {
        let result = Tools::resolve(Some(Path::new("/no/such/ffmpeg")), None);
        assert!(matches!(
            result,
            Err(MediaError::ToolMissing { tool: "ffmpeg" })
        ));
    }

    #[test]
    fn an_empty_override_is_not_a_path_lookup() {
        // A missing explicit path fails rather than silently falling back to PATH.
        let result = Tools::resolve(None, Some(Path::new("/nope/ffprobe")));
        assert!(result.is_err());
    }
}
