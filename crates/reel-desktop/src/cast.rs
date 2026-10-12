//! Casting a title to a DLNA renderer on the local network.
//!
//! [`CastService`] owns the pieces the rest of the app should not see:
//!
//! * a LAN-bound, token-gated media server ([`reel_http::build_media_router`]),
//! * ffmpeg/ffprobe ([`reel_media`]) used to inspect and, if needed, convert,
//! * SSDP discovery and the [`CastSession`] that drives the renderer.
//!
//! The desktop UI is immediate-mode and must never block, so every network or
//! ffmpeg operation is spawned onto the backend runtime; the UI polls
//! [`CastService::devices`] and [`CastService::status`] snapshots and calls the
//! `&self` methods to act.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reel_cast::session::{CastSession, SessionState};
use reel_cast::{CastDevice, CastTarget, DlnaTarget, didl, discover_all, probe_address};
use reel_cast_gcast::{Discovery, GcastDevice};
use reel_catalog::{CatalogSettings, ConversionMode};
use reel_core::Engine;
use reel_http::{MediaState, build_media_router};
use reel_media::{
    AudioTrack, AvailableEncoders, CacheKey, ConversionCache, MediaPlan, PlanOptions, SinkSupport,
    Tools, TranscodeMode, VideoEncoder, plan,
};
use reel_catalog::HwAccel;

/// How long an SSDP search waits for replies.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

/// How often an active cast is polled for position/state.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How many consecutive poll failures before a cast is called lost. Renderers
/// drop the odd connection; a few misses in a row is a real problem.
const MAX_POLL_FAILURES: u32 = 5;

/// Cap on the conversion cache.
const CACHE_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Notified with the last position and duration when a cast ends — the user
/// stopped it, or the renderer did — so the app can hand playback back to this
/// machine. Kept as a callback so this module stays free of backend types.
pub type EndedHook = Arc<dyn Fn(Option<Duration>, Option<Duration>) + Send + Sync>;

/// Which casting protocol a device speaks, for the picker and for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    /// DLNA/UPnP-AV (a smart TV).
    Dlna,
    /// Google Cast (Chromecast, Google TV).
    GoogleCast,
    /// FCast (the open protocol; needs its receiver app).
    FCast,
}

impl CastKind {
    /// A short label for the device picker.
    pub fn label(self) -> &'static str {
        match self {
            CastKind::Dlna => "DLNA",
            CastKind::GoogleCast => "Google Cast",
            CastKind::FCast => "FCast",
        }
    }
}

/// A renderer as the UI needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Friendly name.
    pub name: String,
    /// Model name, when advertised.
    pub model: Option<String>,
    /// Stable unique device name.
    pub udn: String,
    /// Which protocol the device speaks.
    pub kind: CastKind,
}

/// A discovered renderer, DLNA or Google Cast/FCast.
#[derive(Clone)]
enum Discovered {
    Dlna(CastDevice),
    Gcast(GcastDevice),
}

impl Discovered {
    fn name(&self) -> &str {
        match self {
            Discovered::Dlna(device) => device.name(),
            Discovered::Gcast(device) => &device.name,
        }
    }

    fn id(&self) -> &str {
        match self {
            Discovered::Dlna(device) => device.udn(),
            Discovered::Gcast(device) => &device.id,
        }
    }

    fn model(&self) -> Option<&str> {
        match self {
            Discovered::Dlna(device) => device.model(),
            Discovered::Gcast(_) => None,
        }
    }

    fn kind(&self) -> CastKind {
        match self {
            Discovered::Dlna(_) => CastKind::Dlna,
            Discovered::Gcast(device) => match device.protocol {
                reel_cast_gcast::Protocol::Chromecast => CastKind::GoogleCast,
                reel_cast_gcast::Protocol::FCast => CastKind::FCast,
            },
        }
    }
}

impl From<&Discovered> for DeviceInfo {
    fn from(device: &Discovered) -> DeviceInfo {
        DeviceInfo {
            name: device.name().to_string(),
            model: device.model().map(str::to_string),
            udn: device.id().to_string(),
            kind: device.kind(),
        }
    }
}

/// How far along a cast is before the renderer plays, so the UI can tell a
/// short wait from a stuck one instead of showing a bare spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparePhase {
    /// Contacting the renderer and resolving the media URL.
    Connecting,
    /// Waiting for the title's torrent to deliver bytes.
    WaitingForStream,
    /// Inspecting the file and asking the renderer what it can play.
    Checking,
    /// Remuxing or re-encoding for the renderer. `percent` when knowable.
    Preparing { percent: Option<u8> },
    /// Handing the URL to the renderer and starting playback.
    Starting,
}

/// What the UI shows about the current cast.
#[derive(Debug, Clone, PartialEq)]
pub enum CastStatus {
    /// Getting the media ready before the renderer can play it.
    Preparing {
        /// Target device name.
        device: String,
        /// Title being cast.
        title: String,
        /// How far along the preparation is.
        phase: PreparePhase,
    },
    /// The renderer is playing (or paused/stopped).
    Active {
        /// Target device name.
        device: String,
        /// Title being cast.
        title: String,
        /// Transport state.
        state: SessionState,
        /// Last known position.
        position: Option<Duration>,
        /// Media duration, when known.
        duration: Option<Duration>,
        /// Master volume, 0–100. `None` when the renderer has no volume control.
        volume: Option<u8>,
    },
    /// The last operation failed.
    Failed {
        /// A user-facing message.
        message: String,
    },
}

/// An audio track the current cast can deliver, for the remote's picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrackInfo {
    /// The ffmpeg stream index; the value `start_cast` takes to select it.
    pub index: u32,
    /// A human label, e.g. "English · ac3".
    pub label: String,
    /// Whether this is the track currently being cast.
    pub selected: bool,
}

/// A control action against the active cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastAction {
    /// Pause playback.
    Pause,
    /// Resume playback.
    Resume,
    /// Stop and end the cast.
    Stop,
    /// Seek to an absolute offset.
    Seek(Duration),
    /// Set the renderer's master volume, 0–100.
    SetVolume(u8),
    /// Mute or unmute the renderer's master channel.
    SetMute(bool),
}

#[derive(Default)]
struct Inner {
    devices: Vec<Discovered>,
    status: Option<CastStatus>,
    discovering: bool,
    /// The device last cast to, kept even when a discovery pass comes back
    /// empty so a transient drop cannot fail an in-flight cast.
    last_device: Option<Discovered>,
    /// Bumped on every new cast, so an older poll loop stops.
    generation: u64,
    /// The audio tracks of the current cast, for the remote's picker.
    audio_tracks: Vec<AudioTrackInfo>,
}

/// The casting service. `None` from [`CastService::new`] means casting is off
/// or unavailable (see the returned reason).
pub struct CastService {
    engine: Arc<Engine>,
    lan_base: String,
    token: String,
    tools: Tools,
    cache: ConversionCache,
    /// Conversion policy, swappable while running.
    options: Mutex<PlanOptions>,
    /// Force H.265/AC3 passthrough, ignoring the renderer's advertised support.
    passthrough: Mutex<bool>,
    /// Trust HEVC even when the renderer does not advertise it.
    trust_hevc: Mutex<bool>,
    /// The hardware H.264 encoders this machine's ffmpeg lists.
    available_encoders: AvailableEncoders,
    /// The encoder used for the next re-encode. `Auto` resolves to a hardware
    /// encoder when one is listed, else software; a failed attempt retries on
    /// the CPU (`reel-media` handles that).
    encoder: Mutex<VideoEncoder>,
    loopback_base: String,
    runtime: tokio::runtime::Handle,
    inner: Arc<Mutex<Inner>>,
    session: Arc<tokio::sync::Mutex<Option<CastSession>>>,
    /// Resolving this stops the LAN media server.
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// MP4 sample-entry tag for the current live remux (`hvc1` for HEVC), set
    /// when a cast is prepared and read by the live source.
    remux_tag: Arc<Mutex<Option<String>>>,
    /// The audio stream index the current live remux must map, set when a cast
    /// is prepared and read by the live source (like `remux_tag`).
    remux_audio: Arc<Mutex<Option<u32>>>,
    /// What each renderer accepts, keyed by UDN. A renderer's capabilities do
    /// not change within its session, so they are asked once and reused instead
    /// of being re-queried on every cast (including every episode switch).
    capabilities: Arc<Mutex<HashMap<String, SinkSupport>>>,
    /// Called when a cast ends, so playback can resume locally.
    on_ended: EndedHook,
    /// Long-lived Google Cast / FCast discovery, when the SDK could start.
    discovery: Option<Arc<Discovery>>,
}

impl CastService {
    /// Start the LAN media server and prepare the conversion tools.
    ///
    /// Returns `Err` with a user-facing reason when casting cannot run — the
    /// setting is off, ffmpeg is missing, or there is no LAN address.
    pub fn new(
        engine: Arc<Engine>,
        loopback_base: String,
        data_dir: PathBuf,
        settings: &CatalogSettings,
        runtime: tokio::runtime::Handle,
        on_ended: EndedHook,
    ) -> Result<CastService, String> {
        if !settings.allow_lan_streaming {
            return Err("streaming to the local network is turned off".to_string());
        }

        let ffmpeg_override = settings.ffmpeg_path.as_deref().map(std::path::Path::new);
        let tools = Tools::resolve(ffmpeg_override, None)
            .map_err(|error| format!("casting needs ffmpeg: {error}"))?;

        // Ask ffmpeg once which hardware encoders it lists, and resolve the
        // configured preference to one that is usable. Detection spawns ffmpeg,
        // which is fine here: `new` already blocks on binding the listener.
        let available_encoders = tools.available_encoders();
        let encoder = available_encoders.choose(encoder_preference(settings.conversion_hwaccel));
        tracing::info!(
            encoder = encoder.ffmpeg_name(),
            hardware = encoder.is_hardware(),
            listed_hardware = available_encoders.any_hardware(),
            "video encoder for casting"
        );

        let address = lan_address(settings)
            .ok_or_else(|| "no local network address to advertise".to_string())?;

        let token = random_token();
        let cache = ConversionCache::new(data_dir.join("cast-cache"), CACHE_LIMIT_BYTES);

        // Google Cast / FCast discovery runs for the session; it is independent
        // of the DLNA SSDP search below.
        let discovery = Discovery::start().map(Arc::new);
        if discovery.is_none() {
            tracing::warn!(
                "Google Cast discovery could not start; only DLNA renderers will be found"
            );
        }

        let listener = runtime
            .block_on(reel_http::bind(SocketAddr::new(
                Ipv4Addr::UNSPECIFIED.into(),
                settings.cast_media_port,
            )))
            .map_err(|error| format!("could not open a LAN media port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("could not read the LAN media port: {error}"))?
            .port();
        let lan_base = format!("http://{address}:{port}");

        // Live remux: ffmpeg reads the loopback stream and writes a fragmented
        // MP4, which the LAN endpoint streams and tees into the cache.
        let remux_tag: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let remux_audio: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let live_source: reel_http::LiveSource = {
            let engine = engine.clone();
            let ffmpeg = tools.ffmpeg.clone();
            let cache = cache.clone();
            let loopback = loopback_base.clone();
            let remux_tag = remux_tag.clone();
            let remux_audio = remux_audio.clone();
            Arc::new(move |id: usize, file_id: usize| {
                let length = engine.probe_file(id, file_id).ok()?.length;
                let info_hash = engine.view(id).ok()?.info_hash;
                let audio = *remux_audio.lock().unwrap_or_else(|e| e.into_inner());
                let key = CacheKey::for_plan(
                    info_hash,
                    file_id as u64,
                    length,
                    &MediaPlan::Remux,
                    audio,
                );
                let source = format!("{loopback}/stream/{id}/{file_id}");
                let tag = remux_tag.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let mut command = tokio::process::Command::new(&ffmpeg);
                command.args(reel_media::convert::remux_stream_args(
                    &source,
                    tag.as_deref(),
                    audio,
                ));
                Some(reel_http::LiveSession {
                    command,
                    tee: Some(cache.path_for(&key)),
                })
            })
        };

        let media = MediaState::new(
            engine.clone(),
            token.clone(),
            cache.root().to_path_buf(),
            reel_cast::dlna::content_features(true, false),
        )
        .with_live_source(live_source);
        let router = build_media_router(media);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        runtime.spawn(async move {
            let _ = reel_http::serve_router_with_shutdown(listener, router, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });
        tracing::info!(%lan_base, "LAN media server ready for casting");

        Ok(CastService {
            engine,
            lan_base,
            token,
            tools,
            cache,
            options: Mutex::new(plan_options(settings)),
            passthrough: Mutex::new(settings.allow_hevc_passthrough),
            trust_hevc: Mutex::new(settings.cast_trust_hevc),
            available_encoders,
            encoder: Mutex::new(encoder),
            loopback_base,
            runtime,
            inner: Arc::new(Mutex::new(Inner::default())),
            session: Arc::new(tokio::sync::Mutex::new(None)),
            shutdown: Mutex::new(Some(shutdown_tx)),
            remux_tag,
            remux_audio,
            capabilities: Arc::new(Mutex::new(HashMap::new())),
            on_ended,
            discovery,
        })
    }

    /// Replace the conversion policy used for the next cast.
    pub fn set_options(&self, options: PlanOptions) {
        *self.options.lock().unwrap_or_else(|e| e.into_inner()) = options;
    }

    /// Force H.265/AC-3 passthrough, ignoring the renderer's advertised support.
    pub fn set_passthrough(&self, on: bool) {
        *self.passthrough.lock().unwrap_or_else(|e| e.into_inner()) = on;
    }

    /// Trust HEVC even when the renderer does not advertise it.
    pub fn set_trust_hevc(&self, on: bool) {
        *self.trust_hevc.lock().unwrap_or_else(|e| e.into_inner()) = on;
    }

    /// Apply a new video-encoding preference, resolved against what ffmpeg
    /// listed at startup.
    pub fn set_hwaccel(&self, hwaccel: HwAccel) {
        let encoder = self.available_encoders.choose(encoder_preference(hwaccel));
        tracing::info!(encoder = encoder.ffmpeg_name(), "video encoder changed");
        *self.encoder.lock().unwrap_or_else(|e| e.into_inner()) = encoder;
    }

    /// The encoder that will be used for the next re-encode, for Settings.
    pub fn encoder_label(&self) -> String {
        self.encoder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .label()
            .to_string()
    }

    /// Stop the LAN media server and end any cast in progress.
    pub fn shutdown(&self) {
        self.dismiss();
        if let Some(sender) = self
            .shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = sender.send(());
        }
    }

    /// The LAN base URL that cast URLs are built from.
    pub fn lan_base(&self) -> &str {
        &self.lan_base
    }

    /// The per-run media token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Discovered renderers, plus the last-connected one even if a discovery
    /// pass missed it — so a device that was just chosen never vanishes.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut devices: Vec<DeviceInfo> = inner.devices.iter().map(DeviceInfo::from).collect();
        // Google Cast / FCast receivers come from the long-lived mDNS browse,
        // which updates on its own; the DLNA list comes from the SSDP search.
        if let Some(discovery) = &self.discovery {
            for device in discovery.devices() {
                let discovered = Discovered::Gcast(device);
                if !devices.iter().any(|seen| seen.udn == discovered.id()) {
                    devices.push(DeviceInfo::from(&discovered));
                }
            }
        }
        if let Some(last) = &inner.last_device {
            if !devices.iter().any(|device| device.udn == last.id()) {
                devices.push(DeviceInfo::from(last));
            }
        }
        devices
    }

    /// Whether a discovery search is running.
    pub fn discovering(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .discovering
    }

    /// The audio tracks of the current cast, for the remote's picker. Empty
    /// when nothing is cast or the file has no audio.
    pub fn audio_tracks(&self) -> Vec<AudioTrackInfo> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .audio_tracks
            .clone()
    }

    /// The current cast status, if any.
    pub fn status(&self) -> Option<CastStatus> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    /// Start an SSDP search in the background, replacing the device list.
    ///
    /// A pass that finds nothing leaves the previous list in place: discovery is
    /// lossy, and emptying the list would drop the device a cast is using.
    pub fn refresh_devices(&self) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.discovering {
                return;
            }
            inner.discovering = true;
        }
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            let found = discover_all(DISCOVERY_TIMEOUT).await;
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.discovering = false;
            match found {
                Ok(devices) if !devices.is_empty() => {
                    tracing::info!(count = devices.len(), "renderer search finished");
                    guard.devices = devices.into_iter().map(Discovered::Dlna).collect();
                }
                Ok(_) => tracing::info!("renderer search found nothing; keeping the known list"),
                Err(error) => tracing::warn!(%error, "renderer search failed"),
            }
        });
    }

    /// Find a renderer at a specific address and add it to the list.
    ///
    /// Accepts an IP, `ip:port`, or a description URL. This bypasses SSDP, for
    /// networks that drop multicast between clients.
    pub fn add_by_address(&self, address: &str) {
        let address = address.trim().to_string();
        if address.is_empty() {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.discovering = true;
        }
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            let found = probe_address(&address).await;
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.discovering = false;
            match found {
                Ok(devices) if devices.is_empty() => {
                    tracing::info!(%address, "no renderer found at that address")
                }
                Ok(devices) => {
                    for device in devices {
                        let discovered = Discovered::Dlna(device);
                        if !guard.devices.iter().any(|seen| seen.id() == discovered.id()) {
                            tracing::info!(name = discovered.name(), %address, "added renderer by address");
                            guard.devices.push(discovered);
                        }
                    }
                }
                Err(error) => tracing::warn!(%address, %error, "address scan failed"),
            }
        });
    }

    /// Cast a torrent file to the device with the given UDN.
    ///
    /// `start_at` hands the picture over from the local player: the renderer is
    /// seeked to that position once it is playing.
    // The parameters are the cast request's fields; grouping them into a struct
    // would only move the same list behind one more name.
    #[allow(clippy::too_many_arguments)]
    pub fn start_cast(
        &self,
        udn: String,
        info_hash: String,
        file_id: usize,
        title: String,
        start_at: Option<Duration>,
        subtitles: bool,
        subtitle_language: Option<String>,
        audio_language: Option<String>,
        audio_index: Option<u32>,
    ) {
        let Some(device) = self.resolve_device(&udn) else {
            self.set_status(CastStatus::Failed {
                message: "that device is no longer available".to_string(),
            });
            return;
        };
        self.remember_discovered(&device);

        let generation = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.generation += 1;
            inner.generation
        };
        self.set_status(CastStatus::Preparing {
            device: device.name().to_string(),
            title: title.clone(),
            phase: PreparePhase::Connecting,
        });

        let engine = self.engine.clone();
        let tools = self.tools.clone();
        let cache = self.cache.clone();
        let options = self
            .options
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let loopback_base = self.loopback_base.clone();
        let lan_base = self.lan_base.clone();
        let token = self.token.clone();
        let session_slot = self.session.clone();
        let inner = self.inner.clone();
        let remux_tag = self.remux_tag.clone();
        let remux_audio = self.remux_audio.clone();
        let force_passthrough = *self.passthrough.lock().unwrap_or_else(|e| e.into_inner());
        let trust_hevc = *self.trust_hevc.lock().unwrap_or_else(|e| e.into_inner());
        let encoder = *self.encoder.lock().unwrap_or_else(|e| e.into_inner());
        let capabilities = self.capabilities.clone();
        let on_ended = self.on_ended.clone();
        let discovery = self.discovery.clone();

        self.runtime.spawn(async move {
            // Resolve the live engine id: a re-added stream-only title gets a
            // new engine id, and the loopback stream URL is built from it. The
            // engine's stats can be briefly unavailable right after a stream
            // starts, so retry rather than fail a cast that is actually up.
            let mut engine_id = None;
            for _ in 0..50 {
                if let Some(id) = engine
                    .list()
                    .into_iter()
                    .find(|view| view.info_hash.eq_ignore_ascii_case(&info_hash))
                    .map(|view| view.id)
                {
                    engine_id = Some(id);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let Some(engine_id) = engine_id else {
                if current_generation(&inner) == generation {
                    set_status_locked(
                        &inner,
                        CastStatus::Failed {
                            message: "that title is not streaming".to_string(),
                        },
                    );
                }
                return;
            };

            let result = prepare_and_start(
                &engine,
                &tools,
                &cache,
                options,
                &loopback_base,
                &lan_base,
                &token,
                &device,
                discovery.as_ref(),
                engine_id,
                file_id,
                &title,
                &remux_tag,
                &remux_audio,
                force_passthrough,
                start_at,
                &inner,
                generation,
                &capabilities,
                subtitles,
                subtitle_language.as_deref(),
                audio_language.as_deref(),
                audio_index,
                trust_hevc,
                encoder,
            )
            .await;

            let session = match result {
                Ok(session) => session,
                Err(error) => {
                    // Only report if this cast is still the current one.
                    if current_generation(&inner) == generation {
                        tracing::warn!(%error, "cast failed");
                        set_status_locked(
                            &inner,
                            CastStatus::Failed {
                                message: humanize_renderer_error(&error.to_string()),
                            },
                        );
                    }
                    return;
                }
            };

            // A newer cast, or a dismissal, superseded this one while it
            // prepared.
            if current_generation(&inner) != generation {
                return;
            }

            *session_slot.lock().await = Some(session);
            // Poll until the renderer stops or a newer cast takes over.
            //
            // A renderer that misses a poll or two does not end the cast: LG
            // webOS TVs drop connections under load and while switching media,
            // and treating one dropped poll as a failure reported "cast failed"
            // while the episode was still playing. Only a run of consecutive
            // failures is reported, and the last good state is kept meanwhile.
            let mut poll_failures = 0u32;
            let mut last_state = SessionState::Connecting;
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                if current_generation(&inner) != generation {
                    break;
                }
                let mut guard = session_slot.lock().await;
                let Some(session) = guard.as_mut() else { break };
                // Remember the last position: a renderer that resets to 0:00 on
                // stop must not erase where the picture actually was.
                let before = (session.position(), session.duration());
                let polled = session.poll().await.is_ok();
                if polled {
                    last_state = session.state().clone();
                    poll_failures = 0;
                } else {
                    poll_failures += 1;
                }
                let state = session.state().clone();
                let ended = matches!(state, SessionState::Stopped);
                let errored = matches!(state, SessionState::Error(_));
                let position = match session.position() {
                    Some(position) if position.as_secs_f64() > 1.0 => Some(position),
                    _ => before.0,
                };
                let duration = session.duration().or(before.1);
                // Volume is only asked of a renderer that has it; a renderer
                // without RenderingControl reports `None`, so the UI hides it.
                let volume = if session.has_volume() {
                    session.volume().await.ok()
                } else {
                    None
                };
                drop(guard);
                if ended {
                    clear_status_locked(&inner);
                    on_ended(position, duration);
                    break;
                }
                if errored && poll_failures >= MAX_POLL_FAILURES {
                    let message = match &state {
                        SessionState::Error(message) => humanize_renderer_error(message),
                        _ => "lost the connection to the TV".to_string(),
                    };
                    set_status_locked(&inner, CastStatus::Failed { message });
                    break;
                }
                let display = if errored { last_state.clone() } else { state };
                set_status_locked(
                    &inner,
                    CastStatus::Active {
                        device: device.name().to_string(),
                        title: title.clone(),
                        state: display,
                        position,
                        duration,
                        volume,
                    },
                );
            }
        });
    }

    /// Send a control action to the active cast.
    pub fn control(&self, action: CastAction) {
        let session = self.session.clone();
        let inner = self.inner.clone();
        let on_ended = self.on_ended.clone();
        self.runtime.spawn(async move {
            let mut guard = session.lock().await;
            let Some(session) = guard.as_mut() else {
                return;
            };
            let result = match action {
                CastAction::Pause => session.pause().await,
                CastAction::Resume => session.resume().await,
                CastAction::Seek(position) => session.seek(position).await,
                CastAction::SetVolume(volume) => session.set_volume(volume).await,
                CastAction::SetMute(muted) => session.set_mute(muted).await,
                CastAction::Stop => {
                    // Capture where it got to *before* stopping clears it, so
                    // playback can hand back to this machine.
                    let position = session.position();
                    let duration = session.duration();
                    let result = session.stop().await;
                    if result.is_ok() {
                        // Forget the cast only once the renderer confirmed the
                        // stop, so "end the cast" is real.
                        *guard = None;
                        drop(guard);
                        clear_status_locked(&inner);
                        on_ended(position, duration);
                    } else {
                        // The renderer refused to stop: keep the cast and its
                        // session so the failure is honest and can be retried,
                        // and stop the poll loop from papering the error over.
                        let message = match session.state() {
                            SessionState::Error(message) => message.clone(),
                            _ => "the renderer did not stop".to_string(),
                        };
                        drop(guard);
                        inner.lock().unwrap_or_else(|e| e.into_inner()).generation += 1;
                        set_status_locked(&inner, CastStatus::Failed { message });
                    }
                    result
                }
            };
            if let Err(error) = result {
                tracing::warn!(%error, "cast control failed");
            }
        });
    }

    /// Drop the current cast view and session without talking to the renderer,
    /// and cancel a cast that is still being prepared.
    pub fn dismiss(&self) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // A new generation makes any in-flight prepare discard its result.
            inner.generation += 1;
            inner.status = None;
        }
        let session = self.session.clone();
        self.runtime.spawn(async move {
            *session.lock().await = None;
        });
    }

    /// Delete every converted file.
    pub fn clear_cache(&self) -> anyhow::Result<u64> {
        Ok(self.cache.clear()?)
    }

    fn set_status(&self, status: CastStatus) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.status = Some(status);
    }

    /// Show a cast as connecting before the renderer has been contacted.
    ///
    /// Bumps the generation so any cast still being prepared is superseded, and
    /// so the UI has a status to render the moment a device is chosen — the
    /// casting screen opens on "Connecting…" instead of waiting for the stream.
    pub fn prepare(&self, device: &str, title: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.generation += 1;
        inner.status = Some(CastStatus::Preparing {
            device: device.to_string(),
            title: title.to_string(),
            phase: PreparePhase::Connecting,
        });
    }

    /// Move the current (preparing) cast to a new phase. A no-op once the cast
    /// is playing, failed, or has been superseded.
    pub fn set_phase(&self, phase: PreparePhase) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(CastStatus::Preparing { phase: current, .. }) = &mut inner.status {
            *current = phase;
        }
    }

    /// Remember the renderer a cast is using, so a transient discovery drop
    /// cannot make it look unreachable.
    fn remember_discovered(&self, device: &Discovered) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_device = Some(device.clone());
    }

    /// Find a discovered renderer by id (UDN or Google Cast device id), looking
    /// in the DLNA list, the last-used device, then the Cast discovery.
    fn resolve_device(&self, id: &str) -> Option<Discovered> {
        {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(found) = inner.devices.iter().find(|device| device.id() == id) {
                return Some(found.clone());
            }
            if let Some(found) = inner
                .last_device
                .as_ref()
                .filter(|device| device.id() == id)
            {
                return Some(found.clone());
            }
        }
        self.discovery.as_ref().and_then(|discovery| {
            discovery
                .devices()
                .into_iter()
                .find(|device| device.id == id)
                .map(Discovered::Gcast)
        })
    }

    /// Mark the current cast as failed without a renderer session — a title
    /// that never started streaming, or a device that went away.
    pub fn report_failed(&self, message: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.generation += 1;
        inner.status = Some(CastStatus::Failed { message });
    }
}

fn current_generation(inner: &Arc<Mutex<Inner>>) -> u64 {
    inner.lock().unwrap_or_else(|e| e.into_inner()).generation
}

fn set_status_locked(inner: &Arc<Mutex<Inner>>, status: CastStatus) {
    inner.lock().unwrap_or_else(|e| e.into_inner()).status = Some(status);
}

fn clear_status_locked(inner: &Arc<Mutex<Inner>>) {
    inner.lock().unwrap_or_else(|e| e.into_inner()).status = None;
}

/// Advance the phase of the current cast, unless a newer one has superseded it.
fn set_phase_if_current(inner: &Arc<Mutex<Inner>>, generation: u64, phase: PreparePhase) {
    let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
    if guard.generation != generation {
        return;
    }
    if let Some(CastStatus::Preparing { phase: current, .. }) = &mut guard.status {
        *current = phase;
    }
}

/// The renderer's capabilities, memoized per UDN. `fetch` runs only on a miss,
/// so switching files (or episodes) never re-queries the device: a cast is a
/// session, and a renderer's `SinkProtocolInfo` does not change within one.
async fn cached_sink_support<F, Fut>(
    cache: &Mutex<HashMap<String, SinkSupport>>,
    udn: &str,
    fetch: F,
) -> Option<SinkSupport>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = reel_cast::Result<reel_cast::SinkCapabilities>>,
{
    if let Some(sink) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(udn)
        .copied()
    {
        tracing::debug!(%udn, "using the renderer's memoized capabilities");
        return Some(sink);
    }
    // Query once per device, even when the query fails: capabilities do not
    // change within a session, and a renderer that does not answer must not be
    // re-queried on every media switch.
    let sink = match fetch().await {
        Ok(caps) => {
            tracing::info!(
                %udn,
                entries = caps.entries.len(),
                "renderer capabilities queried (once per device)"
            );
            SinkSupport {
                hevc: caps.supports_hevc(),
                ac3: caps.supports_ac3(),
                eac3: caps.supports_eac3(),
                mp4: caps.accepts_mime("video/mp4"),
                mkv: caps.accepts_mime("video/x-matroska"),
            }
        }
        Err(error) => {
            tracing::info!(
                %udn,
                %error,
                "renderer did not report capabilities; using the conservative default for the session"
            );
            SinkSupport::default()
        }
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(udn.to_string(), sink);
    tracing::debug!(%udn, hevc = sink.hevc, mp4 = sink.mp4, mkv = sink.mkv, "capabilities cached");
    Some(sink)
}

/// Turn a raw renderer transport failure into something a person can act on.
///
/// The underlying message is a reqwest error with a control URL in it, which is
/// noise in the UI; a dropped connection becomes a plain sentence. Anything
/// else (a UPnP fault, an unsupported format) is already meaningful and is left
/// alone.
fn humanize_renderer_error(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let transport_noise = [
        "error sending request",
        "operation timed out",
        "timed out",
        "connection refused",
        "connection reset",
        "broken pipe",
        "error trying to connect",
        "dns error",
        "connect error",
    ];
    if transport_noise.iter().any(|needle| lower.contains(needle)) {
        return "lost the connection to the TV".to_string();
    }
    message.to_string()
}

/// Probe, (optionally) convert, then hand the renderer the media URL and start.
#[allow(clippy::too_many_arguments)]
async fn prepare_and_start(
    engine: &Arc<Engine>,
    tools: &Tools,
    cache: &ConversionCache,
    options: PlanOptions,
    loopback_base: &str,
    lan_base: &str,
    token: &str,
    device: &Discovered,
    discovery: Option<&Arc<Discovery>>,
    id: usize,
    file_id: usize,
    title: &str,
    remux_tag: &Arc<Mutex<Option<String>>>,
    remux_audio: &Arc<Mutex<Option<u32>>>,
    force_passthrough: bool,
    start_at: Option<Duration>,
    inner: &Arc<Mutex<Inner>>,
    generation: u64,
    capabilities: &Arc<Mutex<HashMap<String, SinkSupport>>>,
    subtitles: bool,
    subtitle_language: Option<&str>,
    audio_language: Option<&str>,
    requested_audio_index: Option<u32>,
    trust_hevc: bool,
    encoder: VideoEncoder,
) -> anyhow::Result<CastSession> {
    set_phase_if_current(inner, generation, PreparePhase::Checking);
    let source_url = format!("{loopback_base}/stream/{id}/{file_id}");
    let probe = tools.probe(&source_url).await?;
    tracing::debug!(container = ?probe.format_name, "probed the source for casting");

    // Pick the audio track: an explicit choice from the remote wins, else the
    // preferred language when the file offers it, else the file's first.
    // Choosing a stream forces a conversion, so the language path only changes
    // anything when a preference is actually set.
    let chosen_audio = match requested_audio_index {
        Some(index) => probe
            .audio_tracks
            .iter()
            .find(|track| track.index == index)
            .cloned(),
        None => pick_audio_track(&probe.audio_tracks, audio_language),
    };
    let audio_index = chosen_audio.as_ref().map(|track| track.index);
    *remux_audio.lock().unwrap_or_else(|e| e.into_inner()) = audio_index;

    // Record the tracks for the remote's picker, unless a newer cast has
    // superseded this one.
    {
        let tracks = audio_track_infos(&probe.audio_tracks, audio_index);
        let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation == generation {
            guard.audio_tracks = tracks;
        }
    }

    // Decide what the renderer accepts. A forced passthrough overrides its own
    // answer (some televisions under-report); otherwise use the reported
    // capabilities, and stay conservative when it does not answer.
    let mut options = options;
    options.audio = chosen_audio.clone();
    if force_passthrough {
        options.sink = SinkSupport {
            hevc: true,
            ac3: true,
            eac3: true,
            mp4: true,
            mkv: true,
        };
        tracing::debug!("passthrough forced; ignoring the renderer's capabilities");
    } else {
        match device {
            Discovered::Dlna(cast_device) => {
                // A renderer's capabilities are a property of the device, not of
                // the cast: ask once and reuse them for every cast to it.
                // Switching episodes (or a title) must not trigger another
                // `GetProtocolInfo`.
                if let Some(sink) = cached_sink_support(capabilities, cast_device.udn(), || {
                    reel_cast::sink_capabilities(cast_device)
                })
                .await
                {
                    options.sink = sink;
                }
            }
            Discovered::Gcast(_) => {
                // Google Cast exposes no codec query here (the SDK reports no
                // capabilities for Chromecast), so start from what every Cast
                // receiver decodes: MP4/H.264/AAC. `Trust HEVC` may then pass
                // HEVC through rather than re-encoding it.
                options.sink = SinkSupport {
                    mp4: true,
                    ..SinkSupport::default()
                };
            }
        }
    }

    // Many renderers under-report HEVC (LG webOS does). When the container is
    // accepted, trust the codec rather than re-encoding: it keeps the picture,
    // avoids a heavy transcode, and lets the TV show subtitles embedded in the
    // container. A TV that genuinely cannot decode it fails to start.
    if trust_hevc
        && !options.sink.hevc
        && (options.sink.mkv || options.sink.mp4)
        && probe
            .video
            .as_ref()
            .is_some_and(|video| matches!(video.codec.as_str(), "hevc" | "h265"))
    {
        options.sink.hevc = true;
        tracing::info!("renderer under-reports HEVC; passing it through");
    }

    let plan = plan(&probe, &options);

    // The subtitle to deliver as an external sidecar (a text source: a `.srt`
    // shipped with the title, or a text track extracted to one).
    let is_gcast = matches!(device, Discovered::Gcast(_));
    let captions = if subtitles {
        resolve_caption(
            engine,
            tools,
            cache,
            lan_base,
            token,
            loopback_base,
            id,
            file_id,
            &probe,
            subtitle_language,
        )
        .await
        .map(|(url, _name)| {
            // A Google Cast receiver cannot read a SubRip sidecar; the WebVTT
            // sibling (written alongside) is side-loaded as a track instead.
            let url = if is_gcast {
                url.replace(".srt", ".vtt")
            } else {
                url
            };
            vec![didl::Resource::subtitle(url)]
        })
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    // When there is no text subtitle, the only way to show an image subtitle
    // (PGS/DVD) is to burn it in: it cannot be a text sidecar, and renderers
    // (the LG included) do not expose a container's image subtitle track over
    // DLNA. Burning is done by the live transcode path below, so playback still
    // starts immediately; it is skipped when conversion is turned off.
    let burn_subtitle = if subtitles && captions.is_empty() {
        pick_image_track(&probe.subtitle_tracks, subtitle_language).map(|track| track.index)
    } else {
        None
    };

    // HEVC in MP4 must carry the `hvc1` sample-entry tag, or televisions report
    // the format as unrecognised. Remembered for the live-remux command.
    let video_tag = probe
        .video
        .as_ref()
        .is_some_and(|video| matches!(video.codec.as_str(), "hevc" | "h265"))
        .then(|| "hvc1".to_string());
    *remux_tag.lock().unwrap_or_else(|e| e.into_inner()) = video_tag.clone();

    // The source file's name (for the URL/DIDL MIME) and length, fetched once.
    let file = engine.probe_file(id, file_id)?;
    let file_name = file.name.clone();
    let length = file.length;

    // Burning an image subtitle needs a finished, indexed file to stay
    // seekable (DLNA seeks by byte range), so a title that would otherwise pass
    // through is transcoded to a faststart MP4 — only when subtitles are wanted
    // and conversion is allowed. The result is cached, so the wait is one-time.
    let burn_subtitle = burn_subtitle.filter(|_| !matches!(plan, MediaPlan::Unavailable { .. }));
    let plan = match (burn_subtitle, plan) {
        (Some(_), MediaPlan::Original | MediaPlan::Remux) => MediaPlan::Transcode {
            max_height: options.max_height.max(1),
        },
        (_, plan) => plan,
    };
    let (url, seekable, converted) = match plan {
        MediaPlan::Original => (
            original_url(lan_base, token, id, file_id, &file_name),
            true,
            false,
        ),
        MediaPlan::Remux => {
            let key = CacheKey::for_plan(
                engine.view(id)?.info_hash,
                file_id as u64,
                length,
                &MediaPlan::Remux,
                audio_index,
            );
            let output = cache.path_for(&key);
            if output.is_file() {
                // A previous remux finished: serve it with range support.
                (converted_url(lan_base, token, &key.file_name()), true, true)
            } else if matches!(device, Discovered::Gcast(_)) {
                // Google Cast needs a real duration and seeking, which a live
                // fragmented stream does not give: it reports the media as only
                // as long as what has streamed, and stops there. Remux to a
                // seekable file first (a copy, so it is quick).
                set_phase_if_current(inner, generation, PreparePhase::Preparing { percent: None });
                tools
                    .remux_to_file(&source_url, video_tag.as_deref(), audio_index, &output)
                    .await?;
                if let Err(error) = cache.evict_to_limit() {
                    tracing::warn!(%error, "could not trim the conversion cache");
                }
                (converted_url(lan_base, token, &key.file_name()), true, true)
            } else {
                // Remux on the fly: the LAN endpoint streams fragmented MP4 and
                // tees it into the cache, so a later cast can seek the file.
                (
                    remux_url(lan_base, token, id, file_id, &file_name),
                    false,
                    true,
                )
            }
        }
        MediaPlan::Transcode { max_height } => {
            let key = CacheKey::for_plan(
                engine.view(id)?.info_hash,
                file_id as u64,
                length,
                &MediaPlan::Transcode { max_height },
                audio_index,
            );
            // A burned-in subtitle produces a different file, kept under its own
            // name so it does not collide with the plain transcode in the cache.
            let (name, output) = match burn_subtitle {
                Some(index) => {
                    let name = format!(
                        "{}-sub{index}.mp4",
                        key.file_name().trim_end_matches(".mp4")
                    );
                    (name.clone(), cache.path_for(&key).with_file_name(&name))
                }
                None => (key.file_name(), cache.path_for(&key)),
            };
            if !output.is_file() {
                set_phase_if_current(inner, generation, PreparePhase::Preparing { percent: None });
                match burn_subtitle {
                    Some(index) => {
                        let total = probe
                            .duration
                            .map(|duration| duration.as_secs_f32())
                            .filter(|total| *total > 0.0);
                        tools
                            .transcode_burn(
                                &source_url,
                                &probe,
                                max_height,
                                &output,
                                index,
                                encoder,
                                chosen_audio.as_ref(),
                                |seconds| {
                                    if let Some(total) = total {
                                        let percent =
                                            ((seconds / total) * 100.0).clamp(0.0, 99.0) as u8;
                                        set_phase_if_current(
                                            inner,
                                            generation,
                                            PreparePhase::Preparing {
                                                percent: Some(percent),
                                            },
                                        );
                                    }
                                },
                            )
                            .await?;
                    }
                    None => {
                        tools
                            .transcode(
                                &source_url,
                                &probe,
                                max_height,
                                &output,
                                encoder,
                                chosen_audio.as_ref(),
                            )
                            .await?;
                    }
                }
                if let Err(error) = cache.evict_to_limit() {
                    tracing::warn!(%error, "could not trim the conversion cache");
                }
            }
            (converted_url(lan_base, token, &name), true, true)
        }
        MediaPlan::Unavailable { reason } => anyhow::bail!("{reason}"),
    };

    set_phase_if_current(inner, generation, PreparePhase::Starting);
    let metadata = metadata(title, &url, seekable, converted, captions);
    let target: Box<dyn CastTarget> = match device {
        Discovered::Dlna(cast_device) => Box::new(DlnaTarget::new(cast_device)?),
        Discovered::Gcast(gcast) => {
            let discovery =
                discovery.ok_or_else(|| anyhow::anyhow!("Google Cast discovery is not running"))?;
            Box::new(discovery.target(gcast)?)
        }
    };
    let mut session = CastSession::new(target, title);
    // The real duration, so a live stream's "duration follows the playhead"
    // value does not make the progress bar (or the end-of-cast check) wrong.
    session.set_duration_hint(probe.duration);
    match start_at {
        // Hand the picture over from the local player: start, then seek.
        Some(position) => session.start_at(url, &metadata, position).await?,
        None => session.start(url, &metadata).await?,
    }
    Ok(session)
}

/// Map the stored conversion mode to the planner's.
pub fn transcode_mode(mode: ConversionMode) -> TranscodeMode {
    match mode {
        ConversionMode::Off => TranscodeMode::Off,
        ConversionMode::Remux => TranscodeMode::RemuxOnly,
        ConversionMode::Auto => TranscodeMode::Auto,
    }
}

/// Build the planner options from settings.
///
/// The renderer's own capabilities (or the forced passthrough) are applied at
/// cast time, so `sink` starts conservative here.
pub fn plan_options(settings: &CatalogSettings) -> PlanOptions {
    PlanOptions {
        mode: transcode_mode(settings.conversion_mode),
        max_height: settings.conversion_max_height,
        sink: SinkSupport::default(),
        // The audio track is chosen per cast, from the probe.
        audio: None,
    }
}

/// Map a stored video-encoding preference to an encoder to target. `None` means
/// "auto": pick the best hardware encoder the machine lists, else software.
///
/// A pinned encoder is still resolved against what ffmpeg listed, so an
/// unavailable choice degrades to software rather than failing the cast.
fn encoder_preference(hwaccel: HwAccel) -> Option<VideoEncoder> {
    match hwaccel {
        HwAccel::Auto => None,
        HwAccel::Off => Some(VideoEncoder::Software),
        HwAccel::Vaapi => Some(VideoEncoder::Vaapi),
        HwAccel::Qsv => Some(VideoEncoder::Qsv),
        HwAccel::Nvenc => Some(VideoEncoder::Nvenc),
        HwAccel::VideoToolbox => Some(VideoEncoder::VideoToolbox),
        HwAccel::Amf => Some(VideoEncoder::Amf),
    }
}

/// The LAN URL for a file served directly from the engine.
pub fn original_url(lan_base: &str, token: &str, id: usize, file_id: usize, name: &str) -> String {
    format!(
        "{lan_base}/media/{token}/original/{id}/{file_id}/{}",
        url_name(name, None)
    )
}

/// The LAN URL for a converted file.
pub fn converted_url(lan_base: &str, token: &str, name: &str) -> String {
    format!("{lan_base}/media/{token}/converted/{name}")
}

/// The LAN URL for a live remux of a file.
pub fn remux_url(lan_base: &str, token: &str, id: usize, file_id: usize, name: &str) -> String {
    format!(
        "{lan_base}/media/{token}/remux/{id}/{file_id}/{}",
        url_name(name, Some("mp4"))
    )
}

/// A URL-safe file name for a media path, optionally forcing an extension, so
/// the DIDL `protocolInfo` MIME (derived from the URL) is right.
fn url_name(name: &str, force_extension: Option<&str>) -> String {
    let leaf = name.rsplit('/').next().unwrap_or("media");
    let mut safe: String = leaf
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe.starts_with('.') {
        safe = format!("media{safe}");
    }
    match force_extension {
        Some(extension) => {
            let stem = safe.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&safe);
            format!("{stem}.{extension}")
        }
        None => safe,
    }
}

/// DIDL-Lite metadata for a cast, including any subtitle sidecars.
pub fn metadata(
    title: &str,
    url: &str,
    seekable: bool,
    converted: bool,
    captions: Vec<didl::Resource>,
) -> String {
    didl::Item::video(title, url, seekable, converted)
        .with_captions(captions)
        .to_xml()
}

/// The LAN URL for an extracted/served subtitle file.
fn subtitle_url(lan_base: &str, token: &str, name: &str) -> String {
    format!("{lan_base}/media/{token}/subtitle/{name}")
}

/// Resolve the subtitle to deliver to the renderer, as a `(url, cache name)`.
///
/// A sidecar `.srt`/`.ass` next to the video is preferred (matching the wanted
/// language when one is set); otherwise one text track embedded in the file is
/// extracted. Nothing is returned when the title has no subtitles, or when the
/// only tracks are image-based.
#[allow(clippy::too_many_arguments)]
async fn resolve_caption(
    engine: &Arc<Engine>,
    tools: &Tools,
    cache: &ConversionCache,
    lan_base: &str,
    token: &str,
    loopback_base: &str,
    id: usize,
    file_id: usize,
    probe: &reel_media::Probe,
    language: Option<&str>,
) -> Option<(String, String)> {
    let info_hash = engine.view(id).ok()?.info_hash.clone();
    let root = cache.root().to_path_buf();

    // 1. A sidecar file shipped with the torrent.
    if let Ok(view) = engine.view(id) {
        if let Some(sidecar) = pick_sidecar(&view.files, language) {
            let name = format!("sub-{info_hash}-{file_id}-sidecar-{}.srt", sidecar.id);
            let output = root.join(&name);
            // Convert to `.srt` (a sidecar may be `.ass`/`.vtt`) unless it
            // already is one, in which case copy its bytes.
            let ok = if sidecar.name.to_ascii_lowercase().ends_with(".srt") {
                copy_from_engine(engine, id, sidecar.id, &output).await.is_ok()
            } else {
                let source = format!("{loopback_base}/stream/{id}/{}", sidecar.id);
                tools.extract_subtitle(&source, 0, &output).await.is_ok()
            };
            if ok && output.is_file() {
                ensure_webvtt(&output).await;
                return Some((subtitle_url(lan_base, token, &name), name));
            }
        }
    }

    // 2. A text track embedded in the video: extract it once.
    if let Some(track) = pick_track(&probe.subtitle_tracks, language) {
        let name = format!("sub-{info_hash}-{file_id}-embedded-{}.srt", track.index);
        let output = root.join(&name);
        if !output.is_file() {
            let source = format!("{loopback_base}/stream/{id}/{file_id}");
            if tools.extract_subtitle(&source, track.index, &output).await.is_err() {
                return None;
            }
        }
        if output.is_file() {
            ensure_webvtt(&output).await;
            return Some((subtitle_url(lan_base, token, &name), name));
        }
    }

    None
}

/// Write a WebVTT sibling of an `.srt` file, so a browser-based receiver (a
/// future Google Cast target) can fetch it. Best-effort: DLNA keeps the SRT,
/// and a failure here must not fail the cast.
async fn ensure_webvtt(srt: &std::path::Path) {
    let vtt = srt.with_extension("vtt");
    if vtt.is_file() {
        return;
    }
    let Ok(text) = tokio::fs::read_to_string(srt).await else {
        return;
    };
    if let Err(error) = tokio::fs::write(&vtt, reel_media::convert::srt_to_vtt(&text)).await {
        tracing::debug!(%error, path = %vtt.display(), "could not write the WebVTT sidecar");
    }
}

/// Read a small file straight from the engine (a subtitle sidecar) into the
/// cache. The sidecar is not fetched over HTTP: the engine already owns it.
async fn copy_from_engine(
    engine: &Arc<Engine>,
    id: usize,
    file_id: usize,
    output: &std::path::Path,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut stream = engine.stream_from(id, file_id, 0).await?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(output, &bytes).await?;
    Ok(())
}

/// Pick a sidecar subtitle file, preferring one that names the wanted language.
fn pick_sidecar(files: &[reel_core::model::FileView], language: Option<&str>) -> Option<reel_core::model::FileView> {
    let candidates: Vec<&reel_core::model::FileView> = files
        .iter()
        .filter(|file| file.is_subtitle && is_text_subtitle(&file.name))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if let Some(language) = language {
        if let Some(found) = candidates
            .iter()
            .find(|file| name_matches_language(&file.name, language))
        {
            return Some((*found).clone());
        }
    }
    Some(candidates[0].clone())
}

/// Pick the audio track to deliver: the one matching the wanted language, else
/// nothing (the file's first track is used, and the cast can stay a passthrough).
///
/// Returning `None` when the preference is unset or unmatched is deliberate: an
/// explicit choice forces a conversion, so it must only happen when the user
/// actually asked for a track the file has.
fn pick_audio_track(tracks: &[AudioTrack], language: Option<&str>) -> Option<AudioTrack> {
    let language = language?;
    tracks
        .iter()
        .find(|track| tag_matches(track.language.as_deref(), language))
        .cloned()
}

/// Describe the file's audio tracks for the remote's picker.
fn audio_track_infos(tracks: &[AudioTrack], selected: Option<u32>) -> Vec<AudioTrackInfo> {
    // With no explicit choice the renderer plays the first track.
    let selected = selected.or_else(|| tracks.first().map(|track| track.index));
    tracks
        .iter()
        .map(|track| AudioTrackInfo {
            index: track.index,
            label: audio_track_label(track),
            selected: Some(track.index) == selected,
        })
        .collect()
}

/// A human label for an audio track, e.g. "English · ac3".
fn audio_track_label(track: &AudioTrack) -> String {
    let name = track
        .title
        .clone()
        .or_else(|| track.language.clone())
        .unwrap_or_else(|| format!("Track {}", track.index));
    format!("{name} \u{00b7} {}", track.codec)
}

/// Pick a text subtitle track, preferring one that names the wanted language.
fn pick_track<'a>(
    tracks: &'a [reel_media::SubtitleTrack],
    language: Option<&str>,
) -> Option<&'a reel_media::SubtitleTrack> {
    let text: Vec<&reel_media::SubtitleTrack> = tracks.iter().filter(|t| t.is_text()).collect();
    if text.is_empty() {
        return None;
    }
    if let Some(language) = language {
        if let Some(found) = text.iter().find(|track| {
            track
                .language
                .as_deref()
                .is_some_and(|tag| tag.eq_ignore_ascii_case(language))
                || tag_matches(track.language.as_deref(), language)
        }) {
            return Some(*found);
        }
    }
    Some(text[0])
}

/// Pick an image subtitle track (PGS/DVD), preferring the wanted language.
fn pick_image_track<'a>(
    tracks: &'a [reel_media::SubtitleTrack],
    language: Option<&str>,
) -> Option<&'a reel_media::SubtitleTrack> {
    let image: Vec<&reel_media::SubtitleTrack> = tracks.iter().filter(|t| !t.is_text()).collect();
    if image.is_empty() {
        return None;
    }
    if let Some(language) = language {
        if let Some(found) = image
            .iter()
            .find(|track| tag_matches(track.language.as_deref(), language))
        {
            return Some(*found);
        }
    }
    Some(image[0])
}

/// Whether a file name suggests the wanted language, e.g. `.en.` or `.eng.`.
fn name_matches_language(name: &str, language: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let wanted = language.to_ascii_lowercase();
    let short: &str = match wanted.as_str() {
        "english" => "en",
        "spanish" => "es",
        "french" => "fr",
        "german" => "de",
        "italian" => "it",
        "portuguese" => "pt",
        "japanese" => "ja",
        "korean" => "ko",
        other => other,
    };
    lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token == wanted || token == short)
}

fn tag_matches(tag: Option<&str>, language: &str) -> bool {
    tag.is_some_and(|tag| name_matches_language(tag, language))
}

/// Whether a subtitle file is text (deliverable as `.srt`), not image-based.
fn is_text_subtitle(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".srt", ".vtt", ".ass", ".ssa", ".sub"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// The address to advertise in cast URLs.
///
/// An explicit override wins. Otherwise the primary outbound IPv4 is used: a
/// connected UDP socket asks the routing table which interface would reach the
/// internet, without sending a packet.
fn lan_address(settings: &CatalogSettings) -> Option<IpAddr> {
    if let Some(value) = settings.cast_lan_address.as_deref() {
        if let Ok(address) = value.trim().parse::<IpAddr>() {
            return Some(address);
        }
    }
    primary_lan_ip()
}

fn primary_lan_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback()).then_some(address)
}

/// An unguessable per-run token, from two randomly-seeded hashers.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let value = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        out.push_str(&format!("{value:016x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_failures_read_as_a_plain_sentence() {        let raw = "request to the device failed: error sending request for url \
                   (http://192.168.1.187:1244/AVTransport/abc/control.xml)";
        assert_eq!(humanize_renderer_error(raw), "lost the connection to the TV");
        assert_eq!(
            humanize_renderer_error("operation timed out"),
            "lost the connection to the TV"
        );
        // A meaningful message (a UPnP fault, a format problem) is left alone.
        assert_eq!(
            humanize_renderer_error("upnp fault 716: resource not available"),
            "upnp fault 716: resource not available"
        );
    }

    #[tokio::test]
    async fn renderer_capabilities_are_fetched_once_per_device() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache: Mutex<HashMap<String, SinkSupport>> = Mutex::new(HashMap::new());
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok::<_, reel_cast::CastError>(reel_cast::SinkCapabilities::parse(
                    "http-get:*:video/mp4:*",
                ))
            }
        };
        let first = cached_sink_support(&cache, "uuid:tv", fetch).await;
        let second = cached_sink_support(&cache, "uuid:tv", fetch).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "capabilities must be asked for once per device, not per file"
        );
        assert_eq!(first, second);
        assert!(first.is_some_and(|sink| sink.mp4));
    }

    #[tokio::test]
    async fn a_failed_capability_query_is_not_retried_per_media() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache: Mutex<HashMap<String, SinkSupport>> = Mutex::new(HashMap::new());
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<reel_cast::SinkCapabilities, _>(reel_cast::CastError::Missing("no answer".into())) }
        };
        let first = cached_sink_support(&cache, "uuid:tv", fetch).await;
        let second = cached_sink_support(&cache, "uuid:tv", fetch).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a renderer that does not answer must not be re-queried on every switch"
        );
        // The conservative default is used and remembered.
        assert_eq!(first, Some(SinkSupport::default()));
        assert_eq!(second, Some(SinkSupport::default()));
    }

    #[test]
    fn conversion_modes_map_to_the_planner() {
        assert_eq!(transcode_mode(ConversionMode::Off), TranscodeMode::Off);        assert_eq!(
            transcode_mode(ConversionMode::Remux),
            TranscodeMode::RemuxOnly
        );
        assert_eq!(transcode_mode(ConversionMode::Auto), TranscodeMode::Auto);
    }

    #[test]
    fn plan_options_come_from_settings() {
        let settings = CatalogSettings {
            conversion_mode: ConversionMode::Remux,
            conversion_max_height: 720,
            allow_hevc_passthrough: true,
            ..Default::default()
        };
        let options = plan_options(&settings);
        assert_eq!(options.mode, TranscodeMode::RemuxOnly);
        assert_eq!(options.max_height, 720);
        // Capabilities are applied per cast, not baked in here.
        assert_eq!(options.sink, SinkSupport::default());
    }

    #[test]
    fn hwaccel_preferences_map_to_encoders() {
        assert_eq!(encoder_preference(HwAccel::Auto), None);
        assert_eq!(
            encoder_preference(HwAccel::Off),
            Some(VideoEncoder::Software)
        );
        assert_eq!(
            encoder_preference(HwAccel::Nvenc),
            Some(VideoEncoder::Nvenc)
        );
        assert_eq!(
            encoder_preference(HwAccel::VideoToolbox),
            Some(VideoEncoder::VideoToolbox)
        );

        // A pinned encoder that is not listed resolves to software.
        let none_listed = AvailableEncoders::default();
        assert_eq!(
            none_listed.choose(encoder_preference(HwAccel::Vaapi)),
            VideoEncoder::Software
        );
        assert_eq!(
            none_listed.choose(encoder_preference(HwAccel::Auto)),
            VideoEncoder::Software
        );
    }

    #[test]
    fn audio_track_infos_mark_the_selection() {
        let tracks = vec![
            AudioTrack {
                index: 1,
                codec: "aac".into(),
                channels: Some(2),
                language: Some("eng".into()),
                title: None,
            },
            AudioTrack {
                index: 2,
                codec: "ac3".into(),
                channels: Some(6),
                language: Some("fra".into()),
                title: None,
            },
        ];
        let infos = audio_track_infos(&tracks, None);
        assert_eq!(infos.len(), 2);
        assert!(infos[0].selected, "the first track is the default");
        assert!(!infos[1].selected);
        assert_eq!(infos[1].label, "fra \u{00b7} ac3");

        let infos = audio_track_infos(&tracks, Some(2));
        assert!(!infos[0].selected);
        assert!(infos[1].selected);
    }

    #[test]
    fn cast_devices_report_their_kind_and_id() {
        let device = reel_cast_gcast::GcastDevice::manual(
            "Bedroom",
            "cast-abc",
            reel_cast_gcast::Protocol::Chromecast,
            vec![std::net::IpAddr::from([10, 0, 0, 5])],
            8009,
        );
        let info = DeviceInfo::from(&Discovered::Gcast(device));
        assert_eq!(info.kind, CastKind::GoogleCast);
        assert_eq!(info.udn, "cast-abc");
        assert_eq!(info.name, "Bedroom");
        assert_eq!(CastKind::GoogleCast.label(), "Google Cast");
        assert_eq!(CastKind::Dlna.label(), "DLNA");
    }

    #[test]
    fn media_urls_carry_the_token() {
        assert_eq!(
            original_url("http://10.0.0.5:4000", "tok", 3, 1, "Movie.2024.mkv"),
            "http://10.0.0.5:4000/media/tok/original/3/1/Movie.2024.mkv"
        );
        assert_eq!(
            converted_url("http://10.0.0.5:4000", "tok", "abc-1-2-remux.mp4"),
            "http://10.0.0.5:4000/media/tok/converted/abc-1-2-remux.mp4"
        );
    }

    #[test]
    fn metadata_is_didl_with_protocol_info() {
        let xml = metadata(
            "Episode 1",
            "http://10.0.0.5:4000/media/tok/original/3/1",
            true,
            false,
            Vec::new(),
        );
        assert!(xml.contains("<dc:title>Episode 1</dc:title>"));
        assert!(xml.contains("protocolInfo=\"http-get:*:"));
        assert!(xml.contains("http://10.0.0.5:4000/media/tok/original/3/1"));
    }

    #[test]
    fn metadata_carries_a_subtitle_caption() {
        let xml = metadata(
            "Episode 1",
            "http://h/x.mkv",
            true,
            false,
            vec![reel_cast::didl::Resource::subtitle(
                "http://h/media/tok/subtitle/sub-x.srt",
            )],
        );
        assert!(xml.contains("<sec:CaptionInfoEx sec:type=\"srt\">"));
        assert!(xml.contains("protocolInfo=\"http-get:*:text/srt:*\""));
    }

    #[test]
    fn media_urls_include_a_live_remux_route() {
        assert_eq!(
            remux_url("http://10.0.0.5:4000", "tok", 3, 1, "Episode 1.mkv"),
            "http://10.0.0.5:4000/media/tok/remux/3/1/Episode_1.mp4"
        );
    }

    #[test]
    fn an_explicit_lan_address_wins() {
        let settings = CatalogSettings {
            cast_lan_address: Some("192.168.1.42".to_string()),
            ..Default::default()
        };
        assert_eq!(
            lan_address(&settings),
            Some("192.168.1.42".parse().unwrap())
        );
    }

    #[test]
    fn tokens_are_unique_and_long_enough() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
