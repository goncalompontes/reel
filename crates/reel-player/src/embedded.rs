//! The embedded libmpv backend.
//!
//! # Why a worker thread
//!
//! mpv's render API has strict threading rules: only one `mpv_render_*` call at
//! a time, never from inside an mpv callback, and no lock dependency between
//! the renderer and other libmpv users. Rather than juggle that from a GUI
//! thread, one dedicated thread owns *all* libmpv calls — commands, the event
//! queue and rendering — and publishes finished frames into shared state. The
//! UI only ever touches that shared state, so it can never deadlock mpv.
//!
//! # Why the software renderer
//!
//! `MPV_RENDER_API_TYPE_SW` needs no OpenGL context and works on Wayland, X11,
//! Windows and macOS alike. mpv's docs call it "very slow" relative to GPU
//! rendering, but measured on a TigerLake-H it costs ~1.2 ms per 1080p frame
//! (~2 ms with dense subtitles) against a 16.7 ms budget, and ~4.4 ms at 4K.
//! That is cheap enough to keep, and it avoids the GL/FBO interop that would be
//! required to share a texture with the windowing backend.

use std::ffi::{c_int, c_void};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::PlayerError;
use crate::ffi::{self, MpvHandle, MpvLib, MpvRenderContext, mpv_render_param};
use crate::player::{lock_shared, Command, Notify, PlayerConfig, PlayerState, Shared, VideoFrame};

/// How long the worker sleeps when nothing signals it.
///
/// mpv's update callback normally wakes us immediately, but there is a small
/// window where the callback has fired and `mpv_render_context_update` does not
/// yet report the frame. Waiting for tens of milliseconds there costs real
/// frames (measured: 52 fps instead of 59 at 720p60), so the fallback poll is
/// deliberately short. 500 wakeups/second is negligible CPU.
const IDLE_TIMEOUT: Duration = Duration::from_millis(2);

/// Never keep more than this many recycled frame buffers.
const MAX_POOLED_BUFFERS: usize = 3;

// Property observation ids. Deliberately small numbers so they are easy to read
// in a debugger.
const PROP_TIME_POS: u64 = 1;
const PROP_DURATION: u64 = 2;
const PROP_PAUSE: u64 = 3;
const PROP_EOF: u64 = 4;
const PROP_CORE_IDLE: u64 = 5;
const PROP_WIDTH: u64 = 6;
const PROP_HEIGHT: u64 = 7;
const PROP_VOLUME: u64 = 8;
const PROP_SPEED: u64 = 9;
const PROP_BUFFERING: u64 = 10;
const PROP_PAUSED_FOR_CACHE: u64 = 11;

/// mpv objects, created **and** destroyed on the player worker thread.
///
/// # Why single-threaded ownership
///
/// The client API allows creating these elsewhere, but ownership across threads
/// is the classic source of teardown crashes (a callback firing while mpv is
/// being torn down). Creating, using and freeing them on one thread keeps that
/// simple to reason about.
pub(crate) struct EmbeddedCore {
    pub(crate) lib: Arc<MpvLib>,
    pub(crate) handle: MpvHandle,
    pub(crate) render: MpvRenderContext,
    pub(crate) notify: Arc<Notify>,
    default_start: Option<f64>,
}

impl Drop for EmbeddedCore {
    fn drop(&mut self) {
        unsafe {
            // Free the render context first: it disables the VO and stops the
            // update callback before the core goes away.
            if !self.render.is_null() {
                (self.lib.mpv_render_context_free)(self.render);
            }
            if !self.handle.is_null() {
                (self.lib.mpv_terminate_destroy)(self.handle);
            }
        }
    }
}

/// Called from mpv's internal threads. Must not call libmpv and must not panic.
unsafe extern "C" fn on_wakeup(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let notify = unsafe { &*(data as *const Notify) };
    notify.signal();
}

impl EmbeddedCore {
    /// Create and initialise mpv. Must be called on the thread that will use it.
    pub(crate) fn create(config: &PlayerConfig, notify: Arc<Notify>) -> Result<Self, PlayerError> {
        let lib = Arc::new(MpvLib::load()?);
        let handle = unsafe { (lib.mpv_create)() };
        if handle.is_null() {
            return Err(PlayerError::Mpv("mpv_create returned null".into()));
        }

        // A partially initialised core still needs destroying on early return.
        let mut core = EmbeddedCore {
            lib: lib.clone(),
            handle,
            render: std::ptr::null_mut(),
            notify,
            default_start: config.start_position,
        };

        core.apply_options(config)?;

        let rc = unsafe { (lib.mpv_initialize)(handle) };
        if rc < 0 {
            return Err(PlayerError::Mpv(format!(
                "mpv_initialize failed: {}",
                lib.error_string(rc)
            )));
        }

        core.create_render_context()?;
        core.observe_properties();
        core.install_callbacks();

        Ok(core)
    }

    fn apply_options(&mut self, config: &PlayerConfig) -> Result<(), PlayerError> {
        let options: &[(&str, &str)] = &[
            // Never read the user's mpv.conf: a GUI needs predictable behaviour.
            ("config", "no"),
            // The render API requires the libmpv VO and forbids a real window.
            ("vo", "libmpv"),
            ("force-window", "no"),
            // Stay alive between files and hold the last frame at the end.
            ("idle", "yes"),
            ("keep-open", "yes"),
            ("terminal", "no"),
            ("msg-level", "all=warn"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("audio-display", "no"),
            ("osd-level", "0"),
            // Enough cushion that seeking into a torrent does not stall the
            // demuxer while pieces are still arriving.
            ("cache", "yes"),
            ("demuxer-max-bytes", "64MiB"),
            ("demuxer-readahead-secs", "30"),
        ];

        for (name, value) in options {
            self.lib.set_option(self.handle, name, value)?;
        }

        self.lib.set_option(self.handle, "hwdec", &config.hwdec)?;

        if config.no_audio {
            self.lib.set_option(self.handle, "audio", "no")?;
        } else if config.mute {
            self.lib.set_option(self.handle, "mute", "yes")?;
        }
        if !config.no_audio && !config.mute {
            self.lib
                .set_option(self.handle, "volume", &config.volume.to_string())?;
        }

        for (name, value) in &config.mpv_options {
            self.lib.set_option(self.handle, name, value)?;
        }

        Ok(())
    }

    fn create_render_context(&mut self) -> Result<(), PlayerError> {
        let mut params = [
            mpv_render_param::new(
                ffi::MPV_RENDER_PARAM_API_TYPE,
                ffi::RENDER_API_TYPE_SW.as_ptr() as *mut c_void,
            ),
            mpv_render_param::new(ffi::MPV_RENDER_PARAM_INVALID, std::ptr::null_mut()),
        ];

        let mut render: MpvRenderContext = std::ptr::null_mut();
        let rc = unsafe {
            (self.lib.mpv_render_context_create)(&mut render, self.handle, params.as_mut_ptr())
        };
        if rc < 0 {
            return Err(PlayerError::Mpv(format!(
                "mpv_render_context_create(SW) failed: {}",
                self.lib.error_string(rc)
            )));
        }
        self.render = render;
        Ok(())
    }

    fn observe_properties(&self) {
        use ffi::{MPV_FORMAT_DOUBLE, MPV_FORMAT_FLAG, MPV_FORMAT_INT64};
        let props: &[(u64, &str, c_int)] = &[
            (PROP_TIME_POS, "time-pos", MPV_FORMAT_DOUBLE),
            (PROP_DURATION, "duration", MPV_FORMAT_DOUBLE),
            (PROP_PAUSE, "pause", MPV_FORMAT_FLAG),
            (PROP_EOF, "eof-reached", MPV_FORMAT_FLAG),
            (PROP_CORE_IDLE, "core-idle", MPV_FORMAT_FLAG),
            (PROP_WIDTH, "width", MPV_FORMAT_INT64),
            (PROP_HEIGHT, "height", MPV_FORMAT_INT64),
            (PROP_VOLUME, "volume", MPV_FORMAT_DOUBLE),
            (PROP_SPEED, "speed", MPV_FORMAT_DOUBLE),
            (PROP_BUFFERING, "cache-buffering-state", MPV_FORMAT_INT64),
            (PROP_PAUSED_FOR_CACHE, "paused-for-cache", MPV_FORMAT_FLAG),
        ];
        for (id, name, format) in props {
            self.lib.observe(self.handle, *id, name, *format);
        }
    }

    fn install_callbacks(&self) {
        let ptr = Arc::as_ptr(&self.notify) as *mut c_void;
        unsafe {
            (self.lib.mpv_set_wakeup_callback)(self.handle, on_wakeup, ptr);
            (self.lib.mpv_render_context_set_update_callback)(self.render, on_wakeup, ptr);
        }
    }
}

// ------------------------------------------------------------------ worker

/// Owns the mpv core for its whole lifetime and drives it.
struct Worker {
    core: EmbeddedCore,
    shared: Arc<Mutex<Shared>>,
    /// Recycled frame buffers, to keep the render loop allocation free.
    pool: Vec<Vec<u8>>,
    /// A superseded frame whose buffer we could not reclaim yet.
    pending_recycle: Option<Arc<VideoFrame>>,
    /// Size we last successfully rendered at.
    last_size: Option<(u32, u32)>,
    /// Applied once, after the first file loads.
    pending_start: Option<f64>,
    seen_generation: u64,
    shutdown: bool,
    /// Avoids logging the same render error at 60 Hz.
    logged_render_error: bool,
}

/// Entry point for the player thread: create mpv, report readiness, then drive
/// it until told to stop. Everything mpv-related happens on this one thread.
pub(crate) fn run_worker(
    config: PlayerConfig,
    shared: Arc<Mutex<Shared>>,
    commands: Receiver<Command>,
    notify: Arc<Notify>,
    ready: Sender<Result<(), PlayerError>>,
) {
    let core = match EmbeddedCore::create(&config, notify) {
        Ok(core) => core,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let mut worker = Worker {
        pending_start: core.default_start,
        core,
        shared,
        pool: Vec::new(),
        pending_recycle: None,
        last_size: None,
        seen_generation: 0,
        shutdown: false,
        logged_render_error: false,
    };
    worker.run(&commands);
}

impl Worker {
    fn run(&mut self, commands: &Receiver<Command>) {
        loop {
            self.drain_commands(commands);
            if self.shutdown {
                break;
            }

            // Non-blocking drain: the wakeup callback tells us when to look.
            self.drain_events();
            if self.shutdown {
                break;
            }

            self.render_if_needed();

            if self.shutdown {
                break;
            }
            // Woken immediately by mpv events, render updates or command sends.
            self.core.notify.wait(&mut self.seen_generation, IDLE_TIMEOUT);
        }
    }

    fn drain_commands(&mut self, commands: &Receiver<Command>) {
        loop {
            match commands.try_recv() {
                Ok(Command::Load { url, start_position }) => {
                    self.load(&url, start_position);
                }
                Ok(Command::SetPaused(paused)) => self.set_flag("pause", paused),
                Ok(Command::SeekAbsolute(seconds)) => self.seek(seconds, "absolute"),
                Ok(Command::SeekRelative(delta)) => self.seek(delta, "relative"),
                Ok(Command::SetVolume(volume)) => self.set_volume(volume),
                Ok(Command::Stop) => self.stop(),
                Ok(Command::Shutdown) => self.shutdown = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.shutdown = true;
                    break;
                }
            }
        }
    }

    fn load(&mut self, url: &str, start_position: Option<f64>) {
        if let Err(e) = self.core.lib.command(self.core.handle, &["loadfile", url, "replace"]) {
            self.set_error(format!("loading {url}: {e}"));
            return;
        }
        // Seek once the file has actually loaded; seeking earlier is a no-op.
        self.pending_start = start_position.or(self.pending_start.take());

        let mut guard = lock_shared(&self.shared);
        let volume = guard.state.volume;
        guard.state = PlayerState {
            loaded: true,
            url: Some(url.to_string()),
            volume,
            ..Default::default()
        };
        drop(guard);

        self.last_size = None;
        self.request_repaint();
    }

    fn set_flag(&mut self, name: &str, value: bool) {
        if let Err(e) = self.core.lib.set_flag(self.core.handle, name, value) {
            tracing::warn!(error = %e, name, "failed to set mpv flag");
        }
    }

    fn set_volume(&mut self, volume: f64) {
        if let Err(e) = self.core.lib.set_double(self.core.handle, "volume", volume) {
            tracing::warn!(error = %e, "failed to set volume");
        }
    }

    fn seek(&mut self, amount: f64, mode: &str) {
        let amount = amount.to_string();
        if let Err(e) = self
            .core
            .lib
            .command(self.core.handle, &["seek", &amount, mode])
        {
            tracing::warn!(error = %e, %mode, "seek failed");
        }
    }

    fn stop(&mut self) {
        if let Err(e) = self.core.lib.command(self.core.handle, &["stop"]) {
            tracing::warn!(error = %e, "stop failed");
        }
        let mut guard = lock_shared(&self.shared);
        guard.state.loaded = false;
        guard.state.paused = false;
        drop(guard);
        self.request_repaint();
    }

    fn set_error(&mut self, message: String) {
        tracing::warn!("{message}");
        lock_shared(&self.shared).state.error = Some(message);
        self.request_repaint();
    }

    fn drain_events(&mut self) {
        loop {
            let event = unsafe { (self.core.lib.mpv_wait_event)(self.core.handle, 0.0) };
            if event.is_null() {
                break;
            }
            let event = unsafe { *event };
            if event.event_id == ffi::MPV_EVENT_NONE {
                break;
            }
            self.handle_event(&event);
        }
    }

    fn handle_event(&mut self, event: &ffi::mpv_event) {
        match event.event_id {
            ffi::MPV_EVENT_PROPERTY_CHANGE => self.handle_property_change(event),
            ffi::MPV_EVENT_FILE_LOADED => {
                lock_shared(&self.shared).state.loaded = true;
                if let Some(position) = self.pending_start.take() {
                    if position > 0.0 {
                        self.seek(position, "absolute");
                    }
                }
                self.last_size = None;
                self.request_repaint();
            }
            ffi::MPV_EVENT_END_FILE => {
                let mut error = None;
                if !event.data.is_null() {
                    let end = unsafe { &*(event.data as *const ffi::mpv_event_end_file) };
                    if end.error != 0 {
                        error = Some(self.core.lib.error_string(end.error));
                    }
                }
                let mut guard = lock_shared(&self.shared);
                guard.state.eof = true;
                guard.state.loaded = false;
                if let Some(error) = error {
                    guard.state.error = Some(error);
                }
                drop(guard);
                self.request_repaint();
            }
            ffi::MPV_EVENT_SHUTDOWN => {
                self.shutdown = true;
            }
            _ => {}
        }
    }

    fn handle_property_change(&mut self, event: &ffi::mpv_event) {
        if event.data.is_null() {
            return;
        }
        let prop = unsafe { &*(event.data as *const ffi::mpv_event_property) };

        // Reads are only valid when the format matches what we asked for; when
        // a property is unavailable mpv reports MPV_FORMAT_NONE and we keep the
        // previous value.
        let double = || prop_double(prop);
        let flag = || prop_flag(prop);
        let int = || prop_int(prop);

        let mut guard = lock_shared(&self.shared);
        match event.reply_userdata {
            PROP_TIME_POS => {
                if let Some(v) = double() {
                    guard.state.position = v;
                }
            }
            PROP_DURATION => {
                if let Some(v) = double() {
                    guard.state.duration = (v > 0.0).then_some(v);
                }
            }
            PROP_PAUSE => {
                if let Some(v) = flag() {
                    guard.state.paused = v;
                }
            }
            PROP_EOF => {
                if let Some(v) = flag() {
                    guard.state.eof = v;
                }
            }
            PROP_CORE_IDLE => {
                if let Some(v) = flag() {
                    guard.state.idle = v;
                }
            }
            PROP_WIDTH => {
                if let Some(v) = int() {
                    guard.state.video_width = (v > 0).then_some(v as u32);
                }
            }
            PROP_HEIGHT => {
                if let Some(v) = int() {
                    guard.state.video_height = (v > 0).then_some(v as u32);
                }
            }
            PROP_VOLUME => {
                if let Some(v) = double() {
                    guard.state.volume = v;
                }
            }
            PROP_SPEED => {
                if let Some(v) = double() {
                    guard.state.speed = v;
                }
            }
            PROP_BUFFERING => {
                if let Some(v) = int() {
                    guard.state.buffering_percent = Some(v as f64);
                }
            }
            PROP_PAUSED_FOR_CACHE => {
                if let Some(v) = flag() {
                    guard.state.paused_for_cache = v;
                }
            }
            _ => {}
        }

        let position_changed = matcher_changed(event.reply_userdata);
        drop(guard);
        if position_changed {
            // Keeps the seek bar moving without the UI polling on a timer.
            self.request_repaint();
        }
    }

    /// Render the current video frame, if the UI wants one and mpv has one.
    fn render_if_needed(&mut self) {
        let Some(size) = lock_shared(&self.shared).target_size else {
            self.last_size = None;
            return;
        };
        let (width, height) = size;
        if width == 0 || height == 0 {
            return;
        }

        let update_start = std::time::Instant::now();
        let flags = unsafe { (self.core.lib.mpv_render_context_update)(self.core.render) };
        let update_to_render_ms = (std::time::Instant::now() - update_start).as_secs_f64() * 1000.0;
        let size_changed = self.last_size != Some(size);
        if flags & ffi::MPV_RENDER_UPDATE_FRAME == 0 && !size_changed {
            return;
        }

        // Reclaim the previous frame's buffer if the consumer has released it.
        // Without a free buffer we allocate, so a stale reference only costs an
        // allocation, never correctness.
        let deferred = self.pending_recycle.take();
        self.recycle(deferred);

        let needed = width as usize * height as usize * 4;
        let mut buffer = self.pool.pop().unwrap_or_default();
        if buffer.len() != needed {
            buffer.resize(needed, 0);
        }

        let mut mpv_size: [c_int; 2] = [width as c_int, height as c_int];
        let mut stride: usize = width as usize * 4;
        // 0 = block until the frame's presentation time. That is what paces
        // playback to the video's frame rate instead of spinning.
        let mut block_for_target_time: c_int = 0;
        // "rgb0" is the documented, guaranteed-supported format. Its fourth
        // byte is explicitly garbage, so it is forced to 255 below.
        let format = c"rgb0";

        let mut params = [
            mpv_render_param::new(ffi::SW_SIZE, mpv_size.as_mut_ptr() as *mut c_void),
            mpv_render_param::new(ffi::SW_FORMAT, format.as_ptr() as *mut c_void),
            mpv_render_param::new(ffi::SW_STRIDE, &mut stride as *mut usize as *mut c_void),
            mpv_render_param::new(ffi::SW_POINTER, buffer.as_mut_ptr() as *mut c_void),
            mpv_render_param::new(
                ffi::BLOCK_FOR_TARGET_TIME,
                &mut block_for_target_time as *mut c_int as *mut c_void,
            ),
            mpv_render_param::new(ffi::MPV_RENDER_PARAM_INVALID, std::ptr::null_mut()),
        ];

        let render_start = std::time::Instant::now();
        let rc = unsafe {
            (self.core.lib.mpv_render_context_render)(self.core.render, params.as_mut_ptr())
        };
        let render_done = std::time::Instant::now();
        if rc < 0 {
            // Expected before the first frame is decoded; retry next tick.
            self.pool.push(buffer);
            self.last_size = None;
            if !self.logged_render_error {
                self.logged_render_error = true;
                tracing::debug!(
                    error = %self.core.lib.error_string(rc),
                    "mpv has no frame to render yet"
                );
            }
            return;
        }
        self.logged_render_error = false;

        // The UI's texture format is RGBA with alpha = 255.
        let alpha_start = std::time::Instant::now();
        for pixel in buffer.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        let alpha_done = std::time::Instant::now();

        self.last_size = Some(size);
        let frame = Arc::new(VideoFrame {
            width,
            height,
            rgba: buffer,
        });

        // Publish *before* recycling: the frame must stay visible in the shared
        // slot, and mpv's render call above can block for a whole frame period.
        let previous = {
            let mut guard = lock_shared(&self.shared);
            guard.frame_sequence = guard.frame_sequence.wrapping_add(1);
            guard.frame.replace(frame)
        };
        tracing::debug!(
            update_ms = update_to_render_ms,
            render_ms = (render_done - render_start).as_secs_f64() * 1000.0,
            alpha_ms = (alpha_done - alpha_start).as_secs_f64() * 1000.0,
            pool_len = self.pool.len(),
            deferred = self.pending_recycle.is_some(),
            "frame rendered"
        );
        self.recycle(previous);
        self.request_repaint();
    }

    /// Put a superseded frame's buffer back in the pool, deferring the attempt
    /// if a consumer still holds a reference to it.
    fn recycle(&mut self, candidate: Option<Arc<VideoFrame>>) {
        let Some(candidate) = candidate else {
            return;
        };
        match Arc::try_unwrap(candidate) {
            Ok(frame) => {
                if self.pool.len() < MAX_POOLED_BUFFERS {
                    self.pool.push(frame.rgba);
                }
            }
            // Still in use; try again next time round.
            Err(still_shared) => self.pending_recycle = Some(still_shared),
        }
    }

    fn request_repaint(&self) {
        let callback = lock_shared(&self.shared).repaint_callback();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// Properties whose changes should move the UI immediately.
fn matcher_changed(id: u64) -> bool {
    matches!(
        id,
        PROP_TIME_POS | PROP_PAUSE | PROP_BUFFERING | PROP_PAUSED_FOR_CACHE
    )
}

fn prop_double(prop: &ffi::mpv_event_property) -> Option<f64> {
    (prop.format == ffi::MPV_FORMAT_DOUBLE && !prop.data.is_null())
        .then(|| unsafe { *(prop.data as *const f64) })
}

fn prop_flag(prop: &ffi::mpv_event_property) -> Option<bool> {
    (prop.format == ffi::MPV_FORMAT_FLAG && !prop.data.is_null())
        .then(|| unsafe { *(prop.data as *const c_int) != 0 })
}

fn prop_int(prop: &ffi::mpv_event_property) -> Option<i64> {
    (prop.format == ffi::MPV_FORMAT_INT64 && !prop.data.is_null())
        .then(|| unsafe { *(prop.data as *const i64) })
}
