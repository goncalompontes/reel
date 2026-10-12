//! Google Cast / FCast casting for reel.
//!
//! A second [`reel_cast::CastTarget`] backend beside DLNA. The CASTV2/TLS
//! protocol and mDNS discovery live in `fcast-sender-sdk`; this crate adapts its
//! device API to the trait the session and UI layers already speak, so a cast to
//! a Chromecast behaves like a cast to a TV.
//!
//! Discovery is a long-lived mDNS browse ([`Discovery`]) that keeps a
//! name-keyed list of receivers; the desktop merges that list with its DLNA
//! renderers. A [`GcastTarget`] owns one connected device and mirrors the
//! receiver's state from the SDK's event callbacks, so the shared `CastSession`
//! polling works unchanged.

use std::collections::HashMap;
use std::net::IpAddr as StdIpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fcast_sender_sdk::context::CastContext;
use fcast_sender_sdk::device::{
    self as sdk, CastingDevice, DeviceConnectionState, DeviceEventHandler, DeviceFeature,
    LoadRequest, MediaTrack, MediaTrackType, Metadata, PlaybackState, ProtocolType, QueueState,
    ReceiverError, Source, TrackList,
};
use fcast_sender_sdk::DeviceDiscovererEventHandler;
use reel_cast::soap;
use reel_cast::target::CastTarget;
use reel_cast::{CastError, MediaInfo, PositionInfo, TransportInfo};

/// How often the SDK reports progress, in milliseconds.
const PROGRESS_INTERVAL_MS: u64 = 1000;

/// Reconnect interval passed to the SDK; `0` would disable reconnects.
const RECONNECT_MS: u64 = 3000;

/// How long to wait for the receiver to be connected before loading. The SDK
/// launches the Default Media Receiver lazily (polling every second, up to 15
/// attempts), so a cold receiver can take several seconds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);

/// How long to wait for the receiver to load the media before seeking. A seek
/// sent before the media session exists is ignored by the receiver.
const SEEK_TIMEOUT: Duration = Duration::from_secs(25);

/// How long to wait for the receiver to actually start playing after a load.
const START_TIMEOUT: Duration = Duration::from_secs(25);

/// How long to wait between `Play` retries while the media session settles. The
/// Default Media Receiver loads without autoplay, and a `Play` sent before its
/// media session exists is ignored, so the play is retried until it takes.
const START_STEP: Duration = Duration::from_millis(1500);

/// Polling step while waiting on the receiver.
const WAIT_STEP: Duration = Duration::from_millis(100);

/// Which casting protocol a device speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Google Cast (Chromecast, Google TV, Android TV with Cast).
    Chromecast,
    /// FCast, the open protocol (needs the FCast receiver app).
    FCast,
}

impl Protocol {
    /// A short label for the device picker.
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Chromecast => "Google Cast",
            Protocol::FCast => "FCast",
        }
    }
}

/// A discovered Google Cast / FCast receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct GcastDevice {
    /// Friendly name, from the mDNS `fn` record (Chromecast) or the service name.
    pub name: String,
    /// Stable id: the mDNS `id` record when present, else the name.
    pub id: String,
    /// Which protocol the device speaks.
    pub protocol: Protocol,
    /// The SDK device info, used to build a connected target.
    info: sdk::DeviceInfo,
}

impl GcastDevice {
    /// Build a device by hand, e.g. for "add by address". `port` is the CASTV2
    /// port (8009 for Google Cast). Connecting to a hand-built device requires a
    /// reachable address.
    pub fn manual(
        name: impl Into<String>,
        id: impl Into<String>,
        protocol: Protocol,
        addresses: Vec<StdIpAddr>,
        port: u16,
    ) -> GcastDevice {
        let name = name.into();
        let id = id.into();
        let addresses = addresses.into_iter().map(sdk_ip).collect();
        let txt_records = HashMap::new();
        let info = match protocol {
            Protocol::Chromecast => {
                sdk::DeviceInfo::chromecast(name.clone(), addresses, port, txt_records)
            }
            Protocol::FCast => sdk::DeviceInfo::fcast(name.clone(), addresses, port, txt_records),
        };
        GcastDevice {
            name,
            id,
            protocol,
            info,
        }
    }

    fn from_sdk(info: sdk::DeviceInfo) -> GcastDevice {
        let id = info
            .txt_records
            .get("id")
            .cloned()
            .unwrap_or_else(|| info.name.clone());
        let protocol = match info.protocol {
            ProtocolType::Chromecast => Protocol::Chromecast,
            ProtocolType::FCast => Protocol::FCast,
        };
        GcastDevice {
            name: info.name.clone(),
            id,
            protocol,
            info,
        }
    }
}

/// A running mDNS discovery, keeping the receivers found so far.
///
/// The browse has no natural end, so this is started once and lives for the
/// session; [`Discovery::devices`] returns the current list.
pub struct Discovery {
    context: Arc<CastContext>,
    devices: Arc<Mutex<Vec<GcastDevice>>>,
}

impl Discovery {
    /// Start discovery, or `None` when the SDK cannot start (no async runtime).
    pub fn start() -> Option<Discovery> {
        let context = Arc::new(CastContext::new().ok()?);
        let devices = Arc::new(Mutex::new(Vec::new()));
        context.start_discovery(Arc::new(DiscoveryHandler {
            devices: devices.clone(),
        }));
        tracing::info!("Google Cast / FCast discovery started");
        Some(Discovery { context, devices })
    }

    /// The receivers found so far.
    pub fn devices(&self) -> Vec<GcastDevice> {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Connect to a discovered device and build a target.
    pub fn target(&self, device: &GcastDevice) -> Result<GcastTarget, CastError> {
        let sdk_device = self.context.create_device_from_info(device.info.clone());
        GcastTarget::connect(sdk_device)
    }
}

/// Keeps the device list current as the SDK reports mDNS changes.
struct DiscoveryHandler {
    devices: Arc<Mutex<Vec<GcastDevice>>>,
}

impl DiscoveryHandler {
    fn upsert(&self, info: sdk::DeviceInfo) {
        let device = GcastDevice::from_sdk(info);
        let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = devices.iter_mut().find(|seen| seen.name == device.name) {
            *existing = device;
        } else {
            tracing::debug!(name = device.name, protocol = device.protocol.label(), "cast receiver found");
            devices.push(device);
        }
    }
}

impl DeviceDiscovererEventHandler for DiscoveryHandler {
    fn device_available(&self, device_info: sdk::DeviceInfo) {
        self.upsert(device_info);
    }

    fn device_changed(&self, device_info: sdk::DeviceInfo) {
        self.upsert(device_info);
    }

    fn device_removed(&self, device_name: String) {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|device| device.name != device_name);
    }
}

/// Playback state, mirrored from the SDK so it can be `Copy`ed into the status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Playback {
    #[default]
    Idle,
    Buffering,
    Playing,
    Paused,
    Ended,
}

impl Playback {
    fn from_sdk(state: PlaybackState) -> Playback {
        match state {
            PlaybackState::Idle => Playback::Idle,
            PlaybackState::Buffering => Playback::Buffering,
            PlaybackState::Playing => Playback::Playing,
            PlaybackState::Paused => Playback::Paused,
            PlaybackState::Ended => Playback::Ended,
        }
    }

    /// The UPnP transport state the shared session understands.
    fn transport(self) -> &'static str {
        match self {
            Playback::Idle => "NO_MEDIA_PRESENT",
            Playback::Buffering => "TRANSITIONING",
            Playback::Playing => "PLAYING",
            Playback::Paused => "PAUSED_PLAYBACK",
            Playback::Ended => "STOPPED",
        }
    }
}

/// The receiver's state, updated from the SDK's event callbacks.
#[derive(Debug, Default)]
struct SharedState {
    connected: bool,
    playback: Playback,
    position: f64,
    duration: f64,
    /// Volume as a fraction, 0.0–1.0 (the Cast scale).
    volume: f64,
    error: Option<String>,
}

/// Bridges the SDK's push callbacks into [`SharedState`].
struct CastEventHandler {
    state: Arc<Mutex<SharedState>>,
}

impl CastEventHandler {
    fn with(&self, f: impl FnOnce(&mut SharedState)) {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

impl DeviceEventHandler for CastEventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        let connected = matches!(state, DeviceConnectionState::Connected { .. });
        self.with(|shared| shared.connected = connected);
        tracing::debug!(?state, "cast connection state changed");
    }

    fn volume_changed(&self, volume: f64) {
        self.with(|shared| shared.volume = volume);
    }

    fn time_changed(&self, time: f64) {
        self.with(|shared| shared.position = time);
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        let playback = Playback::from_sdk(state);
        self.with(|shared| shared.playback = playback);
    }

    fn duration_changed(&self, duration: f64) {
        self.with(|shared| shared.duration = duration);
    }

    fn speed_changed(&self, _speed: f64) {}

    fn source_changed(&self, _source: Source) {}

    fn playback_stopped(&self) {
        self.with(|shared| {
            shared.playback = Playback::Idle;
            shared.position = 0.0;
        });
    }

    fn playback_error(&self, message: String) {
        self.with(|shared| shared.error = Some(message));
    }

    fn tracks_available(&self, _tracks: Vec<MediaTrack>) {}

    fn track_selected(&self, _id: Option<u32>, _typ: MediaTrackType) {}

    fn tracks_changed(&self, _tracks: TrackList) {}

    fn queue_changed(&self, _queue: QueueState) {}

    fn command_error(&self, error: ReceiverError) {
        self.with(|shared| shared.error = Some(format!("{error:?}")));
    }
}

/// A media item waiting for `play`.
struct Pending {
    url: String,
    title: String,
    content_type: String,
    /// A WebVTT sidecar URL to side-load as a subtitle track, if any.
    subtitle_url: Option<String>,
}

/// A connected Google Cast / FCast receiver, as a [`CastTarget`].
pub struct GcastTarget {
    device: Arc<dyn CastingDevice>,
    state: Arc<Mutex<SharedState>>,
    pending: Mutex<Option<Pending>>,
    /// Volume to restore when unmuting (the Cast scale, 0.0–1.0).
    unmute_volume: Mutex<f64>,
}

impl GcastTarget {
    /// Connect to an SDK device and start mirroring its state.
    pub fn connect(device: Arc<dyn CastingDevice>) -> Result<GcastTarget, CastError> {
        let state = Arc::new(Mutex::new(SharedState {
            volume: 1.0,
            ..SharedState::default()
        }));
        let handler = Arc::new(CastEventHandler {
            state: state.clone(),
        });
        device
            .connect(None, handler, RECONNECT_MS)
            .map_err(|error| command_error("connect", error))?;
        Ok(GcastTarget {
            device,
            state,
            pending: Mutex::new(None),
            unmute_volume: Mutex::new(1.0),
        })
    }

    fn snapshot(&self) -> SharedState {
        let shared = self.state.lock().unwrap_or_else(|e| e.into_inner());
        SharedState {
            connected: shared.connected,
            playback: shared.playback,
            position: shared.position,
            duration: shared.duration,
            volume: shared.volume,
            error: shared.error.clone(),
        }
    }

    /// Wait until the receiver is connected — the SDK has launched the media
    /// receiver and can accept a load. `load` is fire-and-forget and is silently
    /// dropped before this point, so a load issued earlier never plays.
    async fn wait_until_connected(&self, timeout: Duration) -> Result<(), CastError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.snapshot().connected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CastError::Command(
                    "the receiver did not connect in time".into(),
                ));
            }
            tokio::time::sleep(WAIT_STEP).await;
        }
    }

    /// Wait until the receiver has loaded the media (playback has begun). A seek
    /// sent before the media session exists carries session id 0 and is ignored.
    async fn wait_until_playable(&self, timeout: Duration) -> Result<(), CastError> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.snapshot();
            if matches!(state.playback, Playback::Playing | Playback::Paused) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CastError::Command(
                    "the receiver did not start playing in time".into(),
                ));
            }
            tokio::time::sleep(WAIT_STEP).await;
        }
    }

    /// Start playback after a load.
    ///
    /// The SDK sends the load with no `autoplay`, so the Default Media Receiver
    /// loads the media and stays idle; a `Play` is required. The media session
    /// id is set by the first `MEDIA_STATUS`, which can arrive just after the
    /// load, so `Play` is retried until playback actually starts.
    async fn start_playback(&self) -> Result<(), CastError> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            let _ = self.device.resume_playback();
            if self.wait_until_playable(START_STEP).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CastError::Command(
                    "the receiver did not start playing in time".into(),
                ));
            }
        }
    }
}

/// Convert a std IP address to the SDK's own address type.
fn sdk_ip(addr: StdIpAddr) -> fcast_sender_sdk::IpAddr {
    match addr {
        StdIpAddr::V4(v4) => {
            let [o1, o2, o3, o4] = v4.octets();
            fcast_sender_sdk::IpAddr::V4 { o1, o2, o3, o4 }
        }
        StdIpAddr::V6(v6) => {
            let o = v6.octets();
            fcast_sender_sdk::IpAddr::V6 {
                o1: o[0],
                o2: o[1],
                o3: o[2],
                o4: o[3],
                o5: o[4],
                o6: o[5],
                o7: o[6],
                o8: o[7],
                o9: o[8],
                o10: o[9],
                o11: o[10],
                o12: o[11],
                o13: o[12],
                o14: o[13],
                o15: o[14],
                o16: o[15],
                // A bare IPv6 address carries no scope id.
                scope_id: 0,
            }
        }
    }
}

/// Turn an SDK device error into a cast error with context.
fn command_error(action: &str, error: sdk::CastingDeviceError) -> CastError {
    CastError::Command(format!("{action}: {error:?}"))
}

#[async_trait]
impl CastTarget for GcastTarget {
    /// Store the media; the actual load happens on `play`, so a subsequent
    /// `resume` is not mistaken for a fresh load.
    async fn set_uri(&self, uri: &str, metadata: &str) -> Result<(), CastError> {
        let pending = Pending {
            url: uri.to_string(),
            title: title_from_didl(metadata),
            content_type: reel_cast::dlna::mime_for_url(uri).to_string(),
            subtitle_url: caption_from_didl(metadata),
        };
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending);
        Ok(())
    }

    async fn set_next_uri(&self, _uri: &str, _metadata: &str) -> Result<(), CastError> {
        // Queueing is FCast-v4-only; the default is "unsupported", which the
        // caller treats as best-effort.
        Err(CastError::Missing("SetNextAVTransportURI support".into()))
    }

    async fn play(&self) -> Result<(), CastError> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match pending {
            Some(pending) => {
                // The load is dropped unless the receiver app is up.
                self.wait_until_connected(CONNECT_TIMEOUT).await?;
                let request = LoadRequest::Video {
                    content_type: pending.content_type,
                    url: pending.url,
                    resume_position: 0.0,
                    speed: None,
                    volume: None,
                    metadata: Some(Metadata {
                        title: Some(pending.title),
                        thumbnail_url: None,
                    }),
                    request_headers: None,
                    subtitle_url: pending.subtitle_url,
                };
                self.device
                    .load(request, Some(PROGRESS_INTERVAL_MS))
                    .map_err(|error| command_error("load", error))?;
                // The load does not autoplay; start it explicitly.
                self.start_playback().await
            }
            None => self
                .device
                .resume_playback()
                .map_err(|error| command_error("resume", error)),
        }
    }

    async fn pause(&self) -> Result<(), CastError> {
        self.device
            .pause_playback()
            .map_err(|error| command_error("pause", error))
    }

    async fn stop(&self) -> Result<(), CastError> {
        self.device
            .stop_playback()
            .map_err(|error| command_error("stop", error))
    }

    async fn seek(&self, position: Duration) -> Result<(), CastError> {
        // A seek before the media is loaded is ignored, which is what broke the
        // "resume where you left off" hand-off.
        self.wait_until_playable(SEEK_TIMEOUT).await?;
        self.device
            .seek(position.as_secs_f64())
            .map_err(|error| command_error("seek", error))
    }

    async fn transport_info(&self) -> Result<TransportInfo, CastError> {
        let state = self.snapshot();
        Ok(TransportInfo {
            state: state.playback.transport().to_string(),
            status: Some(if state.error.is_some() { "ERROR_OCCURRED" } else { "OK" }.to_string()),
            speed: Some("1".to_string()),
        })
    }

    async fn position_info(&self) -> Result<PositionInfo, CastError> {
        let state = self.snapshot();
        Ok(PositionInfo {
            track: Some(1),
            track_duration: Some(soap::format_rel_time(seconds(state.duration))),
            rel_time: Some(soap::format_rel_time(seconds(state.position))),
            abs_time: None,
            track_uri: None,
        })
    }

    async fn media_info(&self) -> Result<MediaInfo, CastError> {
        let state = self.snapshot();
        Ok(MediaInfo {
            nr_tracks: Some(1),
            media_duration: Some(soap::format_rel_time(seconds(state.duration))),
            current_uri: None,
        })
    }

    async fn set_volume(&self, volume: u8) -> Result<(), CastError> {
        let level = f64::from(volume.min(100)) / 100.0;
        self.device
            .change_volume(level)
            .map_err(|error| command_error("set volume", error))
    }

    fn has_volume(&self) -> bool {
        self.device.supports_feature(DeviceFeature::SetVolume)
    }

    async fn volume(&self) -> Result<u8, CastError> {
        Ok((self.snapshot().volume * 100.0).round().clamp(0.0, 100.0) as u8)
    }

    /// The Cast protocol has no mute; this emulates it by dropping the volume to
    /// zero and restoring it on unmute.
    async fn set_mute(&self, muted: bool) -> Result<(), CastError> {
        let mut restore = self.unmute_volume.lock().unwrap_or_else(|e| e.into_inner());
        if muted {
            *restore = self.snapshot().volume;
            self.device
                .change_volume(0.0)
                .map_err(|error| command_error("mute", error))
        } else {
            let level = *restore;
            self.device
                .change_volume(level)
                .map_err(|error| command_error("unmute", error))
        }
    }
}

/// A non-negative duration from a seconds value.
fn seconds(value: f64) -> Duration {
    Duration::from_secs_f64(value.max(0.0))
}

/// Pull the title out of the DIDL-Lite metadata the caller already builds.
fn title_from_didl(metadata: &str) -> String {
    const OPEN: &str = "<dc:title>";
    const CLOSE: &str = "</dc:title>";
    if let Some(start) = metadata.find(OPEN) {
        let rest = &metadata[start + OPEN.len()..];
        if let Some(end) = rest.find(CLOSE) {
            return unescape(&rest[..end]);
        }
    }
    "reel".to_string()
}

/// Pull the subtitle sidecar URL out of the DIDL-Lite metadata, if any. The
/// caller advertises it as `<sec:CaptionInfoEx …>URL</sec:CaptionInfoEx>`.
fn caption_from_didl(metadata: &str) -> Option<String> {
    const OPEN: &str = "<sec:CaptionInfoEx";
    let start = metadata.find(OPEN)?;
    let rest = &metadata[start..];
    let gt = rest.find('>')?;
    let after = &rest[gt + 1..];
    let end = after.find("</sec:CaptionInfoEx>")?;
    let url = unescape(&after[..end]);
    (!url.is_empty()).then_some(url)
}

/// Undo the XML escaping `reel_cast::didl` applies to the title.
fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_playback_to_transport_states() {
        assert_eq!(Playback::Idle.transport(), "NO_MEDIA_PRESENT");
        assert_eq!(Playback::Playing.transport(), "PLAYING");
        assert_eq!(Playback::Paused.transport(), "PAUSED_PLAYBACK");
        assert_eq!(Playback::Buffering.transport(), "TRANSITIONING");
        assert_eq!(Playback::Ended.transport(), "STOPPED");
        // The shared session must recognise them.
        assert!(reel_cast::TransportState::parse(Playback::Playing.transport()).is_playing());
        assert!(matches!(
            reel_cast::TransportState::parse(Playback::Ended.transport()),
            reel_cast::TransportState::Stopped
        ));
    }

    #[test]
    fn extracts_the_caption_url_from_didl() {
        let didl = r#"<DIDL-Lite><item><res protocolInfo="http-get:*:video/mp4:*">http://h/x.mp4</res><sec:CaptionInfoEx sec:type="srt">http://h/sub.vtt</sec:CaptionInfoEx></item></DIDL-Lite>"#;
        assert_eq!(
            caption_from_didl(didl).as_deref(),
            Some("http://h/sub.vtt")
        );
        assert_eq!(caption_from_didl("<DIDL-Lite/>"), None);
    }

    #[test]
    fn extracts_the_title_from_didl() {
        let didl = r#"<DIDL-Lite><item><dc:title>Tom &amp; Jerry</dc:title></item></DIDL-Lite>"#;
        assert_eq!(title_from_didl(didl), "Tom & Jerry");
        assert_eq!(title_from_didl("<DIDL-Lite/>"), "reel");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(soap::format_rel_time(seconds(65.4)), "0:01:05");
        assert_eq!(soap::format_rel_time(seconds(-3.0)), "0:00:00");
    }
}
