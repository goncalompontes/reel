//! A casting session: a target plus a media item, with its live state.
//!
//! The session is deliberately protocol-agnostic — it drives a boxed
//! [`CastTarget`], so the same code will work for a future Google Cast target.
//! It holds no timers: the caller decides when to [`CastSession::poll`] (the
//! desktop polls about once a second while casting).

use std::time::Duration;

use crate::TransportState;
use crate::error::Result;
use crate::target::CastTarget;

/// What a session is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// No media has been handed to the renderer yet.
    Idle,
    /// The renderer is loading, buffering or switching media.
    Connecting,
    /// Playing at normal (or scan) speed.
    Playing,
    /// Paused by the user.
    Paused,
    /// Stopped; the media may still be loaded.
    Stopped,
    /// The last operation failed; the string is the reason.
    Error(String),
}

impl SessionState {
    /// Whether the renderer is actively playing.
    pub fn is_playing(&self) -> bool {
        matches!(self, SessionState::Playing)
    }

    /// Whether the session is in a failed state.
    pub fn is_error(&self) -> bool {
        matches!(self, SessionState::Error(_))
    }

    /// Map a reported transport state, if it is one we model.
    fn from_transport(state: TransportState) -> Option<SessionState> {
        match state {
            TransportState::Playing | TransportState::FastForward | TransportState::Rewind => {
                Some(SessionState::Playing)
            }
            TransportState::PausedPlayback | TransportState::PausedRecording => {
                Some(SessionState::Paused)
            }
            TransportState::Stopped | TransportState::NoMediaPresent => Some(SessionState::Stopped),
            TransportState::Transitioning => Some(SessionState::Connecting),
            // Recording is not a reel use case, and Unknown tells us nothing.
            TransportState::Recording | TransportState::Unknown => None,
        }
    }
}

/// An active (or recently active) cast.
pub struct CastSession {
    target: Box<dyn CastTarget>,
    title: String,
    media_url: String,
    state: SessionState,
    position: Option<Duration>,
    duration: Option<Duration>,
}

impl CastSession {
    /// Wrap a target for a titled item. The media URL is set by [`Self::start`].
    pub fn new(target: Box<dyn CastTarget>, title: impl Into<String>) -> CastSession {
        CastSession {
            target,
            title: title.into(),
            media_url: String::new(),
            state: SessionState::Idle,
            position: None,
            duration: None,
        }
    }

    /// The title being cast.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The media URL last handed to the renderer.
    pub fn media_url(&self) -> &str {
        &self.media_url
    }

    /// The current state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Last known playback position.
    pub fn position(&self) -> Option<Duration> {
        self.position
    }

    /// Last known media duration.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Hand the renderer a media URL plus its DIDL-Lite metadata and start it.
    pub async fn start(&mut self, media_url: impl Into<String>, metadata: &str) -> Result<()> {
        self.media_url = media_url.into();
        self.state = SessionState::Connecting;
        let result = match self.target.set_uri(&self.media_url, metadata).await {
            Ok(()) => self.target.play().await,
            Err(error) => Err(error),
        };
        self.apply(result, SessionState::Playing)
    }

    /// Pause playback.
    pub async fn pause(&mut self) -> Result<()> {
        let result = self.target.pause().await;
        self.apply(result, SessionState::Paused)
    }

    /// Resume playback.
    pub async fn resume(&mut self) -> Result<()> {
        let result = self.target.play().await;
        self.apply(result, SessionState::Playing)
    }

    /// Stop playback.
    pub async fn stop(&mut self) -> Result<()> {
        let result = self.target.stop().await;
        self.position = None;
        self.apply(result, SessionState::Stopped)
    }

    /// Seek to an absolute offset.
    pub async fn seek(&mut self, position: Duration) -> Result<()> {
        let result = self.target.seek(position).await;
        if result.is_ok() {
            self.position = Some(position);
        }
        self.apply(result, self.state.clone())
    }

    /// Refresh state, position and duration from the renderer.
    pub async fn poll(&mut self) -> Result<()> {
        let info = match self.target.transport_info().await {
            Ok(info) => info,
            Err(error) => {
                self.state = SessionState::Error(error.to_string());
                return Err(error);
            }
        };
        if let Some(state) = SessionState::from_transport(TransportState::parse(&info.state)) {
            self.state = state;
        }
        if let Ok(position) = self.target.position_info().await {
            self.position = position
                .rel_time
                .as_deref()
                .and_then(crate::soap::parse_rel_time);
            self.duration = position
                .track_duration
                .as_deref()
                .and_then(crate::soap::parse_rel_time);
        }
        Ok(())
    }

    /// Record the outcome of an operation and its success state.
    fn apply(&mut self, result: Result<()>, ok_state: SessionState) -> Result<()> {
        match result {
            Ok(()) => {
                self.state = ok_state;
                Ok(())
            }
            Err(error) => {
                self.state = SessionState::Error(error.to_string());
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_transport_states() {
        assert_eq!(
            SessionState::from_transport(TransportState::Playing),
            Some(SessionState::Playing)
        );
        assert_eq!(
            SessionState::from_transport(TransportState::PausedPlayback),
            Some(SessionState::Paused)
        );
        assert_eq!(SessionState::from_transport(TransportState::Unknown), None);
        assert!(SessionState::Playing.is_playing());
        assert!(SessionState::Error("x".into()).is_error());
    }
}
