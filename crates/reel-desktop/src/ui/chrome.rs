//! The app shell: navigation rail, header, modals and toasts.

use super::*;

// ------------------------------------------------------------------ chrome

impl App {
    /// Keyboard shortcuts that belong to the shell rather than a view.
    pub(crate) fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let focus_search = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K));
        let (back, forward) = ctx.input(|i| {
            let back = i.key_pressed(egui::Key::Escape)
                || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft))
                || i.pointer.button_clicked(egui::PointerButton::Extra1);
            let forward =
                (i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight))
                    || i.pointer.button_clicked(egui::PointerButton::Extra2);
            (back, forward)
        });

        // The Add modal owns Escape and the backdrop while it is up.
        if back && !self.add_open {
            self.back();
        }
        if forward {
            self.nav.forward();
        }
        if focus_search {
            if !matches!(self.root(), Screen::Discover) {
                self.go_to(Screen::Discover);
            }
            self.focus_search = true;
        }
    }

    /// The left rail: destinations, the Add action, and the transfers summary.
    pub(crate) fn rail(&mut self, ui: &mut egui::Ui, width: f32) {
        let compact = width < 100.0;
        ui.vertical(|ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(icons::FILM_STRIP)
                        .size(if compact { 20.0 } else { 22.0 })
                        .color(theme::ACCENT),
                );
                if !compact {
                    ui.label(
                        egui::RichText::new("reel")
                            .size(22.0)
                            .family(theme::semibold(ui.ctx()))
                            .color(theme::TEXT),
                    );
                }
            });
            ui.add_space(14.0);

            self.rail_item(ui, Screen::Home, icons::HOUSE, "Home", compact);
            ui.add_space(3.0);
            self.rail_item(
                ui,
                Screen::Discover,
                icons::MAGNIFYING_GLASS,
                "Discover",
                compact,
            );
            ui.add_space(3.0);
            self.rail_item(ui, Screen::Activity, icons::DOWNLOAD, "Activity", compact);

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                ui.add_space(2.0);
                self.rail_transfers(ui, compact);
                ui.add_space(10.0);
                self.rail_item(ui, Screen::Settings, icons::GEAR, "Settings", compact);
                ui.add_space(8.0);
                let add = if compact {
                    icons::PLUS.to_string()
                } else {
                    format!("{}   Add", icons::PLUS)
                };
                if ui
                    .add_sized(
                        [ui.available_width(), 32.0],
                        egui::Button::new(
                            egui::RichText::new(add)
                                .size(if compact { 15.0 } else { 13.5 })
                                .color(theme::TEXT),
                        )
                        .fill(theme::ACCENT_DEEP)
                        .corner_radius(theme::CONTROL_RADIUS),
                    )
                    .clicked()
                {
                    self.add_open = true;
                }
            });
        });
    }

    pub(crate) fn rail_item(
        &mut self,
        ui: &mut egui::Ui,
        target: Screen,
        glyph: &str,
        label: &str,
        compact: bool,
    ) {
        let selected = self.root() == &target;
        // The collapsed rail is icon-only; the label lives in a tooltip.
        let text = if compact {
            egui::RichText::new(glyph).size(17.0)
        } else {
            egui::RichText::new(format!("{glyph}    {label}")).size(14.0)
        };
        let response = ui.add_sized(
            [ui.available_width(), 34.0],
            egui::Button::new(text)
                .selected(selected)
                .corner_radius(theme::CONTROL_RADIUS),
        );
        let response = if compact {
            response.on_hover_text(label)
        } else {
            response
        };
        if response.clicked() {
            self.nav.go(target);
        }
    }

    pub(crate) fn rail_transfers(&mut self, ui: &mut egui::Ui, compact: bool) {
        let (down, peers, active) =
            self.library
                .iter()
                .fold((0u64, 0u32, 0usize), |(down, peers, active), item| {
                    (
                        down + item.torrent.stats.download_bps,
                        peers + item.torrent.stats.peers.live,
                        active
                            + usize::from(item.downloading || item.torrent.stats.download_bps > 0),
                    )
                });
        if compact {
            if active > 0 {
                ui.label(
                    egui::RichText::new(format!("{} {active}", icons::DOWNLOAD))
                        .size(13.0)
                        .color(theme::OK),
                );
            }
            return;
        }
        if down > 0 {
            ui.label(
                egui::RichText::new(format!("{}  {}", icons::DOWNLOAD, fmt::human_rate(down)))
                    .size(11.5)
                    .color(theme::OK),
            );
        } else {
            ui.label(
                egui::RichText::new(format!("{active} active"))
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );
        }
        ui.label(
            egui::RichText::new(format!("{peers} peers"))
                .size(11.0)
                .color(theme::TEXT_DIM),
        );
    }

    /// The header for pushed views: Back/Forward and a breadcrumb trail.
    pub(crate) fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.nav.can_back()
                && ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!("{}  Back", icons::ARROW_LEFT)).size(13.5),
                        )
                        .frame(false),
                    )
                    .clicked()
            {
                self.nav.back();
            }
            if self.nav.can_forward()
                && ui
                    .add(
                        egui::Button::new(egui::RichText::new(icons::ARROW_RIGHT).size(13.5))
                            .frame(false),
                    )
                    .on_hover_text("Forward")
                    .clicked()
            {
                self.nav.forward();
            }
            ui.add_space(6.0);

            let trail: Vec<Screen> = self.nav.trail().to_vec();
            let last = trail.len().saturating_sub(1);
            for (index, crumb) in trail.iter().enumerate() {
                if index > 0 {
                    ui.label(
                        egui::RichText::new(icons::CARET_RIGHT)
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                }
                let label = self.crumb_label(crumb);
                if index == last {
                    ui.label(
                        egui::RichText::new(label)
                            .size(15.0)
                            .family(theme::semibold(ui.ctx())),
                    );
                } else if ui
                    .link(egui::RichText::new(label).color(theme::TEXT_DIM))
                    .clicked()
                {
                    self.nav.truncate(index + 1);
                }
            }
        });
    }

    pub(crate) fn crumb_label(&self, screen: &Screen) -> String {
        match screen {
            Screen::Work(id) => self
                .work(*id)
                .map(|work| work.heading())
                .unwrap_or_else(|| "Title".to_string()),
            Screen::Result(key) => self
                .search_view
                .as_ref()
                .and_then(|view| {
                    view.works.iter().find(|work| &work.key == key).or_else(|| {
                        let anchor = self.result_anchor.as_deref()?;
                        view.works
                            .iter()
                            .find(|work| work_contains_hit(work, &view.shown, anchor))
                    })
                })
                .map(|work| work.heading())
                .unwrap_or_else(|| "Result".to_string()),
            other => other.root_label().to_string(),
        }
    }

    /// The global Add modal, opened from the rail or an empty state.
    pub(crate) fn draw_add_modal(&mut self, ctx: &egui::Context) {
        if !self.add_open {
            return;
        }
        let mut close = false;
        let mut submit = false;

        let modal = egui::Modal::new(egui::Id::new("add-modal")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(egui::RichText::new("Add a torrent").size(18.0).strong());
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Paste a magnet link, an http(s) .torrent URL, or a 40-character info hash.",
                )
                .color(theme::TEXT_DIM)
                .size(12.0),
            );
            ui.add_space(10.0);
            let field = ui.add(
                egui::TextEdit::multiline(&mut self.add_source)
                    .hint_text("magnet:?xt=urn:btih:\u{2026}")
                    .desired_width(f32::INFINITY)
                    .desired_rows(3),
            );
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.add_media_only,
                "Only download playable files (skips samples and extras)",
            );

            let status = self.backend.catalog_status();
            if !status.configured {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        status
                            .note
                            .unwrap_or_else(|| "Metadata is unavailable.".to_string()),
                    )
                    .color(theme::WARN)
                    .size(12.0),
                );
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui
                    .button(egui::RichText::new("Add torrent").size(15.0))
                    .clicked()
                {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });

            // Ctrl/Cmd+Enter submits from the field.
            if field.has_focus()
                && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter))
            {
                submit = true;
            }
        });

        if modal.backdrop_response.clicked() {
            close = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }

        if submit {
            let had_source = !self.add_source.trim().is_empty();
            self.submit_add();
            if had_source {
                close = true;
            }
        }
        if close {
            self.add_open = false;
            self.add_source.clear();
        }
    }

    /// Transfers that are moving or kept, so the rail's Activity item has a home.
    pub(crate) fn activity_screen(&mut self, ui: &mut egui::Ui) {
        // "Moving" is what the repaint loop already treats as active; "kept" is
        // a download that finished. A stream-only title nobody is watching is
        // neither, and does not belong here.
        let active: Vec<LibraryItem> = self
            .library
            .iter()
            .filter(|item| {
                item.torrent.stats.download_bps > 0 || (item.downloading && !item.torrent.finished)
            })
            .cloned()
            .collect();
        let kept: Vec<LibraryItem> = self
            .library
            .iter()
            .filter(|item| item.downloading && item.torrent.finished)
            .cloned()
            .collect();
        let down: u64 = self
            .library
            .iter()
            .map(|item| item.torrent.stats.download_bps)
            .sum();

        let mut open: Option<usize> = None;
        let mut pause: Option<(usize, bool)> = None;
        let mut pause_all: Option<bool> = None;
        let mut go_limits = false;

        ui.add_space(16.0);
        page_title(ui, "Activity");
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Transfers that are moving, and titles kept on disk.")
                .color(theme::TEXT_DIM)
                .size(11.5),
        );
        ui.add_space(10.0);

        // Summary on the left, global controls on the right.
        ui.horizontal(|ui| {
            let mut facts = Vec::new();
            if down > 0 {
                facts.push(format!("{} \u{2b07}", fmt::human_rate(down)));
            }
            facts.push(plural(active.len(), "transfer"));
            facts.push(format!("{} kept", kept.len()));
            ui.label(
                egui::RichText::new(facts.join("   \u{2022}   "))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Limits")
                    .on_hover_text("Bandwidth limits live in Settings \u{2192} Network")
                    .clicked()
                {
                    go_limits = true;
                }
                if ui.small_button("Pause all").clicked() {
                    pause_all = Some(true);
                }
                if ui.small_button("Resume all").clicked() {
                    pause_all = Some(false);
                }
            });
        });

        if active.is_empty() && kept.is_empty() {
            empty_state(
                ui,
                "Nothing is transferring",
                "Playing a title streams it; downloading a copy keeps it on disk.",
            );
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            if !active.is_empty() {
                ui.add_space(6.0);
                section_header(ui, "Transferring");
                ui.add_space(4.0);
                for item in &active {
                    let work_id = self
                        .work_id_for_torrent(item.torrent.id)
                        .unwrap_or(item.torrent.id);
                    let paused = item.torrent.stats.is_paused();
                    egui::Frame::NONE
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .corner_radius(theme::CARD_RADIUS)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new(reel_core::title::truncate(
                                            &item.heading(),
                                            60,
                                        ))
                                        .size(13.5)
                                        .strong(),
                                    );
                                    let mut facts = vec![item.torrent.stats.state.clone()];
                                    if item.downloading {
                                        facts.push("keeping".to_string());
                                    }
                                    if item.torrent.stats.download_bps > 0 {
                                        facts
                                            .push(fmt::human_rate(item.torrent.stats.download_bps));
                                    }
                                    facts.push(format!("{} peers", item.torrent.stats.peers.live));
                                    if let Some(eta) = item.torrent.stats.eta_seconds {
                                        facts.push(fmt::human_eta(Some(eta)));
                                    }
                                    ui.label(
                                        egui::RichText::new(facts.join("   \u{2022}   "))
                                            .size(11.0)
                                            .color(theme::TEXT_DIM),
                                    );
                                    ui.add(
                                        egui::ProgressBar::new(
                                            (item.torrent.stats.percent as f32 / 100.0)
                                                .clamp(0.0, 1.0),
                                        )
                                        .desired_width(280.0)
                                        .fill(if paused { theme::WARN } else { theme::ACCENT })
                                        .text(format!("{:.0}%", item.torrent.stats.percent)),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("Open").clicked() {
                                            open = Some(work_id);
                                        }
                                        if ui
                                            .add_enabled(
                                                !item.torrent.finished,
                                                egui::Button::new(if paused {
                                                    "Resume"
                                                } else {
                                                    "Pause"
                                                })
                                                .small(),
                                            )
                                            .clicked()
                                        {
                                            pause = Some((item.torrent.id, !paused));
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(6.0);
                }
            }

            if !kept.is_empty() {
                ui.add_space(10.0);
                section_header(ui, "Kept on disk");
                ui.add_space(4.0);
                for item in &kept {
                    let work_id = self
                        .work_id_for_torrent(item.torrent.id)
                        .unwrap_or(item.torrent.id);
                    egui::Frame::NONE
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .corner_radius(theme::CARD_RADIUS)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(reel_core::title::truncate(
                                        &item.heading(),
                                        60,
                                    ))
                                    .size(13.5)
                                    .strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("Open").clicked() {
                                            open = Some(work_id);
                                        }
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "on disk   \u{2022}   {}",
                                                fmt::human_bytes(item.torrent.stats.total_bytes)
                                            ))
                                            .size(11.0)
                                            .color(theme::OK),
                                        );
                                    },
                                );
                            });
                        });
                    ui.add_space(6.0);
                }
            }
        });

        if go_limits {
            self.settings_section = SettingsSection::Network;
            self.go_to(Screen::Settings);
        }
        if let Some(paused) = pause_all {
            for item in &active {
                if item.torrent.stats.is_paused() != paused {
                    self.backend.set_paused(item.torrent.id, paused);
                }
            }
            self.invalidate();
        }
        if let Some(id) = open {
            self.open_work(id);
        }
        if let Some((id, paused)) = pause {
            self.backend.set_paused(id, paused);
            self.invalidate();
        }
    }

    pub(crate) fn draw_toast(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (message, color) = {
            let Some(toast) = self.toast.as_ref() else {
                return;
            };
            if toast.shown.elapsed() > TOAST_LIFETIME {
                self.toast = None;
                return;
            }
            let color = if toast.error {
                theme::DANGER
            } else {
                theme::OK
            };
            (toast.message.clone(), color)
        };

        // Keep animating so the toast expires even without user input.
        ctx.request_repaint_after(Duration::from_millis(200));

        let mut dismiss = false;
        egui::Panel::bottom("toast")
            .frame(
                egui::Frame::NONE
                    .fill(theme::SURFACE_RAISED)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .stroke(egui::Stroke::new(1.0, color))
                    .corner_radius(CornerRadius::same(8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(color, "\u{2022}");
                    ui.label(message);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                });
            });

        if dismiss {
            self.toast = None;
        }
    }

    /// Open the remove dialog for a work. `only` pre-ticks a single source;
    /// otherwise every source is ticked, so removing a merged title removes all
    /// of it unless the user says otherwise.
    pub fn open_remove_dialog(&mut self, work_id: usize, only: Option<usize>) {
        let Some(work) = self.work(work_id) else {
            return;
        };
        self.remove_selected = match only {
            Some(torrent_id) => HashSet::from([torrent_id]),
            None => work
                .members
                .iter()
                .map(|member| member.torrent_id())
                .collect(),
        };
        self.remove_files = false;
        self.show_remove = Some(work_id);
    }

    pub(crate) fn draw_remove_dialog(&mut self, _ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(work_id) = self.show_remove else {
            return;
        };
        // Navigating away dismisses the dialog; it belongs to one detail page.
        if !matches!(self.screen(), Screen::Work(id) if *id == work_id) {
            self.show_remove = None;
            return;
        }
        let Some(work) = self.work(work_id) else {
            self.show_remove = None;
            return;
        };

        let mut close = false;
        let mut confirmed = false;
        let mut paused_change: Option<(usize, bool)> = None;

        egui::Window::new("Remove torrents")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!("Which copies of {}?", work.heading())).strong(),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Untick a source to keep it. Removing forgets the torrent and any saved \
                         position; deleting also removes downloaded files from disk.",
                    )
                    .color(theme::TEXT_DIM)
                    .size(11.5),
                );
                ui.add_space(8.0);

                for member in &work.members {
                    let torrent_id = member.torrent_id();
                    let mut selected = self.remove_selected.contains(&torrent_id);
                    let label = format!(
                        "{}  \u{2022}  {}",
                        reel_core::title::truncate(&member.entry.heading(), 48),
                        member.attributes().quality_label()
                    );
                    if ui.checkbox(&mut selected, label).changed() {
                        if selected {
                            self.remove_selected.insert(torrent_id);
                        } else {
                            self.remove_selected.remove(&torrent_id);
                        }
                    }
                }

                ui.add_space(6.0);
                ui.checkbox(&mut self.remove_files, "Also delete downloaded files");

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    let count = self.remove_selected.len();
                    let remove_label = if count == 0 {
                        "Remove selected".to_string()
                    } else {
                        format!(
                            "Remove {count} torrent{}",
                            if count == 1 { "" } else { "s" }
                        )
                    };
                    if ui
                        .add_enabled(
                            count > 0,
                            egui::Button::new(
                                egui::RichText::new(remove_label).color(theme::DANGER),
                            ),
                        )
                        .clicked()
                    {
                        confirmed = true;
                    }
                });

                // Pausing a source is the other thing one wants from here.
                for member in &work.members {
                    let torrent_id = member.torrent_id();
                    if let Some(item) = self.item(torrent_id) {
                        let paused = item.torrent.stats.is_paused();
                        let label = if paused {
                            "Resume download"
                        } else {
                            "Pause download"
                        };
                        if ui
                            .small_button(format!("{label} \u{2014} {}", item.heading()))
                            .clicked()
                        {
                            paused_change = Some((torrent_id, !paused));
                        }
                    }
                }
            });

        if let Some((torrent_id, paused)) = paused_change {
            self.backend.set_paused(torrent_id, paused);
            self.invalidate();
        }

        if confirmed {
            let ids: Vec<usize> = self.remove_selected.iter().copied().collect();
            let count = ids.len();
            for torrent_id in ids {
                self.backend.remove(torrent_id, self.remove_files);
            }
            self.set_toast(
                format!(
                    "Removed {count} torrent{}",
                    if count == 1 { "" } else { "s" }
                ),
                false,
            );
            self.refresh();
            // The work may have shrunk or vanished entirely.
            if self.work(work_id).is_none() {
                self.nav.go(Screen::Home);
            }
            close = true;
        }
        if close {
            self.show_remove = None;
            self.remove_selected.clear();
        }
    }
}

