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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reel_cast::session::{CastSession, SessionState};
use reel_cast::{CastDevice, DlnaTarget, didl, discover_all, probe_address};
use reel_catalog::{CatalogSettings, ConversionMode};
use reel_core::Engine;
use reel_http::{MediaState, build_media_router};
use reel_media::{CacheKey, ConversionCache, MediaPlan, PlanOptions, Tools, TranscodeMode, plan};

/// How long an SSDP search waits for replies.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

/// How often an active cast is polled for position/state.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cap on the conversion cache.
const CACHE_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// A renderer as the UI needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Friendly name.
    pub name: String,
    /// Model name, when advertised.
    pub model: Option<String>,
    /// Stable unique device name.
    pub udn: String,
}

impl From<&CastDevice> for DeviceInfo {
    fn from(device: &CastDevice) -> DeviceInfo {
        DeviceInfo {
            name: device.name().to_string(),
            model: device.model().map(str::to_string),
            udn: device.udn().to_string(),
        }
    }
}

/// What the UI shows about the current cast.
#[derive(Debug, Clone, PartialEq)]
pub enum CastStatus {
    /// Converting the file before it can be played.
    Preparing {
        /// Target device name.
        device: String,
        /// Title being cast.
        title: String,
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
    },
    /// The last operation failed.
    Failed {
        /// A user-facing message.
        message: String,
    },
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
}

#[derive(Default)]
struct Inner {
    devices: Vec<CastDevice>,
    status: Option<CastStatus>,
    discovering: bool,
    /// Bumped on every new cast, so an older poll loop stops.
    generation: u64,
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
    loopback_base: String,
    runtime: tokio::runtime::Handle,
    inner: Arc<Mutex<Inner>>,
    session: Arc<tokio::sync::Mutex<Option<CastSession>>>,
    /// Resolving this stops the LAN media server.
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
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
    ) -> Result<CastService, String> {
        if !settings.allow_lan_streaming {
            return Err("streaming to the local network is turned off".to_string());
        }

        let ffmpeg_override = settings.ffmpeg_path.as_deref().map(std::path::Path::new);
        let tools = Tools::resolve(ffmpeg_override, None)
            .map_err(|error| format!("casting needs ffmpeg: {error}"))?;

        let address = lan_address(settings)
            .ok_or_else(|| "no local network address to advertise".to_string())?;

        let token = random_token();
        let cache = ConversionCache::new(data_dir.join("cast-cache"), CACHE_LIMIT_BYTES);

        let listener = runtime
            .block_on(reel_http::bind(SocketAddr::new(
                Ipv4Addr::UNSPECIFIED.into(),
                0,
            )))
            .map_err(|error| format!("could not open a LAN media port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("could not read the LAN media port: {error}"))?
            .port();
        let lan_base = format!("http://{address}:{port}");

        // Live remux: ffmpeg reads the loopback stream and writes a fragmented
        // MP4, which the LAN endpoint streams and tees into the cache.
        let live_source: reel_http::LiveSource = {
            let engine = engine.clone();
            let ffmpeg = tools.ffmpeg.clone();
            let cache = cache.clone();
            let loopback = loopback_base.clone();
            Arc::new(move |id: usize, file_id: usize| {
                let length = engine.probe_file(id, file_id).ok()?.length;
                let info_hash = engine.view(id).ok()?.info_hash;
                let key = CacheKey::for_plan(info_hash, file_id as u64, length, &MediaPlan::Remux);
                let source = format!("{loopback}/stream/{id}/{file_id}");
                let mut command = tokio::process::Command::new(&ffmpeg);
                command.args(reel_media::convert::remux_stream_args(&source));
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
            loopback_base,
            runtime,
            inner: Arc::new(Mutex::new(Inner::default())),
            session: Arc::new(tokio::sync::Mutex::new(None)),
            shutdown: Mutex::new(Some(shutdown_tx)),
        })
    }

    /// Replace the conversion policy used for the next cast.
    pub fn set_options(&self, options: PlanOptions) {
        *self.options.lock().unwrap_or_else(|e| e.into_inner()) = options;
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

    /// Discovered renderers.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .devices
            .iter()
            .map(DeviceInfo::from)
            .collect()
    }

    /// Whether a discovery search is running.
    pub fn discovering(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .discovering
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
                Ok(devices) => {
                    tracing::info!(count = devices.len(), "renderer search finished");
                    guard.devices = devices;
                }
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
                        if !guard.devices.iter().any(|seen| seen.udn() == device.udn()) {
                            tracing::info!(name = device.name(), %address, "added renderer by address");
                            guard.devices.push(device);
                        }
                    }
                }
                Err(error) => tracing::warn!(%address, %error, "address scan failed"),
            }
        });
    }

    /// Cast a torrent file to the device with the given UDN.
    pub fn start_cast(&self, udn: String, id: usize, file_id: usize, title: String) {
        let device = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .devices
            .iter()
            .find(|device| device.udn() == udn)
            .cloned();

        let Some(device) = device else {
            self.set_status(CastStatus::Failed {
                message: "that device is no longer available".to_string(),
            });
            return;
        };

        let generation = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.generation += 1;
            inner.generation
        };
        self.set_status(CastStatus::Preparing {
            device: device.name().to_string(),
            title: title.clone(),
        });

        let engine = self.engine.clone();
        let tools = self.tools.clone();
        let cache = self.cache.clone();
        let options = *self.options.lock().unwrap_or_else(|e| e.into_inner());
        let loopback_base = self.loopback_base.clone();
        let lan_base = self.lan_base.clone();
        let token = self.token.clone();
        let session_slot = self.session.clone();
        let inner = self.inner.clone();

        self.runtime.spawn(async move {
            let result = prepare_and_start(
                &engine,
                &tools,
                &cache,
                options,
                &loopback_base,
                &lan_base,
                &token,
                &device,
                id,
                file_id,
                &title,
            )
            .await;

            let session = match result {
                Ok(session) => session,
                Err(error) => {
                    set_status_locked(
                        &inner,
                        CastStatus::Failed {
                            message: error.to_string(),
                        },
                    );
                    return;
                }
            };

            *session_slot.lock().await = Some(session);
            // Poll until the renderer stops or a newer cast takes over.
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                if current_generation(&inner) != generation {
                    break;
                }
                let mut guard = session_slot.lock().await;
                let Some(session) = guard.as_mut() else { break };
                if session.poll().await.is_err() {
                    // `poll` records the error in the session state.
                }
                let status = status_of(device.name(), &title, session);
                let ended = matches!(session.state(), SessionState::Stopped);
                let failed = matches!(session.state(), SessionState::Error(_));
                drop(guard);
                if ended {
                    clear_status_locked(&inner);
                } else {
                    set_status_locked(&inner, status);
                }
                if ended || failed {
                    break;
                }
            }
        });
    }

    /// Send a control action to the active cast.
    pub fn control(&self, action: CastAction) {
        let session = self.session.clone();
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            let mut guard = session.lock().await;
            let Some(session) = guard.as_mut() else {
                return;
            };
            let result = match action {
                CastAction::Pause => session.pause().await,
                CastAction::Resume => session.resume().await,
                CastAction::Stop => {
                    let result = session.stop().await;
                    *guard = None;
                    clear_status_locked(&inner);
                    result
                }
                CastAction::Seek(position) => session.seek(position).await,
            };
            if let Err(error) = result {
                tracing::warn!(%error, "cast control failed");
            }
        });
    }

    /// Drop the current cast view and session without talking to the renderer.
    pub fn dismiss(&self) {
        clear_status_locked(&self.inner);
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

fn status_of(device: &str, title: &str, session: &CastSession) -> CastStatus {
    if let SessionState::Error(message) = session.state() {
        return CastStatus::Failed {
            message: message.clone(),
        };
    }
    CastStatus::Active {
        device: device.to_string(),
        title: title.to_string(),
        state: session.state().clone(),
        position: session.position(),
        duration: session.duration(),
    }
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
    device: &CastDevice,
    id: usize,
    file_id: usize,
    title: &str,
) -> anyhow::Result<CastSession> {
    let source_url = format!("{loopback_base}/stream/{id}/{file_id}");
    let probe = tools.probe(&source_url).await?;
    let plan = plan(&probe, &options);

    let (url, seekable, converted) = match plan {
        MediaPlan::Original => (original_url(lan_base, token, id, file_id), true, false),
        MediaPlan::Remux => {
            let length = engine.probe_file(id, file_id)?.length;
            let key = CacheKey::for_plan(
                engine.view(id)?.info_hash,
                file_id as u64,
                length,
                &MediaPlan::Remux,
            );
            if cache.path_for(&key).is_file() {
                // A previous live remux finished: serve it with range support.
                (converted_url(lan_base, token, &key.file_name()), true, true)
            } else {
                // Remux on the fly: the LAN endpoint streams fragmented MP4 and
                // tees it into the cache, so a later cast can seek the file.
                (remux_url(lan_base, token, id, file_id), false, true)
            }
        }
        MediaPlan::Transcode { max_height } => {
            let length = engine.probe_file(id, file_id)?.length;
            let key = CacheKey::for_plan(
                engine.view(id)?.info_hash,
                file_id as u64,
                length,
                &MediaPlan::Transcode { max_height },
            );
            let output = cache.path_for(&key);
            if !output.is_file() {
                tools
                    .transcode(&source_url, &probe, max_height, &output)
                    .await?;
                if let Err(error) = cache.evict_to_limit() {
                    tracing::warn!(%error, "could not trim the conversion cache");
                }
            }
            (converted_url(lan_base, token, &key.file_name()), true, true)
        }
        MediaPlan::Unavailable { reason } => anyhow::bail!("{reason}"),
    };

    let metadata = metadata(title, &url, seekable, converted);
    let target = DlnaTarget::new(device)?;
    let mut session = CastSession::new(Box::new(target), title);
    session.start(url, &metadata).await?;
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
pub fn plan_options(settings: &CatalogSettings) -> PlanOptions {
    PlanOptions {
        mode: transcode_mode(settings.conversion_mode),
        max_height: settings.conversion_max_height,
    }
}

/// The LAN URL for a file served directly from the engine.
pub fn original_url(lan_base: &str, token: &str, id: usize, file_id: usize) -> String {
    format!("{lan_base}/media/{token}/original/{id}/{file_id}")
}

/// The LAN URL for a converted file.
pub fn converted_url(lan_base: &str, token: &str, name: &str) -> String {
    format!("{lan_base}/media/{token}/converted/{name}")
}

/// The LAN URL for a live remux of a file.
pub fn remux_url(lan_base: &str, token: &str, id: usize, file_id: usize) -> String {
    format!("{lan_base}/media/{token}/remux/{id}/{file_id}")
}

/// DIDL-Lite metadata for a cast.
pub fn metadata(title: &str, url: &str, seekable: bool, converted: bool) -> String {
    didl::Item::video(title, url, seekable, converted).to_xml()
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
    fn conversion_modes_map_to_the_planner() {
        assert_eq!(transcode_mode(ConversionMode::Off), TranscodeMode::Off);
        assert_eq!(
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
            ..Default::default()
        };
        let options = plan_options(&settings);
        assert_eq!(options.mode, TranscodeMode::RemuxOnly);
        assert_eq!(options.max_height, 720);
    }

    #[test]
    fn media_urls_carry_the_token() {
        assert_eq!(
            original_url("http://10.0.0.5:4000", "tok", 3, 1),
            "http://10.0.0.5:4000/media/tok/original/3/1"
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
        );
        assert!(xml.contains("<dc:title>Episode 1</dc:title>"));
        assert!(xml.contains("protocolInfo=\"http-get:*:"));
        assert!(xml.contains("http://10.0.0.5:4000/media/tok/original/3/1"));
    }

    #[test]
    fn media_urls_include_a_live_remux_route() {
        assert_eq!(
            remux_url("http://10.0.0.5:4000", "tok", 3, 1),
            "http://10.0.0.5:4000/media/tok/remux/3/1"
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
