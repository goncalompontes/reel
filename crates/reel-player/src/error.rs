/// Everything that can go wrong setting up or driving playback.
#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    /// libmpv could not be loaded, is too old, or is missing pieces.
    #[error("embedded playback unavailable: {0}")]
    EmbeddedUnavailable(String),
    /// libmpv was usable but rejected a call.
    #[error("mpv: {0}")]
    Mpv(String),
    /// Neither embedded nor external playback could be set up.
    #[error("no playback backend is available")]
    NoBackend,
    /// The external backend deliberately has no transport control: the user
    /// drives the player in its own window.
    #[error("the external player does not support transport control")]
    ExternalControl,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
