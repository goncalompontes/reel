//! Video playback for the UI: owns a [`reel_player::Player`], turns its frames
//! into an egui texture, and draws the transport controls.
//!
//! The controller does not create panels: the app decides the layout and calls
//! [`PlayerController::video_ui`] and [`PlayerController::controls_ui`].

use std::sync::Arc;

use egui::{Color32, CornerRadius, Painter, Sense, Vec2};
use reel_player::{Backend, Player, PlayerConfig, PlayerState};

use crate::backend::PlayerCapability;
use crate::theme;

/// What is currently being watched.
#[derive(Debug, Clone)]
pub struct PlaybackInfo {
    pub torrent_id: usize,
    /// Which file of the torrent is playing. Watch positions are per file, so a
    /// series resumes the episode that was actually being watched.
    pub file_id: usize,
    /// Stable across restarts, and the key watch positions are stored under.
    pub info_hash: String,
    pub title: String,
    pub file_name: String,
    /// Duration known from metadata, used until the player reports its own.
    pub fallback_duration: Option<f64>,
    /// Resume from here rather than the beginning.
    pub start_at: Option<f64>,
    /// Sidecar subtitle files to register with the player, as `(url, title)`.
    pub subtitles: Vec<(String, String)>,
}

/// Playback preferences read from the canonical settings.
#[derive(Debug, Clone, Default)]
pub struct PlaybackPreferences {
    /// Turn subtitles on when a file has them.
    pub subtitles_enabled: bool,
    /// Preferred subtitle language (`slang`), e.g. `en`.
    pub subtitle_language: Option<String>,
    /// Starting volume, 0–130.
    pub volume: f32,
}

/// Engine numbers the controls display alongside the player's own state.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaybackStats {
    /// Bytes per second arriving from peers.
    pub download_bps: u64,
    pub peers: u32,
    /// How much of the file is on disk, 0..=100.
    pub file_percent: f64,
}

pub struct PlayerController {
    player: Option<Player>,
    texture: Option<egui::TextureHandle>,
    last_sequence: u64,
    current: Option<PlaybackInfo>,
    error: Option<String>,
    volume: f32,
    /// Non-empty while the user drags the seek bar, so the reported position
    /// does not fight the drag.
    scrub_position: Option<f64>,
    created_with: Option<PlayerCapability>,
    preferences: PlaybackPreferences,
    /// Whether the window is currently fullscreen.
    fullscreen: bool,
}

impl Default for PlayerController {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerController {
    pub fn new() -> Self {
        Self {
            player: None,
            texture: None,
            last_sequence: 0,
            current: None,
            error: None,
            volume: 100.0,
            scrub_position: None,
            created_with: None,
            preferences: PlaybackPreferences {
                subtitles_enabled: true,
                subtitle_language: None,
                volume: 100.0,
            },
            fullscreen: false,
        }
    }

    /// Apply settings-derived preferences. The next player created picks them
    /// up; the volume applies to a running player immediately.
    pub fn set_preferences(&mut self, preferences: PlaybackPreferences) {
        self.volume = preferences.volume.clamp(0.0, 130.0);
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_volume(self.volume as f64);
            let _ = player.set_subtitles_visible(preferences.subtitles_enabled);
        }
        self.preferences = preferences;
    }

    pub fn preferences(&self) -> &PlaybackPreferences {
        &self.preferences
    }

    pub fn is_active(&self) -> bool {
        self.current.is_some()
    }

    pub fn current(&self) -> Option<&PlaybackInfo> {
        self.current.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn state(&self) -> PlayerState {
        self.player
            .as_ref()
            .map(|player| player.state())
            .unwrap_or_default()
    }

    pub fn backend(&self) -> Option<Backend> {
        self.player.as_ref().map(|player| player.backend())
    }

    /// True when a hand-off player process is still running.
    pub fn is_external_running(&self) -> bool {
        self.player
            .as_ref()
            .is_some_and(|player| player.is_playing_externally())
    }

    /// Start playback. Creates the player on first use, and lazily when the
    /// capability changes.
    pub fn open(
        &mut self,
        ctx: &egui::Context,
        capability: &PlayerCapability,
        url: &str,
        info: PlaybackInfo,
    ) -> Result<(), String> {
        self.ensure_player(ctx, capability)?;
        self.current = Some(info);
        self.error = None;
        self.last_sequence = 0;
        self.scrub_position = None;
        self.texture = None;

        let start_at = self.current.as_ref().and_then(|info| info.start_at);
        let file_name = self
            .current
            .as_ref()
            .map(|info| info.file_name.clone())
            .unwrap_or_default();

        let player = self.player.as_ref().expect("player created above");
        player.set_target_size(Some((640, 360)));
        let _ = player.set_volume(self.volume as f64);
        let _ = player.set_subtitles_visible(self.preferences.subtitles_enabled);

        // Sidecar subtitles are registered now and loaded once the file is
        // ready; mpv ignores `sub-add` before then, so the player queues them.
        let subtitles = self
            .current
            .as_ref()
            .map(|info| info.subtitles.clone())
            .unwrap_or_default();
        let _ = player.set_subtitles(subtitles);

        match start_at.filter(|start| *start > 1.0) {
            Some(start) => {
                tracing::info!(file = %file_name, start, "resuming playback");
                player.load_at(url, start).map_err(|e| e.to_string())?;
            }
            None => {
                player.load(url).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn ensure_player(
        &mut self,
        ctx: &egui::Context,
        capability: &PlayerCapability,
    ) -> Result<(), String> {
        if self.player.is_some() && self.created_with.as_ref() == Some(capability) {
            return Ok(());
        }

        if let PlayerCapability::Unavailable { reason } = capability {
            return Err(format!(
                "No video backend is available.\n\n{reason}\n\n\
                 Install mpv (with libmpv) to play inside the app, or use \
                 `reel play` from the CLI to hand playback to an external player."
            ));
        }

        let config = PlayerConfig {
            prefer_embedded: matches!(capability, PlayerCapability::Embedded),
            volume: self.preferences.volume as f64,
            subtitles_enabled: self.preferences.subtitles_enabled,
            subtitle_language: self.preferences.subtitle_language.clone(),
            ..Default::default()
        };
        let player = Player::new(config).map_err(|e| e.to_string())?;

        // Rendering happens on the player's own thread; this is how it asks the
        // UI to pick up a new frame.
        let ctx = ctx.clone();
        player.set_repaint_callback(Arc::new(move || ctx.request_repaint()));

        if let Some(previous) = self.player.take() {
            previous.set_target_size(None);
            let _ = previous.stop();
        }
        self.created_with = Some(capability.clone());
        self.player = Some(player);
        Ok(())
    }

    pub fn close(&mut self) {
        if let Some(player) = self.player.as_ref() {
            player.set_target_size(None);
            let _ = player.stop();
        }
        // Drop the whole mpv instance rather than reusing it. A reused core
        // keeps its demuxer cache, decoder and seek index, and after a seeked
        // HEVC stream it has been seen to wedge: the server has bytes queued and
        // mpv simply stops reading them. A fresh core per play costs a few tens
        // of milliseconds and cannot inherit that state.
        self.player = None;
        self.current = None;
        self.scrub_position = None;
        self.texture = None;
        self.last_sequence = 0;
    }

    pub fn toggle_pause(&self) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.toggle_pause();
        }
    }

    pub fn seek_relative(&self, delta: f64) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.seek_relative(delta);
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 130.0);
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_volume(self.volume as f64);
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Toggle the OS window's fullscreen state.
    pub fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.set_fullscreen(ctx, !self.fullscreen);
    }

    pub fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    /// Select a subtitle track, or turn subtitles off.
    pub fn set_subtitle(&self, id: Option<i64>) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_subtitle(id);
        }
    }

    pub fn set_audio(&self, id: Option<i64>) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_audio(id);
        }
    }

    pub fn set_subtitles_visible(&self, visible: bool) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_subtitles_visible(visible);
        }
    }

    pub fn set_subtitle_delay(&self, seconds: f64) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_subtitle_delay(seconds);
        }
    }

    pub fn set_speed(&self, speed: f64) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_speed(speed);
        }
    }

    pub fn set_aspect_override(&self, aspect: Option<String>) {
        if let Some(player) = self.player.as_ref() {
            let _ = player.set_aspect_override(aspect);
        }
    }

    /// How many frames have been uploaded as textures. Used by tests to tell
    /// that the decode → texture path actually ran.
    pub fn frames_uploaded(&self) -> u64 {
        self.last_sequence
    }

    /// Upload any newly rendered frame and paint the video area.
    ///
    /// The video is letterboxed inside the available space and rendered at the
    /// surface's physical pixel size, so it stays sharp on HiDPI displays.
    pub fn video_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let avail = ui.available_size();
        let (rect, _response) = ui.allocate_exact_size(avail, Sense::hover());
        let painter = ui.painter().clone();
        painter.rect_filled(rect, CornerRadius::ZERO, Color32::BLACK);

        let Some(player) = self.player.as_ref() else {
            return;
        };

        let ppp = ctx.pixels_per_point();
        let target = (
            ((avail.x * ppp).round().max(16.0)) as u32,
            ((avail.y * ppp).round().max(16.0)) as u32,
        );
        player.set_target_size(Some(target));

        if let Some(snapshot) = player.frame_after(self.last_sequence) {
            let frame = &snapshot.frame;
            let size = [frame.width as usize, frame.height as usize];
            let image = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);

            // Reuse the texture when the size matches, so a new GPU texture is
            // not allocated every frame.
            match self.texture.as_mut() {
                Some(texture) if texture.size() == size => {
                    texture.set(image, egui::TextureOptions::LINEAR);
                }
                _ => {
                    self.texture = Some(ctx.load_texture(
                        "reel-video",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            self.last_sequence = snapshot.sequence;
        }

        if let Some(texture) = self.texture.as_ref() {
            let [tw, th] = texture.size();
            if tw > 0 && th > 0 {
                let aspect = tw as f32 / th as f32;
                let target = fit_into(rect, aspect);
                painter.image(
                    texture.id(),
                    target,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        }

        if !self.is_embedded_ready() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for the first frame\u{2026}",
                egui::FontId::proportional(15.0),
                theme::TEXT_DIM,
            );
        }
    }

    fn is_embedded_ready(&self) -> bool {
        self.last_sequence > 0
    }

    /// The transport bar. `stats` come from the engine, not the player.
    pub fn controls_ui(&mut self, ui: &mut egui::Ui, stats: &PlaybackStats) {
        let Some(info) = self.current.clone() else {
            return;
        };

        let state = self.state();
        let duration = state
            .duration
            .or(info.fallback_duration)
            .filter(|d| *d > 0.0)
            .unwrap_or(0.0);
        let display_position = self.scrub_position.unwrap_or(state.position);

        self.seek_bar(ui, duration, display_position);

        // Choices are collected and applied after the rows, so the menu closures
        // never borrow the controller mutably at the same time as the UI does.
        let mut set_volume: Option<f32> = None;
        let mut set_speed: Option<f64> = None;
        let mut set_subtitle: Option<Option<i64>> = None;
        let mut toggle_subtitles: Option<bool> = None;
        let mut set_audio: Option<Option<i64>> = None;
        let mut set_delay: Option<f64> = None;
        let mut set_aspect: Option<Option<String>> = None;
        let mut toggle_fullscreen = false;

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let play_label = if state.paused {
                "\u{25b6}  Play"
            } else {
                "\u{23f8}  Pause"
            };
            if ui.button(play_label).clicked() {
                self.toggle_pause();
            }
            if ui.button("\u{23ee}  10s").on_hover_text("Back 10 seconds").clicked() {
                self.seek_relative(-10.0);
            }
            if ui.button("\u{23ed}  30s").on_hover_text("Forward 30 seconds").clicked() {
                self.seek_relative(30.0);
            }

            ui.label(
                egui::RichText::new(format!(
                    "{}  /  {}",
                    reel_core::fmt::human_duration(display_position),
                    if duration > 0.0 {
                        reel_core::fmt::human_duration(duration)
                    } else {
                        "--:--".to_string()
                    }
                ))
                .monospace()
                .color(theme::TEXT_DIM),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.player.is_some() {
                    let icon = if self.fullscreen {
                        "\u{1f5d7}"
                    } else {
                        "\u{26f6}"
                    };
                    if ui
                        .button(icon)
                        .on_hover_text("Fullscreen (F)")
                        .clicked()
                    {
                        toggle_fullscreen = true;
                    }
                }

                let mut volume = self.volume;
                if ui
                    .add(
                        egui::Slider::new(&mut volume, 0.0..=130.0)
                            .show_value(false)
                            .text("\u{1f50a}"),
                    )
                    .changed()
                {
                    set_volume = Some(volume);
                }

                if state.paused_for_cache {
                    ui.spinner();
                    ui.colored_label(theme::WARN, "buffering from peers");
                } else if stats.download_bps > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} \u{2b07}   {} peers",
                            reel_core::fmt::human_rate(stats.download_bps),
                            stats.peers
                        ))
                        .color(theme::TEXT_DIM),
                    );
                }

                ui.label(
                    egui::RichText::new(format!(
                        "{}  \u{2022}  {:.0}% downloaded",
                        info.file_name, stats.file_percent
                    ))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
                );
            });
        });

        // Second row: tracks and speed.
        ui.horizontal(|ui| {
            let subtitle_label = if state.subtitles_visible && state.active_subtitle.is_some() {
                "CC  on"
            } else {
                "CC  off"
            };
            ui.menu_button(subtitle_label, |ui| {
                if ui
                    .selectable_label(state.subtitles_visible, "Subtitles on")
                    .clicked()
                {
                    toggle_subtitles = Some(true);
                    ui.close();
                }
                if ui
                    .selectable_label(!state.subtitles_visible, "Subtitles off")
                    .clicked()
                {
                    toggle_subtitles = Some(false);
                    ui.close();
                }
                if !state.subtitle_tracks.is_empty() {
                    ui.separator();
                    for track in &state.subtitle_tracks {
                        let selected =
                            state.subtitles_visible && state.active_subtitle == Some(track.id);
                        if ui.selectable_label(selected, track.label()).clicked() {
                            set_subtitle = Some(Some(track.id));
                            ui.close();
                        }
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.small_button("-0.1s").clicked() {
                        set_delay = Some(state.subtitle_delay - 0.1);
                    }
                    if ui.small_button("+0.1s").clicked() {
                        set_delay = Some(state.subtitle_delay + 0.1);
                    }
                    ui.label(
                        egui::RichText::new(format!("delay {:+0.1}s", state.subtitle_delay))
                            .color(theme::TEXT_DIM),
                    );
                });
                if state.subtitle_tracks.is_empty() {
                    ui.label(
                        egui::RichText::new("No subtitle tracks in this file")
                            .color(theme::TEXT_DIM)
                            .size(11.0),
                    );
                }
            });

            if !state.audio_tracks.is_empty() {
                let label = state
                    .audio_tracks
                    .iter()
                    .find(|track| state.active_audio == Some(track.id))
                    .map(|track| track.label())
                    .unwrap_or_else(|| "Audio".to_string());
                ui.menu_button(format!("\u{1f50a} {label}"), |ui| {
                    for track in &state.audio_tracks {
                        let selected = state.active_audio == Some(track.id);
                        if ui.selectable_label(selected, track.label()).clicked() {
                            set_audio = Some(Some(track.id));
                            ui.close();
                        }
                    }
                });
            }

            let speed = if state.speed > 0.0 { state.speed } else { 1.0 };
            ui.menu_button(format!("{speed:.2}x"), |ui| {
                for option in [0.5_f64, 0.75, 1.0, 1.25, 1.5, 2.0] {
                    if ui
                        .selectable_label((speed - option).abs() < 0.01, format!("{option}x"))
                        .clicked()
                    {
                        set_speed = Some(option);
                        ui.close();
                    }
                }
            });

            ui.menu_button("Aspect", |ui| {
                if ui.selectable_label(state.aspect_override.is_none(), "Auto").clicked() {
                    set_aspect = Some(None);
                    ui.close();
                }
                for option in ["16:9", "4:3", "21:9", "1:1"] {
                    if ui
                        .selectable_label(state.aspect_override.as_deref() == Some(option), option)
                        .clicked()
                    {
                        set_aspect = Some(Some(option.to_string()));
                        ui.close();
                    }
                }
            });
        });

        if let Some(volume) = set_volume {
            self.set_volume(volume);
        }
        if let Some(speed) = set_speed {
            self.set_speed(speed);
        }
        if let Some(id) = set_subtitle {
            // Picking a track implies you want to see it.
            if id.is_some() {
                self.set_subtitles_visible(true);
            }
            self.set_subtitle(id);
        }
        if let Some(visible) = toggle_subtitles {
            self.set_subtitles_visible(visible);
        }
        if let Some(id) = set_audio {
            self.set_audio(id);
        }
        if let Some(delay) = set_delay {
            self.set_subtitle_delay(delay);
        }
        if let Some(aspect) = set_aspect {
            self.set_aspect_override(aspect);
        }
        if toggle_fullscreen {
            let ctx = ui.ctx().clone();
            self.toggle_fullscreen(&ctx);
        }
    }

    fn seek_bar(&mut self, ui: &mut egui::Ui, duration: f64, position: f64) {
        let width = ui.available_width();
        let (bar_rect, response) =
            ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::click_and_drag());
        let painter: Painter = ui.painter().clone();

        let track = egui::Rect::from_min_max(
            egui::pos2(bar_rect.left(), bar_rect.center().y - 3.0),
            egui::pos2(bar_rect.right(), bar_rect.center().y + 3.0),
        );
        painter.rect_filled(track, CornerRadius::same(3), theme::SURFACE_RAISED);

        let fraction = if duration > 0.0 {
            (position / duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        let mut filled = track;
        filled.set_right(track.left() + track.width() * fraction);
        painter.rect_filled(filled, CornerRadius::same(3), theme::ACCENT);

        let knob_x = if duration > 0.0 { filled.right() } else { track.left() };
        painter.circle_filled(egui::pos2(knob_x, bar_rect.center().y), 6.0, theme::ACCENT);

        if bar_rect.contains(ui.input(|i| i.pointer.hover_pos().unwrap_or_default())) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        if duration <= 0.0 {
            return;
        }

        // While dragging, only track the position; seek once on release (or
        // immediately on a click). Seeking on every drag event would hammer the
        // engine with throwaway range requests.
        let mut seek_to = None;

        if response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let fraction = ((pos.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0);
                self.scrub_position = Some(fraction as f64 * duration);
            }
        } else if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let fraction = ((pos.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0);
                seek_to = Some(fraction as f64 * duration);
            }
        }

        if response.drag_stopped() {
            seek_to = self.scrub_position.take();
        }

        if let Some(target) = seek_to {
            if let Some(player) = self.player.as_ref() {
                let _ = player.seek_absolute(target);
            }
        }
    }
}

/// Letterbox `aspect` inside `available`, centred.
///
/// Pure geometry, kept out of the draw call so it can be tested directly: a
/// wrong fit shows up as a stretched or cropped picture, which is easy to miss
/// by eye and hard to prove from a static snapshot.
pub fn fit_into(available: egui::Rect, aspect: f32) -> egui::Rect {
    if !aspect.is_finite()
        || aspect <= 0.0
        || available.width() <= 0.0
        || available.height() <= 0.0
    {
        return available;
    }

    let mut target = available;
    if target.width() / target.height() > aspect {
        // The box is wider than the video: pillarbox it.
        target.set_width(target.height() * aspect);
    } else {
        // The box is taller than the video: letterbox it.
        target.set_height(target.width() / aspect);
    }
    egui::Rect::from_center_size(available.center(), target.size())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(w, h))
    }

    #[test]
    fn pillarboxes_when_the_box_is_wider_than_the_video() {
        // 16:9 video in a 4:1 box: full height, bars left and right.
        let fitted = fit_into(rect(400.0, 100.0), 16.0 / 9.0);
        assert!((fitted.height() - 100.0).abs() < 0.01);
        assert!((fitted.width() - 177.78).abs() < 0.01);
        assert!((fitted.center().x - 200.0).abs() < 0.01, "must stay centred");
    }

    #[test]
    fn letterboxes_when_the_box_is_taller_than_the_video() {
        // 16:9 video in a square box: full width, bars top and bottom.
        let fitted = fit_into(rect(100.0, 100.0), 16.0 / 9.0);
        assert!((fitted.width() - 100.0).abs() < 0.01);
        assert!((fitted.height() - 56.25).abs() < 0.01);
        assert!((fitted.center().y - 50.0).abs() < 0.01, "must stay centred");
    }

    #[test]
    fn exact_aspect_is_unchanged() {
        let fitted = fit_into(rect(1920.0, 1080.0), 16.0 / 9.0);
        assert!((fitted.width() - 1920.0).abs() < 0.01);
        assert!((fitted.height() - 1080.0).abs() < 0.01);
    }

    #[test]
    fn degenerate_inputs_do_not_produce_nan() {
        for aspect in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let fitted = fit_into(rect(100.0, 50.0), aspect);
            assert!(fitted.width().is_finite() && fitted.height().is_finite());
        }
        let fitted = fit_into(rect(0.0, 0.0), 1.5);
        assert!(fitted.width().is_finite() && fitted.height().is_finite());
    }
}
