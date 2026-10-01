use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::error::PlayerError;

/// A decoded video frame in RGBA8, ready to hand to a GPU texture.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, always with alpha = 255.
    pub rgba: Vec<u8>,
}

impl VideoFrame {
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

/// A frame plus the sequence number it was published with. Consumers compare
/// the sequence to avoid re-uploading an unchanged frame.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub sequence: u64,
    pub frame: Arc<VideoFrame>,
}

/// One selectable track (audio or subtitle) reported by the player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// mpv's track id, used with `sid`/`aid`.
    pub id: i64,
    /// `audio`, `sub` or `video`.
    pub kind: String,
    pub title: Option<String>,
    pub lang: Option<String>,
    /// True for a file added with `sub-add`, rather than embedded in the video.
    pub external: bool,
    pub selected: bool,
}

impl Track {
    /// A short human label: title, language, or the id.
    pub fn label(&self) -> String {
        match (self.title.as_deref(), self.lang.as_deref()) {
            (Some(title), Some(lang)) if !title.is_empty() => format!("{title} ({lang})"),
            (Some(title), _) if !title.is_empty() => title.to_string(),
            (_, Some(lang)) => lang.to_string(),
            _ => format!("track {}", self.id),
        }
    }
}

/// Snapshot of the player's transport state.
#[derive(Debug, Clone, Default)]
pub struct PlayerState {
    /// True once `load` has been called and playback has not ended.
    pub loaded: bool,
    pub paused: bool,
    pub position: f64,
    pub duration: Option<f64>,
    /// Playback reached the end of the file.
    pub eof: bool,
    /// The core has nothing to play.
    pub idle: bool,
    /// Decoded video size, if the current file has video.
    pub video_width: Option<u32>,
    pub video_height: Option<u32>,
    pub volume: f64,
    pub speed: f64,
    /// Percentage of the demuxer cache filled, when buffering.
    pub buffering_percent: Option<f64>,
    pub paused_for_cache: bool,
    /// Human readable failure, surfaced from mpv.
    pub error: Option<String>,
    pub url: Option<String>,
    /// Subtitle tracks the current file offers (embedded and sidecar).
    pub subtitle_tracks: Vec<Track>,
    /// Audio tracks the current file offers.
    pub audio_tracks: Vec<Track>,
    /// Whether subtitles are currently displayed.
    pub subtitles_visible: bool,
    /// Subtitle timing offset in seconds.
    pub subtitle_delay: f64,
    /// Selected subtitle track id, if any.
    pub active_subtitle: Option<i64>,
    /// Selected audio track id, if any.
    pub active_audio: Option<i64>,
    /// Video aspect override, e.g. `16:9`, or `None` for the source aspect.
    pub aspect_override: Option<String>,
}

impl PlayerState {
    /// Position/duration as a 0..=1 fraction, when the duration is known.
    pub fn progress(&self) -> Option<f64> {
        let duration = self.duration?;
        if duration <= 0.0 {
            return None;
        }
        Some((self.position / duration).clamp(0.0, 1.0))
    }

    pub fn aspect_ratio(&self) -> Option<f32> {
        let (w, h) = (self.video_width?, self.video_height?);
        if h == 0 {
            return None;
        }
        Some(w as f32 / h as f32)
    }
}

/// Which playback mechanism is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// libmpv renders frames into memory that we upload as a texture.
    Embedded,
    /// A separate player process owns its own window.
    External,
    /// Nothing could be set up; `Player::unavailable_reason` explains why.
    Unavailable,
}

/// How to create a [`Player`].
#[derive(Debug, Clone)]
pub struct PlayerConfig {
    /// Try to load libmpv and render in-process. If this fails, the external
    /// backend is used instead.
    pub prefer_embedded: bool,
    /// Command used by the external backend, e.g. `mpv` or `vlc`.
    pub external_command: String,
    /// Extra libmpv options, applied before initialisation.
    pub mpv_options: Vec<(String, String)>,
    /// Initial volume, 0..=100.
    pub volume: f64,
    /// Start playback at this many seconds.
    pub start_position: Option<f64>,
    /// Mute audio (used by headless tooling).
    pub mute: bool,
    /// Do not create an audio output at all.
    pub no_audio: bool,
    /// Hardware decoding mode passed to mpv (`auto-safe`, `no`, `auto`, ...).
    pub hwdec: String,
    /// Turn subtitles on when the file has them.
    pub subtitles_enabled: bool,
    /// Preferred subtitle language, passed to mpv as `slang`.
    pub subtitle_language: Option<String>,
    /// Preferred audio language, passed to mpv as `alang`.
    pub audio_language: Option<String>,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            prefer_embedded: true,
            external_command: "mpv".to_string(),
            mpv_options: Vec::new(),
            volume: 100.0,
            start_position: None,
            mute: false,
            no_audio: false,
            hwdec: "auto-safe".to_string(),
            subtitles_enabled: true,
            subtitle_language: None,
            audio_language: None,
        }
    }
}

// ------------------------------------------------------------------- shared

/// State shared between the worker thread and the UI.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) state: PlayerState,
    /// Size (physical pixels) the UI wants the video rendered at. `None`
    /// suspends rendering entirely.
    pub(crate) target_size: Option<(u32, u32)>,
    pub(crate) frame: Option<Arc<VideoFrame>>,
    pub(crate) frame_sequence: u64,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Shared {
    pub(crate) fn repaint_callback(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.repaint.clone()
    }
}

/// Lock the shared state, tolerating poisoning by a panicking UI.
pub(crate) fn lock_shared(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Wake-up channel shared with mpv's own callback threads.
///
/// mpv calls these callbacks from internal threads, so `signal` must never
/// panic and must not touch mpv.
pub(crate) struct Notify {
    generation: Mutex<u64>,
    condvar: Condvar,
}

impl Notify {
    pub(crate) fn new() -> Self {
        Self {
            generation: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }

    pub(crate) fn signal(&self) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *generation = generation.wrapping_add(1);
        self.condvar.notify_all();
    }

    /// Block until signalled or the timeout elapses. `seen` tracks the last
    /// generation observed by the caller so stale signals do not spin.
    pub(crate) fn wait(&self, seen: &mut u64, timeout: Duration) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        if *generation == *seen {
            let (guard, _) = self
                .condvar
                .wait_timeout(generation, timeout)
                .unwrap_or_else(|e| e.into_inner());
            generation = guard;
        }
        *seen = *generation;
    }
}

// -------------------------------------------------------- worker commands

#[derive(Debug)]
pub(crate) enum Command {
    Load {
        url: String,
        start_position: Option<f64>,
    },
    SetPaused(bool),
    SeekAbsolute(f64),
    SeekRelative(f64),
    SetVolume(f64),
    SetSubtitle(Option<i64>),
    SetAudio(Option<i64>),
    SetSubtitleVisible(bool),
    SetSubtitleDelay(f64),
    SetSpeed(f64),
    SetAspectOverride(Option<String>),
    /// Replace the sidecar subtitles to load once the file is ready.
    SetSubtitles(Vec<(String, String)>),
    Stop,
    Shutdown,
}

// ------------------------------------------------------------------- player

/// A native video player.
///
/// With the [`Backend::Embedded`] backend, mpv decodes and renders frames into
/// memory on a worker thread, and the caller pulls the latest frame to display.
/// This is what lets a GUI toolkit show video without owning an OpenGL context.
pub struct Player {
    shared: Arc<Mutex<Shared>>,
    commands: Option<std_mpsc::Sender<Command>>,
    worker: Option<std::thread::JoinHandle<()>>,
    backend: Backend,
    reason: Option<String>,
    external: Option<crate::external::ExternalPlayer>,
    current_url: Arc<Mutex<Option<String>>>,
    /// Signalled to wake the worker immediately when a command is queued.
    worker_notify: Option<Arc<Notify>>,
}

impl Player {
    pub fn new(config: PlayerConfig) -> Result<Self, PlayerError> {
        let shared = Arc::new(Mutex::new(Shared::default()));

        if config.prefer_embedded {
            // The worker owns mpv from creation to destruction; we wait for it
            // to report whether that worked so errors surface synchronously.
            let notify = Arc::new(Notify::new());
            let (tx, rx) = std_mpsc::channel();
            let (ready_tx, ready_rx) = std_mpsc::channel();
            let shared_for_worker = shared.clone();
            let notify_for_worker = notify.clone();
            let config_for_worker = config.clone();

            let worker = std::thread::Builder::new()
                .name("reel-player".to_string())
                .spawn(move || {
                    crate::embedded::run_worker(
                        config_for_worker,
                        shared_for_worker,
                        rx,
                        notify_for_worker,
                        ready_tx,
                    )
                })
                .map_err(PlayerError::Io)?;

            match ready_rx.recv_timeout(Duration::from_secs(30)) {
                Ok(Ok(())) => {
                    tracing::info!("embedded playback ready (libmpv)");
                    return Ok(Self {
                        shared,
                        commands: Some(tx),
                        worker: Some(worker),
                        backend: Backend::Embedded,
                        reason: None,
                        external: None,
                        current_url: Arc::new(Mutex::new(None)),
                        worker_notify: Some(notify),
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "embedded playback unavailable");
                    let _ = worker.join();
                    return Ok(Self::external(config, Some(e.to_string())));
                }
                Err(e) => {
                    let _ = worker.join();
                    return Err(PlayerError::Mpv(format!(
                        "player thread did not start: {e}"
                    )));
                }
            }
        }

        Ok(Self::external(config, None))
    }

    fn external(config: PlayerConfig, reason: Option<String>) -> Self {
        match crate::external::ExternalPlayer::new(config.external_command.clone()) {
            Ok(external) => Self {
                shared: Arc::new(Mutex::new(Shared::default())),
                commands: None,
                worker: None,
                backend: Backend::External,
                reason,
                external: Some(external),
                current_url: Arc::new(Mutex::new(None)),
                worker_notify: None,
            },
            Err(e) => {
                let reason = match reason {
                    Some(r) => format!("{r}; external player unusable: {e}"),
                    None => format!("external player unusable: {e}"),
                };
                Self {
                    shared: Arc::new(Mutex::new(Shared::default())),
                    commands: None,
                    worker: None,
                    backend: Backend::Unavailable,
                    reason: Some(reason),
                    external: None,
                    current_url: Arc::new(Mutex::new(None)),
                    worker_notify: None,
                }
            }
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Why embedded playback was not used, if it was not.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn is_embedded(&self) -> bool {
        self.backend == Backend::Embedded
    }

    /// Whether a handed-off external player process is still running. Always
    /// `false` for the embedded and unavailable backends.
    pub fn is_playing_externally(&self) -> bool {
        self.external
            .as_ref()
            .is_some_and(|external| external.is_running())
    }

    /// Register a callback invoked when a new frame is available or state
    /// changed. A GUI uses this to request a repaint.
    pub fn set_repaint_callback(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        lock_shared(&self.shared).repaint = Some(callback);
    }

    /// Tell the player the size (in physical pixels) of the video surface.
    /// `None` suspends frame production.
    pub fn set_target_size(&self, size: Option<(u32, u32)>) {
        let mut guard = lock_shared(&self.shared);
        if guard.target_size != size {
            guard.target_size = size;
        }
    }

    pub fn load(&self, url: &str) -> Result<(), PlayerError> {
        *self.current_url.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.to_string());

        match self.backend {
            Backend::Embedded => {
                lock_shared(&self.shared).state.url = Some(url.to_string());
                self.send(Command::Load {
                    url: url.to_string(),
                    start_position: None,
                })
            }
            Backend::External => {
                let external = self.external.as_ref().expect("external backend");
                external.play(url)?;
                let mut guard = lock_shared(&self.shared);
                guard.state = PlayerState {
                    loaded: true,
                    url: Some(url.to_string()),
                    volume: 100.0,
                    ..Default::default()
                };
                drop(guard);
                self.notify();
                Ok(())
            }
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// Start playback at a specific position.
    pub fn load_at(&self, url: &str, start_position: f64) -> Result<(), PlayerError> {
        *self.current_url.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.to_string());
        match self.backend {
            Backend::Embedded => {
                lock_shared(&self.shared).state.url = Some(url.to_string());
                self.send(Command::Load {
                    url: url.to_string(),
                    start_position: Some(start_position),
                })
            }
            Backend::External => self.load(url),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetPaused(paused)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn toggle_pause(&self) -> Result<(), PlayerError> {
        let paused = lock_shared(&self.shared).state.paused;
        self.set_paused(!paused)
    }

    pub fn seek_absolute(&self, seconds: f64) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SeekAbsolute(seconds.max(0.0))),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn seek_relative(&self, delta: f64) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SeekRelative(delta)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        let volume = volume.clamp(0.0, 130.0);
        match self.backend {
            Backend::Embedded => self.send(Command::SetVolume(volume)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// Select a subtitle track by id, or `None` to turn subtitles off.
    pub fn set_subtitle(&self, id: Option<i64>) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetSubtitle(id)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// Select an audio track by id, or `None` to let the player choose.
    pub fn set_audio(&self, id: Option<i64>) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetAudio(id)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn set_subtitles_visible(&self, visible: bool) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetSubtitleVisible(visible)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// Shift subtitle timing by `seconds` (positive is later).
    pub fn set_subtitle_delay(&self, seconds: f64) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetSubtitleDelay(seconds)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn set_speed(&self, speed: f64) -> Result<(), PlayerError> {
        let speed = speed.clamp(0.1, 8.0);
        match self.backend {
            Backend::Embedded => self.send(Command::SetSpeed(speed)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// Override the displayed aspect ratio, or `None` to use the source's.
    pub fn set_aspect_override(&self, aspect: Option<String>) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetAspectOverride(aspect)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// Register sidecar subtitle files as `(url, title)`. They are loaded once
    /// the video is ready.
    pub fn set_subtitles(&self, subtitles: Vec<(String, String)>) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::SetSubtitles(subtitles)),
            Backend::External => Err(PlayerError::ExternalControl),
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    pub fn stop(&self) -> Result<(), PlayerError> {
        match self.backend {
            Backend::Embedded => self.send(Command::Stop),
            Backend::External => {
                if let Some(external) = self.external.as_ref() {
                    external.stop()?;
                }
                lock_shared(&self.shared).state.loaded = false;
                self.notify();
                Ok(())
            }
            Backend::Unavailable => Err(PlayerError::NoBackend),
        }
    }

    /// The latest transport state.
    pub fn state(&self) -> PlayerState {
        lock_shared(&self.shared).state.clone()
    }

    /// The most recently rendered frame, if the sequence number is newer than
    /// `after`. Pass the last sequence you uploaded; `0` gets the first frame.
    pub fn frame_after(&self, after: u64) -> Option<FrameSnapshot> {
        let guard = lock_shared(&self.shared);
        if guard.frame_sequence <= after {
            return None;
        }
        Some(FrameSnapshot {
            sequence: guard.frame_sequence,
            frame: guard.frame.clone()?,
        })
    }

    pub fn frame_sequence(&self) -> u64 {
        lock_shared(&self.shared).frame_sequence
    }

    fn send(&self, command: Command) -> Result<(), PlayerError> {
        let Some(tx) = self.commands.as_ref() else {
            return Err(PlayerError::NoBackend);
        };
        tx.send(command)
            .map_err(|_| PlayerError::Mpv("player worker has stopped".into()))?;
        self.notify();
        Ok(())
    }

    /// Wake the worker (so queued commands are picked up immediately) and the
    /// UI (so it repaints).
    fn notify(&self) {
        if let Some(notify) = self.worker_notify.as_ref() {
            notify.signal();
        }
        let callback = lock_shared(&self.shared).repaint_callback();
        if let Some(callback) = callback {
            callback();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(tx) = self.commands.take() {
            let _ = tx.send(Command::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(external) = self.external.as_mut() {
            external.shutdown();
        }
    }
}
