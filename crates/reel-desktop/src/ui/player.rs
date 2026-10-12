//! The player overlay: the OSD, transport and stall recovery.

use super::*;

// ----------------------------------------------------------------- player

impl App {
    pub(crate) fn player_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Repaint continuously so the video keeps advancing without input.
        ctx.request_repaint();
        self.record_watch_progress();

        // Let the backend cap how far ahead the stream fetches.
        let info = self.player.current().cloned();
        let state = self.player.state();
        if let Some(info) = info {
            self.backend.note_playback(
                info.torrent_id,
                info.file_id,
                state.position,
                state.duration,
            );
        }
        self.maybe_recover_stalled_stream();

        let Some(info) = self.player.current().cloned() else {
            // Nothing to show: drop back to the shell, which still points
            // wherever the player was opened from.
            self.player_open = false;
            return;
        };

        // Keyboard shortcuts. The player has no text fields, so plain keys are
        // unambiguous here.
        let (space, escape, fullscreen, left, right, up, down, help) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Questionmark),
            )
        });
        // Resting the pointer on the controls keeps them up, even with no
        // movement, using the rects they occupied last frame.
        let hover = ctx.input(|i| i.pointer.hover_pos()).unwrap_or_default();
        let hovering_controls = self
            .player_controls_rect
            .is_some_and(|rect| rect.contains(hover))
            || self.player_topbar_rect.is_some_and(|rect| rect.contains(hover));
        let activity = ctx.input(|i| {
            i.pointer.delta() != Vec2::ZERO || i.pointer.any_down() || i.pointer.any_click()
        }) || space
            || fullscreen
            || left
            || right
            || up
            || down
            || help
            || hovering_controls;
        if activity {
            self.player_last_activity = Instant::now();
            self.player_controls_hidden = false;
        }

        if help {
            self.player_help = !self.player_help;
        }
        if fullscreen {
            self.player.toggle_fullscreen(ctx);
        }
        if escape {
            // Escape closes the help overlay first, then defullscreens, then
            // leaves the player.
            if self.player_help {
                self.player_help = false;
            } else if self.player.is_fullscreen() {
                self.player.set_fullscreen(ctx, false);
            } else {
                self.leave_player();
                return;
            }
        }
        if space && !self.player_help {
            self.player.toggle_pause();
        }
        if left {
            self.player.seek_relative(-10.0);
        }
        if right {
            self.player.seek_relative(30.0);
        }
        if up {
            let volume = (self.player.volume() + 5.0).min(130.0);
            self.player.set_volume(volume);
        }
        if down {
            let volume = (self.player.volume() - 5.0).max(0.0);
            self.player.set_volume(volume);
        }

        // The chrome fades out while playing and nothing is happening, and comes
        // back the moment the pointer, a key, or the help overlay says so.
        let vstate = self.player.state();
        let playing = vstate.loaded && !vstate.paused && !vstate.eof;
        self.player_controls_hidden = playing
            && !self.player_help
            && self.player_last_activity.elapsed() > Duration::from_secs(3)
            && !hovering_controls;
        let show_chrome = !self.player_controls_hidden;

        let mut play_next: Option<(usize, usize)> = None;
        let mut toggle_help = false;
        let up_next = self.up_next(info.torrent_id, info.file_id);

        // The top bar is the header of the OSD: Back, what is playing, and Up
        // next. In fullscreen it fades with the transport controls so the video
        // is clean; in a window it stays, so there is always a way back.
        if show_chrome || !self.player.is_fullscreen() {
            let panel = egui::Panel::top("player-bar")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(14, 8)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("\u{2b05}  Back").clicked() {
                            self.leave_player();
                        }
                        ui.label(egui::RichText::new(&info.title).size(15.0).strong());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = match self.player.backend() {
                                Some(reel_player::Backend::Embedded) => "libmpv (embedded)",
                                Some(reel_player::Backend::External) => "external player",
                                _ => "unknown backend",
                            };
                            ui.label(egui::RichText::new(label).size(11.0).color(theme::TEXT_DIM));
                            self.cast_button(ui);
                            if ui.button("?").on_hover_text("Keyboard shortcuts").clicked() {
                                toggle_help = true;
                            }
                            if let Some((tid, fid, next)) = &up_next {
                                if ui
                                    .button(format!("\u{25b6}  Up next: {next}"))
                                    .on_hover_text("Play the next episode")
                                    .clicked()
                                {
                                    play_next = Some((*tid, *fid));
                                }
                            }
                        });
                    });
                });
            self.player_topbar_rect = Some(panel.response.rect);
        } else {
            self.player_topbar_rect = None;
        }

        // External players own their window: explain rather than show a void.
        if self.player.backend() == Some(reel_player::Backend::External) {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(theme::BG))
                .show(ui, |ui| {
                    ui.add_space(70.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Playing {} elsewhere", info.title))
                                .size(22.0)
                                .strong(),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(
                                "libmpv is not available, so playback was handed to a \
                                 separate player process. The picture is in that program's \
                                 own window.",
                            )
                            .color(theme::TEXT_DIM),
                        );
                        ui.add_space(16.0);
                        let running = self.player.is_external_running();
                        ui.label(
                            egui::RichText::new(if running {
                                "The external player is running."
                            } else {
                                "The external player has exited."
                            })
                            .color(if running {
                                theme::OK
                            } else {
                                theme::TEXT_DIM
                            }),
                        );
                    });
                });
            self.draw_toast(ui, ctx);
            return;
        }

        if show_chrome {
            let panel = egui::Panel::bottom("player-controls")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(16, 10)),
                )
                .show(ui, |ui| {
                    let stats = self.playback_stats(info.torrent_id);
                    ui.set_min_width(ui.available_width());
                    self.player.controls_ui(ui, &stats);
                });
            self.player_controls_rect = Some(panel.response.rect);
        } else {
            self.player_controls_rect = None;
        }

        let video = egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ui, |ui| self.player.video_ui(ui, ctx))
            .inner;
        if video.clicked() {
            self.player.toggle_pause();
        }

        if self.player_help {
            self.player_help_overlay(ctx);
        }
        if toggle_help {
            self.player_help = !self.player_help;
        }
        if let Some((torrent_id, file_id)) = play_next {
            self.play_file(ctx, torrent_id, file_id);
        }

        if let Some(error) = self.player.error().map(str::to_string) {
            egui::Panel::bottom("player-error")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::SURFACE_RAISED)
                        .inner_margin(egui::Margin::symmetric(14, 8)),
                )
                .show(ui, |ui| {
                    ui.colored_label(theme::DANGER, error);
                });
        }
    }

    /// The keyboard-shortcut overlay, toggled with `?`.
    pub(crate) fn player_help_overlay(&self, ctx: &egui::Context) {
        egui::Window::new("Keyboard shortcuts")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                for (keys, what) in [
                    ("Space", "Play / pause"),
                    ("\u{2190} / \u{2192}", "Back 10s / forward 30s"),
                    ("\u{2191} / \u{2193}", "Volume"),
                    ("F", "Fullscreen"),
                    ("?", "Show or hide this help"),
                    ("Esc", "Close this, then leave the player"),
                ] {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [96.0, 18.0],
                            egui::Label::new(egui::RichText::new(keys).monospace()),
                        );
                        ui.label(what);
                    });
                }
            });
    }

    /// The episode after the one playing, as `(torrent id, file id, code)`.
    pub(crate) fn up_next(&self, torrent_id: usize, file_id: usize) -> Option<(usize, usize, String)> {
        let work_id = self.work_id_for_torrent(torrent_id)?;
        let work = self.work(work_id)?;
        if !work.is_series() {
            return None;
        }
        let episodes = work.episodes();
        let index = episodes.iter().position(|episode| {
            episode
                .variants
                .iter()
                .any(|variant| variant.torrent_id == torrent_id && variant.file_id == file_id)
        })?;
        let next = episodes.get(index + 1)?;
        let variant = self.chosen_variant(&work, next)?;
        Some((variant.torrent_id, variant.file_id, next.code()))
    }

    /// Recover a genuinely wedged picture.
    ///
    /// Only when the player is loaded and supposedly playing, the picture *and*
    /// the playback clock are both frozen, nothing is buffering, and the torrent
    /// is not delivering bytes. A slow torrent, a seek, a rebuffer and a still
    /// scene all look "not advancing" for a while, but none of them satisfies
    /// all of those at once, so they are left alone.
    pub(crate) fn maybe_recover_stalled_stream(&mut self) {
        let state = self.player.state();
        let busy = stream_looks_busy(&state);
        let fetching = self.stream_is_fetching();
        let frames = self.player.frames_uploaded();
        let recent_seek = self.player.seeked_within(Duration::from_secs(20));

        if !playback_is_wedged(
            busy,
            fetching,
            frames,
            self.player_last_frames,
            state.position,
            self.player_progress_position,
            recent_seek,
        ) {
            self.note_player_progress(&state);
            return;
        }

        if self.stream_recovered || self.player_progress_at.elapsed() < Duration::from_secs(15) {
            return;
        }
        let Some(info) = self.player.current().cloned() else {
            return;
        };
        self.stream_recovered = true;
        tracing::warn!(
            torrent_id = info.torrent_id,
            file_id = info.file_id,
            frames,
            position = state.position,
            "playback clock frozen; rebuilding the stream and the player"
        );
        // A wedged picture is as likely to be the decoder as the fetch, so the
        // core is dropped too; the next play builds a fresh one.
        self.player.reset();
        self.pending_play = Some((info.torrent_id, info.file_id));
        self.backend
            .restart_stream(info.torrent_id, &[info.file_id]);
        self.set_toast("Reconnecting the stream\u{2026}", false);
    }

    /// Record that playback is still progressing, so the stall watchdog starts
    /// its window afresh.
    pub(crate) fn note_player_progress(&mut self, state: &reel_player::PlayerState) {
        self.player_last_frames = self.player.frames_uploaded();
        self.player_progress_position = state.position;
        self.player_progress_at = Instant::now();
        self.stream_recovered = false;
    }

    /// Whether the engine is still delivering bytes for what is playing.
    pub(crate) fn stream_is_fetching(&self) -> bool {
        let Some(info) = self.player.current() else {
            return false;
        };
        self.item(info.torrent_id)
            .is_some_and(|item| item.torrent.stats.download_bps > 0)
    }

    /// Leave the player, saving where we got to.
    pub(crate) fn leave_player(&mut self) {
        if let (Some(info_hash), true) =
            (self.watch_recording_for.clone(), self.player.state().loaded)
        {
            let state = self.player.state();
            let info = self.player.current().cloned();
            if state.position >= 1.0 {
                self.backend.record_watch(
                    &info_hash,
                    info.as_ref().map(|i| i.file_id).unwrap_or(0),
                    info.as_ref().map(|i| i.file_name.clone()),
                    info.as_ref().map(|i| i.title.clone()),
                    state.position,
                    state.duration,
                );
            }
        }

        self.player.close();
        self.player_last_frames = 0;
        self.player_progress_position = 0.0;
        self.player_progress_at = Instant::now();
        self.stream_recovered = false;
        self.player_controls_rect = None;
        self.player_topbar_rect = None;
        self.watch_recording_for = None;
        self.watch_last_recorded = 0.0;
        // The trail still points at wherever the player was opened from.
        self.player_open = false;
        // A stream-only title gives its temporary storage back now; a download
        // is left alone.
        if let Some(origin) = self.player_origin {
            self.backend.stop_streaming(origin);
        }
        self.refresh();
    }
}

