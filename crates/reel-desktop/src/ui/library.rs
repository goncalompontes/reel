//! The Home screen: hero, rows and grids.

use super::*;

// ----------------------------------------------------------------- library

impl App {
    pub(crate) fn library_screen(&mut self, ui: &mut egui::Ui) {
        // Home's own header: the title, and the library filter that used to live
        // in the (now removed) global top bar.
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            page_title(ui, "Home");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_sized(
                    [280.0, 26.0],
                    egui::TextEdit::singleline(&mut self.filter)
                        .id(egui::Id::new("library-filter"))
                        .hint_text("Filter your library\u{2026}"),
                );
            });
        });
        ui.add_space(10.0);

        let filter = self.filter.trim().to_lowercase();

        if self.works.is_empty() {
            self.empty_library(ui);
            return;
        }

        // When filtering, show one flat grid: rows would only fragment the
        // matches.
        if !filter.is_empty() {
            let matches: Vec<Work> = self
                .works
                .iter()
                .filter(|work| {
                    format!("{} {}", work.title, work.heading())
                        .to_lowercase()
                        .contains(&filter)
                })
                .cloned()
                .collect();

            egui::ScrollArea::vertical().show(ui, |ui| {
                section_header(
                    ui,
                    format!(
                        "{} match{} for \"{}\"",
                        matches.len(),
                        if matches.len() == 1 { "" } else { "es" },
                        self.filter.trim()
                    ),
                );
                ui.add_space(10.0);
                if matches.is_empty() {
                    ui.label(egui::RichText::new("Nothing here matches.").color(theme::TEXT_DIM));
                } else {
                    self.card_grid(ui, &matches);
                }
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some(featured) = self.works.first().cloned() {
                self.hero(ui, &featured);
                ui.add_space(16.0);
            }

            for row in self.rows.clone() {
                let works: Vec<Work> = row.ids.iter().filter_map(|id| self.work(*id)).collect();
                if works.is_empty() {
                    continue;
                }

                ui.horizontal(|ui| {
                    section_header(ui, row.title.as_str());
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(plural(works.len(), "title"))
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.add_space(6.0);
                self.row_strip(ui, &works, row.kind);
                ui.add_space(20.0);
            }
        });
    }

    /// A horizontally scrolling strip of posters.
    pub(crate) fn row_strip(&mut self, ui: &mut egui::Ui, works: &[Work], kind: RowKind) {
        // Resume cards are taller so the "resume from" line has room.
        let (width, height) = match kind {
            RowKind::ContinueWatching => (240.0, 135.0),
            _ => (168.0, 252.0),
        };

        let mut open: Option<usize> = None;
        let mut play: Option<usize> = None;

        egui::ScrollArea::horizontal()
            .id_salt(format!("row-{kind:?}"))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for work in works {
                        let hit = self.card(ui, work, width, height);
                        let id = work.lead_torrent_id();
                        if hit.double_clicked {
                            play = Some(id);
                        } else if hit.clicked {
                            open = Some(id);
                        }
                        ui.add_space(10.0);
                    }
                });
            });

        if let Some(id) = play {
            self.play_with_ctx(id);
        } else if let Some(id) = open {
            self.open_work(id);
        }
    }

    /// A wrapped grid, used for filtered results and small libraries.
    pub(crate) fn card_grid(&mut self, ui: &mut egui::Ui, works: &[Work]) {
        const CARD_W: f32 = 168.0;
        // 2:3, the shape of a real poster, so artwork is not stretched.
        const CARD_H: f32 = 252.0;
        const GAP: f32 = 12.0;

        let available = ui.available_width();
        let per_row = (((available + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        let mut open: Option<usize> = None;
        let mut play: Option<usize> = None;

        for chunk in works.chunks(per_row) {
            ui.horizontal(|ui| {
                for work in chunk {
                    let hit = self.card(ui, work, CARD_W, CARD_H);
                    let id = work.lead_torrent_id();
                    if hit.double_clicked {
                        play = Some(id);
                    } else if hit.clicked {
                        open = Some(id);
                    }
                    ui.add_space(GAP);
                }
            });
            ui.add_space(GAP);
        }

        if let Some(id) = play {
            self.play_with_ctx(id);
        } else if let Some(id) = open {
            self.open_work(id);
        }
    }

    pub(crate) fn empty_library(&mut self, ui: &mut egui::Ui) {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("Your library is empty")
                    .size(24.0)
                    .strong(),
            );
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Add a magnet link or a .torrent URL and reel will start fetching the \
                     video, streaming it while it downloads.",
                )
                .color(theme::TEXT_DIM),
            );
            ui.add_space(20.0);
            if ui.button("  Add a torrent  ").clicked() {
                self.add_open = true;
            }
        });
    }

    /// The banner at the top of the library: newest entry, blown up.
    pub(crate) fn hero(&mut self, ui: &mut egui::Ui, work: &Work) {
        let height = 300.0;
        let width = ui.available_width();
        let (rect, hero_response) =
            ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        hero_response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), work.heading())
        });
        let painter = ui.painter().clone();

        Self::paint_art(
            ui,
            rect,
            work.backdrop(),
            &work.title,
            ArtworkKind::Backdrop,
            theme::PANEL_RADIUS,
        );

        // A gradient from the bottom, so the title reads over any artwork.
        let mut scrim = rect;
        scrim.set_top(rect.bottom() - rect.height() * 0.9);
        theme::paint_scrim(&painter, scrim, theme::PANEL_RADIUS, 235);

        let resuming = work.resume_position().is_some();
        painter.text(
            egui::pos2(rect.left() + 28.0, rect.top() + 24.0),
            egui::Align2::LEFT_TOP,
            if resuming { "CONTINUE WATCHING" } else { "NEWLY ADDED" },
            egui::FontId::proportional(11.0),
            theme::ACCENT,
        );

        painter.text(
            egui::pos2(rect.left() + 28.0, rect.bottom() - 78.0),
            egui::Align2::LEFT_BOTTOM,
            reel_core::title::truncate(&work.heading(), 52),
            egui::FontId::new(32.0, theme::semibold(ui.ctx())),
            Color32::WHITE,
        );

        // A row of fact pills: year, rating, runtime, genres (or transfer state
        // when there is no metadata).
        let lead = self.lead_item(work);
        let mut facts: Vec<(String, Color32)> = Vec::new();
        match work.metadata.as_ref() {
            Some(metadata) => {
                if let Some(year) = metadata.year {
                    facts.push((year.to_string(), theme::TEXT));
                }
                if let Some(rating) = metadata.rating {
                    facts.push((format!("{} {rating:.1}", icons::STAR), theme::WARN));
                }
                if let Some(runtime) = metadata.runtime_label() {
                    facts.push((runtime, theme::TEXT));
                }
                if work.torrent_count() > 1 {
                    facts.push((format!("{} copies", work.torrent_count()), theme::TEXT_DIM));
                }
                if !metadata.genres.is_empty() {
                    facts.push((metadata.genres.join(" \u{00b7} "), theme::TEXT_DIM));
                }
            }
            None => {
                if let Some(item) = lead.as_ref() {
                    facts.push((item.torrent.stats.state.clone(), theme::TEXT));
                    facts.push((
                        format!("{} peers", item.torrent.stats.peers.live),
                        theme::TEXT_DIM,
                    ));
                    facts.push((
                        format!("{:.0}% downloaded", item.torrent.stats.percent),
                        theme::TEXT_DIM,
                    ));
                }
            }
        }
        let mut x = rect.left() + 28.0;
        let pill_y = rect.bottom() - 66.0;
        let max_right = rect.right() - 28.0;
        for (text, color) in facts {
            let width_hint = ui.painter().layout_no_wrap(
                text.clone(),
                egui::FontId::proportional(10.0),
                color,
            );
            if x + width_hint.size().x + 20.0 > max_right {
                break;
            }
            let r = Self::paint_pill(
                ui,
                egui::pos2(x, pill_y),
                &text,
                color,
                Color32::from_black_alpha(120),
            );
            x = r.right() + 6.0;
        }

        // Actions: the one primary Play, and More info.
        let play_label = match work.resume_position() {
            Some(_) => format!("{}  Resume", icons::PLAY),
            None => format!("{}  Play", icons::PLAY),
        };
        let play = ui
            .put(
                Rect::from_min_size(
                    egui::pos2(rect.left() + 28.0, rect.bottom() - 44.0),
                    Vec2::new(150.0, 32.0),
                ),
                egui::Button::new(
                    egui::RichText::new(play_label)
                        .size(14.0)
                        .family(theme::semibold(ui.ctx())),
                )
                .fill(theme::ACCENT_DEEP)
                .corner_radius(theme::CONTROL_RADIUS),
            )
            .clicked();
        let more = ui
            .put(
                Rect::from_min_size(
                    egui::pos2(rect.left() + 186.0, rect.bottom() - 44.0),
                    Vec2::new(130.0, 32.0),
                ),
                egui::Button::new(egui::RichText::new("More info").size(13.0))
                    .fill(Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .corner_radius(theme::CONTROL_RADIUS),
            )
            .clicked();

        let id = work.lead_torrent_id();
        if play {
            self.play_with_ctx(id);
        } else if more || hero_response.clicked() {
            self.open_work(id);
        }
    }

    /// Play a work, using the context saved at the start of the frame.
    pub(crate) fn play_with_ctx(&mut self, work_id: usize) {
        let Some(ctx) = self.pending_ctx.clone() else {
            self.warn("Playback is not ready yet");
            return;
        };
        self.play_work(&ctx, work_id);
    }
}

