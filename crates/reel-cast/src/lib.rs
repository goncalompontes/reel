//! DLNA / UPnP-AV casting for `reel`.
//!
//! This crate is the controller half of casting a torrent to a smart TV with no
//! TV-side app:
//!
//! 1. [`ssdp::discover`] finds `MediaRenderer`s on the LAN.
//! 2. [`device::parse_description`] turns each device description into its
//!    service endpoints.
//! 3. [`didl`] builds the metadata and [`dlna`] the `protocolInfo` for the
//!    media URL that `reel-http` will serve on the LAN.
//! 4. [`soap`] builds the `AVTransport` calls (`SetAVTransportURI`, `Play`,
//!    `Seek`, …) and parses the replies.
//!
//! It is transport- and UI-free: no `reqwest`, no egui. The HTTP client that
//! POSTs those SOAP documents lives in the layer above (see the casting plan),
//! which keeps the wire formats here unit-testable without a network.

pub mod device;
pub mod didl;
pub mod dlna;
pub mod error;
pub mod soap;
pub mod ssdp;
pub mod xml;

pub use device::{DeviceDescription, Service};
pub use error::{CastError, Result};
pub use soap::{MediaInfo, PositionInfo, SoapFault, SoapRequest, TransportInfo};
pub use ssdp::SsdpResponse;

/// UPnP `AVTransport` transport states, as reported by `GetTransportInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    /// Not playing; may or may not have media loaded.
    Stopped,
    /// Buffering or switching media.
    Transitioning,
    /// Media is loaded but not started.
    NoMediaPresent,
    /// Playing at normal speed.
    Playing,
    /// Paused by the user.
    PausedPlayback,
    /// Paused because the recording is being written.
    PausedRecording,
    /// Recording.
    Recording,
    /// Playing at 1x or faster.
    FastForward,
    /// Rewinding.
    Rewind,
    /// A state this crate does not model.
    Unknown,
}

impl TransportState {
    /// Parse the `CurrentTransportState` text.
    pub fn parse(value: &str) -> TransportState {
        match value.trim() {
            "STOPPED" => TransportState::Stopped,
            "TRANSITIONING" => TransportState::Transitioning,
            "NO_MEDIA_PRESENT" => TransportState::NoMediaPresent,
            "PLAYING" => TransportState::Playing,
            "PAUSED_PLAYBACK" => TransportState::PausedPlayback,
            "PAUSED_RECORDING" => TransportState::PausedRecording,
            "RECORDING" => TransportState::Recording,
            "FAST_FORWARD" => TransportState::FastForward,
            "REWIND" => TransportState::Rewind,
            _ => TransportState::Unknown,
        }
    }

    /// Whether the renderer is actively playing.
    pub fn is_playing(self) -> bool {
        matches!(
            self,
            TransportState::Playing | TransportState::FastForward | TransportState::Rewind
        )
    }
}

impl std::fmt::Display for TransportState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            TransportState::Stopped => "STOPPED",
            TransportState::Transitioning => "TRANSITIONING",
            TransportState::NoMediaPresent => "NO_MEDIA_PRESENT",
            TransportState::Playing => "PLAYING",
            TransportState::PausedPlayback => "PAUSED_PLAYBACK",
            TransportState::PausedRecording => "PAUSED_RECORDING",
            TransportState::Recording => "RECORDING",
            TransportState::FastForward => "FAST_FORWARD",
            TransportState::Rewind => "REWIND",
            TransportState::Unknown => "UNKNOWN",
        };
        f.write_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transport_states() {
        assert_eq!(TransportState::parse("PLAYING"), TransportState::Playing);
        assert_eq!(
            TransportState::parse("PAUSED_PLAYBACK"),
            TransportState::PausedPlayback
        );
        assert_eq!(TransportState::parse("WAT"), TransportState::Unknown);
        assert!(TransportState::Playing.is_playing());
        assert!(!TransportState::Stopped.is_playing());
        assert_eq!(TransportState::Playing.to_string(), "PLAYING");
    }
}
