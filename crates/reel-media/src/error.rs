//! Errors for media inspection and conversion.

use thiserror::Error;

/// Result alias for media operations.
pub type Result<T, E = MediaError> = std::result::Result<T, E>;

/// Anything that can go wrong probing or converting media.
#[derive(Debug, Error)]
pub enum MediaError {
    /// A process could not be spawned or a file could not be read.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// ffprobe produced output we could not read.
    #[error("could not parse ffprobe output: {0}")]
    Json(#[from] serde_json::Error),

    /// A required tool was not found.
    #[error("{tool} was not found (install it, or point reel at a bundled copy)")]
    ToolMissing {
        /// `ffmpeg` or `ffprobe`.
        tool: &'static str,
    },

    /// A tool ran but failed.
    #[error("{tool} failed: {message}")]
    Tool {
        /// `ffmpeg` or `ffprobe`.
        tool: &'static str,
        /// Captured stderr, trimmed.
        message: String,
    },
}
