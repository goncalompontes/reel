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
            if let Some((torrent_id, info_hash, file_id, title)) = self.cast_target() {
                self.request_cast(torrent_id, info_hash, file_id, title);
            } else {
                // Nothing to cast yet: still show the picker so the reason is visible.
                self.backend.refresh_cast_devices();
                self.cast_open = true;
            }
        }
    }

    /// Ask to cast a specific file, opening the device picker.
    pub(crate) fn request_cast(
        &mut self,
        torrent_id: usize,
        info_hash: String,
        file_id: usize,
        title: String,
    ) {
        self.cast_pending = Some((torrent_id, info_hash, file_id, title));
        self.backend.refresh_cast_devices();
        self.cast_open = true;
    }

    /// Start casting once a device is chosen.
    ///
    /// Mirrors playback: start the files and wait for
    /// [`crate::backend::BackendEvent::Ready`] before casting. A title that is
    /// not already streaming has no valid stream URL yet (and a re-add changes
    /// its engine id), so probing immediately gives a 404.
    pub(crate) fn begin_cast(
        &mut self,
        udn: String,
        torrent_id: usize,
        info_hash: String,
        file_id: usize,
        title: String,
    ) {
        self.start_streaming(torrent_id, &[file_id]);
        self.pending_cast = Some((udn, torrent_id, info_hash, file_id, title));
        self.cast_view_open = true;
        self.set_toast("Preparing to stream\u{2026}", false);
    }

    /// The file the current screen implies should be cast, as
    /// `(torrent id, info hash, file id, title)`.
    fn cast_target(&self) -> Option<(usize, String, usize, String)> {
        if let Some(info) = self.player.current() {
            return Some((
                info.torrent_id,
                info.info_hash.clone(),
                info.file_id,
                info.title.clone(),
            ));
        }
        let work = self.selected_work()?;
        let (torrent_id, file_id) = self.work_target(&work)?;
        let info_hash = self.item(torrent_id)?.entry.info_hash.clone();
        Some((torrent_id, info_hash, file_id, work.heading()))
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
                    ui.label(
                        egui::RichText::new(
                            "No renderers found. If your TV supports DLNA, add it by address below.",
                        )
                        .color(theme::TEXT_DIM),
                    );
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

                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Add by address");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cast_address)
                            .hint_text("192.168.1.50")
                            .desired_width(130.0),
                    );
                    if ui.button("Add").clicked() {
                        let address = self.cast_address.trim().to_string();
                        if !address.is_empty() {
                            self.backend.add_cast_device(address);
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Use this when the TV does not appear (some networks block discovery).",
                    )
                    .size(11.0)
                    .color(theme::TEXT_DIM),
                );

                if let Some(udn) = chosen {
                    if let Some((torrent_id, info_hash, file_id, title)) =
                        self.cast_pending.clone()
                    {
                        self.begin_cast(udn, torrent_id, info_hash, file_id, title);
                        self.cast_open = false;
                    }
                }
            });
        if !open {
            self.cast_open = false;
        }
    }

    /// The full-screen remote-control view for an active cast.
    ///
    /// Closing it does not stop the cast: the mini-bar reopens it. Only "Stop"
    /// (or "Cancel" while preparing) ends it.
    pub(crate) fn cast_overlay(&mut self, ctx: &egui::Context) {
        let Some(status) = self.backend.cast_status() else {
            self.cast_view_open = false;
            return;
        };
        if !self.cast_view_open {
            return;
        }
        let screen = ctx.content_rect();
        let mut action: Option<CastAction> = None;
        let mut stop = false;
        let mut cancel = false;
        let mut hide = false;

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
                            ui.add_space(screen.height() * 0.14);
                            match &status {
                                CastStatus::Failed { message } => {
                                    ui.label(
                                        egui::RichText::new("Casting stopped").size(26.0).strong(),
                                    );
                                    ui.add_space(8.0);
                                    ui.label(egui::RichText::new(message).color(theme::DANGER));
                                    ui.add_space(18.0);
                                    if ui.button("Close").clicked() {
                                        cancel = true;
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
                                    ui.horizontal(|ui| {
                                        if ui.button("Hide").clicked() {
                                            hide = true;
                                        }
                                        if ui.button("Cancel casting").clicked() {
                                            cancel = true;
                                        }
                                    });
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
                                        egui::RichText::new(format!("Casting to {device}"))
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
                                            stop = true;
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
                                    ui.add_space(16.0);
                                    if ui
                                        .button("Hide (keeps casting)")
                                        .on_hover_text(
                                            "The cast keeps running; the bar at the bottom \
                                             reopens this view.",
                                        )
                                        .clicked()
                                    {
                                        hide = true;
                                    }
                                }
                            }
                        });
                    });
            });

        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if stop {
            self.stop_cast();
            self.cast_view_open = false;
        }
        if cancel {
            self.backend.dismiss_cast();
            self.cast_view_open = false;
        }
        if hide {
            self.cast_view_open = false;
        }
    }

    /// End the cast on the renderer, or clear a failed one.
    fn stop_cast(&mut self) {
        if matches!(self.backend.cast_status(), Some(CastStatus::Failed { .. })) {
            self.backend.dismiss_cast();
        } else {
            self.backend.cast_action(CastAction::Stop);
        }
    }

    /// The persistent mini-controller shown whenever a cast is running and its
    /// full view is closed, so the cast is always reachable and stoppable.
    pub(crate) fn cast_bar(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.backend.cast_status() else {
            return;
        };
        let mut action: Option<CastAction> = None;
        let mut open_view = false;
        let mut stop = false;

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("\u{25c9}")
                    .size(14.0)
                    .color(theme::ACCENT),
            );
            match &status {
                CastStatus::Preparing { device, title } => {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(format!("Preparing {title} for {device}\u{2026}"))
                            .size(13.0),
                    );
                }
                CastStatus::Active {
                    device,
                    title,
                    state,
                    position,
                    ..
                } => {
                    let playing = state.is_playing();
                    if ui.button(if playing { "Pause" } else { "Play" }).clicked() {
                        action = Some(if playing {
                            CastAction::Pause
                        } else {
                            CastAction::Resume
                        });
                    }
                    let pos = position
                        .map(|p| fmt::human_duration(p.as_secs_f64()))
                        .unwrap_or_else(|| "0:00".to_string());
                    ui.label(egui::RichText::new(title).size(13.0));
                    ui.label(
                        egui::RichText::new(format!("{pos}  \u{00b7}  {device}"))
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                }
                CastStatus::Failed { message } => {
                    ui.label(
                        egui::RichText::new(format!("Casting failed: {message}"))
                            .size(12.0)
                            .color(theme::DANGER),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Stop").clicked() {
                    stop = true;
                }
                if ui.button("Open cast").clicked() {
                    open_view = true;
                }
            });
        });

        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if stop {
            self.stop_cast();
        }
        if open_view {
            self.cast_view_open = true;
        }
    }
}
