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
        }
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
                let mut volume = self.volume;
                if ui
                    .add(
                        egui::Slider::new(&mut volume, 0.0..=130.0)
                            .show_value(false)
                            .text("\u{1f50a}"),
                    )
                    .changed()
                {
                    self.set_volume(volume);
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
