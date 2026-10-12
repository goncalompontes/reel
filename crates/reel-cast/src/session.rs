//! A casting session: a target plus a media item, with its live state.
//!
//! The session is deliberately protocol-agnostic — it drives a boxed
//! [`CastTarget`], so the same code will work for a future Google Cast target.
//! The desktop polls [`CastSession::poll`] about once a second for status; the
//! only other waiting is the short, bounded settle inside
//! [`CastSession::start`] and [`CastSession::stop`], which verifies the
//! renderer actually reached the state it was told to.

use std::time::Duration;

use crate::TransportState;
use crate::error::{CastError, Result};
use crate::target::CastTarget;

/// How long to give a renderer to reach an expected transport state.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long between transport-state checks while settling.
const SETTLE_INTERVAL: Duration = Duration::from_millis(200);

/// A position at or below this right after `Play` counts as "at the start".
/// Above it, the renderer resumed an old position and is seeked back.
const NEAR_START: Duration = Duration::from_secs(3);

/// Whether a transport state means "not playing" for our purposes.
fn is_stopped(state: TransportState) -> bool {
    matches!(
        state,
        TransportState::Stopped | TransportState::NoMediaPresent
    )
}

/// Whether a transport state means the renderer is ready to be seeked.
fn is_playable(state: TransportState) -> bool {
    matches!(
        state,
        TransportState::Playing
            | TransportState::PausedPlayback
            | TransportState::FastForward
            | TransportState::Rewind
    )
}

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
    /// The media's real duration, from the probe. A live stream (fragmented
    /// remux/transcode) reports its duration as "however much has played", so
    /// the renderer's value must not be trusted for the total or for deciding
    /// that playback reached the end.
    duration_hint: Option<Duration>,
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
            duration_hint: None,
        }
    }

    /// Record the media's true duration, so the reported total does not follow
    /// the playhead for a live stream.
    pub fn set_duration_hint(&mut self, duration: Option<Duration>) {
        self.duration_hint = duration;
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
        self.duration_hint.or(self.duration)
    }

    /// Hand the renderer a media URL plus its DIDL-Lite metadata and start it.
    pub async fn start(&mut self, media_url: impl Into<String>, metadata: &str) -> Result<()> {
        self.begin(media_url, metadata, None).await
    }

    /// Start media and hand playback over at `position`.
    ///
    /// Used when a title moves from this machine to the renderer: the renderer
    /// is started, then seeked to where the local player had got to.
    pub async fn start_at(
        &mut self,
        media_url: impl Into<String>,
        metadata: &str,
        position: Duration,
    ) -> Result<()> {
        self.begin(media_url, metadata, Some(position)).await
    }

    /// Queue the next media item for gapless playback (best-effort: a renderer
    /// that ignores `SetNextAVTransportURI` simply keeps playing the current
    /// item, and this reports the error to the caller).
    pub async fn set_next(&mut self, media_url: impl Into<String>, metadata: &str) -> Result<()> {
        self.target.set_next_uri(&media_url.into(), metadata).await
    }

    /// Start media, optionally at a handed-over position.
    async fn begin(
        &mut self,
        media_url: impl Into<String>,
        metadata: &str,
        start_at: Option<Duration>,
    ) -> Result<()> {
        self.media_url = media_url.into();
        self.position = None;
        self.duration = None;
        self.state = SessionState::Connecting;

        // Clear any previous media and verify the renderer really stopped before
        // loading the new URI. A renderer still playing — or wedged in
        // `TRANSITIONING` — can ignore the new URI and resume the previous
        // episode's position, which is the resume-across-casts bug this guards.
        self.stop_if_needed().await?;

        let result = match self.target.set_uri(&self.media_url, metadata).await {
            Ok(()) => self.target.play().await,
            Err(error) => Err(error),
        };

        // 701 again on `Play` often clears with another `Stop` and one retry.
        let result = match result {
            Err(CastError::Fault {
                code: Some(701), ..
            }) => {
                let _ = self.target.stop().await;
                let _ = self.settle_until(is_stopped).await;
                self.target.play().await
            }
            other => other,
        };

        let result = self.apply(result, SessionState::Playing);
        if result.is_ok() {
            self.settle_position(start_at).await;
        }
        result
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

    /// Whether the renderer exposes volume control.
    pub fn has_volume(&self) -> bool {
        self.target.has_volume()
    }

    /// The renderer's master volume, 0–100.
    pub async fn volume(&self) -> Result<u8> {
        self.target.volume().await
    }

    /// Set the renderer's master volume, 0–100.
    pub async fn set_volume(&mut self, volume: u8) -> Result<()> {
        self.target.set_volume(volume).await
    }

    /// Mute or unmute the renderer's master channel.
    pub async fn set_mute(&mut self, muted: bool) -> Result<()> {
        self.target.set_mute(muted).await
    }

    /// Stop playback, and verify the renderer actually stopped.
    ///
    /// The reply alone is not trusted: a renderer can accept `Stop` and keep
    /// playing, or fault (701) while wedged in `TRANSITIONING`. After the
    /// request the transport is checked, and success — with the position
    /// cleared — is only reported once it reads `STOPPED`.
    pub async fn stop(&mut self) -> Result<()> {
        // A renderer wedged in `TRANSITIONING` often faults on the first `Stop`;
        // one retry clears it.
        let request = match self.target.stop().await {
            Err(first) => {
                tracing::debug!(%first, "stop faulted; retrying once");
                self.target.stop().await
            }
            ok => ok,
        };

        // A refused `Stop` (both attempts faulted) will not settle, so decide
        // with a single check instead of waiting the timeout out.
        if let Err(error) = request {
            if self.transport_is_stopped().await {
                return self.confirm_stopped();
            }
            self.state = SessionState::Error(error.to_string());
            return Err(error);
        }

        // `Stop` was accepted; it may take a moment for the renderer to settle.
        match self.settle_until(is_stopped).await {
            Ok(state) if is_stopped(state) => self.confirm_stopped(),
            Ok(state) => {
                let error = CastError::Stuck {
                    expected: "stop",
                    reported: state.to_string(),
                };
                self.state = SessionState::Error(error.to_string());
                Err(error)
            }
            Err(error) => {
                self.state = SessionState::Error(error.to_string());
                Err(error)
            }
        }
    }

    /// Record a stop only after the renderer confirmed it.
    fn confirm_stopped(&mut self) -> Result<()> {
        self.position = None;
        self.state = SessionState::Stopped;
        Ok(())
    }

    /// Stop the renderer, unless it already reports stopped.
    async fn stop_if_needed(&mut self) -> Result<()> {
        if self.transport_is_stopped().await {
            return Ok(());
        }
        self.stop().await
    }

    /// Whether the renderer reports a stopped (or media-less) transport.
    async fn transport_is_stopped(&self) -> bool {
        matches!(self.transport_state().await, Ok(state) if is_stopped(state))
    }

    /// The renderer's current transport state.
    async fn transport_state(&self) -> Result<TransportState> {
        let info = self.target.transport_info().await?;
        Ok(TransportState::parse(&info.state))
    }

    /// Poll the transport until `done` accepts the state, or [`SETTLE_TIMEOUT`]
    /// passes; returns the last state the renderer reported.
    async fn settle_until(&self, done: impl Fn(TransportState) -> bool) -> Result<TransportState> {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let state = self.transport_state().await?;
            if done(state) || tokio::time::Instant::now() >= deadline {
                return Ok(state);
            }
            tokio::time::sleep(SETTLE_INTERVAL).await;
        }
    }

    /// Put the renderer at the wanted position once it is playing.
    ///
    /// `wanted` is a hand-off position from the local player. Without one, a
    /// renderer that kept an old transport position (the LG behaviour) is reset
    /// to the start. Only a materially non-zero difference is corrected, so a
    /// stream that already began at the right place is not disturbed.
    async fn settle_position(&mut self, wanted: Option<Duration>) {
        let Ok(info) = self.target.position_info().await else {
            return;
        };
        if let Some(rel) = info
            .rel_time
            .as_deref()
            .and_then(crate::soap::parse_rel_time)
        {
            self.position = Some(rel);
        }
        if let Some(duration) = info
            .track_duration
            .as_deref()
            .and_then(crate::soap::parse_rel_time)
        {
            self.duration = Some(duration);
        }

        let current = self.position.unwrap_or_default();
        let target = match wanted {
            // A hand-off position worth seeking to.
            Some(wanted) if wanted > NEAR_START => wanted,
            // Handing over from the very start: already there.
            Some(_) => return,
            // No hand-off: reset a stale position (the resume-across-casts bug).
            None if current > NEAR_START => Duration::ZERO,
            None => return,
        };
        if target == current {
            return;
        }

        // A renderer still `TRANSITIONING` may refuse the seek; give it a moment.
        let _ = self.settle_until(is_playable).await;
        tracing::info!(
            position = %crate::soap::format_rel_time(target),
            "seeking the renderer to the hand-off position"
        );
        // One retry: a renderer that has only just started can refuse the first.
        if self.target.seek(target).await.is_ok() || self.target.seek(target).await.is_ok() {
            self.position = Some(target);
        } else {
            tracing::warn!(
                position = %crate::soap::format_rel_time(target),
                "the renderer refused to seek to the hand-off position"
            );
        }
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
