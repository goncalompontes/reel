//! Finding and running ffmpeg and ffprobe.
//!
//! Resolution order: an explicit override, then a copy bundled next to the reel
//! executable, then `PATH`. When both tools are absent, casting is disabled
//! rather than failing to build — see [`Tools::try_resolve`].

use std::path::{Path, PathBuf};

use crate::convert;
use crate::error::{MediaError, Result};
use crate::probe::Probe;

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

    /// Inspect a file or stream URL.
    pub async fn probe(&self, input: &str) -> Result<Probe> {
        let output = tokio::process::Command::new(&self.ffprobe)
            .args(convert::probe_args(input))
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
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
    pub fn spawn_remux(&self, input: &str) -> Result<tokio::process::Child> {
        tokio::process::Command::new(&self.ffmpeg)
            .args(convert::remux_stream_args(input))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(Into::into)
    }

    /// Run a full conversion to `output`, waiting for it to finish.
    pub async fn transcode(
        &self,
        input: &str,
        probe: &Probe,
        max_height: u32,
        output: &Path,
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let result = tokio::process::Command::new(&self.ffmpeg)
            .args(convert::transcode_args(input, probe, max_height, output))
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
