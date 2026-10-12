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
            if let Some(target) = self.cast_target() {
                self.request_cast(target);
            } else {
                // Nothing to cast yet: still show the picker so the reason is visible.
                self.backend.refresh_cast_devices();
                self.cast_open = true;
            }
        }
    }

    /// Ask to cast a specific file, opening the device picker.
    pub(crate) fn request_cast(&mut self, target: CastTarget) {
        self.cast_pending = Some(target);
        self.backend.refresh_cast_devices();
        self.cast_open = true;
    }

    /// Start casting once a device is chosen.
    ///
    /// Mirrors playback: start the files and wait for
    /// [`crate::backend::BackendEvent::Ready`] before casting. A title that is
    /// not already streaming has no valid stream URL yet (and a re-add changes
    /// its engine id), so probing immediately gives a 404.
    pub(crate) fn begin_cast(&mut self, udn: String, target: CastTarget) {
        self.start_streaming(target.torrent_id, &[target.file_id]);
        self.pending_cast = Some((udn, target));
        // The compact bar carries the progress; the full remote is opt-in, so a
        // cast does not take over the screen.
        self.set_toast("Casting\u{2026}", false);
    }

    /// The file the current screen implies should be cast.
    ///
    /// The start position hands the picture over seamlessly: the local player's
    /// position when it is open on this file, otherwise the saved watch position.
    fn cast_target(&self) -> Option<CastTarget> {
        if let Some(info) = self.player.current() {
            let state = self.player.state();
            let start_at = (state.loaded && state.position >= 1.0).then_some(state.position);
            return Some(CastTarget {
                torrent_id: info.torrent_id,
                info_hash: info.info_hash.clone(),
                file_id: info.file_id,
                title: info.title.clone(),
                start_at,
            });
        }
        let work = self.selected_work()?;
        let (torrent_id, file_id) = self.work_target(&work)?;
        let item = self.item(torrent_id)?;
        Some(CastTarget {
            torrent_id,
            info_hash: item.entry.info_hash.clone(),
            file_id,
            title: work.heading(),
            start_at: resume_start_at(&item, file_id),
        })
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

                // What will be cast, and where the picture continues from, so a
                // hand-off is never a surprise.
                if let Some(target) = self.cast_pending.clone() {
                    ui.label(egui::RichText::new(target.title).size(14.0).strong());
                    match target.start_at.filter(|from| *from >= 1.0) {
                        Some(from) => ui.label(
                            egui::RichText::new(format!(
                                "Continues from {}",
                                fmt::human_duration(from)
                            ))
                            .size(11.0)
                            .color(theme::ACCENT),
                        ),
                        None => ui.label(
                            egui::RichText::new("Starts from the beginning")
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                        ),
                    };
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);
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
                        if ghost_button(ui, "Rescan").clicked() {
                            self.backend.refresh_cast_devices();
                        }
                    });
                });
                ui.add_space(2.0);

                let devices = self.backend.cast_devices();
                if devices.is_empty() && !self.backend.cast_discovering() {
                    ui.label(
                        egui::RichText::new(
                            "No renderers found. If your TV supports DLNA, add it by address below.",
                        )
                        .color(theme::TEXT_DIM)
                        .size(11.0),
                    );
                }
                let mut chosen: Option<String> = None;
                for device in &devices {
                    let label = match &device.model {
                        Some(model) => format!("{}\n{model}", device.name),
                        None => device.name.clone(),
                    };
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new(label))
                                .min_size(Vec2::new(ui.available_width(), 0.0)),
                        )
                        .clicked()
                    {
                        chosen = Some(device.udn.clone());
                    }
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label("Add by address");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cast_address)
                            .hint_text("192.168.1.50")
                            .desired_width(120.0),
                    );
                    if ghost_button(ui, "Add").clicked() {
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
                    if let Some(target) = self.cast_pending.clone() {
                        self.begin_cast(udn, target);
                        self.cast_open = false;
                    }
                }
            });
        if !open {
            self.cast_open = false;
        }
    }

    /// The full cast remote: a centred card with the transport and a seek bar.
    ///
    /// Opt-in — the compact bar is the primary surface — so a cast never takes
    /// over the screen. Closing it (the backdrop, or Escape) leaves the cast
    /// running; only Stop ends it.
    pub(crate) fn cast_overlay(&mut self, ctx: &egui::Context) {
        let Some(status) = self.backend.cast_status() else {
            self.cast_view_open = false;
            return;
        };
        if !self.cast_view_open {
            return;
        }
        let mut action: Option<CastAction> = None;
        let mut stop = false;
        let mut dismiss = false;

        let modal = egui::Modal::new(egui::Id::new("cast-remote")).show(ctx, |ui| {
            ui.set_width(420.0);
            match &status {
                CastStatus::Failed { message } => {
                    ui.label(egui::RichText::new("Casting stopped").size(18.0).strong());
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(message).color(theme::DANGER));
                    ui.add_space(14.0);
                    if ghost_button(ui, "Close").clicked() {
                        dismiss = true;
                    }
                }
                CastStatus::Preparing { device, title } => {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(title).size(18.0).strong());
                        ui.spinner();
                    });
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("Connecting to {device}\u{2026}"))
                            .color(theme::TEXT_DIM)
                            .size(12.0),
                    );
                    ui.add_space(14.0);
                    if ghost_button(ui, "Cancel casting").clicked() {
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
                    ui.label(egui::RichText::new(title).size(18.0).strong());
                    ui.label(
                        egui::RichText::new(format!("Playing on {device}"))
                            .color(theme::TEXT_DIM)
                            .size(11.0),
                    );
                    ui.add_space(14.0);

                    let playing = state.is_playing();
                    ui.horizontal(|ui| {
                        if crate::components::primary_button(
                            ui,
                            if playing { "Pause" } else { "Play" },
                        )
                        .clicked() {
                            action = Some(if playing {
                                CastAction::Pause
                            } else {
                                CastAction::Resume
                            });
                        }
                        if ghost_button(ui, "Stop & watch here")
                            .on_hover_text("Stops the TV and resumes on this screen")
                            .clicked()
                        {
                            stop = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                        });
                    });

                    let total = duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                    if total > 0.0 {
                        ui.add_space(12.0);
                        let mut pos = position.map(|p| p.as_secs_f64()).unwrap_or(0.0);
                        let slider = ui.add(
                            egui::Slider::new(&mut pos, 0.0..=total).show_value(false),
                        );
                        if slider.drag_stopped() {
                            action = Some(CastAction::Seek(Duration::from_secs_f64(pos)));
                        }
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(fmt::human_duration(pos))
                                    .size(11.0)
                                    .color(theme::TEXT_DIM),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(fmt::human_duration(total))
                                            .size(11.0)
                                            .color(theme::TEXT_DIM),
                                    );
                                },
                            );
                        });
                    }
                }
            }
        });

        // Closing the remote (backdrop or Escape) keeps the cast running.
        if modal.backdrop_response.clicked() {
            self.cast_view_open = false;
        }
        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if stop {
            self.stop_cast();
            self.cast_view_open = false;
        }
        if dismiss {
            self.casting = None;
            self.backend.dismiss_cast();
            self.cast_view_open = false;
        }
    }

    /// End the cast on the renderer, or clear a failed one.
    ///
    /// A confirmed stop comes back as a
    /// [`crate::backend::BackendEvent::CastEnded`], which hands playback back to
    /// this machine at the cast position.
    fn stop_cast(&mut self) {
        if matches!(self.backend.cast_status(), Some(CastStatus::Failed { .. })) {
            self.casting = None;
            self.backend.dismiss_cast();
        } else {
            self.backend.cast_action(CastAction::Stop);
        }
    }

    /// The picture is moving to a renderer: stop the local player if it is on
    /// the same file, leaving the stream running for the cast.
    pub(crate) fn hand_over_to_cast(&mut self, target: &CastTarget) {
        let same_file = self.player.current().is_some_and(|info| {
            info.torrent_id == target.torrent_id && info.file_id == target.file_id
        });
        if !same_file {
            return;
        }
        self.record_watch_progress();
        self.player.close();
        self.player_open = false;
        self.watch_recording_for = None;
        self.player_origin = None;
        self.player_controls_rect = None;
        self.player_topbar_rect = None;
    }

    /// The renderer stopped — the user stopped it, or the TV did. Hand the
    /// picture back to this machine at the cast position, unless the cast had
    /// reached the end of the media.
    pub(crate) fn on_cast_ended(
        &mut self,
        position: Option<Duration>,
        duration: Option<Duration>,
    ) {
        let Some(target) = self.casting.take() else {
            return;
        };
        self.cast_view_open = false;
        let Some(position) = position.map(|position| position.as_secs_f64()) else {
            return;
        };
        if position < 1.0 || cast_ended_at_the_end(position, duration) {
            return;
        }
        self.play_file_at(target.torrent_id, target.file_id, position);
        self.set_toast(
            format!(
                "Resuming here at {}",
                fmt::human_duration(position)
            ),
            false,
        );
    }

    /// The persistent "now casting" bar.
    ///
    /// This is the primary casting surface: a compact strip that shows what is
    /// on the TV and lets it be controlled without leaving what you were doing.
    /// The full remote is opt-in, so a cast never takes over the screen.
    pub(crate) fn cast_bar(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.backend.cast_status() else {
            return;
        };
        // Where the picture was handed over from, shown while connecting.
        let handoff = self
            .casting
            .as_ref()
            .and_then(|target| target.start_at)
            .or_else(|| {
                self.pending_cast
                    .as_ref()
                    .and_then(|(_, target)| target.start_at)
            });

        let mut action: Option<CastAction> = None;
        let mut open_view = false;
        let mut stop = false;
        let mut dismiss = false;

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(icons::TELEVISION)
                    .size(18.0)
                    .color(theme::ACCENT),
            );
            ui.add_space(8.0);

            ui.vertical(|ui| match &status {
                CastStatus::Preparing { device, title } => {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(title).size(15.0).strong());
                        ui.spinner();
                    });
                    let mut line = format!("Connecting to {device}\u{2026}");
                    if let Some(from) = handoff {
                        line.push_str(&format!(
                            "  \u{00b7}  continues from {}",
                            fmt::human_duration(from)
                        ));
                    }
                    ui.label(
                        egui::RichText::new(line)
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                }
                CastStatus::Active {
                    device,
                    title,
                    position,
                    duration,
                    ..
                } => {
                    ui.label(egui::RichText::new(title).size(15.0).strong());
                    let pos = position
                        .map(|p| fmt::human_duration(p.as_secs_f64()))
                        .unwrap_or_else(|| "0:00".to_string());
                    let line = match duration {
                        Some(total) if total.as_secs_f64() > 0.0 => format!(
                            "{pos} / {}  \u{00b7}  on {device}",
                            fmt::human_duration(total.as_secs_f64())
                        ),
                        _ => format!("{pos}  \u{00b7}  on {device}"),
                    };
                    ui.label(
                        egui::RichText::new(line)
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                }
                CastStatus::Failed { message } => {
                    ui.label(egui::RichText::new("Casting stopped").size(15.0).strong());
                    ui.label(
                        egui::RichText::new(message)
                            .size(11.0)
                            .color(theme::DANGER),
                    );
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match &status {
                    CastStatus::Active {
                        state,
                        position,
                        duration,
                        ..
                    } => {
                        let playing = state.is_playing();
                        if crate::components::icon_button(
                            ui,
                            if playing { icons::PAUSE } else { icons::PLAY },
                        )
                        .on_hover_text(if playing {
                            "Pause on the TV"
                        } else {
                            "Play on the TV"
                        })
                        .clicked()
                        {
                            action = Some(if playing {
                                CastAction::Pause
                            } else {
                                CastAction::Resume
                            });
                        }
                        if ghost_button(ui, "Stop")
                            .on_hover_text("Stops the TV; playback resumes here")
                            .clicked()
                        {
                            stop = true;
                        }
                        if ghost_button(ui, "Remote").clicked() {
                            open_view = true;
                        }
                        if let Some(position) = position {
                            let total = duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                            cast_progress(ui, position.as_secs_f64(), total, 160.0);
                        }
                    }
                    CastStatus::Preparing { .. } => {
                        if ghost_button(ui, "Cancel").clicked() {
                            dismiss = true;
                        }
                    }
                    CastStatus::Failed { .. } => {
                        if ghost_button(ui, "Dismiss").clicked() {
                            dismiss = true;
                        }
                    }
                }
            });
        });

        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if stop {
            self.stop_cast();
        }
        if dismiss {
            self.casting = None;
            self.backend.dismiss_cast();
        }
        if open_view {
            self.cast_view_open = true;
        }
    }
}

/// A slim progress rail for the cast bar.
fn cast_progress(ui: &mut egui::Ui, position: f64, total: f64, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), egui::Sense::hover());
    let radius = CornerRadius::same(3);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, theme::SURFACE_RAISED);
    let fraction = if total > 0.0 {
        (position / total).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    if fraction > 0.0 {
        let mut filled = rect;
        filled.set_right(rect.left() + rect.width() * fraction);
        painter.rect_filled(filled, radius, theme::ACCENT);
    }
}

/// Whether a cast that ended had reached the end of the media, so there is
/// nothing worth resuming on this machine.
fn cast_ended_at_the_end(position: f64, duration: Option<Duration>) -> bool {
    duration.is_some_and(|duration| {
        let total = duration.as_secs_f64();
        total > 0.0 && position >= total - 30.0
    })
}
