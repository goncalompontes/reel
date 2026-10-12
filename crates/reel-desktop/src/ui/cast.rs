//! Casting UI: the device picker and the remote-control overlay.

use super::*;
use crate::cast::{CastAction, CastStatus};
use reel_cast::session::SessionState;

impl App {
    /// A "Cast" button for the player OSD and the title page.
    pub(crate) fn cast_button(&mut self, ui: &mut egui::Ui) {
        let unavailable = self.backend.cast_unavailable();
        let active = self.backend.cast_status().is_some();
        let label = if active { "Casting" } else { "Cast" };
        let response = ui.add_enabled(
            unavailable.is_none(),
            egui::Button::new(egui::RichText::new(label).size(13.0)),
        );
        let response = match &unavailable {
            Some(reason) => response.on_disabled_hover_text(reason),
            None => response.on_hover_text("Play on a TV on your network"),
        };
        if response.clicked() {
            if let Some((torrent_id, file_id, title)) = self.cast_target() {
                self.request_cast(torrent_id, file_id, title);
            } else {
                // Nothing to cast yet: still show the picker so the reason is visible.
                self.backend.refresh_cast_devices();
                self.cast_open = true;
            }
        }
    }

    /// Ask to cast a specific file, opening the device picker.
    pub(crate) fn request_cast(&mut self, torrent_id: usize, file_id: usize, title: String) {
        self.cast_pending = Some((torrent_id, file_id, title));
        self.backend.refresh_cast_devices();
        self.cast_open = true;
    }

    /// The file the current screen implies should be cast.
    fn cast_target(&self) -> Option<(usize, usize, String)> {
        if let Some(info) = self.player.current() {
            return Some((info.torrent_id, info.file_id, info.title.clone()));
        }
        let work = self.selected_work()?;
        let (torrent_id, file_id) = self.work_target(&work)?;
        Some((torrent_id, file_id, work.heading()))
    }

    /// The device picker popover.
    pub(crate) fn cast_picker(&mut self, ctx: &egui::Context) {
        if !self.cast_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Cast to")
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .open(&mut open)
            .show(ctx, |ui| {
                if let Some(reason) = self.backend.cast_unavailable() {
                    ui.label(egui::RichText::new(reason).color(theme::TEXT_DIM));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Enable it in Settings \u{2192} Casting.")
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                    return;
                }

                ui.horizontal(|ui| {
                    if self.backend.cast_discovering() {
                        ui.spinner();
                        ui.label(egui::RichText::new("Searching\u{2026}").color(theme::TEXT_DIM));
                    } else {
                        ui.label(
                            egui::RichText::new("Devices on your network").color(theme::TEXT_DIM),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Rescan").clicked() {
                            self.backend.refresh_cast_devices();
                        }
                    });
                });
                ui.add_space(4.0);

                let devices = self.backend.cast_devices();
                if devices.is_empty() && !self.backend.cast_discovering() {
                    ui.label(egui::RichText::new("No renderers found.").color(theme::TEXT_DIM));
                }
                let mut chosen: Option<String> = None;
                for device in &devices {
                    ui.horizontal(|ui| {
                        if ui.button(&device.name).clicked() {
                            chosen = Some(device.udn.clone());
                        }
                        if let Some(model) = &device.model {
                            ui.label(egui::RichText::new(model).size(11.0).color(theme::TEXT_DIM));
                        }
                    });
                }
                if let Some(udn) = chosen {
                    if let Some((torrent_id, file_id, title)) = self.cast_pending.clone() {
                        self.backend.start_cast(udn, torrent_id, file_id, title);
                        self.cast_open = false;
                    }
                }
            });
        if !open {
            self.cast_open = false;
        }
    }

    /// The full-screen remote-control view for an active cast.
    pub(crate) fn cast_overlay(&mut self, ctx: &egui::Context) {
        let Some(status) = self.backend.cast_status() else {
            return;
        };
        let screen = ctx.content_rect();
        let mut action: Option<CastAction> = None;
        let mut dismiss = false;

        egui::Area::new(egui::Id::new("cast-overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.set_min_size(screen.size());
                egui::Frame::NONE
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(48))
                    .show(ui, |ui| {
                        ui.set_min_size(screen.size());
                        ui.vertical_centered(|ui| {
                            ui.add_space(screen.height() * 0.16);
                            match &status {
                                CastStatus::Failed { message } => {
                                    ui.label(
                                        egui::RichText::new("Casting stopped").size(26.0).strong(),
                                    );
                                    ui.add_space(8.0);
                                    ui.label(egui::RichText::new(message).color(theme::DANGER));
                                    ui.add_space(18.0);
                                    if ui.button("Close").clicked() {
                                        dismiss = true;
                                    }
                                }
                                CastStatus::Preparing { device, title } => {
                                    ui.label(egui::RichText::new(title).size(24.0).strong());
                                    ui.add_space(10.0);
                                    ui.spinner();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Preparing for {device}\u{2026}"
                                        ))
                                        .color(theme::TEXT_DIM),
                                    );
                                    ui.add_space(18.0);
                                    if ui.button("Cancel").clicked() {
                                        dismiss = true;
                                    }
                                }
                                CastStatus::Active {
                                    device,
                                    title,
                                    state,
                                    position,
                                    duration,
                                } => {
                                    ui.label(egui::RichText::new(title).size(24.0).strong());
                                    ui.label(
                                        egui::RichText::new(format!("on {device}"))
                                            .color(theme::TEXT_DIM),
                                    );
                                    ui.add_space(14.0);

                                    let playing = state.is_playing();
                                    ui.horizontal(|ui| {
                                        let label = if playing { "Pause" } else { "Play" };
                                        if ui.button(label).clicked() {
                                            action = Some(if playing {
                                                CastAction::Pause
                                            } else {
                                                CastAction::Resume
                                            });
                                        }
                                        if ui.button("Stop casting").clicked() {
                                            action = Some(CastAction::Stop);
                                        }
                                    });

                                    let total = duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                                    if total > 0.0 {
                                        let mut pos =
                                            position.map(|p| p.as_secs_f64()).unwrap_or(0.0);
                                        let slider = ui.add(
                                            egui::Slider::new(&mut pos, 0.0..=total)
                                                .show_value(false),
                                        );
                                        if slider.drag_stopped() {
                                            action = Some(CastAction::Seek(
                                                Duration::from_secs_f64(pos),
                                            ));
                                        }
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} / {}",
                                                fmt::human_duration(pos),
                                                fmt::human_duration(total)
                                            ))
                                            .size(12.0)
                                            .color(theme::TEXT_DIM),
                                        );
                                    }

                                    ui.add_space(12.0);
                                    let label = match state {
                                        SessionState::Paused => "Paused",
                                        SessionState::Connecting => "Buffering\u{2026}",
                                        _ if playing => "Playing",
                                        _ => "Stopped",
                                    };
                                    ui.label(
                                        egui::RichText::new(label)
                                            .size(11.0)
                                            .color(theme::TEXT_DIM),
                                    );
                                    ui.add_space(12.0);
                                    if ui.button("Back to this device").clicked() {
                                        dismiss = true;
                                    }
                                }
                            }
                        });
                    });
            });

        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if dismiss {
            self.backend.dismiss_cast();
        }
    }
}
